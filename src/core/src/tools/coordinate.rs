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

use crate::agent::agent::{build_subagent, SubagentSpec, SUBAGENT_MAX_TURNS};
use crate::agent::env::AgentEnv;
use crate::agent::lifecycle::SubagentLifecycle;
use crate::agent::team::{mailbox, AgentRegistry, AgentStatus};
use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Shared configuration for building coordinated child agents — mirrors the
/// pieces `TaskTool` uses to assemble a subagent, plus the team registry.
#[derive(Clone)]
pub struct CoordDeps {
    /// The shared subagent-spawning environment (provider, tools, bus, cwd,
    /// system, jobs, lsp, cancel) — the same bundle `task`/`workflow`/`explore`
    /// hold.
    pub env: AgentEnv,
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
            description: format!(
                "Start a named background collaborator for work that benefits from ongoing \
                coordination. Prefer direct tools for small tasks and task for an independent \
                batch whose results you need together. Supply a self-contained task with the \
                necessary facts, ownership boundaries, and expected result; the agent does not \
                inherit your conversation. Set read_only:true for investigation or review. \
                This call returns only a launch confirmation. The final result arrives later \
                as a message from the agent; do not respawn it because the result is absent \
                here, and do not use job_status/job_output for this agent. Use send_message \
                to steer it, stop_agent to cancel it, or list_agents to inspect state when needed. \
                Combine the required results before answering, accounting for failures.{}",
                self.deps.env.definitions_help()
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Short handle for the agent (e.g. \"researcher\")." },
                    "description": { "type": "string", "description": "A 3-5 word summary of what this agent does (e.g. \"review src/core\"), shown in the UI. NOT the full task." },
                    "task": { "type": "string", "description": "Self-contained instructions: necessary facts, scope, ownership boundaries, and expected result. The agent does not inherit your conversation." },
                    "subagent_type": { "type": "string", "description": "Optional: the name of a predefined agent type to run (see the list in this tool's description). Uses that definition's specialized prompt + tools." },
                    "read_only": { "type": "boolean", "description": "Confine the agent to read-only tools (reads/searches, no write/edit/bash). Use for audits/analysis that must not mutate files (default false)." }
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

        // Resolve before publishing a running child: an invalid type must not
        // leave a phantom agent blocking its parent's completion wake.
        let subagent_type = input["subagent_type"].as_str();
        let read_only = input["read_only"].as_bool().unwrap_or(false);
        let (child_system, mut child_tools) = self
            .deps
            .env
            .resolve_child(subagent_type, read_only)
            .map_err(ToolError::invalid_input)?;

        // Build the child's mailbox and register it in the team before spawning,
        // so a sibling can message it immediately.
        let (inbox, tx) = mailbox();
        let handle = coord
            .team
            .try_register(name.clone(), child_depth, coord.name.clone(), tx)
            .map_err(ToolError::invalid_input)?;

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
        let mut lifecycle = SubagentLifecycle::start(
            self.deps.env.bus.clone(),
            coord.name.clone(),
            name.clone(),
            label,
            task.clone(),
        )
        .with_handle(handle.clone())
        .with_report_to(coord.team.get(&coord.name));

        // Recursive spawning is added here to avoid a registry cycle, but never
        // outside the caller's read-only stance or the definition's tool allowlist.
        let definition = subagent_type.and_then(|name| self.deps.env.definitions.get(name));
        let may_spawn = !read_only
            && definition.is_none_or(|def| {
                !def.read_only
                    && def
                        .tools
                        .as_ref()
                        .is_none_or(|tools| tools.iter().any(|name| name == "spawn_agent"))
            });
        if may_spawn {
            child_tools.add(Arc::new(SpawnAgentTool {
                deps: self.deps.clone(),
            }));
        }

        // Assemble the child agent, itself a team member (so it can coordinate).
        let child = build_subagent(SubagentSpec {
            provider: self.deps.env.provider.clone(),
            tools: child_tools,
            bus: self.deps.env.bus.clone(),
            system: child_system,
            cwd: if self.deps.env.cwd.is_empty() {
                ctx.cwd.clone()
            } else {
                self.deps.env.cwd.clone()
            },
            jobs: self.deps.env.jobs.clone(),
            lsp: self.deps.env.lsp.clone(),
            name: name.clone(),
            max_turns: SUBAGENT_MAX_TURNS,
            depth: child_depth,
            inbox: Some(inbox),
            team: Some(self.deps.team.clone()),
            parent_cancel: Some(self.deps.env.parent_cancel.clone()),
        });

        // Run in the background. On completion, deliver the result back to the
        // spawner as a message and mark the child's status.
        let parent_cancel = self.deps.env.parent_cancel.clone();
        tokio::spawn(async move {
            let mut child = child;
            let driver = async {
                let mut result = child.run(&task).await?;
                while child.has_outstanding_coordination() {
                    if child.has_pending_coordination() {
                        result = child.run("").await?;
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                Ok::<_, anyhow::Error>(result)
            };
            let outcome = handle.run_until_stopped(parent_cancel, driver).await;
            let cancelled = outcome.is_none() || child.is_cancelled();
            let (failed, result) = match outcome {
                Some(Ok(out)) => (false, out),
                Some(Err(e)) => (true, format!("error: {e}")),
                None => (false, "[cancelled]".into()),
            };
            lifecycle.finish_with_output(failed && !cancelled, cancelled, &result);
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
            description: "Send a message to a running agent by its name or ID from list_agents \
                (including task, explore, and workflow agents). It receives the message at its \
                next model step. Finished or stopping agents cannot receive messages; this \
                does not restart them. Use this to steer work, provide context, or reply."
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

/// Cancel a selected execution without stopping the main conversation or siblings.
pub struct StopAgentTool {
    pub team: AgentRegistry,
}

#[async_trait]
impl Tool for StopAgentTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "stop_agent".into(),
            description: "Request cancellation of a running agent by its name or ID from \
                list_agents, including its descendants. The root may stop any child; other \
                agents may stop only their own descendants. Cannot stop the root or restart \
                finished agents. The cancelled transcript stays inspectable. This stops agent \
                execution, not already-completed edits, remote side effects, or detached shell jobs.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string", "description": "Name or ID of the agent to stop."}},
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let caller = ctx
            .coord
            .as_ref()
            .ok_or_else(|| ToolError::unavailable("agent controls are unavailable"))?;
        let name = input["name"].as_str().unwrap_or("").trim();
        if name.is_empty() {
            return Err(ToolError::invalid_input("name is required"));
        }
        let count = self
            .team
            .stop(&caller.name, name)
            .map_err(ToolError::invalid_input)?;
        Ok(if count == 0 {
            format!("cancellation already requested for '{name}'")
        } else {
            format!("cancellation requested for '{name}' and its running descendants ({count} agent(s))")
        })
    }
}

/// `list_agents`: the team roster + each member's status.
pub struct ListAgentsTool {
    pub team: AgentRegistry,
}

#[async_trait]
impl Tool for ListAgentsTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_agents".to_string(),
            description:
                "List all agents by name/ID and state (running / done / failed / cancelled), \
                including inline/background tasks, explore agents, and workflow children. \
                Use these IDs with send_message or stop_agent."
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
                AgentStatus::Cancelled => "cancelled",
            };
            out.push_str(&format!("  {} (depth {}) — {}\n", name, depth, s));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{AgentEvent, EventBus};
    use crate::providers::mock::MockProvider;
    use crate::tools::file_tracker::FileTracker;
    use crate::tools::jobs::JobRegistry;
    use crate::tools::registry::{CoordContext, ToolRegistry};
    use crate::tools::todo::TodoStore;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test]
    async fn unknown_agent_type_does_not_leave_a_running_child() {
        let team = AgentRegistry::new();
        let bus = EventBus::new();
        let spawns = Arc::new(AtomicUsize::new(0));
        let count = spawns.clone();
        bus.on(Arc::new(move |event| {
            if matches!(event, AgentEvent::SubagentSpawn { .. }) {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }));
        let jobs = JobRegistry::new();
        let tool = SpawnAgentTool {
            deps: CoordDeps {
                env: AgentEnv {
                    provider: Arc::new(MockProvider::new(vec![])),
                    subagent_tools: ToolRegistry::new(None),
                    bus,
                    cwd: ".".into(),
                    subagent_system: None,
                    jobs: jobs.clone(),
                    team: team.clone(),
                    lsp: None,
                    parent_cancel: Arc::new(AtomicBool::new(false)),
                    definitions: Default::default(),
                },
                team: team.clone(),
            },
        };
        let ctx = ToolContext {
            cwd: ".".into(),
            files: Arc::new(FileTracker::new()),
            todos: Arc::new(TodoStore::new()),
            jobs,
            user_asker: None,
            lsp: None,
            coord: Some(CoordContext {
                name: "root".into(),
                depth: 0,
                team: team.clone(),
            }),
            permissions: None,
        };
        let result = tool
            .execute(
                json!({
                    "name": "reviewer",
                    "task": "review the code",
                    "subagent_type": "missing"
                }),
                &ctx,
            )
            .await;
        assert!(result.is_err());
        assert!(team.roster().is_empty());
        assert!(!team.has_running_children("root"));
        assert_eq!(spawns.load(Ordering::Relaxed), 0);
    }
}
