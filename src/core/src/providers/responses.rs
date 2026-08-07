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
    context_window: Option<usize>,
}

impl ResponsesProvider {
    pub fn with_auth(model: String, base_url: String, auth: Arc<dyn TokenSource>) -> Self {
        ResponsesProvider {
            auth,
            model,
            base_url: base_url.trim_end_matches('/').to_string(),
            client: super::openai::http_client(),
            context_window: None,
        }
    }

    /// Set an authoritative context window, overriding the id-based heuristic.
    pub fn with_context_window(mut self, window: Option<usize>) -> Self {
        self.context_window = window;
        self
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
        // Output cap: omit unless explicitly pinned, so the server enforces the
        // model's true max (only wind-down/compaction pin a small cap).
        if let Some(max) = opts.max_tokens {
            body["max_output_tokens"] = json!(max);
        }
        // Reasoning intensity (gpt-5.x / o-series). Off → omit the field.
        if let Some(effort) = opts.reasoning.as_str() {
            body["reasoning"] = json!({ "effort": effort });
            // With store:false the backend keeps no server-side state, so we must
            // fetch the encrypted reasoning items and echo them back on the next
            // request to preserve reasoning continuity across tool calls.
            body["include"] = json!(["reasoning.encrypted_content"]);
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
    fn context_window(&self) -> usize {
        self.context_window
            .unwrap_or_else(|| crate::providers::provider::context_window_for(&self.model))
    }

    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        // Reuse the streaming path and collect it.
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
        let res =
            crate::providers::provider::send_with_retry(self.request(body).await?, "responses")
                .await?;

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut text = String::new();
            // Tool calls accumulate by output index → (call_id, name, args).
            let mut tools: BTreeMap<i64, (String, String, String)> = BTreeMap::new();
            // Reasoning items captured verbatim by output index, to be echoed
            // back on the next request (reasoning continuity with store:false).
            let mut reasoning: BTreeMap<i64, Value> = BTreeMap::new();
            // The output_index the assistant text lives at, so it can be placed in
            // the right spot among reasoning/tool blocks when finalizing.
            let mut text_index: Option<i64> = None;
            let mut usage = Usage::default();
            // Whether a terminal event (completed / failed / error) was seen. If the
            // SSE stream ends WITHOUT one — dropped connection, truncated body, a
            // `response.incomplete`, or an unrecognized terminal type — we must still
            // send a MessageStop below, or the agent loop's receiver closes with no
            // completion and the turn hangs. (Anthropic guards the same case.)
            let mut terminal_seen = false;

            let _ = parse_sse(res, |evt| {
                let kind = evt["type"].as_str().unwrap_or("");
                match kind {
                    // Streamed assistant text.
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
                    // A finished output item — capture reasoning items verbatim
                    // (they carry the encrypted_content we must echo back).
                    "response.output_item.done" => {
                        let item = &evt["item"];
                        if item["type"] == "reasoning" {
                            let idx = evt["output_index"].as_i64().unwrap_or(0);
                            reasoning.insert(idx, item.clone());
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
                        terminal_seen = true;
                        let u = &evt["response"]["usage"];
                        usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(0);
                        usage.output_tokens = u["output_tokens"].as_u64().unwrap_or(0);
                        usage.cache_read_input_tokens = u["input_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .unwrap_or(0);

                        let mut content: Vec<ContentBlock> = Vec::new();
                        // Emit blocks in output_index order so each reasoning item
                        // immediately precedes the function_call it justifies (the
                        // backend requires this pairing when we echo them back with
                        // store:false). Merge reasoning + tool calls + the assistant
                        // text into one index-sorted sequence.
                        let mut ordered: Vec<(i64, ContentBlock)> = Vec::new();
                        for (idx, item) in &reasoning {
                            ordered
                                .push((*idx, ContentBlock::ReasoningItem { item: item.clone() }));
                        }
                        if !text.is_empty() {
                            ordered.push((
                                text_index.unwrap_or(i64::MAX),
                                ContentBlock::Text { text: text.clone() },
                            ));
                        }
                        for (idx, (id, name, args)) in &tools {
                            let input = crate::providers::codec::parse_tool_input(args);
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
                        terminal_seen = true;
                        // A real API failure — propagate it as an error event so the
                        // agent loop surfaces it (and can retry), rather than
                        // fabricating an assistant turn that poisons history.
                        let msg = evt["response"]["error"]["message"]
                            .as_str()
                            .or_else(|| evt["message"].as_str())
                            .unwrap_or("responses stream error");
                        let _ = tx.send(StreamEvent::Error {
                            message: format!("responses: {}", msg),
                        });
                    }
                    _ => {}
                }
            })
            .await;

            // Stream ended without any terminal event: assemble a best-effort
            // MessageStop from whatever we accumulated so the agent loop always
            // gets a completion and terminates the turn (instead of hanging on a
            // silently-closed channel). Mirrors anthropic.rs's stop fallback.
            if !terminal_seen {
                let mut ordered: Vec<(i64, ContentBlock)> = Vec::new();
                for (idx, item) in &reasoning {
                    ordered.push((*idx, ContentBlock::ReasoningItem { item: item.clone() }));
                }
                if !text.is_empty() {
                    ordered.push((
                        text_index.unwrap_or(i64::MAX),
                        ContentBlock::Text { text: text.clone() },
                    ));
                }
                for (idx, (id, name, args)) in &tools {
                    ordered.push((
                        *idx,
                        ContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: crate::providers::codec::parse_tool_input(args),
                        },
                    ));
                }
                ordered.sort_by_key(|(idx, _)| *idx);
                let content: Vec<ContentBlock> = ordered.into_iter().map(|(_, b)| b).collect();
                let stop = if tools.is_empty() {
                    StopReason::EndTurn
                } else {
                    StopReason::ToolUse
                };
                let _ = tx.send(StreamEvent::MessageStop {
                    completion: Completion {
                        message: Message {
                            role: Role::Assistant,
                            content,
                        },
                        stop_reason: stop,
                        usage,
                    },
                });
            }
        });

        Ok(rx)
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
                    // A user turn never carries these; nothing to emit. Exhaustive
                    // so a new variant can't silently vanish from the user path.
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
                    "output": content,
                })),
                // A Tool message only carries tool_results.
                ContentBlock::Text { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::ReasoningItem { .. } => None,
            })
            .collect(),
        Role::Assistant => {
            let mut items = Vec::new();
            // Reasoning items first — echo them back verbatim so the backend can
            // continue the reasoning chain across tool calls (store:false).
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
