//! Anthropic Messages API with prompt caching.

use crate::core::types::*;
use crate::providers::codec::BlockWire;
use crate::providers::provider::{Provider, StreamTimeouts};
use crate::providers::sse::{parse_sse_control, SseEvent};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::ops::ControlFlow;
use tokio::sync::mpsc;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// Explicit API keys use `x-api-key`; otherwise prefer stored OAuth over
/// `ANTHROPIC_API_KEY`.
pub async fn create(
    model: Option<String>,
    auth: Option<crate::auth::ProviderAuth>,
) -> anyhow::Result<std::sync::Arc<dyn Provider>> {
    create_with_options(model, auth, StreamTimeouts::default()).await
}

pub(crate) async fn create_with_options(
    model: Option<String>,
    auth: Option<crate::auth::ProviderAuth>,
    timeouts: StreamTimeouts,
) -> anyhow::Result<std::sync::Arc<dyn Provider>> {
    let model = model.unwrap_or_else(|| "claude-sonnet-4-5-20250929".to_string());
    if let Some(crate::auth::ProviderAuth::ApiKey(key)) = auth {
        return Ok(std::sync::Arc::new(AnthropicProvider {
            auth: AuthMode::ApiKey(key),
            model,
            base_url: API_URL.to_string(),
            client: reqwest::Client::new(),
            timeouts,
        }));
    }
    Ok(std::sync::Arc::new(
        AnthropicProvider::new(Some(model))?.with_stream_timeouts(timeouts),
    ))
}

#[derive(Clone)]
enum AuthMode {
    ApiKey(String),
    /// Subscription OAuth requires Bearer auth and the OAuth beta header.
    Oauth(std::sync::Arc<dyn crate::auth::AuthProvider>),
}

#[derive(Clone)]
pub struct AnthropicProvider {
    auth: AuthMode,
    model: String,
    base_url: String,
    client: reqwest::Client,
    timeouts: StreamTimeouts,
}

impl AnthropicProvider {
    pub fn new(model: Option<String>) -> anyhow::Result<Self> {
        if crate::auth::anthropic::is_logged_in() {
            return Ok(AnthropicProvider {
                auth: AuthMode::Oauth(std::sync::Arc::new(crate::auth::AnthropicOAuth)),
                model: model.unwrap_or_else(|| "claude-sonnet-4-5-20250929".to_string()),
                base_url: API_URL.to_string(),
                client: reqwest::Client::new(),
                timeouts: StreamTimeouts::default(),
            });
        }
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            anyhow::bail!("no Anthropic auth — run `bob login anthropic` or set ANTHROPIC_API_KEY");
        }
        Ok(AnthropicProvider {
            auth: AuthMode::ApiKey(api_key),
            model: model.unwrap_or_else(|| "claude-sonnet-4-5-20250929".to_string()),
            base_url: API_URL.to_string(),
            client: reqwest::Client::new(),
            timeouts: StreamTimeouts::default(),
        })
    }

    pub fn with_stream_timeouts(mut self, timeouts: StreamTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    fn build_body(&self, opts: &GenerateOptions, stream: bool) -> Value {
        let cache = opts.cache;
        let cc = json!({ "cache_control": { "type": "ephemeral" } });

        // System prompt as a text block so we can attach a cache breakpoint.
        let system = opts.system.as_ref().map(|s| {
            let mut block = json!({ "type": "text", "text": s });
            if cache {
                merge_into(&mut block, &cc);
            }
            json!([block])
        });

        // Tools: cache the whole set by marking the LAST tool.
        let n_tools = opts.tools.len();
        let tools: Vec<Value> = opts
            .tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut v = json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                });
                if cache && i == n_tools - 1 {
                    merge_into(&mut v, &cc);
                }
                v
            })
            .collect();

        // History: cache the growing prefix by marking the last block of the
        // second-to-last message.
        let mut messages: Vec<Value> = opts.messages.iter().map(to_api_message).collect();
        if cache && messages.len() >= 2 {
            let idx = messages.len() - 2;
            if let Some(blocks) = messages[idx]
                .get_mut("content")
                .and_then(|c| c.as_array_mut())
            {
                if let Some(last) = blocks.last_mut() {
                    merge_into(last, &cc);
                }
            }
        }

        let mut body = json!({
            "model": self.model,
            "max_tokens": opts.max_tokens.unwrap_or_else(|| {
                crate::providers::provider::max_output_tokens_for(&self.model)
            }),
            "stream": stream,
            "messages": messages,
            "tools": tools,
        });
        // Extended thinking. When enabled, max_tokens must exceed the budget and
        // temperature must be unset (the API requires temperature=1 with thinking).
        if let Some(budget) = opts.reasoning.thinking_budget() {
            body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            let floor = budget + 4096;
            if body["max_tokens"].as_u64().unwrap_or(0) < floor as u64 {
                body["max_tokens"] = json!(floor);
            }
        } else if let Some(t) = opts.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(s) = system {
            body["system"] = s;
        }
        body
    }

    async fn apply_auth(
        &self,
        req: reqwest::RequestBuilder,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(match &self.auth {
            AuthMode::ApiKey(key) => req.header("x-api-key", key),
            AuthMode::Oauth(source) => {
                let token = source.token().await?;
                req.header("authorization", format!("Bearer {}", token))
                    .header("anthropic-beta", OAUTH_BETA)
            }
        })
    }

    async fn request(&self, body: Value) -> anyhow::Result<reqwest::RequestBuilder> {
        let req = self
            .client
            .post(&self.base_url)
            .header("content-type", "application/json")
            .header("anthropic-version", API_VERSION);
        Ok(self.apply_auth(req).await?.json(&body))
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }
    fn model(&self) -> &str {
        &self.model
    }

    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        crate::providers::provider::generate_with_retry(self, opts, &|_| {}).await
    }

    async fn generate_once(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        let body = self.build_body(&opts, false);
        let res = tokio::time::timeout(self.timeouts.startup, async {
            crate::providers::provider::send_once(self.request(body).await?, "anthropic").await
        })
        .await
        .map_err(|_| anyhow::anyhow!("anthropic startup deadline exceeded"))??;
        let data: Value = tokio::time::timeout(
            self.timeouts.idle,
            crate::providers::provider::response_json(res),
        )
        .await
        .map_err(|_| anyhow::anyhow!("anthropic response body deadline exceeded"))??;
        Ok(Completion {
            message: from_api_message(&data),
            stop_reason: map_stop_reason(data["stop_reason"].as_str().unwrap_or("end_turn")),
            usage: Usage {
                input_tokens: data["usage"]["input_tokens"].as_u64().unwrap_or(0),
                output_tokens: data["usage"]["output_tokens"].as_u64().unwrap_or(0),
                cache_creation_input_tokens: data["usage"]["cache_creation_input_tokens"]
                    .as_u64()
                    .unwrap_or(0),
                cache_read_input_tokens: data["usage"]["cache_read_input_tokens"]
                    .as_u64()
                    .unwrap_or(0),
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
            let Some(res) =
                crate::providers::provider::start_stream(&tx, provider.timeouts.startup, async {
                    crate::providers::provider::send_once(
                        provider.request(body).await?,
                        "anthropic",
                    )
                    .await
                })
                .await
            else {
                return;
            };
            let mut blocks: Vec<Option<ContentBlock>> = Vec::new();
            let mut tool_json: Vec<String> = Vec::new();
            let mut stop_reason = None;
            let mut open_blocks = BTreeSet::new();
            let mut usage = Usage::default();
            let mut stopped = false;

            let consume = parse_sse_control(
                res,
                |event| {
                    let SseEvent::Json(data) = event else {
                        return ControlFlow::Break(());
                    };
                    let typ = data["type"].as_str().unwrap_or("");
                    match typ {
                        "message_start" => {
                            let u = &data["message"]["usage"];
                            usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                            usage.cache_creation_input_tokens =
                                u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                            usage.cache_read_input_tokens =
                                u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                        }
                        "content_block_start" => {
                            let idx = data["index"].as_u64().unwrap_or(0) as usize;
                            open_blocks.insert(idx);
                            ensure_len(&mut blocks, &mut tool_json, idx);
                            let b = &data["content_block"];
                            match b["type"].as_str() {
                                Some("text") => {
                                    blocks[idx] = Some(ContentBlock::Text {
                                        text: b["text"].as_str().unwrap_or("").to_string(),
                                    });
                                }
                                Some("tool_use") => {
                                    let id = b["id"].as_str().unwrap_or("").to_string();
                                    let name = b["name"].as_str().unwrap_or("").to_string();
                                    blocks[idx] = Some(ContentBlock::ToolUse {
                                        id: id.clone(),
                                        name: name.clone(),
                                        input: b["input"].clone(),
                                    });
                                    let _ = tx.send(StreamEvent::ToolUseStart { id, name });
                                }
                                Some("thinking") => {
                                    blocks[idx] = Some(ContentBlock::Thinking {
                                        thinking: b["thinking"].as_str().unwrap_or("").to_string(),
                                        signature: b["signature"]
                                            .as_str()
                                            .unwrap_or("")
                                            .to_string(),
                                    });
                                }
                                Some("redacted_thinking") => {
                                    blocks[idx] = Some(ContentBlock::RedactedThinking {
                                        data: b["data"].as_str().unwrap_or("").to_string(),
                                    });
                                }
                                _ => {}
                            }
                        }
                        "content_block_delta" => {
                            let idx = data["index"].as_u64().unwrap_or(0) as usize;
                            ensure_len(&mut blocks, &mut tool_json, idx);
                            let d = &data["delta"];
                            match d["type"].as_str() {
                                Some("text_delta") => {
                                    let t = d["text"].as_str().unwrap_or("").to_string();
                                    if let Some(Some(ContentBlock::Text { text })) =
                                        blocks.get_mut(idx)
                                    {
                                        text.push_str(&t);
                                    }
                                    let _ = tx.send(StreamEvent::TextDelta { text: t });
                                }
                                Some("input_json_delta") => {
                                    let pj = d["partial_json"].as_str().unwrap_or("").to_string();
                                    tool_json[idx].push_str(&pj);
                                    let id = match blocks.get(idx) {
                                        Some(Some(ContentBlock::ToolUse { id, .. })) => id.clone(),
                                        _ => String::new(),
                                    };
                                    let _ = tx.send(StreamEvent::ToolUseInputDelta {
                                        id,
                                        partial_json: pj,
                                    });
                                }
                                Some("thinking_delta") => {
                                    let t = d["thinking"].as_str().unwrap_or("");
                                    if let Some(Some(ContentBlock::Thinking { thinking, .. })) =
                                        blocks.get_mut(idx)
                                    {
                                        thinking.push_str(t);
                                    }
                                }
                                Some("signature_delta") => {
                                    let s = d["signature"].as_str().unwrap_or("");
                                    if let Some(Some(ContentBlock::Thinking {
                                        signature, ..
                                    })) = blocks.get_mut(idx)
                                    {
                                        signature.push_str(s);
                                    }
                                }
                                _ => {}
                            }
                        }
                        "content_block_stop" => {
                            if let Some(idx) = data["index"].as_u64() {
                                open_blocks.remove(&(idx as usize));
                            }
                        }
                        "message_delta" => {
                            if let Some(sr) = data["delta"]["stop_reason"].as_str() {
                                stop_reason = Some(sr.to_string());
                            }
                            let u = &data["usage"];
                            if let Some(tokens) = u["input_tokens"].as_u64() {
                                usage.input_tokens = tokens;
                            }
                            if let Some(tokens) = u["cache_creation_input_tokens"].as_u64() {
                                usage.cache_creation_input_tokens = tokens;
                            }
                            if let Some(tokens) = u["cache_read_input_tokens"].as_u64() {
                                usage.cache_read_input_tokens = tokens;
                            }
                            if let Some(ot) = u["output_tokens"].as_u64() {
                                usage.output_tokens = ot;
                            }
                        }
                        "message_stop" => {
                            stopped = true;
                            let completion = if !open_blocks.is_empty() {
                                Err(anyhow::anyhow!(
                                    "anthropic terminal event with unfinished content blocks"
                                ))
                            } else if let Some(reason) = &stop_reason {
                                finalize(&mut blocks, &tool_json, reason, usage)
                            } else {
                                Err(anyhow::anyhow!(
                                    "anthropic terminal event without stop_reason"
                                ))
                            };
                            let event = match completion {
                                Ok(completion) => StreamEvent::MessageStop { completion },
                                Err(error) => StreamEvent::Error {
                                    message: error.to_string(),
                                },
                            };
                            let _ = tx.send(event);
                        }
                        "error" => {
                            stopped = true;
                            let _ = tx.send(StreamEvent::Error {
                                message: format!("anthropic: {}", data["error"]),
                            });
                        }
                        _ => {}
                    }
                    if stopped {
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
            if !stopped {
                let message = match sse {
                    Err(error) => format!("anthropic stream ended: {error}"),
                    Ok(_) => "anthropic stream ended without message_stop".into(),
                };
                let _ = tx.send(StreamEvent::Error { message });
            }
        });

        Ok(rx)
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        let url = self.base_url.replace("/messages", "/models");
        let req = self
            .client
            .get(&url)
            .header("anthropic-version", API_VERSION);
        let res = self.apply_auth(req).await?.send().await?;
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
}

fn ensure_len(blocks: &mut Vec<Option<ContentBlock>>, tool_json: &mut Vec<String>, idx: usize) {
    while blocks.len() <= idx {
        blocks.push(None);
        tool_json.push(String::new());
    }
}

fn finalize(
    blocks: &mut [Option<ContentBlock>],
    tool_json: &[String],
    stop_reason: &str,
    usage: Usage,
) -> anyhow::Result<Completion> {
    for (i, b) in blocks.iter_mut().enumerate() {
        if let Some(ContentBlock::ToolUse { id, name, input }) = b {
            if stop_reason == "max_tokens" {
                *b = None;
                continue;
            }
            let raw = if tool_json[i].is_empty() {
                input.to_string()
            } else {
                tool_json[i].clone()
            };
            *input = crate::providers::codec::complete_tool_input(id, name, &raw)?;
        }
    }
    let content: Vec<ContentBlock> = blocks.iter().flatten().cloned().collect();
    Ok(Completion {
        message: Message {
            role: Role::Assistant,
            content,
        },
        stop_reason: map_stop_reason(stop_reason),
        usage,
    })
}

fn merge_into(target: &mut Value, extra: &Value) {
    if let (Some(t), Some(e)) = (target.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            t.insert(k.clone(), v.clone());
        }
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    crate::providers::codec::map_stop_reason(reason)
}

fn to_api_message(m: &Message) -> Value {
    let role = if m.role == Role::Tool {
        "user"
    } else {
        match m.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "user",
        }
    };
    let content: Vec<Value> = m
        .content
        .iter()
        .filter_map(|b| block_to_wire(b).into_value())
        .collect();
    json!({ "role": role, "content": content })
}

fn block_to_wire(b: &ContentBlock) -> BlockWire {
    match b {
        ContentBlock::Text { text } => BlockWire::Emit(json!({ "type": "text", "text": text })),
        ContentBlock::ToolUse { id, name, input } => {
            BlockWire::Emit(json!({ "type": "tool_use", "id": id, "name": name, "input": input }))
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => BlockWire::Emit(json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content.model_text(),
            "is_error": is_error,
        })),
        ContentBlock::Thinking {
            thinking,
            signature,
        } => BlockWire::Emit(json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": signature,
        })),
        ContentBlock::RedactedThinking { data } => {
            BlockWire::Emit(json!({ "type": "redacted_thinking", "data": data }))
        }
        // OpenAI Responses-specific; Anthropic has no representation — drop it.
        ContentBlock::ReasoningItem { .. } => BlockWire::Skip,
    }
}

fn from_api_message(data: &Value) -> Message {
    let content = data["content"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|b| match b["type"].as_str() {
                    Some("text") => ContentBlock::Text {
                        text: b["text"].as_str().unwrap_or("").to_string(),
                    },
                    Some("tool_use") => ContentBlock::ToolUse {
                        id: b["id"].as_str().unwrap_or("").to_string(),
                        name: b["name"].as_str().unwrap_or("").to_string(),
                        input: b["input"].clone(),
                    },
                    Some("thinking") => ContentBlock::Thinking {
                        thinking: b["thinking"].as_str().unwrap_or("").to_string(),
                        signature: b["signature"].as_str().unwrap_or("").to_string(),
                    },
                    Some("redacted_thinking") => ContentBlock::RedactedThinking {
                        data: b["data"].as_str().unwrap_or("").to_string(),
                    },
                    _ => ContentBlock::Text {
                        text: String::new(),
                    },
                })
                .collect()
        })
        .unwrap_or_default();
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
            let spec = workflow_spec();
            let provider = AnthropicProvider {
                auth: AuthMode::ApiKey("test".into()),
                model: "claude-sonnet-4-5-20250929".into(),
                base_url: API_URL.into(),
                client: reqwest::Client::new(),
                timeouts: StreamTimeouts::default(),
            };
            for stream in [false, true] {
                for cache in [false, true] {
                    let opts = GenerateOptions {
                        messages: vec![Message {
                            role: Role::User,
                            content: vec![ContentBlock::Text {
                                text: "Hello".into(),
                            }],
                        }],
                        tools: vec![spec.clone()],
                        cache,
                        ..Default::default()
                    };
                    let request = provider
                        .request(provider.build_body(&opts, stream))
                        .await
                        .unwrap()
                        .build()
                        .unwrap();
                    assert_eq!(request.url().as_str(), API_URL);
                    assert_eq!(request.headers()["x-api-key"], "test");
                    assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
                    assert!(request.headers().get("authorization").is_none());
                    let payload: Value =
                        serde_json::from_slice(request.body().unwrap().as_bytes().unwrap())
                            .unwrap();
                    let tool = &payload["tools"][0];
                    assert_workflow_tool(tool, "input_schema", &spec);
                    assert_eq!(tool.get("cache_control").is_some(), cache);
                }
            }
        }

        #[test]
        fn thinking_block_round_trips_through_api_message() {
            let msg = Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "let me reason".to_string(),
                        signature: "sig-abc".to_string(),
                    },
                    ContentBlock::ToolUse {
                        id: "tu_1".to_string(),
                        name: "read_file".to_string(),
                        input: json!({"path": "x"}),
                    },
                ],
            };
            let v = to_api_message(&msg);
            let blocks = v["content"].as_array().unwrap();
            assert_eq!(blocks[0]["type"], "thinking");
            assert_eq!(blocks[0]["thinking"], "let me reason");
            assert_eq!(blocks[0]["signature"], "sig-abc");
            assert_eq!(blocks[1]["type"], "tool_use");
        }

        #[test]
        fn reasoning_item_is_dropped_for_anthropic() {
            let msg = Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ReasoningItem {
                        item: json!({"type": "reasoning"}),
                    },
                    ContentBlock::Text {
                        text: "hi".to_string(),
                    },
                ],
            };
            let v = to_api_message(&msg);
            let blocks = v["content"].as_array().unwrap();
            assert_eq!(blocks.len(), 1);
            assert_eq!(blocks[0]["type"], "text");
        }

        #[test]
        fn every_content_block_has_an_explicit_wire_or_skip_expectation() {
            for block in all_content_blocks() {
                let expected = match &block {
                    ContentBlock::Text { .. } => Some(json!({"type":"text","text":"hi"})),
                    ContentBlock::ToolUse { .. } => Some(json!({
                        "type":"tool_use","id":"t1","name":"read_file","input":{"path":"a.rs"}
                    })),
                    ContentBlock::ToolResult { .. } => Some(json!({
                        "type":"tool_result","tool_use_id":"t1","content":"ok","is_error":false
                    })),
                    ContentBlock::Thinking { .. } => Some(json!({
                        "type":"thinking","thinking":"hmm","signature":"sig"
                    })),
                    ContentBlock::RedactedThinking { .. } => {
                        Some(json!({"type":"redacted_thinking","data":"xxx"}))
                    }
                    ContentBlock::ReasoningItem { .. } => None,
                };
                let expected_content: Vec<Value> = expected.into_iter().collect();
                let message = Message {
                    role: Role::Assistant,
                    content: vec![block],
                };
                assert_eq!(
                    to_api_message(&message),
                    json!({"role":"assistant","content":expected_content})
                );
            }
        }

        fn request_provider() -> AnthropicProvider {
            AnthropicProvider {
                auth: AuthMode::ApiKey("test".into()),
                model: "claude-sonnet-5".into(),
                base_url: API_URL.into(),
                client: reqwest::Client::new(),
                timeouts: StreamTimeouts::default(),
            }
        }

        #[test]
        fn thinking_budget_sets_a_floor_without_lowering_explicit_caps() {
            let provider = request_provider();
            assert_eq!(
                provider.build_body(&GenerateOptions::default(), false),
                json!({
                    "model":"claude-sonnet-5","max_tokens":64000,"stream":false,"messages":[],"tools":[]
                })
            );
            for (effort, budget, floor) in [
                (ReasoningEffort::Low, 4000, 8096),
                (ReasoningEffort::Medium, 10000, 14096),
                (ReasoningEffort::High, 24000, 28096),
                (ReasoningEffort::Max, 48000, 52096),
            ] {
                for (cap, expected) in [(1, floor), (floor, floor), (64000, 64000)] {
                    let body = provider.build_body(
                        &GenerateOptions {
                            reasoning: effort,
                            max_tokens: Some(cap),
                            temperature: Some(0.5),
                            ..Default::default()
                        },
                        true,
                    );
                    assert_eq!(
                        body["thinking"],
                        json!({"type":"enabled","budget_tokens":budget})
                    );
                    assert_eq!(body["max_tokens"], expected);
                    assert!(body.get("temperature").is_none());
                }
            }
            let body = provider.build_body(
                &GenerateOptions {
                    temperature: Some(0.5),
                    max_tokens: Some(17),
                    ..Default::default()
                },
                false,
            );
            assert_eq!(body["temperature"], 0.5);
            assert_eq!(body["max_tokens"], 17);
            assert!(body.get("thinking").is_none());
        }

        #[test]
        fn cache_marks_only_system_last_tool_and_penultimate_message_last_block() {
            let provider = request_provider();
            let mut options = GenerateOptions {
                system: Some("Be precise".into()),
                messages: vec![
                    Message::user_text("first"),
                    Message {
                        role: Role::Assistant,
                        content: vec![
                            ContentBlock::Text { text: "one".into() },
                            ContentBlock::Text { text: "two".into() },
                        ],
                    },
                    Message::user_text("latest"),
                ],
                tools: vec![
                    ToolSpec {
                        name: "first".into(),
                        description: "First".into(),
                        input_schema: json!({"type":"object"}),
                    },
                    ToolSpec {
                        name: "last".into(),
                        description: "Last".into(),
                        input_schema: json!({"type":"object"}),
                    },
                ],
                cache: true,
                ..Default::default()
            };
            let cached = provider.build_body(&options, true);
            assert_eq!(
                cached["system"],
                json!([{"type":"text","text":"Be precise","cache_control":{"type":"ephemeral"}}])
            );
            assert_eq!(
                cached["tools"],
                json!([
                    {"name":"first","description":"First","input_schema":{"type":"object"}},
                    {"name":"last","description":"Last","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}
                ])
            );
            assert_eq!(
                cached["messages"],
                json!([
                    {"role":"user","content":[{"type":"text","text":"first"}]},
                    {"role":"assistant","content":[{"type":"text","text":"one"},{"type":"text","text":"two","cache_control":{"type":"ephemeral"}}]},
                    {"role":"user","content":[{"type":"text","text":"latest"}]}
                ])
            );
            options.cache = false;
            let uncached = provider.build_body(&options, false);
            assert!(uncached["system"][0].get("cache_control").is_none());
            for tool in uncached["tools"].as_array().unwrap() {
                assert!(tool.get("cache_control").is_none());
            }
            for message in uncached["messages"].as_array().unwrap() {
                for block in message["content"].as_array().unwrap() {
                    assert!(block.get("cache_control").is_none());
                }
            }
        }

        #[tokio::test]
        async fn tool_outputs_remain_strings_in_serialized_requests() {
            let provider = request_provider();
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
                let content = &body["messages"][0]["content"][0]["content"];
                let decoded: ShellOutput = serde_json::from_str(content.as_str().unwrap()).unwrap();
                assert_eq!(decoded, shell);
                assert_eq!(
                    body["messages"],
                    json!([{"role":"user","content":[
                        {"type":"tool_result","tool_use_id":"shell-call","content":content,"is_error":true},
                        {"type":"tool_result","tool_use_id":"text-call","content":"plain \"text\"\n日本語","is_error":false}
                    ]}])
                );
            }
        }

        #[test]
        fn tool_results_use_user_role_and_keep_error_identity_and_unicode() {
            let message = Message {
                role: Role::Tool,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call-2".into(),
                        content: "日本語\nnot found".into(),
                        is_error: Some(true),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call-1".into(),
                        content: "😀".into(),
                        is_error: Some(false),
                    },
                ],
            };
            assert_eq!(
                to_api_message(&message),
                json!({"role":"user","content":[
                    {"type":"tool_result","tool_use_id":"call-2","content":"日本語\nnot found","is_error":true},
                    {"type":"tool_result","tool_use_id":"call-1","content":"😀","is_error":false}
                ]})
            );
        }

        #[test]
        fn response_decoding_preserves_all_supported_blocks_in_order() {
            let message = from_api_message(&json!({"content":[
                {"type":"thinking","thinking":"考える","signature":"opaque-signature"},
                {"type":"redacted_thinking","data":"opaque-data"},
                {"type":"text","text":"日本語 😀"},
                {"type":"tool_use","id":"call-1","name":"read_file","input":{"path":"日本語.rs"}}
            ]}));
            assert_eq!(
                serde_json::to_value(message).unwrap(),
                json!({"role":"assistant","content":[
                    {"type":"thinking","thinking":"考える","signature":"opaque-signature"},
                    {"type":"redacted_thinking","data":"opaque-data"},
                    {"type":"text","text":"日本語 😀"},
                    {"type":"tool_use","id":"call-1","name":"read_file","input":{"path":"日本語.rs"}}
                ]})
            );
        }
    }

    mod streams {
        use super::*;
        use crate::test_support::http::*;
        use std::sync::Arc;

        fn provider(base: String, timeouts: StreamTimeouts) -> Arc<dyn Provider> {
            Arc::new(AnthropicProvider {
                auth: AuthMode::ApiKey("test".into()),
                model: "test".into(),
                base_url: format!("{base}/v1/messages"),
                client: reqwest::Client::new(),
                timeouts,
            })
        }

        #[tokio::test]
        async fn terminal_eof_deadlines_and_cancellation() {
            lifecycle_checks(provider,
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
            ).await;
        }

        #[tokio::test]
        async fn explicit_error_and_unfinished_blocks_fail() {
            for body in [
                "data: {\"type\":\"error\",\"error\":{\"message\":\"overloaded\"}}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"memory\",\"input\":{}}}\n\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
            ] {
                let (base, server) = serve(body, true).await;
                assert_error(&collect(provider(base, deadlines())).await);
                server.await.unwrap();
            }
        }

        #[test]
        fn tools_require_complete_json_but_truncation_keeps_usage() {
            let mut blocks = vec![Some(ContentBlock::ToolUse {
                id: "t1".into(),
                name: "memory".into(),
                input: json!({}),
            })];
            assert!(finalize(
                &mut blocks,
                &["{\"action\":".into()],
                "tool_use",
                Usage::default()
            )
            .is_err());
            let completion = finalize(
                &mut blocks,
                &["{\"action\":".into()],
                "max_tokens",
                Usage {
                    output_tokens: 19,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(completion.stop_reason, StopReason::MaxTokens);
            assert!(completion.message.content.is_empty());
            assert_eq!(completion.usage.output_tokens, 19);
        }

        #[test]
        fn sparse_blocks_finalize_in_order_with_arguments_and_all_usage_counters() {
            let mut blocks = Vec::new();
            let mut arguments = Vec::new();
            ensure_len(&mut blocks, &mut arguments, 4);
            blocks[0] = Some(ContentBlock::Thinking {
                thinking: "考える".into(),
                signature: "sig".into(),
            });
            blocks[2] = Some(ContentBlock::Text {
                text: "日本語 😀".into(),
            });
            blocks[3] = Some(ContentBlock::ToolUse {
                id: "call-b".into(),
                name: "read_file".into(),
                input: json!({}),
            });
            blocks[4] = Some(ContentBlock::ToolUse {
                id: "call-a".into(),
                name: "list_agents".into(),
                input: json!({}),
            });
            arguments[3] = ["{\"path\":", "\"日本", "語.rs\"}"].concat();
            ensure_len(&mut blocks, &mut arguments, 2);
            let completion = finalize(
                &mut blocks,
                &arguments,
                "tool_use",
                Usage {
                    input_tokens: 11,
                    output_tokens: 7,
                    cache_creation_input_tokens: 13,
                    cache_read_input_tokens: 17,
                },
            )
            .unwrap();
            assert_eq!(completion.stop_reason, StopReason::ToolUse);
            assert_eq!(
                serde_json::to_value(&completion.message).unwrap(),
                json!({"role":"assistant","content":[
                    {"type":"thinking","thinking":"考える","signature":"sig"},
                    {"type":"text","text":"日本語 😀"},
                    {"type":"tool_use","id":"call-b","name":"read_file","input":{"path":"日本語.rs"}},
                    {"type":"tool_use","id":"call-a","name":"list_agents","input":{}}
                ]})
            );
            assert_eq!(
                serde_json::to_value(completion.usage).unwrap(),
                json!({
                    "input_tokens":11,"output_tokens":7,"cache_creation_input_tokens":13,"cache_read_input_tokens":17
                })
            );
        }

        #[test]
        fn truncation_discards_even_valid_calls_but_keeps_text_and_thinking() {
            let mut blocks = vec![
                Some(ContentBlock::Thinking {
                    thinking: "partial thought".into(),
                    signature: "sig".into(),
                }),
                Some(ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "list_agents".into(),
                    input: json!({}),
                }),
                Some(ContentBlock::Text {
                    text: "partial answer".into(),
                }),
                Some(ContentBlock::ToolUse {
                    id: "call-2".into(),
                    name: "read_file".into(),
                    input: json!({}),
                }),
            ];
            let completion = finalize(
                &mut blocks,
                &["".into(), "{}".into(), "".into(), "{".into()],
                "max_tokens",
                Usage::default(),
            )
            .unwrap();
            assert_eq!(completion.stop_reason, StopReason::MaxTokens);
            assert_eq!(
                serde_json::to_value(completion.message).unwrap(),
                json!({"role":"assistant","content":[
                    {"type":"thinking","thinking":"partial thought","signature":"sig"},
                    {"type":"text","text":"partial answer"}
                ]})
            );
        }

        #[test]
        fn finalization_rejects_missing_identity_and_non_object_inputs() {
            for (id, name, input, raw) in [
                ("", "read_file", json!({}), "{}"),
                ("call-1", "", json!({}), "{}"),
                ("call-1", "read_file", json!(null), ""),
                ("call-1", "read_file", json!({}), "[]"),
            ] {
                let mut blocks = vec![Some(ContentBlock::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input,
                })];
                assert!(
                    finalize(&mut blocks, &[raw.into()], "tool_use", Usage::default()).is_err()
                );
            }
        }
    }
}
