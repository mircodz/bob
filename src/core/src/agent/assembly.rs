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

pub struct RootAgentParams {
    pub provider: Arc<dyn Provider>,
    pub permissions: Arc<crate::core::permissions::PermissionEngine>,
    pub bus: EventBus,
    pub jobs: JobRegistry,
    pub team: AgentRegistry,
    pub cwd: String,
    pub shell: Arc<crate::core::shell::BashShell>,
    pub system_prompt: String,
    /// Configured MCP tools (already connected), namespaced `<server>.<tool>`.
    pub mcp_tools: Vec<Arc<dyn Tool>>,
    pub extra_tools: Vec<Arc<dyn Tool>>,
    pub lsp: Option<Arc<LspManager>>,
    pub user_asker: Arc<dyn UserAsker>,
    pub max_turns: Option<u32>,
    /// Caller definitions override built-in profiles.
    pub definitions: std::collections::HashMap<String, crate::agent::env::AgentDefinition>,
    pub hooks: crate::agent::hooks::Hooks,
    pub memory: Option<Arc<dyn crate::core::memory::MemoryRepository>>,
}

fn build_subagent_tools(p: &RootAgentParams) -> ToolRegistry {
    let mut tools = ToolRegistry::new(Some(p.permissions.clone()));
    // First registration wins: reserve trusted names before MCP and custom tools.
    for t in crate::tools::builtin_tools(p.shell.clone()) {
        tools.add(t);
    }
    if let Some(repository) = &p.memory {
        tools.add(Arc::new(crate::tools::memory::MemoryTool {
            repository: repository.clone(),
        }));
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
    for t in p.mcp_tools.iter().chain(&p.extra_tools) {
        // Root-only entry points are reserved even when absent from this subset.
        if !matches!(
            t.spec().name.as_str(),
            "spawn_agent" | "task" | "workflow" | "explore" | "memory"
        ) {
            tools.add(t.clone());
        }
    }
    tools
}

pub fn build_root_agent(p: RootAgentParams) -> Agent {
    let subagent_tools = build_subagent_tools(&p);

    // Children share the root's flag as parent_cancel; interruption cascades.
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let mut tools = ToolRegistry::new(Some(p.permissions.clone()));
    // Reserve trusted names, including delegation tools, before untrusted tools.
    for t in crate::tools::builtin_tools(p.shell.clone()) {
        tools.add(t);
    }
    if let Some(repository) = &p.memory {
        tools.add(Arc::new(crate::tools::memory::MemoryTool {
            repository: repository.clone(),
        }));
    }
    if let Some(lsp) = &p.lsp {
        tools.add(Arc::new(LspTool::new(lsp.clone())));
        tools.add(Arc::new(RenameSymbolTool::new(lsp.clone())));
        tools.add(Arc::new(CodeActionTool::new(lsp.clone())));
    }
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
        definitions: crate::agent::profiles::with_builtins(&p.system_prompt, p.definitions.clone()),
        hooks: p.hooks.clone(),
        memory: p.memory.clone(),
    };
    tools.add(Arc::new(TaskTool { env: env.clone() }));
    tools.add(Arc::new(crate::tools::workflow_tool::WorkflowTool {
        env: env.clone(),
    }));
    tools.add(Arc::new(crate::tools::task::ExploreTool {
        env: env.clone(),
    }));

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
        if t.spec().name != "memory" {
            tools.add(t.clone());
        }
    }
    for t in &p.extra_tools {
        if t.spec().name != "memory" {
            tools.add(t.clone());
        }
    }

    // The root needs a mailbox to receive child reports and wake for them.
    let (root_inbox, root_tx) = mailbox();
    p.team
        .register(ROOT_AGENT_ID.to_string(), 0, String::new(), root_tx);

    let mut agent = Agent::new(AgentConfig {
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
    });
    agent.set_memory_repository(p.memory);
    agent
}
