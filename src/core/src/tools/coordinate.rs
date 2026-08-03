//! Coordination tools: the surface a team agent uses to spawn, message, and
//! inspect other agents. These turn bob's fire-and-forget `task` into a live,
//! addressable team.
//!
//! - `spawn_agent`: start a named subagent in the background with its own inbox,
//!   registered in the shared team. It runs concurrently; its result is delivered
//!   back to the spawner as a message when it finishes.
//! - `send_message`: deliver a message into another agent's inbox (steer it, hand
//!   it work, or reply). The recipient sees it at its next turn boundary.
//! - `list_agents`: the team roster + each member's status.
//!
//! A spawned agent's result is not returned by the spawn call — it's delivered
//! back as a message when the agent finishes, and the spawner's run loop is woken
//! for a fresh turn to process it (see `Agent::has_pending_coordination`). So the
//! spawner keeps control and can do other work while children run, with no
//! blocking primitive that could deadlock.

use crate::agent::agent::{Agent, AgentConfig};
use crate::agent::team::{mailbox, AgentRegistry, AgentStatus};
use crate::core::events::EventBus;
use crate::core::types::ToolSpec;
use crate::providers::provider::Provider;
use crate::tools::jobs::JobRegistry;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Shared configuration for building coordinated child agents — mirrors the
/// pieces `TaskTool` uses to assemble a subagent, plus the team registry.
#[derive(Clone)]
pub struct CoordDeps {
    pub provider: Arc<dyn Provider>,
    pub subagent_tools: ToolRegistry,
    pub bus: EventBus,
    pub cwd: String,
    pub subagent_system: Option<String>,
    pub jobs: JobRegistry,
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    pub team: AgentRegistry,
}

/// `spawn_agent`: start a named background subagent that is part of the team.
pub struct SpawnAgentTool {
    pub deps: CoordDeps,
}

#[async_trait]
impl Tool for SpawnAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "spawn_agent".to_string(),
            description: "Start a named subagent that runs in the background and is part of your \
                team. Unlike `task` (fire-and-forget), a spawned agent is addressable: you can \
                `send_message` to steer it while it works, and it can message you back. \
                \n\nThe agent does NOT share your context — it starts blank. So the `task` prompt \
                MUST be specific and self-contained: name the exact files/dirs to look at, state \
                the concrete deliverable, and demand real findings (e.g. `review src/core/src/tools/ \
                for error-handling gaps; report each as file:line + the problem + a fix`). A vague \
                prompt like \"review the code for style\" yields a useless meta-answer about \
                methodology — be concrete or the agent will waste the turn. \
                \n\nIMPORTANT: the result is NOT returned by this call — it arrives LATER as a \
                message (`[message from <name>]: finished: …`). Do NOT use job_status/job_output for \
                spawned agents. When you spawn several, WAIT for ALL of them to report back, then \
                write ONE synthesized summary for the user (grouped/cross-referenced) — do not dump \
                a separate paragraph per agent as each trickles in. Use `task` instead for simple \
                independent fan-out you don't need to coordinate with."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Short handle for the agent (e.g. \"researcher\")." },
                    "description": { "type": "string", "description": "A 3-5 word summary of what this agent does (e.g. \"review src/core\"), shown in the UI. NOT the full task." },
                    "task": { "type": "string", "description": "Complete, self-contained instructions: exact files/scope + the concrete deliverable + demand for specific findings. The agent has none of your context." }
                },
                "required": ["name", "task"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let coord = match &ctx.coord {
            Some(c) => c,
            None => {
                return Err(ToolError::unavailable(
                    "coordination is not available in this context",
                ))
            }
        };
        let name = input["name"].as_str().unwrap_or("").trim().to_string();
        let task = input["task"].as_str().unwrap_or("").to_string();
        if name.is_empty() {
            return Err(ToolError::invalid_input("name is required"));
        }
        if task.is_empty() {
            return Err(ToolError::invalid_input("task is required"));
        }
        if coord.team.name_in_use(&name) {
            return Err(ToolError::invalid_input(format!(
                "an agent named '{}' is already running",
                name
            )));
        }
        let child_depth = coord.depth + 1;
        if child_depth > crate::agent::team::MAX_SPAWN_DEPTH {
            return Err(ToolError::invalid_input(format!(
                "spawn nesting too deep (max {} levels); do this work directly instead \
                 of spawning another agent",
                crate::agent::team::MAX_SPAWN_DEPTH
            )));
        }
        if coord.team.active_len() >= crate::agent::team::MAX_TEAM_SIZE {
            return Err(ToolError::invalid_input(format!(
                "team is at the maximum of {} running agents; wait for some to finish",
                crate::agent::team::MAX_TEAM_SIZE
            )));
        }

        // Build the child's mailbox and register it in the team before spawning,
        // so a sibling can message it immediately.
        let (inbox, tx) = mailbox();
        coord
            .team
            .register(name.clone(), child_depth, coord.name.clone(), tx);

        // Announce the spawn so the UI shows a subagent cell (same signal the
        // `task` tool emits). Prefer the short `description` for the label; fall
        // back to the full task if the model didn't give one. The child's inner
        // tool calls stay hidden; only this notice + its status appear.
        let label = input["description"]
            .as_str()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(&task)
            .to_string();
        self.deps
            .bus
            .emit(crate::core::events::AgentEvent::SubagentSpawn {
                parent_id: coord.name.clone(),
                agent_id: name.clone(),
                task: label,
                prompt: task.clone(),
            });

        // The child gets the same tool set PLUS the coordination tools, so nesting
        // works: a spawned agent can itself spawn/message/inspect. We add them at
        // spawn time (rather than baking them into subagent_tools up front) to
        // avoid a self-referential cycle — SpawnAgentTool would otherwise need to
        // contain a copy of itself.
        let mut child_tools = self.deps.subagent_tools.clone();
        child_tools.add(Arc::new(SpawnAgentTool {
            deps: self.deps.clone(),
        }));
        child_tools.add(Arc::new(SendMessageTool {
            team: self.deps.team.clone(),
        }));
        child_tools.add(Arc::new(ListAgentsTool {
            team: self.deps.team.clone(),
        }));

        // Assemble the child agent, itself a team member (so it can coordinate).
        let child = Agent::new(AgentConfig {
            provider: self.deps.provider.clone(),
            tools: child_tools,
            bus: self.deps.bus.clone(),
            system: self.deps.subagent_system.clone(),
            cwd: if self.deps.cwd.is_empty() {
                ctx.cwd.clone()
            } else {
                self.deps.cwd.clone()
            },
            max_turns: 100,
            id: Some(name.clone()),
            context_window: 200_000,
            compact_threshold: 0.8,
            keep_recent: 6,
            jobs: self.deps.jobs.clone(),
            user_asker: None,
            lsp: self.deps.lsp.clone(),
            inbox: Some(inbox),
            team: Some(self.deps.team.clone()),
            name: name.clone(),
            depth: child_depth,
        });

        // Run in the background. On completion, deliver the result back to the
        // spawner as a message and mark the child's status.
        let team = coord.team.clone();
        let spawner = coord.name.clone();
        let child_name = name.clone();
        let bus = self.deps.bus.clone();
        tokio::spawn(async move {
            let mut child = child;
            let (status, result) = match child.run(&task).await {
                Ok(out) => (AgentStatus::Done, out),
                Err(e) => (AgentStatus::Failed, format!("error: {}", e)),
            };
            let failed = status == AgentStatus::Failed;
            bus.emit(crate::core::events::AgentEvent::SubagentDone {
                agent_id: child_name.clone(),
                failed,
            });
            if let Some(h) = team.get(&child_name) {
                h.set_status(status);
            }
            // Report back to whoever spawned it.
            team.send(&spawner, &child_name, &format!("finished: {}", result));
        });

        Ok(format!(
            "spawned agent '{}'. It runs in the background; its result will arrive as a message \
             from it. Use send_message to steer it or list_agents to check status.",
            name
        ))
    }
}

/// `send_message`: deliver a message into another agent's inbox.
pub struct SendMessageTool {
    pub team: AgentRegistry,
}

#[async_trait]
impl Tool for SendMessageTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "send_message".to_string(),
            description: "Send a message to another agent in your team by name (e.g. one you \
                spawned, or your spawner via its name). The recipient sees it at its next step and \
                can act on or reply to it. Use it to steer a running agent, hand it more work, or \
                answer its question."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Name of the recipient agent." },
                    "message": { "type": "string", "description": "The message text." }
                },
                "required": ["to", "message"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let from = match &ctx.coord {
            Some(c) => c.name.clone(),
            None => {
                return Err(ToolError::unavailable(
                    "coordination is not available in this context",
                ))
            }
        };
        let to = input["to"].as_str().unwrap_or("").trim();
        let message = input["message"].as_str().unwrap_or("");
        if to.is_empty() || message.is_empty() {
            return Err(ToolError::invalid_input(
                "both 'to' and 'message' are required",
            ));
        }
        if self.team.send(to, &from, message) {
            Ok(format!("delivered to '{}'", to))
        } else {
            Err(ToolError::not_found(format!(
                "no agent named '{}' (it may have finished)",
                to
            )))
        }
    }
}

/// `list_agents`: the team roster + each member's status.
pub struct ListAgentsTool {
    pub team: AgentRegistry,
}

#[async_trait]
impl Tool for ListAgentsTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_agents".to_string(),
            description:
                "List the agents in your team and their status (running / done / failed), \
                so you can coordinate — e.g. see whether a spawned agent has finished before \
                relying on its result."
                    .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
        let roster = self.team.roster();
        if roster.is_empty() {
            return Ok("no agents in the team yet.".to_string());
        }
        let mut out = String::from("team:\n");
        for (name, depth, status) in roster {
            let s = match status {
                AgentStatus::Running => "running",
                AgentStatus::Done => "done",
                AgentStatus::Failed => "failed",
            };
            out.push_str(&format!("  {} (depth {}) — {}\n", name, depth, s));
        }
        Ok(out)
    }
}
