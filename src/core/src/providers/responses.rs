//! Responses uses `input` items, `instructions`, and a distinct SSE protocol.

use crate::core::types::*;
use crate::providers::openai::TokenSource;
use crate::providers::provider::{ModelEntry, Provider, StreamTimeouts};
use crate::providers::sse::{parse_sse_control, SseEvent};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct ResponsesProvider {
    auth: Arc<dyn TokenSource>,
    model: String,
    base_url: String,
    client: reqwest::Client,
    context_window: Option<usize>,
    model_catalog: Vec<ModelEntry>,
    timeouts: StreamTimeouts,
}

impl ResponsesProvider {
    pub fn with_auth(model: String, base_url: String, auth: Arc<dyn TokenSource>) -> Self {
        ResponsesProvider {
            auth,
            model,
            base_url: base_url.trim_end_matches('/').to_string(),
            client: super::openai::http_client(),
            context_window: None,
            model_catalog: Vec::new(),
            timeouts: StreamTimeouts::default(),
        }
    }

    /// Set an authoritative context window, overriding the id-based heuristic.
    pub fn with_context_window(mut self, window: Option<usize>) -> Self {
        self.context_window = window;
        self
    }

    pub fn with_model_catalog(mut self, models: Vec<ModelEntry>) -> Self {
        self.model_catalog = models;
        self
    }

    pub fn with_stream_timeouts(mut self, timeouts: StreamTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    fn build_body(&self, opts: &GenerateOptions) -> Value {
        let mut input: Vec<Value> = Vec::new();
        for m in &opts.messages {
            input.extend(to_input_items(m));
        }

        let mut body = json!({
            "model": self.model,
            "input": input,
            "stream": true,
            "store": false,
        });
        if let Some(sys) = &opts.system {
            body["instructions"] = json!(sys);
        }
        // Let the server enforce its output limit unless the caller pins one.
        if let Some(max) = opts.max_tokens {
            body["max_output_tokens"] = json!(max);
        }
        if let Some(effort) = opts.reasoning.as_str() {
            body["reasoning"] = json!({ "effort": effort });
            // store:false requires encrypted reasoning to be echoed across tool calls.
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
        if !opts.tools.is_empty() {
            body["tools"] = json!(opts
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                    "strict": false,
                }))
                .collect::<Vec<_>>());
        }
        body
    }

    async fn request(&self, body: Value) -> anyhow::Result<reqwest::RequestBuilder> {
        let token = self.auth.token().await?;
        let mut req = self
            .client
            .post(format!("{}/responses", self.base_url))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", token))
            // Required by the codex backend.
            .header("openai-beta", "responses=v1")
            .json(&body);
        for (k, v) in self.auth.extra_headers() {
            req = req.header(k, v);
        }
        Ok(req)
    }
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn name(&self) -> &str {
        "openai"
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn context_window(&self) -> usize {
        self.context_window
            .unwrap_or_else(|| crate::providers::provider::context_window_for(&self.model))
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(self
            .model_catalog
            .iter()
            .map(|entry| entry.id.clone())
            .collect())
    }

    async fn list_models_detailed(&self) -> anyhow::Result<Vec<ModelEntry>> {
        Ok(self.model_catalog.clone())
    }

    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        crate::providers::provider::generate_with_retry(self, opts, &|_| {}).await
    }

    async fn generate_once(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        let mut rx = self.stream(opts).await?;
        let mut completion = None;
        while let Some(evt) = rx.recv().await {
            match evt {
                StreamEvent::MessageStop { completion: c } => completion = Some(c),
                StreamEvent::Error { message } => anyhow::bail!("{}", message),
                _ => {}
            }
        }
        completion.ok_or_else(|| anyhow::anyhow!("responses stream ended without completion"))
    }

    async fn stream(
        &self,
        opts: GenerateOptions,
    ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
        let body = self.build_body(&opts);
        let provider = self.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let Some(res) =
                crate::providers::provider::start_stream(&tx, provider.timeouts.startup, async {
                    crate::providers::provider::send_once(
                        provider.request(body).await?,
                        "responses",
                    )
                    .await
                })
                .await
            else {
                return;
            };
            let mut text = String::new();
            // Tool calls accumulate by output index → (call_id, name, args).
            let mut tools: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
            // Echo opaque reasoning verbatim for store:false continuity.
            let mut reasoning: BTreeMap<i64, Value> = BTreeMap::new();
            // Preserve text placement relative to reasoning and tool calls.
            let mut text_index: Option<i64> = None;
            let mut usage = Usage::default();
            let mut terminal_seen = false;

            let consume = parse_sse_control(
                res,
                |event| {
                    let SseEvent::Json(evt) = event else {
                        return ControlFlow::Break(());
                    };
                    let kind = evt["type"].as_str().unwrap_or("");
                    match kind {
                        "response.output_text.delta" => {
                            if let Some(d) = evt["delta"].as_str() {
                                if text_index.is_none() {
                                    text_index = Some(evt["output_index"].as_i64().unwrap_or(0));
                                }
                                text.push_str(d);
                                let _ = tx.send(StreamEvent::TextDelta {
                                    text: d.to_string(),
                                });
                            }
                        }
                        "response.output_item.done" => {
                            let item = &evt["item"];
                            if item["type"] == "reasoning" {
                                let idx = evt["output_index"].as_i64().unwrap_or(0);
                                reasoning.insert(idx, item.clone());
                            } else if item["type"] == "function_call" {
                                let idx = evt["output_index"].as_i64().unwrap_or(0);
                                tools.insert(
                                    idx,
                                    (
                                        item["call_id"].as_str().unwrap_or("").to_string(),
                                        item["name"].as_str().unwrap_or("").to_string(),
                                        item["arguments"].as_str().unwrap_or("").to_string(),
                                    ),
                                );
                            }
                        }
                        "response.output_item.added" => {
                            let item = &evt["item"];
                            if item["type"] == "function_call" {
                                let idx = evt["output_index"].as_i64().unwrap_or(0);
                                let call_id = item["call_id"].as_str().unwrap_or("").to_string();
                                let name = item["name"].as_str().unwrap_or("").to_string();
                                if !name.is_empty() {
                                    let _ = tx.send(StreamEvent::ToolUseStart {
                                        id: call_id.clone(),
                                        name: name.clone(),
                                    });
                                }
                                tools.insert(idx, (call_id, name, String::new()));
                            }
                        }
                        "response.function_call_arguments.delta" => {
                            let idx = evt["output_index"].as_i64().unwrap_or(0);
                            if let Some(d) = evt["delta"].as_str() {
                                if let Some(entry) = tools.get_mut(&idx) {
                                    entry.2.push_str(d);
                                    let _ = tx.send(StreamEvent::ToolUseInputDelta {
                                        id: entry.0.clone(),
                                        partial_json: d.to_string(),
                                    });
                                }
                            }
                        }
                        "response.function_call_arguments.done" => {
                            let idx = evt["output_index"].as_i64().unwrap_or(0);
                            if let Some(entry) = tools.get_mut(&idx) {
                                if let Some(args) = evt["arguments"].as_str() {
                                    entry.2 = args.to_string();
                                }
                            }
                        }
                        "response.completed" | "response.incomplete" => {
                            terminal_seen = true;
                            let u = &evt["response"]["usage"];
                            usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                            usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                            usage.cache_read_input_tokens = u["input_tokens_details"]
                                ["cached_tokens"]
                                .as_u64()
                                .unwrap_or(0);
                            usage.input_tokens = usage
                                .input_tokens
                                .saturating_sub(usage.cache_read_input_tokens);
                            let truncated = kind == "response.incomplete"
                                && evt["response"]["incomplete_details"]["reason"]
                                    == "max_output_tokens";
                            if kind == "response.incomplete" && !truncated {
                                let _ = tx.send(StreamEvent::Error {
                                    message: format!(
                                        "responses incomplete: {}",
                                        evt["response"]["incomplete_details"]
                                    ),
                                });
                                return ControlFlow::Break(());
                            }
                            if let Some(output) = evt["response"]["output"].as_array() {
                                text.clear();
                                text_index = None;
                                tools.clear();
                                reasoning.clear();
                                for (idx, item) in output.iter().enumerate() {
                                    let idx = idx as i64;
                                    match item["type"].as_str() {
                                        Some("reasoning") => {
                                            reasoning.insert(idx, item.clone());
                                        }
                                        Some("function_call") if !truncated => {
                                            if matches!(
                                                item["status"].as_str(),
                                                Some("in_progress" | "incomplete")
                                            ) {
                                                let _ = tx.send(StreamEvent::Error { message: "responses completed with unfinished tool call".into() });
                                                return ControlFlow::Break(());
                                            }
                                            tools.insert(
                                                idx,
                                                (
                                                    item["call_id"]
                                                        .as_str()
                                                        .unwrap_or("")
                                                        .to_string(),
                                                    item["name"].as_str().unwrap_or("").to_string(),
                                                    item["arguments"]
                                                        .as_str()
                                                        .unwrap_or("")
                                                        .to_string(),
                                                ),
                                            );
                                        }
                                        Some("message") => {
                                            if let Some(parts) = item["content"].as_array() {
                                                for part in parts {
                                                    if let Some(value) = part["text"].as_str() {
                                                        text_index.get_or_insert(idx);
                                                        text.push_str(value);
                                                    }
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            if truncated {
                                tools.clear();
                            }

                            let mut content: Vec<ContentBlock> = Vec::new();
                            // Preserve output_index order: store:false requires reasoning/tool pairing.
                            let mut ordered: Vec<(i64, ContentBlock)> = Vec::new();
                            for (idx, item) in &reasoning {
                                ordered.push((
                                    *idx,
                                    ContentBlock::ReasoningItem { item: item.clone() },
                                ));
                            }
                            if !text.is_empty() {
                                ordered.push((
                                    text_index.unwrap_or(i64::MAX),
                                    ContentBlock::Text { text: text.clone() },
                                ));
                            }
                            for (idx, (id, name, args)) in &tools {
                                let input = match crate::providers::codec::complete_tool_input(
                                    id, name, args,
                                ) {
                                    Ok(input) => input,
                                    Err(error) => {
                                        let _ = tx.send(StreamEvent::Error {
                                            message: format!("responses: {error}"),
                                        });
                                        return ControlFlow::Break(());
                                    }
                                };
                                ordered.push((
                                    *idx,
                                    ContentBlock::ToolUse {
                                        id: id.clone(),
                                        name: name.clone(),
                                        input,
                                    },
                                ));
                            }
                            ordered.sort_by_key(|(idx, _)| *idx);
                            content.extend(ordered.into_iter().map(|(_, b)| b));
                            let stop = if truncated {
                                StopReason::MaxTokens
                            } else if tools.is_empty() {
                                StopReason::EndTurn
                            } else {
                                StopReason::ToolUse
                            };
                            let completion = Completion {
                                message: Message {
                                    role: Role::Assistant,
                                    content,
                                },
                                stop_reason: stop,
                                usage,
                            };
                            let _ = tx.send(StreamEvent::MessageStop { completion });
                        }
                        "response.failed" | "error" => {
                            terminal_seen = true;
                            // Failures must not become assistant turns in history.
                            let error = evt["response"]
                                .get("error")
                                .or_else(|| evt.get("error"))
                                .unwrap_or(&evt);
                            let msg = error["message"]
                                .as_str()
                                .unwrap_or("responses stream error");
                            let code = error["code"]
                                .as_str()
                                .or_else(|| error["type"].as_str())
                                .unwrap_or("error");
                            let _ = tx.send(StreamEvent::Error {
                                message: format!("responses: {code}: {msg}"),
                            });
                        }
                        _ => {}
                    }
                    if terminal_seen {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                },
                provider.timeouts.idle,
            );
            let sse = tokio::select! {
                biased;
                _ = tx.closed() => return,
                result = consume => result,
            };
            if !terminal_seen {
                let message = match sse {
                    Err(error) => format!("responses stream ended: {error}"),
                    Ok(_) => "responses stream ended without a terminal event".into(),
                };
                let _ = tx.send(StreamEvent::Error { message });
            }
        });

        Ok(rx)
    }
}

fn to_input_items(m: &Message) -> Vec<Value> {
    match m.role {
        Role::User => {
            let mut items = Vec::new();
            let mut parts: Vec<Value> = Vec::new();
            for b in &m.content {
                match b {
                    ContentBlock::Text { text } => {
                        parts.push(json!({ "type": "input_text", "text": text }));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content.model_text(),
                        }));
                    }
                    ContentBlock::ToolUse { .. }
                    | ContentBlock::Thinking { .. }
                    | ContentBlock::RedactedThinking { .. }
                    | ContentBlock::ReasoningItem { .. } => {}
                }
            }
            if !parts.is_empty() {
                items.push(json!({ "type": "message", "role": "user", "content": parts }));
            }
            items
        }
        Role::Tool => m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some(json!({
                    "type": "function_call_output",
                    "call_id": tool_use_id,
                    "output": content.model_text(),
                })),
                ContentBlock::Text { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::ReasoningItem { .. } => None,
            })
            .collect(),
        Role::Assistant => {
            let mut items = Vec::new();
            // Echo opaque reasoning verbatim for store:false continuity.
            for b in &m.content {
                if let ContentBlock::ReasoningItem { item } = b {
                    items.push(item.clone());
                }
            }
            let text: String = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if !text.is_empty() {
                items.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text }],
                }));
            }
            for b in &m.content {
                if let ContentBlock::ToolUse { id, name, input } = b {
                    items.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": input.to_string(),
                    }));
                }
            }
            items
        }
        Role::System => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod requests {
        use super::*;
        use crate::auth::StaticKey;
        use crate::core::store::{MemoryStore, SessionStore};
        use crate::test_support::http::{all_content_blocks, assert_workflow_tool, workflow_spec};
        use crate::tools::memory::MemoryTool;
        use crate::tools::registry::Tool;

        #[tokio::test]
        async fn workflow_request_preserves_details_without_root_combiners() {
            let spec = workflow_spec();
            let provider = ResponsesProvider::with_auth(
                "gpt-5".into(),
                "https://example.invalid".into(),
                Arc::new(StaticKey("test".into())),
            );
            let opts = GenerateOptions {
                messages: vec![Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Hello".into(),
                    }],
                }],
                tools: vec![spec.clone()],
                ..Default::default()
            };
            let request = provider
                .request(provider.build_body(&opts))
                .await
                .unwrap()
                .build()
                .unwrap();
            assert_eq!(request.url().as_str(), "https://example.invalid/responses");
            assert_eq!(request.headers()["authorization"], "Bearer test");
            assert_eq!(request.headers()["openai-beta"], "responses=v1");
            let payload: Value =
                serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
            let tool = &payload["tools"][0];
            assert_eq!(tool["type"], "function");
            assert_eq!(tool["strict"], false);
            assert_workflow_tool(tool, "parameters", &spec);
        }

        #[test]
        fn every_content_block_has_an_explicit_wire_or_skip_expectation() {
            for role in [Role::Assistant, Role::User, Role::Tool, Role::System] {
                for block in all_content_blocks() {
                    let expected = match role {
                        Role::Assistant => match &block {
                            ContentBlock::Text { .. } => vec![json!({
                                "type":"message","role":"assistant",
                                "content":[{"type":"output_text","text":"hi"}]
                            })],
                            ContentBlock::ToolUse { .. } => vec![json!({
                                "type":"function_call","call_id":"t1","name":"read_file",
                                "arguments":"{\"path\":\"a.rs\"}"
                            })],
                            ContentBlock::ReasoningItem { .. } => vec![json!({"id":"r1"})],
                            ContentBlock::ToolResult { .. }
                            | ContentBlock::Thinking { .. }
                            | ContentBlock::RedactedThinking { .. } => vec![],
                        },
                        Role::User => match &block {
                            ContentBlock::Text { .. } => vec![json!({
                                "type":"message","role":"user",
                                "content":[{"type":"input_text","text":"hi"}]
                            })],
                            ContentBlock::ToolResult { .. } => vec![json!({
                                "type":"function_call_output","call_id":"t1","output":"ok"
                            })],
                            ContentBlock::ToolUse { .. }
                            | ContentBlock::Thinking { .. }
                            | ContentBlock::RedactedThinking { .. }
                            | ContentBlock::ReasoningItem { .. } => vec![],
                        },
                        Role::Tool => match &block {
                            ContentBlock::ToolResult { .. } => vec![json!({
                                "type":"function_call_output","call_id":"t1","output":"ok"
                            })],
                            ContentBlock::Text { .. }
                            | ContentBlock::ToolUse { .. }
                            | ContentBlock::Thinking { .. }
                            | ContentBlock::RedactedThinking { .. }
                            | ContentBlock::ReasoningItem { .. } => vec![],
                        },
                        Role::System => vec![],
                    };
                    let message = Message {
                        role,
                        content: vec![block],
                    };
                    assert_eq!(to_input_items(&message), expected, "{message:?}");
                }
            }
        }

        #[tokio::test]
        async fn memory_request_keeps_action_specific_fields_optional() {
            let store = MemoryStore::new();
            let spec = MemoryTool {
                repository: store.memory_repository().unwrap(),
            }
            .spec();
            let provider = ResponsesProvider::with_auth(
                "test".into(),
                "http://127.0.0.1:1".into(),
                Arc::new(StaticKey("test".into())),
            );
            let options = GenerateOptions {
                tools: vec![spec.clone()],
                ..Default::default()
            };
            let request = provider
                .request(provider.build_body(&options))
                .await
                .unwrap()
                .build()
                .unwrap();
            let body: Value =
                serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
            let tool = &body["tools"][0];
            assert_eq!(tool["strict"], false);
            assert_eq!(tool["name"], "memory");
            assert_eq!(tool["parameters"], spec.input_schema);
            assert!(tool["parameters"].get("required").is_none());
            let validator = jsonschema::validator_for(&tool["parameters"]).unwrap();
            for arguments in [
                json!({"content":"Use scoped tests to avoid unrelated changes."}),
                json!({"action":"list"}),
                json!({"action":"forget","id":1}),
            ] {
                assert!(validator.is_valid(&arguments));
            }
        }

        #[test]
        fn stateless_request_omits_unset_options_and_maps_explicit_options() {
            let provider = ResponsesProvider::with_auth(
                "gpt-5.6-sol".into(),
                "https://example.invalid".into(),
                Arc::new(StaticKey("test".into())),
            );
            assert_eq!(
                provider.build_body(&GenerateOptions::default()),
                json!({
                    "model":"gpt-5.6-sol","input":[],"stream":true,"store":false
                })
            );
            let options = GenerateOptions {
                system: Some("Be precise".into()),
                messages: vec![Message::user_text("日本語 😀")],
                max_tokens: Some(321),
                reasoning: ReasoningEffort::High,
                tools: vec![ToolSpec {
                    name: "read_file".into(),
                    description: "Read a file".into(),
                    input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
                }],
                ..Default::default()
            };
            assert_eq!(
                provider.build_body(&options),
                json!({
                    "model":"gpt-5.6-sol","stream":true,"store":false,
                    "instructions":"Be precise","max_output_tokens":321,
                    "reasoning":{"effort":"high"},"include":["reasoning.encrypted_content"],
                    "input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"日本語 😀"}]}],
                    "tools":[{"type":"function","name":"read_file","description":"Read a file","strict":false,
                        "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}]
                })
            );
        }

        #[test]
        fn assistant_echo_keeps_opaque_reasoning_and_real_call_ids() {
            let message = Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ReasoningItem {
                        item: json!({"type":"reasoning","id":"reasoning-1","encrypted_content":"opaque+/=","summary":[{"type":"summary_text","text":"考える"}]}),
                    },
                    ContentBlock::Text {
                        text: "日本語 ".into(),
                    },
                    ContentBlock::Text {
                        text: "😀".into()
                    },
                    ContentBlock::ToolUse {
                        id: "call-b".into(),
                        name: "read_file".into(),
                        input: json!({"path":"a\nb.rs"}),
                    },
                    ContentBlock::ToolUse {
                        id: "call-a".into(),
                        name: "list_agents".into(),
                        input: json!({}),
                    },
                ],
            };
            assert_eq!(
                to_input_items(&message),
                vec![
                    json!({"type":"reasoning","id":"reasoning-1","encrypted_content":"opaque+/=","summary":[{"type":"summary_text","text":"考える"}]}),
                    json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"日本語 😀"}]}),
                    json!({"type":"function_call","call_id":"call-b","name":"read_file","arguments":"{\"path\":\"a\\nb.rs\"}"}),
                    json!({"type":"function_call","call_id":"call-a","name":"list_agents","arguments":"{}"}),
                ]
            );
        }

        #[tokio::test]
        async fn tool_outputs_remain_strings_in_serialized_requests() {
            let provider = ResponsesProvider::with_auth(
                "gpt-5".into(),
                "https://example.invalid".into(),
                Arc::new(StaticKey("test".into())),
            );
            for role in [Role::Tool, Role::User] {
                let (mut message, shell) = crate::test_support::http::tool_output_fixture();
                message.role = role;
                let options = GenerateOptions {
                    messages: vec![message],
                    ..Default::default()
                };
                let request = provider
                    .request(provider.build_body(&options))
                    .await
                    .unwrap()
                    .build()
                    .unwrap();
                let body: Value =
                    serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
                let output = &body["input"][0]["output"];
                let decoded: ShellOutput = serde_json::from_str(output.as_str().unwrap()).unwrap();
                assert_eq!(decoded, shell);
                assert_eq!(
                    body["input"],
                    json!([
                        {"type":"function_call_output","call_id":"shell-call","output":output},
                        {"type":"function_call_output","call_id":"text-call","output":"plain \"text\"\n日本語"}
                    ])
                );
            }
        }

        #[test]
        fn user_text_parts_and_tool_outputs_keep_their_distinct_wire_shapes() {
            let user = Message {
                role: Role::User,
                content: vec![
                    ContentBlock::Text { text: "one".into() },
                    ContentBlock::Text {
                        text: "日本語 😀".into(),
                    },
                ],
            };
            assert_eq!(
                to_input_items(&user),
                vec![json!({"type":"message","role":"user","content":[
                    {"type":"input_text","text":"one"},{"type":"input_text","text":"日本語 😀"}
                ]})]
            );
            for role in [Role::Tool, Role::User] {
                let results = Message {
                    role,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call-b".into(),
                            content: "not found".into(),
                            is_error: Some(true),
                        },
                        ContentBlock::ToolResult {
                            tool_use_id: "call-a".into(),
                            content: "[]".into(),
                            is_error: Some(false),
                        },
                    ],
                };
                assert_eq!(
                    to_input_items(&results),
                    vec![
                        json!({"type":"function_call_output","call_id":"call-b","output":"not found"}),
                        json!({"type":"function_call_output","call_id":"call-a","output":"[]"}),
                    ]
                );
            }
        }
    }

    mod streams {
        use super::*;
        use crate::auth::StaticKey;
        use crate::test_support::http::*;

        fn responses(base: String, timeouts: StreamTimeouts) -> Arc<dyn Provider> {
            Arc::new(
                ResponsesProvider::with_auth(
                    "test".into(),
                    base,
                    Arc::new(StaticKey("test".into())),
                )
                .with_stream_timeouts(timeouts),
            )
        }

        #[tokio::test]
        async fn responses_terminal_eof_deadlines_and_cancellation() {
            lifecycle_checks(
                responses,
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
            )
            .await;
        }

        #[tokio::test]
        async fn responses_final_output_is_authoritative_and_usage_does_not_double_count_cache() {
            let output = json!({"type":"response.completed","response":{
                "usage":{"input_tokens":100,"output_tokens":7,"input_tokens_details":{"cached_tokens":40}},
                "output":[{"type":"function_call","call_id":"real-id","name":"memory","arguments":"{\"action\":\"list\"}","status":"completed"}]
            }});
            let stale_text =
                json!({"type":"response.output_text.delta","output_index":0,"delta":"stale text"});
            let stale_call = json!({"type":"response.output_item.added","output_index":1,
                "item":{"type":"function_call","call_id":"stale-id","name":"read_file"}});
            let body = format!(
                "data: {stale_text}\n\ndata: {stale_call}\n\ndata: {output}\n\ndata: broken\n\n"
            );
            let (base, server) = serve(&body, true).await;
            let events = collect(responses(base, deadlines())).await;
            let completion = events
                .iter()
                .find_map(|event| match event {
                    StreamEvent::MessageStop { completion } => Some(completion),
                    _ => None,
                })
                .unwrap();
            assert_eq!(completion.usage.input_tokens, 60);
            assert_eq!(completion.usage.cache_read_input_tokens, 40);
            assert_eq!(completion.usage.output_tokens, 7);
            assert_eq!(completion.stop_reason, StopReason::ToolUse);
            assert_eq!(completion.usage.cache_creation_input_tokens, 0);
            assert_eq!(
                serde_json::to_value(&completion.message).unwrap(),
                json!({"role":"assistant","content":[
                    {"type":"tool_use","id":"real-id","name":"memory","input":{"action":"list"}}
                ]})
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e, StreamEvent::MessageStop { .. }))
                    .count(),
                1
            );
            assert!(!events
                .iter()
                .any(|e| matches!(e, StreamEvent::Error { .. })));
            server.await.unwrap();
        }

        #[tokio::test]
        async fn responses_preserves_error_codes_for_retry_classification() {
            for (event, retryable) in [
                (
                    json!({"type":"response.failed","response":{"error":{"code":"server_error","message":"Internal server error"}}}),
                    true,
                ),
                (
                    json!({"type":"error","code":"service_unavailable","message":"Try again"}),
                    true,
                ),
                (
                    json!({"type":"error","error":{"code":"invalid_api_key","message":"Rejected"}}),
                    false,
                ),
            ] {
                let (base, server) = serve(&format!("data: {event}\n\n"), true).await;
                let events = collect(responses(base, deadlines())).await;
                assert_error(&events);
                let message = events
                    .iter()
                    .find_map(|event| match event {
                        StreamEvent::Error { message } => Some(message),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(
                    crate::providers::provider::is_transient_error(message),
                    retryable,
                    "{message}"
                );
                server.await.unwrap();
            }
        }

        #[tokio::test]
        async fn responses_errors_and_incomplete_tool_calls_never_complete() {
            for event in [
                json!({"type":"response.failed","response":{"error":{"message":"failed"}}}),
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"}}}),
                json!({"type":"response.completed","response":{"output":[{"type":"function_call","id":"item-not-call-id","name":"memory","arguments":"{}"}]}}),
                json!({"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"call1","name":"memory","arguments":"{\"action\":"}]}}),
            ] {
                let (base, server) = serve(&format!("data: {event}\n\n"), true).await;
                assert_error(&collect(responses(base, deadlines())).await);
                server.await.unwrap();
            }
        }

        #[tokio::test]
        async fn truncated_response_keeps_usage_without_executable_calls() {
            let event = json!({"type":"response.incomplete","response":{
                "incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":100,"output_tokens":20},
                "output":[{"type":"function_call","call_id":"call1","name":"memory","arguments":"{"}]
            }});
            let (base, server) = serve(&format!("data: {event}\n\n"), true).await;
            let events = collect(responses(base, deadlines())).await;
            let completion = events
                .iter()
                .find_map(|event| match event {
                    StreamEvent::MessageStop { completion } => Some(completion),
                    _ => None,
                })
                .unwrap();
            assert_eq!(completion.stop_reason, StopReason::MaxTokens);
            assert_eq!(completion.usage.output_tokens, 20);
            assert!(!completion
                .message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. })));
            server.await.unwrap();
        }
    }
}
