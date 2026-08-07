//! The one trait every provider implements. Deliberately minimal: a blocking
//! `generate` and a streaming `stream`. Everything else is built on these two.

use crate::core::types::{Completion, GenerateOptions, StreamEvent};
use async_trait::async_trait;
use tokio::sync::mpsc;

/// One model offered by a provider, with its context window when known. Used by
/// the model picker so the user can see each model's window before switching.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    /// Context window (input-token budget) in tokens, if the provider knows it.
    pub context_window: Option<usize>,
}

/// Send an HTTP request with bounded exponential backoff on transient failures.
///
/// Retries on 429 (rate limit) and 5xx (server) responses, and on connection-level
/// errors (timeouts, resets). Honors a `Retry-After` header when present. A 4xx
/// other than 429 is a client error — returned immediately, never retried. The
/// returned `Response` has a success status; the caller still owns reading the body.
///
/// `req` must be cloneable (JSON bodies are); a streaming body can't be retried and
/// this will fail to clone — callers here all use JSON request bodies.
pub async fn send_with_retry(
    req: reqwest::RequestBuilder,
    label: &str,
) -> anyhow::Result<reqwest::Response> {
    const MAX_ATTEMPTS: u32 = 5;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let this = req
            .try_clone()
            .ok_or_else(|| anyhow::anyhow!("{label}: request body not cloneable for retry"))?;
        match this.send().await {
            Ok(res) => {
                let status = res.status();
                let retriable = status.as_u16() == 429 || status.is_server_error();
                if status.is_success() || !retriable || attempt >= MAX_ATTEMPTS {
                    if !status.is_success() {
                        let body = res.text().await.unwrap_or_default();
                        anyhow::bail!("{label} {status}: {body}");
                    }
                    return Ok(res);
                }
                let wait = retry_after(&res).unwrap_or_else(|| backoff(attempt));
                tokio::time::sleep(wait).await;
            }
            Err(e) => {
                // Connection-level error (timeout, reset). Retry unless exhausted.
                if attempt >= MAX_ATTEMPTS {
                    return Err(anyhow::anyhow!("{label}: {e}"));
                }
                tokio::time::sleep(backoff(attempt)).await;
            }
        }
    }
}

/// Exponential backoff with a fixed cap: ~0.5s, 1s, 2s, 4s (capped at 8s).
fn backoff(attempt: u32) -> std::time::Duration {
    let secs = (0.5 * 2f64.powi(attempt as i32 - 1)).min(8.0);
    std::time::Duration::from_secs_f64(secs)
}

/// Parse a `Retry-After` header: either delay-seconds or an HTTP date (we support
/// the integer-seconds form, which is what these APIs send).
fn retry_after(res: &reqwest::Response) -> Option<std::time::Duration> {
    let secs: u64 = res
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(std::time::Duration::from_secs(secs.min(60)))
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion>;
    /// Returns a channel of stream events. The provider spawns a task that
    /// drives the HTTP stream and sends events; the receiver is consumed by
    /// the agent loop.
    async fn stream(
        &self,
        opts: GenerateOptions,
    ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>>;

    /// List the model ids this provider offers (via its /models endpoint).
    /// Default: none — providers that can enumerate override this.
    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }

    /// Like [`list_models`], but each entry carries the model's context window
    /// (input-token budget) when the backend advertises it. The default derives
    /// the window from each id via [`context_window_for`]; a provider with an
    /// authoritative source (Copilot's /models limits) overrides this to report
    /// real numbers. Returns an empty vec when the provider can't enumerate.
    async fn list_models_detailed(&self) -> anyhow::Result<Vec<ModelEntry>> {
        Ok(self
            .list_models()
            .await?
            .into_iter()
            .map(|id| {
                let window = context_window_for(&id);
                ModelEntry {
                    id,
                    context_window: Some(window),
                }
            })
            .collect())
    }

    /// The context window (max input tokens) of this provider's active model.
    /// Used to size compaction and the graded context warnings. The default
    /// infers a sane value from the model id via [`context_window_for`]; a
    /// provider that knows better (e.g. from a /models response) can override.
    fn context_window(&self) -> usize {
        context_window_for(self.model())
    }
}

/// Best-effort context window (in tokens) for a model id, by substring match on
/// the family. Conservative: an unknown model gets a safe 128k default rather
/// than an optimistic guess that would overflow. Keep the arms ordered
/// most-specific first. This is the single place model→window knowledge lives.
pub fn context_window_for(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    // Explicit window marker in the id wins over the family default: an internal
    // long-context build like `claude-opus-1m` advertises its window in the name,
    // and would otherwise be capped at its family's standard window below.
    if m.contains("-1m") || m.ends_with("1m") || m.contains("1m-") {
        return 1_000_000;
    }
    // Anthropic Claude: 200k across current families (1M is opt-in/beta only).
    if m.contains("claude") {
        return 200_000;
    }
    // Google Gemini 1.5/2.x: 1M+ context.
    if m.contains("gemini") {
        return 1_000_000;
    }
    // OpenAI GPT-5 family (incl. Codex/Responses backend): 400k.
    if m.contains("gpt-5") || m.contains("o3") || m.contains("o4") {
        return 400_000;
    }
    // GPT-4.1 family: 1M. Older gpt-4o/4-turbo: 128k.
    if m.contains("gpt-4.1") {
        return 1_000_000;
    }
    if m.contains("gpt-4") {
        return 128_000;
    }
    // Safe default for anything unrecognized.
    128_000
}

/// The documented max OUTPUT-token ceiling for a model id, by family substring.
/// Unlike the OpenAI providers (which can OMIT the cap and let the server enforce
/// the true max), Anthropic REQUIRES `max_tokens` on every request — so we must
/// send a concrete number. These are the real per-family ceilings (not a
/// conservative guess); anything the model wants beyond this is caught by the
/// agent loop's truncation recovery. Keep arms most-specific first.
pub fn max_output_tokens_for(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    // Claude Sonnet 3.7/4.x support up to 64k output; Opus 4.x up to 32k.
    if m.contains("opus") {
        return 32_000;
    }
    if m.contains("sonnet") || m.contains("claude") {
        return 64_000;
    }
    // Conservative fallback for a non-Claude model routed here.
    16_000
}

#[cfg(test)]
mod tests {
    use super::context_window_for;

    #[test]
    fn windows_by_family() {
        assert_eq!(context_window_for("claude-opus-4.8"), 200_000);
        assert_eq!(context_window_for("claude-sonnet-5"), 200_000);
        assert_eq!(context_window_for("gemini-3.1-pro-preview"), 1_000_000);
        assert_eq!(context_window_for("gpt-5.4"), 400_000);
        assert_eq!(context_window_for("gpt-5.3-codex"), 400_000);
        assert_eq!(context_window_for("gpt-4o"), 128_000);
        assert_eq!(context_window_for("gpt-4.1"), 1_000_000);
        // Unknown model gets the safe default, not an optimistic guess.
        assert_eq!(context_window_for("some-new-llm"), 128_000);
    }

    #[test]
    fn explicit_1m_marker_overrides_family() {
        // An internal long-context Claude build advertises its window in the id;
        // it must not be capped at the family's standard 200k.
        assert_eq!(context_window_for("claude-opus-1m"), 1_000_000);
        assert_eq!(context_window_for("claude-opus-4.8-1m"), 1_000_000);
    }

    #[test]
    fn anthropic_output_ceiling_by_family() {
        use super::max_output_tokens_for;
        // Opus caps lower than Sonnet; both must exceed the old 4096 that truncated
        // large tool-call arguments.
        assert_eq!(max_output_tokens_for("claude-opus-4.8"), 32_000);
        assert_eq!(max_output_tokens_for("claude-sonnet-5"), 64_000);
        // Bare "claude" (unknown sub-family) still gets a generous ceiling.
        assert_eq!(max_output_tokens_for("claude-future"), 64_000);
        assert!(max_output_tokens_for("something-else") >= 16_000);
    }
}
