//! Stdio MCP transport: spawn the server subprocess and exchange newline-delimited
//! JSON-RPC 2.0 over its stdin/stdout. A background reader matches each response to
//! its pending request by id.

use super::Transport;
use crate::core::config::McpServerConfig;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>;

pub(crate) struct StdioTransport {
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Pending,
    next_id: AtomicI64,
    // Keep the child alive for the life of the transport.
    _child: tokio::sync::Mutex<Child>,
}

impl StdioTransport {
    pub(crate) async fn connect(cfg: &McpServerConfig) -> anyhow::Result<StdioTransport> {
        let mut command = tokio::process::Command::new(&cfg.command);
        command
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = command
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to start MCP server '{}': {}", cfg.name, e))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        // Background reader: match each JSON-RPC response to its pending sender.
        {
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                        if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
                            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(msg);
                            }
                        }
                        // Notifications (no id) are ignored.
                    }
                }
            });
        }

        Ok(StdioTransport {
            stdin: tokio::sync::Mutex::new(stdin),
            pending,
            next_id: AtomicI64::new(1),
            _child: tokio::sync::Mutex::new(child),
        })
    }

    async fn write_line(&self, msg: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.write_line(&msg).await?;

        let resp = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| anyhow::anyhow!("MCP request '{}' timed out", method))?
            .map_err(|_| anyhow::anyhow!("MCP connection closed"))?;

        if let Some(err) = resp.get("error") {
            anyhow::bail!("MCP error: {}", err);
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.write_line(&msg).await;
    }
}
