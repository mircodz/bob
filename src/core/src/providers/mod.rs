pub mod anthropic;
pub mod codec;
pub mod copilot;
#[cfg(any(test, feature = "testing"))]
pub mod mock;
pub mod models;
pub mod openai;
pub mod provider;
pub mod responses;
pub mod sse;

use crate::auth::ProviderAuth;
use crate::providers::models::ModelSpec;
use crate::providers::provider::{Provider, StreamTimeouts};
use std::sync::Arc;

pub async fn create_provider(spec: &str) -> anyhow::Result<Arc<dyn Provider>> {
    create_provider_with_auth(spec, None).await
}

/// Explicit auth overrides provider-specific ambient credential resolution.
pub async fn create_provider_with_auth(
    spec: &str,
    auth: Option<ProviderAuth>,
) -> anyhow::Result<Arc<dyn Provider>> {
    create_provider_with_options(spec, auth, StreamTimeouts::default()).await
}

pub async fn create_provider_with_options(
    spec: &str,
    auth: Option<ProviderAuth>,
    timeouts: StreamTimeouts,
) -> anyhow::Result<Arc<dyn Provider>> {
    let spec = ModelSpec::parse(spec);
    let model = if spec.model.is_empty() {
        None
    } else {
        Some(spec.model.clone())
    };
    tokio::time::timeout(timeouts.startup, async {
        match spec.provider.as_str() {
            "anthropic" => anthropic::create_with_options(model, auth, timeouts).await,
            "openai" => openai::create_with_options(model, auth, timeouts).await,
            "copilot" => copilot::create_with_options(model, auth, timeouts).await,
            other => anyhow::bail!(
                "unknown provider \"{}\". known: anthropic, openai, copilot",
                other
            ),
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("provider initialization deadline exceeded"))?
}

/// The Codex backend does not enumerate models; these populate the picker.
pub const RESPONSES_MODELS: &[&str] = &["gpt-5.6-luna", "gpt-5.6-sol"];
