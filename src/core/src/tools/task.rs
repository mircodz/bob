use crate::agent::agent::{
    build_subagent, RunStopReason, RunStopped, SubagentSpec, EXPLORE_MAX_TURNS, ROOT_AGENT_ID,
    SUBAGENT_MAX_TURNS,
};
use crate::agent::env::AgentEnv;
use crate::agent::lifecycle::SubagentLifecycle;
use crate::agent::team::{mailbox, AgentStatus};
use crate::core::types::ToolSpec;
use crate::tools::jobs::JobStatus;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolErrorKind, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide monotonic counter keeps subagent ids unique across task calls.
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

#[async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task".to_string(),
            description: format!(
                "Run independent implementation or review tasks in fresh agents and return their results together. \
                Use direct tools for known-path or symbol lookups. For repository investigation, use explore \
                instead, including parallel explore calls for independent questions. Agents do not inherit \
                your conversation: supply the scope, prior findings and failed approaches, and expected \
                deliverable. Give parallel editors non-overlapping file ownership and leave \
                workspace-wide verification to one owner. Preserve successful sibling results if a task \
                fails; retry only the unresolved scope. \
                By default this call waits for every result. Set background:true only when \
                useful independent work can continue; it returns job IDs for later collection \
                with job_output. Do not detach merely to poll or run sleeps. \
                Live agents can receive send_message or stop_agent using IDs from list_agents. \
                Task agents cannot spawn further agents. For review or other deliverables that must \
                not mutate files, set read_only:true to exclude shell and editing tools. \
                subagent_type selects a listed definition.{}",
                self.env.definitions_help()
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": crate::agent::team::MAX_TEAM_SIZE,
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
            Some(t) if !t.is_empty() && t.len() <= crate::agent::team::MAX_TEAM_SIZE => t.clone(),
            _ => {
                return Err(ToolError::invalid_input(format!(
                    "tasks must contain 1..={} entries",
                    crate::agent::team::MAX_TEAM_SIZE
                )))
            }
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

        let depth = ctx.coord.as_ref().map_or(1, |coord| coord.depth + 1);
        let mut prepared = Vec::with_capacity(tasks.len());
        for task in &tasks {
            let description = task["description"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    ToolError::invalid_input("every task requires a nonempty description")
                })?;
            let prompt = task["prompt"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| ToolError::invalid_input("every task requires a nonempty prompt"))?;
            if task
                .get("subagent_type")
                .is_some_and(|value| !value.is_string())
            {
                return Err(ToolError::invalid_input("subagent_type must be a string"));
            }
            let (system, tools) = self
                .env
                .resolve_child(task["subagent_type"].as_str(), read_only)
                .map_err(ToolError::invalid_input)?;
            prepared.push((description.to_owned(), prompt.to_owned(), system, tools));
        }
        let mut mailboxes = Vec::with_capacity(prepared.len());
        let mut requests = Vec::with_capacity(prepared.len());
        for _ in &prepared {
            let id = if background {
                ctx.jobs.next_id()
            } else {
                next_subagent_id()
            };
            let (inbox, tx) = mailbox();
            requests.push((id, depth, parent.clone(), tx));
            mailboxes.push(inbox);
        }
        let admitted = self
            .env
            .team
            .try_register_batch(requests)
            .map_err(ToolError::invalid_input)?;
        let mut work = Vec::with_capacity(prepared.len());
        let mut background_work = Vec::with_capacity(prepared.len());
        let mut ids = Vec::with_capacity(prepared.len());
        for (((description, prompt, system, tools), inbox), handle) in
            prepared.into_iter().zip(mailboxes).zip(admitted)
        {
            let id = handle.name.clone();
            let mut child = build_subagent(SubagentSpec {
                provider: self.env.provider.clone(),
                tools,
                bus: self.env.bus.clone(),
                system,
                cwd: base_cwd.clone(),
                jobs: self.env.jobs.clone(),
                lsp: self.env.lsp.clone(),
                name: id.clone(),
                max_turns: SUBAGENT_MAX_TURNS,
                depth,
                inbox: Some(inbox),
                team: Some(self.env.team.clone()),
                parent_cancel: Some(self.env.parent_cancel.clone()),
                hooks: self.env.hooks.clone(),
                memory: self.env.memory.clone(),
            });
            let mut lifecycle = SubagentLifecycle::start(
                self.env.bus.clone(),
                parent.clone(),
                id.clone(),
                description.clone(),
                prompt.clone(),
            )
            .with_handle(handle.clone());
            let parent_cancel = self.env.parent_cancel.clone();
            let run = async move {
                let outcome = crate::core::panic::catch(handle.run_until_stopped(
                    parent_cancel,
                    child.cancel_handle(),
                    child.run(&prompt),
                ))
                .await;
                let panicked = match &outcome {
                    Err(_) => true,
                    Ok(Some(Err(error))) => error
                        .downcast_ref::<RunStopped>()
                        .is_some_and(|stopped| stopped.0.stop_reason == RunStopReason::ToolPanic),
                    _ => false,
                };
                let mutations_settled = child.settle_owned_mutations().await;
                let cancelled = matches!(outcome, Ok(None)) || child.is_cancelled();
                let (mut failed, mut output) = match outcome {
                    Ok(Some(Ok(out))) => (false, out),
                    Ok(Some(Err(error))) => (true, format!("error: {error}")),
                    Ok(None) => (false, "[cancelled]".into()),
                    Err(_) => (true, "error: subagent panicked".into()),
                };
                if !mutations_settled {
                    failed = true;
                    output.push_str(
                        "\nMemory mutation cleanup timed out; changes may still complete.",
                    );
                }
                let status = match lifecycle.finish(failed && !cancelled, cancelled) {
                    AgentStatus::Cancelled => JobStatus::Cancelled,
                    AgentStatus::Failed => JobStatus::Failed,
                    _ => JobStatus::Done,
                };
                if status == JobStatus::Cancelled && mutations_settled {
                    output = "[cancelled]".into();
                }
                (status, output, panicked)
            };
            if background {
                background_work.push((
                    id.clone(),
                    "task".into(),
                    description.clone(),
                    async move {
                        let (status, output, _) = run.await;
                        (status, output)
                    },
                ));
                ids.push(format!("{id} ({description})"));
            } else {
                work.push(async move {
                    let (status, output, panicked) = run.await;
                    (status, format!("### {description}\n{output}"), panicked)
                });
            }
        }
        if background {
            if !ctx.jobs.spawn_batch(background_work) {
                return Err(ToolError::failed(
                    "background job admission rejected; no tasks launched",
                ));
            }
            return Ok(format!(
                "started {} background job(s): {}\nUse job_status / job_output to check on them.",
                ids.len(),
                ids.join(", ")
            )
            .into());
        }
        let results = futures::future::join_all(work).await;
        let failed = results
            .iter()
            .any(|(status, _, _)| *status != JobStatus::Done);
        let panicked = results.iter().any(|(_, _, panicked)| *panicked);
        let output = results
            .into_iter()
            .map(|(_, output, _)| output)
            .collect::<Vec<_>>()
            .join("\n\n");
        if panicked {
            Err(ToolError::new(ToolErrorKind::Panic, output))
        } else if failed {
            Err(ToolError::failed(output))
        } else {
            Ok(output.into())
        }
    }
}

const EXPLORE_SYSTEM: &str = "You are bob's read-only code explorer. Answer the supplied \
    repository question with evidence; do not implement fixes or write report files. Use \
    only the read-only tools exposed to you. Do not run shell commands, modify files, or \
    delegate further. Start from the supplied file or symbol and follow the code that \
    controls the behavior. Widen the search only to resolve a specific unanswered question, \
    and match its depth to the caller's request. Use glob for paths, grep for file contents, \
    and LSP for symbols when available. Parallelize independent probes. Narrow or page truncated \
    results before claiming something is absent. On a failed query, use the error to correct \
    it rather than repeating it unchanged; respect permission and mode restrictions. \
    Reuse findings and stop when the evidence answers the question. Return concise findings \
    with file:line references, relevant relationships, \
    and any remaining uncertainty. When the caller is preparing an implementation, identify \
    relevant reusable functions, types, or analogous existing features you found; do not \
    expand into an unrelated survey just to find a precedent. Distinguish observed behavior \
    from hypotheses; if evidence is missing, say so. Return a synthesized answer, not a transcript of tool use.";

pub struct ExploreTool {
    pub env: AgentEnv,
}

#[async_trait]
impl Tool for ExploreTool {
    fn is_read_only(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "explore".to_string(),
            description: "Investigate a repository question in a fresh read-only agent and \
                return a concise answer with file:line evidence. Use early for open-ended or \
                multi-component questions likely to need several searches. Do not first exhaust \
                direct searches; also use when directed search grows into an investigation. \
                Run parallel explore calls for independent questions rather than a read-only task batch. Examples: \
                trace authentication from the CLI through provider selection and token refresh, \
                or map how errors propagate across the runtime and UI. For simple known-path \
                or symbol lookups, use direct read/search/LSP tools instead. Supply a self-contained \
                query, scope, prior findings, and requested depth: quick, medium, or very thorough. \
                This call waits for the result. The child uses the parent model, not a separate \
                cheaper model, and cannot edit, run shell commands, or delegate further."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "A self-contained question naming the scope, what to find, and requested depth: quick (focused trace), medium (related components), or very thorough (cross-cutting paths and edge cases)." },
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

        let id = next_subagent_id();
        let parent = ctx
            .coord
            .as_ref()
            .map(|c| c.name.clone())
            .unwrap_or_else(|| ROOT_AGENT_ID.into());
        let (inbox, tx) = mailbox();
        let depth = ctx.coord.as_ref().map_or(1, |coord| coord.depth + 1);
        let handle = self
            .env
            .team
            .try_register(id.clone(), depth, parent, tx)
            .map_err(ToolError::invalid_input)?;
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

        let mut child = build_subagent(SubagentSpec {
            provider: self.env.provider.clone(),
            tools: self.env.subagent_tools.read_only_subset(),
            bus: self.env.bus.clone(),
            system: Some(format!(
                "{EXPLORE_SYSTEM}\n\n{}",
                crate::agent::prompt::project_material_trust_block()
            )),
            cwd,
            jobs: self.env.jobs.clone(),
            lsp: self.env.lsp.clone(),
            name: id.clone(),
            max_turns: EXPLORE_MAX_TURNS,
            depth,
            inbox: Some(inbox),
            team: Some(self.env.team.clone()),
            parent_cancel: Some(self.env.parent_cancel.clone()),
            hooks: self.env.hooks.clone(),
            memory: self.env.memory.clone(),
        });
        let outcome = crate::core::panic::catch(handle.run_until_stopped(
            self.env.parent_cancel.clone(),
            child.cancel_handle(),
            child.run(&query),
        ))
        .await;
        let panicked = match &outcome {
            Err(_) => true,
            Ok(Some(Err(error))) => error
                .downcast_ref::<RunStopped>()
                .is_some_and(|stopped| stopped.0.stop_reason == RunStopReason::ToolPanic),
            _ => false,
        };
        let mutations_settled = child.settle_owned_mutations().await;
        let cancelled = matches!(outcome, Ok(None)) || child.is_cancelled();
        let (failed, output) = match outcome {
            Ok(Some(Ok(out))) => (false, out),
            Ok(Some(Err(e))) => (true, format!("error: {e}")),
            Ok(None) => (false, "[cancelled]".into()),
            Err(_) => (true, "error: explorer panicked; outcome unknown".into()),
        };
        let status = lifecycle.finish((failed || !mutations_settled) && !cancelled, cancelled);
        if !mutations_settled {
            return Err(ToolError::failed(
                "memory mutation cleanup timed out; changes may still complete",
            ));
        }
        if panicked {
            Err(ToolError::new(ToolErrorKind::Panic, output))
        } else if status == AgentStatus::Done {
            Ok(output.into())
        } else if status == AgentStatus::Cancelled {
            Err(ToolError::failed("[cancelled]"))
        } else {
            Err(ToolError::failed(output))
        }
    }
}

#[cfg(test)]
mod tests {
    mod task {
        use crate::agent::agent::{EXPLORE_MAX_TURNS, SUBAGENT_MAX_TURNS};
        use crate::agent::env::AgentEnv;
        use crate::agent::team::{mailbox, AgentStatus};
        use crate::core::events::{AgentEvent, EventBus};
        use crate::core::types::{ToolOutput, ToolSpec};
        use crate::providers::mock::{MockProvider, MockReply};
        use crate::providers::provider::Provider;
        use crate::tools::file_tracker::FileTracker;
        use crate::tools::jobs::JobStatus;
        use crate::tools::registry::{Tool, ToolContext, ToolResult};
        use crate::tools::task::{reserve_subagent_id, ExploreTool, TaskTool};
        use crate::tools::todo::TodoStore;
        use async_trait::async_trait;
        use serde_json::{json, Value};
        use std::collections::HashSet;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        type ScriptStep = (bool, &'static str, MockReply);

        #[derive(Clone)]
        struct ScriptedProvider {
            steps: Arc<Mutex<Vec<ScriptStep>>>,
            calls: Arc<AtomicUsize>,
            delay: std::time::Duration,
        }

        impl ScriptedProvider {
            fn new(steps: Vec<(&'static str, MockReply)>) -> Self {
                Self {
                    steps: Arc::new(Mutex::new(
                        steps
                            .into_iter()
                            .map(|(needle, reply)| (false, needle, reply))
                            .collect(),
                    )),
                    calls: Arc::new(AtomicUsize::new(0)),
                    delay: std::time::Duration::ZERO,
                }
            }

            fn with_summary(self, output: &str) -> Self {
                self.steps.lock().unwrap().push((
                    true,
                    "You've reached your turn limit",
                    MockReply::Text(output.into()),
                ));
                self
            }

            fn with_delay(mut self, delay: std::time::Duration) -> Self {
                self.delay = delay;
                self
            }

            fn call_count(&self) -> usize {
                self.calls.load(Ordering::Relaxed)
            }

            fn reply(
                &self,
                opts: &crate::core::types::GenerateOptions,
                summary: bool,
            ) -> MockReply {
                self.calls.fetch_add(1, Ordering::Relaxed);
                let last = opts
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role != crate::core::types::Role::Assistant)
                    .expect("provider requires an input message")
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        crate::core::types::ContentBlock::Text { text } => Some(text.as_str()),
                        crate::core::types::ContentBlock::ToolResult { content, .. } => {
                            Some(content.as_text().unwrap())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut steps = self.steps.lock().unwrap();
                let index = steps
                    .iter()
                    .position(|(generate, needle, _)| {
                        *generate == summary && last.contains(*needle)
                    })
                    .unwrap_or_else(|| {
                        panic!("unexpected provider call (generate={summary}): {last}")
                    });
                steps.remove(index).2
            }
        }

        #[async_trait]
        impl Provider for ScriptedProvider {
            fn name(&self) -> &str {
                "scripted-delegation"
            }
            fn model(&self) -> &str {
                "mock-model"
            }

            async fn generate(
                &self,
                opts: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<crate::core::types::Completion> {
                let reply = self.reply(&opts, true);
                MockProvider::new(vec![])
                    .with_default(reply)
                    .generate(opts)
                    .await
            }

            async fn stream(
                &self,
                opts: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<crate::core::types::StreamEvent>>
            {
                let reply = self.reply(&opts, false);
                MockProvider::new(vec![])
                    .with_default(reply)
                    .with_delay(self.delay)
                    .stream(opts)
                    .await
            }
        }

        fn exhausted_provider(budget: u32, output: &str) -> ScriptedProvider {
            let missing = MockReply::ToolCall {
                name: "missing".into(),
                input: json!({}),
            };
            let mut steps = vec![("go", missing.clone())];
            steps.extend((1..budget).map(|_| ("unknown tool", missing.clone())));
            ScriptedProvider::new(steps).with_summary(output)
        }

        #[tokio::test]
        async fn subagent_ids_are_unique_across_batches() {
            let (bus, events) = capture_events();
            let mut tool = task_tool_with(bus);
            tool.env.provider = Arc::new(ScriptedProvider::new(vec![
                (
                    "identity check",
                    MockReply::Text("ok".into())
                );
                8
            ]));
            let context = ctx();
            for _ in 0..2 {
                tool.execute(
                    json!({"tasks": [
                        {"description":"first", "prompt":"identity check"},
                        {"description":"second", "prompt":"identity check"},
                        {"description":"third", "prompt":"identity check"},
                        {"description":"fourth", "prompt":"identity check"}
                    ]}),
                    &context,
                )
                .await
                .unwrap();
            }
            let mut seen = HashSet::new();
            for event in events.lock().unwrap().iter() {
                if let AgentEvent::SubagentSpawn { agent_id, .. } = event {
                    assert!(
                        seen.insert(agent_id.clone()),
                        "duplicate subagent id: {agent_id}"
                    );
                }
            }
            assert_eq!(seen.len(), 8);
            assert_eq!(tool.env.team.roster().len(), 8);
            assert!(tool
                .env
                .team
                .roster()
                .iter()
                .all(|(_, _, status)| *status == AgentStatus::Done));
        }

        #[tokio::test]
        async fn restored_task_ids_are_not_reused() {
            reserve_subagent_id("task_10000");
            reserve_subagent_id("task_5");
            let (bus, events) = capture_events();
            let mut tool = task_tool_with(bus);
            tool.env.provider = Arc::new(ScriptedProvider::new(vec![(
                "restored identity",
                MockReply::Text("ok".into()),
            )]));
            tool.execute(
                json!({"tasks":[{"description":"restored", "prompt":"restored identity"}]}),
                &ctx(),
            )
            .await
            .unwrap();
            let events = events.lock().unwrap();
            let ids: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::SubagentSpawn { agent_id, .. } => Some(agent_id),
                    _ => None,
                })
                .collect();
            assert_eq!(ids.len(), 1);
            assert!(
                ids[0]
                    .strip_prefix("task_")
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
                    > 10000
            );
        }

        fn task_tool_with(bus: EventBus) -> TaskTool {
            TaskTool {
                env: AgentEnv {
                    provider: Arc::new(ScriptedProvider::new(vec![])),
                    subagent_tools: crate::tools::registry::ToolRegistry::new(None),
                    bus,
                    cwd: ".".to_string(),
                    subagent_system: Some("sys".to_string()),
                    jobs: crate::tools::jobs::JobRegistry::new(),
                    team: crate::agent::team::AgentRegistry::new(),
                    lsp: None,
                    parent_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    definitions: std::collections::HashMap::new(),
                    hooks: Default::default(),
                    memory: None,
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

        async fn run_delegated(kind: &str, env: AgentEnv) -> String {
            use crate::tools::coordinate::{CoordDeps, SpawnAgentTool};
            use crate::tools::registry::CoordContext;
            let team = env.team.clone();
            let (mut inbox, tx) = mailbox();
            team.register("root".into(), 0, String::new(), tx);
            let mut context = ctx();
            context.jobs = env.jobs.clone();
            context.coord = Some(CoordContext {
                name: "root".into(),
                depth: 0,
                team: team.clone(),
            });
            let (tool, input): (Box<dyn Tool>, Value) = match kind {
                "inline" | "background" => (
                    Box::new(TaskTool { env }),
                    json!({"background":kind == "background", "tasks":[{"description":"guardrails", "prompt":"go"}]}),
                ),
                "explore" => (Box::new(ExploreTool { env }), json!({"query":"go"})),
                "named" => (
                    Box::new(SpawnAgentTool {
                        deps: CoordDeps { env, team },
                    }),
                    json!({"name":"worker", "task":"go"}),
                ),
                "workflow" => (
                    Box::new(crate::tools::workflow_tool::WorkflowTool { env }),
                    json!({"shape":"fan_out", "items":["go"], "map_prompt":"{item}"}),
                ),
                _ => unreachable!(),
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                let result = tool.execute(input, &context).await;
                match kind {
                    "background" => {
                        result.unwrap();
                        while context.jobs.status_of("job_1") == Some(JobStatus::Running) {
                            tokio::task::yield_now().await;
                        }
                        context.jobs.output_of("job_1").unwrap().1
                    }
                    "named" => {
                        result.unwrap();
                        inbox.recv().await.unwrap().text
                    }
                    _ => result
                        .map(|output| output.as_text().unwrap().to_owned())
                        .unwrap_or_else(|error| error.message),
                }
            })
            .await
            .expect("delegated run must finish")
        }

        struct GuardedTool(Arc<std::sync::atomic::AtomicUsize>);

        #[async_trait]
        impl Tool for GuardedTool {
            fn is_read_only(&self) -> bool {
                true
            }
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: "guarded".into(),
                    description: "guardrail fixture".into(),
                    input_schema: json!({"type":"object"}),
                }
            }
            async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok("SECRET".into())
            }
        }

        struct DenyGuarded;
        #[async_trait]
        impl crate::agent::hooks::PreToolUse for DenyGuarded {
            async fn on_pre_tool(
                &self,
                tool: &str,
                input: &Value,
            ) -> crate::agent::hooks::PreToolDecision {
                if tool == "guarded" {
                    crate::agent::hooks::PreToolDecision::Deny("blocked by inherited hook".into())
                } else {
                    crate::agent::hooks::PreToolDecision::Proceed(input.clone())
                }
            }
        }

        struct RedactGuarded;
        #[async_trait]
        impl crate::agent::hooks::PostToolUse for RedactGuarded {
            async fn on_post_tool(&self, _: &str, output: ToolOutput, _: bool) -> ToolOutput {
                output
                    .as_text()
                    .unwrap()
                    .replace("SECRET", "REDACTED")
                    .into()
            }
        }

        struct HookCheckingProvider {
            calls: AtomicUsize,
            expected: &'static str,
        }

        #[async_trait]
        impl Provider for HookCheckingProvider {
            fn name(&self) -> &str {
                "hook-check"
            }
            fn model(&self) -> &str {
                "mock-model"
            }
            async fn generate(
                &self,
                _: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<crate::core::types::Completion> {
                anyhow::bail!("hook fixture unexpectedly exhausted its budget")
            }
            async fn stream(
                &self,
                opts: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<crate::core::types::StreamEvent>>
            {
                use crate::core::types::{ContentBlock, Role};
                use crate::providers::mock::MockReply;
                let last = opts
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role != Role::Assistant)
                    .unwrap();
                let call = self.calls.fetch_add(1, Ordering::Relaxed);
                assert!(call < 2, "hook fixture received an extra provider call");
                assert_eq!(last.role == Role::Tool, call == 1);
                let reply = if last.role == Role::Tool {
                    let output = last
                        .content
                        .iter()
                        .find_map(|block| match block {
                            ContentBlock::ToolResult { content, .. } => {
                                Some(content.as_text().unwrap())
                            }
                            _ => None,
                        })
                        .unwrap();
                    anyhow::ensure!(
                        output.contains(self.expected),
                        "hook output mismatch: {output}"
                    );
                    MockReply::Text("guardrails verified".into())
                } else {
                    MockReply::ToolCall {
                        name: "guarded".into(),
                        input: json!({}),
                    }
                };
                MockProvider::new(vec![])
                    .with_default(reply)
                    .stream(opts)
                    .await
            }
        }

        #[tokio::test]
        async fn delegated_children_inherit_pre_denial_and_post_redaction() {
            for kind in ["inline", "background", "explore", "named", "workflow"] {
                for deny in [false, true] {
                    let (bus, events) = capture_events();
                    let mut env = task_tool_with(bus).env;
                    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                    env.subagent_tools.add(Arc::new(GuardedTool(calls.clone())));
                    if deny {
                        env.hooks.pre.push(Arc::new(DenyGuarded));
                    }
                    env.hooks.post.push(Arc::new(RedactGuarded));
                    let expected = if deny {
                        "blocked by inherited hook"
                    } else {
                        "REDACTED"
                    };
                    env.provider = Arc::new(HookCheckingProvider {
                        calls: AtomicUsize::new(0),
                        expected,
                    });
                    let output = run_delegated(kind, env).await;
                    assert!(output.contains("guardrails verified"), "{kind}: {output}");
                    assert_eq!(calls.load(Ordering::Relaxed), usize::from(!deny), "{kind}");
                    assert!(
                        !format!("{:?}", events.lock().unwrap()).contains("SECRET"),
                        "{kind}"
                    );
                }
            }
        }

        #[tokio::test]
        async fn exhausted_children_are_failed_and_keep_partial_output() {
            for kind in ["inline", "background", "explore", "named"] {
                let (bus, events) = capture_events();
                let mut env = task_tool_with(bus).env;
                let team = env.team.clone();
                let jobs = env.jobs.clone();
                let budget = if kind == "explore" {
                    EXPLORE_MAX_TURNS
                } else {
                    SUBAGENT_MAX_TURNS
                };
                let provider = exhausted_provider(budget, "Partial work; verification unfinished.");
                env.provider = Arc::new(provider.clone());
                let output = run_delegated(kind, env).await;
                assert!(
                    output.contains("Partial work; verification unfinished."),
                    "{kind}: {output}"
                );
                assert_eq!(provider.call_count(), budget as usize + 1, "{kind}");
                assert!(
                    team.roster()
                        .iter()
                        .filter(|(id, _, _)| id != "root")
                        .all(|(_, _, status)| *status == AgentStatus::Failed),
                    "{kind}"
                );
                if kind == "background" {
                    assert_eq!(jobs.status_of("job_1"), Some(JobStatus::Failed));
                }
                assert!(
                    events.lock().unwrap().iter().any(|event| matches!(
                        event,
                        AgentEvent::SubagentDone {
                            failed: true,
                            cancelled: false,
                            ..
                        }
                    )),
                    "{kind}"
                );
            }
        }

        struct ToolsetProvider {
            tools: Arc<Mutex<Vec<String>>>,
        }

        #[async_trait]
        impl crate::providers::provider::Provider for ToolsetProvider {
            fn name(&self) -> &str {
                "toolset-fixture"
            }
            fn model(&self) -> &str {
                "toolset-fixture"
            }
            async fn generate(
                &self,
                _: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<crate::core::types::Completion> {
                panic!("toolset fixture must not request a summary")
            }
            async fn stream(
                &self,
                opts: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<crate::core::types::StreamEvent>>
            {
                {
                    let mut tools = self.tools.lock().unwrap();
                    assert!(tools.is_empty(), "toolset fixture must run once");
                    *tools = opts.tools.iter().map(|tool| tool.name.clone()).collect();
                }
                MockProvider::new(vec![]).stream(opts).await
            }
        }

        #[tokio::test]
        async fn explore_is_plan_safe_and_exposes_only_read_only_child_tools() {
            use crate::core::permissions::{Decision, Mode, PermissionEngine};
            use crate::tools::coordinate::{CoordDeps, SpawnAgentTool};
            use crate::tools::registry::ToolRegistry;
            let mut env = task_tool_with(EventBus::new()).env;
            let tools = Arc::new(Mutex::new(Vec::new()));
            env.provider = Arc::new(ToolsetProvider {
                tools: tools.clone(),
            });
            for tool in crate::tools::builtin_tools(Arc::new(
                crate::core::shell::CommandShell::resolve(std::path::Path::new("."), None),
            )) {
                env.subagent_tools.add(tool);
            }
            env.subagent_tools
                .add(Arc::new(TaskTool { env: env.clone() }));
            env.subagent_tools.add(Arc::new(SpawnAgentTool {
                deps: CoordDeps {
                    env: env.clone(),
                    team: env.team.clone(),
                },
            }));
            env.subagent_tools
                .add(Arc::new(crate::tools::workflow_tool::WorkflowTool {
                    env: env.clone(),
                }));
            let permissions = Arc::new(PermissionEngine::new(Decision::Allow, None));
            permissions.set_mode(Mode::Plan);
            let mut registry = ToolRegistry::new(Some(permissions));
            registry.add(Arc::new(ExploreTool { env }));
            assert_eq!(
                registry
                    .execute("explore", json!({"query":"go"}), &ctx())
                    .await
                    .unwrap()
                    .as_text()
                    .unwrap(),
                "ok"
            );
            let tools = tools.lock().unwrap();
            assert!(tools.iter().any(|name| name == "read_file"));
            for forbidden in [
                "bash",
                "write_file",
                "edit_file",
                "multi_edit",
                "task",
                "spawn_agent",
                "workflow",
                "explore",
            ] {
                assert!(!tools.iter().any(|name| name == forbidden), "{forbidden}");
            }
        }

        #[tokio::test]
        async fn exhausted_named_child_does_not_wait_for_outstanding_coordination() {
            let mut env = task_tool_with(EventBus::new()).env;
            let provider = exhausted_provider(SUBAGENT_MAX_TURNS, "unfinished named work");
            env.provider = Arc::new(provider.clone());
            let team = env.team.clone();
            let registry = team.clone();
            env.bus.on(Arc::new(move |event| {
            if matches!(event, AgentEvent::SubagentSpawn { agent_id, .. } if agent_id == "worker") {
                let (_inbox, tx) = mailbox();
                registry.register("waiting-grandchild".into(), 2, "worker".into(), tx);
            }
        }));
            let output = run_delegated("named", env).await;
            assert!(output.contains("turn limit"), "{output}");
            assert_eq!(team.get("worker").unwrap().status(), AgentStatus::Failed);
            assert_eq!(provider.call_count(), SUBAGENT_MAX_TURNS as usize + 1);
        }

        #[tokio::test]
        async fn spawned_parent_waits_for_its_child_report() {
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
            ScriptedProvider::new(vec![
                ("start-parent", MockReply::ToolCall {
                    name: "spawn_agent".into(),
                    input: json!({"name":"grandchild","task":"start-child","read_only":true}),
                }),
                ("start-child", MockReply::ToolCall {
                    name: "list_agents".into(),
                    input: json!({}),
                }),
                ("finished: child-report", MockReply::Text("all children collected".into())),
                ("spawned agent 'grandchild'", MockReply::Text("child-report".into())),
                ("team:", MockReply::Text("child-report".into())),
            ])
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
            use crate::tools::coordinate::{
                CoordDeps, SendMessageTool, SpawnAgentTool, StopAgentTool,
            };
            use crate::tools::registry::CoordContext;
            for kind in ["inline", "background", "explore", "named", "workflow"] {
                let mut task = task_tool_with(EventBus::new());
                let started = Arc::new(tokio::sync::Notify::new());
                task.env.provider = Arc::new(ScriptedProvider::new(vec![(
                    "wait",
                    MockReply::ToolCall {
                        name: "hold".into(),
                        input: json!({}),
                    },
                )]));
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
                assert!(message.as_text().unwrap().contains("delivered"), "{kind}");
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
                    "workflow" => {
                        assert!(result.unwrap_err().message.contains("workflow incomplete"))
                    }
                    "inline" | "explore" => {
                        assert!(result.unwrap_err().message.contains("cancelled"))
                    }
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

        struct NestedToolsetProvider {
            mock: ScriptedProvider,
            tools: Arc<Mutex<Vec<Vec<String>>>>,
        }

        #[async_trait]
        impl Provider for NestedToolsetProvider {
            fn name(&self) -> &str {
                "nested-toolset"
            }
            fn model(&self) -> &str {
                "mock"
            }
            async fn generate(
                &self,
                opts: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<crate::core::types::Completion> {
                self.mock.generate(opts).await
            }
            async fn stream(
                &self,
                opts: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<crate::core::types::StreamEvent>>
            {
                self.tools
                    .lock()
                    .unwrap()
                    .push(opts.tools.iter().map(|tool| tool.name.clone()).collect());
                self.mock.stream(opts).await
            }
        }

        #[tokio::test]
        async fn recursive_profiles_cannot_widen_ancestor_tool_restrictions() {
            use crate::agent::env::AgentDefinition;
            use crate::tools::coordinate::{CoordDeps, SpawnAgentTool};
            use crate::tools::registry::CoordContext;
            let mut env = task_tool_with(EventBus::new()).env;
            let observed = Arc::new(Mutex::new(Vec::new()));
            env.provider = Arc::new(NestedToolsetProvider {
                mock: ScriptedProvider::new(vec![
                    (
                        "start-limited",
                        MockReply::ToolCall {
                            name: "spawn_agent".into(),
                            input: json!({"name":"leaf", "task":"leaf-work", "subagent_type":"unrestricted"}),
                        },
                    ),
                    ("leaf-work", MockReply::Text("ok".into())),
                    ("spawned agent 'leaf'", MockReply::Text("ok".into())),
                    ("finished: ok", MockReply::Text("ok".into())),
                ]),
                tools: observed.clone(),
            });
            env.subagent_tools
                .add(Arc::new(crate::tools::builtin::ReadFileTool));
            env.subagent_tools
                .add(Arc::new(crate::tools::builtin::WriteFileTool));
            for (name, tools) in [
                (
                    "limited",
                    Some(vec!["read_file".into(), "spawn_agent".into()]),
                ),
                ("unrestricted", None),
            ] {
                env.definitions.insert(
                    name.into(),
                    AgentDefinition {
                        description: name.into(),
                        prompt: name.into(),
                        tools,
                        read_only: false,
                        model: None,
                    },
                );
            }
            let team = env.team.clone();
            let (mut inbox, tx) = mailbox();
            team.register("root".into(), 0, String::new(), tx);
            let mut context = ctx();
            context.coord = Some(CoordContext {
                name: "root".into(),
                depth: 0,
                team: team.clone(),
            });
            let tool = SpawnAgentTool {
                deps: CoordDeps {
                    env,
                    team: team.clone(),
                },
            };
            tool.execute(
                json!({"name":"parent", "task":"start-limited", "subagent_type":"limited"}),
                &context,
            )
            .await
            .unwrap();
            let report = tokio::time::timeout(std::time::Duration::from_secs(2), inbox.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(report.text, "finished: ok");
            assert_eq!(team.get("parent").unwrap().status(), AgentStatus::Done);
            assert_eq!(team.get("leaf").unwrap().status(), AgentStatus::Done);
            let observed = observed.lock().unwrap();
            assert!(observed.len() >= 2);
            for tools in observed.iter() {
                assert!(tools.iter().any(|tool| tool == "read_file"));
                assert!(!tools.iter().any(|tool| tool == "write_file"));
            }
        }

        #[tokio::test]
        async fn full_shared_team_rejects_direct_launches_and_workflow_waits_cancellably() {
            use crate::tools::coordinate::{CoordDeps, SpawnAgentTool};
            use crate::tools::registry::CoordContext;
            for kind in ["inline", "background", "explore", "named", "workflow"] {
                let (bus, events) = capture_events();
                let mut env = task_tool_with(bus).env;
                let provider = ScriptedProvider::new(vec![]);
                env.provider = Arc::new(provider.clone());
                let cancel = env.parent_cancel.clone();
                let team = env.team.clone();
                for index in 0..crate::agent::team::MAX_TEAM_SIZE {
                    let (_, tx) = mailbox();
                    team.register(format!("busy-{index}"), 1, "root".into(), tx);
                }
                let mut context = ctx();
                context.coord = Some(CoordContext {
                    name: "root".into(),
                    depth: 0,
                    team: team.clone(),
                });
                let (tool, input): (Box<dyn Tool>, Value) = match kind {
                    "inline" | "background" => (
                        Box::new(TaskTool { env }),
                        json!({
                            "background":kind == "background", "tasks":[{"description":"work", "prompt":"go"}]
                        }),
                    ),
                    "explore" => (Box::new(ExploreTool { env }), json!({"query":"go"})),
                    "named" => (
                        Box::new(SpawnAgentTool {
                            deps: CoordDeps {
                                env,
                                team: team.clone(),
                            },
                        }),
                        json!({"name":"new", "task":"go"}),
                    ),
                    "workflow" => (
                        Box::new(crate::tools::workflow_tool::WorkflowTool { env }),
                        json!({"shape":"fan_out", "items":["go"], "map_prompt":"{item}"}),
                    ),
                    _ => unreachable!(),
                };
                let mut execute = tool.execute(input, &context);
                if kind == "workflow" {
                    assert!(
                        std::future::poll_fn(|cx| std::task::Poll::Ready(
                            execute.as_mut().poll(cx).is_pending()
                        ))
                        .await
                    );
                    cancel.store(true, Ordering::Relaxed);
                }
                let error = tokio::time::timeout(std::time::Duration::from_secs(2), execute)
                    .await
                    .unwrap()
                    .unwrap_err();
                if kind == "workflow" {
                    assert!(error.message.contains("cancel"), "{error}");
                } else {
                    assert!(error.message.contains("maximum"), "{kind}: {error}");
                }
                assert_eq!(team.active_len(), crate::agent::team::MAX_TEAM_SIZE);
                assert!(context.jobs.list().is_empty());
                assert_eq!(provider.call_count(), 0, "{kind}");
                assert!(
                    !events
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|event| matches!(event, AgentEvent::SubagentSpawn { .. })),
                    "{kind}"
                );
            }
        }

        #[tokio::test]
        async fn invalid_later_task_never_partially_launches_a_batch() {
            for background in [false, true] {
                for invalid in [
                    json!({"description":"invalid", "prompt":"go", "subagent_type":"missing"}),
                    json!({"description":"", "prompt":"go"}),
                    json!({"description":" \n\t", "prompt":"go"}),
                    json!({"description":"invalid"}),
                    json!({"description":"invalid", "prompt":42}),
                    json!({"description":"invalid", "prompt":" \n\t"}),
                    json!({"description":"invalid", "prompt":"go", "subagent_type":false}),
                ] {
                    let (bus, events) = capture_events();
                    let mut tool = task_tool_with(bus);
                    let provider = ScriptedProvider::new(vec![]);
                    tool.env.provider = Arc::new(provider.clone());
                    let context = ctx();
                    let unknown_type = invalid["subagent_type"].as_str() == Some("missing");
                    let error = tool
                        .execute(
                            json!({"background":background,"tasks":[
                                {"description":"valid", "prompt":"go"}, invalid
                            ]}),
                            &context,
                        )
                        .await
                        .unwrap_err();
                    assert_eq!(
                        error.kind,
                        crate::tools::registry::ToolErrorKind::InvalidInput
                    );
                    if unknown_type {
                        assert!(error.message.contains("unknown subagent"));
                    }
                    assert!(tool.env.team.roster().is_empty());
                    assert_eq!(tool.env.team.active_len(), 0);
                    assert!(!tool.env.team.has_unsettled_descendants("root"));
                    assert!(context.jobs.list().is_empty());
                    assert!(tool.env.jobs.list().is_empty());
                    assert!(events.lock().unwrap().is_empty());
                    assert_eq!(provider.call_count(), 0, "background={background}");
                }
            }
        }

        #[tokio::test]
        async fn insufficient_team_capacity_rejects_the_whole_task_batch() {
            for background in [false, true] {
                let (bus, events) = capture_events();
                let mut tool = task_tool_with(bus);
                let provider = ScriptedProvider::new(vec![]);
                tool.env.provider = Arc::new(provider.clone());
                for index in 0..crate::agent::team::MAX_TEAM_SIZE - 1 {
                    let (_, tx) = mailbox();
                    tool.env
                        .team
                        .register(format!("busy-{index}"), 1, "root".into(), tx);
                }
                let before = tool.env.team.roster();
                let context = ctx();
                let error = tool
                    .execute(
                        json!({"background":background,"tasks":[
                            {"description":"one", "prompt":"first"},
                            {"description":"two", "prompt":"second"}
                        ]}),
                        &context,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.kind,
                    crate::tools::registry::ToolErrorKind::InvalidInput
                );
                assert!(error.message.contains("maximum"));
                assert_eq!(tool.env.team.roster(), before);
                assert!(context.jobs.list().is_empty());
                assert!(events.lock().unwrap().is_empty());
                assert_eq!(provider.call_count(), 0);
            }
        }

        #[tokio::test]
        async fn closed_job_admission_settles_every_prepared_child_without_running_it() {
            let (bus, events) = capture_events();
            let mut tool = task_tool_with(bus);
            let provider = ScriptedProvider::new(vec![]);
            tool.env.provider = Arc::new(provider.clone());
            let context = ctx();
            assert!(context.jobs.shutdown().await);
            let error = tool
                .execute(
                    json!({"background":true,"tasks":[
                        {"description":"one", "prompt":"first"},
                        {"description":"two", "prompt":"second"}
                    ]}),
                    &context,
                )
                .await
                .unwrap_err();
            assert_eq!(
                error.message,
                "background job admission rejected; no tasks launched"
            );
            assert_eq!(provider.call_count(), 0);
            assert!(context.jobs.list().is_empty());
            assert_eq!(tool.env.team.active_len(), 0);
            assert!(!tool.env.team.has_unsettled_descendants("root"));
            for id in ["job_1", "job_2"] {
                assert_eq!(
                    tool.env.team.get(id).unwrap().status(),
                    AgentStatus::Cancelled
                );
                let events = events.lock().unwrap();
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event,
                            AgentEvent::SubagentSpawn { agent_id, .. } if agent_id == id
                        ))
                        .count(),
                    1
                );
                assert_eq!(events.iter().filter(|event| matches!(event,
                AgentEvent::SubagentDone { agent_id, failed: false, cancelled: true } if agent_id == id
            )).count(), 1);
                assert!(!events
                    .iter()
                    .any(|event| matches!(event, AgentEvent::TurnStart { .. })));
            }
        }

        #[tokio::test]
        async fn cancellation_before_first_poll_does_not_run_child_or_cancel_sibling() {
            let (bus, events) = capture_events();
            let mut tool = task_tool_with(bus);
            let provider =
                ScriptedProvider::new(vec![("second", MockReply::Text("sibling result".into()))]);
            tool.env.provider = Arc::new(provider.clone());
            let context = ctx();
            tool.execute(
                json!({"background":true,"tasks":[
                    {"description":"one", "prompt":"must not reach provider"},
                    {"description":"two", "prompt":"second"}
                ]}),
                &context,
            )
            .await
            .unwrap();
            assert_eq!(provider.call_count(), 0);
            assert!(context.jobs.cancel_and_wait("job_1").await);
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while context.jobs.status_of("job_2") == Some(JobStatus::Running) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                context.jobs.output_of("job_1"),
                Some((JobStatus::Cancelled, String::new()))
            );
            assert_eq!(
                context.jobs.output_of("job_2"),
                Some((JobStatus::Done, "sibling result".into()))
            );
            assert_eq!(
                tool.env.team.get("job_1").unwrap().status(),
                AgentStatus::Cancelled
            );
            assert_eq!(
                tool.env.team.get("job_2").unwrap().status(),
                AgentStatus::Done
            );
            assert_eq!(provider.call_count(), 1);
            assert!(provider.steps.lock().unwrap().is_empty());
            assert!(!tool.env.team.has_unsettled_descendants("root"));
            let events = events.lock().unwrap();
            assert!(!events.iter().any(|event| matches!(event,
                AgentEvent::TurnStart { agent_id } if agent_id == "job_1"
            )));
            assert_eq!(events.iter().filter(|event| matches!(event,
            AgentEvent::SubagentDone { agent_id, failed: false, cancelled: true } if agent_id == "job_1"
        )).count(), 1);
        }

        #[tokio::test]
        async fn dropping_inline_batch_cancels_owned_children_without_detaching() {
            use crate::providers::mock::MockReply;
            let (bus, events) = capture_events();
            let mut tool = task_tool_with(bus);
            let started = Arc::new(tokio::sync::Notify::new());
            tool.env.subagent_tools.add(Arc::new(HoldingTool {
                started: started.clone(),
            }));
            tool.env.provider = Arc::new(ScriptedProvider::new(vec![(
                "go",
                MockReply::ToolCall {
                    name: "hold".into(),
                    input: json!({}),
                },
            )]));
            let context = ctx();
            let mut run = Box::pin(tool.execute(
                json!({"tasks":[{"description":"wait", "prompt":"go"}]}),
                &context,
            ));
            tokio::select! {
                _ = started.notified() => {},
                _ = &mut run => panic!("holding tool unexpectedly finished"),
            }
            drop(run);
            assert!(!tool.env.team.has_unsettled_descendants("root"));
            assert!(tool
                .env
                .team
                .roster()
                .iter()
                .all(|(_, _, status)| *status == AgentStatus::Cancelled));
            assert_eq!(
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|event| matches!(
                        event,
                        AgentEvent::SubagentDone {
                            cancelled: true,
                            ..
                        }
                    ))
                    .count(),
                1
            );
        }

        #[tokio::test]
        async fn task_consumes_message_received_during_its_final_response() {
            let (bus, events) = capture_events();
            let mut task = task_tool_with(bus);
            let provider = ScriptedProvider::new(vec![
                ("original", MockReply::Text("ok".into())),
                (
                    "new instructions",
                    MockReply::Text("message received".into()),
                ),
            ])
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
            let mut tool = task_tool_with(bus);
            tool.env.provider = Arc::new(ScriptedProvider::new(vec![
                ("first task", MockReply::Text("ok".into())),
                ("second task", MockReply::Text("ok".into())),
            ]));
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
            assert!(result
                .as_text()
                .unwrap()
                .contains("started 2 background job(s)"));
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
                .position(|event| matches!(event, AgentEvent::TurnStart { agent_id } if agent_id == id))
                .unwrap();
                let completions: Vec<_> = events.iter().enumerate().filter(|(_, event)| matches!(event,
                    AgentEvent::SubagentDone { agent_id, failed: false, cancelled: false } if agent_id == id
                )).collect();
                assert_eq!(completions.len(), 1);
                assert!(spawn < start && start < completions[0].0);
            }
        }

        struct PendingStreamProvider {
            started: Arc<tokio::sync::Notify>,
            calls: AtomicUsize,
            senders:
                Mutex<Vec<tokio::sync::mpsc::UnboundedSender<crate::core::types::StreamEvent>>>,
        }

        #[async_trait]
        impl Provider for PendingStreamProvider {
            fn name(&self) -> &str {
                "pending-stream"
            }
            fn model(&self) -> &str {
                "mock-model"
            }

            async fn generate(
                &self,
                _: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<crate::core::types::Completion> {
                panic!("cancelled stream must not request a summary")
            }

            async fn stream(
                &self,
                opts: crate::core::types::GenerateOptions,
            ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<crate::core::types::StreamEvent>>
            {
                assert_eq!(self.calls.fetch_add(1, Ordering::Relaxed), 0);
                assert_eq!(opts.messages.last().unwrap().text(), "review code");
                let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
                self.senders.lock().unwrap().push(sender);
                self.started.notify_one();
                Ok(receiver)
            }
        }

        #[tokio::test]
        async fn background_task_cancellation_is_terminal_in_registry_and_events() {
            for direct_abort in [true, false] {
                let (bus, events) = capture_events();
                let mut tool = task_tool_with(bus);
                let started = Arc::new(tokio::sync::Notify::new());
                let provider = Arc::new(PendingStreamProvider {
                    started: started.clone(),
                    calls: AtomicUsize::new(0),
                    senders: Mutex::new(Vec::new()),
                });
                tool.env.provider = provider.clone();
                let ctx = ctx();
                tool.execute(json!({"background": true, "tasks": [{"description": "review", "prompt": "review code"}]}), &ctx).await.unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
                    .await
                    .expect("child should reach its pending stream before cancellation");
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
                assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
                assert!(provider
                    .senders
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|sender| sender.is_closed()));
                assert_eq!(
                    tool.env.team.get("job_1").unwrap().status(),
                    AgentStatus::Cancelled
                );
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
            tool.execute(
            json!({"background": true, "tasks": [{"description": "review", "prompt": "review code"}]}),
            &ctx,
        )
        .await
        .unwrap();
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
    }

    mod environment {
        use crate::agent::env::*;
        use crate::core::events::EventBus;
        use crate::providers::mock::MockProvider;
        use crate::tools::jobs::JobRegistry;
        use crate::tools::registry::ToolRegistry;
        use std::collections::HashMap;
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        fn env_with(defs: Vec<(&str, AgentDefinition)>) -> AgentEnv {
            AgentEnv {
                provider: Arc::new(MockProvider::new(vec![])),
                subagent_tools: ToolRegistry::new(None),
                bus: EventBus::new(),
                cwd: ".".to_string(),
                subagent_system: Some("shared prompt".to_string()),
                jobs: JobRegistry::new(),
                team: crate::agent::team::AgentRegistry::new(),
                lsp: None,
                parent_cancel: Arc::new(AtomicBool::new(false)),
                definitions: defs.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                hooks: Default::default(),
                memory: None,
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
            let system = system.unwrap();
            assert!(system.starts_with("you are a reviewer\n\n"));
            assert!(system.contains(crate::agent::prompt::project_material_trust_block()));
        }

        #[test]
        fn read_only_and_definition_allowlists_do_not_gain_control_tools() {
            use crate::tools::builtin::ReadFileTool;
            use crate::tools::coordinate::StopAgentTool;
            let mut env = env_with(vec![("reviewer", def("review", "review", false))]);
            env.subagent_tools.add(Arc::new(ReadFileTool));
            env.subagent_tools.add(Arc::new(StopAgentTool {
                team: env.team.clone(),
            }));
            let (_, tools) = env.resolve_child(Some("reviewer"), true).unwrap();
            assert!(tools.get("read_file").is_some());
            assert!(tools.get("stop_agent").is_none());
            env.definitions.get_mut("reviewer").unwrap().tools = Some(vec!["read_file".into()]);
            let (_, tools) = env.resolve_child(Some("reviewer"), false).unwrap();
            assert!(tools.get("stop_agent").is_none());
        }

        #[tokio::test]
        async fn builtin_profiles_preserve_tools_permissions_and_anonymous_defaults() {
            use crate::core::permissions::{Decision, PermissionEngine};
            use crate::test_support::TestDir;
            use crate::tools::registry::{ToolContext, ToolErrorKind};
            let dir = TestDir::new();
            let mut env = env_with(vec![]);
            env.cwd = dir.path().to_string_lossy().into_owned();
            let mut permissions = PermissionEngine::new(Decision::Allow, None);
            permissions.add(crate::core::policies::deny_tools(vec!["write_file".into()]));
            env.subagent_tools = ToolRegistry::new(Some(Arc::new(permissions)));
            for tool in crate::tools::builtin_tools(Arc::new(
                crate::core::shell::CommandShell::resolve(std::path::Path::new("."), None),
            )) {
                env.subagent_tools.add(tool);
            }
            env.definitions =
                crate::agent::profiles::with_builtins("project instructions", HashMap::new());
            let (prompt, general) = env.resolve_child(Some("general-purpose"), false).unwrap();
            assert!(prompt.unwrap().starts_with("project instructions\n\n"));
            for name in ["bash", "write_file", "read_file"] {
                assert!(general.get(name).is_some(), "{name}");
            }
            let (_, reviewer) = env.resolve_child(Some("reviewer"), false).unwrap();
            assert!(reviewer.get("read_file").is_some());
            for name in [
                "bash",
                "write_file",
                "edit_file",
                "multi_edit",
                "spawn_agent",
                "task",
                "workflow",
            ] {
                assert!(reviewer.get(name).is_none(), "{name}");
            }
            let context = ToolContext {
                cwd: env.cwd.clone(),
                files: Arc::new(crate::tools::file_tracker::FileTracker::new()),
                todos: Arc::new(crate::tools::todo::TodoStore::new()),
                jobs: env.jobs.clone(),
                user_asker: None,
                lsp: None,
                coord: None,
                permissions: None,
            };
            let error = general
                .execute(
                    "write_file",
                    serde_json::json!({"path":"denied-test-file", "content":"not written"}),
                    &context,
                )
                .await
                .unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::PermissionDenied);
            assert!(!dir.join("denied-test-file").exists());
            for subagent_type in [None, Some("")] {
                let (anonymous, tools) = env.resolve_child(subagent_type, false).unwrap();
                assert_eq!(anonymous.as_deref(), Some("shared prompt"));
                for name in ["bash", "write_file", "read_file"] {
                    assert!(tools.get(name).is_some(), "{name}");
                }
                let error = tools
                    .execute(
                        "write_file",
                        serde_json::json!({"path":"denied-test-file", "content":"not written"}),
                        &context,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(error.kind, ToolErrorKind::PermissionDenied);
                assert!(!dir.join("denied-test-file").exists());
                let (prompt, read_only) = env.resolve_child(subagent_type, true).unwrap();
                let prompt = prompt.unwrap();
                assert!(prompt.starts_with("shared prompt\n\n"));
                assert!(prompt.contains(crate::agent::prompt::project_material_trust_block()));
                assert!(read_only.get("read_file").is_some());
                assert!(read_only.get("write_file").is_none());
                assert!(read_only.get("bash").is_none());
            }
            env.subagent_system = None;
            assert!(env.resolve_child(None, false).unwrap().0.is_none());
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
        fn unsupported_model_overrides_fail_explicitly() {
            let mut definition = def("d", "p", false);
            definition.model = Some("other-model".into());
            let env = env_with(vec![("custom", definition)]);
            assert!(env
                .resolve_child(Some("custom"), false)
                .err()
                .unwrap()
                .contains("model overrides are not supported"));
        }
    }

    mod assembly {
        use crate::agent::assembly::{build_root_agent, RootAgentParams};
        use crate::agent::team::AgentRegistry;
        use crate::core::events::EventBus;
        use crate::core::permissions::{Decision, PermissionEngine};
        use crate::core::types::{Completion, GenerateOptions, Role, StreamEvent, ToolSpec};
        use crate::providers::mock::{MockProvider, MockReply};
        use crate::providers::provider::Provider;
        use crate::test_support::TestDir;
        use crate::tools::jobs::JobRegistry;
        use crate::tools::registry::{Tool, ToolContext, ToolResult, UserAsker, UserQuery};
        use async_trait::async_trait;
        use serde_json::{json, Value};
        use std::sync::{Arc, Mutex};

        struct NoQuestions;

        #[async_trait]
        impl UserAsker for NoQuestions {
            async fn ask(&self, _: &UserQuery) -> Option<String> {
                panic!("assembly fixture must not ask the user")
            }
        }

        struct ExternalTool(&'static str);

        #[async_trait]
        impl Tool for ExternalTool {
            fn spec(&self) -> ToolSpec {
                ToolSpec {
                    name: self.0.into(),
                    description: "external impostor".into(),
                    input_schema: json!({}),
                }
            }

            async fn execute(&self, _: Value, _: &ToolContext) -> ToolResult {
                panic!("external impostor must never execute")
            }
        }

        struct AssemblyProvider {
            read_only: bool,
            observed: Mutex<Vec<Vec<ToolSpec>>>,
        }

        #[async_trait]
        impl Provider for AssemblyProvider {
            fn name(&self) -> &str {
                "assembly-fixture"
            }
            fn model(&self) -> &str {
                "mock-model"
            }

            async fn generate(&self, _: GenerateOptions) -> anyhow::Result<Completion> {
                panic!("assembly fixture must not request a summary")
            }

            async fn stream(
                &self,
                opts: GenerateOptions,
            ) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<StreamEvent>> {
                let step = {
                    let mut observed = self.observed.lock().unwrap();
                    let step = observed.len();
                    observed.push(opts.tools.clone());
                    step
                };
                let last = opts
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role != Role::Assistant)
                    .unwrap();
                let reply = match step {
                    0 => {
                        assert_eq!(last.text(), "launch-child");
                        MockReply::ToolCall {
                            name: "task".into(),
                            input: json!({"read_only":self.read_only,"tasks":[
                                {"description":"inspect", "prompt":"inspect-child"}
                            ]}),
                        }
                    }
                    1 => {
                        assert_eq!(last.text(), "inspect-child");
                        MockReply::Text("child inspected".into())
                    }
                    2 => {
                        assert_eq!(last.role, Role::Tool);
                        assert_eq!(last.content.len(), 1);
                        match &last.content[0] {
                            crate::core::types::ContentBlock::ToolResult {
                                content,
                                is_error,
                                ..
                            } => {
                                assert_eq!(
                                    content.as_text().unwrap(),
                                    "### inspect\nchild inspected"
                                );
                                assert_eq!(*is_error, Some(false));
                            }
                            other => panic!("expected the delegated tool result, got {other:?}"),
                        }
                        MockReply::Text("assembly verified".into())
                    }
                    _ => panic!("unexpected assembly provider call {step}"),
                };
                MockProvider::new(vec![])
                    .with_default(reply)
                    .stream(opts)
                    .await
            }
        }

        #[tokio::test]
        async fn external_tools_cannot_shadow_agent_control_entry_points() {
            for read_only in [false, true] {
                let dir = TestDir::new();
                let provider = Arc::new(AssemblyProvider {
                    read_only,
                    observed: Mutex::new(Vec::new()),
                });
                let impostors = || {
                    [
                        "spawn_agent",
                        "task",
                        "workflow",
                        "explore",
                        "memory",
                        "stop_agent",
                    ]
                    .into_iter()
                    .map(|name| Arc::new(ExternalTool(name)) as Arc<dyn Tool>)
                    .collect()
                };
                let mut root = build_root_agent(RootAgentParams {
                    provider: provider.clone(),
                    permissions: Arc::new(PermissionEngine::new(Decision::Allow, None)),
                    bus: EventBus::new(),
                    jobs: JobRegistry::new(),
                    team: AgentRegistry::new(),
                    cwd: dir.path().to_string_lossy().into_owned(),
                    system_prompt: String::new(),
                    shell: Arc::new(crate::core::shell::CommandShell::resolve(dir.path(), None)),
                    mcp_tools: impostors(),
                    extra_tools: impostors(),
                    lsp: None,
                    user_asker: Arc::new(NoQuestions),
                    max_turns: Some(3),
                    definitions: Default::default(),
                    hooks: Default::default(),
                    memory: None,
                });
                for name in ["task", "spawn_agent"] {
                    let specs = root.tool_specs();
                    let description = &specs
                        .iter()
                        .find(|tool| tool.name == name)
                        .unwrap()
                        .description;
                    assert!(description.contains("general-purpose:"));
                    assert!(description.contains("reviewer:"));
                }
                assert_eq!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        root.run("launch-child")
                    )
                    .await
                    .unwrap()
                    .unwrap(),
                    "assembly verified"
                );
                let observed = provider.observed.lock().unwrap();
                assert_eq!(observed.len(), 3);
                let root_tools = &observed[0];
                for name in ["spawn_agent", "stop_agent", "task", "workflow", "explore"] {
                    assert_ne!(
                        root_tools
                            .iter()
                            .find(|tool| tool.name == name)
                            .unwrap()
                            .description,
                        "external impostor"
                    );
                }
                assert!(!root_tools.iter().any(|tool| tool.name == "memory"));
                let child_tools = &observed[1];
                for name in ["spawn_agent", "task", "workflow", "explore", "memory"] {
                    assert!(!child_tools.iter().any(|tool| tool.name == name), "{name}");
                }
                assert!(child_tools.iter().any(|tool| tool.name == "read_file"));
                let stop = child_tools.iter().find(|tool| tool.name == "stop_agent");
                if read_only {
                    assert!(stop.is_none());
                } else {
                    assert_ne!(stop.unwrap().description, "external impostor");
                }
            }
        }
    }
}
