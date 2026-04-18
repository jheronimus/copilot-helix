//! Bidirectional message loop between Helix and the language server.
//!
//! [`Proxy::run`] first completes the LSP initialization handshake
//! sequentially via [`crate::initializer`], then spawns two concurrent tasks:
//! - A *Helix reader* that forwards Helix stdin → upstream.
//! - An *upstream reader* that forwards upstream stdout → Helix stdout.
//!
//! A shared write channel serialises all writes to Helix stdout so any
//! component can inject messages without a mutex.

use anyhow::Result;
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncWrite, BufReader},
    sync::mpsc,
};
use tracing::{debug, error, warn, Level};

use crate::{
    auth, initializer,
    jsonrpc::{self, Message},
    router::{self, Direction, RouteAction},
    translator::Translator,
    upstream::Upstream,
};

const HELIX_WRITE_CAP: usize = 256;

/// The running proxy.
pub struct Proxy {
    upstream: Upstream,
}

impl Proxy {
    pub fn new(upstream: Upstream) -> Self {
        Self { upstream }
    }

    /// Run the proxy until either side closes its connection.
    pub async fn run<R, W>(mut self, helix_in: R, helix_out: W) -> Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (helix_tx, helix_rx) = mpsc::channel::<Message>(HELIX_WRITE_CAP);
        let upstream_tx = self.upstream.sender();
        let translator = Translator::new();

        // Start writer immediately so the initializer can respond to Helix.
        let writer_task = tokio::spawn(async move {
            run_helix_writer(helix_out, helix_rx).await;
        });

        // ── Phase 1: initialization handshake ────────────────────────────
        let mut helix_in = BufReader::new(helix_in);
        let buffered =
            initializer::run_handshake(&mut helix_in, &helix_tx, &upstream_tx, &mut self.upstream)
                .await?;

        // ── Phase 1b: auth check ──────────────────────────────────────────
        // Run this before replaying buffered Helix messages so auth does not
        // temporarily own the upstream response stream while an early
        // completion response is in flight.
        if let Err(e) = auth::check_and_warn(&upstream_tx, &mut self.upstream, &helix_tx).await {
            debug!(error = %e, "auth check failed (non-fatal)");
        }

        for msg in buffered {
            let out = prepare_helix_message(msg, &translator);
            if upstream_tx.send(out).await.is_err() {
                writer_task.abort();
                return Ok(());
            }
        }

        // ── Phase 2: bidirectional proxy tasks ───────────────────────────
        let translator_for_helix = translator.clone();
        let helix_reader_task = tokio::spawn(async move {
            run_helix_reader(helix_in, upstream_tx, translator_for_helix).await;
        });

        let upstream_reader_task = tokio::spawn(async move {
            run_upstream_reader(&mut self.upstream, helix_tx, translator).await;
        });

        tokio::select! {
            _ = helix_reader_task    => { debug!("helix reader finished"); }
            _ = upstream_reader_task => { debug!("upstream reader finished"); }
        }

        writer_task.abort();
        Ok(())
    }
}

// ── Internal task functions ──────────────────────────────────────────────────

async fn run_helix_writer<W>(helix_out: W, mut rx: mpsc::Receiver<Message>)
where
    W: AsyncWrite + Unpin,
{
    let mut out = helix_out;
    while let Some(msg) = rx.recv().await {
        if let Err(e) = jsonrpc::write_message(&mut out, &msg).await {
            error!(error = %e, "write to Helix failed");
            break;
        }
    }
    debug!("helix writer task exiting");
}

async fn run_helix_reader<R>(
    mut helix_in: BufReader<R>,
    upstream_tx: mpsc::Sender<Message>,
    translator: Translator,
) where
    R: AsyncRead + Unpin,
{
    loop {
        let msg = match jsonrpc::read_message(&mut helix_in).await {
            Ok(m) => m,
            Err(e) => {
                debug!(error = %e, "Helix stdin closed");
                break;
            }
        };

        let action = router::route(msg.method(), Direction::HelixToUpstream);
        let out = match prepare_helix_message_with_action(msg, action, &translator) {
            Some(out) => out,
            None => continue,
        };

        if upstream_tx.send(out).await.is_err() {
            warn!("upstream channel closed while forwarding from Helix");
            break;
        }
    }
    debug!("helix reader task exiting");
}

fn prepare_helix_message(msg: Message, translator: &Translator) -> Message {
    let action = router::route(msg.method(), Direction::HelixToUpstream);
    prepare_helix_message_with_action(msg, action, translator)
        .expect("Helix-to-upstream messages should not be dropped during replay")
}

fn prepare_helix_message_with_action(
    msg: Message,
    action: RouteAction,
    translator: &Translator,
) -> Option<Message> {
    log_helix_message(&msg, action);
    translator.observe_helix_message(&msg);

    match action {
        RouteAction::PassThrough => Some(msg),
        RouteAction::Drop => {
            debug!(method = ?msg.method(), "dropping H→U message");
            None
        }
        RouteAction::Intercept => Some(translate_helix_intercept(msg, translator)),
    }
}

fn log_helix_message(msg: &Message, action: RouteAction) {
    if !tracing::enabled!(Level::DEBUG) {
        return;
    }

    match msg.method() {
        Some("textDocument/completion") => {
            if let Some(fields) = completion_log_fields(msg, true) {
                debug!(
                    ?action,
                    uri = fields.uri,
                    line = fields.line,
                    character = fields.character,
                    trigger_kind = fields.trigger_kind,
                    "received Helix completion request"
                );
            }
        }
        Some("textDocument/didOpen")
        | Some("textDocument/didChange")
        | Some("textDocument/didClose") => {
            debug!(method = ?msg.method(), "received Helix document sync message");
        }
        _ => {}
    }
}

fn translate_helix_intercept(msg: Message, translator: &Translator) -> Message {
    match msg.method() {
        Some("textDocument/completion") => translator.translate_request(msg),
        Some("$/cancelRequest") => translator.try_remap_cancel(msg),
        Some(method) => {
            debug_assert!(false, "unexpected intercepted Helix method: {method}");
            msg
        }
        None => msg,
    }
}

fn completion_log_fields(msg: &Message, debug_enabled: bool) -> Option<CompletionLogFields<'_>> {
    if !debug_enabled || msg.method() != Some("textDocument/completion") {
        return None;
    }

    let params = msg.params.as_ref();
    Some(CompletionLogFields {
        uri: params
            .and_then(|value| value.get("textDocument"))
            .and_then(|value| value.get("uri"))
            .and_then(Value::as_str)
            .unwrap_or("<missing>"),
        line: params
            .and_then(|value| value.get("position"))
            .and_then(|value| value.get("line"))
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        character: params
            .and_then(|value| value.get("position"))
            .and_then(|value| value.get("character"))
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        trigger_kind: params
            .and_then(|value| value.get("context"))
            .and_then(|value| value.get("triggerKind"))
            .and_then(Value::as_u64),
    })
}

struct CompletionLogFields<'a> {
    uri: &'a str,
    line: u64,
    character: u64,
    trigger_kind: Option<u64>,
}

async fn run_upstream_reader(
    upstream: &mut Upstream,
    helix_tx: mpsc::Sender<Message>,
    translator: Translator,
) {
    loop {
        let msg = match upstream.recv().await {
            Some(m) => m,
            None => {
                debug!("upstream stdout closed");
                break;
            }
        };

        // Responses to proxied completions are intercepted by the translator
        // regardless of what the router says (responses have no method name).
        let msg = if translator.is_pending_response(&msg) {
            translator.try_translate_response(msg)
        } else {
            let action = router::route(msg.method(), Direction::UpstreamToHelix);
            match action {
                RouteAction::PassThrough => msg,
                RouteAction::Drop => {
                    debug!(method = ?msg.method(), "dropping U→H message");
                    continue;
                }
                RouteAction::Intercept => msg,
            }
        };

        if helix_tx.send(msg).await.is_err() {
            warn!("helix channel closed while forwarding from upstream");
            break;
        }
    }
    debug!("upstream reader task exiting");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn completion_log_fields_skip_extraction_when_debug_is_disabled() {
        let msg = Message::request(
            1,
            "textDocument/completion",
            json!({
                "textDocument": { "uri": "file:///src/main.rs" },
                "position": { "line": 7, "character": 3 },
                "context": { "triggerKind": 1 }
            }),
        );

        assert!(completion_log_fields(&msg, false).is_none());
    }

    #[test]
    fn completion_log_fields_extract_expected_values_when_enabled() {
        let msg = Message::request(
            1,
            "textDocument/completion",
            json!({
                "textDocument": { "uri": "file:///src/main.rs" },
                "position": { "line": 7, "character": 3 },
                "context": { "triggerKind": 1 }
            }),
        );

        let fields = completion_log_fields(&msg, true).expect("expected completion log fields");
        assert_eq!(fields.uri, "file:///src/main.rs");
        assert_eq!(fields.line, 7);
        assert_eq!(fields.character, 3);
        assert_eq!(fields.trigger_kind, Some(1));
    }
}
