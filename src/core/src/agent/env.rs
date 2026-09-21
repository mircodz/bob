use crate::core::events::EventBus;
use crate::lsp::LspManager;
use crate::providers::provider::Provider;
use crate::tools::jobs::JobRegistry;
use crate::tools::registry::ToolRegistry;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct AgentDefinition {
    pub description: String,
    pub prompt: String,
    /// None inherits the full subagent toolset.
    pub tools: Option<Vec<String>>,
    /// Further restricts `tools` to the read-only subset.
    pub read_only: bool,
    /// Per-definition model overrides are not supported and are rejected at launch.
    pub model: Option<String>,
}

#[derive(Clone)]
pub struct AgentEnv {
    pub provider: Arc<dyn Provider>,
    pub subagent_tools: ToolRegistry,
    pub bus: EventBus,
    pub cwd: String,
    pub subagent_system: Option<String>,
    /// Shared with the root and UI.
    pub jobs: JobRegistry,
    pub team: crate::agent::team::AgentRegistry,
    pub lsp: Option<Arc<LspManager>>,
    /// Shared root cancellation, including nested spawns.
    pub parent_cancel: Arc<AtomicBool>,
    pub definitions: HashMap<String, AgentDefinition>,
    pub hooks: crate::agent::hooks::Hooks,
    pub memory: Option<Arc<dyn crate::core::memory::MemoryRepository>>,
}

impl AgentEnv {
    pub fn resolve_child(
        &self,
        subagent_type: Option<&str>,
        read_only: bool,
    ) -> Result<(Option<String>, ToolRegistry), String> {
        if let Some(name) = subagent_type.filter(|s| !s.is_empty()) {
            let def = self.definitions.get(name).ok_or_else(|| {
                let mut names: Vec<&str> = self.definitions.keys().map(String::as_str).collect();
                names.sort_unstable();
                format!(
                    "unknown subagent type \"{name}\". registered: [{}]",
                    names.join(", ")
                )
            })?;
            if def.model.is_some() {
                return Err(format!("subagent type \"{name}\" specifies a model override, but model overrides are not supported"));
            }
            let tools = if let Some(allow) = &def.tools {
                self.subagent_tools.subset(allow)
            } else {
                self.subagent_tools.clone()
            };
            let tools = if read_only || def.read_only {
                tools.read_only_subset()
            } else {
                tools
            };
            let prompt = if read_only || def.read_only {
                format!(
                    "{}\n\n{}",
                    def.prompt,
                    crate::agent::prompt::project_material_trust_block()
                )
            } else {
                def.prompt.clone()
            };
            return Ok((Some(prompt), tools));
        }
        let tools = if read_only {
            self.subagent_tools.read_only_subset()
        } else {
            self.subagent_tools.clone()
        };
        let prompt = if read_only {
            Some(format!(
                "{}\n\n{}",
                self.subagent_system.as_deref().unwrap_or(""),
                crate::agent::prompt::project_material_trust_block()
            ))
        } else {
            self.subagent_system.clone()
        };
        Ok((prompt, tools))
    }

    pub fn definitions_help(&self) -> String {
        if self.definitions.is_empty() {
            return String::new();
        }
        let mut names: Vec<&String> = self.definitions.keys().collect();
        names.sort();
        let mut out = String::from("\n\nAvailable agent types (pass as `subagent_type`):");
        for name in names {
            let def = &self.definitions[name];
            out.push_str(&format!("\n- {}: {}", name, def.description));
        }
        out
    }
}
