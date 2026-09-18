//! Shared root-agent assembly for the CLI and headless callers. Builds the tool
//! registries (builtins, MCP, LSP, and coordination), wires delegation around a
//! shared team, and registers the root mailbox so children can report back.

use crate::agent::agent::{
    Agent, AgentConfig, COMPACT_THRESHOLD, DEFAULT_MAX_TURNS, KEEP_RECENT, ROOT_AGENT_ID,
};
use crate::agent::team::{mailbox, AgentRegistry};
use crate::core::events::EventBus;
use crate::lsp::LspManager;
use crate::providers::provider::Provider;
use crate::tools::coordinate::{
    CoordDeps, ListAgentsTool, SendMessageTool, SpawnAgentTool, StopAgentTool,
};
use crate::tools::jobs::JobRegistry;
use crate::tools::lsp::LspTool;
use crate::tools::lsp_actions::{CodeActionTool, RenameSymbolTool};
use crate::tools::registry::{Tool, ToolRegistry, UserAsker};
use crate::tools::task::TaskTool;
use std::sync::Arc;

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
    /// Caller-supplied custom tools (SDK `.tool(...)`). Merged into the root +
    /// subagent registries alongside the builtins and MCP tools. Empty for the
    /// frontends, which don't register custom tools.
    pub extra_tools: Vec<Arc<dyn Tool>>,
    /// Shared language servers, or None if none are configured.
    pub lsp: Option<Arc<LspManager>>,
    /// UI hook for ask_user / exit_plan.
    pub user_asker: Arc<dyn UserAsker>,
    /// Turn budget for the root agent; None → the default.
    pub max_turns: Option<u32>,
    /// Named subagent definitions the model may delegate to (SDK `.agent(...)`).
    /// Empty for the frontends, which don't register custom types.
    pub definitions: std::collections::HashMap<String, crate::agent::env::AgentDefinition>,
    /// Tool-call hooks (SDK `.on_pre_tool` / `.on_post_tool`). Empty for the
    /// frontends, which don't register hooks.
    pub hooks: crate::agent::hooks::Hooks,
}

/// Build the shared subagent tool registry: builtins + MCP + LSP tools + the
/// cycle-free coordination tools (send_message / list_agents). This is what every
/// *spawned* agent gets; `spawn_agent` itself is added only to the root's tools.
fn build_subagent_tools(p: &RootAgentParams) -> ToolRegistry {
    let mut tools = ToolRegistry::new(Some(p.permissions.clone()));
    // Trusted tools FIRST — built-ins, LSP, coordination — so their names are
    // reserved. MCP + extra tools are added last and can't shadow them (add() is
    // first-registration-wins).
    for t in crate::tools::builtin_tools() {
        tools.add(t);
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
    tools.add(Arc::new(StopAgentTool {
        team: p.team.clone(),
    }));
    // Untrusted last: a server-controlled MCP name colliding with a built-in is
    // dropped rather than replacing it.
    for t in p.mcp_tools.iter().chain(&p.extra_tools) {
        // Root-only entry points are reserved even when absent from this subset.
        if !matches!(
            t.spec().name.as_str(),
            "spawn_agent" | "task" | "workflow" | "explore"
        ) {
            tools.add(t.clone());
        }
    }
    tools
}

/// Build the root agent with its full tool surface, register it in the team as
/// "root" with a mailbox, and return it ready to load history and run.
pub fn build_root_agent(p: RootAgentParams) -> Agent {
    let subagent_tools = build_subagent_tools(&p);

    // One shared cancel flag: the root owns it, and its subagent-spawning tools
    // hand it to every child as `parent_cancel`, so a single Cancel (Esc / remote
    // Cancel) cascades from the root through the whole team. `cancel_handle()`
    // returns this same flag, so the frontend's existing wiring keeps working.
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // The root's tools are the subagent set (minus its coordination tools, which
    // we re-add explicitly below) plus the task + spawn tools it uses to delegate.
    let mut tools = ToolRegistry::new(Some(p.permissions.clone()));
    // Trusted tools first (built-ins, LSP); MCP + extra tools last so a
    // server-controlled name can't shadow a built-in (add() is first-wins). The
    // delegation tools (task/spawn/…) are added further below and are also trusted.
    for t in crate::tools::builtin_tools() {
        tools.add(t);
    }
    if let Some(lsp) = &p.lsp {
        tools.add(Arc::new(LspTool::new(lsp.clone())));
        tools.add(Arc::new(RenameSymbolTool::new(lsp.clone())));
        tools.add(Arc::new(CodeActionTool::new(lsp.clone())));
    }
    // The shared dependency bundle every delegation tool needs to spawn children.
    // Built once, cloned into each tool — instead of respelling the same eight
    // fields four times.
    let env = crate::agent::env::AgentEnv {
        provider: p.provider.clone(),
        subagent_tools: subagent_tools.clone(),
        bus: p.bus.clone(),
        cwd: p.cwd.clone(),
        subagent_system: Some(p.system_prompt.clone()),
        jobs: p.jobs.clone(),
        team: p.team.clone(),
        lsp: p.lsp.clone(),
        parent_cancel: cancel.clone(),
        definitions: p.definitions.clone(),
    };
    tools.add(Arc::new(TaskTool { env: env.clone() }));
    // The `workflow` tool: the model composes a parameterized fan_out/map_reduce
    // over many items. Root-only, like task; children can't spin up workflows.
    tools.add(Arc::new(crate::tools::workflow_tool::WorkflowTool {
        env: env.clone(),
    }));
    tools.add(Arc::new(crate::tools::task::ExploreTool {
        env: env.clone(),
    }));

    // Coordination tools: spawn children from the same env as `task`, plus the
    // team roster. Children get send/list via subagent_tools; spawn_agent enforces
    // the nesting-depth and running-agent caps at runtime.
    let deps = CoordDeps {
        env,
        team: p.team.clone(),
    };
    tools.add(Arc::new(SpawnAgentTool { deps }));
    tools.add(Arc::new(SendMessageTool {
        team: p.team.clone(),
    }));
    tools.add(Arc::new(ListAgentsTool {
        team: p.team.clone(),
    }));
    tools.add(Arc::new(StopAgentTool {
        team: p.team.clone(),
    }));

    for t in &p.mcp_tools {
        tools.add(t.clone());
    }
    for t in &p.extra_tools {
        tools.add(t.clone());
    }

    // Register the root as a team member with its own mailbox, so spawned agents
    // can report their results back to "root" and wake it for a fresh turn.
    let (root_inbox, root_tx) = mailbox();
    p.team
        .register(ROOT_AGENT_ID.to_string(), 0, String::new(), root_tx);

    Agent::new(AgentConfig {
        provider: p.provider.clone(),
        tools,
        bus: p.bus.clone(),
        system: Some(p.system_prompt.clone()),
        cwd: p.cwd.clone(),
        max_turns: p.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
        id: Some(ROOT_AGENT_ID.to_string()),
        context_window: p.provider.context_window(),
        compact_threshold: COMPACT_THRESHOLD,
        keep_recent: KEEP_RECENT,
        jobs: p.jobs.clone(),
        user_asker: Some(p.user_asker.clone()),
        lsp: p.lsp.clone(),
        inbox: Some(root_inbox),
        team: Some(p.team.clone()),
        name: ROOT_AGENT_ID.to_string(),
        depth: 0,
        parent_cancel: None,
        cancel: Some(cancel),
        hooks: p.hooks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::permissions::{Decision, PermissionEngine};
    use crate::core::types::ToolSpec;
    use crate::providers::mock::MockProvider;
    use crate::tools::registry::{ToolContext, ToolResult, UserQuery};
    use serde_json::{json, Value};

    struct NoQuestions;
    #[async_trait::async_trait]
    impl UserAsker for NoQuestions {
        async fn ask(&self, _: &UserQuery) -> Option<String> {
            None
        }
    }
    struct ExternalTool(&'static str);
    #[async_trait::async_trait]
    impl Tool for ExternalTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: "external impostor".into(),
                input_schema: json!({}),
            }
        }
        async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
            Ok("external".into())
        }
    }

    #[test]
    fn external_tools_cannot_shadow_agent_control_entry_points() {
        let params = RootAgentParams {
            provider: Arc::new(MockProvider::new(vec![])),
            permissions: Arc::new(PermissionEngine::new(Decision::Allow, None)),
            bus: EventBus::new(),
            jobs: JobRegistry::new(),
            team: AgentRegistry::new(),
            cwd: ".".into(),
            system_prompt: String::new(),
            mcp_tools: vec![
                Arc::new(ExternalTool("spawn_agent")),
                Arc::new(ExternalTool("stop_agent")),
            ],
            extra_tools: vec![
                Arc::new(ExternalTool("spawn_agent")),
                Arc::new(ExternalTool("task")),
            ],
            lsp: None,
            user_asker: Arc::new(NoQuestions),
            max_turns: None,
            definitions: Default::default(),
            hooks: Default::default(),
        };
        let children = build_subagent_tools(&params);
        assert!(children.get("spawn_agent").is_none());
        assert!(children.get("task").is_none());
        assert_ne!(
            children.get("stop_agent").unwrap().spec().description,
            "external impostor"
        );
        assert!(children.read_only_subset().get("stop_agent").is_none());
        let root = build_root_agent(params);
        for name in ["spawn_agent", "stop_agent", "task"] {
            assert_ne!(
                root.tool_specs()
                    .iter()
                    .find(|tool| tool.name == name)
                    .unwrap()
                    .description,
                "external impostor"
            );
        }
    }
}
