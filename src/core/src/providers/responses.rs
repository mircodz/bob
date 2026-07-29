//! OpenAI **Responses API** provider (`/responses`), for the newer models that
//! don't accept `/chat/completions` (gpt-5.x, codex, sol, luna, …). It speaks a
//! different request shape (`input` items + `instructions`) and a different SSE
//! event stream than chat-completions, but presents the same `Provider` trait so
//! the agent loop is unchanged.
//!
//! Auth is pluggable (same `TokenSource` as the chat provider) so it works with
//! the ChatGPT-subscription OAuth token against chatgpt.com/backend-api/codex.

use crate::core::types::*;
use crate::providers::openai::TokenSource;
use crate::providers::provider::Provider;
use crate::providers::sse::parse_sse;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct ResponsesProvider {
    auth: Arc<dyn TokenSource>,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

impl ResponsesProvider {
    pub fn with_auth(model: String, base_url: String, auth: Arc<dyn TokenSource>) -> Self {
        ResponsesProvider {
            auth,
            model,
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(20))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }

    /// Translate our provider-agnostic messages into Responses `input` items,
    /// pulling the system prompt out as `instructions`.
    fn build_body(&self, opts: &GenerateOptions) -> Value {
        let mut input: Vec<Value> = Vec::new();
        for m in &opts.messages {
            input.extend(to_input_items(m));
        }

        let mut body = json!({
            "model": self.model,
            "input": input,
            "stream": true,
            // Subscription/codex backend is stateless per request for our use.
            "store": false,
        });
        if let Some(sys) = &opts.system {
            body["instructions"] = json!(sys);
        }
        // Reasoning intensity (gpt-5.x / o-series). Off → omit the field.
        if let Some(effort) = opts.reasoning.as_str() {
            body["reasoning"] = json!({ "effort": effort });
        }
        if !opts.tools.is_empty() {
            // Responses API tool shape: flat {type:function, name, description, parameters}.
            body["tools"] = json!(opts
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
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

    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        // Reuse the streaming path and collect it.
        let mut rx = self.stream(opts).await?;
        let mut completion = None;
        while let Some(evt) = rx.recv().await {
            if let StreamEvent::MessageStop { completion: c } = evt {
                completion = Some(c);
            }
        }
        completion.ok_or_else(|| anyhow::anyhow!("responses stream ended without completion"))
    }

    async fn stream(
        &self,
        opts: GenerateOptions,
    ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
        let body = self.build_body(&opts);
        let res = self.request(body).await?.send().await?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!(
                "responses {}: {}",
                status,
                res.text().await.unwrap_or_default()
            );
        }

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut text = String::new();
            // Tool calls accumulate by output index → (call_id, name, args).
            let mut tools: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
            let mut usage = Usage::default();

            let _ = parse_sse(res, |evt| {
                let kind = evt["type"].as_str().unwrap_or("");
                match kind {
                    // Streamed assistant text.
                    "response.output_text.delta" => {
                        if let Some(d) = evt["delta"].as_str() {
                            text.push_str(d);
                            let _ = tx.send(StreamEvent::TextDelta {
                                text: d.to_string(),
                            });
                        }
                    }
                    // A new output item — capture function-call metadata.
                    "response.output_item.added" => {
                        let item = &evt["item"];
                        if item["type"] == "function_call" {
                            let idx = evt["output_index"].as_i64().unwrap_or(0);
                            let call_id = item["call_id"]
                                .as_str()
                                .or_else(|| item["id"].as_str())
                                .unwrap_or("")
                                .to_string();
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
                    // Streamed function-call argument JSON.
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
                    // Terminal event with final output + usage.
                    "response.completed" => {
                        let u = &evt["response"]["usage"];
                        usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                        usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                        usage.cache_read_input_tokens = u["input_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .unwrap_or(0);

                        let mut content: Vec<ContentBlock> = Vec::new();
                        if !text.is_empty() {
                            content.push(ContentBlock::Text { text: text.clone() });
                        }
                        for (id, name, args) in tools.values() {
                            let input = if args.is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(args).unwrap_or(json!({}))
                            };
                            content.push(ContentBlock::ToolUse {
                                id: id.clone(),
                                name: name.clone(),
                                input,
                            });
                        }
                        let stop = if tools.is_empty() {
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
                        // Surface an empty completion so the loop unwinds.
                        let msg = evt["response"]["error"]["message"]
                            .as_str()
                            .or_else(|| evt["message"].as_str())
                            .unwrap_or("responses stream error");
                        let completion = Completion {
                            message: Message {
                                role: Role::Assistant,
                                content: vec![ContentBlock::Text {
                                    text: format!("error: {}", msg),
                                }],
                            },
                            stop_reason: StopReason::EndTurn,
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
        // The responses backend doesn't expose /models the same way; return the
        // known set (see providers::mod::RESPONSES_MODELS).
        Ok(vec![])
    }
}

/// Translate one of our messages into Responses API `input` items.
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
                        // Tool results are their own top-level items.
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        }));
                    }
                    _ => {}
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
                    "output": content,
                })),
                _ => None,
            })
            .collect(),
        Role::Assistant => {
            let mut items = Vec::new();
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
