//! MCP uses JSON-RPC over stdio or HTTP with JSON/SSE replies.
//! Server tools are registered as `<server>_<tool>`.

mod http;
mod stdio;

use crate::core::config::McpServerConfig;
use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

#[async_trait]
pub(crate) trait Transport: Send + Sync {
    /// JSON-RPC errors become `Err`, not successful results.
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value>;
    /// Notifications expect no reply; delivery errors are ignored.
    async fn notify(&self, method: &str, params: Value);
}

#[derive(Clone)]
pub struct McpClient {
    name: String,
    transport: Arc<dyn Transport>,
}

impl McpClient {
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
        client
            .transport
            .notify("notifications/initialized", json!({}))
            .await;

        Ok(client)
    }

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

    async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<String> {
        let result = self
            .transport
            .request("tools/call", json!({ "name": name, "arguments": args }))
            .await?;
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

struct McpTool {
    client: McpClient,
    /// Tool name on the server, without the local namespace prefix.
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
            .map(Into::into)
            .map_err(|e| ToolError::failed(e.to_string()))
    }
}

/// Connection and listing failures become notices; other servers remain available.
pub async fn connect_all(configs: &[McpServerConfig]) -> (Vec<Arc<dyn Tool>>, Vec<String>) {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    let mut notices: Vec<String> = Vec::new();
    for cfg in configs {
        match McpClient::connect(cfg).await {
            Ok(client) => match client.list_tools().await {
                Ok(t) => {
                    notices.push(format!("MCP '{}': {} tool(s)", cfg.name, t.len()));
                    // Registered tools keep the transport and subprocess alive.
                    tools.extend(t);
                }
                Err(e) => notices.push(format!("MCP '{}': list failed: {}", cfg.name, e)),
            },
            Err(e) => notices.push(format!("MCP '{}': {}", cfg.name, e)),
        }
    }
    (tools, notices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct ExpectedRequest {
        method: &'static str,
        params: Value,
        reply: anyhow::Result<Value>,
    }

    struct ScriptedTransport(Mutex<VecDeque<ExpectedRequest>>);

    #[async_trait]
    impl Transport for ScriptedTransport {
        async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
            let expected = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request");
            assert_eq!(method, expected.method);
            assert_eq!(params, expected.params);
            expected.reply
        }

        async fn notify(&self, method: &str, _params: Value) {
            panic!("unexpected notification: {method}");
        }
    }

    fn scripted_client(requests: Vec<ExpectedRequest>) -> (McpClient, Arc<ScriptedTransport>) {
        let transport = Arc::new(ScriptedTransport(Mutex::new(requests.into())));
        (
            McpClient {
                name: "fixture".into(),
                transport: transport.clone(),
            },
            transport,
        )
    }

    #[tokio::test]
    async fn listed_tools_preserve_metadata_and_execute_with_raw_server_name() {
        let schema = json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false});
        let arguments = json!({"query":"é😀","nested":{"values":[1,null,true]}});
        let (client, transport) = scripted_client(vec![
            ExpectedRequest {
                method: "tools/list",
                params: json!({}),
                reply: Ok(json!({"tools":[
                    {"name":"search","description":"Search a project","inputSchema":schema},
                    {"name":"ping"},
                    {"name":""},
                    {"description":"unnamed"},
                    {"name":42}
                ]})),
            },
            ExpectedRequest {
                method: "tools/call",
                params: json!({"name":"search","arguments":arguments}),
                reply: Ok(json!({"content":[{"type":"text","text":"found"}]})),
            },
        ]);
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        let spec = tools[0].spec();
        assert_eq!(spec.name, "fixture_search");
        assert_eq!(spec.description, "Search a project");
        assert_eq!(spec.input_schema, schema);
        let fallback = tools[1].spec();
        assert_eq!(fallback.name, "fixture_ping");
        assert_eq!(fallback.description, "");
        assert_eq!(
            fallback.input_schema,
            json!({"type":"object","properties":{}})
        );
        drop(client);
        let ctx = crate::test_support::files::tool_context(std::path::Path::new("."));
        assert_eq!(
            tools[0]
                .execute(arguments, &ctx)
                .await
                .unwrap()
                .as_text()
                .unwrap(),
            "found"
        );
        assert!(transport.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn content_blocks_keep_order_and_non_text_payloads() {
        let image = json!({"type":"image","data":"AA==","mimeType":"image/png"});
        let resource =
            json!({"type":"resource","resource":{"uri":"file:///fixture.rs","text":"source"}});
        let (client, transport) = scripted_client(vec![ExpectedRequest {
            method: "tools/call",
            params: json!({"name":"inspect","arguments":{}}),
            reply: Ok(json!({"content":[
                {"type":"text","text":"first\nsecond"}, image,
                {"type":"text","text":""}, resource,
                {"type":"text","text":"last"}
            ]})),
        }]);
        assert_eq!(
            client.call_tool("inspect", json!({})).await.unwrap(),
            format!("first\nsecond\n{image}\n\n{resource}\nlast")
        );
        assert!(transport.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn transport_errors_propagate_through_listing_and_tool_execution() {
        let (client, transport) = scripted_client(vec![
            ExpectedRequest {
                method: "tools/list",
                params: json!({}),
                reply: Err(anyhow::anyhow!("list denied")),
            },
            ExpectedRequest {
                method: "tools/call",
                params: json!({"name":"run","arguments":{"x":1}}),
                reply: Err(anyhow::anyhow!("connection lost")),
            },
        ]);
        assert_eq!(
            client.list_tools().await.err().unwrap().to_string(),
            "list denied"
        );
        let tool = McpTool {
            client,
            raw_name: "run".into(),
            spec: ToolSpec {
                name: "fixture_run".into(),
                description: String::new(),
                input_schema: json!({"type":"object"}),
            },
        };
        let ctx = crate::test_support::files::tool_context(std::path::Path::new("."));
        let error = tool.execute(json!({"x":1}), &ctx).await.unwrap_err();
        assert!(matches!(
            error.kind,
            crate::tools::registry::ToolErrorKind::Failed
        ));
        assert_eq!(error.message, "connection lost");
        assert!(transport.0.lock().unwrap().is_empty());
    }
}
