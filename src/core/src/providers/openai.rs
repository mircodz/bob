//! OpenAI-compatible Chat Completions with static or refreshed bearer tokens.

use crate::core::types::*;
use crate::providers::provider::{Provider, StreamTimeouts};
use crate::providers::sse::{parse_sse_control, SseEvent};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Arc;
use tokio::sync::mpsc;

pub use crate::auth::AuthProvider as TokenSource;

struct OpenAiOauth;

#[async_trait]
impl TokenSource for OpenAiOauth {
    async fn token(&self) -> anyhow::Result<String> {
        crate::auth::openai::access_token().await
    }
}

/// Explicit API keys select Chat Completions. Otherwise, stored ChatGPT OAuth
/// selects Codex Responses before falling back to `OPENAI_API_KEY` for chat.
pub async fn create(
    model: Option<String>,
    auth: Option<crate::auth::ProviderAuth>,
) -> anyhow::Result<Arc<dyn Provider>> {
    create_with_options(model, auth, StreamTimeouts::default()).await
}

pub(crate) async fn create_with_options(
    model: Option<String>,
    auth: Option<crate::auth::ProviderAuth>,
    timeouts: StreamTimeouts,
) -> anyhow::Result<Arc<dyn Provider>> {
    use crate::providers::responses::ResponsesProvider;
    if let Some(crate::auth::ProviderAuth::ApiKey(key)) = auth {
        let base = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let source: Arc<dyn TokenSource> = Arc::new(crate::auth::StaticKey(key));
        return Ok(Arc::new(
            OpenAiProvider::with_auth(model.unwrap_or_else(|| "gpt-4o".to_string()), base, source)
                .with_stream_timeouts(timeouts),
        ));
    }
    if crate::auth::openai::is_logged_in() {
        let source: Arc<dyn TokenSource> = Arc::new(OpenAiOauth);
        Ok(Arc::new(
            ResponsesProvider::with_auth(
                model.unwrap_or_else(|| "gpt-5".to_string()),
                "https://chatgpt.com/backend-api/codex".to_string(),
                source,
            )
            .with_stream_timeouts(timeouts),
        ))
    } else {
        Ok(Arc::new(
            OpenAiProvider::new(model, None).with_stream_timeouts(timeouts),
        ))
    }
}

/// Bound connection setup, not total read time: streamed generations can take minutes.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Clone)]
pub struct OpenAiProvider {
    auth: Arc<dyn TokenSource>,
    model: String,
    base_url: String,
    client: reqwest::Client,
    /// Authoritative input-token budget; `None` uses model-name heuristics.
    context_window: Option<usize>,
    timeouts: StreamTimeouts,
}

impl OpenAiProvider {
    pub fn new(model: Option<String>, base_url: Option<String>) -> Self {
        let base_url = base_url
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let base_url = base_url.trim_end_matches('/').to_string();
        OpenAiProvider {
            auth: Arc::new(crate::auth::EnvKey {
                var: "OPENAI_API_KEY".to_string(),
                fallback: "unused".to_string(),
            }),
            model: model.unwrap_or_else(|| "gpt-4o".to_string()),
            base_url,
            client: http_client(),
            context_window: None,
            timeouts: StreamTimeouts::default(),
        }
    }

    pub fn with_auth(model: String, base_url: String, auth: Arc<dyn TokenSource>) -> Self {
        OpenAiProvider {
            auth,
            model,
            base_url: base_url.trim_end_matches('/').to_string(),
            client: http_client(),
            context_window: None,
            timeouts: StreamTimeouts::default(),
        }
    }

    pub fn with_context_window(mut self, window: Option<usize>) -> Self {
        self.context_window = window;
        self
    }

    pub fn with_stream_timeouts(mut self, timeouts: StreamTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    fn build_body(&self, opts: &GenerateOptions, stream: bool) -> Value {
        let mut messages: Vec<Value> = Vec::new();
        if let Some(sys) = &opts.system {
            messages.push(json!({ "role": "system", "content": sys }));
        }
        for m in &opts.messages {
            messages.extend(to_api_messages(m));
        }
        let mut body = json!({
            "model": self.model,
            "stream": stream,
            "messages": messages,
        });
        // Let the server enforce its output limit unless the caller pins one.
        if let Some(max) = opts.max_tokens {
            body["max_tokens"] = json!(max);
        }
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(t) = opts.temperature {
            body["temperature"] = json!(t);
        }
        // Non-reasoning models reject reasoning_effort; Off omits it.
        if chat_supports_reasoning(&self.model) {
            if let Some(effort) = opts.reasoning.as_str() {
                body["reasoning_effort"] = json!(effort);
                // Reasoning models reject an explicit temperature.
                body.as_object_mut().map(|o| o.remove("temperature"));
            }
        }
        if !opts.tools.is_empty() {
            body["tools"] = json!(opts
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                }))
                .collect::<Vec<_>>());
        }
        body
    }

    async fn authed(
        &self,
        mut req: reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        let token = self.auth.token().await?;
        req = req.header("authorization", format!("Bearer {}", token));
        for (k, v) in self.auth.extra_headers() {
            req = req.header(k, v);
        }
        Ok(req)
    }

    async fn request(&self, body: Value) -> anyhow::Result<reqwest::RequestBuilder> {
        let req = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("content-type", "application/json")
            .json(&body);
        self.authed(req).await
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
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
    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        crate::providers::provider::generate_with_retry(self, opts, &|_| {}).await
    }

    async fn generate_once(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        let body = self.build_body(&opts, false);
        let res = tokio::time::timeout(self.timeouts.startup, async {
            crate::providers::provider::send_once(self.request(body).await?, "openai").await
        })
        .await
        .map_err(|_| anyhow::anyhow!("openai startup deadline exceeded"))??;
        let data: Value = tokio::time::timeout(
            self.timeouts.idle,
            crate::providers::provider::response_json(res),
        )
        .await
        .map_err(|_| anyhow::anyhow!("openai response body deadline exceeded"))??;
        let choice = &data["choices"][0];
        let cached = data["usage"]["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        let prompt = data["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        Ok(Completion {
            message: from_api_message(&choice["message"]),
            stop_reason: map_finish(choice["finish_reason"].as_str().unwrap_or("stop")),
            usage: Usage {
                // prompt_tokens includes cache reads; input_tokens counts fresh input only.
                input_tokens: prompt.saturating_sub(cached),
                output_tokens: data["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                cache_read_input_tokens: cached,
                cache_creation_input_tokens: 0,
            },
        })
    }

    async fn stream(
        &self,
        opts: GenerateOptions,
    ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
        let body = self.build_body(&opts, true);
        let provider = self.clone();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let Some(res) = crate::providers::provider::start_stream(
                &tx, provider.timeouts.startup, async {
                    crate::providers::provider::send_once(provider.request(body).await?, "openai")
                        .await.map_err(|error| {
                            if error.to_string().contains("unsupported_api_for_model") {
                                anyhow::anyhow!("model '{}' needs the Responses API — log in with `bob login openai` or pick a chat model", provider.model)
                            } else { error }
                        })
                },
            ).await else { return; };
            let mut text = String::new();
            let mut finish = None;
            let mut protocol_error = None;
            let mut usage = Usage::default();
            // Tool calls arrive incrementally, keyed by index (ordered).
            let mut tool_acc: BTreeMap<i64, (String, String, String)> = BTreeMap::new();

            let consume = parse_sse_control(
                res,
                |event| {
                    let SseEvent::Json(evt) = event else {
                        return ControlFlow::Break(());
                    };
                    if let Some(error) = evt.get("error") {
                        protocol_error = Some(format!("openai: {error}"));
                        return ControlFlow::Break(());
                    }
                    // The final chunk carries usage with an empty choices array.
                    if let Some(u) = evt.get("usage") {
                        if !u.is_null() {
                            let prompt = u["prompt_tokens"].as_u64().unwrap_or(
                                usage
                                    .input_tokens
                                    .saturating_add(usage.cache_read_input_tokens),
                            );
                            let cached = u["prompt_tokens_details"]["cached_tokens"]
                                .as_u64()
                                .unwrap_or(usage.cache_read_input_tokens);
                            // Exclude cache reads from fresh input tokens.
                            usage.input_tokens = prompt.saturating_sub(cached);
                            usage.output_tokens = u["completion_tokens"]
                                .as_u64()
                                .unwrap_or(usage.output_tokens);
                            usage.cache_read_input_tokens = cached;
                        }
                    }
                    let choice = &evt["choices"][0];
                    if choice.is_null() {
                        return ControlFlow::Continue(());
                    }
                    let delta = &choice["delta"];

                    if let Some(c) = delta["content"].as_str() {
                        if !c.is_empty() {
                            text.push_str(c);
                            let _ = tx.send(StreamEvent::TextDelta {
                                text: c.to_string(),
                            });
                        }
                    }
                    if let Some(calls) = delta["tool_calls"].as_array() {
                        for tc in calls {
                            let idx = tc["index"].as_i64().unwrap_or(0);
                            let entry = tool_acc.entry(idx).or_insert_with(|| {
                                let id =
                                    tc["id"].as_str().map(|s| s.to_string()).unwrap_or_default();
                                let name =
                                    tc["function"]["name"].as_str().unwrap_or("").to_string();
                                if !name.is_empty() {
                                    let _ = tx.send(StreamEvent::ToolUseStart {
                                        id: id.clone(),
                                        name: name.clone(),
                                    });
                                }
                                (id, name, String::new())
                            });
                            if let Some(id) = tc["id"].as_str() {
                                entry.0 = id.to_string();
                            }
                            if entry.1.is_empty() {
                                if let Some(n) = tc["function"]["name"].as_str() {
                                    entry.1 = n.to_string();
                                }
                            }
                            if let Some(args) = tc["function"].get("arguments") {
                                let Some(args) = args.as_str() else {
                                    protocol_error = Some(format!(
                                        "openai: arguments for {} must be a JSON string",
                                        entry.1
                                    ));
                                    return ControlFlow::Break(());
                                };
                                entry.2.push_str(args);
                                let _ = tx.send(StreamEvent::ToolUseInputDelta {
                                    id: entry.0.clone(),
                                    partial_json: args.to_string(),
                                });
                            }
                        }
                    }
                    if let Some(fr) = choice["finish_reason"].as_str() {
                        finish = Some(fr.to_string());
                    }
                    ControlFlow::Continue(())
                },
                provider.timeouts.idle,
            );
            let sse = tokio::select! {
                biased;
                _ = tx.closed() => return,
                result = consume => result,
            };

            if let Err(e) = sse {
                let _ = tx.send(StreamEvent::Error {
                    message: format!("openai stream ended: {e}"),
                });
                return;
            }

            if let Some(message) = protocol_error {
                let _ = tx.send(StreamEvent::Error { message });
                return;
            }
            let Some(finish) = finish else {
                let _ = tx.send(StreamEvent::Error {
                    message: "openai stream ended without finish_reason".into(),
                });
                return;
            };
            if finish == "content_filter" {
                let _ = tx.send(StreamEvent::Error {
                    message: "openai completion blocked by content_filter".into(),
                });
                return;
            }
            let mut content: Vec<ContentBlock> = Vec::new();
            if !text.is_empty() {
                content.push(ContentBlock::Text { text });
            }
            if finish != "length" {
                for (_, (id, name, args)) in tool_acc {
                    let input = match crate::providers::codec::complete_tool_input(
                        &id,
                        &name,
                        &args,
                        &opts.tools,
                    ) {
                        Ok(input) => input,
                        Err(error) => {
                            let _ = tx.send(StreamEvent::Error {
                                message: format!("openai: {error}"),
                            });
                            return;
                        }
                    };
                    content.push(ContentBlock::ToolUse { id, name, input });
                }
            }
            let completion = Completion {
                message: Message {
                    role: Role::Assistant,
                    content,
                },
                stop_reason: map_finish(&finish),
                usage,
            };
            let _ = tx.send(StreamEvent::MessageStop { completion });
        });

        Ok(rx)
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let req = self.client.get(format!("{}/models", self.base_url));
        let res = self.authed(req).await?.send().await?;
        if !res.status().is_success() {
            anyhow::bail!(
                "models {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            );
        }
        let data: Value = res.json().await?;
        let mut ids: Vec<String> = data["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m["id"].as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    async fn list_models_detailed(
        &self,
    ) -> anyhow::Result<Vec<crate::providers::provider::ModelEntry>> {
        use crate::providers::provider::{context_window_for, ModelEntry};
        let req = self.client.get(format!("{}/models", self.base_url));
        let res = self.authed(req).await?.send().await?;
        if !res.status().is_success() {
            anyhow::bail!(
                "models {}: {}",
                res.status(),
                res.text().await.unwrap_or_default()
            );
        }
        let data: Value = res.json().await?;
        let mut entries: Vec<ModelEntry> = data["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| {
                        let id = m["id"].as_str()?.to_string();
                        // Backend input-token limits override model-name heuristics.
                        let limits = &m["capabilities"]["limits"];
                        let window = limits["max_prompt_tokens"]
                            .as_u64()
                            .or_else(|| limits["max_context_window_tokens"].as_u64())
                            .map(|n| n as usize)
                            .unwrap_or_else(|| context_window_for(&id));
                        Some(ModelEntry {
                            id,
                            context_window: Some(window),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort_by(|a, b| a.id.cmp(&b.id));
        entries.dedup_by(|a, b| a.id == b.id);
        Ok(entries)
    }
}

/// Whether an OpenAI-compatible chat-completions model accepts the
/// `reasoning_effort` field. True for the o-series and gpt-5 chat families;
/// false for classic chat models (gpt-4o, gpt-4, gpt-3.5) which reject it.
pub(crate) fn chat_supports_reasoning(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("gpt-5")
        // Copilot proxies some of these under a vendor prefix.
        || m.contains("/o1")
        || m.contains("/o3")
        || m.contains("/o4")
        || m.contains("/gpt-5")
}

fn map_finish(reason: &str) -> StopReason {
    crate::providers::codec::map_stop_reason(reason)
}

/// Tool results split into separate Chat Completions messages.
fn to_api_messages(m: &Message) -> Vec<Value> {
    if m.role == Role::Tool {
        return m
            .content
            .iter()
            .filter_map(|b| match classify_block(b) {
                OpenAiBlock::ToolResult {
                    tool_use_id,
                    content,
                } => Some(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                })),
                _ => None,
            })
            .collect();
    }

    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for b in &m.content {
        match classify_block(b) {
            OpenAiBlock::Text(t) => text.push_str(&t),
            OpenAiBlock::ToolCall(v) => tool_calls.push(v),
            // Tool results require Role::Tool; reasoning blocks have no chat representation.
            OpenAiBlock::ToolResult { .. } | OpenAiBlock::SkipUnsupported => {}
        }
    }

    let role = match m.role {
        Role::Assistant => "assistant",
        Role::User => "user",
        Role::System => "system",
        Role::Tool => "tool",
    };
    let mut msg = json!({
        "role": role,
        "content": if text.is_empty() { Value::Null } else { json!(text) },
    });
    if !tool_calls.is_empty() {
        msg["tool_calls"] = json!(tool_calls);
    }
    vec![msg]
}

enum OpenAiBlock {
    Text(String),
    ToolCall(Value),
    ToolResult {
        tool_use_id: String,
        content: String,
    },
    SkipUnsupported,
}

fn classify_block(b: &ContentBlock) -> OpenAiBlock {
    match b {
        ContentBlock::Text { text } => OpenAiBlock::Text(text.clone()),
        ContentBlock::ToolUse { id, name, input } => OpenAiBlock::ToolCall(json!({
            "id": id,
            "type": "function",
            "function": { "name": name, "arguments": input.to_string() },
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => OpenAiBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.model_text().into_owned(),
        },
        // Anthropic thinking and Responses reasoning have no chat representation.
        ContentBlock::Thinking { .. }
        | ContentBlock::RedactedThinking { .. }
        | ContentBlock::ReasoningItem { .. } => OpenAiBlock::SkipUnsupported,
    }
}

fn from_api_message(msg: &Value) -> Message {
    let mut content: Vec<ContentBlock> = Vec::new();
    if let Some(c) = msg["content"].as_str() {
        if !c.is_empty() {
            content.push(ContentBlock::Text {
                text: c.to_string(),
            });
        }
    }
    if let Some(calls) = msg["tool_calls"].as_array() {
        for tc in calls {
            let args = tc["function"]["arguments"].as_str().unwrap_or("");
            content.push(ContentBlock::ToolUse {
                id: tc["id"].as_str().unwrap_or("").to_string(),
                name: tc["function"]["name"].as_str().unwrap_or("").to_string(),
                input: crate::providers::codec::parse_tool_input(args),
            });
        }
    }
    Message {
        role: Role::Assistant,
        content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod requests {
        use super::*;
        use crate::test_support::http::{all_content_blocks, assert_workflow_tool, workflow_spec};

        #[tokio::test]
        async fn workflow_request_preserves_details_without_root_combiners() {
            use crate::auth::StaticKey;

            let spec = workflow_spec();
            let provider = OpenAiProvider::with_auth(
                "gpt-4o".into(),
                "https://example.invalid".into(),
                Arc::new(StaticKey("test".into())),
            );
            for stream in [false, true] {
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
                    .request(provider.build_body(&opts, stream))
                    .await
                    .unwrap()
                    .build()
                    .unwrap();
                assert_eq!(
                    request.url().as_str(),
                    "https://example.invalid/chat/completions"
                );
                assert_eq!(request.headers()["authorization"], "Bearer test");
                assert_eq!(request.headers()["content-type"], "application/json");
                let payload: Value =
                    serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
                assert_eq!(payload["tools"][0]["type"], "function");
                let tool = &payload["tools"][0]["function"];
                assert_workflow_tool(tool, "parameters", &spec);
            }
        }

        #[test]
        fn reasoning_capability_gate() {
            assert!(chat_supports_reasoning("o1"));
            assert!(chat_supports_reasoning("o3-mini"));
            assert!(chat_supports_reasoning("gpt-5.1"));
            assert!(chat_supports_reasoning("copilot/gpt-5"));
            assert!(!chat_supports_reasoning("gpt-4o"));
            assert!(!chat_supports_reasoning("claude-sonnet-4-5"));
            assert!(!chat_supports_reasoning("gpt-3.5-turbo"));
        }

        #[test]
        fn every_content_block_has_an_explicit_wire_or_skip_expectation() {
            for block in all_content_blocks() {
                let (role, expected) = match &block {
                    ContentBlock::Text { .. } => {
                        (Role::Assistant, json!({"role":"assistant","content":"hi"}))
                    }
                    ContentBlock::ToolUse { .. } => {
                        let call = json!({
                            "id":"t1","type":"function",
                            "function":{"name":"read_file","arguments":"{\"path\":\"a.rs\"}"}
                        });
                        (
                            Role::Assistant,
                            json!({"role":"assistant","content":null,"tool_calls":[call]}),
                        )
                    }
                    ContentBlock::ToolResult { .. } => (
                        Role::Tool,
                        json!({"role":"tool","tool_call_id":"t1","content":"ok"}),
                    ),
                    ContentBlock::Thinking { .. }
                    | ContentBlock::RedactedThinking { .. }
                    | ContentBlock::ReasoningItem { .. } => {
                        (Role::Assistant, json!({"role":"assistant","content":null}))
                    }
                };
                let message = Message {
                    role,
                    content: vec![block],
                };
                assert_eq!(to_api_messages(&message), vec![expected], "{message:?}");
            }
        }
        #[test]
        fn request_options_gate_reasoning_temperature_and_stream_usage() {
            for (model, effort, expected_effort, expected_temperature) in [
                ("gpt-4o", ReasoningEffort::High, None, Some(0.5)),
                ("o3-mini", ReasoningEffort::Off, None, Some(0.5)),
                ("o3-mini", ReasoningEffort::Low, Some("low"), None),
                ("COPILOT/GPT-5", ReasoningEffort::Max, Some("high"), None),
            ] {
                let provider = OpenAiProvider::with_auth(
                    model.into(),
                    "https://example.invalid/".into(),
                    Arc::new(crate::auth::StaticKey("test".into())),
                );
                for stream in [false, true] {
                    let options = GenerateOptions {
                        system: Some("Be precise".into()),
                        messages: vec![Message::user_text("日本語 😀")],
                        temperature: Some(0.5),
                        reasoning: effort,
                        ..Default::default()
                    };
                    let body = provider.build_body(&options, stream);
                    assert_eq!(
                        body["messages"],
                        json!([
                            {"role":"system","content":"Be precise"},
                            {"role":"user","content":"日本語 😀"}
                        ])
                    );
                    assert_eq!(body["model"], model);
                    assert_eq!(body["stream"], stream);
                    assert_eq!(
                        body.get("reasoning_effort"),
                        expected_effort.map(|v| json!(v)).as_ref()
                    );
                    assert_eq!(
                        body.get("temperature"),
                        expected_temperature.map(|v| json!(v)).as_ref()
                    );
                    assert_eq!(
                        body.get("stream_options"),
                        stream.then(|| json!({"include_usage":true})).as_ref()
                    );
                    assert!(body.get("tools").is_none());
                    assert!(body.get("max_tokens").is_none());
                    let pinned = GenerateOptions {
                        max_tokens: Some(123),
                        ..options
                    };
                    assert_eq!(provider.build_body(&pinned, stream)["max_tokens"], 123);
                }
            }
        }

        #[tokio::test]
        async fn tool_outputs_remain_strings_in_serialized_requests() {
            let provider = OpenAiProvider::with_auth(
                "gpt-4o".into(),
                "https://example.invalid".into(),
                Arc::new(crate::auth::StaticKey("test".into())),
            );
            let (message, shell) = crate::test_support::http::tool_output_fixture();
            let options = GenerateOptions {
                messages: vec![message],
                ..Default::default()
            };
            for stream in [false, true] {
                let request = provider
                    .request(provider.build_body(&options, stream))
                    .await
                    .unwrap()
                    .build()
                    .unwrap();
                let body: Value =
                    serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap();
                let content = &body["messages"][0]["content"];
                let decoded: ShellOutput = serde_json::from_str(content.as_str().unwrap()).unwrap();
                assert_eq!(decoded, shell);
                assert_eq!(
                    body["messages"],
                    json!([
                        {"role":"tool","tool_call_id":"shell-call","content":content},
                        {"role":"tool","tool_call_id":"text-call","content":"plain \"text\"\n日本語"}
                    ])
                );
            }
        }

        #[test]
        fn assistant_text_and_multiple_call_ids_and_arguments_are_preserved() {
            let message = Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "日本語 ".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "call-b".into(),
                        name: "read_file".into(),
                        input: json!({"path":"a\nb.rs"}),
                    },
                    ContentBlock::Text {
                        text: "😀".into()
                    },
                    ContentBlock::ToolUse {
                        id: "call-a".into(),
                        name: "list_agents".into(),
                        input: json!({}),
                    },
                ],
            };
            assert_eq!(
                to_api_messages(&message),
                vec![json!({
                    "role":"assistant","content":"日本語 😀","tool_calls":[
                        {"id":"call-b","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\\nb.rs\"}"}},
                        {"id":"call-a","type":"function","function":{"name":"list_agents","arguments":"{}"}}
                    ]
                })]
            );
            let results = Message {
                role: Role::Tool,
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
                to_api_messages(&results),
                vec![
                    json!({"role":"tool","tool_call_id":"call-b","content":"not found"}),
                    json!({"role":"tool","tool_call_id":"call-a","content":"[]"}),
                ]
            );
        }

        #[test]
        fn response_decoding_keeps_call_identity_and_tags_only_broken_arguments() {
            let message = from_api_message(&json!({"content":"日本語 😀","tool_calls":[
                {"id":"call-1","function":{"name":"read_file","arguments":"{\"path\":\"日本語.rs\"}"}},
                {"id":"call-2","function":{"name":"read_file","arguments":"{\"path\":"}}
            ]}));
            assert_eq!(message.role, Role::Assistant);
            assert_eq!(message.content.len(), 3);
            assert!(
                matches!(&message.content[0], ContentBlock::Text { text } if text == "日本語 😀")
            );
            assert!(
                matches!(&message.content[1], ContentBlock::ToolUse { id, name, input }
                if id == "call-1" && name == "read_file" && input == &json!({"path":"日本語.rs"}))
            );
            let ContentBlock::ToolUse { id, name, input } = &message.content[2] else {
                panic!("missing malformed call: {message:?}");
            };
            assert_eq!(id, "call-2");
            assert_eq!(name, "read_file");
            assert_eq!(
                input[crate::providers::codec::TOOL_ARGS_PARSE_ERROR_KEY]["raw"],
                "{\"path\":"
            );
            assert!(crate::providers::codec::tool_input_parse_error(input).is_some());
        }
    }

    mod streams {
        use super::*;
        use crate::auth::StaticKey;
        use crate::test_support::http::*;

        fn chat(base: String, timeouts: StreamTimeouts) -> Arc<dyn Provider> {
            Arc::new(
                OpenAiProvider::with_auth("test".into(), base, Arc::new(StaticKey("test".into())))
                    .with_stream_timeouts(timeouts),
            )
        }

        #[tokio::test]
        async fn chat_terminal_eof_deadlines_and_cancellation() {
            lifecycle_checks(
                chat,
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            )
            .await;
        }

        #[tokio::test]
        async fn chat_requires_finish_reason_even_after_done() {
            let (base, server) = serve("data: [DONE]\n\n", false).await;
            assert_error(&collect(chat(base, deadlines())).await);
            server.await.unwrap();
        }

        #[tokio::test]
        async fn chat_finish_at_eof_preserves_final_usage() {
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":5,\"prompt_tokens_details\":{\"cached_tokens\":40}}}\n\n";
            let (base, server) = serve(body, false).await;
            let events = collect(chat(base, deadlines())).await;
            let completion = events
                .iter()
                .find_map(|event| match event {
                    StreamEvent::MessageStop { completion } => Some(completion),
                    _ => None,
                })
                .unwrap();
            assert_eq!(completion.usage.input_tokens, 60);
            assert_eq!(completion.usage.cache_read_input_tokens, 40);
            assert_eq!(completion.usage.output_tokens, 5);
            assert_eq!(completion.message.role, Role::Assistant);
            assert_eq!(completion.stop_reason, StopReason::EndTurn);
            assert_eq!(completion.usage.cache_creation_input_tokens, 0);
            assert_eq!(
                serde_json::to_value(&completion.message.content).unwrap(),
                json!([{"type":"text","text":"ok"}])
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

        fn tool_options() -> GenerateOptions {
            use crate::tools::registry::Tool;
            GenerateOptions {
                tools: vec![
                    crate::tools::coordinate::ListAgentsTool {
                        team: Default::default(),
                    }
                    .spec(),
                    crate::tools::builtin::ReadFileTool.spec(),
                ],
                ..Default::default()
            }
        }

        #[tokio::test]
        async fn chat_empty_arguments_follow_the_advertised_tool_schema() {
            for (name, arguments, succeeds) in [
                ("list_agents", None, true),
                ("list_agents", Some(json!("")), true),
                ("list_agents", Some(json!("{}")), true),
                ("list_agents", Some(json!("{")), false),
                ("list_agents", Some(json!(" ")), false),
                ("list_agents", Some(json!("[]")), false),
                ("list_agents", Some(json!(null)), false),
                ("list_agents", Some(json!({})), false),
                ("read_file", None, false),
                ("read_file", Some(json!("")), false),
                ("unknown_tool", Some(json!("")), false),
            ] {
                let mut function = json!({"name": name});
                if let Some(arguments) = arguments {
                    function["arguments"] = arguments;
                }
                let delta = json!({"choices":[{"delta":{"tool_calls":[{
                    "index":0,"id":"call-1","function":function
                }]},"finish_reason":"tool_calls"}]});
                let body = format!("data: {delta}\n\ndata: [DONE]\n\n");
                let (base, server) = serve(&body, false).await;
                let events = collect_with_options(chat(base, deadlines()), tool_options()).await;
                if succeeds {
                    assert!(
                        !events
                            .iter()
                            .any(|event| matches!(event, StreamEvent::Error { .. })),
                        "{events:?}"
                    );
                    let completions: Vec<_> = events
                        .iter()
                        .filter_map(|event| match event {
                            StreamEvent::MessageStop { completion } => Some(completion),
                            _ => None,
                        })
                        .collect();
                    assert_eq!(completions.len(), 1, "{events:?}");
                    assert_eq!(completions[0].stop_reason, StopReason::ToolUse);
                    assert_eq!(
                        serde_json::to_value(&completions[0].message).unwrap(),
                        json!({
                            "role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"list_agents","input":{}}]
                        })
                    );
                } else {
                    assert_error(&events);
                }
                server.await.unwrap();
            }
        }

        #[tokio::test]
        async fn chat_empty_calls_do_not_change_fragmented_sibling_arguments() {
            let first = json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"list","function":{"name":"list_agents","arguments":""}},
                {"index":1,"id":"read","function":{"name":"read_file","arguments":"{\"path\":\"src/"}}
            ]}}]});
            let last = json!({"choices":[{"delta":{"tool_calls":[
                {"index":1,"function":{"arguments":"日本語.rs\"}"}}
            ]},"finish_reason":"tool_calls"}]});
            let body = format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n");
            let (base, server) = serve(&body, false).await;
            let events = collect_with_options(chat(base, deadlines()), tool_options()).await;
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, StreamEvent::Error { .. })),
                "{events:?}"
            );
            let completion = events
                .iter()
                .find_map(|event| match event {
                    StreamEvent::MessageStop { completion } => Some(completion),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                serde_json::to_value(&completion.message).unwrap(),
                json!({
                    "role":"assistant","content":[
                        {"type":"tool_use","id":"list","name":"list_agents","input":{}},
                        {"type":"tool_use","id":"read","name":"read_file","input":{"path":"src/日本語.rs"}}
                    ]
                })
            );
            server.await.unwrap();
        }

        #[tokio::test]
        async fn chat_empty_calls_still_require_successful_stream_termination() {
            for finish in [None, Some("length"), Some("content_filter")] {
                let delta = json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"call-1","function":{"name":"list_agents"}}
                ]},"finish_reason":finish}]});
                let body = format!("data: {delta}\n\ndata: [DONE]\n\n");
                let (base, server) = serve(&body, false).await;
                let events = collect_with_options(chat(base, deadlines()), tool_options()).await;
                if finish == Some("length") {
                    let completion = events
                        .iter()
                        .find_map(|event| match event {
                            StreamEvent::MessageStop { completion } => Some(completion),
                            _ => None,
                        })
                        .unwrap();
                    assert_eq!(completion.stop_reason, StopReason::MaxTokens);
                    assert!(completion.message.content.is_empty());
                } else {
                    assert_error(&events);
                }
                server.await.unwrap();
            }
        }

        #[tokio::test]
        async fn chat_requires_complete_tool_arguments_and_real_ids() {
            for (id, args) in [
                ("call1", "{\"path\":"),
                ("call1", ""),
                ("", "{}"),
                ("call1", "[]"),
            ] {
                let delta = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"function":{"name":"read_file","arguments":args}}]},"finish_reason":"tool_calls"}]});
                let body = format!("data: {delta}\n\ndata: [DONE]\n\n");
                let (base, server) = serve(&body, false).await;
                assert_error(&collect(chat(base, deadlines())).await);
                server.await.unwrap();
            }
        }
    }
}
