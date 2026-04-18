//! Manages the resolved Copilot language server subprocess.
//!
//! [`Upstream`] spawns the configured language server command, wires its
//! stdin/stdout to two background tasks, and exposes a simple send/recv
//! interface.
//!
//! Dropping [`Upstream`] sends a shutdown signal to the background tasks and
//! the subprocess will be killed when its stdin pipe is closed.

use anyhow::{Context, Result};
use tokio::{
    io::BufReader,
    process::{Child, Command},
    sync::mpsc,
    task::JoinHandle,
};
use tracing::{debug, error, warn};

use crate::{
    config::Config,
    jsonrpc::{self, Message},
};

// Channel capacity: large enough to absorb bursts without back-pressure.
const CHANNEL_CAP: usize = 256;

/// Handle to the running language server subprocess.
pub struct Upstream {
    /// Send a message to the language server's stdin.
    tx: mpsc::Sender<Message>,
    /// Receive messages from the language server's stdout.
    /// `None` signals that the process has exited.
    rx: mpsc::Receiver<Option<Message>>,
    // Kept alive so tasks run until Upstream is dropped.
    _writer_task: JoinHandle<()>,
    _reader_task: JoinHandle<()>,
    _stderr_task: JoinHandle<()>,
    // Child is kept so the OS doesn't reap it immediately; it will be killed
    // when all stdio handles are closed (i.e. when tasks finish).
    _child: Child,
}

impl Upstream {
    /// Spawn the language server and start background I/O tasks.
    pub async fn spawn(config: &Config) -> Result<Self> {
        debug!(
            program = %config.program.display(),
            args = ?config.args,
            "spawning language server"
        );

        let mut child = Command::new(&config.program)
            .args(&config.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("failed to spawn copilot-language-server")?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        // proxy → language server
        let (proxy_tx, mut writer_rx) = mpsc::channel::<Message>(CHANNEL_CAP);

        // language server → proxy  (None = process closed stdout)
        let (reader_tx, proxy_rx) = mpsc::channel::<Option<Message>>(CHANNEL_CAP);

        // Writer task: drain the proxy→ls channel into the child's stdin.
        let writer_task = tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(msg) = writer_rx.recv().await {
                if let Err(e) = jsonrpc::write_message(&mut stdin, &msg).await {
                    error!(error = %e, "write to language server failed");
                    break;
                }
            }
            debug!("writer task exiting");
        });

        // Reader task: forward messages from child stdout to the proxy.
        let reader_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                match jsonrpc::read_message(&mut reader).await {
                    Ok(msg) => {
                        if reader_tx.send(Some(msg)).await.is_err() {
                            // Proxy dropped its receiver; nothing to do.
                            break;
                        }
                    }
                    Err(e) => {
                        // EOF or parse error — language server exited or is broken.
                        debug!(error = %e, "language server stdout closed");
                        let _ = reader_tx.send(None).await;
                        break;
                    }
                }
            }
            debug!("reader task exiting");
        });

        // Stderr task: log language server stderr at debug level.
        let stderr_task = tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(target: "copilot_ls", "{}", line);
            }
        });

        Ok(Self {
            tx: proxy_tx,
            rx: proxy_rx,
            _writer_task: writer_task,
            _reader_task: reader_task,
            _stderr_task: stderr_task,
            _child: child,
        })
    }

    /// Returns a cloneable sender that other tasks can use to write to the
    /// language server without holding a mutable reference to [`Upstream`].
    pub(crate) fn sender(&self) -> mpsc::Sender<Message> {
        self.tx.clone()
    }

    /// Send a message to the language server.
    ///
    /// Returns an error if the subprocess has already exited and the writer
    /// task has shut down.
    pub async fn send(&self, msg: Message) -> Result<()> {
        self.tx
            .send(msg)
            .await
            .map_err(|_| anyhow::anyhow!("language server writer task has shut down"))
    }

    /// Receive the next message from the language server.
    ///
    /// Returns `None` when the language server has exited (stdout was closed).
    /// Returns `Some(msg)` for each message received.
    pub async fn recv(&mut self) -> Option<Message> {
        // The inner Option distinguishes "process exited" (Some(None)) from
        // "channel closed unexpectedly" (None from recv()), treating both as exit.
        self.rx.recv().await.flatten()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        warn!("Upstream dropped — language server subprocess will be killed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{ffi::OsString, path::PathBuf};

    /// Verify that spawning with a nonexistent binary gives a clear error.
    #[tokio::test]
    async fn spawn_bad_program_fails() {
        let config = Config {
            program: PathBuf::from("/nonexistent/program"),
            args: vec![OsString::from("--stdio")],
        };
        let result = Upstream::spawn(&config).await;
        assert!(result.is_err(), "expected spawn to fail with bad node path");
    }

    /// Smoke-test the generic spawn path using `node -e` as a trivial echo server.
    ///
    /// The script reads one framed JSON-RPC message and echoes it back.
    #[tokio::test]
    async fn echo_round_trip() {
        let node = match which("node") {
            Some(p) => p,
            None => {
                eprintln!("skipping echo_round_trip: node not found in PATH");
                return;
            }
        };

        // A tiny Node.js script that reads one Content-Length message and
        // writes it back unchanged, then exits.
        let script = r#"
const chunks = [];
process.stdin.on('data', chunk => {
    chunks.push(chunk);
    const buf = Buffer.concat(chunks);
    const headerEnd = buf.indexOf('\r\n\r\n');
    if (headerEnd === -1) return;
    const header = buf.slice(0, headerEnd).toString();
    const match = header.match(/Content-Length:\s*(\d+)/i);
    if (!match) return;
    const len = parseInt(match[1], 10);
    const bodyStart = headerEnd + 4;
    if (buf.length < bodyStart + len) return;
    const body = buf.slice(bodyStart, bodyStart + len);
    const response = `Content-Length: ${body.length}\r\n\r\n`;
    process.stdout.write(response);
    process.stdout.write(body);
    process.exit(0);
});
"#;

        let config = Config {
            program: node,
            args: vec![OsString::from("-e"), OsString::from(script)],
        };
        let mut upstream = Upstream::spawn(&config).await.expect("spawn failed");

        let msg = Message::request(1, "ping", json!({"hello": "world"}));
        upstream.send(msg).await.expect("write failed");

        let got = upstream.recv().await.expect("read failed");

        assert_eq!(got.id, Some(json!(1)));
        assert_eq!(got.method(), Some("ping"));
    }

    fn which(name: &str) -> Option<PathBuf> {
        let path_var = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }
}
