//! Newline-delimited JSON-RPC over an owned server process.

use super::Transport;
use crate::core::config::McpServerConfig;
use crate::core::process::ManagedProcess;
use crate::lsp::transport::RpcConnection;
use async_trait::async_trait;
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

pub(crate) struct StdioTransport {
    connection: Arc<RpcConnection>,
}

impl StdioTransport {
    pub(crate) async fn connect(cfg: &McpServerConfig) -> anyhow::Result<Self> {
        let mut command = tokio::process::Command::new(&cfg.command);
        command
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ManagedProcess::spawn(&mut command)
            .map_err(|e| anyhow::anyhow!("failed to start MCP server '{}': {}", cfg.name, e))?;
        let stdin = child
            .take_stdin()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child
            .take_stdout()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
        let connection = RpcConnection::new(stdin, Some(child), false, Duration::from_secs(30));
        spawn_reader(&connection, stdout);
        Ok(Self { connection })
    }
}

fn spawn_reader(
    connection: &Arc<RpcConnection>,
    stdout: impl tokio::io::AsyncRead + Send + Unpin + 'static,
) {
    let weak = Arc::downgrade(connection);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Some(connection) = weak.upgrade() else {
                return;
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(message) => connection.receive(message),
                Err(_) => break,
            }
        }
        if let Some(connection) = weak.upgrade() {
            connection.disconnect();
        }
    });
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.connection.request(method, params).await
    }

    async fn notify(&self, method: &str, params: Value) {
        let _ = self.connection.notify(method, params).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn reader_ignores_blank_lines_and_notifications_and_preserves_unicode_reply() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let (reader, mut writer) = tokio::io::duplex(1024);
        spawn_reader(&connection, reader);
        let transport = StdioTransport {
            connection: connection.clone(),
        };
        let mut request = Box::pin(transport.request("fixture", Value::Null));
        assert!(futures::poll!(&mut request).is_pending());
        writer
            .write_all(b"\n \t\r\n{\"method\":\"notice\"}\n{\"id\":99,\"result\":\"wrong\"}\n")
            .await
            .unwrap();
        writer
            .write_all("{\"id\":1,\"result\":\"é😀\\ntext\"}\r\n".as_bytes())
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), request)
                .await
                .unwrap()
                .unwrap(),
            "é😀\ntext"
        );
        assert!(!connection.is_closed());
    }

    #[tokio::test]
    async fn server_error_is_not_a_parse_failure_and_next_request_can_succeed() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let (reader, mut writer) = tokio::io::duplex(1024);
        spawn_reader(&connection, reader);
        let transport = StdioTransport {
            connection: connection.clone(),
        };
        let mut request = Box::pin(transport.request("fixture", Value::Null));
        assert!(futures::poll!(&mut request).is_pending());
        writer
            .write_all(b"{\"id\":1,\"error\":{\"code\":-32602,\"message\":\"invalid params\"}}\n")
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(1), request)
            .await
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("JSON-RPC error:"));
        assert!(error.contains("invalid params"));
        assert!(!connection.is_closed());
        let mut next = Box::pin(transport.request("next", Value::Null));
        assert!(futures::poll!(&mut next).is_pending());
        writer
            .write_all(b"{\"id\":2,\"result\":null}\n")
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), next)
                .await
                .unwrap()
                .unwrap(),
            Value::Null
        );
    }

    #[tokio::test]
    async fn reader_does_not_keep_stdio_connection_alive() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let (reader, _writer) = tokio::io::duplex(16);
        spawn_reader(&connection, reader);
        let weak = Arc::downgrade(&connection);
        drop(connection);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn eof_and_malformed_frames_release_stdio_waiters() {
        for bytes in [b"".as_slice(), b"not json\n".as_slice()] {
            let connection =
                RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
            let (reader, mut server) = tokio::io::duplex(256);
            spawn_reader(&connection, reader);
            let transport = StdioTransport {
                connection: connection.clone(),
            };
            let mut request = Box::pin(transport.request("fixture", Value::Null));
            assert!(futures::poll!(&mut request).is_pending());
            server.write_all(bytes).await.unwrap();
            drop(server);
            let result = tokio::time::timeout(Duration::from_millis(200), request)
                .await
                .unwrap();
            assert!(result.unwrap_err().to_string().contains("closed"));
            assert!(connection.is_closed());
        }
    }
}
