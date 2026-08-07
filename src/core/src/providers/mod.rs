pub mod anthropic;
pub mod codec;
pub mod copilot;
#[cfg(test)]
pub mod mock;
pub mod models;
pub mod openai;
pub mod provider;
pub mod responses;
pub mod sse;

use crate::providers::models::ModelSpec;
use crate::providers::provider::Provider;
use std::sync::Arc;

/// Resolve a `"provider/model"` (or legacy `"provider:model"` / bare `"provider"`)
/// spec into a ready provider. This is the registry seam: dispatch on the parsed
/// `ModelSpec.provider` to the family's constructor. Adding a provider is one arm
/// here plus its module — the rest of the stack only ever sees `dyn Provider`.
pub async fn create_provider(spec: &str) -> anyhow::Result<Arc<dyn Provider>> {
    let spec = ModelSpec::parse(spec);
    let model = if spec.model.is_empty() {
        None
    } else {
        Some(spec.model.clone())
    };
    match spec.provider.as_str() {
        "anthropic" => anthropic::create(model).await,
        "openai" => openai::create(model).await,
        "copilot" => copilot::create(model).await,
        other => anyhow::bail!(
            "unknown provider \"{}\". known: anthropic, openai, copilot",
            other
        ),
    }
}

/// Known model ids offered through the ChatGPT-subscription (Codex) backend.
/// Used to populate the /models picker since that backend doesn't list them.
/// Verified against the live endpoint: only these accept requests on a ChatGPT
/// account (gpt-5 / gpt-5-codex etc. return "not supported").
pub const RESPONSES_MODELS: &[&str] = &["gpt-5.6-luna", "gpt-5.6-sol"];
