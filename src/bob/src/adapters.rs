pub use crate::tools::ToolService;
use crate::{provider::ProviderService, services::Services};
use anyhow::Result;
use async_trait::async_trait;
use bob_kernel::{
    Executor, Invocation, ModelCapabilities, ModelFailureKind, ModelInfo, PreparedTool,
    RequestSpec, ToolCapabilities, ToolDescriptor, ToolOutcome,
};
use bob_types::Completion;
use std::sync::Arc;
use tokio::sync::watch;

struct AppExecutor {
    provider: ProviderService,
    tools: ToolService,
    services: Option<Arc<Services>>,
}

pub(crate) fn configured_executor(
    provider: ProviderService,
    configured_shell: Option<bob_core::core::shell::ShellSelection>,
    services: Arc<Services>,
) -> Arc<dyn Executor> {
    Arc::new(AppExecutor {
        provider,
        tools: ToolService::new(configured_shell, dirs::home_dir()),
        services: Some(services),
    })
}

pub async fn create_executor(models: &[String]) -> Result<Arc<dyn Executor>> {
    Ok(Arc::new(AppExecutor {
        provider: ProviderService::create(models).await?,
        tools: ToolService::default(),
        services: None,
    }))
}

#[async_trait]
impl Executor for AppExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        self.provider.models()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        let mut tools = self.tools.tools();
        if let Some(services) = &self.services {
            tools.extend(services.descriptors());
        }
        tools
    }
    fn model_capabilities(&self, model: &str) -> ModelCapabilities {
        self.provider.capabilities(model)
    }
    fn measured_input_tokens(&self, _model: &str, usage: &bob_types::Usage) -> Option<u64> {
        full_input_measurement(usage)
    }
    fn classify_error(&self, error: &anyhow::Error) -> ModelFailureKind {
        self.provider.classify_error(error)
    }
    fn tool_capabilities(&self, name: &str) -> ToolCapabilities {
        if let Some(services) = self
            .services
            .as_ref()
            .filter(|services| services.contains(name))
        {
            services.tool_capabilities(name)
        } else {
            self.tools.capabilities(name)
        }
    }
    fn prepared_capabilities(&self, tool: &PreparedTool) -> Result<ToolCapabilities> {
        if let Some(services) = self
            .services
            .as_ref()
            .filter(|services| services.contains(&tool.invocation.name))
        {
            services.prepared_capabilities(tool)
        } else {
            self.tools.prepared_capabilities(tool)
        }
    }
    fn prepared_shell_command(
        &self,
        tool: &PreparedTool,
    ) -> Result<Option<bob_kernel::ShellCommandGrant>> {
        if self
            .services
            .as_ref()
            .is_some_and(|services| services.contains(&tool.invocation.name))
        {
            Ok(None)
        } else {
            self.tools.prepared_shell_command(tool)
        }
    }
    fn prepared_policy(
        &self,
        tool: &PreparedTool,
        permissions: &bob_kernel::PermissionPolicy,
    ) -> Result<Option<bob_kernel::RuleDecision>> {
        if self
            .services
            .as_ref()
            .is_some_and(|services| services.contains(&tool.invocation.name))
        {
            Ok(None)
        } else {
            self.tools.prepared_policy(tool, permissions)
        }
    }
    fn prepared_authorization_target(
        &self,
        tool: &PreparedTool,
    ) -> Result<Option<std::path::PathBuf>> {
        if self
            .services
            .as_ref()
            .is_some_and(|services| services.contains(&tool.invocation.name))
        {
            Ok(None)
        } else {
            self.tools.prepared_authorization_target(tool)
        }
    }
    fn prepared_authorization_requirements(
        &self,
        tool: &PreparedTool,
    ) -> Result<Vec<bob_kernel::PreparedAuthorizationRequirement>> {
        if let Some(services) = self
            .services
            .as_ref()
            .filter(|services| services.contains(&tool.invocation.name))
        {
            services.prepared_authorization_requirements(tool)
        } else {
            Ok(Vec::new())
        }
    }
    fn display_text(&self, tool: &PreparedTool, outcome: &ToolOutcome) -> Result<Option<String>> {
        self.tools.display_text(tool, outcome)
    }
    async fn system_context(&self, workspace: &str, base: &str) -> Result<String> {
        if let Some(services) = &self.services {
            services.system_context(workspace, base).await
        } else {
            Ok(base.to_owned())
        }
    }
    async fn generate(
        &self,
        request: RequestSpec,
        cancel: watch::Receiver<bool>,
        progress: bob_kernel::ModelProgress,
    ) -> Result<Completion> {
        self.provider.generate(request, cancel, progress).await
    }
    async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        if let Some(services) = self
            .services
            .as_ref()
            .filter(|services| services.contains(&invocation.name))
        {
            services.prepare(invocation).await
        } else {
            self.tools.prepare(invocation).await
        }
    }
    async fn prepare_with_cancel(
        &self,
        invocation: Invocation,
        cancel: watch::Receiver<bool>,
    ) -> Result<PreparedTool> {
        if let Some(services) = self
            .services
            .as_ref()
            .filter(|services| services.contains(&invocation.name))
        {
            services.prepare_with_cancel(invocation, cancel).await
        } else {
            self.tools.prepare(invocation).await
        }
    }
    async fn execute_with_progress(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
        progress: bob_kernel::ToolProgress,
    ) -> Result<ToolOutcome> {
        self.execute_observed(tool, cancel, Some(progress)).await
    }
    async fn execute(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        self.execute_observed(tool, cancel, None).await
    }
}

fn full_input_measurement(usage: &bob_types::Usage) -> Option<u64> {
    let input = usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens);
    (input > 0).then_some(input)
}

impl AppExecutor {
    async fn execute_observed(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
        progress: Option<bob_kernel::ToolProgress>,
    ) -> Result<ToolOutcome> {
        if let Some(services) = self
            .services
            .as_ref()
            .filter(|services| services.contains(&tool.invocation.name))
        {
            return services.execute(tool, cancel).await;
        }
        let changed = matches!(
            tool.invocation.name.as_str(),
            "write_file" | "edit_file" | "multi_edit" | "create_file"
        )
        .then(|| {
            tool.invocation
                .input
                .get("path")
                .and_then(serde_json::Value::as_str)
        })
        .flatten()
        .map(|path| std::path::Path::new(&tool.invocation.workspace).join(path));
        let outcome = match progress {
            Some(progress) => {
                self.tools
                    .execute_observed(tool, cancel, Some(progress))
                    .await?
            }
            None => self.tools.execute(tool, cancel).await?,
        };
        if !outcome.is_error && !outcome.interrupted {
            if let (Some(services), Some(path)) = (&self.services, changed) {
                services.notify_changed(&path);
            }
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod measurement_tests {
    use super::*;

    #[test]
    fn context_measurement_counts_normalized_input_once_without_changing_usage() {
        for (input, created, cached, expected) in [(60, 0, 40, 100), (11, 13, 17, 41)] {
            let usage = bob_types::Usage {
                input_tokens: input,
                cache_creation_input_tokens: created,
                cache_read_input_tokens: cached,
                output_tokens: 700,
            };
            let before = serde_json::to_value(usage).unwrap();
            assert_eq!(full_input_measurement(&usage), Some(expected));
            assert_eq!(serde_json::to_value(usage).unwrap(), before);
        }
        assert_eq!(
            full_input_measurement(&bob_types::Usage {
                output_tokens: 700,
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            full_input_measurement(&bob_types::Usage {
                input_tokens: u64::MAX,
                cache_creation_input_tokens: 1,
                cache_read_input_tokens: 1,
                ..Default::default()
            }),
            Some(u64::MAX)
        );
    }
}
