//! LSP initialization handshake with Copilot-specific augmentation.
//!
//! The handshake is intentionally sequential and must complete before the
//! bidirectional proxy tasks start. The LSP spec guarantees that `initialize`
//! is always the first message from the client, so we can handle it without
//! concurrent message buffering complexity.
//!
//! Sequence:
//! 1. Read `initialize` from Helix (buffer any premature messages).
//! 2. Inject `editorInfo` / `editorPluginInfo` into `initializationOptions`.
//! 3. Forward the augmented request to the language server.
//! 4. Forward any upstream notifications that arrive while waiting.
//! 5. When the `initialize` response arrives, forward it to Helix.
//! 6. Read `initialized` from Helix (buffer any other messages).
//! 7. Forward `initialized` to the language server.
//! 8. Send `workspace/didChangeConfiguration` to the language server.
//! 9. Return any messages buffered in steps 1 and 6 for the caller to replay.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::{io::AsyncRead, sync::mpsc};
use tracing::{debug, warn};

use crate::{
    jsonrpc::{self, Message},
    router::{self, Direction, RouteAction},
    upstream::Upstream,
};

const PLUGIN_NAME: &str = "copilot-helix";
const PLUGIN_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Run the initialization handshake.
///
/// Returns messages received from Helix that are not part of the handshake
/// itself (anything before `initialize` or between `initialize` response and
/// `initialized`). The caller should replay these to the upstream in order.
pub async fn run_handshake<R>(
    helix_in: &mut tokio::io::BufReader<R>,
    helix_tx: &mpsc::Sender<Message>,
    upstream_tx: &mpsc::Sender<Message>,
    upstream: &mut Upstream,
) -> Result<Vec<Message>>
where
    R: AsyncRead + Unpin,
{
    let helix_version = std::env::var("HX_VERSION").unwrap_or_else(|_| "unknown".into());
    let mut buffered: Vec<Message> = Vec::new();

    // ── Phase 1: wait for `initialize` from Helix ────────────────────────
    let init_request = read_until_method(helix_in, "initialize", &mut buffered).await?;

    init_request
        .id
        .as_ref()
        .context("`initialize` request has no id")?;

    let augmented = augment_initialize(init_request, &helix_version);
    debug!("forwarding augmented initialize to upstream");
    upstream_tx
        .send(augmented)
        .await
        .map_err(|_| anyhow::anyhow!("upstream channel closed before initialize"))?;

    // ── Phase 2: wait for `initialize` response from upstream ─────────────
    // Forward any notifications that arrive in the meantime (Copilot may send
    // featureFlagsNotification immediately on startup; those are dropped by the
    // router and must not block us here).
    loop {
        let msg = upstream
            .recv()
            .await
            .context("upstream closed before sending initialize response")?;

        if msg.is_response() {
            // This should be the initialize response (only pending request).
            debug!("received initialize response from upstream");
            helix_tx
                .send(augment_initialize_response(msg))
                .await
                .map_err(|_| anyhow::anyhow!("helix channel closed"))?;
            break;
        }

        // A notification arrived before the response.
        let action = router::route(msg.method(), Direction::UpstreamToHelix);
        match action {
            RouteAction::Drop => {
                debug!(method = ?msg.method(), "dropping upstream notification during init");
            }
            _ => {
                // Forward everything else to Helix.
                debug!(method = ?msg.method(), "forwarding upstream notification during init");
                let _ = helix_tx.send(msg).await;
            }
        }
    }

    // ── Phase 3: wait for `initialized` from Helix ───────────────────────
    let initialized = read_until_method(helix_in, "initialized", &mut buffered).await?;

    debug!("forwarding initialized to upstream");
    upstream_tx
        .send(initialized)
        .await
        .map_err(|_| anyhow::anyhow!("upstream channel closed before initialized"))?;

    // ── Phase 4: send workspace/didChangeConfiguration ────────────────────
    // Copilot requires this to apply its settings after initialization.
    let config_notification = build_did_change_configuration();
    debug!("sending workspace/didChangeConfiguration");
    upstream_tx
        .send(config_notification)
        .await
        .map_err(|_| anyhow::anyhow!("upstream channel closed after initialized"))?;

    debug!(
        buffered = buffered.len(),
        "initialization complete; replaying buffered messages"
    );
    Ok(buffered)
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Read messages from `reader` until one with `method` is found.
/// Messages with other methods are appended to `buffer`.
async fn read_until_method<R>(
    reader: &mut tokio::io::BufReader<R>,
    method: &str,
    buffer: &mut Vec<Message>,
) -> Result<Message>
where
    R: AsyncRead + Unpin,
{
    loop {
        let msg = jsonrpc::read_message(reader)
            .await
            .with_context(|| format!("reading from Helix while waiting for `{method}`"))?;

        if msg.method() == Some(method) {
            return Ok(msg);
        }

        // Per LSP spec `initialize` is always first, but be defensive.
        warn!(
            got = ?msg.method(),
            expected = method,
            "unexpected message before `{method}`; buffering"
        );
        buffer.push(msg);
    }
}

/// Inject `editorInfo` and `editorPluginInfo` into `initializationOptions`.
fn augment_initialize(mut msg: Message, helix_version: &str) -> Message {
    let params = msg.params.get_or_insert(json!({}));
    let initialization_options = build_initialization_options(helix_version);

    // Ensure initializationOptions is an object.
    let opts = params.as_object_mut().and_then(|p| {
        if p.get("initializationOptions")
            .map(Value::is_null)
            .unwrap_or(true)
        {
            p.insert("initializationOptions".into(), json!({}));
        }
        p.get_mut("initializationOptions")?.as_object_mut()
    });

    if let Some(opts) = opts {
        if let Some(initialization_options) = initialization_options.as_object() {
            for (key, value) in initialization_options {
                opts.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
    }

    msg
}

/// Advertise standard completion support back to Helix.
///
/// Copilot exposes `inlineCompletionProvider`, but Helix requests standard
/// `textDocument/completion`. The proxy translates those requests, so it must
/// also present a `completionProvider` capability during initialize.
fn augment_initialize_response(mut msg: Message) -> Message {
    let Some(result) = msg.result.as_mut() else {
        return msg;
    };
    let Some(capabilities) = result
        .as_object_mut()
        .and_then(|obj| obj.get_mut("capabilities"))
        .and_then(Value::as_object_mut)
    else {
        return msg;
    };

    capabilities
        .entry("completionProvider")
        .or_insert_with(|| json!({ "resolveProvider": false }));

    debug!(
        has_completion_provider = capabilities.contains_key("completionProvider"),
        has_inline_completion_provider = capabilities.contains_key("inlineCompletionProvider"),
        "augmented initialize response for Helix"
    );

    msg
}

/// Build the shared `initializationOptions` payload for Copilot.
pub(crate) fn build_initialization_options(helix_version: &str) -> Value {
    json!({
        "editorInfo": { "name": "Helix", "version": helix_version },
        "editorPluginInfo": { "name": PLUGIN_NAME, "version": PLUGIN_VERSION }
    })
}

/// Build the shared Copilot settings payload.
pub(crate) fn build_did_change_configuration() -> Message {
    Message::notification(
        "workspace/didChangeConfiguration",
        json!({
            "settings": {
                "http": {
                    "proxy": null,
                    "proxyStrictSSL": null
                },
                "github-enterprise": {
                    "uri": null
                }
            }
        }),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn augment_adds_editor_info_when_absent() {
        let msg = Message::request(
            1,
            "initialize",
            json!({ "capabilities": {}, "initializationOptions": null }),
        );
        let got = augment_initialize(msg, "25.1");
        let opts = &got.params.as_ref().unwrap()["initializationOptions"];
        assert_eq!(opts["editorInfo"]["name"], "Helix");
        assert_eq!(opts["editorInfo"]["version"], "25.1");
        assert_eq!(opts["editorPluginInfo"]["name"], PLUGIN_NAME);
    }

    #[test]
    fn augment_does_not_overwrite_existing_editor_info() {
        let msg = Message::request(
            1,
            "initialize",
            json!({
                "capabilities": {},
                "initializationOptions": {
                    "editorInfo": { "name": "CustomEditor", "version": "1.0" }
                }
            }),
        );
        let got = augment_initialize(msg, "25.1");
        let opts = &got.params.as_ref().unwrap()["initializationOptions"];
        // Existing value is preserved.
        assert_eq!(opts["editorInfo"]["name"], "CustomEditor");
        // Missing key is added.
        assert_eq!(opts["editorPluginInfo"]["name"], PLUGIN_NAME);
    }

    #[test]
    fn augment_creates_initialization_options_when_missing() {
        let msg = Message::request(1, "initialize", json!({ "capabilities": {} }));
        let got = augment_initialize(msg, "25.1");
        let opts = &got.params.as_ref().unwrap()["initializationOptions"];
        assert_eq!(opts["editorInfo"]["name"], "Helix");
    }

    #[test]
    fn did_change_configuration_is_notification() {
        let n = build_did_change_configuration();
        assert!(n.is_notification());
        assert_eq!(n.method(), Some("workspace/didChangeConfiguration"));
        assert!(!n.params.as_ref().unwrap()["settings"].is_null());
    }

    #[test]
    fn augment_initialize_response_adds_completion_provider() {
        let msg = Message::success(
            json!(1),
            json!({
                "capabilities": {
                    "inlineCompletionProvider": {}
                }
            }),
        );

        let got = augment_initialize_response(msg);
        let capabilities = &got.result.as_ref().unwrap()["capabilities"];
        assert_eq!(capabilities["inlineCompletionProvider"], json!({}));
        assert_eq!(capabilities["completionProvider"]["resolveProvider"], false);
    }

    #[test]
    fn augment_initialize_response_preserves_existing_completion_provider() {
        let msg = Message::success(
            json!(1),
            json!({
                "capabilities": {
                    "completionProvider": {
                        "triggerCharacters": ["."]
                    }
                }
            }),
        );

        let got = augment_initialize_response(msg);
        assert_eq!(
            got.result.as_ref().unwrap()["capabilities"]["completionProvider"]["triggerCharacters"],
            json!(["."])
        );
    }
}
