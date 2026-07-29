pub mod anthropic;
pub mod copilot;
pub mod openai;
pub mod provider;
pub mod responses;
pub mod sse;

use crate::providers::anthropic::AnthropicProvider;
use crate::providers::provider::Provider;
use std::sync::Arc;

/// Registry mapping a provider id to a constructor. spec form:
/// "anthropic" or "anthropic:claude-sonnet-4-5".
pub async fn create_provider(spec: &str) -> anyhow::Result<Arc<dyn Provider>> {
    let mut parts = spec.splitn(2, ':');
    let id = parts.next().unwrap_or("");
    let model = parts
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    match id {
        "anthropic" => Ok(Arc::new(AnthropicProvider::new(model)?)),
        "openai" => openai_provider(model),
        "copilot" => {
            // Native GitHub Copilot, authenticated via the device-code login.
            // Requires `bob login copilot` first. Discovers the API endpoint and
            // routes newer models to the Responses API.
            copilot::native_copilot(model).await
        }
        _ => anyhow::bail!(
            "unknown provider \"{}\". known: anthropic, openai, copilot",
            id
        ),
    }
}

/// The ChatGPT-subscription (Codex) OAuth token source.
struct OpenAiOauth;
#[async_trait::async_trait]
impl crate::providers::openai::TokenSource for OpenAiOauth {
    async fn token(&self) -> anyhow::Result<String> {
        crate::auth::openai::access_token().await
    }
}

/// Build the OpenAI provider:
///   - Logged in with ChatGPT (OAuth) → the **Responses API** against the Codex
///     backend (chatgpt.com/backend-api/codex). This backend only speaks
///     /responses, so ALL models on this path use it — the user doesn't have to
///     know which models are "new".
///   - Otherwise → the classic /chat/completions provider with an API key.
fn openai_provider(model: Option<String>) -> anyhow::Result<Arc<dyn Provider>> {
    use crate::providers::openai::OpenAiProvider;
    use crate::providers::responses::ResponsesProvider;

    if crate::auth::openai::is_logged_in() {
        let source: Arc<dyn crate::providers::openai::TokenSource> = Arc::new(OpenAiOauth);
        Ok(Arc::new(ResponsesProvider::with_auth(
            model.unwrap_or_else(|| "gpt-5".to_string()),
            "https://chatgpt.com/backend-api/codex".to_string(),
            source,
        )))
    } else {
        Ok(Arc::new(OpenAiProvider::new(model, None)))
    }
}

/// Known model ids offered through the ChatGPT-subscription (Codex) backend.
/// Used to populate the /models picker since that backend doesn't list them.
/// Verified against the live endpoint: only these accept requests on a ChatGPT
/// account (gpt-5 / gpt-5-codex etc. return "not supported").
pub const RESPONSES_MODELS: &[&str] = &["gpt-5.6-luna", "gpt-5.6-sol"];
