use crate::providers::provider::{context_window_for, max_output_tokens_for};

/// Which wire protocol a model speaks. Anthropic and the OpenAI Chat Completions
/// API are "chat"; the OpenAI Responses/Codex backend is "responses".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireApi {
    Chat,
    Responses,
}

#[derive(Clone, Debug)]
pub struct ModelSpec {
    pub provider: String,
    pub model: String,
    /// Max input tokens (context window).
    pub context_window: usize,
    /// Default max output tokens for this model (Anthropic requires a value; the
    /// OpenAI providers may omit their cap, but this is the recovery ceiling).
    pub max_output: u32,
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

pub fn wire_for(provider: &str, model: &str) -> WireApi {
    let m = model.to_ascii_lowercase();
    let responses_family = m.contains("codex")
        || m.contains("-sol")
        || m.contains("-luna")
        || m.contains("-terra")
        || m.contains("-astra")
        || m.starts_with("gpt-5.")
        || m == "gpt-5"
        || m.starts_with("gpt-6");
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

        let c = ModelSpec::parse("openai:gpt-4o");
        assert_eq!(c.provider, "openai");
        assert_eq!(c.model, "gpt-4o");
    }

    #[test]
    fn wire_detects_responses_family() {
        assert_eq!(wire_for("openai", "gpt-5.6-sol"), WireApi::Responses);
        assert_eq!(wire_for("openai", "gpt-5-codex"), WireApi::Responses);
        assert_eq!(wire_for("copilot", "gpt-5.6-luna"), WireApi::Responses);
        assert_eq!(wire_for("copilot", "gpt-6-astra"), WireApi::Responses);
        assert_eq!(wire_for("openai", "gpt-4o"), WireApi::Chat);
        assert_eq!(wire_for("anthropic", "claude-opus-4.8"), WireApi::Chat);
    }

    #[test]
    fn parsing_preserves_model_suffixes_and_resolves_capabilities() {
        for (input, provider, model, window, output, wire) in [
            (
                "openai/  gpt-5.6-sol  ",
                "openai",
                "gpt-5.6-sol",
                400_000,
                16_000,
                WireApi::Responses,
            ),
            (
                "copilot:vendor/model:preview",
                "copilot",
                "vendor/model:preview",
                128_000,
                16_000,
                WireApi::Chat,
            ),
            (
                "anthropic/CLAUDE-OPUS-1M",
                "anthropic",
                "CLAUDE-OPUS-1M",
                1_000_000,
                32_000,
                WireApi::Chat,
            ),
            (
                "openai/日本語:preview",
                "openai",
                "日本語:preview",
                128_000,
                16_000,
                WireApi::Chat,
            ),
            ("anthropic", "anthropic", "", 128_000, 16_000, WireApi::Chat),
            ("openai:", "openai", "", 128_000, 16_000, WireApi::Chat),
        ] {
            let spec = ModelSpec::parse(input);
            assert_eq!(spec.provider, provider, "{input}");
            assert_eq!(spec.model, model, "{input}");
            assert_eq!(spec.context_window, window, "{input}");
            assert_eq!(spec.max_output, output, "{input}");
            assert_eq!(spec.wire, wire, "{input}");
        }
    }

    #[test]
    fn responses_routing_is_provider_scoped_and_model_case_insensitive() {
        for provider in ["openai", "copilot"] {
            for model in ["GPT-5.6-SOL", "GPT-5-CODEX", "gpt-6-astra"] {
                assert_eq!(
                    wire_for(provider, model),
                    WireApi::Responses,
                    "{provider}/{model}"
                );
            }
            for model in ["gpt-4o", "claude-sonnet-5", "future-model", ""] {
                assert_eq!(
                    wire_for(provider, model),
                    WireApi::Chat,
                    "{provider}/{model}"
                );
            }
        }
        for provider in ["anthropic", "custom", ""] {
            assert_eq!(
                wire_for(provider, "gpt-5-codex"),
                WireApi::Chat,
                "{provider}"
            );
        }
    }
}
