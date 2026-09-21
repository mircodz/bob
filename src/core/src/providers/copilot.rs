//! Copilot tokens discover the API base through `endpoints.api`, including
//! Enterprise-specific endpoints.

use crate::auth::copilot as auth;
use crate::providers::openai::{OpenAiProvider, TokenSource};
use crate::providers::provider::{ModelEntry, Provider, StreamTimeouts};
use crate::providers::responses::ResponsesProvider;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

const CLIENT_ID: &str = "mircodz/bob";
const CLIENT_VERSION: &str = concat!("mircodz/bob/", env!("CARGO_PKG_VERSION"));

struct CopilotAuth {
    github_token: String,
    cached: Mutex<Option<(String, u64, String)>>, // (token, Unix expiry seconds, api_base)
}

impl CopilotAuth {
    async fn ensure(&self) -> anyhow::Result<(String, String)> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        {
            let cached = self.cached.lock().await;
            if let Some((tok, exp, base)) = cached.as_ref() {
                if *exp > now + 60 {
                    return Ok((tok.clone(), base.clone()));
                }
            }
        }
        let (tok, exp, base) = auth::fetch_copilot_token(&self.github_token).await?;
        *self.cached.lock().await = Some((tok.clone(), exp, base.clone()));
        Ok((tok, base))
    }
}

#[async_trait]
impl TokenSource for CopilotAuth {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.ensure().await?.0)
    }

    fn extra_headers(&self) -> Vec<(String, String)> {
        vec![
            ("editor-version".to_string(), CLIENT_VERSION.to_string()),
            (
                "editor-plugin-version".to_string(),
                CLIENT_VERSION.to_string(),
            ),
            (
                "copilot-integration-id".to_string(),
                "vscode-chat".to_string(),
            ),
            ("user-agent".to_string(), CLIENT_VERSION.to_string()),
            ("x-client".to_string(), CLIENT_ID.to_string()),
        ]
    }
}

/// Copilot's newer GPT families require `/responses`, not `/chat/completions`.
fn is_responses_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("-sol")
        || m.contains("-luna")
        || m.contains("-terra")
        || m.starts_with("gpt-5.5")
        || m.starts_with("gpt-5.6")
        || m.starts_with("gpt-5.4")
        || m.starts_with("gpt-6")
        || m.contains("gpt-5.3-codex")
        || m == "gpt-5-codex"
}

/// Copilot requires device-flow login; explicit API keys are rejected.
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
    if auth.is_some() {
        anyhow::bail!(
            "the copilot provider authenticates via `bob login copilot`; it does not accept a \
             programmatic API key"
        );
    }
    let github_token = auth::github_token()
        .ok_or_else(|| anyhow::anyhow!("not logged in to Copilot — run `bob login copilot`"))?;
    let source = CopilotAuth {
        github_token,
        cached: Mutex::new(None),
    };
    let (tok, api_base) = source.ensure().await?;
    let model = model.unwrap_or_else(|| "gpt-4o".to_string());

    // Backend input-token limits take precedence over model-name heuristics.
    let model_limits = auth::fetch_model_limits(&tok, &api_base).await;
    let window = model_limits
        .iter()
        .find(|m| m.id == model)
        .and_then(|m| m.max_prompt_tokens.or(m.max_context_window_tokens));

    let source: Arc<dyn TokenSource> = Arc::new(source);

    if is_responses_model(&model) {
        let model_catalog = model_limits
            .into_iter()
            .map(|entry| ModelEntry {
                id: entry.id,
                context_window: entry.max_prompt_tokens.or(entry.max_context_window_tokens),
            })
            .collect();
        Ok(Arc::new(
            ResponsesProvider::with_auth(model, api_base, source)
                .with_stream_timeouts(timeouts)
                .with_context_window(window)
                .with_model_catalog(model_catalog),
        ))
    } else {
        Ok(Arc::new(
            OpenAiProvider::with_auth(model, api_base, source)
                .with_stream_timeouts(timeouts)
                .with_context_window(window),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt_6_uses_responses_api() {
        assert!(is_responses_model("gpt-6-astra"));
        assert!(!is_responses_model("gpt-4o"));
    }
}
