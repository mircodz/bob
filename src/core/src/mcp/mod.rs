//! Minimal MCP (Model Context Protocol) client. MCP is JSON-RPC 2.0 exchanged
//! either over a spawned subprocess's stdin/stdout (stdio transport) or over
//! HTTP POST with SSE/JSON responses (streamable-HTTP transport). We perform the
//! initialize handshake, list the server's tools, and expose each as a native
//! `Tool` (namespaced `<server>_<tool>`) so the agent can't tell them apart from
//! built-ins.

mod http;
mod stdio;

use crate::core::config::McpServerConfig;
use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// A transport carries JSON-RPC requests/notifications to one MCP server.
#[async_trait]
pub(crate) trait Transport: Send + Sync {
    /// Send a request and await its `result` (mapping a JSON-RPC error to Err).
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value>;
    /// Fire-and-forget notification (no response expected).
    async fn notify(&self, method: &str, params: Value);
}

/// A live connection to one MCP server, over whichever transport it uses.
#[derive(Clone)]
pub struct McpClient {
    name: String,
    transport: Arc<dyn Transport>,
}

impl McpClient {
    /// Connect to the server (spawning it for stdio, or opening an HTTP session),
    /// run the initialize handshake, and return the client.
    pub async fn connect(cfg: &McpServerConfig) -> anyhow::Result<McpClient> {
        let transport: Arc<dyn Transport> = if cfg.is_http() {
            Arc::new(http::HttpTransport::connect(cfg).await?)
        } else {
            Arc::new(stdio::StdioTransport::connect(cfg).await?)
        };
        let client = McpClient {
            name: cfg.name.clone(),
            transport,
        };

        // initialize handshake.
        client
            .transport
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
        client
            .transport
            .notify("notifications/initialized", json!({}))
            .await;

        Ok(client)
    }

    /// List the server's tools and wrap each as a native namespaced Tool.
    pub async fn list_tools(&self) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
        let result = self.transport.request("tools/list", json!({})).await?;
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
                name: format!("{}_{}", self.name, raw_name),
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
            .transport
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

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        self.client
            .call_tool(&self.raw_name, input)
            .await
            .map_err(|e| ToolError::failed(e.to_string()))
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
