//! Native GitHub Copilot provider: an OpenAI-compatible client against the
//! Copilot API, authenticated with a short-lived Copilot token minted from the
//! stored GitHub OAuth token (and refreshed transparently). The API base is
//! discovered from the token response's `endpoints.api` — Enterprise accounts
//! use api.enterprise.githubcopilot.com, individuals api.githubcopilot.com.

use crate::auth::copilot as auth;
use crate::providers::openai::{OpenAiProvider, TokenSource};
use crate::providers::provider::Provider;
use crate::providers::responses::ResponsesProvider;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Client-identity headers so the provider is trackable as this project.
const CLIENT_ID: &str = "mircodz/bob";
const CLIENT_VERSION: &str = concat!("mircodz/bob/", env!("CARGO_PKG_VERSION"));

/// Caches the short-lived Copilot API token + discovered API base, refreshing
/// before expiry.
struct CopilotAuth {
    github_token: String,
    cached: Mutex<Option<(String, u64, String)>>, // (token, expires_at, api_base)
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

/// True if a Copilot model must be called via the Responses API rather than
/// /chat/completions. The newer GPT-5.x families (sol/luna/terra/codex) and
/// gpt-5.5 are Responses-only; classic chat models (gpt-4o, claude-*, gemini-*)
/// use /chat/completions.
fn is_responses_model(model: &str) -> bool {
    // gpt-5.4 and up on the Copilot backend speak Responses only.
    let m = model.to_ascii_lowercase();
    m.contains("-sol")
        || m.contains("-luna")
        || m.contains("-terra")
        || m.starts_with("gpt-5.5")
        || m.starts_with("gpt-5.6")
        || m.starts_with("gpt-5.4")
        || m.contains("gpt-5.3-codex")
        || m == "gpt-5-codex"
}

/// Registry constructor for the "copilot" provider — uniform
/// `async fn create(model) -> Result<Arc<dyn Provider>>` shape shared by every
/// provider family (delegates to [`native_copilot`]).
pub async fn create(model: Option<String>) -> anyhow::Result<Arc<dyn Provider>> {
    native_copilot(model).await
}

/// Build the native Copilot provider, or an error if not logged in. Discovers
/// the correct API base by minting a token up front, and routes newer models to
/// the Responses API (`/responses`) while classic models use /chat/completions.
pub async fn native_copilot(model: Option<String>) -> anyhow::Result<Arc<dyn Provider>> {
    let github_token = auth::github_token()
        .ok_or_else(|| anyhow::anyhow!("not logged in to Copilot — run `bob login copilot`"))?;
    let source = CopilotAuth {
        github_token,
        cached: Mutex::new(None),
    };
    // Prime the cache + discover the endpoint.
    let (tok, api_base) = source.ensure().await?;
    let model = model.unwrap_or_else(|| "gpt-4o".to_string());

    // Ask the backend for this model's real limits so compaction sizes against the
    // true input budget (Copilot advertises up to ~936k for Claude, not the 200k
    // the id-based heuristic assumes). Best-effort: None → heuristic fallback.
    let window = auth::fetch_model_limits(&tok, &api_base)
        .await
        .into_iter()
        .find(|m| m.id == model)
        .and_then(|m| m.max_prompt_tokens.or(m.max_context_window_tokens));

    let source: Arc<dyn TokenSource> = Arc::new(source);

    if is_responses_model(&model) {
        Ok(Arc::new(
            ResponsesProvider::with_auth(model, api_base, source).with_context_window(window),
        ))
    } else {
        Ok(Arc::new(
            OpenAiProvider::with_auth(model, api_base, source).with_context_window(window),
        ))
    }
}
