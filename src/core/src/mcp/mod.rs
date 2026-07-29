//! Minimal MCP (Model Context Protocol) client over stdio. No SDK — MCP is just
//! newline-delimited JSON-RPC 2.0 over a subprocess's stdin/stdout. We spawn the
//! server, perform the initialize handshake, list its tools, and expose each as
//! a native `Tool` (namespaced `<server>.<tool>`) so the agent can't tell them
//! apart from built-ins.

use crate::core::config::McpServerConfig;
use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

/// A live connection to one MCP server. Cloneable handle; the actual process +
/// pending-request map live behind Arcs so wrapped tools can call back in.
#[derive(Clone)]
pub struct McpClient {
    inner: Arc<McpInner>,
}

struct McpInner {
    name: String,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    next_id: AtomicI64,
    // Keep the child alive for the life of the client.
    _child: tokio::sync::Mutex<Child>,
}

impl McpClient {
    /// Spawn the server, run the initialize handshake, and return the client.
    pub async fn connect(cfg: &McpServerConfig) -> anyhow::Result<McpClient> {
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

        let inner = Arc::new(McpInner {
            name: cfg.name.clone(),
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            _child: tokio::sync::Mutex::new(child),
        });

        // Background reader: match each JSON-RPC response to its pending sender.
        {
            let inner = inner.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                        if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
                            if let Some(tx) = inner.pending.lock().unwrap().remove(&id) {
                                let _ = tx.send(msg);
                            }
                        }
                        // Notifications (no id) are ignored.
                    }
                }
            });
        }

        let client = McpClient { inner };

        // initialize handshake.
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "bob", "version": "0.0.1" }
                }),
            )
            .await?;
        // notifications/initialized (fire-and-forget).
        client.notify("notifications/initialized", json!({})).await;

        Ok(client)
    }

    /// List the server's tools and wrap each as a native namespaced Tool.
    pub async fn list_tools(&self) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();

        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for t in tools {
            let raw_name = t["name"].as_str().unwrap_or("").to_string();
            if raw_name.is_empty() {
                continue;
            }
            let spec = ToolSpec {
                name: format!("{}.{}", self.inner.name, raw_name),
                description: t["description"].as_str().unwrap_or("").to_string(),
                input_schema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            };
            out.push(Arc::new(McpTool {
                client: self.clone(),
                raw_name,
                spec,
            }));
        }
        Ok(out)
    }

    /// Call a tool on the server, returning its text content joined.
    async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<String> {
        let result = self
            .request("tools/call", json!({ "name": name, "arguments": args }))
            .await?;
        // result.content is an array of {type, text|...}.
        let parts = result
            .get("content")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();
        let text = parts
            .iter()
            .map(|p| match p["type"].as_str() {
                Some("text") => p["text"].as_str().unwrap_or("").to_string(),
                _ => p.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        if result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            Ok(format!("error: {}", text))
        } else {
            Ok(text)
        }
    }

    /// Send a JSON-RPC request and await its response `result` (or error).
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.write_line(&msg).await?;

        // Await the matching response (with a generous timeout).
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

    async fn write_line(&self, msg: &Value) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(msg)?;
        line.push('\n');
        let mut stdin = self.inner.stdin.lock().await;
        stdin.write_all(line.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

/// A single MCP tool, wrapped to look like a native Tool.
struct McpTool {
    client: McpClient,
    /// The tool's name on the server (without the server namespace prefix).
    raw_name: String,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> String {
        match self.client.call_tool(&self.raw_name, input).await {
            Ok(text) => text,
            Err(e) => format!("error: {}", e),
        }
    }
}

/// Connect to every configured MCP server and return all their tools flattened,
/// plus a notice string per server (connected N tools / failed). Servers that
/// fail to connect are skipped (not fatal).
pub async fn connect_all(configs: &[McpServerConfig]) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut notices: Vec<String> = Vec::new();
    for cfg in configs {
        match McpClient::connect(cfg).await {
            Ok(client) => match client.list_tools().await {
                Ok(t) => {
                    notices.push(format!("MCP '{}': {} tool(s)", cfg.name, t.len()));
                    // Each wrapped McpTool holds an Arc clone of the client, so
                    // the connection (and its subprocess) stays alive as long as
                    // any of its tools remain registered.
                    tools.extend(t);
                }
                Err(e) => notices.push(format!("MCP '{}': list failed: {}", cfg.name, e)),
            },
            Err(e) => notices.push(format!("MCP '{}': {}", cfg.name, e)),
        }
    }
    (tools, notices)
}
