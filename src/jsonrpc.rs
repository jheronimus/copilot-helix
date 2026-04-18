//! JSON-RPC 2.0 message types and Content-Length framing.
//!
//! The LSP transport is: `Content-Length: <N>\r\n\r\n<N bytes of UTF-8 JSON>`.
//! This module handles reading and writing that framing over any async I/O pair.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// ── Types ────────────────────────────────────────────────────────────────────

/// A JSON-RPC 2.0 message (request, response, or notification).
///
/// All three share the same wire format; callers use [`Message::kind`] or the
/// `is_*` helpers to distinguish them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub jsonrpc: String,
    /// Present on requests and responses; absent on notifications.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    /// Present on requests and notifications; absent on responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Request,
    Response,
    Notification,
}

impl Message {
    pub fn kind(&self) -> MessageKind {
        match (&self.id, &self.method) {
            (Some(_), Some(_)) => MessageKind::Request,
            (Some(_), None) => MessageKind::Response,
            (None, _) => MessageKind::Notification,
        }
    }

    pub fn is_request(&self) -> bool {
        self.kind() == MessageKind::Request
    }

    pub fn is_response(&self) -> bool {
        self.kind() == MessageKind::Response
    }

    pub fn is_notification(&self) -> bool {
        self.kind() == MessageKind::Notification
    }

    /// Convenience: method name for requests and notifications, `None` for responses.
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// Build a success response for the given request id.
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    /// Build an error response for the given request id.
    pub fn error_response(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: None,
            params: None,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }

    /// Build a notification (no id).
    pub fn notification(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    /// Build a request with the given numeric id.
    pub fn request(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self::request_with_id(Value::Number(id.into()), method, params)
    }

    /// Build a request with an arbitrary JSON-RPC id.
    pub fn request_with_id(id: Value, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }
}

// ── I/O ──────────────────────────────────────────────────────────────────────

/// Read one framed JSON-RPC message from `reader`.
///
/// Expects `Content-Length: <N>\r\n` optionally followed by other headers,
/// then `\r\n`, then exactly N bytes of UTF-8 JSON.
pub async fn read_message<R>(reader: &mut R) -> Result<Message>
where
    R: AsyncBufRead + Unpin,
{
    // Read headers until blank line.
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading header line")?;
        if n == 0 {
            bail!("connection closed while reading headers");
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break; // blank line ends headers
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = Some(
                rest.trim()
                    .parse::<usize>()
                    .context("parsing Content-Length value")?,
            );
        }
        // Ignore other headers (e.g. Content-Type).
    }

    let len = content_length.context("no Content-Length header found")?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .context("reading message body")?;

    serde_json::from_slice(&body).context("deserializing JSON-RPC message")
}

/// Write one framed JSON-RPC message to `writer`.
pub async fn write_message<W>(writer: &mut W, msg: &Message) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let body = serialize_message(msg)?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .context("writing header")?;
    writer.write_all(&body).await.context("writing body")?;
    writer.flush().await.context("flushing writer")?;
    Ok(())
}

fn serialize_message(msg: &Message) -> Result<Vec<u8>> {
    let mut value = serde_json::to_value(msg).context("serializing JSON-RPC message")?;

    // JSON-RPC success responses must carry a `result` member. When the
    // upstream server replies with `result: null`, serde deserializes that as
    // `None`, so re-serializing the struct would incorrectly drop the field.
    if msg.is_response() && msg.result.is_none() && msg.error.is_none() {
        value
            .as_object_mut()
            .context("serialized JSON-RPC message was not an object")?
            .insert("result".into(), Value::Null);
    }

    serde_json::to_vec(&value).context("encoding JSON-RPC message")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::BufReader;

    async fn round_trip(msg: &Message) -> Message {
        let mut buf = Vec::new();
        write_message(&mut buf, msg).await.unwrap();
        let mut reader = BufReader::new(buf.as_slice());
        read_message(&mut reader).await.unwrap()
    }

    #[tokio::test]
    async fn request_round_trip() {
        let msg = Message::request(1, "textDocument/completion", json!({"key": "value"}));
        let got = round_trip(&msg).await;
        assert_eq!(got.id, Some(json!(1)));
        assert_eq!(got.method(), Some("textDocument/completion"));
        assert!(got.is_request());
    }

    #[tokio::test]
    async fn notification_round_trip() {
        let msg = Message::notification("initialized", json!({}));
        let got = round_trip(&msg).await;
        assert!(got.is_notification());
        assert_eq!(got.method(), Some("initialized"));
    }

    #[tokio::test]
    async fn response_round_trip() {
        let msg = Message::success(json!(42), json!({"items": []}));
        let got = round_trip(&msg).await;
        assert!(got.is_response());
        assert_eq!(got.id, Some(json!(42)));
        assert_eq!(got.result, Some(json!({"items": []})));
    }

    #[tokio::test]
    async fn multi_message_stream() {
        let msgs = vec![
            Message::request(1, "initialize", json!({})),
            Message::notification("initialized", json!({})),
            Message::success(json!(1), json!({"capabilities": {}})),
        ];
        let mut buf = Vec::new();
        for m in &msgs {
            write_message(&mut buf, m).await.unwrap();
        }
        let mut reader = BufReader::new(buf.as_slice());
        for expected in &msgs {
            let got = read_message(&mut reader).await.unwrap();
            assert_eq!(got.method, expected.method);
            assert_eq!(got.id, expected.id);
        }
    }

    #[tokio::test]
    async fn ignores_extra_headers() {
        // Some LSP clients send Content-Type alongside Content-Length.
        let body = r#"{"jsonrpc":"2.0","method":"ping","id":1}"#;
        let raw = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{}",
            body.len(),
            body
        );
        let mut reader = BufReader::new(raw.as_bytes());
        let msg = read_message(&mut reader).await.unwrap();
        assert_eq!(msg.method(), Some("ping"));
    }

    #[tokio::test]
    async fn missing_content_length_errors() {
        let raw = "Content-Type: application/json\r\n\r\n{}";
        let mut reader = BufReader::new(raw.as_bytes());
        assert!(read_message(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn eof_on_empty_stream_errors() {
        let mut reader = BufReader::new(b"".as_slice());
        assert!(read_message(&mut reader).await.is_err());
    }

    #[tokio::test]
    async fn response_without_payload_serializes_as_null_result() {
        let raw = "Content-Length: 38\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":null}";
        let mut reader = BufReader::new(raw.as_bytes());
        let msg = read_message(&mut reader).await.unwrap();

        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let encoded = String::from_utf8(buf).unwrap();
        assert!(encoded.contains(r#""result":null"#));
    }
}
