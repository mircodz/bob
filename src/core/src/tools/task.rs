//! The `task` tool lets the MODEL spawn subagents — parallelism becomes a
//! decision the model can make. By default subagents run inline (blocking) and
//! their answers are returned together. With `background: true` each task is
//! detached as a background *job*; the tool returns immediately with job ids the
//! model later polls via job_status / job_output.

use crate::agent::agent::{
    build_subagent, Agent, SubagentSpec, EXPLORE_MAX_TURNS, ROOT_AGENT_ID, SUBAGENT_MAX_TURNS,
};
use crate::agent::env::AgentEnv;
use crate::agent::lifecycle::SubagentLifecycle;
use crate::agent::team::{mailbox, AgentHandle, AgentStatus};
use crate::core::types::ToolSpec;
use crate::tools::jobs::JobStatus;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide monotonic counter for subagent ids. Per-batch `task_{i+1}` ids
/// collided across successive `task` calls — the team drawer keys threads by id,
/// so batch B's `task_1` silently overwrote batch A's finished `task_1` thread
/// (spawned agents "didn't show up"). A global counter makes every spawn unique.
static SUBAGENT_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_subagent_id() -> String {
    format!("task_{}", SUBAGENT_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Reserve a restored task id so subsequent launches retain separate transcripts.
pub fn reserve_subagent_id(id: &str) {
    if let Some(next) = id
        .strip_prefix("task_")
        .and_then(|n| n.parse::<u64>().ok())
        .and_then(|n| n.checked_add(1))
    {
        SUBAGENT_SEQ.fetch_max(next, Ordering::Relaxed);
    }
}

pub struct TaskTool {
    pub env: AgentEnv,
}

impl TaskTool {
    /// Build a subagent for one task and return the future that runs it.
    /// `subagent_type` selects a named definition (its prompt + tools); `read_only`
    /// otherwise confines the child to the read-only tool subset. Returns an error
    /// if `subagent_type` names an unknown definition.
    fn make_child(
        &self,
        id: String,
        cwd: String,
        subagent_type: Option<&str>,
        read_only: bool,
        parent: String,
    ) -> Result<(Agent, AgentHandle), String> {
        let (system, tools) = self.env.resolve_child(subagent_type, read_only)?;
        let (inbox, tx) = mailbox();
        let handle = self.env.team.register(id.clone(), 1, parent, tx);
        let child = build_subagent(SubagentSpec {
            provider: self.env.provider.clone(),
            tools,
            bus: self.env.bus.clone(),
            system,
            cwd,
            jobs: self.env.jobs.clone(),
            lsp: self.env.lsp.clone(),
            name: id,
            max_turns: SUBAGENT_MAX_TURNS,
            depth: 1,
            inbox: Some(inbox),
            team: Some(self.env.team.clone()),
            parent_cancel: Some(self.env.parent_cancel.clone()),
        });
        Ok((child, handle))
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task".to_string(),
            description: format!(
                "Run independent tasks in fresh agents and return their results together. \
                Use direct tools for small lookups; use explore for substantial read-only \
                investigation. Agents do not inherit your conversation: provide the necessary \
                facts, exact scope, and expected deliverable. Give parallel editors non-overlapping \
                file ownership and leave workspace-wide verification to one owner. \
                By default this call waits for every result. Set background:true only when \
                useful independent work can continue; it returns job IDs for later collection \
                with job_output. Do not detach merely to poll or run sleeps. \
                Live agents can receive send_message or stop_agent using IDs from list_agents. \
                Task agents cannot spawn further agents. For research or review, set read_only:true \
                to exclude shell and editing tools. subagent_type selects a listed definition.{}",
                self.env.definitions_help()
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "description": { "type": "string", "description": "A 3-5 word label for the sub-task (e.g. \"review src/core\"), shown in the UI. NOT the full instructions." },
                                "prompt": { "type": "string", "description": "Full instructions for the subagent." },
                                "subagent_type": { "type": "string", "description": "Optional: name of a predefined agent type to run this sub-task as (see the list in this tool's description). Uses that definition's prompt + tools." }
                            },
                            "required": ["description", "prompt"]
                        }
                    },
                    "background": { "type": "boolean", "description": "Run detached as background jobs (default false)." },
                    "read_only": { "type": "boolean", "description": "Confine every subagent to read-only tools (reads/searches, no write/edit/bash). Use for audits/analysis that must not mutate files (default false)." }
                },
                "required": ["tasks"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let tasks = match input["tasks"].as_array() {
            Some(t) if !t.is_empty() => t.clone(),
            _ => return Err(ToolError::invalid_input("no tasks provided")),
        };
        let background = input["background"].as_bool().unwrap_or(false);
        let read_only = input["read_only"].as_bool().unwrap_or(false);
        let parent = ctx
            .coord
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| ROOT_AGENT_ID.into());
        let base_cwd = if self.env.cwd.is_empty() {
            ctx.cwd.clone()
        } else {
            self.env.cwd.clone()
        };

        if background {
            // Detach each task as a background job; return the ids immediately.
            let mut ids = Vec::new();
            for t in tasks {
                let description = t["description"].as_str().unwrap_or("").to_string();
                let prompt = t["prompt"].as_str().unwrap_or("").to_string();
                let job_id = ctx.jobs.next_id();
                let (child, handle) = self
                    .make_child(
                        job_id.clone(),
                        base_cwd.clone(),
                        t["subagent_type"].as_str(),
                        read_only,
                        parent.clone(),
                    )
                    .map_err(ToolError::invalid_input)?;
                let mut lifecycle = SubagentLifecycle::start(
                    self.env.bus.clone(),
                    ctx.coord
                        .as_ref()
                        .map(|c| c.name.clone())
                        .unwrap_or_else(|| ROOT_AGENT_ID.into()),
                    job_id.clone(),
                    description.clone(),
                    prompt.clone(),
                )
                .with_handle(handle.clone());
                let parent_cancel = self.env.parent_cancel.clone();
                let work = async move {
                    let mut child = child;
                    let outcome = handle
                        .run_until_stopped(parent_cancel, child.run(&prompt))
                        .await;
                    let cancelled = outcome.is_none() || child.is_cancelled();
                    let (failed, output) = match outcome {
                        Some(Ok(out)) => (false, out),
                        Some(Err(e)) => (true, format!("error: {e}")),
                        None => (false, "[cancelled]".into()),
                    };
                    let status = match lifecycle.finish(failed && !cancelled, cancelled) {
                        AgentStatus::Cancelled => JobStatus::Cancelled,
                        AgentStatus::Failed => JobStatus::Failed,
                        _ => JobStatus::Done,
                    };
                    (status, output)
                };
                ctx.jobs
                    .spawn(job_id.clone(), "task", description.clone(), work);
                ids.push(format!("{} ({})", job_id, description));
            }
            return Ok(format!(
                "started {} background job(s): {}\nUse job_status / job_output to check on them.",
                ids.len(),
                ids.join(", ")
            ));
        }

        // Inline (blocking) — run all subagents concurrently and join.
        let mut handles = Vec::new();
        for t in tasks.into_iter() {
            let description = t["description"].as_str().unwrap_or("").to_string();
            let prompt = t["prompt"].as_str().unwrap_or("").to_string();
            let id = next_subagent_id();
            // Build the child FIRST — `make_child` is fallible (e.g. an unknown
            // `subagent_type`). Announcing the spawn before this could leave a
            // phantom "running" agent in the roster with no matching SubagentDone
            // if `?` returned early. Announce only once the child is guaranteed to
            // run.
            let (child, handle) = self
                .make_child(
                    id.clone(),
                    base_cwd.clone(),
                    t["subagent_type"].as_str(),
                    read_only,
                    parent.clone(),
                )
                .map_err(ToolError::invalid_input)?;
            let mut lifecycle = SubagentLifecycle::start(
                self.env.bus.clone(),
                ctx.coord
                    .as_ref()
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| ROOT_AGENT_ID.into()),
                id,
                description.clone(),
                prompt.clone(),
            )
            .with_handle(handle.clone());
            let parent_cancel = self.env.parent_cancel.clone();
            handles.push(tokio::spawn(async move {
                let mut child = child;
                let outcome = handle
                    .run_until_stopped(parent_cancel, child.run(&prompt))
                    .await;
                let cancelled = outcome.is_none() || child.is_cancelled();
                let (failed, output) = match outcome {
                    Some(Ok(out)) => (false, out),
                    Some(Err(e)) => (true, format!("error: {e}")),
                    None => (false, "[cancelled]".into()),
                };
                lifecycle.finish(failed && !cancelled, cancelled);
                format!("### {}\n{}", description, output)
            }));
        }

        let mut results = Vec::new();
        for h in handles {
            match h.await {
                Ok(r) => results.push(r),
                Err(e) => results.push(format!("error: subagent join failed: {}", e)),
            }
        }
        Ok(results.join("\n\n"))
    }
}

/// Focused instructions for an exploration child, separate from implementation work.
const EXPLORE_SYSTEM: &str = "You are bob's read-only code explorer. Answer the supplied \
    repository question with evidence; do not implement fixes or write report files. Use \
    only the read-only tools exposed to you. Do not run shell commands, modify files, or \
    delegate further. Start from the supplied file or symbol and follow the code that \
    controls the behavior. Widen the search only to resolve a specific unanswered question, \
    and match its depth to the caller's request. Reuse findings rather than repeating \
    searches. Return concise findings with file:line references, relevant relationships, \
    and any remaining uncertainty. Distinguish observed behavior from hypotheses; if \
    evidence is missing, say so. Return a synthesized answer, not a transcript of tool use.";

/// A read-only investigation child using the parent provider.
pub struct ExploreTool {
    pub env: AgentEnv,
}

#[async_trait]
impl Tool for ExploreTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "explore".to_string(),
            description: "Investigate a repository question in a fresh read-only agent and \
                return a concise answer with file:line evidence. Use for substantial exploration \
                when a directed lookup is insufficient or keeping large results out of the main \
                conversation is useful. For a known file or symbol, use direct read/search/LSP \
                tools instead. Supply a self-contained query, scope, and desired depth. \
                This call waits for the result. The child uses the parent model, not a separate \
                cheaper model, and cannot edit or run shell commands. Plan mode currently blocks \
                this tool; use direct read/search tools there."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The question to investigate, self-contained (name the scope + what to find)." },
                    "description": { "type": "string", "description": "A 3-5 word label shown in the UI (e.g. \"trace auth flow\")." }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let query = input["query"].as_str().unwrap_or("").trim().to_string();
        if query.is_empty() {
            return Err(ToolError::invalid_input("query is required"));
        }
        let label = input["description"]
            .as_str()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("explore")
            .to_string();
        let cwd = if self.env.cwd.is_empty() {
            ctx.cwd.clone()
        } else {
            self.env.cwd.clone()
        };

        // Announce as a subagent so it appears in the transcript + team drawer.
        let id = next_subagent_id();
        let parent = ctx
            .coord
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| ROOT_AGENT_ID.into());
        let (inbox, tx) = mailbox();
        let handle = self.env.team.register(id.clone(), 1, parent, tx);
        let mut lifecycle = SubagentLifecycle::start(
            self.env.bus.clone(),
            ctx.coord
                .as_ref()
                .map(|c| c.name.clone())
                .unwrap_or_else(|| ROOT_AGENT_ID.into()),
            id.clone(),
            label,
            query.clone(),
        )
        .with_handle(handle.clone());

        // A read-only child: only reads/searches, its own focused prompt.
        let mut child = build_subagent(SubagentSpec {
            provider: self.env.provider.clone(),
            tools: self.env.subagent_tools.read_only_subset(),
            bus: self.env.bus.clone(),
            system: Some(EXPLORE_SYSTEM.to_string()),
            cwd,
            jobs: self.env.jobs.clone(),
            lsp: self.env.lsp.clone(),
            name: id.clone(),
            max_turns: EXPLORE_MAX_TURNS,
            depth: 1,
            inbox: Some(inbox),
            team: Some(self.env.team.clone()),
            parent_cancel: Some(self.env.parent_cancel.clone()),
        });
        let outcome = handle
            .run_until_stopped(self.env.parent_cancel.clone(), child.run(&query))
            .await;
        let cancelled = outcome.is_none() || child.is_cancelled();
        let (failed, output) = match outcome {
            Some(Ok(out)) => (false, out),
            Some(Err(e)) => (true, format!("error: {e}")),
            None => (false, "[cancelled]".into()),
        };
        lifecycle.finish(failed && !cancelled, cancelled);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::env::AgentEnv;
    use crate::core::events::{AgentEvent, EventBus};
    use crate::providers::mock::MockProvider;
    use crate::tools::file_tracker::FileTracker;
    use crate::tools::todo::TodoStore;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    // Regression: subagent ids used to be `task_{i+1}` numbered per-batch, so a
    // second `task` call reused `task_1..` and the team drawer (which keys threads
    // by id) silently overwrote the first batch's finished threads — spawned
    // agents "didn't show up". Ids must be globally unique across calls.
    #[test]
    fn subagent_ids_are_unique_across_batches() {
        let mut seen = HashSet::new();
        // Two "batches" of four, as two separate `task` calls would produce.
        for _ in 0..8 {
            let id = next_subagent_id();
            assert!(seen.insert(id.clone()), "duplicate subagent id: {id}");
        }
    }

    #[test]
    fn restored_task_ids_are_not_reused() {
        reserve_subagent_id("task_10000");
        reserve_subagent_id("task_5");
        let id = next_subagent_id();
        assert!(id.strip_prefix("task_").unwrap().parse::<u64>().unwrap() > 10000);
    }

    fn task_tool_with(bus: EventBus) -> TaskTool {
        TaskTool {
            env: AgentEnv {
                provider: Arc::new(MockProvider::new(vec![])),
                subagent_tools: crate::tools::registry::ToolRegistry::new(None),
                bus,
                cwd: ".".to_string(),
                subagent_system: Some("sys".to_string()),
                jobs: crate::tools::jobs::JobRegistry::new(),
                team: crate::agent::team::AgentRegistry::new(),
                lsp: None,
                parent_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                definitions: std::collections::HashMap::new(),
            },
        }
    }

    fn ctx() -> ToolContext {
        ToolContext {
            cwd: ".".to_string(),
            files: Arc::new(FileTracker::new()),
            todos: Arc::new(TodoStore::new()),
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: None,
            coord: None,
            permissions: None,
        }
    }

    #[tokio::test]
    async fn spawned_parent_waits_for_its_child_report() {
        use crate::providers::mock::{MockReply, MockRule};
        use crate::tools::coordinate::{CoordDeps, ListAgentsTool, SpawnAgentTool};
        use crate::tools::registry::CoordContext;
        let mut task = task_tool_with(EventBus::new());
        let team = task.env.team.clone();
        let (mut inbox, tx) = mailbox();
        team.register("root".into(), 0, String::new(), tx);
        task.env
            .subagent_tools
            .add(Arc::new(ListAgentsTool { team: team.clone() }));
        task.env.provider = Arc::new(
            MockProvider::new(vec![
                MockRule {
                    needle: "start-parent".into(),
                    reply: MockReply::ToolCall {
                        name: "spawn_agent".into(),
                        input: json!({"name":"grandchild","task":"start-child","read_only":true}),
                    },
                },
                MockRule {
                    needle: "start-child".into(),
                    reply: MockReply::ToolCall {
                        name: "list_agents".into(),
                        input: json!({}),
                    },
                },
                MockRule {
                    needle: "finished: child-report".into(),
                    reply: MockReply::Text("all children collected".into()),
                },
            ])
            .with_default(MockReply::Text("child-report".into()))
            .with_delay(std::time::Duration::from_millis(20)),
        );
        let tool = SpawnAgentTool {
            deps: CoordDeps {
                env: task.env,
                team: team.clone(),
            },
        };
        let mut context = ctx();
        context.coord = Some(CoordContext {
            name: "root".into(),
            depth: 0,
            team: team.clone(),
        });
        tool.execute(json!({"name":"parent","task":"start-parent"}), &context)
            .await
            .unwrap();
        let report = tokio::time::timeout(std::time::Duration::from_secs(2), inbox.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report.text, "finished: all children collected");
        assert_eq!(team.get("parent").unwrap().status(), AgentStatus::Done);
        assert_eq!(team.get("grandchild").unwrap().status(), AgentStatus::Done);
    }

    struct HoldingTool {
        started: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl Tool for HoldingTool {
        fn is_read_only(&self) -> bool {
            true
        }
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "hold".into(),
                description: "test wait".into(),
                input_schema: json!({"type":"object"}),
            }
        }
        async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
            self.started.notify_one();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn every_agent_kind_can_be_messaged_and_stopped_during_a_tool_wait() {
        use crate::agent::team::AgentRegistry;
        use crate::providers::mock::MockReply;
        use crate::tools::coordinate::{CoordDeps, SendMessageTool, SpawnAgentTool, StopAgentTool};
        use crate::tools::registry::CoordContext;
        for kind in ["inline", "background", "explore", "named", "workflow"] {
            let mut task = task_tool_with(EventBus::new());
            let started = Arc::new(tokio::sync::Notify::new());
            task.env.provider =
                Arc::new(MockProvider::new(vec![]).with_default(MockReply::ToolCall {
                    name: "hold".into(),
                    input: json!({}),
                }));
            task.env.subagent_tools.add(Arc::new(HoldingTool {
                started: started.clone(),
            }));
            let team: AgentRegistry = task.env.team.clone();
            let (mut root_inbox, root_tx) = mailbox();
            team.register("root".into(), 0, String::new(), root_tx);
            let mut context = ctx();
            context.jobs = task.env.jobs.clone();
            context.coord = Some(CoordContext {
                name: "root".into(),
                depth: 0,
                team: team.clone(),
            });
            let controller = context.clone();
            let (tool, input): (Box<dyn Tool>, Value) = match kind {
                "inline" | "background" => (
                    Box::new(task),
                    json!({"background": kind == "background", "read_only": true, "tasks": [{"description":"wait", "prompt":"wait"}]}),
                ),
                "explore" => (
                    Box::new(ExploreTool { env: task.env }),
                    json!({"query":"wait"}),
                ),
                "named" => (
                    Box::new(SpawnAgentTool {
                        deps: CoordDeps {
                            env: task.env,
                            team: team.clone(),
                        },
                    }),
                    json!({"name":"worker", "task":"wait", "read_only":true}),
                ),
                _ => (
                    Box::new(crate::tools::workflow_tool::WorkflowTool { env: task.env }),
                    json!({"shape":"fan_out", "items":["wait"], "map_prompt":"{item}"}),
                ),
            };
            let run = tokio::spawn(async move { tool.execute(input, &context).await });
            tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
                .await
                .expect("child should enter the tool wait");
            let id = team
                .roster()
                .into_iter()
                .find(|(id, _, _)| id != "root")
                .unwrap()
                .0;
            let message = SendMessageTool { team: team.clone() }
                .execute(json!({"to":id, "message":"new instructions"}), &controller)
                .await
                .unwrap();
            assert!(message.contains("delivered"), "{kind}");
            StopAgentTool { team: team.clone() }
                .execute(json!({"name":id}), &controller)
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while team.get(&id).unwrap().status() == AgentStatus::Running {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("stop must interrupt a pending tool");
            assert_eq!(
                team.get(&id).unwrap().status(),
                AgentStatus::Cancelled,
                "{kind}"
            );
            assert_eq!(team.get("root").unwrap().status(), AgentStatus::Running);
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), run)
                .await
                .unwrap()
                .unwrap();
            match kind {
                "workflow" => assert!(result.unwrap_err().message.contains("workflow incomplete")),
                "inline" | "explore" => assert!(result.unwrap().contains("cancelled")),
                "background" => {
                    assert!(result.is_ok());
                    assert_eq!(controller.jobs.status_of(&id), Some(JobStatus::Cancelled));
                }
                "named" => {
                    assert!(result.is_ok());
                    assert!(root_inbox
                        .drain()
                        .iter()
                        .any(|m| m.text.contains("cancelled")));
                }
                _ => unreachable!(),
            }
            assert!(!team.send(&id, "root", "too late"));
        }
    }

    #[tokio::test]
    async fn task_consumes_message_received_during_its_final_response() {
        use crate::providers::mock::{MockReply, MockRule};
        let (bus, events) = capture_events();
        let mut task = task_tool_with(bus);
        let provider = MockProvider::new(vec![MockRule {
            needle: "new instructions".into(),
            reply: MockReply::Text("message received".into()),
        }])
        .with_delay(std::time::Duration::from_millis(40));
        task.env.provider = Arc::new(provider.clone());
        let context = ctx();
        task.execute(json!({"background":true,"read_only":true,"tasks":[{"description":"review", "prompt":"original"}]}), &context).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while provider.call_count() == 0 {
                tokio::task::yield_now().await;
            }
            assert!(task.env.team.send("job_1", "user", "new instructions"));
            while context.jobs.status_of("job_1") == Some(JobStatus::Running) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            context.jobs.output_of("job_1"),
            Some((JobStatus::Done, "message received".into()))
        );
        assert_eq!(provider.call_count(), 2);
        assert!(events.lock().unwrap().iter().any(|e| matches!(e, AgentEvent::AgentMessage { to, text, .. } if to == "job_1" && text == "new instructions")));
    }

    fn capture_events() -> (EventBus, Arc<Mutex<Vec<AgentEvent>>>) {
        let bus = EventBus::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        bus.on(Arc::new(move |event| {
            captured.lock().unwrap().push(event.clone())
        }));
        (bus, events)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn background_tasks_publish_collectable_results() {
        let (bus, events) = capture_events();
        let tool = task_tool_with(bus);
        let ctx = ctx();
        let result = tool
            .execute(
                json!({
                    "background": true,
                    "tasks": [
                        {"description": "first", "prompt": "first task"},
                        {"description": "second", "prompt": "second task"}
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.contains("started 2 background job(s)"));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while ctx
                .jobs
                .list()
                .iter()
                .any(|job| job.3 == JobStatus::Running)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("background tasks should finish");
        for id in ["job_1", "job_2"] {
            assert_eq!(ctx.jobs.output_of(id), Some((JobStatus::Done, "ok".into())));
        }
        assert!(ctx.jobs.take_finished().is_empty());
        let events = events.lock().unwrap();
        for (id, label, prompt) in [
            ("job_1", "first", "first task"),
            ("job_2", "second", "second task"),
        ] {
            let spawn = events.iter().position(|event| matches!(event,
                AgentEvent::SubagentSpawn { agent_id, parent_id, task, prompt: delegated }
                    if agent_id == id && parent_id == "root" && task == label && delegated == prompt
            )).expect("background task must announce its description before running");
            let start = events
                .iter()
                .position(
                    |event| matches!(event, AgentEvent::TurnStart { agent_id } if agent_id == id),
                )
                .unwrap();
            let completions: Vec<_> = events.iter().enumerate().filter(|(_, event)| matches!(event,
                AgentEvent::SubagentDone { agent_id, failed: false, cancelled: false } if agent_id == id
            )).collect();
            assert_eq!(completions.len(), 1);
            assert!(spawn < start && start < completions[0].0);
        }
    }

    #[tokio::test]
    async fn background_task_cancellation_is_terminal_in_registry_and_events() {
        for direct_abort in [true, false] {
            let (bus, events) = capture_events();
            let mut tool = task_tool_with(bus);
            tool.env.provider =
                Arc::new(MockProvider::new(vec![]).with_delay(std::time::Duration::from_secs(1)));
            let ctx = ctx();
            tool.execute(json!({"background": true, "tasks": [{"description": "review", "prompt": "review code"}]}), &ctx).await.unwrap();
            if direct_abort {
                assert!(ctx.jobs.cancel("job_1"));
            } else {
                tool.env.parent_cancel.store(true, Ordering::Relaxed);
            }
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    let finished = events.lock().unwrap().iter().any(|event| matches!(event,
                        AgentEvent::SubagentDone { agent_id, failed: false, cancelled: true } if agent_id == "job_1"
                    ));
                    if finished && ctx.jobs.status_of("job_1") == Some(JobStatus::Cancelled) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }).await.expect("cancelled task must settle both views");
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| matches!(e, AgentEvent::SubagentDone { .. }))
                    .count(),
                1
            );
        }
    }

    struct FailingProvider;

    #[async_trait]
    impl crate::providers::provider::Provider for FailingProvider {
        fn name(&self) -> &str {
            "fixture"
        }
        fn model(&self) -> &str {
            "fixture"
        }
        async fn generate(
            &self,
            _: crate::core::types::GenerateOptions,
        ) -> anyhow::Result<crate::core::types::Completion> {
            anyhow::bail!("fixture failure")
        }
        async fn stream(
            &self,
            _: crate::core::types::GenerateOptions,
        ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<crate::core::types::StreamEvent>>
        {
            anyhow::bail!("fixture failure")
        }
    }

    #[tokio::test]
    async fn background_task_failure_emits_failed_completion() {
        let (bus, events) = capture_events();
        let mut tool = task_tool_with(bus);
        tool.env.provider = Arc::new(FailingProvider);
        let ctx = ctx();
        tool.execute(json!({"background": true, "tasks": [{"description": "review", "prompt": "review code"}]}), &ctx).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while ctx.jobs.status_of("job_1") == Some(JobStatus::Running) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(ctx.jobs.status_of("job_1"), Some(JobStatus::Failed));
        assert!(events.lock().unwrap().iter().any(|event| matches!(event,
            AgentEvent::SubagentDone { agent_id, failed: true, cancelled: false } if agent_id == "job_1"
        )));
    }

    // Regression: `SubagentSpawn` was emitted BEFORE the fallible `make_child`, so
    // an unknown `subagent_type` announced a phantom agent to the roster that never
    // ran and never got a `SubagentDone` — leaving it stuck "running" forever with
    // no tool calls or output. A failed spawn must emit NO SubagentSpawn.
    #[tokio::test]
    async fn unknown_subagent_type_emits_no_phantom_spawn() {
        let bus = EventBus::new();
        let spawns = Arc::new(Mutex::new(0usize));
        {
            let spawns = spawns.clone();
            bus.on(Arc::new(move |e: &AgentEvent| {
                if matches!(e, AgentEvent::SubagentSpawn { .. }) {
                    *spawns.lock().unwrap() += 1;
                }
            }));
        }
        let tool = task_tool_with(bus);
        let input = json!({
            "tasks": [{
                "description": "x",
                "prompt": "do x",
                "subagent_type": "does-not-exist"
            }]
        });
        // The call fails (unknown type) — and crucially announced nothing.
        assert!(tool.execute(input, &ctx()).await.is_err());
        assert_eq!(
            *spawns.lock().unwrap(),
            0,
            "a failed spawn must not announce a phantom agent"
        );
    }
}
