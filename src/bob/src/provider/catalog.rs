use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use bob_core::auth::copilot::{self, ModelLimits};
use bob_core::providers::models::ModelSpec;
use bob_core::providers::provider::{ModelEntry, Provider, StreamTimeouts};
use bob_kernel::{ModelCapabilities, ModelInfo};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::OnceCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ProviderKind {
    Copilot,
    OpenAi,
    Anthropic,
}

impl ProviderKind {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "copilot" => Ok(Self::Copilot),
            "openai" => Ok(Self::OpenAi),
            "anthropic" => Ok(Self::Anthropic),
            _ => anyhow::bail!(
                "unknown provider {value:?}; known providers: copilot, openai, anthropic"
            ),
        }
    }

    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Copilot => "copilot",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Selection {
    pub provider: ProviderKind,
    pub model: String,
}

impl Selection {
    pub(super) fn parse(value: &str) -> Result<Self> {
        ensure!(
            !value.is_empty()
                && !value.chars().any(char::is_whitespace)
                && !value.chars().any(char::is_control),
            "model ID must be nonempty without whitespace or control characters"
        );
        if let Ok(provider) = ProviderKind::parse(value) {
            return Ok(Self {
                provider,
                model: String::new(),
            });
        }
        let (provider, model) = match value.find([':', '/']) {
            Some(index) => (ProviderKind::parse(&value[..index])?, &value[index + 1..]),
            None => (ProviderKind::Copilot, value),
        };
        ensure!(!model.is_empty(), "missing model ID after provider prefix");
        Ok(Self {
            provider,
            model: model.to_owned(),
        })
    }

    pub(super) fn id(&self) -> String {
        if self.model.is_empty() {
            self.provider.name().to_owned()
        } else {
            format!("{}:{}", self.provider.name(), self.model)
        }
    }

    pub(super) fn factory_spec(&self) -> String {
        if self.model.is_empty() {
            self.provider.name().to_owned()
        } else {
            format!("{}/{}", self.provider.name(), self.model)
        }
    }
}

pub(super) fn requested_models(models: &[String]) -> Result<Vec<Selection>> {
    ensure!(
        !models.is_empty(),
        "request at least one provider or model ID"
    );
    let mut seen = HashSet::new();
    let mut selections = Vec::new();
    for value in models {
        let selection = Selection::parse(value)?;
        if seen.insert(selection.clone()) {
            selections.push(selection);
        }
    }
    Ok(selections)
}

pub(super) struct ModelBinding {
    pub info: ModelInfo,
    pub capabilities: ModelCapabilities,
    pub selection: Selection,
    pub provider: OnceCell<Arc<dyn Provider>>,
}

impl ModelBinding {
    pub(super) async fn provider(
        &self,
        factory: &dyn ProviderFactory,
        timeouts: StreamTimeouts,
    ) -> Result<&Arc<dyn Provider>> {
        self.provider
            .get_or_try_init(|| async {
                let provider = factory
                    .create(&self.selection.factory_spec(), timeouts)
                    .await?;
                ensure!(
                    provider.model() == self.selection.model,
                    "provider factory changed the requested model"
                );
                Ok(provider)
            })
            .await
    }
}

pub(super) fn copilot_binding(limits: &ModelLimits) -> Result<ModelBinding> {
    let context_window = limits.max_context_window_tokens.or(limits.max_prompt_tokens)
        .filter(|limit| *limit > 1)
        .with_context(|| format!("Copilot did not advertise a usable context window for {}; refusing a guessed limit", limits.id))?;
    ensure!(
        limits.max_prompt_tokens != Some(0),
        "Copilot advertised a zero input limit for {}",
        limits.id
    );
    ensure!(
        limits.max_output_tokens != Some(0),
        "Copilot advertised a zero output limit for {}",
        limits.id
    );
    let selection = Selection::parse(&format!("copilot:{}", limits.id))?;
    let max_output_tokens = limits
        .max_output_tokens
        .unwrap_or(context_window - 1)
        .min(context_window - 1)
        .min(u32::MAX as usize) as u32;
    Ok(ModelBinding {
        info: ModelInfo {
            id: selection.id(),
            context_window,
        },
        capabilities: ModelCapabilities {
            max_input_tokens: limits.max_prompt_tokens,
            max_output_tokens,
            preferred_output_tokens: max_output_tokens,
        },
        selection,
        provider: OnceCell::new(),
    })
}

pub(super) fn normal_binding(kind: ProviderKind, entry: ModelEntry) -> Result<ModelBinding> {
    let selection = Selection::parse(&format!("{}:{}", kind.name(), entry.id))?;
    let spec = ModelSpec::new(kind.name(), &entry.id);
    let context_window = entry.context_window.unwrap_or(spec.context_window);
    ensure!(
        context_window > 1,
        "provider advertised an unusable context window for {}",
        selection.id()
    );
    let max_output_tokens = spec
        .max_output
        .min((context_window - 1).min(u32::MAX as usize) as u32);
    Ok(ModelBinding {
        info: ModelInfo {
            id: selection.id(),
            context_window,
        },
        capabilities: ModelCapabilities {
            max_input_tokens: Some(context_window),
            max_output_tokens,
            preferred_output_tokens: max_output_tokens,
        },
        selection,
        provider: OnceCell::new(),
    })
}

#[async_trait]
pub(super) trait ProviderFactory: Send + Sync {
    async fn create(&self, spec: &str, timeouts: StreamTimeouts) -> Result<Arc<dyn Provider>>;
    async fn copilot_limits(&self) -> Result<Vec<ModelLimits>>;
}

pub(super) struct CoreFactory;

#[async_trait]
impl ProviderFactory for CoreFactory {
    async fn create(&self, spec: &str, timeouts: StreamTimeouts) -> Result<Arc<dyn Provider>> {
        bob_core::providers::create_provider_with_options(spec, None, timeouts).await
    }

    async fn copilot_limits(&self) -> Result<Vec<ModelLimits>> {
        let token = copilot::github_token()
            .context("not logged in to GitHub Copilot; run `bob login copilot`")?;
        let (token, _, endpoint) = copilot::fetch_copilot_token(&token).await?;
        let catalog = copilot::fetch_model_limits(&token, &endpoint).await;
        ensure!(!catalog.is_empty(), "Copilot model discovery returned no models (the request may have failed); refusing guessed IDs or limits");
        Ok(catalog)
    }
}

pub(super) async fn discover(
    selections: &[Selection],
    factory: &dyn ProviderFactory,
    timeouts: StreamTimeouts,
) -> Result<(Vec<ModelBinding>, Vec<(ProviderKind, String)>)> {
    let mut kinds = HashSet::new();
    let mut bindings = Vec::new();
    let mut defaults = Vec::new();
    for requested in selections {
        let kind = requested.provider;
        if !kinds.insert(kind) {
            continue;
        }
        let group: Vec<_> = selections.iter().filter(|s| s.provider == kind).collect();
        let seed = group
            .iter()
            .find(|s| s.model.is_empty())
            .copied()
            .unwrap_or(requested);
        let provider = factory.create(&seed.factory_spec(), timeouts).await?;
        ensure!(
            seed.model.is_empty() || seed.model == provider.model(),
            "provider factory changed the requested model"
        );
        let active = Selection::parse(&format!("{}:{}", kind.name(), provider.model()))?;
        if seed.model.is_empty() {
            defaults.push((kind, active.id()));
        }
        let mut catalog = if kind == ProviderKind::Copilot {
            factory
                .copilot_limits()
                .await?
                .iter()
                .filter_map(|m| copilot_binding(m).ok())
                .collect::<Vec<_>>()
        } else {
            let mut entries =
                tokio::time::timeout(timeouts.startup / 2, provider.list_models_detailed())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_default();
            if entries.is_empty() && kind == ProviderKind::OpenAi {
                entries.extend(
                    bob_core::providers::RESPONSES_MODELS
                        .iter()
                        .map(|id| ModelEntry {
                            id: (*id).to_owned(),
                            context_window: None,
                        }),
                );
            }
            if !entries.iter().any(|m| m.id == active.model) {
                entries.push(ModelEntry {
                    id: active.model.clone(),
                    context_window: Some(provider.context_window()),
                });
            }
            for selection in &group {
                if !selection.model.is_empty() && !entries.iter().any(|m| m.id == selection.model) {
                    entries.push(ModelEntry {
                        id: selection.model.clone(),
                        context_window: None,
                    });
                }
            }
            entries
                .into_iter()
                .map(|entry| normal_binding(kind, entry))
                .collect::<Result<Vec<_>>>()?
        };
        catalog.sort_by(|a, b| a.info.id.cmp(&b.info.id));
        catalog.dedup_by(|a, b| a.info.id == b.info.id);
        for selection in &group {
            let id = if selection.model.is_empty() {
                active.id()
            } else {
                selection.id()
            };
            ensure!(
                catalog.iter().any(|binding| binding.info.id == id),
                "unsupported {} model ID {id:?}; advertised usable models: {}",
                kind.name(),
                catalog
                    .iter()
                    .map(|binding| binding.info.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        let binding = catalog
            .iter_mut()
            .find(|binding| binding.info.id == active.id())
            .context("provider's active model is absent from its usable catalog")?;
        binding.provider = OnceCell::new_with(Some(provider));
        bindings.extend(catalog);
    }
    bindings.sort_by_key(|binding| {
        selections
            .iter()
            .position(|selection| {
                selection.id() == binding.info.id
                    || (selection.model.is_empty()
                        && defaults.iter().any(|(kind, id)| {
                            *kind == selection.provider && id == &binding.info.id
                        }))
            })
            .unwrap_or(usize::MAX)
    });
    Ok((bindings, defaults))
}
