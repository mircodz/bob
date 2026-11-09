//! DeepSeek's OpenAI-compatible Chat Completions API. DeepSeek has no OAuth login,
//! only API keys from https://platform.deepseek.com/api_keys.

use crate::auth::{ProviderAuth, StaticKey};
use crate::providers::openai::OpenAiProvider;
use crate::providers::provider::{Provider, StreamTimeouts};
use std::sync::Arc;

const BASE_URL: &str = "https://api.deepseek.com";
const DEFAULT_MODEL: &str = "deepseek-flash";
const API_KEY_VAR: &str = "DEEPSEEK_API_KEY";
/// DeepSeek accepts `max` above `high`; it maps `medium` to `high` itself.
const REASONING_EFFORTS: [&str; 4] = ["low", "medium", "high", "max"];

pub(crate) async fn create_with_options(
    model: Option<String>,
    auth: Option<ProviderAuth>,
    timeouts: StreamTimeouts,
) -> anyhow::Result<Arc<dyn Provider>> {
    let key = api_key(auth, std::env::var(API_KEY_VAR).ok())?;
    Ok(Arc::new(
        OpenAiProvider::with_auth(
            model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            BASE_URL.to_string(),
            Arc::new(StaticKey(key)),
        )
        .with_deepseek_dialect()
        .with_reasoning_efforts(REASONING_EFFORTS.map(String::from).to_vec())
        .with_stream_timeouts(timeouts),
    ))
}

/// An explicit key wins over `DEEPSEEK_API_KEY`.
fn api_key(auth: Option<ProviderAuth>, env: Option<String>) -> anyhow::Result<String> {
    match auth {
        Some(ProviderAuth::ApiKey(key)) => Ok(key),
        None => env.filter(|key| !key.is_empty()).ok_or_else(|| {
            anyhow::anyhow!(
                "no DeepSeek API key — set {API_KEY_VAR} (create one at https://platform.deepseek.com/api_keys)"
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_keys_win_over_the_environment_and_a_missing_key_explains_where_to_get_one() {
        let explicit = Some(ProviderAuth::ApiKey("explicit".into()));
        assert_eq!(api_key(explicit, Some("env".into())).unwrap(), "explicit");
        assert_eq!(api_key(None, Some("env".into())).unwrap(), "env");
        for env in [None, Some(String::new())] {
            let error = api_key(None, env).unwrap_err().to_string();
            assert!(error.contains(API_KEY_VAR), "{error}");
            assert!(error.contains("platform.deepseek.com"), "{error}");
        }
    }

    #[tokio::test]
    async fn defaults_to_flash_with_its_1m_window_and_deepseek_labels() {
        let auth = Some(ProviderAuth::ApiKey("test".into()));
        let provider = create_with_options(None, auth.clone(), StreamTimeouts::default())
            .await
            .unwrap();
        assert_eq!(provider.name(), "deepseek");
        assert_eq!(provider.model(), "deepseek-flash");
        assert_eq!(provider.context_window(), 1_000_000);
        let pro = create_with_options(Some("deepseek-v4-pro".into()), auth, Default::default())
            .await
            .unwrap();
        assert_eq!(pro.model(), "deepseek-v4-pro");
        assert_eq!(pro.context_window(), 1_000_000);
    }
}
