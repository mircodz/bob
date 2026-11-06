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
pub trait Transport: Send + Sync {
    /// JSON-RPC errors become `Err`, not successful results.
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value>;
    /// Notifications expect no reply; delivery errors are ignored.
    async fn notify(&self, method: &str, params: Value);

    fn is_closed(&self) -> bool {
        false
    }
}

pub(crate) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_DISCOVERY_PAGES: usize = 128;
const MAX_DISCOVERED_TOOLS: usize = 4096;

#[derive(Clone, Debug)]
pub struct McpToolMetadata {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

pub fn tool_result_text(result: &Value) -> String {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .map(|part| match part["type"].as_str() {
                    Some("text") => part["text"].as_str().unwrap_or("").to_string(),
                    _ => part.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    match result.get("structuredContent") {
        Some(structured) if !text.is_empty() => {
            format!("{text}\n\nStructured content:\n{structured}")
        }
        Some(structured) => structured.to_string(),
        None => text,
    }
}

#[derive(Clone)]
pub struct McpClient {
    name: String,
    transport: Arc<dyn Transport>,
}

impl McpClient {
    pub async fn connect(cfg: &McpServerConfig) -> anyhow::Result<McpClient> {
        Self::connect_with_workspace(cfg, None).await
    }

    pub async fn connect_in(
        cfg: &McpServerConfig,
        workspace: &std::path::Path,
    ) -> anyhow::Result<McpClient> {
        Self::connect_with_workspace(cfg, Some(workspace)).await
    }

    async fn connect_with_workspace(
        cfg: &McpServerConfig,
        workspace: Option<&std::path::Path>,
    ) -> anyhow::Result<McpClient> {
        let transport: Arc<dyn Transport> = if cfg.is_http() {
            Arc::new(http::HttpTransport::connect(cfg).await?)
        } else {
            Arc::new(stdio::StdioTransport::connect(cfg, workspace).await?)
        };
        Self::from_transport(cfg.name.clone(), transport).await
    }

    pub async fn from_transport(
        name: String,
        transport: Arc<dyn Transport>,
    ) -> anyhow::Result<Self> {
        let client = McpClient { name, transport };

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

    pub fn is_closed(&self) -> bool {
        self.transport.is_closed()
    }

    pub async fn list_tools_raw(&self) -> anyhow::Result<Vec<McpToolMetadata>> {
        let mut out = Vec::new();
        let mut cursor = None;
        let mut cursors = std::collections::HashSet::new();
        let mut names = std::collections::HashSet::new();
        let mut bytes = 0;
        for _ in 0..MAX_DISCOVERY_PAGES {
            let params = cursor
                .as_ref()
                .map(|cursor| json!({"cursor":cursor}))
                .unwrap_or_else(|| json!({}));
            let result = self.transport.request("tools/list", params).await?;
            bytes += serde_json::to_vec(&result)?.len();
            anyhow::ensure!(bytes <= MAX_FRAME_BYTES, "MCP discovery exceeds byte limit");
            let tools = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("MCP discovery response has no tools array"))?;
            for tool in tools {
                let Some(name) = tool["name"].as_str().filter(|name| !name.is_empty()) else {
                    continue;
                };
                anyhow::ensure!(
                    names.insert(name.to_string()),
                    "MCP discovery contains duplicate tool names"
                );
                anyhow::ensure!(
                    out.len() < MAX_DISCOVERED_TOOLS,
                    "MCP discovery exceeds tool limit"
                );
                out.push(McpToolMetadata {
                    name: name.to_string(),
                    description: tool["description"].as_str().unwrap_or("").to_string(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type":"object","properties":{}})),
                });
            }
            match result.get("nextCursor") {
                None | Some(Value::Null) => return Ok(out),
                Some(Value::String(next)) if !next.is_empty() => {
                    anyhow::ensure!(
                        cursors.insert(next.clone()),
                        "MCP discovery cursor repeated"
                    );
                    cursor = Some(next.clone());
                }
                _ => anyhow::bail!("MCP discovery cursor is invalid"),
            }
        }
        anyhow::bail!("MCP discovery exceeds page limit")
    }

    pub async fn call_tool_raw(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        let result = self
            .transport
            .request("tools/call", json!({"name":name,"arguments":args}))
            .await?;
        anyhow::ensure!(result.is_object(), "MCP tool response is not an object");
        Ok(result)
    }

    pub async fn list_tools(&self) -> anyhow::Result<Vec<Arc<dyn Tool>>> {
        Ok(self
            .list_tools_raw()
            .await?
            .into_iter()
            .map(|tool| {
                let spec = ToolSpec {
                    name: format!("{}_{}", self.name, tool.name),
                    description: tool.description,
                    input_schema: tool.input_schema,
                };
                Arc::new(McpTool {
                    client: self.clone(),
                    raw_name: tool.name,
                    spec,
                }) as Arc<dyn Tool>
            })
            .collect())
    }

    async fn call_tool(&self, name: &str, args: Value) -> ToolResult {
        let result = self
            .call_tool_raw(name, args)
            .await
            .map_err(|error| ToolError::failed(error.to_string()))?;
        let text = tool_result_text(&result);
        if result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            if text.is_empty() {
                Err(ToolError::failed(
                    "MCP server reported a tool error without content",
                ))
            } else {
                Err(ToolError::failed("MCP server reported a tool error").with_output(text))
            }
        } else {
            Ok(text.into())
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
        self.client.call_tool(&self.raw_name, input).await
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
    async fn raw_discovery_follows_cursors_and_raw_call_preserves_all_metadata() {
        let response = json!({"content":[],"structuredContent":{"value":1},"isError":true,"_meta":{"trace":"fixture"}});
        let (client, transport) = scripted_client(vec![
            ExpectedRequest {
                method: "tools/list",
                params: json!({}),
                reply: Ok(json!({"tools":[{"name":"first"}],"nextCursor":"page-two"})),
            },
            ExpectedRequest {
                method: "tools/list",
                params: json!({"cursor":"page-two"}),
                reply: Ok(json!({"tools":[{"name":"second","inputSchema":{"type":"object"}}]})),
            },
            ExpectedRequest {
                method: "tools/call",
                params: json!({"name":"second","arguments":{"x":1}}),
                reply: Ok(response.clone()),
            },
        ]);
        let tools = client.list_tools_raw().await.unwrap();
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(
            client
                .call_tool_raw("second", json!({"x":1}))
                .await
                .unwrap(),
            response
        );
        assert!(transport.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn discovery_rejects_duplicate_names_cursors_and_invalid_pages() {
        for second in [
            json!({"tools":[],"nextCursor":"again"}),
            json!({"tools":[{"name":"first"}]}),
            json!({"tools":[],"nextCursor":true}),
            json!({"unexpected":[]}),
        ] {
            let (client, _) = scripted_client(vec![
                ExpectedRequest {
                    method: "tools/list",
                    params: json!({}),
                    reply: Ok(json!({"tools":[{"name":"first"}],"nextCursor":"again"})),
                },
                ExpectedRequest {
                    method: "tools/list",
                    params: json!({"cursor":"again"}),
                    reply: Ok(second),
                },
            ]);
            assert!(client.list_tools_raw().await.is_err());
        }
    }

    #[tokio::test]
    async fn discovery_is_bounded_even_when_every_page_has_a_new_cursor() {
        let requests = (0..MAX_DISCOVERY_PAGES)
            .map(|page| ExpectedRequest {
                method: "tools/list",
                params: if page == 0 {
                    json!({})
                } else {
                    json!({"cursor":page.to_string()})
                },
                reply: Ok(json!({"tools":[],"nextCursor":(page+1).to_string()})),
            })
            .collect();
        let (client, transport) = scripted_client(requests);
        assert!(client
            .list_tools_raw()
            .await
            .unwrap_err()
            .to_string()
            .contains("page limit"));
        assert!(transport.0.lock().unwrap().is_empty());
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
            client
                .call_tool("inspect", json!({}))
                .await
                .unwrap()
                .model_text(),
            format!("first\nsecond\n{image}\n\n{resource}\nlast")
        );
        assert!(transport.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn application_errors_preserve_content_and_failure_status() {
        let resource =
            json!({"type":"resource","resource":{"uri":"file:///fixture.rs","text":"details"}});
        for error_flag in [Some(true), Some(false), None] {
            let mut reply = json!({"content":[{"type":"text","text":"server feedback"}, resource]});
            if let Some(value) = error_flag {
                reply["isError"] = json!(value);
            }
            let (client, transport) = scripted_client(vec![ExpectedRequest {
                method: "tools/call",
                params: json!({"name":"run","arguments":{}}),
                reply: Ok(reply),
            }]);
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
            let result = tool.execute(json!({}), &ctx).await;
            assert_eq!(result.is_err(), error_flag == Some(true));
            let output = result.unwrap_or_else(ToolError::into_output);
            assert_eq!(output.model_text(), format!("server feedback\n{resource}"));
            assert!(transport.0.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn application_error_reaches_events_and_the_next_model_request() {
        use crate::agent::agent::Agent;
        use crate::core::events::AgentEvent;
        use crate::core::types::ContentBlock;
        use crate::test_support::agent::{config, text, tool, ScriptProvider, Step};

        let (client, _) = scripted_client(vec![ExpectedRequest {
            method: "tools/call",
            params: json!({"name":"run","arguments":{}}),
            reply: Ok(
                json!({"isError":true,"content":[{"type":"text","text":"permission missing on server"}]}),
            ),
        }]);
        let provider = Arc::new(ScriptProvider::new(vec![
            Step::Complete(vec![tool("call-1", "fixture_run")]),
            Step::Complete(vec![text("The server rejected the operation.")]),
        ]));
        let mut cfg = config(provider.clone());
        cfg.tools.add(Arc::new(McpTool {
            client,
            raw_name: "run".into(),
            spec: ToolSpec {
                name: "fixture_run".into(),
                description: String::new(),
                input_schema: json!({"type":"object"}),
            },
        }));
        let results = Arc::new(Mutex::new(Vec::new()));
        let captured = results.clone();
        cfg.bus.on(Arc::new(move |event| {
            if let AgentEvent::ToolResult {
                output, is_error, ..
            } = event
            {
                captured
                    .lock()
                    .unwrap()
                    .push((output.model_text().into_owned(), *is_error));
            }
        }));
        Agent::new(cfg).run("Run the operation").await.unwrap();
        assert_eq!(
            *results.lock().unwrap(),
            vec![("permission missing on server".into(), true)]
        );
        let requests = provider.requests.lock().unwrap();
        assert!(requests[1].iter().flat_map(|message| &message.content).any(|block| {
            matches!(block, ContentBlock::ToolResult { tool_use_id, content, is_error: Some(true) }
                if tool_use_id == "call-1" && content.model_text() == "permission missing on server")
        }));
        provider.assert_exhausted();
    }

    #[tokio::test]
    async fn empty_application_error_is_not_an_empty_success() {
        let (client, _) = scripted_client(vec![ExpectedRequest {
            method: "tools/call",
            params: json!({"name":"run","arguments":{}}),
            reply: Ok(json!({"isError":true,"content":[]})),
        }]);
        let error = client.call_tool("run", json!({})).await.unwrap_err();
        assert!(error.into_output().model_text().contains("without content"));
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
