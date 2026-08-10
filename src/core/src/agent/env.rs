//! `AgentEnv` — the shared bundle of dependencies every subagent-spawning tool
//! needs to build a child agent.
//!
//! `TaskTool`, `WorkflowTool`, `ExploreTool`, and the coordination `CoordDeps`
//! each independently carried the SAME set of fields (provider, subagent tools,
//! event bus, cwd, system prompt, jobs, lsp, the root cancel flag). That's one
//! concept spelled four times. `AgentEnv` is that concept once: the tools hold an
//! `env: AgentEnv` and read `self.env.provider` etc., so adding a dependency is a
//! single field here instead of a four-site edit.

use crate::core::events::EventBus;
use crate::lsp::LspManager;
use crate::providers::provider::Provider;
use crate::tools::jobs::JobRegistry;
use crate::tools::registry::ToolRegistry;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// A named, specialized subagent the model can delegate to (mirrors the Claude
/// Agent SDK's `agents` definitions). The model picks one by matching a task to
/// its `description`, or a caller names it explicitly; the spawned child then runs
/// with this definition's own system prompt and tool restriction instead of the
/// shared defaults.
#[derive(Clone, Debug)]
pub struct AgentDefinition {
    /// When to use this agent — surfaced to the model so it can auto-delegate.
    pub description: String,
    /// The child's system prompt (its role/expertise/constraints).
    pub prompt: String,
    /// Allow-list of tool names this agent may use. `None` inherits the full
    /// subagent toolset; `Some([..])` restricts to exactly those tools.
    pub tools: Option<Vec<String>>,
    /// Convenience restriction: confine the agent to the read-only tool subset
    /// (reads/searches, no write/edit/bash). Takes precedence over `tools`.
    pub read_only: bool,
    /// Per-definition model override. Reserved — bob's subagents currently inherit
    /// the root provider, so this is not yet applied; kept so callers can specify
    /// it and it's honored once subagent-provider-override lands.
    pub model: Option<String>,
}

/// Everything needed to spawn a child agent, shared by every delegation tool.
#[derive(Clone)]
pub struct AgentEnv {
    pub provider: Arc<dyn Provider>,
    /// The toolset a spawned child receives (explore uses only its read-only
    /// subset).
    pub subagent_tools: ToolRegistry,
    pub bus: EventBus,
    pub cwd: String,
    /// The composed system prompt handed to spawned children. `None` for tools
    /// (explore) that supply their own focused prompt.
    pub subagent_system: Option<String>,
    /// Shared job registry (same instance the root agent + UI use).
    pub jobs: JobRegistry,
    /// Shared language servers, so subagents get diagnostics/nav too. `None` when
    /// no lsp_servers are configured.
    pub lsp: Option<Arc<LspManager>>,
    /// The root's cancel flag, handed to every child as `parent_cancel`, so one
    /// Cancel cascades through the whole team (including nested spawns).
    pub parent_cancel: Arc<AtomicBool>,
    /// Named subagent definitions the model may delegate to by `subagent_type`.
    /// Empty by default (every child then uses `subagent_system` + the full set).
    pub definitions: HashMap<String, AgentDefinition>,
}

impl AgentEnv {
    /// Resolve the child system prompt + toolset for a spawn, honoring (in order):
    /// a named definition's prompt/tools, then a `read_only` flag, then the shared
    /// defaults. Returns an error when `subagent_type` names an unknown definition,
    /// listing the registered names so the model can correct itself.
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
            let tools = if def.read_only {
                self.subagent_tools.read_only_subset()
            } else if let Some(allow) = &def.tools {
                self.subagent_tools.subset(allow)
            } else {
                self.subagent_tools.clone()
            };
            return Ok((Some(def.prompt.clone()), tools));
        }
        // No named definition: the shared prompt + (optionally read-only) toolset.
        let tools = if read_only {
            self.subagent_tools.read_only_subset()
        } else {
            self.subagent_tools.clone()
        };
        Ok((self.subagent_system.clone(), tools))
    }

    /// A rendered list of the available subagent definitions, appended to the
    /// spawn tools' descriptions so the model knows what it can delegate to. Empty
    /// string when no definitions are registered.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock::MockProvider;

    fn env_with(defs: Vec<(&str, AgentDefinition)>) -> AgentEnv {
        AgentEnv {
            provider: Arc::new(MockProvider::new(vec![])),
            subagent_tools: ToolRegistry::new(None),
            bus: EventBus::new(),
            cwd: ".".to_string(),
            subagent_system: Some("shared prompt".to_string()),
            jobs: JobRegistry::new(),
            lsp: None,
            parent_cancel: Arc::new(AtomicBool::new(false)),
            definitions: defs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    fn def(desc: &str, prompt: &str, read_only: bool) -> AgentDefinition {
        AgentDefinition {
            description: desc.to_string(),
            prompt: prompt.to_string(),
            tools: None,
            read_only,
            model: None,
        }
    }

    #[test]
    fn resolve_child_uses_the_named_definitions_prompt() {
        let env = env_with(vec![(
            "reviewer",
            def("reviews code", "you are a reviewer", true),
        )]);
        let system = env.resolve_child(Some("reviewer"), false).ok().unwrap().0;
        assert_eq!(system.as_deref(), Some("you are a reviewer"));
    }

    #[test]
    fn resolve_child_falls_back_to_shared_prompt_when_no_type() {
        let env = env_with(vec![]);
        let system = env.resolve_child(None, false).ok().unwrap().0;
        assert_eq!(system.as_deref(), Some("shared prompt"));
    }

    #[test]
    fn resolve_child_errors_on_unknown_type_and_names_the_set() {
        let env = env_with(vec![("reviewer", def("d", "p", false))]);
        let err = match env.resolve_child(Some("nope"), false) {
            Ok(_) => panic!("expected an error for an unknown subagent type"),
            Err(e) => e,
        };
        assert!(err.contains("unknown subagent type"), "{err}");
        assert!(err.contains("reviewer"), "{err}");
    }

    #[test]
    fn definitions_help_lists_registered_agents() {
        let env = env_with(vec![("reviewer", def("reviews code", "p", false))]);
        let help = env.definitions_help();
        assert!(help.contains("Available agent types"));
        assert!(help.contains("reviewer: reviews code"));
        // Empty when none registered.
        assert!(env_with(vec![]).definitions_help().is_empty());
    }
}
