//! The one trait every provider implements. Deliberately minimal: a blocking
//! `generate` and a streaming `stream`. Everything else is built on these two.

use crate::core::types::{Completion, GenerateOptions, StreamEvent};
use async_trait::async_trait;
use tokio::sync::mpsc;

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
}
