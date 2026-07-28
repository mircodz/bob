//! Anthropic provider built on plain reqwest — no SDK. Translates our
//! provider-agnostic types to/from the Messages API, with prompt caching.

use crate::core::types::*;
use crate::providers::provider::Provider;
use crate::providers::sse::parse_sse;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::mpsc;

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";

/// How this provider authenticates.
enum AuthMode {
    /// Console API key (x-api-key header) — pay per token.
    ApiKey(String),
    /// Claude Pro/Max subscription OAuth (Bearer + oauth beta header).
    Oauth,
}

pub struct AnthropicProvider {
    auth: AuthMode,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(model: Option<String>) -> anyhow::Result<Self> {
        // Prefer subscription OAuth if the user logged in; else use an API key.
        if crate::auth::anthropic::is_logged_in() {
            return Ok(AnthropicProvider {
                auth: AuthMode::Oauth,
                model: model.unwrap_or_else(|| "claude-sonnet-4-5-20250929".to_string()),
                base_url: API_URL.to_string(),
                client: reqwest::Client::new(),
            });
        }
        let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            anyhow::bail!(
                "no Anthropic auth — run `bob login anthropic` or set ANTHROPIC_API_KEY"
            );
        }
        Ok(AnthropicProvider {
            auth: AuthMode::ApiKey(api_key),
            model: model.unwrap_or_else(|| "claude-sonnet-4-5-20250929".to_string()),
            base_url: API_URL.to_string(),
            client: reqwest::Client::new(),
        })
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
            if let Some(blocks) = messages[idx].get_mut("content").and_then(|c| c.as_array_mut()) {
                if let Some(last) = blocks.last_mut() {
                    merge_into(last, &cc);
                }
            }
        }

        let mut body = json!({
            "model": self.model,
            "max_tokens": opts.max_tokens.unwrap_or(4096),
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

    async fn request(&self, body: Value) -> anyhow::Result<reqwest::RequestBuilder> {
        let mut req = self
            .client
            .post(&self.base_url)
            .header("content-type", "application/json")
            .header("anthropic-version", API_VERSION);
        req = match &self.auth {
            AuthMode::ApiKey(key) => req.header("x-api-key", key),
            AuthMode::Oauth => {
                let token = crate::auth::anthropic::access_token().await?;
                req.header("authorization", format!("Bearer {}", token))
                    .header("anthropic-beta", OAUTH_BETA)
            }
        };
        Ok(req.json(&body))
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
        let body = self.build_body(&opts, false);
        let res = self.request(body).await?.send().await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("anthropic {}: {}", status, res.text().await.unwrap_or_default());
        }
        let data: Value = res.json().await?;
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

    async fn stream(&self, opts: GenerateOptions) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
        let body = self.build_body(&opts, true);
        let res = self.request(body).await?.send().await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("anthropic {}: {}", status, res.text().await.unwrap_or_default());
        }

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut blocks: Vec<Option<ContentBlock>> = Vec::new();
            let mut tool_json: Vec<String> = Vec::new();
            let mut stop_reason = "end_turn".to_string();
            let mut usage = Usage::default();

            let _ = parse_sse(res, |data| {
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
                        ensure_len(&mut blocks, &mut tool_json, idx);
                        let b = &data["content_block"];
                        match b["type"].as_str() {
                            Some("text") => {
                                blocks[idx] = Some(ContentBlock::Text { text: String::new() });
                            }
                            Some("tool_use") => {
                                let id = b["id"].as_str().unwrap_or("").to_string();
                                let name = b["name"].as_str().unwrap_or("").to_string();
                                blocks[idx] = Some(ContentBlock::ToolUse {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: Value::Null,
                                });
                                let _ = tx.send(StreamEvent::ToolUseStart { id, name });
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
                                if let Some(Some(ContentBlock::Text { text })) = blocks.get_mut(idx) {
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
                            _ => {}
                        }
                    }
                    "message_delta" => {
                        if let Some(sr) = data["delta"]["stop_reason"].as_str() {
                            stop_reason = sr.to_string();
                        }
                        if let Some(ot) = data["usage"]["output_tokens"].as_u64() {
                            usage.output_tokens = ot;
                        }
                    }
                    "message_stop" => {
                        // Finalize: parse accumulated tool_use JSON.
                        for (i, b) in blocks.iter_mut().enumerate() {
                            if let Some(ContentBlock::ToolUse { input, .. }) = b {
                                let raw = &tool_json[i];
                                *input = if raw.is_empty() {
                                    json!({})
                                } else {
                                    serde_json::from_str(raw).unwrap_or(json!({}))
                                };
                            }
                        }
                        let content: Vec<ContentBlock> = blocks.iter().flatten().cloned().collect();
                        let completion = Completion {
                            message: Message {
                                role: Role::Assistant,
                                content,
                            },
                            stop_reason: map_stop_reason(&stop_reason),
                            usage,
                        };
                        let _ = tx.send(StreamEvent::MessageStop { completion });
                    }
                    _ => {}
                }
            })
            .await;
        });

        Ok(rx)
    }

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        // base_url points at .../v1/messages; the models endpoint is .../v1/models.
        let url = self.base_url.replace("/messages", "/models");
        let mut req = self.client.get(&url).header("anthropic-version", API_VERSION);
        req = match &self.auth {
            AuthMode::ApiKey(key) => req.header("x-api-key", key),
            AuthMode::Oauth => {
                let token = crate::auth::anthropic::access_token().await?;
                req.header("authorization", format!("Bearer {}", token))
                    .header("anthropic-beta", OAUTH_BETA)
            }
        };
        let res = req.send().await?;
        if !res.status().is_success() {
            anyhow::bail!("models {}: {}", res.status(), res.text().await.unwrap_or_default());
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

fn merge_into(target: &mut Value, extra: &Value) {
    if let (Some(t), Some(e)) = (target.as_object_mut(), extra.as_object()) {
        for (k, v) in e {
            t.insert(k.clone(), v.clone());
        }
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

fn to_api_message(m: &Message) -> Value {
    let role = if m.role == Role::Tool { "user" } else {
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
        .map(|b| match b {
            ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
            ContentBlock::ToolUse { id, name, input } => {
                json!({ "type": "tool_use", "id": id, "name": name, "input": input })
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": content,
                "is_error": is_error,
            }),
        })
        .collect();
    json!({ "role": role, "content": content })
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
                    _ => ContentBlock::Text { text: String::new() },
                })
                .collect()
        })
        .unwrap_or_default();
    Message {
        role: Role::Assistant,
        content,
    }
}
