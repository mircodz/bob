use anyhow::{ensure, Result};
use bob_core::core::config::McpServerConfig;
use bob_core::mcp::{tool_result_text, McpClient, McpToolMetadata};
use bob_core::tools::schema::CompiledSchema;
use bob_kernel::{EffectClass, ToolDescriptor, ToolOutcome};
use bob_types::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpState {
    Ready,
    Failed,
    Disconnected,
}

#[derive(Clone, Debug, Serialize)]
pub struct McpStatus {
    pub name: String,
    pub transport: McpTransportKind,
    pub state: McpState,
    pub tools: usize,
    pub message: Option<String>,
}

struct Server {
    status: Mutex<McpStatus>,
    client: Option<McpClient>,
    identity: String,
    schema_failures: Vec<String>,
}

struct BoundTool {
    server: usize,
    raw_name: String,
    descriptor: ToolDescriptor,
    identity: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedCall {
    name: String,
    server: String,
    tool: String,
    input: Value,
    binding: String,
}

#[derive(Default)]
pub struct McpService {
    servers: Vec<Server>,
    tools: BTreeMap<String, BoundTool>,
}

impl McpService {
    pub async fn start(configs: &[McpServerConfig], workspace: &Path) -> Self {
        Self::start_with(configs, |config| async move {
            McpClient::connect_in(&config, workspace).await
        })
        .await
    }

    async fn start_with<F, Fut>(configs: &[McpServerConfig], connect: F) -> Self
    where
        F: Fn(McpServerConfig) -> Fut,
        Fut: Future<Output = Result<McpClient>>,
    {
        let mut names = HashMap::new();
        for config in configs {
            *names.entry(config.name.as_str()).or_insert(0usize) += 1;
        }
        let started = futures::future::join_all(configs.iter().map(|config| {
            let valid = !config.name.trim().is_empty() && names[config.name.as_str()] == 1;
            let connect = &connect;
            async move {
                let mut status = McpStatus {
                    name: config.name.clone(),
                    transport: if config.is_http() {
                        McpTransportKind::Http
                    } else {
                        McpTransportKind::Stdio
                    },
                    state: McpState::Failed,
                    tools: 0,
                    message: None,
                };
                let mut client = None;
                let mut tools = Vec::new();
                if valid {
                    let startup = async {
                        let connected = connect(config.clone()).await.map_err(|_| {
                            "MCP connection failed; check configuration and authorization"
                        })?;
                        let tools = connected
                            .list_tools_raw()
                            .await
                            .map_err(|_| "MCP tool discovery failed")?;
                        Ok::<_, &str>((connected, tools))
                    };
                    match tokio::time::timeout(STARTUP_TIMEOUT, startup).await {
                        Ok(Ok((connected, discovered))) => {
                            status.state = McpState::Ready;
                            status.tools = discovered.len();
                            client = Some(connected);
                            tools = discovered;
                        }
                        Ok(Err(message)) => status.message = Some(message.into()),
                        Err(_) => status.message = Some("MCP startup timed out".into()),
                    }
                } else {
                    status.message = Some("MCP server name is empty or duplicated".into());
                }
                let identity =
                    digest(&serde_json::to_value(config).expect("serializable MCP config"));
                (
                    Server {
                        status: Mutex::new(status),
                        client,
                        identity,
                        schema_failures: Vec::new(),
                    },
                    tools,
                )
            }
        }))
        .await;
        Self::from_started(started)
    }

    fn from_started(started: Vec<(Server, Vec<McpToolMetadata>)>) -> Self {
        let mut counts = HashMap::new();
        for (server, tools) in &started {
            let status = server.status.lock().unwrap();
            for tool in tools {
                *counts
                    .entry(format!("{}_{}", status.name, tool.name))
                    .or_insert(0usize) += 1;
            }
        }
        let mut service = Self::default();
        for (mut server, tools) in started {
            let status = server.status.get_mut().unwrap();
            if tools.iter().any(|tool| {
                let name = format!("{}_{}", status.name, tool.name);
                counts[&name] > 1 || reserved_tool_name(&name)
            }) {
                status.state = McpState::Failed;
                status.tools = 0;
                status.message = Some("MCP tool namespace collision".into());
                server.client = None;
            } else {
                status.tools = 0;
                for tool in tools {
                    let name = format!("{}_{}", status.name, tool.name);
                    if CompiledSchema::compile(&tool.input_schema).is_err() {
                        server.schema_failures.push(name);
                        continue;
                    }
                    status.tools += 1;
                    let identity = digest(&serde_json::json!({
                        "config": server.identity,
                        "name": name,
                        "raw_name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.input_schema,
                    }));
                    service.tools.insert(
                        name.clone(),
                        BoundTool {
                            server: service.servers.len(),
                            raw_name: tool.name,
                            identity,
                            descriptor: ToolDescriptor {
                                spec: ToolSpec {
                                    name,
                                    description: tool.description,
                                    input_schema: tool.input_schema,
                                },
                                effect: EffectClass::Write,
                            },
                        },
                    );
                }
                if !server.schema_failures.is_empty() {
                    status.state = McpState::Failed;
                    status.message = Some(format!(
                        "Invalid MCP tool schemas quarantined: {}",
                        server.schema_failures.join(", ")
                    ));
                }
            }
            service.servers.push(server);
        }
        service
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        self.tools
            .values()
            .map(|tool| tool.descriptor.clone())
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn prepare(&self, name: &str, input: &Value) -> Result<Value> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Unknown MCP tool"))?;
        ensure!(input.is_object(), "MCP tool input must be an object");
        let server = &self.servers[tool.server];
        let prepared = PreparedCall {
            name: name.into(),
            server: server.status.lock().unwrap().name.clone(),
            tool: tool.raw_name.clone(),
            input: input.clone(),
            binding: digest(&serde_json::json!({"tool":tool.identity,"input":input})),
        };
        Ok(serde_json::to_value(prepared)?)
    }

    pub async fn execute(&self, name: &str, spec: &Value) -> Result<ToolOutcome> {
        let prepared: PreparedCall = serde_json::from_value(spec.clone())
            .map_err(|_| anyhow::anyhow!("Invalid prepared MCP call"))?;
        ensure!(
            self.prepare(name, &prepared.input)? == *spec,
            "Prepared MCP binding changed"
        );
        let tool = &self.tools[name];
        let server = &self.servers[tool.server];
        let client = server
            .client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("MCP server unavailable"))?;
        let result = match client.call_tool_raw(&tool.raw_name, prepared.input).await {
            Ok(result) => {
                let mut status = server.status.lock().unwrap();
                status.state = if server.schema_failures.is_empty() {
                    McpState::Ready
                } else {
                    McpState::Failed
                };
                status.message = (!server.schema_failures.is_empty()).then(|| {
                    format!(
                        "Invalid MCP tool schemas quarantined: {}",
                        server.schema_failures.join(", ")
                    )
                });
                result
            }
            Err(_) => {
                let mut status = server.status.lock().unwrap();
                status.state = if client.is_closed() {
                    McpState::Disconnected
                } else {
                    McpState::Failed
                };
                status.message =
                    Some("MCP request failed; external effects may be uncertain".into());
                anyhow::bail!("MCP request failed; external effects may be uncertain");
            }
        };
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut text = tool_result_text(&result);
        if is_error && text.is_empty() {
            text = "MCP server reported a tool error without content".into();
        }
        Ok(ToolOutcome {
            text,
            is_error,
            interrupted: false,
            presentation: None,
        })
    }

    pub fn statuses(&self) -> Vec<McpStatus> {
        self.servers
            .iter()
            .map(|server| {
                let mut status = server.status.lock().unwrap().clone();
                if server.client.as_ref().is_some_and(McpClient::is_closed) {
                    status.state = McpState::Disconnected;
                    status.message = Some("MCP connection closed".into());
                }
                status
            })
            .collect()
    }
}

fn reserved_tool_name(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "write_file"
            | "edit_file"
            | "multi_edit"
            | "create_file"
            | "list_dir"
            | "glob"
            | "grep"
            | "project_instructions"
            | "bash"
            | "web_fetch"
            | "web_search"
            | "memory"
            | "lsp"
            | "rename_symbol"
            | "code_action"
            | "todo_write"
            | "job_status"
            | "job_output"
            | "read_history"
            | "search_conversation"
            | "read_conversation"
            | "ask_user"
            | "enter_plan"
            | "exit_plan"
            | "task"
            | "explore"
            | "workflow"
            | "spawn_agent"
            | "send_message"
            | "list_agents"
            | "stop_agent"
            | "launch_agent"
            | "wait_agent"
            | "wait_message"
    )
}

fn digest(value: &Value) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let sorted: BTreeMap<_, _> = object.iter().collect();
                Value::Object(
                    sorted
                        .into_iter()
                        .map(|(key, value)| (key.clone(), canonical(value)))
                        .collect(),
                )
            }
            Value::Array(array) => Value::Array(array.iter().map(canonical).collect()),
            value => value.clone(),
        }
    }
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical(value)).expect("serializable MCP binding"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bob_core::mcp::Transport;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct MockTransport {
        listing: Value,
        replies: Mutex<VecDeque<Result<Value>>>,
        requests: Mutex<Vec<(String, Value)>>,
        closed: AtomicBool,
        stall: AtomicBool,
    }

    impl MockTransport {
        fn new(tools: Value) -> Arc<Self> {
            Arc::new(Self {
                listing: json!({"tools": tools}),
                replies: Mutex::new(VecDeque::new()),
                requests: Mutex::new(Vec::new()),
                closed: AtomicBool::new(false),
                stall: AtomicBool::new(false),
            })
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn request(&self, method: &str, params: Value) -> Result<Value> {
            self.requests.lock().unwrap().push((method.into(), params));
            match method {
                "initialize" => Ok(
                    json!({"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fixture","version":"1"}}),
                ),
                "tools/list" => Ok(self.listing.clone()),
                "tools/call" => {
                    if self.stall.load(Ordering::SeqCst) {
                        return futures::future::pending().await;
                    }
                    self.replies
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected tool call")
                }
                _ => panic!("unexpected method"),
            }
        }

        async fn notify(&self, method: &str, params: Value) {
            assert_eq!(method, "notifications/initialized");
            assert_eq!(params, json!({}));
        }

        fn is_closed(&self) -> bool {
            self.closed.load(Ordering::SeqCst)
        }
    }

    fn config(name: &str) -> McpServerConfig {
        serde_json::from_value(json!({"name":name,"command":"never-spawned"})).unwrap()
    }

    async fn service(config: McpServerConfig, transport: Arc<MockTransport>) -> McpService {
        McpService::start_with(&[config], move |config| {
            let transport = transport.clone();
            async move { McpClient::from_transport(config.name, transport).await }
        })
        .await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdio_start_uses_selected_workspace_not_launcher_directory() {
        let selected = std::env::temp_dir().join(format!("bob-mcp-cwd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&selected).unwrap();
        let selected = std::fs::canonicalize(selected).unwrap();
        let launcher = std::env::current_dir().unwrap();
        assert_ne!(launcher, selected);
        let script = r#"
while IFS= read -r request; do
    case "$request" in
        *'"method":"initialize"'*) printf '%s\n' '{"id":1,"result":{}}' ;;
        *'"method":"tools/list"'*) printf '%s\n' '{"id":2,"result":{"tools":[{"name":"cwd"}]}}' ;;
        *'"method":"tools/call"'*) printf '{"id":3,"result":{"content":[{"type":"text","text":"%s"}]}}\n' "$(pwd -P)" ;;
    esac
done
"#;
        let config: McpServerConfig = serde_json::from_value(json!({
            "name":"fixture", "command":"/bin/sh", "args":["-c",script]
        }))
        .unwrap();
        let services = super::super::Services::create(
            &bob_core::core::config::BobConfig {
                mcp_servers: vec![config],
                ..Default::default()
            },
            &selected,
            Arc::new(bob_core::core::memory::InMemoryMemories::default()),
        )
        .await
        .unwrap();
        let service = &services.mcp;
        assert_eq!(service.statuses()[0].state, McpState::Ready);
        let prepared = service.prepare("fixture_cwd", &json!({})).unwrap();
        let result = service.execute("fixture_cwd", &prepared).await.unwrap();
        assert_eq!(Path::new(&result.text), selected);
        assert_eq!(std::env::current_dir().unwrap(), launcher);
        drop(services);
        std::fs::remove_dir(selected).unwrap();
    }

    #[tokio::test]
    async fn empty_configuration_has_no_implicit_servers() {
        let service = McpService::start(&[], Path::new(".")).await;
        assert!(service.descriptors().is_empty());
        assert!(service.statuses().is_empty());
        assert!(!service.contains("anything"));
        assert!(service.prepare("anything", &json!({})).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn startup_is_bounded_parallel_partial_failure_and_sanitized() {
        let transport = MockTransport::new(json!([{"name":"ping"}]));
        let started = tokio::time::Instant::now();
        let service = McpService::start_with(
            &[config("slow"), config("ready"), config("bad")],
            |config| {
                let transport = transport.clone();
                async move {
                    match config.name.as_str() {
                        "slow" => futures::future::pending().await,
                        "bad" => anyhow::bail!("Bearer secret-in-error"),
                        _ => McpClient::from_transport(config.name, transport).await,
                    }
                }
            },
        )
        .await;
        assert_eq!(started.elapsed(), STARTUP_TIMEOUT);
        assert!(service.contains("ready_ping"));
        let statuses = service.statuses();
        assert_eq!(
            statuses
                .iter()
                .map(|status| status.state)
                .collect::<Vec<_>>(),
            vec![McpState::Failed, McpState::Ready, McpState::Failed]
        );
        assert!(statuses[0].message.as_ref().unwrap().contains("timed out"));
        assert!(!serde_json::to_string(&statuses)
            .unwrap()
            .contains("secret-in-error"));
    }

    #[tokio::test]
    async fn exact_names_schema_write_effect_and_side_effect_free_preparation() {
        let schema =
            json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]});
        let transport = MockTransport::new(
            json!([{"name":"search_tool","description":"Find things","inputSchema":schema,"annotations":{"readOnlyHint":true}}]),
        );
        let service = service(config("a_server"), transport.clone()).await;
        let descriptors = service.descriptors();
        assert_eq!(descriptors[0].spec.name, "a_server_search_tool");
        assert_eq!(descriptors[0].spec.input_schema, schema);
        assert_eq!(descriptors[0].spec.description, "Find things");
        assert_eq!(descriptors[0].effect, EffectClass::Write);
        let input = json!({"query":"é😀","nested":[null,true,1]});
        let prepared = service.prepare("a_server_search_tool", &input).unwrap();
        assert_eq!(prepared["input"], input);
        assert_eq!(prepared["tool"], "search_tool");
        assert_eq!(transport.requests.lock().unwrap().len(), 2);
        assert!(service
            .prepare("a_server_search_tool", &Value::Null)
            .is_err());
        transport
            .replies
            .lock()
            .unwrap()
            .push_back(Ok(json!({"content":[{"type":"text","text":"found"}]})));
        let outcome = service
            .execute("a_server_search_tool", &prepared)
            .await
            .unwrap();
        assert_eq!(outcome.text, "found");
        assert!(!outcome.is_error);
        assert_eq!(
            transport.requests.lock().unwrap().last().unwrap(),
            &(
                "tools/call".into(),
                json!({"name":"search_tool","arguments":input})
            )
        );
    }

    #[tokio::test]
    async fn altered_bindings_and_changed_configuration_or_schema_never_call_server() {
        let transport = MockTransport::new(json!([{"name":"run","inputSchema":{"type":"object"}}]));
        let original = service(config("fixture"), transport.clone()).await;
        let prepared = original.prepare("fixture_run", &json!({"x":1})).unwrap();
        for field in ["name", "server", "tool", "input", "binding", "unexpected"] {
            let mut altered = prepared.clone();
            altered[field] = if field == "input" {
                json!({"x":2})
            } else {
                json!("changed")
            };
            assert!(original.execute("fixture_run", &altered).await.is_err());
        }
        let mut changed_config = config("fixture");
        changed_config
            .env
            .insert("TOKEN".into(), "secret-value".into());
        let changed = service(changed_config, transport.clone()).await;
        assert!(changed.execute("fixture_run", &prepared).await.is_err());
        let new_prepared = changed.prepare("fixture_run", &json!({"x":1})).unwrap();
        assert!(!new_prepared.to_string().contains("secret-value"));
        let other_transport = MockTransport::new(
            json!([{"name":"run","inputSchema":{"type":"object","required":["x"]}}]),
        );
        let changed = service(config("fixture"), other_transport.clone()).await;
        assert!(changed.execute("fixture_run", &prepared).await.is_err());
        assert!(transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _)| method != "tools/call"));
        assert!(other_transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(method, _)| method != "tools/call"));
    }

    #[tokio::test]
    async fn errors_and_disconnect_health_preserve_server_output_without_logging_secrets() {
        let transport = MockTransport::new(json!([{"name":"run"}]));
        let service = service(config("fixture"), transport.clone()).await;
        let prepared = service.prepare("fixture_run", &json!({})).unwrap();
        transport.replies.lock().unwrap().extend([
            Ok(json!({"isError":true,"content":[{"type":"text","text":"permission denied"},{"type":"resource","resource":{"uri":"fixture:resource"}}]})),
            Ok(json!({"isError":true,"content":[]})),
            Err(anyhow::anyhow!("Bearer secret-in-error")),
            Ok(json!({"structuredContent":{"answer":42}})),
        ]);
        let outcome = service.execute("fixture_run", &prepared).await.unwrap();
        assert!(outcome.is_error);
        assert!(outcome.text.starts_with("permission denied\n"));
        assert!(outcome.text.contains("fixture:resource"));
        assert_eq!(service.statuses()[0].state, McpState::Ready);
        assert!(service
            .execute("fixture_run", &prepared)
            .await
            .unwrap()
            .text
            .contains("without content"));
        let error = service.execute("fixture_run", &prepared).await.unwrap_err();
        assert!(!error.to_string().contains("secret-in-error"));
        assert_eq!(service.statuses()[0].state, McpState::Failed);
        assert!(!serde_json::to_string(&service.statuses())
            .unwrap()
            .contains("secret-in-error"));
        assert_eq!(
            service
                .execute("fixture_run", &prepared)
                .await
                .unwrap()
                .text,
            r#"{"answer":42}"#
        );
        assert_eq!(service.statuses()[0].state, McpState::Ready);
        transport.closed.store(true, Ordering::SeqCst);
        assert_eq!(service.statuses()[0].state, McpState::Disconnected);
    }

    #[tokio::test]
    async fn textual_and_structured_results_are_both_retained() {
        let transport = MockTransport::new(json!([{"name":"run"}]));
        let service = service(config("fixture"), transport.clone()).await;
        let prepared = service.prepare("fixture_run", &json!({})).unwrap();
        transport.replies.lock().unwrap().push_back(Ok(json!({
            "content":[{"type":"text","text":"human summary"}],
            "structuredContent":{"unique_id":"structured-only-identifier"}
        })));
        let result = service.execute("fixture_run", &prepared).await.unwrap();
        assert!(result.text.contains("human summary"));
        assert!(result.text.contains("structured-only-identifier"));
    }

    #[tokio::test]
    async fn builtin_and_kernel_control_collisions_fail_the_server() {
        for name in [
            "read_file",
            "write_file",
            "edit_file",
            "multi_edit",
            "create_file",
            "list_dir",
            "project_instructions",
            "web_fetch",
            "web_search",
            "rename_symbol",
            "code_action",
            "todo_write",
            "job_status",
            "job_output",
            "ask_user",
            "enter_plan",
            "exit_plan",
            "spawn_agent",
            "send_message",
            "list_agents",
            "stop_agent",
            "launch_agent",
            "wait_agent",
            "wait_message",
            "read_history",
            "search_conversation",
            "read_conversation",
        ] {
            let (server, raw) = name.split_once('_').unwrap();
            let transport =
                MockTransport::new(json!([{"name":raw,"annotations":{"readOnlyHint":true}}]));
            let service = service(config(server), transport.clone()).await;
            assert!(!service.contains(name), "{name}");
            assert!(service.descriptors().is_empty());
            assert_eq!(service.statuses()[0].state, McpState::Failed);
            assert!(service.statuses()[0]
                .message
                .as_ref()
                .unwrap()
                .contains("collision"));
            assert!(service.prepare(name, &json!({})).is_err());
            assert!(transport
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(method, _)| method != "tools/call"));
        }
        for descriptor in crate::tools::ToolService::default().tools() {
            assert!(
                reserved_tool_name(&descriptor.spec.name),
                "{}",
                descriptor.spec.name
            );
        }
    }

    #[tokio::test]
    async fn invalid_schemas_are_quarantined_without_losing_healthy_tools_or_servers() {
        let mut nested = json!({"type":"string"});
        for _ in 0..66 {
            nested = json!({"properties":{"child":nested}});
        }
        let transport = MockTransport::new(json!([
            {"name":"valid","inputSchema":{"type":"object","properties":{"value":{"$ref":"#/$defs/value"}},"$defs":{"value":{"type":"string"}}}},
            {"name":"invalid_type","inputSchema":{"type":"not-a-type"}},
            {"name":"external_ref","inputSchema":{"$ref":"https://example.invalid/must-not-fetch"}},
            {"name":"missing_ref","inputSchema":{"$ref":"#/$defs/missing"}},
            {"name":"nested","inputSchema":nested}
        ]));
        let healthy = MockTransport::new(json!([{"name":"run"}]));
        let service = McpService::start_with(&[config("mixed"), config("healthy")], |config| {
            let transport = if config.name == "mixed" {
                transport.clone()
            } else {
                healthy.clone()
            };
            async move { McpClient::from_transport(config.name, transport).await }
        })
        .await;
        let descriptors = service.descriptors();
        assert_eq!(
            descriptors
                .iter()
                .map(|d| d.spec.name.as_str())
                .collect::<Vec<_>>(),
            vec!["healthy_run", "mixed_valid"]
        );
        for descriptor in descriptors {
            CompiledSchema::compile(&descriptor.spec.input_schema).unwrap();
        }
        let status = &service.statuses()[0];
        assert_eq!(status.tools, 1);
        assert_eq!(status.state, McpState::Failed);
        for name in ["invalid_type", "external_ref", "missing_ref", "nested"] {
            let name = format!("mixed_{name}");
            assert!(status.message.as_ref().unwrap().contains(&name));
            assert!(service.prepare(&name, &json!({})).is_err());
        }
        assert_eq!(service.statuses()[1].state, McpState::Ready);
        transport
            .replies
            .lock()
            .unwrap()
            .push_back(Ok(json!({"content":[{"type":"text","text":"ok"}]})));
        let prepared = service
            .prepare("mixed_valid", &json!({"value":"yes"}))
            .unwrap();
        assert_eq!(
            service
                .execute("mixed_valid", &prepared)
                .await
                .unwrap()
                .text,
            "ok"
        );
        assert_eq!(service.statuses()[0].state, McpState::Failed);
        assert!(service.statuses()[0]
            .message
            .as_ref()
            .unwrap()
            .contains("mixed_invalid_type"));
    }

    #[tokio::test]
    async fn cancellation_drops_call_without_background_retry() {
        let transport = MockTransport::new(json!([{"name":"run"}]));
        transport.stall.store(true, Ordering::SeqCst);
        let service = service(config("fixture"), transport.clone()).await;
        let prepared = service.prepare("fixture_run", &json!({})).unwrap();
        let mut call = Box::pin(service.execute("fixture_run", &prepared));
        assert!(futures::poll!(&mut call).is_pending());
        drop(call);
        assert_eq!(transport.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn duplicate_server_names_and_ambiguous_tool_names_are_not_registered() {
        let transport = MockTransport::new(json!([{"name":"run"}]));
        let service = McpService::start_with(&[config("same"), config("same")], |config| {
            let transport = transport.clone();
            async move { McpClient::from_transport(config.name, transport).await }
        })
        .await;
        assert!(service.descriptors().is_empty());
        assert!(transport.requests.lock().unwrap().is_empty());
        let service = McpService::start_with(&[config("a_b"), config("a")], |config| async move {
            let raw = if config.name == "a" { "b_c" } else { "c" };
            McpClient::from_transport(config.name, MockTransport::new(json!([{"name":raw}]))).await
        })
        .await;
        assert!(service.descriptors().is_empty());
        assert!(service
            .statuses()
            .iter()
            .all(|status| status.state == McpState::Failed));
        let service = McpService::start_with(&[config("web")], |config| async move {
            McpClient::from_transport(config.name, MockTransport::new(json!([{"name":"fetch"}])))
                .await
        })
        .await;
        assert!(!service.contains("web_fetch"));
        assert_eq!(service.statuses()[0].state, McpState::Failed);
    }
}
