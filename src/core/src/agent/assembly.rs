//! Assembling the root agent + its full tool surface. Both frontends (the TUI and
//! the remote host) need the exact same wiring: build the tool registries (builtins
//! + MCP + LSP + coordination), compose the `task`/`spawn_agent`/`send_message`/
//! `list_agents` tools around a shared team, and register the root as a team member
//! with its own mailbox so children can report back to it. This module owns that
//! wiring once so the two frontends can't drift.

use crate::agent::agent::{Agent, AgentConfig};
use crate::agent::team::{mailbox, AgentRegistry};
use crate::core::events::EventBus;
use crate::lsp::LspManager;
use crate::providers::provider::Provider;
use crate::tools::coordinate::{CoordDeps, ListAgentsTool, SendMessageTool, SpawnAgentTool};
use crate::tools::jobs::JobRegistry;
use crate::tools::lsp::LspTool;
use crate::tools::lsp_actions::{CodeActionTool, RenameSymbolTool};
use crate::tools::registry::{Tool, ToolRegistry, UserAsker};
use crate::tools::task::TaskTool;
use std::sync::Arc;

/// The tuning knobs shared by every root agent. Centralized so the two frontends
/// stop repeating the same literals (and silently disagreeing).
const CONTEXT_WINDOW: usize = 200_000;
const COMPACT_THRESHOLD: f64 = 0.8;
const KEEP_RECENT: usize = 6;
const DEFAULT_MAX_TURNS: u32 = 20;

/// Everything a frontend must supply to build the root agent. The frontend still
/// owns process-level concerns (event bus, permission engine, how it asks the
/// user); this collects them so the wiring can live in one place.
pub struct RootAgentParams {
    pub provider: Arc<dyn Provider>,
    pub permissions: Arc<crate::core::permissions::PermissionEngine>,
    pub bus: EventBus,
    pub jobs: JobRegistry,
    pub team: AgentRegistry,
    pub cwd: String,
    /// The composed system prompt (base + environment + project context).
    pub system_prompt: String,
    /// Configured MCP tools (already connected), namespaced `<server>.<tool>`.
    pub mcp_tools: Vec<Arc<dyn Tool>>,
    /// Shared language servers, or None if none are configured.
    pub lsp: Option<Arc<LspManager>>,
    /// UI hook for ask_user / exit_plan.
    pub user_asker: Arc<dyn UserAsker>,
    /// Turn budget for the root agent; None → the default.
    pub max_turns: Option<u32>,
}

/// Build the shared subagent tool registry: builtins + MCP + LSP tools + the
/// cycle-free coordination tools (send_message / list_agents). This is what every
/// *spawned* agent gets; `spawn_agent` itself is added only to the root's tools.
fn build_subagent_tools(p: &RootAgentParams) -> ToolRegistry {
    let mut tools = ToolRegistry::new(Some(p.permissions.clone()));
    for t in crate::tools::builtin_tools() {
        tools.add(t);
    }
    for t in &p.mcp_tools {
        tools.add(t.clone());
    }
    if let Some(lsp) = &p.lsp {
        tools.add(Arc::new(LspTool::new(lsp.clone())));
        tools.add(Arc::new(RenameSymbolTool::new(lsp.clone())));
        tools.add(Arc::new(CodeActionTool::new(lsp.clone())));
    }
    tools.add(Arc::new(SendMessageTool {
        team: p.team.clone(),
    }));
    tools.add(Arc::new(ListAgentsTool {
        team: p.team.clone(),
    }));
    tools
}

/// Build the root agent with its full tool surface, register it in the team as
/// "root" with a mailbox, and return it ready to load history and run.
pub fn build_root_agent(p: RootAgentParams) -> Agent {
    let subagent_tools = build_subagent_tools(&p);

    // The root's tools are the subagent set (minus its coordination tools, which
    // we re-add explicitly below) plus the task + spawn tools it uses to delegate.
    let mut tools = ToolRegistry::new(Some(p.permissions.clone()));
    for t in crate::tools::builtin_tools() {
        tools.add(t);
    }
    for t in &p.mcp_tools {
        tools.add(t.clone());
    }
    if let Some(lsp) = &p.lsp {
        tools.add(Arc::new(LspTool::new(lsp.clone())));
        tools.add(Arc::new(RenameSymbolTool::new(lsp.clone())));
        tools.add(Arc::new(CodeActionTool::new(lsp.clone())));
    }
    tools.add(Arc::new(TaskTool {
        provider: p.provider.clone(),
        subagent_tools: subagent_tools.clone(),
        bus: p.bus.clone(),
        cwd: p.cwd.clone(),
        subagent_system: Some(p.system_prompt.clone()),
        jobs: p.jobs.clone(),
        lsp: p.lsp.clone(),
    }));

    // Coordination tools: spawn children from the same deps as `task`, and let the
    // root message + inspect the team. Children get send/list via subagent_tools;
    // nesting depth is enforced at runtime by spawn_agent.
    let deps = CoordDeps {
        provider: p.provider.clone(),
        subagent_tools: subagent_tools.clone(),
        bus: p.bus.clone(),
        cwd: p.cwd.clone(),
        subagent_system: Some(p.system_prompt.clone()),
        jobs: p.jobs.clone(),
        lsp: p.lsp.clone(),
        team: p.team.clone(),
    };
    tools.add(Arc::new(SpawnAgentTool { deps }));
    tools.add(Arc::new(SendMessageTool {
        team: p.team.clone(),
    }));
    tools.add(Arc::new(ListAgentsTool {
        team: p.team.clone(),
    }));

    // Register the root as a team member with its own mailbox, so spawned agents
    // can report their results back to "root" and wake it for a fresh turn.
    let (root_inbox, root_tx) = mailbox();
    p.team.register("root".to_string(), 0, root_tx);

    Agent::new(AgentConfig {
        provider: p.provider.clone(),
        tools,
        bus: p.bus.clone(),
        system: Some(p.system_prompt.clone()),
        cwd: p.cwd.clone(),
        max_turns: p.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
        id: Some("root".to_string()),
        context_window: CONTEXT_WINDOW,
        compact_threshold: COMPACT_THRESHOLD,
        keep_recent: KEEP_RECENT,
        jobs: p.jobs.clone(),
        user_asker: Some(p.user_asker.clone()),
        lsp: p.lsp.clone(),
        inbox: Some(root_inbox),
        team: Some(p.team.clone()),
        name: "root".to_string(),
        depth: 0,
    })
}
