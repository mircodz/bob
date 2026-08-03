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
        }
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
            "max_tokens": opts.max_tokens.unwrap_or(4096),
            "messages": messages,
        });
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
                let input = if args.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(&args).unwrap_or(json!({}))
                };
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
    match reason {
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        _ => StopReason::EndTurn,
    }
}

/// One of our messages may become several OpenAI messages (tool results split).
fn to_api_messages(m: &Message) -> Vec<Value> {
    if m.role == Role::Tool {
        return m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => Some(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": content,
                })),
                _ => None,
            })
            .collect();
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

    let tool_calls: Vec<Value> = m
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some(json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": input.to_string() },
            })),
            _ => None,
        })
        .collect();

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
                input: if args.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str(args).unwrap_or(json!({}))
                },
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
}
