//! OpenAI-compatible provider (chat/completions). Works against OpenAI itself
//! and any compatible gateway — the GitHub Copilot proxy, native Copilot,
//! Ollama, vLLM, etc. Auth is pluggable: a static bearer key, or a dynamic
//! token source (used by native Copilot, which mints short-lived tokens).

use crate::core::types::*;
use crate::providers::provider::Provider;
use crate::providers::sse::parse_sse;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Supplies the bearer token + any extra headers for each request. Implementors
/// can refresh short-lived tokens transparently (native Copilot does this).
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// Return the current bearer token (refreshing if needed).
    async fn token(&self) -> anyhow::Result<String>;
    /// Extra headers required by this backend (e.g. Copilot editor headers).
    fn extra_headers(&self) -> Vec<(String, String)> {
        vec![]
    }
}

/// A fixed API key (OpenAI, proxies, local gateways).
struct StaticKey(String);

/// Shared HTTP client with sane timeouts. A connect timeout prevents hanging on
/// an unreachable endpoint; we deliberately do NOT set an overall read timeout,
/// since legitimate streamed generations can run for minutes.
/// Shared reqwest client builder for the OpenAI-family providers (Chat
/// Completions + Responses), with a bounded connect timeout.
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[async_trait]
impl TokenSource for StaticKey {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

pub struct OpenAiProvider {
    auth: Arc<dyn TokenSource>,
    model: String,
    base_url: String,
    client: reqwest::Client,
    /// Explicit context window (input-token budget) for the active model, when a
    /// caller knows it from an authoritative source (e.g. the Copilot /models
    /// limits). None → fall back to the id-based heuristic in `context_window_for`.
    context_window: Option<usize>,
}

impl OpenAiProvider {
    pub fn new(model: Option<String>, base_url: Option<String>) -> Self {
        let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "unused".to_string());
        let base_url = base_url
            .or_else(|| std::env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        let base_url = base_url.trim_end_matches('/').to_string();
        OpenAiProvider {
            auth: Arc::new(StaticKey(api_key)),
            model: model.unwrap_or_else(|| "gpt-4o".to_string()),
            base_url,
            client: http_client(),
            context_window: None,
        }
    }

    /// Build an OpenAI-compatible provider with a custom auth source + base_url
    /// (used by the native Copilot provider).
    pub fn with_auth(model: String, base_url: String, auth: Arc<dyn TokenSource>) -> Self {
        OpenAiProvider {
            auth,
            model,
            base_url: base_url.trim_end_matches('/').to_string(),
            client: http_client(),
            context_window: None,
        }
    }

    /// Set an authoritative context window (input-token budget), overriding the
    /// id-based heuristic. Used by the Copilot provider once it has fetched the
    /// model's real `max_prompt_tokens` limit.
    pub fn with_context_window(mut self, window: Option<usize>) -> Self {
        self.context_window = window;
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
        // Output cap: omit it unless the caller explicitly pins one, so the server
        // enforces the model's TRUE maximum. A hardcoded 4096 used to truncate large
        // tool-call arguments mid-stream (a workflow subagent's `structured_output`),
        // which the agent loop then retried forever. Only wind-down/compaction pin a
        // small cap, and those are honored here.
        if let Some(max) = opts.max_tokens {
            body["max_tokens"] = json!(max);
        }
        // Ask for usage in the streaming case (arrives in a final chunk).
        if stream {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(t) = opts.temperature {
            body["temperature"] = json!(t);
        }
        // Reasoning-capable chat models (o-series, gpt-5 family) accept
        // `reasoning_effort`. Sending it to a non-reasoning model (gpt-4o) is
        // rejected, so gate on a capability check. Off → omit.
        if chat_supports_reasoning(&self.model) {
            if let Some(effort) = opts.reasoning.as_str() {
                body["reasoning_effort"] = json!(effort);
                // These models don't accept an explicit temperature alongside
                // reasoning; drop it to avoid a 400.
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

    /// Apply auth + extra headers to a request builder.
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
        let body = self.build_body(&opts, false);
        let res = crate::providers::provider::send_with_retry(self.request(body).await?, "openai")
            .await?;
        let data: Value = res.json().await?;
        let choice = &data["choices"][0];
        let cached = data["usage"]["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        let prompt = data["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        Ok(Completion {
            message: from_api_message(&choice["message"]),
            stop_reason: map_finish(choice["finish_reason"].as_str().unwrap_or("stop")),
            usage: Usage {
                // OpenAI's prompt_tokens INCLUDES cached tokens; subtract them so
                // `input_tokens` means "fresh input" (the Anthropic convention) and
                // total_input() doesn't double-count the cache read.
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
        let res = crate::providers::provider::send_with_retry(self.request(body).await?, "openai")
            .await
            .map_err(|e| {
                // Newer models only work over the Responses API. Give an actionable
                // hint instead of the raw 4xx body.
                if e.to_string().contains("unsupported_api_for_model") {
                    anyhow::anyhow!(
                        "model '{}' needs the Responses API — log in with `bob login openai` \
                         to use it on a ChatGPT subscription, or pick a chat model (gpt-4o).",
                        self.model
                    )
                } else {
                    e
                }
            })?;

        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut text = String::new();
            let mut finish = "stop".to_string();
            let mut usage = Usage::default();
            // Tool calls arrive incrementally, keyed by index (ordered).
            let mut tool_acc: BTreeMap<i64, (String, String, String)> = BTreeMap::new();

            let _ = parse_sse(res, |evt| {
                // The final chunk carries usage with an empty choices array.
                if let Some(u) = evt.get("usage") {
                    if !u.is_null() {
                        let prompt = u["prompt_tokens"].as_u64().unwrap_or(usage.input_tokens);
                        let cached = u["prompt_tokens_details"]["cached_tokens"]
                            .as_u64()
                            .unwrap_or(usage.cache_read_input_tokens);
                        // prompt_tokens includes cached; store fresh input only so
                        // total_input() doesn't double-count (Anthropic convention).
                        usage.input_tokens = prompt.saturating_sub(cached);
                        usage.output_tokens = u["completion_tokens"]
                            .as_u64()
                            .unwrap_or(usage.output_tokens);
                        usage.cache_read_input_tokens = cached;
                    }
                }
                let choice = &evt["choices"][0];
                if choice.is_null() {
                    return;
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
                            let id = tc["id"]
                                .as_str()
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| format!("call_{}", idx));
                            let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                            if !name.is_empty() {
                                let _ = tx.send(StreamEvent::ToolUseStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                });
                            }
                            (id, name, String::new())
                        });
                        if entry.1.is_empty() {
                            if let Some(n) = tc["function"]["name"].as_str() {
                                entry.1 = n.to_string();
                            }
                        }
                        if let Some(args) = tc["function"]["arguments"].as_str() {
                            entry.2.push_str(args);
                            let _ = tx.send(StreamEvent::ToolUseInputDelta {
                                id: entry.0.clone(),
                                partial_json: args.to_string(),
                            });
                        }
                    }
                }
                if let Some(fr) = choice["finish_reason"].as_str() {
                    finish = fr.to_string();
                }
            })
            .await;

            let mut content: Vec<ContentBlock> = Vec::new();
            if !text.is_empty() {
                content.push(ContentBlock::Text { text });
            }
            for (_, (id, name, args)) in tool_acc {
                let input = crate::providers::codec::parse_tool_input(&args);
                content.push(ContentBlock::ToolUse { id, name, input });
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
                        // Prefer the backend's real input budget; some backends
                        // (OpenAI-proper) omit limits, so fall back to the
                        // id-based heuristic.
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
    // o1 / o3 / o4 reasoning models, and the gpt-5 family.
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

/// One of our messages may become several OpenAI messages (tool results split).
///
/// Every `ContentBlock` is classified by [`classify_block`] with an EXHAUSTIVE
/// match, so a block this format can't represent is skipped *on purpose* (with a
/// stated reason) rather than silently dropped by a `_ => None` — the bug this
/// replaced. A new `ContentBlock` variant won't compile until it's handled.
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
            // A tool_result on a non-Tool message, or a reasoning/thinking block:
            // chat/completions has no field for these, so they're intentionally
            // not sent. (Thinking is Anthropic-only; ReasoningItem is Responses-only.)
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

/// The chat/completions role a `ContentBlock` maps to. Exhaustive by construction:
/// the match in [`classify_block`] must cover every `ContentBlock` variant, so a
/// newly added variant is a compile error here — never a silent drop.
enum OpenAiBlock {
    Text(String),
    ToolCall(Value),
    ToolResult { tool_use_id: String, content: String },
    /// Deliberately unsupported by chat/completions (thinking / reasoning items).
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
            content: content.clone(),
        },
        // No chat/completions representation — thinking is Anthropic's, reasoning
        // items are the Responses API's. Omitted deliberately, not by accident.
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
    use super::chat_supports_reasoning;

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
    fn text_and_tool_use_survive_conversion() {
        use super::*;
        // The regression that motivated the codec seam: text + tool_use in one
        // assistant message must BOTH appear on the wire (previously two separate
        // filter_maps, now one exhaustive classify_block).
        let m = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "let me look".into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "a.rs"}),
                },
            ],
        };
        let out = to_api_messages(&m);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], json!("let me look"));
        assert_eq!(out[0]["tool_calls"][0]["function"]["name"], json!("read_file"));
    }

    #[test]
    fn tool_result_message_maps_to_tool_role() {
        use super::*;
        let m = Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "42".into(),
                is_error: Some(false),
            }],
        };
        let out = to_api_messages(&m);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], json!("tool"));
        assert_eq!(out[0]["tool_call_id"], json!("t1"));
        assert_eq!(out[0]["content"], json!("42"));
    }

    #[test]
    fn every_content_block_is_classified_not_dropped() {
        use super::*;
        use crate::providers::codec::conformance::all_content_blocks;
        // The anti-silent-drop guarantee: classify_block covers EVERY ContentBlock
        // variant. Unsupported ones (thinking/reasoning) are SkipUnsupported by
        // intent — never an accidental fall-through. If a new variant is added
        // without a classify_block arm, this file won't compile.
        for b in all_content_blocks() {
            let _ = classify_block(&b); // exhaustive match — compile-time proof
        }
    }
}
