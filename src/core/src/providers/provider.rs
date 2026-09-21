use crate::core::types::{Completion, GenerateOptions, StreamEvent};
use anyhow::Context;
use async_trait::async_trait;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy)]
pub struct StreamTimeouts {
    pub startup: Duration,
    pub idle: Duration,
}

impl Default for StreamTimeouts {
    fn default() -> Self {
        Self {
            startup: Duration::from_secs(120),
            idle: Duration::from_secs(120),
        }
    }
}

pub(crate) async fn start_stream(
    tx: &mpsc::UnboundedSender<StreamEvent>,
    timeout: Duration,
    request: impl std::future::Future<Output = anyhow::Result<reqwest::Response>>,
) -> Option<reqwest::Response> {
    let result = tokio::select! {
        biased;
        _ = tx.closed() => return None,
        result = tokio::time::timeout(timeout, request) => result,
    };
    match result {
        Ok(Ok(response)) => Some(response),
        result => {
            let message = match result {
                Ok(Err(error)) => format!("{error:#}"),
                Err(_) => "provider startup deadline exceeded".to_string(),
                Ok(Ok(_)) => unreachable!(),
            };
            let _ = tx.send(StreamEvent::Error { message });
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub id: String,
    /// Context window (input-token budget) in tokens, if the provider knows it.
    pub context_window: Option<usize>,
}

pub(crate) const MAX_RETRIES: u32 = 10;
pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(10);

pub(crate) async fn send_once(
    req: reqwest::RequestBuilder,
    label: &str,
) -> anyhow::Result<reqwest::Response> {
    let res = req.send().await.with_context(|| label.to_owned())?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        anyhow::bail!("{label} {status}: {body}");
    }
    Ok(res)
}

pub(crate) async fn response_json(res: reqwest::Response) -> anyhow::Result<serde_json::Value> {
    let body = res.bytes().await?;
    serde_json::from_slice(&body).context("invalid response JSON")
}

pub(crate) async fn generate_with_retry(
    provider: &dyn Provider,
    opts: GenerateOptions,
    on_retry: &(dyn Fn(u32) + Sync),
) -> anyhow::Result<Completion> {
    let mut retries = 0;
    loop {
        match provider.generate_once(opts.clone()).await {
            Err(error) if retries < MAX_RETRIES && is_transient_error(&format!("{error:#}")) => {
                retries += 1;
                on_retry(retries);
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(error) if retries == MAX_RETRIES && is_transient_error(&format!("{error:#}")) => {
                return Err(retries_exhausted(error));
            }
            result => return result,
        }
    }
}

pub(crate) fn retries_exhausted(error: anyhow::Error) -> anyhow::Error {
    anyhow::anyhow!("provider request failed after {MAX_RETRIES} retries: {error:#}")
}

pub(crate) fn is_transient_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    if [
        "invalid api key",
        "invalid_api_key",
        "authentication",
        "unauthorized",
        "invalid credential",
        "invalid request",
        "invalid_request",
        "invalid schema",
        "invalid response json",
        "malformed sse json",
        "invalid sse utf-8",
        "incomplete or malformed arguments",
        "invalid tool",
        "protocol error",
        "certificate",
        "cert error",
        "tls error",
        "builder error",
        "relative url without a base",
    ]
    .iter()
    .any(|pattern| m.contains(pattern))
        || ["400", "401", "403", "404", "405", "422"]
            .iter()
            .any(|code| has_status_code(&m, code))
    {
        return false;
    }
    m.contains("rate limit")
        || m.contains("rate_limit")
        || m.contains("server_error")
        || m.contains("internal server error")
        || m.contains("service unavailable")
        || m.contains("service_unavailable")
        || m.contains("overloaded")
        || m.contains("timeout")
        || m.contains("timed out")
        || m.contains("deadline exceeded")
        || m.contains("stream ended")
        || m.contains("error sending request for url")
        || m.contains("error decoding response body")
        || m.contains("error reading a body from connection")
        || m.contains("dns error")
        || m.contains("unexpected eof")
        || ["408", "429", "500", "502", "503", "504", "529"]
            .iter()
            .any(|code| has_status_code(&m, code))
        || (m.contains("connection")
            && ["reset", "refused", "closed", "error", "aborted"]
                .iter()
                .any(|pattern| m.contains(pattern)))
}

/// Match HTTP status context, not token counts or parser column numbers.
pub(crate) fn has_status_code(hay: &str, code: &str) -> bool {
    let bytes = hay.as_bytes();
    let mut from = 0;
    while let Some(pos) = hay[from..].find(code) {
        let start = from + pos;
        let end = start + code.len();
        let left_ok = start == 0 || !bytes[start - 1].is_ascii_digit();
        let right_ok = end == bytes.len() || !bytes[end].is_ascii_digit();
        let prefix = hay[..start].trim_end_matches(|c: char| {
            c.is_ascii_whitespace() || matches!(c, ':' | '(' | '[' | '=')
        });
        let label = prefix.split_whitespace().next_back().unwrap_or("");
        let status_context = prefix.is_empty()
            || [
                "http",
                "status",
                "error",
                "openai",
                "anthropic",
                "responses",
            ]
            .iter()
            .any(|candidate| label.eq_ignore_ascii_case(candidate));
        if left_ok && right_ok && status_context {
            return true;
        }
        from = start + 1;
    }
    false
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion>;
    /// Single attempt for callers that own retry progress and cancellation.
    async fn generate_once(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
        self.generate(opts).await
    }
    /// Single streaming attempt; the consumer owns retries, including midstream failures.
    async fn stream(
        &self,
        opts: GenerateOptions,
    ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>>;

    async fn list_models(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }

    /// Context windows are input-token budgets; backend limits override heuristics.
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

    /// Maximum input tokens for the active model, used to size compaction.
    fn context_window(&self) -> usize {
        context_window_for(self.model())
    }
}

/// Estimated context window in tokens. Match specific families first;
/// unknown models use a conservative 128k budget.
pub fn context_window_for(model: &str) -> usize {
    let m = model.to_ascii_lowercase();
    // Explicit window markers override family defaults.
    if m.contains("-1m") || m.ends_with("1m") || m.contains("1m-") {
        return 1_000_000;
    }
    if m.contains("claude") {
        return 200_000;
    }
    if m.contains("gemini") {
        return 1_000_000;
    }
    if m.contains("gpt-5") || m.contains("o3") || m.contains("o4") {
        return 400_000;
    }
    if m.contains("gpt-4.1") {
        return 1_000_000;
    }
    if m.contains("gpt-4") {
        return 128_000;
    }
    128_000
}

/// Output-token ceiling for Anthropic's required `max_tokens` field.
/// Match specific families first; OpenAI may omit the cap.
pub fn max_output_tokens_for(model: &str) -> u32 {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        return 32_000;
    }
    if m.contains("sonnet") || m.contains("claude") {
        return 64_000;
    }
    16_000
}

#[cfg(test)]
mod tests {
    use super::*;

    mod policy {
        use super::*;

        #[test]
        fn windows_by_family() {
            assert_eq!(context_window_for("claude-opus-4.8"), 200_000);
            assert_eq!(context_window_for("claude-sonnet-5"), 200_000);
            assert_eq!(context_window_for("gemini-3.1-pro-preview"), 1_000_000);
            assert_eq!(context_window_for("gpt-5.4"), 400_000);
            assert_eq!(context_window_for("gpt-5.3-codex"), 400_000);
            assert_eq!(context_window_for("gpt-4o"), 128_000);
            assert_eq!(context_window_for("gpt-4.1"), 1_000_000);
            assert_eq!(context_window_for("some-new-llm"), 128_000);
        }

        #[test]
        fn explicit_1m_marker_overrides_family() {
            assert_eq!(context_window_for("claude-opus-1m"), 1_000_000);
            assert_eq!(context_window_for("claude-opus-4.8-1m"), 1_000_000);
        }

        #[test]
        fn anthropic_output_ceiling_by_family() {
            assert_eq!(max_output_tokens_for("claude-opus-4.8"), 32_000);
            assert_eq!(max_output_tokens_for("claude-sonnet-5"), 64_000);
            assert_eq!(max_output_tokens_for("claude-future"), 64_000);
            assert_eq!(max_output_tokens_for("something-else"), 16_000);
        }

        #[test]
        fn family_matching_is_case_insensitive_and_specific_first() {
            for (model, expected) in [
                ("vendor/CLAUDE-OPUS-4.8", 200_000),
                ("GPT-4.1-MINI", 1_000_000),
                ("O3-MINI", 400_000),
                ("o4-mini", 400_000),
                ("claude-sonnet-1m-preview", 1_000_000),
                ("", 128_000),
            ] {
                assert_eq!(context_window_for(model), expected, "{model}");
            }
            assert_eq!(max_output_tokens_for("CLAUDE-OPUS-4.8"), 32_000);
            assert_eq!(max_output_tokens_for("CLAUDE-SONNET-5"), 64_000);
            assert_eq!(max_output_tokens_for(""), 16_000);
        }
    }

    mod streams {
        use super::*;
        use crate::auth::StaticKey;
        use crate::providers::openai::OpenAiProvider;
        use crate::test_support::http::*;
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt;

        fn chat(base: String, timeouts: StreamTimeouts) -> Arc<dyn Provider> {
            Arc::new(
                OpenAiProvider::with_auth("test".into(), base, Arc::new(StaticKey("test".into())))
                    .with_stream_timeouts(timeouts),
            )
        }

        #[tokio::test]
        async fn startup_error_emits_one_error_without_a_completion() {
            let (tx, mut rx) = mpsc::unbounded_channel();
            assert!(start_stream(&tx, Duration::from_secs(1), async {
                anyhow::bail!("credential unavailable")
            })
            .await
            .is_none());
            assert!(
                matches!(rx.try_recv().unwrap(), StreamEvent::Error { message }
                if message == "credential unavailable")
            );
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }

        #[tokio::test]
        async fn already_closed_receiver_does_not_poll_request() {
            let (tx, rx) = mpsc::unbounded_channel();
            drop(rx);
            let polled = std::cell::Cell::new(false);
            let result = start_stream(&tx, Duration::from_secs(1), async {
                polled.set(true);
                anyhow::bail!("must not run")
            })
            .await;
            assert!(result.is_none());
            assert!(!polled.get());
        }

        #[tokio::test]
        async fn dropping_receiver_cancels_startup_future() {
            use std::sync::atomic::{AtomicBool, Ordering};
            struct Cancelled(Arc<AtomicBool>);
            impl Drop for Cancelled {
                fn drop(&mut self) {
                    self.0.store(true, Ordering::SeqCst);
                }
            }
            let entered = Arc::new(tokio::sync::Notify::new());
            let cancelled = Arc::new(AtomicBool::new(false));
            let (tx, rx) = mpsc::unbounded_channel();
            let task = {
                let entered = entered.clone();
                let cancelled = cancelled.clone();
                tokio::spawn(async move {
                    start_stream(&tx, Duration::from_secs(120), async move {
                        let _guard = Cancelled(cancelled);
                        entered.notify_one();
                        std::future::pending::<anyhow::Result<reqwest::Response>>().await
                    })
                    .await
                })
            };
            entered.notified().await;
            drop(rx);
            assert!(tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap()
                .is_none());
            assert!(cancelled.load(Ordering::SeqCst));
        }

        #[tokio::test]
        async fn streaming_leaves_http_retries_to_the_consumer() {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_request(&mut socket).await;
                socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            });
            assert_error(&collect(chat(base, deadlines())).await);
            tokio::time::timeout(Duration::from_secs(3), server)
                .await
                .unwrap()
                .unwrap();
        }
    }
}
