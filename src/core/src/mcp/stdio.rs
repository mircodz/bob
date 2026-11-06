//! Newline-delimited JSON-RPC over an owned server process.

use super::{Transport, MAX_FRAME_BYTES};
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
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl StdioTransport {
    pub(crate) async fn connect(
        cfg: &McpServerConfig,
        workspace: Option<&std::path::Path>,
    ) -> anyhow::Result<Self> {
        let mut command = tokio::process::Command::new(&cfg.command);
        if let Some(workspace) = workspace {
            command.current_dir(workspace);
        }
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
        let reader = spawn_reader(&connection, stdout);
        Ok(Self { connection, reader })
    }
}

async fn read_frame(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
) -> anyhow::Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            anyhow::ensure!(frame.is_empty(), "incomplete MCP frame");
            return Ok(None);
        }
        let end = chunk.iter().position(|byte| *byte == b'\n');
        let count = end.map(|end| end + 1).unwrap_or(chunk.len());
        anyhow::ensure!(
            frame.len() + count <= MAX_FRAME_BYTES,
            "MCP frame exceeds byte limit"
        );
        frame.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(frame));
        }
    }
}

fn spawn_reader(
    connection: &Arc<RpcConnection>,
    stdout: impl tokio::io::AsyncRead + Send + Unpin + 'static,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(connection);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        while let Ok(Some(line)) = read_frame(&mut reader).await {
            let Some(connection) = weak.upgrade() else {
                return;
            };
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            match serde_json::from_slice::<Value>(&line) {
                Ok(message) => connection.receive(message),
                Err(_) => break,
            }
        }
        if let Some(connection) = weak.upgrade() {
            connection.disconnect();
        }
    })
}

#[async_trait]
impl Transport for StdioTransport {
    fn is_closed(&self) -> bool {
        self.connection.is_closed()
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        check_request_size(method, &params)?;
        self.connection.request(method, params).await
    }

    async fn notify(&self, method: &str, params: Value) {
        if check_request_size(method, &params).is_ok() {
            let _ = self.connection.notify(method, params).await;
        }
    }
}

fn check_request_size(method: &str, params: &Value) -> anyhow::Result<()> {
    let frame = serde_json::json!({"jsonrpc":"2.0","id":i64::MAX,"method":method,"params":params});
    anyhow::ensure!(
        serde_json::to_vec(&frame)?.len() < MAX_FRAME_BYTES,
        "MCP request exceeds byte limit"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[cfg(windows)]
    #[tokio::test]
    async fn native_windows_mcp_process_preserves_unicode_paths_and_cleans_up() {
        use crate::test_support::windows_rpc::{Process, Server};
        use crate::test_support::TestDir;
        use serde_json::json;

        let dir = TestDir::new();
        let server = Server::new(&dir, false);
        let cfg = McpServerConfig {
            name: "native-windows-fixture".into(),
            command: server.command,
            args: server.args,
            env: [("BOB_RPC_FIXTURE".into(), "env 日本語".into())]
                .into_iter()
                .collect(),
            url: None,
            oauth: None,
        };
        let transport = StdioTransport::connect(&cfg, Some(&server.root))
            .await
            .unwrap();
        let params = json!({"text":"quotes \" and 日本語 😀\r\nnext"});
        let reply = tokio::time::timeout(
            Duration::from_secs(10),
            transport.request("fixture/echo", params.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        let process = Process::open(reply["pid"].as_u64().unwrap().try_into().unwrap());
        assert_eq!(reply["echo"], params);
        assert_eq!(reply["text"], "日本語 😀\nnext");
        assert_eq!(reply["environment"], "env 日本語");
        assert_eq!(
            std::fs::canonicalize(reply["cwd"].as_str().unwrap()).unwrap(),
            std::fs::canonicalize(&server.root).unwrap()
        );
        assert!(transport
            .request("fixture/error", Value::Null)
            .await
            .unwrap_err()
            .to_string()
            .contains("fixture rejection"));
        assert_eq!(
            transport
                .request("fixture/echo", json!({"after":"error"}))
                .await
                .unwrap()["echo"],
            json!({"after":"error"})
        );
        let mut pending = Box::pin(transport.request("fixture/stall", Value::Null));
        assert!(futures::poll!(&mut pending).is_pending());
        drop(pending);
        drop(transport);
        process.wait_exit().await;
    }

    #[tokio::test]
    async fn bounded_frames_reject_overflow_and_incomplete_eof() {
        let oversized = vec![b' '; MAX_FRAME_BYTES + 1];
        let error = read_frame(&mut BufReader::new(oversized.as_slice()))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("byte limit"));
        assert!(read_frame(&mut BufReader::new(b"{}".as_slice()))
            .await
            .unwrap_err()
            .to_string()
            .contains("incomplete"));
        let exact = [vec![b' '; MAX_FRAME_BYTES - 1], vec![b'\n']].concat();
        assert_eq!(
            read_frame(&mut BufReader::new(exact.as_slice()))
                .await
                .unwrap()
                .unwrap()
                .len(),
            MAX_FRAME_BYTES
        );
        assert!(check_request_size(
            "tools/call",
            &serde_json::json!({"text":"x".repeat(MAX_FRAME_BYTES)})
        )
        .is_err());
    }

    #[tokio::test]
    async fn dropping_transport_aborts_owned_reader() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let (stdout, mut writer) = tokio::io::duplex(16);
        let reader = spawn_reader(&connection, stdout);
        let aborted = reader.abort_handle();
        let transport = StdioTransport { connection, reader };
        drop(transport);
        tokio::task::yield_now().await;
        assert!(aborted.is_finished());
        assert!(writer.write_all(b"x").await.is_err());
    }

    #[tokio::test]
    async fn cancelled_requests_do_not_consume_later_replies_or_retry() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let (stdout, mut writer) = tokio::io::duplex(256);
        let reader = spawn_reader(&connection, stdout);
        let transport = StdioTransport {
            connection: connection.clone(),
            reader,
        };
        let mut first = Box::pin(transport.request("first", Value::Null));
        assert!(futures::poll!(&mut first).is_pending());
        drop(first);
        let mut second = Box::pin(transport.request("second", Value::Null));
        assert!(futures::poll!(&mut second).is_pending());
        writer
            .write_all(b"{\"id\":1,\"result\":\"late\"}\n{\"id\":2,\"result\":\"current\"}\n")
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), second)
                .await
                .unwrap()
                .unwrap(),
            "current"
        );
        assert!(!connection.is_closed());
    }

    #[tokio::test]
    async fn reader_ignores_blank_lines_and_notifications_and_preserves_unicode_reply() {
        let connection = RpcConnection::new(tokio::io::sink(), None, false, Duration::from_secs(1));
        let (reader, mut writer) = tokio::io::duplex(1024);
        let reader = spawn_reader(&connection, reader);
        let transport = StdioTransport {
            connection: connection.clone(),
            reader,
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
        let reader = spawn_reader(&connection, reader);
        let transport = StdioTransport {
            connection: connection.clone(),
            reader,
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
            let reader = spawn_reader(&connection, reader);
            let transport = StdioTransport {
                connection: connection.clone(),
                reader,
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
