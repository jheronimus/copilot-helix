//! GitHub Copilot authentication handling.
//!
//! Two entry points:
//! - [`check_and_warn`] — called once after proxy initialisation; sends a
//!   `window/showMessage` to Helix when the user is not signed in.
//! - [`run_auth_flow`] — the interactive `--auth` CLI mode that walks the user
//!   through the GitHub device-flow OAuth dance.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::{sync::mpsc, time};
use tracing::debug;

use crate::{
    config::Config,
    initializer::{build_did_change_configuration, build_initialization_options},
    jsonrpc::Message,
    upstream::Upstream,
};

const CHECK_STATUS_REQUEST_ID: &str = "copilot-helix:auth:check-status";
const INITIALIZE_REQUEST_ID: &str = "copilot-helix:auth:initialize";
const SIGN_IN_REQUEST_ID: &str = "copilot-helix:auth:sign-in";
const EXECUTE_COMMAND_REQUEST_ID: &str = "copilot-helix:auth:execute-command";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    Authenticated(Option<String>),
    Unauthenticated(String),
}

// ── Proxy-mode: status check ──────────────────────────────────────────────────

/// Send `checkStatus` to the language server after initialisation.
///
/// If the response indicates the user is not signed in, sends a
/// `window/showMessage` warning to Helix via `helix_tx`.
/// Any upstream notifications received while waiting are forwarded to Helix.
pub async fn check_and_warn(
    upstream_tx: &mpsc::Sender<Message>,
    upstream: &mut Upstream,
    helix_tx: &mpsc::Sender<Message>,
) -> Result<()> {
    let request_id = internal_request_id(CHECK_STATUS_REQUEST_ID);
    debug!("sending checkStatus");

    upstream_tx
        .send(Message::request_with_id(
            request_id.clone(),
            "checkStatus",
            json!({ "options": { "localChecksOnly": true } }),
        ))
        .await
        .map_err(|_| anyhow::anyhow!("upstream closed before checkStatus"))?;

    let status = await_response(&request_id, upstream, Some(helix_tx))
        .await
        .context("waiting for checkStatus response")?;

    match classify_status(&status) {
        AuthStatus::Authenticated(Some(user)) => {
            debug!("copilot authenticated as {user}");
        }
        AuthStatus::Authenticated(None) => {}
        AuthStatus::Unauthenticated(status_str) => {
            debug!("copilot not authenticated: {status_str}");
            let _ = helix_tx
                .send(Message::notification(
                    "window/showMessage",
                    json!({
                        "type": 2,  // Warning
                        "message": "GitHub Copilot: not authenticated. Run: copilot-helix --auth"
                    }),
                ))
                .await;
        }
    }

    Ok(())
}

pub async fn check_auth_status() -> Result<AuthStatus> {
    let config = Config::detect()?;
    let mut upstream = Upstream::spawn(&config).await?;

    init_upstream(&mut upstream).await?;
    request_auth_status(&mut upstream, false).await
}

async fn request_auth_status(upstream: &mut Upstream, local_checks_only: bool) -> Result<AuthStatus> {
    let request_id = internal_request_id(CHECK_STATUS_REQUEST_ID);
    upstream
        .send(Message::request_with_id(
            request_id.clone(),
            "checkStatus",
            json!({ "options": { "localChecksOnly": local_checks_only } }),
        ))
        .await?;

    let status = await_response(&request_id, upstream, None).await?;
    Ok(classify_status(&status))
}

fn classify_status(status: &Value) -> AuthStatus {
    let status_str = status
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_owned();
    let user = status
        .get("user")
        .and_then(Value::as_str)
        .map(str::to_owned);

    match status_str.as_str() {
        "OK" | "MaybeOK" | "AlreadySignedIn" => AuthStatus::Authenticated(user),
        _ => AuthStatus::Unauthenticated(status_str),
    }
}

// ── --auth mode: interactive OAuth device flow ────────────────────────────────

const AUTH_TIMEOUT: time::Duration = time::Duration::from_secs(600); // 10 min

/// Interactive authentication flow.  Runs to completion and exits cleanly.
pub async fn run_auth_flow() -> Result<()> {
    let config = Config::detect()?;
    let mut upstream = Upstream::spawn(&config).await?;

    // Minimal LSP initialise — no Helix side, just enough to use auth methods.
    init_upstream(&mut upstream).await?;

    // Check whether the user is already signed in.
    match request_auth_status(&mut upstream, false).await? {
        AuthStatus::Authenticated(Some(user)) => {
            println!("Already authenticated as GitHub user {user}");
            return Ok(());
        }
        AuthStatus::Authenticated(None) => {
            println!("Already authenticated with GitHub Copilot");
            return Ok(());
        }
        AuthStatus::Unauthenticated(_) => {}
    }

    // Kick off the device flow.
    let sign_in_id = internal_request_id(SIGN_IN_REQUEST_ID);
    upstream
        .send(Message::request_with_id(
            sign_in_id.clone(),
            "signIn",
            json!({}),
        ))
        .await?;

    let sign_in_data = await_response(&sign_in_id, &mut upstream, None).await?;

    let verification_uri = sign_in_data
        .get("verificationUri")
        .and_then(Value::as_str)
        .context("signIn response missing verificationUri")?;
    let user_code = sign_in_data
        .get("userCode")
        .and_then(Value::as_str)
        .context("signIn response missing userCode")?;

    // If the server reports we're already signed in at this point, we're done.
    if sign_in_data.get("status").and_then(Value::as_str) == Some("AlreadySignedIn") {
        let user = sign_in_data
            .get("user")
            .and_then(Value::as_str)
            .unwrap_or("?");
        println!("Authenticated as GitHub user {user}");
        return Ok(());
    }

    let command = sign_in_data.get("command").cloned().unwrap_or(Value::Null);

    eprintln!("Open this URL in your browser:");
    eprintln!("  {verification_uri}");
    eprintln!();
    eprintln!("Enter this code when prompted:");
    eprintln!("  {user_code}");
    eprintln!();
    eprintln!("Waiting for authentication…");

    // Ask the language server to poll GitHub until the user completes the flow.
    let result = time::timeout(AUTH_TIMEOUT, confirm_sign_in(&mut upstream, command)).await;

    match result {
        Ok(Ok(user)) => {
            println!("Authenticated as GitHub user {user}");
            Ok(())
        }
        Ok(Err(e)) => Err(e),
        Err(_) => anyhow::bail!("authentication timed out after 10 minutes"),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Minimal LSP initialise / initialized / didChangeConfiguration sequence.
/// Used only in `--auth` mode where there is no Helix client.
async fn init_upstream(upstream: &mut Upstream) -> Result<()> {
    let request_id = internal_request_id(INITIALIZE_REQUEST_ID);
    let initialization_options = build_initialization_options("unknown");

    upstream
        .send(Message::request_with_id(
            request_id.clone(),
            "initialize",
            json!({
                "capabilities": {
                    "workspace": {
                        "didChangeConfiguration": { "dynamicRegistration": true }
                    }
                },
                "initializationOptions": initialization_options
            }),
        ))
        .await?;

    // Wait for initialize response; discard notifications.
    await_response(&request_id, upstream, None).await?;

    upstream
        .send(Message::notification("initialized", json!({})))
        .await?;

    upstream.send(build_did_change_configuration()).await?;

    Ok(())
}

/// Execute the `workspace/executeCommand` from `signIn` and return the
/// authenticated username on success.
async fn confirm_sign_in(upstream: &mut Upstream, command: Value) -> Result<String> {
    let request_id = internal_request_id(EXECUTE_COMMAND_REQUEST_ID);
    upstream
        .send(Message::request_with_id(
            request_id.clone(),
            "workspace/executeCommand",
            command,
        ))
        .await?;

    let result = await_response(&request_id, upstream, None).await?;
    let user = result
        .get("user")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned();
    Ok(user)
}

/// Read messages from `upstream` until a response with `id == req_id` arrives.
///
/// `helix_tx` is optional: when `Some`, any notifications received while
/// waiting are forwarded to Helix rather than discarded.
async fn await_response(
    req_id: &Value,
    upstream: &mut Upstream,
    helix_tx: Option<&mpsc::Sender<Message>>,
) -> Result<Value> {
    loop {
        let msg = upstream
            .recv()
            .await
            .context("upstream closed while waiting for response")?;

        if msg.is_response() && msg.id.as_ref() == Some(req_id) {
            if let Some(err) = msg.error {
                anyhow::bail!("RPC error {}: {}", err.code, err.message);
            }
            return Ok(msg.result.unwrap_or(Value::Null));
        }

        // Not our response — only forward notifications. Unrelated responses
        // would confuse Helix because it never sent the corresponding request.
        if msg.is_notification() {
            if let Some(tx) = helix_tx {
                let _ = tx.send(msg).await;
            }
        }
    }
}

fn internal_request_id(name: &str) -> Value {
    Value::String(name.to_owned())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity-check that the messages we build are well-formed.
    #[test]
    fn check_status_request_shape() {
        let msg = Message::request(
            u64::MAX,
            "checkStatus",
            json!({ "options": { "localChecksOnly": true } }),
        );
        assert!(msg.is_request());
        assert_eq!(msg.method(), Some("checkStatus"));
        assert_eq!(msg.params.unwrap()["options"]["localChecksOnly"], true);
    }

    #[test]
    fn window_show_message_is_notification() {
        let msg = Message::notification(
            "window/showMessage",
            json!({ "type": 2, "message": "test" }),
        );
        assert!(msg.is_notification());
        assert_eq!(msg.params.unwrap()["type"], 2);
    }

    #[test]
    fn classify_status_recognizes_authenticated_user() {
        let status = classify_status(&json!({
            "status": "OK",
            "user": "alice"
        }));

        assert_eq!(status, AuthStatus::Authenticated(Some("alice".into())));
    }

    #[test]
    fn classify_status_recognizes_missing_auth() {
        let status = classify_status(&json!({
            "status": "NotAuthorized"
        }));

        assert_eq!(status, AuthStatus::Unauthenticated("NotAuthorized".into()));
    }
}
