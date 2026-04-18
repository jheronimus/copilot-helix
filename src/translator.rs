//! Translates between Helix's `textDocument/completion` and Copilot's
//! `textDocument/inlineCompletion` protocol extension.
//!
//! # Protocol gap
//!
//! Helix sends standard LSP `textDocument/completion` requests.  Copilot's
//! language server expects `textDocument/inlineCompletion` with an extra
//! `formattingOptions` field and returns `InlineCompletionItem`s instead of
//! `CompletionItem`s.  This module bridges the two.
//!
//! # Concurrency
//!
//! [`Translator`] wraps its mutable state in `Arc<Mutex<_>>` and is `Clone`,
//! so the proxy can hand one clone to the Helix-reader task and another to the
//! upstream-reader task without extra synchronisation boilerplate.  The mutex
//! is held only for the duration of a HashMap lookup or insert — never across
//! an `.await`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use serde_json::{json, Value};

use crate::jsonrpc::Message;

// ── Public API ────────────────────────────────────────────────────────────────

/// Shared, cloneable translation state.
#[derive(Clone, Default)]
pub struct Translator {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    /// Maps proxy-generated request id → original Helix request id.
    pending: HashMap<u64, Value>,
    /// Tracks the latest known document version by URI.
    document_versions: HashMap<String, i64>,
    next_id: u64,
}

impl Translator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a Helix message that may update document version state.
    ///
    /// This is called for both live traffic and messages buffered during the
    /// initialization handshake so completion translation sees the same state
    /// regardless of when a document event arrived.
    pub fn observe_helix_message(&self, msg: &Message) {
        match msg.method() {
            Some("textDocument/didOpen") => {
                let Some(params) = msg.params.as_ref() else {
                    return;
                };
                let Some(document) = params.get("textDocument") else {
                    return;
                };
                self.track_document_version(document);
            }
            Some("textDocument/didChange") => {
                let Some(params) = msg.params.as_ref() else {
                    return;
                };
                let Some(document) = params.get("textDocument") else {
                    return;
                };
                self.track_document_version(document);
            }
            Some("textDocument/didClose") => {
                let Some(uri) = msg
                    .params
                    .as_ref()
                    .and_then(|params| params.get("textDocument"))
                    .and_then(|document| document.get("uri"))
                    .and_then(Value::as_str)
                else {
                    return;
                };
                self.inner
                    .lock()
                    .expect("translator mutex poisoned")
                    .document_versions
                    .remove(uri);
            }
            _ => {}
        }
    }

    /// Rewrite a `textDocument/completion` request from Helix into a
    /// `textDocument/inlineCompletion` request for the language server.
    ///
    /// Records the proxy↔Helix id mapping so the response can be un-mapped.
    pub fn translate_request(&self, msg: Message) -> Message {
        let helix_id = msg.id.clone().unwrap_or(Value::Null);
        let params = self.build_inline_completion_params(msg.params.clone());

        let proxy_id = {
            let mut inner = self.inner.lock().expect("translator mutex poisoned");
            inner.next_id += 1;
            let id = inner.next_id;
            inner.pending.insert(id, helix_id);
            id
        };

        Message {
            jsonrpc: "2.0".into(),
            id: Some(Value::Number(proxy_id.into())),
            method: Some("textDocument/inlineCompletion".into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    /// Inspect a response arriving from the language server.
    ///
    /// If its `id` matches a pending proxied completion, reconstructs a
    /// standard `textDocument/completion` response and returns it.
    /// Otherwise returns the message unchanged.
    pub fn try_translate_response(&self, msg: Message) -> Message {
        let Some(id) = &msg.id else {
            return msg;
        };

        let proxy_id = match id {
            Value::Number(n) => match n.as_u64() {
                Some(v) => v,
                None => return msg,
            },
            _ => return msg,
        };

        let helix_id = {
            let mut inner = self.inner.lock().expect("translator mutex poisoned");
            inner.pending.remove(&proxy_id)
        };

        let Some(helix_id) = helix_id else {
            return msg; // not a proxied completion
        };

        // Re-attach the original Helix id.
        if let Some(err) = msg.error {
            // Forward errors with the original id.
            return Message {
                jsonrpc: "2.0".into(),
                id: Some(helix_id),
                method: None,
                params: None,
                result: None,
                error: Some(err),
            };
        }

        let items = build_completion_items(msg.result.as_ref());
        Message::success(
            helix_id,
            json!({
                "isIncomplete": false,
                "items": items,
            }),
        )
    }

    /// Rewrite a `$/cancelRequest` notification from Helix.
    ///
    /// If the cancelled id belongs to a pending proxied completion, remaps it
    /// to the proxy id so the language server can match it.  Otherwise returns
    /// the message unchanged.
    pub fn try_remap_cancel(&self, mut msg: Message) -> Message {
        let helix_id = msg.params.as_ref().and_then(|p| p.get("id")).cloned();

        let Some(helix_id) = helix_id else {
            return msg;
        };

        let proxy_id = {
            let inner = self.inner.lock().expect("translator mutex poisoned");
            inner
                .pending
                .iter()
                .find(|(_, v)| *v == &helix_id)
                .map(|(k, _)| *k)
        };

        if let Some(proxy_id) = proxy_id {
            if let Some(params) = msg.params.as_mut() {
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("id".into(), Value::Number(proxy_id.into()));
                }
            }
        }

        msg
    }

    /// Returns `true` if the message is a response to a pending proxied
    /// completion request.  Used by the proxy to decide routing.
    pub fn is_pending_response(&self, msg: &Message) -> bool {
        let Some(id) = &msg.id else {
            return false;
        };
        if !msg.is_response() {
            return false;
        }
        let Some(n) = id.as_u64() else {
            return false;
        };
        self.inner
            .lock()
            .expect("translator mutex poisoned")
            .pending
            .contains_key(&n)
    }

    /// Build the upstream inline-completion params using the latest known
    /// document version for the request URI when available.
    fn build_inline_completion_params(&self, params: Option<Value>) -> Value {
        let params = params.unwrap_or(json!({}));
        let tracked_version = params
            .get("textDocument")
            .and_then(|document| document.get("uri"))
            .and_then(Value::as_str)
            .and_then(|uri| self.lookup_document_version(uri));

        build_inline_completion_params(params, tracked_version)
    }

    fn track_document_version(&self, document: &Value) {
        let Some(uri) = document.get("uri").and_then(Value::as_str) else {
            return;
        };
        let Some(version) = document.get("version").and_then(Value::as_i64) else {
            return;
        };

        self.inner
            .lock()
            .expect("translator mutex poisoned")
            .document_versions
            .insert(uri.to_owned(), version);
    }

    fn lookup_document_version(&self, uri: &str) -> Option<i64> {
        self.inner
            .lock()
            .expect("translator mutex poisoned")
            .document_versions
            .get(uri)
            .copied()
    }
}

// ── Request helpers ──────────────────────────────────────────────────────────

/// Build `textDocument/inlineCompletion` params from a `textDocument/completion`
/// params object.
fn build_inline_completion_params(mut params: Value, tracked_version: Option<i64>) -> Value {
    if let Some(version) = tracked_version {
        if let Some(document) = params
            .as_object_mut()
            .and_then(|obj| obj.get_mut("textDocument"))
            .and_then(Value::as_object_mut)
        {
            document
                .entry("version")
                .or_insert_with(|| Value::Number(version.into()));
        }
    }

    // Add formattingOptions if absent — Copilot requires this field.
    if let Some(obj) = params.as_object_mut() {
        obj.entry("formattingOptions")
            .or_insert_with(|| json!({ "insertSpaces": true, "tabSize": 2 }));
    }
    params
}

// ── Response helpers ─────────────────────────────────────────────────────────

/// Convert `InlineCompletionItem`s to standard `CompletionItem`s.
fn build_completion_items(result: Option<&Value>) -> Vec<Value> {
    let items = result
        .and_then(|r| r.get("items"))
        .and_then(Value::as_array);

    let Some(items) = items else {
        return vec![];
    };

    items
        .iter()
        .enumerate()
        .map(|(i, item)| inline_item_to_completion(item, i))
        .collect()
}

fn inline_item_to_completion(item: &Value, index: usize) -> Value {
    let insert_text = item
        .get("insertText")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let label = first_line_truncated(insert_text, 80);
    let sort_text = format!("{:04}", index + 1);

    let range = item.get("range").cloned().unwrap_or_else(|| {
        // Fallback: zero-width range at unknown position.
        json!({
            "start": { "line": 0, "character": 0 },
            "end":   { "line": 0, "character": 0 }
        })
    });

    let mut completion = json!({
        "label": label,
        "kind": 1,                  // Text
        "insertTextFormat": 1,      // PlainText
        "insertText": insert_text,
        "textEdit": {
            "range": range,
            "newText": insert_text,
        },
        "sortText": sort_text,
    });

    // Pass through filterText if the language server provided it.
    if let Some(ft) = item.get("filterText") {
        completion["filterText"] = ft.clone();
    }

    // Stash the Copilot acceptance command for future telemetry (Step 7).
    if let Some(cmd) = item.get("command") {
        completion["data"] = json!({ "_copilot_command": cmd });
    }

    completion
}

/// Returns the first non-empty line of `text`, capped at `max_chars` characters.
fn first_line_truncated(text: &str, max_chars: usize) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or(text);
    // Use .chars().count() for correct Unicode character counting.
    if line.chars().count() <= max_chars {
        line.to_owned()
    } else {
        line.chars().take(max_chars).collect::<String>() + "…"
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_completion_request(id: u64) -> Message {
        Message::request(
            id,
            "textDocument/completion",
            json!({
                "textDocument": { "uri": "file:///src/main.rs" },
                "position": { "line": 5, "character": 10 },
                "context": { "triggerKind": 1 }
            }),
        )
    }

    #[test]
    fn request_rewrites_method_and_id() {
        let t = Translator::new();
        let out = t.translate_request(make_completion_request(7));
        assert_eq!(out.method(), Some("textDocument/inlineCompletion"));
        // Proxy id is different from the original Helix id.
        assert_ne!(out.id, Some(json!(7)));
    }

    #[test]
    fn request_injects_formatting_options() {
        let t = Translator::new();
        let out = t.translate_request(make_completion_request(1));
        let opts = &out.params.unwrap()["formattingOptions"];
        assert_eq!(opts["insertSpaces"], true);
        assert_eq!(opts["tabSize"], 2);
    }

    #[test]
    fn request_injects_tracked_document_version() {
        let t = Translator::new();
        t.observe_helix_message(&Message::notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": "file:///src/main.rs",
                    "version": 7,
                    "languageId": "rust",
                    "text": "fn main() {}\n"
                }
            }),
        ));

        let out = t.translate_request(make_completion_request(1));
        assert_eq!(out.params.unwrap()["textDocument"]["version"], 7);
    }

    #[test]
    fn request_uses_updated_document_version() {
        let t = Translator::new();
        t.observe_helix_message(&Message::notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": "file:///src/main.rs",
                    "version": 7,
                    "languageId": "rust",
                    "text": "fn main() {}\n"
                }
            }),
        ));
        t.observe_helix_message(&Message::notification(
            "textDocument/didChange",
            json!({
                "textDocument": {
                    "uri": "file:///src/main.rs",
                    "version": 9
                },
                "contentChanges": [{ "text": "fn main() { println!(\"hi\"); }\n" }]
            }),
        ));

        let out = t.translate_request(make_completion_request(1));
        assert_eq!(out.params.unwrap()["textDocument"]["version"], 9);
    }

    #[test]
    fn request_does_not_inject_stale_version_after_close() {
        let t = Translator::new();
        t.observe_helix_message(&Message::notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": "file:///src/main.rs",
                    "version": 7,
                    "languageId": "rust",
                    "text": "fn main() {}\n"
                }
            }),
        ));
        t.observe_helix_message(&Message::notification(
            "textDocument/didClose",
            json!({
                "textDocument": {
                    "uri": "file:///src/main.rs"
                }
            }),
        ));

        let out = t.translate_request(make_completion_request(1));
        assert!(out.params.unwrap()["textDocument"].get("version").is_none());
    }

    #[test]
    fn buffered_did_open_updates_state_before_first_completion() {
        let t = Translator::new();
        let buffered_open = Message::notification(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": "file:///src/main.rs",
                    "version": 11,
                    "languageId": "rust",
                    "text": "fn main() {}\n"
                }
            }),
        );

        t.observe_helix_message(&buffered_open);

        let out = t.translate_request(make_completion_request(1));
        assert_eq!(out.params.unwrap()["textDocument"]["version"], 11);
    }

    #[test]
    fn request_preserves_existing_formatting_options() {
        let t = Translator::new();
        let msg = Message::request(
            1,
            "textDocument/completion",
            json!({
                "textDocument": { "uri": "file:///f.rs" },
                "position": { "line": 0, "character": 0 },
                "context": { "triggerKind": 1 },
                "formattingOptions": { "insertSpaces": false, "tabSize": 4 }
            }),
        );
        let out = t.translate_request(msg);
        let opts = &out.params.unwrap()["formattingOptions"];
        assert_eq!(opts["insertSpaces"], false);
        assert_eq!(opts["tabSize"], 4);
    }

    #[test]
    fn response_round_trip_restores_helix_id() {
        let t = Translator::new();
        let req = t.translate_request(make_completion_request(42));
        let proxy_id = req.id.clone().unwrap();

        // Simulate a response from the language server using the proxy id.
        let upstream_resp = Message::success(
            proxy_id,
            json!({
                "items": [
                    {
                        "insertText": "fn hello() {}",
                        "range": {
                            "start": { "line": 5, "character": 0 },
                            "end":   { "line": 5, "character": 0 }
                        },
                        "command": { "command": "notifyAccepted", "arguments": ["id-1"] }
                    }
                ]
            }),
        );

        let out = t.try_translate_response(upstream_resp);
        assert_eq!(out.id, Some(json!(42)));
        let items = out.result.unwrap()["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["label"], "fn hello() {}");
        assert_eq!(items[0]["kind"], 1);
        assert_eq!(items[0]["insertTextFormat"], 1);
        assert_eq!(
            items[0]["data"]["_copilot_command"]["command"],
            "notifyAccepted"
        );
    }

    #[test]
    fn response_empty_items_produces_empty_list() {
        let t = Translator::new();
        let req = t.translate_request(make_completion_request(1));
        let proxy_id = req.id.clone().unwrap();

        let resp = Message::success(proxy_id, json!({ "items": [] }));
        let out = t.try_translate_response(resp);
        assert_eq!(out.result.unwrap()["items"], json!([]));
    }

    #[test]
    fn response_null_result_produces_empty_list() {
        let t = Translator::new();
        let req = t.translate_request(make_completion_request(1));
        let proxy_id = req.id.clone().unwrap();

        let resp = Message::success(proxy_id, Value::Null);
        let out = t.try_translate_response(resp);
        assert_eq!(out.result.unwrap()["items"], json!([]));
    }

    #[test]
    fn unrelated_response_passes_through_unchanged() {
        let t = Translator::new();
        let resp = Message::success(json!(99), json!({ "capabilities": {} }));
        let out = t.try_translate_response(resp);
        assert_eq!(out.id, Some(json!(99)));
        assert_eq!(out.result.unwrap()["capabilities"], json!({}));
    }

    #[test]
    fn error_response_restores_helix_id() {
        let t = Translator::new();
        let req = t.translate_request(make_completion_request(5));
        let proxy_id = req.id.clone().unwrap();

        let resp = Message::error_response(proxy_id, -32603, "internal error");
        let out = t.try_translate_response(resp);
        assert_eq!(out.id, Some(json!(5)));
        assert!(out.error.is_some());
        assert!(out.result.is_none());
    }

    #[test]
    fn cancel_remaps_to_proxy_id() {
        let t = Translator::new();
        let req = t.translate_request(make_completion_request(7));
        let proxy_id = req.id.clone().unwrap();

        let cancel = Message::notification("$/cancelRequest", json!({ "id": 7 }));
        let out = t.try_remap_cancel(cancel);
        assert_eq!(out.params.unwrap()["id"], proxy_id);
    }

    #[test]
    fn cancel_for_unknown_id_passes_through() {
        let t = Translator::new();
        let cancel = Message::notification("$/cancelRequest", json!({ "id": 999 }));
        let out = t.try_remap_cancel(cancel);
        assert_eq!(out.params.unwrap()["id"], 999);
    }

    #[test]
    fn is_pending_response_true_for_proxied_id() {
        let t = Translator::new();
        let req = t.translate_request(make_completion_request(1));
        let proxy_id = req.id.clone().unwrap();
        let resp = Message::success(proxy_id, json!({}));
        assert!(t.is_pending_response(&resp));
    }

    #[test]
    fn is_pending_response_false_for_unknown_id() {
        let t = Translator::new();
        let resp = Message::success(json!(999), json!({}));
        assert!(!t.is_pending_response(&resp));
    }

    #[test]
    fn label_truncates_long_first_line() {
        let long = "a".repeat(100);
        let result = first_line_truncated(&long, 80);
        assert_eq!(result.chars().count(), 81); // 80 chars + ellipsis
        assert!(result.ends_with('…'));
    }

    #[test]
    fn label_skips_blank_first_lines() {
        let text = "\n\n    fn foo() {}";
        let result = first_line_truncated(text, 80);
        assert_eq!(result, "    fn foo() {}");
    }

    #[test]
    fn sort_text_orders_items() {
        let items: Vec<Value> = (0..5)
            .map(|i| {
                inline_item_to_completion(
                    &json!({
                        "insertText": format!("item{i}"),
                        "range": { "start": {"line":0,"character":0}, "end": {"line":0,"character":0} }
                    }),
                    i,
                )
            })
            .collect();

        let sort_texts: Vec<&str> = items
            .iter()
            .map(|v| v["sortText"].as_str().unwrap())
            .collect();

        assert_eq!(sort_texts, ["0001", "0002", "0003", "0004", "0005"]);
    }

    #[test]
    fn multiple_in_flight_requests_isolated() {
        let t = Translator::new();
        let r1 = t.translate_request(make_completion_request(10));
        let r2 = t.translate_request(make_completion_request(20));
        assert_ne!(r1.id, r2.id, "proxy ids must be unique");

        // Respond to r2 first.
        let resp2 = Message::success(r2.id.unwrap(), json!({ "items": [] }));
        let out2 = t.try_translate_response(resp2);
        assert_eq!(out2.id, Some(json!(20)));

        // r1 should still be pending.
        let resp1 = Message::success(r1.id.unwrap(), json!({ "items": [] }));
        let out1 = t.try_translate_response(resp1);
        assert_eq!(out1.id, Some(json!(10)));
    }
}
