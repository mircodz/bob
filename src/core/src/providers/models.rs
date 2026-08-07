//! `ModelSpec` — the declarative metadata for one model, and the seam that
//! resolves a `"provider/model"` string into it. This is the SDK-facing model
//! value: the builder takes a spec string, resolves it here, and the result
//! carries everything a provider needs (id, context window, output ceiling, wire
//! protocol) as data rather than scattered substring lookups at call sites.
//!
//! Capability knowledge still lives in the graceful [`context_window_for`] /
//! [`max_output_tokens_for`] matchers (they handle unknown/future models, which a
//! static table can't) — `ModelSpec` just bundles their output into one value.

use crate::providers::provider::{context_window_for, max_output_tokens_for};

/// Which wire protocol a model speaks. Anthropic and the OpenAI Chat Completions
/// API are "chat"; the OpenAI Responses/Codex backend is "responses".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireApi {
    Chat,
    Responses,
}

/// Declarative metadata for one model — resolved once, then carried through the
/// stack instead of re-deriving caps at each call site.
#[derive(Clone, Debug)]
pub struct ModelSpec {
    /// Provider id, e.g. "anthropic", "openai", "copilot".
    pub provider: String,
    /// Model id, e.g. "claude-opus-4.8".
    pub model: String,
    /// Max input tokens (context window).
    pub context_window: usize,
    /// Default max output tokens for this model (Anthropic requires a value; the
    /// OpenAI providers may omit their cap, but this is the recovery ceiling).
    pub max_output: u32,
    /// Wire protocol the provider should speak for this model.
    pub wire: WireApi,
}

impl ModelSpec {
    /// Parse a `"provider/model"` (or bare `"provider"`) spec string and resolve
    /// its capabilities. A bare provider leaves `model` empty for the provider to
    /// fill with its own default. Also accepts the legacy `"provider:model"` form.
    pub fn parse(spec: &str) -> ModelSpec {
        let sep = spec.find(['/', ':']);
        let (provider, model) = match sep {
            Some(i) => (&spec[..i], spec[i + 1..].trim()),
            None => (spec, ""),
        };
        ModelSpec::new(provider, model)
    }

    /// Build a spec from an explicit provider + model id.
    pub fn new(provider: &str, model: &str) -> ModelSpec {
        ModelSpec {
            provider: provider.to_string(),
            model: model.to_string(),
            context_window: context_window_for(model),
            max_output: max_output_tokens_for(model),
            wire: wire_for(provider, model),
        }
    }
}

/// The wire protocol for a provider+model. The OpenAI "responses"/Codex backend
/// (reached via ChatGPT-subscription OAuth) and specific Codex models speak the
/// Responses API; everything else speaks chat. Keeps the one-off model-name
/// checks (formerly `RESPONSES_MODELS` + copilot's `is_responses_model`) in one
/// place.
pub fn wire_for(provider: &str, model: &str) -> WireApi {
    let m = model.to_ascii_lowercase();
    // Codex/Responses-only model families, regardless of provider routing.
    let responses_family = m.contains("codex")
        || m.contains("-sol")
        || m.contains("-luna")
        || m.starts_with("gpt-5.")
        || m == "gpt-5";
    if (provider == "openai" || provider == "copilot") && responses_family {
        WireApi::Responses
    } else {
        WireApi::Chat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_provider_and_model() {
        let s = ModelSpec::parse("anthropic/claude-opus-4.8");
        assert_eq!(s.provider, "anthropic");
        assert_eq!(s.model, "claude-opus-4.8");
        assert_eq!(s.context_window, 200_000);
        assert_eq!(s.wire, WireApi::Chat);

        // Legacy colon form still parses.
        let c = ModelSpec::parse("openai:gpt-4o");
        assert_eq!(c.provider, "openai");
        assert_eq!(c.model, "gpt-4o");

        // Bare provider leaves model empty for the provider default.
        let b = ModelSpec::parse("anthropic");
        assert_eq!(b.provider, "anthropic");
        assert_eq!(b.model, "");
    }

    #[test]
    fn wire_detects_responses_family() {
        assert_eq!(wire_for("openai", "gpt-5.6-sol"), WireApi::Responses);
        assert_eq!(wire_for("openai", "gpt-5-codex"), WireApi::Responses);
        assert_eq!(wire_for("copilot", "gpt-5.6-luna"), WireApi::Responses);
        // A plain chat model, and Anthropic, stay on chat.
        assert_eq!(wire_for("openai", "gpt-4o"), WireApi::Chat);
        assert_eq!(wire_for("anthropic", "claude-opus-4.8"), WireApi::Chat);
    }
}
