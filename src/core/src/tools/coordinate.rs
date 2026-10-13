//! Spawned results arrive by message and wake the parent; the spawn call never
//! waits for completion.

use crate::agent::agent::{build_subagent, RunStopReason, SubagentSpec, SUBAGENT_MAX_TURNS};
use crate::agent::env::AgentEnv;
use crate::agent::lifecycle::SubagentLifecycle;
use crate::agent::team::{mailbox, AgentRegistry, AgentStatus};
use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct CoordDeps {
    pub env: AgentEnv,
    pub team: AgentRegistry,
}

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

        // Register before spawning so siblings can message it immediately.
        let (inbox, tx) = mailbox();
        let handle = coord
            .team
            .try_register(name.clone(), child_depth, coord.name.clone(), tx)
            .map_err(ToolError::invalid_input)?;

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
        .with_team(coord.team.clone())
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
            let mut env = self.deps.env.clone();
            env.subagent_tools = child_tools.clone();
            env.subagent_system = child_system.clone();
            child_tools.add(Arc::new(SpawnAgentTool {
                deps: CoordDeps {
                    env,
                    team: self.deps.team.clone(),
                },
            }));
        }

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
            hooks: self.deps.env.hooks.clone(),
            memory: self.deps.env.memory.clone(),
        });

        let parent_cancel = self.deps.env.parent_cancel.clone();
        tokio::spawn(async move {
            let mut child = child;
            let child_cancel = child.cancel_handle();
            let driver = async {
                let mut remaining = child.turn_budget();
                let mut result = child.run_with_budget(&task, remaining).await?;
                remaining = remaining.saturating_sub(result.turns);
                loop {
                    if result.stop_reason != RunStopReason::Completed {
                        break;
                    }
                    if !child.has_outstanding_coordination() && child.try_close_inbox() {
                        break;
                    }
                    if remaining == 0 || child.has_pending_coordination() {
                        let next = child.run_with_budget("", remaining).await?;
                        remaining = remaining.saturating_sub(next.turns);
                        let previous = result.text;
                        result = next;
                        if remaining == 0 && result.turns == 0 && !previous.is_empty() {
                            result.text = format!("{previous}\n\n{}", result.text);
                        }
                    } else {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
                result.into_completed_text()
            };
            let outcome = crate::core::panic::catch(handle.run_until_stopped(
                parent_cancel,
                child_cancel,
                driver,
            ))
            .await;
            if !child.settle_owned_mutations().await {
                child.bus().emit(crate::core::events::AgentEvent::Error {
                    agent_id: child.id.clone(),
                    message: "memory mutation cleanup timed out; changes may still complete".into(),
                });
            }
            let cancelled = matches!(outcome, Ok(None)) || child.is_cancelled();
            let (failed, result) = match outcome {
                Ok(Some(Ok(out))) => (false, out),
                Ok(Some(Err(e))) => (true, format!("error: {e}")),
                Ok(None) => (false, "[cancelled]".into()),
                Err(_) => (
                    true,
                    "error: agent panicked; outcome unknown, inspect changes before retrying"
                        .into(),
                ),
            };
            lifecycle.finish_with_output(failed && !cancelled, cancelled, &result);
        });

        Ok(format!(
            "spawned agent '{}'. It runs in the background; its result will arrive as a message \
             from it. Use send_message to steer it or list_agents to check status.",
            name
        )
        .into())
    }
}

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
            Ok(format!("delivered to '{}'", to).into())
        } else {
            Err(ToolError::not_found(format!(
                "no agent named '{}' (it may have finished)",
                to
            )))
        }
    }
}

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
        Ok((if count == 0 {
            format!("cancellation already requested for '{name}'")
        } else {
            format!("cancellation requested for '{name}' and its running descendants ({count} agent(s))")
        }).into())
    }
}

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
            return Ok("no agents in the team yet.".into());
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
        Ok(out.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::team::AgentMessageKind;
    use crate::tools::registry::{CoordContext, ToolErrorKind};

    fn context(team: &AgentRegistry, caller: &str) -> ToolContext {
        ToolContext {
            cwd: ".".into(),
            files: Arc::new(crate::tools::file_tracker::FileTracker::new()),
            todos: Arc::new(crate::tools::todo::TodoStore::new()),
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: None,
            coord: Some(CoordContext {
                name: caller.into(),
                depth: 0,
                team: team.clone(),
            }),
            permissions: None,
        }
    }

    #[tokio::test]
    async fn send_message_preserves_body_sender_and_target_isolation() {
        let team = AgentRegistry::new();
        let (mut inbox, tx) = mailbox();
        team.register("worker".into(), 1, "root".into(), tx);
        let (mut sibling, tx) = mailbox();
        team.register("sibling".into(), 1, "root".into(), tx);
        let tool = SendMessageTool { team: team.clone() };
        let ctx = context(&team, "root");
        for body in [
            "finished: this is steering, not a completion",
            "  keep spacing\n界🙂  ",
        ] {
            assert_eq!(
                tool.execute(json!({"to":" worker ", "message":body}), &ctx)
                    .await
                    .unwrap()
                    .as_text()
                    .unwrap(),
                "delivered to 'worker'"
            );
            let messages = inbox.drain();
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0].from, "root");
            assert_eq!(messages[0].text, body);
            assert_eq!(messages[0].kind, AgentMessageKind::Actionable);
            assert!(sibling.drain().is_empty());
        }
        assert_eq!(team.get("worker").unwrap().status(), AgentStatus::Running);
    }

    #[tokio::test]
    async fn send_message_rejects_invalid_unavailable_and_terminal_recipients() {
        let team = AgentRegistry::new();
        let (mut inbox, tx) = mailbox();
        team.register("worker".into(), 1, "root".into(), tx);
        let tool = SendMessageTool { team: team.clone() };
        let mut ctx = context(&team, "root");
        for input in [
            json!({}),
            json!({"to":" ", "message":"hello"}),
            json!({"to":"worker", "message":""}),
            json!({"to":"worker", "message":42}),
        ] {
            assert_eq!(
                tool.execute(input, &ctx).await.unwrap_err().kind,
                ToolErrorKind::InvalidInput
            );
            assert!(inbox.drain().is_empty());
        }
        ctx.coord = None;
        assert_eq!(
            tool.execute(json!({"to":"worker", "message":"hello"}), &ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::Unavailable
        );
        assert!(inbox.drain().is_empty());
        ctx = context(&team, "root");
        assert_eq!(
            tool.execute(json!({"to":"missing", "message":"hello"}), &ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::NotFound
        );
        team.stop("root", "worker").unwrap();
        assert_eq!(
            tool.execute(json!({"to":"worker", "message":"too late"}), &ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::NotFound
        );
        assert!(inbox.drain().is_empty());
        for status in [
            AgentStatus::Done,
            AgentStatus::Failed,
            AgentStatus::Cancelled,
        ] {
            let team = AgentRegistry::new();
            let (mut inbox, tx) = mailbox();
            let handle = team.register("finished".into(), 1, "root".into(), tx);
            handle.set_status(status.clone());
            let tool = SendMessageTool { team: team.clone() };
            assert_eq!(
                tool.execute(
                    json!({"to":"finished", "message":"too late"}),
                    &context(&team, "root")
                )
                .await
                .unwrap_err()
                .kind,
                ToolErrorKind::NotFound
            );
            assert!(inbox.drain().is_empty());
            assert_eq!(handle.status(), status);
        }
    }

    #[tokio::test]
    async fn stop_agent_enforces_caller_scope_and_is_idempotent_before_settlement() {
        let team = AgentRegistry::new();
        for (name, depth, parent) in [
            ("root", 0, ""),
            ("parent", 1, "root"),
            ("child", 2, "parent"),
            ("grandchild", 3, "child"),
            ("sibling", 1, "root"),
        ] {
            let (_, tx) = mailbox();
            team.register(name.into(), depth, parent.into(), tx);
        }
        let tool = StopAgentTool { team: team.clone() };
        let ctx = context(&team, "parent");
        for name in ["root", "parent", "sibling", "missing", " "] {
            let error = tool.execute(json!({"name":name}), &ctx).await.unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::InvalidInput);
            assert!(team
                .roster()
                .iter()
                .all(|(name, _, _)| !team.get(name).unwrap().cancellation_requested()));
        }
        let mut unavailable = ctx.clone();
        unavailable.coord = None;
        assert_eq!(
            tool.execute(json!({"name":"child"}), &unavailable)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::Unavailable
        );
        assert_eq!(
            tool.execute(json!({"name":" child "}), &ctx)
                .await
                .unwrap()
                .as_text()
                .unwrap(),
            "cancellation requested for 'child' and its running descendants (2 agent(s))"
        );
        assert_eq!(
            tool.execute(json!({"name":"child"}), &ctx)
                .await
                .unwrap()
                .as_text()
                .unwrap(),
            "cancellation already requested for 'child'"
        );
        for name in ["child", "grandchild"] {
            let handle = team.get(name).unwrap();
            assert!(handle.cancellation_requested());
            assert_eq!(handle.status(), AgentStatus::Running);
            assert_eq!(handle.complete(false, false, None), AgentStatus::Cancelled);
        }
        for name in ["root", "parent", "sibling"] {
            assert!(!team.get(name).unwrap().cancellation_requested());
        }
        assert_eq!(
            tool.execute(json!({"name":"child"}), &ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::InvalidInput
        );
    }

    #[tokio::test]
    async fn list_agents_reports_all_terminal_states_without_mutation() {
        let team = AgentRegistry::new();
        let tool = ListAgentsTool { team: team.clone() };
        let ctx = context(&team, "root");
        assert!(tool.is_read_only());
        assert_eq!(
            tool.execute(json!({}), &ctx)
                .await
                .unwrap()
                .as_text()
                .unwrap(),
            "no agents in the team yet."
        );
        for (name, status) in [
            ("running", AgentStatus::Running),
            ("done", AgentStatus::Done),
            ("failed", AgentStatus::Failed),
            ("cancelled", AgentStatus::Cancelled),
        ] {
            let (_, tx) = mailbox();
            team.register(name.into(), 2, "root".into(), tx)
                .set_status(status);
        }
        let before = team.roster();
        let output = tool.execute(json!({}), &ctx).await.unwrap();
        let output = output.as_text().unwrap();
        assert_eq!(output.lines().count(), 5);
        assert!(output.starts_with("team:\n"));
        for name in ["running", "done", "failed", "cancelled"] {
            assert!(output
                .lines()
                .any(|line| line == format!("  {name} (depth 2) — {name}")));
        }
        assert_eq!(team.roster(), before);
    }
}
