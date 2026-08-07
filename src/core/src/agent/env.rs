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
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

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
}
