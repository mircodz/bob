//! The workflow engine: deterministic multi-agent orchestration that sits ABOVE
//! the agent loop. Where `spawn_agent`/`task` let the *model* decide what to run,
//! a workflow is *code* (a Rust `async fn`) that fans agents out, pipelines them,
//! and branches on their **structured** results — the same shape every run.
//!
//! It is a thin layer over machinery that already exists:
//!   - each workflow agent is a normal subagent (`build_subagent`),
//!   - schema-forced output comes from `Agent::run_structured`,
//!   - concurrency is bounded by a `Semaphore` (cap = `MAX_TEAM_SIZE`),
//!   - a `Cancel` still cascades via the shared `parent_cancel` flag (#5),
//!   - progress is reported over the existing `EventBus` (+ two workflow events).
//!
//! Parameterized shapes and the JSON steps interpreter share these primitives.

use crate::agent::agent::{build_subagent, SubagentSpec, SUBAGENT_MAX_TURNS};
use crate::agent::team::{mailbox, AgentRegistry, AgentStatus, MAX_TEAM_SIZE};
use crate::core::events::{AgentEvent, EventBus};
use crate::providers::provider::Provider;
use crate::tools::registry::ToolRegistry;
use futures::FutureExt;
use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Everything the engine needs to build + run agents and report progress. Cheap to
/// clone (all shared handles), so it can be moved into each spawned task.
#[derive(Clone)]
pub struct WorkflowContext {
    /// A unique id for this workflow run, tagged on every emitted event so a UI can
    /// group the run's agents + phases into one live tree.
    pub id: String,
    pub provider: Arc<dyn Provider>,
    pub bus: EventBus,
    pub cwd: String,
    /// The tool set every workflow agent gets (builtins + LSP + MCP, minus the
    /// coordination tools — workflow agents don't spawn their own team).
    pub tools: ToolRegistry,
    pub system: Option<String>,
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    pub jobs: crate::tools::jobs::JobRegistry,
    pub team: AgentRegistry,
    cancelled_agents: Arc<std::sync::Mutex<Vec<String>>>,
    outcomes: Arc<std::sync::Mutex<Vec<(usize, AgentOutcome)>>>,
    /// Shared cancel flag: set by the frontend on Cancel; threaded into every agent
    /// as `parent_cancel` so one Cancel stops the whole workflow (reuses #5).
    pub cancel: Arc<AtomicBool>,
    /// Bounds how many agents run at once (excess queue). Shared across the run.
    limiter: Arc<Semaphore>,
    /// Monotonic counter for unique per-agent ids within the run.
    counter: Arc<AtomicUsize>,
}

impl WorkflowContext {
    /// Build a context. `limit` caps concurrent agents (defaults to `MAX_TEAM_SIZE`
    /// when 0). `id` groups this run's events.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        provider: Arc<dyn Provider>,
        bus: EventBus,
        cwd: String,
        tools: ToolRegistry,
        system: Option<String>,
        lsp: Option<Arc<crate::lsp::LspManager>>,
        jobs: crate::tools::jobs::JobRegistry,
        cancel: Arc<AtomicBool>,
        limit: usize,
    ) -> Self {
        let limit = if limit == 0 { MAX_TEAM_SIZE } else { limit };
        WorkflowContext {
            id,
            provider,
            bus,
            cwd,
            tools,
            system,
            lsp,
            jobs,
            team: AgentRegistry::new(),
            cancelled_agents: Arc::new(std::sync::Mutex::new(Vec::new())),
            outcomes: Arc::new(std::sync::Mutex::new(Vec::new())),
            cancel,
            limiter: Arc::new(Semaphore::new(limit)),
            counter: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_team(mut self, team: AgentRegistry) -> Self {
        self.team = team;
        self
    }

    pub fn cancelled_agents(&self) -> Vec<String> {
        self.cancelled_agents.lock().unwrap().clone()
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || !self.cancelled_agents.lock().unwrap().is_empty()
    }

    pub fn outcomes(&self) -> Vec<AgentOutcome> {
        let mut outcomes = self.outcomes.lock().unwrap().clone();
        outcomes.sort_by_key(|(sequence, _)| *sequence);
        outcomes.into_iter().map(|(_, outcome)| outcome).collect()
    }

    fn record(&self, sequence: usize, outcome: AgentOutcome) -> AgentOutcome {
        if outcome.status == OutcomeStatus::Cancelled {
            self.cancelled_agents
                .lock()
                .unwrap()
                .push(outcome.id.clone());
        }
        self.outcomes
            .lock()
            .unwrap()
            .push((sequence, outcome.clone()));
        outcome
    }

    fn next_agent_name(&self, label: &str) -> (usize, String) {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        // A stable, readable id: "<workflow>.<n>-<label>", label slugged loosely.
        let slug: String = label
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .collect();
        (n, format!("{}.{n}-{}", self.id, slug))
    }

    /// Announce a phase boundary; subsequent agents render under `title`.
    pub fn phase(&self, title: &str, index: usize, total: usize) {
        self.bus.emit(AgentEvent::WorkflowPhase {
            workflow_id: self.id.clone(),
            title: title.to_string(),
            index,
            total,
        });
    }

    /// Emit a free-form progress line for this run.
    pub fn log(&self, message: impl Into<String>) {
        self.bus.emit(AgentEvent::WorkflowLog {
            workflow_id: self.id.clone(),
            message: message.into(),
        });
    }
}

/// Marker prefix on the follow-up user turn injected after a workflow finishes, so
/// the frontend can recognize that message on resume (and anchor the persisted
/// workflow tree to it) rather than showing it as a literal user turn. Mirrors the
/// coordination-message marker pattern in `agent::team`.
pub const HANDOFF_PREFIX: &str = "[workflow result]";

/// Build the follow-up prompt fed to the root agent after a workflow finishes. It
/// carries the run's name + structured result and asks the agent to act on it. The
/// `HANDOFF_PREFIX` lets the frontend detect + special-case it on resume.
pub fn handoff_prompt(workflow_name: &str, result_json: &str) -> String {
    format!(
        "{HANDOFF_PREFIX} The `{workflow_name}` workflow finished. Its structured result is \
         below. Use it to continue the task — summarize the key findings for me and take any \
         obvious next step.\n\n```json\n{result_json}\n```"
    )
}

/// Whether `text` is a workflow hand-off message (see [`handoff_prompt`]).
pub fn is_handoff(text: &str) -> bool {
    text.starts_with(HANDOFF_PREFIX)
}

/// One unit of work: a prompt, an optional output schema, and a display label.
#[derive(Clone)]
pub struct AgentSpec {
    pub prompt: String,
    /// When set, the agent is forced to return a value matching this JSON Schema and
    /// the outcome carries that `Value`. When `None`, the agent's final text
    /// is wrapped as `{"text": "..."}`.
    pub schema: Option<Value>,
    pub label: String,
    pub input: Option<Value>,
}

impl AgentSpec {
    pub fn new(prompt: impl Into<String>, label: impl Into<String>) -> Self {
        AgentSpec {
            prompt: prompt.into(),
            schema: None,
            label: label.into(),
            input: None,
        }
    }

    pub fn with_input(mut self, input: Value) -> Self {
        self.input = Some(input);
        self
    }

    pub fn with_schema(mut self, schema: Value) -> Self {
        self.schema = Some(schema);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeStatus {
    Success,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentOutcome {
    pub id: String,
    pub label: String,
    pub input: Option<Value>,
    pub status: OutcomeStatus,
    pub output: Option<Value>,
    pub error: Option<String>,
}

impl AgentOutcome {
    pub fn is_success(&self) -> bool {
        self.status == OutcomeStatus::Success
    }

    pub fn failure_message(&self) -> String {
        format!(
            "{}: {}",
            self.label,
            self.error.as_deref().unwrap_or("agent failed")
        )
    }
}

#[derive(Debug)]
pub struct WorkflowRun {
    pub output: Value,
    pub error: Option<String>,
}

impl WorkflowRun {
    pub fn success(output: Value) -> Self {
        Self {
            output,
            error: None,
        }
    }

    pub fn failed(output: Value, error: impl Into<String>) -> Self {
        Self {
            output,
            error: Some(error.into()),
        }
    }
}

pub async fn agent(ctx: &WorkflowContext, spec: AgentSpec) -> AgentOutcome {
    let (sequence, name) = ctx.next_agent_name(&spec.label);
    let mut result = AgentOutcome {
        id: name.clone(),
        label: spec.label.clone(),
        input: spec.input.clone(),
        status: OutcomeStatus::Cancelled,
        output: None,
        error: Some("cancelled before starting".into()),
    };
    if ctx.is_cancelled() {
        return ctx.record(sequence, result);
    }
    let _permit = ctx
        .limiter
        .acquire()
        .await
        .expect("workflow semaphore remains open");
    if ctx.is_cancelled() {
        return ctx.record(sequence, result);
    }

    let (inbox, tx) = mailbox();
    let handle = ctx.team.register(name.clone(), 1, "root".into(), tx);
    let mut lifecycle = crate::agent::lifecycle::SubagentLifecycle::start(
        ctx.bus.clone(),
        ctx.id.clone(),
        name.clone(),
        spec.label.clone(),
        spec.prompt.clone(),
    )
    .with_handle(handle.clone());

    let mut child = build_subagent(SubagentSpec {
        provider: ctx.provider.clone(),
        tools: ctx.tools.clone(),
        bus: ctx.bus.clone(),
        system: ctx.system.clone(),
        cwd: ctx.cwd.clone(),
        jobs: ctx.jobs.clone(),
        lsp: ctx.lsp.clone(),
        name,
        max_turns: SUBAGENT_MAX_TURNS,
        depth: 1,
        inbox: Some(inbox),
        team: Some(ctx.team.clone()),
        parent_cancel: Some(ctx.cancel.clone()),
    });

    let work = async {
        match spec.schema.clone() {
            Some(schema) => child.run_structured(&spec.prompt, schema).await,
            None => child
                .run(&spec.prompt)
                .await
                .map(|text| serde_json::json!({ "text": text })),
        }
    };
    let outcome = std::panic::AssertUnwindSafe(handle.run_until_stopped(ctx.cancel.clone(), work))
        .catch_unwind()
        .await;
    let (mut output, mut error, cancelled) = match outcome {
        Ok(Some(Ok(output))) => (Some(output), None, child.is_cancelled()),
        Ok(Some(Err(error))) => (None, Some(error.to_string()), child.is_cancelled()),
        Ok(None) => (None, Some("agent cancelled".into()), true),
        Err(panic) => {
            let message = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            (None, Some(format!("agent panicked: {message}")), false)
        }
    };
    if let Some(summary) = child.turn_limit_summary() {
        output.get_or_insert_with(|| serde_json::json!({"text": summary}));
        error = Some(format!(
            "agent reached its {SUBAGENT_MAX_TURNS}-turn limit without finishing"
        ));
    }
    let status = lifecycle.finish(error.is_some() && !cancelled, cancelled);
    result.status = match status {
        AgentStatus::Cancelled => OutcomeStatus::Cancelled,
        AgentStatus::Failed => OutcomeStatus::Failed,
        _ => OutcomeStatus::Success,
    };
    if result.status == OutcomeStatus::Cancelled {
        result.error = Some("agent cancelled".into());
    } else {
        result.output = output;
        result.error = error;
    }
    ctx.record(sequence, result)
}

pub async fn parallel<F>(thunks: Vec<F>) -> Vec<AgentOutcome>
where
    F: std::future::Future<Output = AgentOutcome> + Send,
{
    futures::future::join_all(thunks).await
}

/// Run each item through the full chain of `stages` INDEPENDENTLY — no barrier
/// between stages, so item A can be in stage 3 while item B is still in stage 1.
/// Wall-clock is the slowest single-item chain, not the sum of per-stage maxima.
/// Each stage maps the previous stage's `Value` to the next; a stage returning
/// `None` drops that item (its remaining stages are skipped) and the item's result
/// is `None`.
pub async fn pipeline<I>(items: Vec<I>, stages: Vec<Stage<I>>) -> Vec<Option<Value>>
where
    I: Clone + Send + 'static,
{
    let stages = Arc::new(stages);
    let handles: Vec<_> = items
        .into_iter()
        .map(|item| {
            let stages = stages.clone();
            tokio::spawn(async move {
                let mut current: Option<Value> = None;
                for (i, stage) in stages.iter().enumerate() {
                    current = stage(current.clone(), item.clone(), i).await;
                    if current.is_none() {
                        break; // a stage dropped this item; skip the rest
                    }
                }
                current
            })
        })
        .collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.ok().flatten());
    }
    out
}

/// A pipeline stage: `(previous_result, original_item, stage_index) -> next_result`.
/// Boxed so a `Vec` of heterogeneous closures can share one type.
pub type Stage<I> = Box<
    dyn Fn(Option<Value>, I, usize) -> futures::future::BoxFuture<'static, Option<Value>>
        + Send
        + Sync,
>;

pub mod dsl;
pub(crate) mod input;
pub mod params;
pub(crate) mod validation;

#[cfg(test)]
mod runtime_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock::{MockProvider, MockReply, MockRule};
    use serde_json::json;

    fn ctx_with(provider: Arc<dyn Provider>, limit: usize) -> WorkflowContext {
        WorkflowContext::new(
            "wf".to_string(),
            provider,
            EventBus::new(),
            ".".to_string(),
            ToolRegistry::new(None),
            None,
            None,
            crate::tools::jobs::JobRegistry::new(),
            Arc::new(AtomicBool::new(false)),
            limit,
        )
    }

    #[tokio::test]
    async fn parallel_preserves_failed_outcomes_and_input_order() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let ctx = ctx_with(provider, 2);
        let specs = vec![
            AgentSpec::new("first", "first"),
            AgentSpec::new("no structured result", "failed").with_schema(json!({"type":"object"})),
            AgentSpec::new("last", "last"),
        ];
        let out = parallel(specs.into_iter().map(|spec| agent(&ctx, spec)).collect()).await;
        assert_eq!(
            out.iter().map(|v| v.label.as_str()).collect::<Vec<_>>(),
            ["first", "failed", "last"]
        );
        assert_eq!(
            out.iter().map(|v| v.status).collect::<Vec<_>>(),
            [
                OutcomeStatus::Success,
                OutcomeStatus::Failed,
                OutcomeStatus::Success
            ]
        );
        assert!(out[1].error.as_ref().unwrap().contains("structured"));
        assert_eq!(ctx.outcomes().len(), 3);
    }

    #[tokio::test]
    async fn pipeline_has_no_barrier_between_stages() {
        // A slow item and a fast item share a 2-stage pipeline. With NO barrier, the
        // fast item completes stage 2 before the slow item finishes stage 1. We
        // record completion order and assert the fast item finishes first.
        use std::sync::Mutex as StdMutex;
        let order = Arc::new(StdMutex::new(Vec::<u64>::new()));

        let stage1: Stage<u64> = {
            Box::new(move |_prev, item, _i| {
                Box::pin(async move {
                    // Item 1 is slow in stage 1; item 2 is fast.
                    let ms = if item == 1 { 80 } else { 5 };
                    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                    Some(json!(item))
                })
            })
        };
        let order2 = order.clone();
        let stage2: Stage<u64> = Box::new(move |prev, _item, _i| {
            let order2 = order2.clone();
            Box::pin(async move {
                let v = prev.unwrap().as_u64().unwrap();
                order2.lock().unwrap().push(v);
                Some(json!(v))
            })
        });

        let out = pipeline(vec![1u64, 2u64], vec![stage1, stage2]).await;
        assert_eq!(out, vec![Some(json!(1)), Some(json!(2))]);
        // The fast item (2) reached stage 2 first — proving stages aren't barriered.
        assert_eq!(order.lock().unwrap()[0], 2);
    }

    #[tokio::test]
    async fn pipeline_drops_item_when_a_stage_returns_none() {
        let stage1: Stage<u64> = Box::new(|_p, item, _i| {
            Box::pin(async move {
                if item == 0 {
                    None
                } else {
                    Some(json!(item))
                }
            })
        });
        let stage2: Stage<u64> = Box::new(|prev, _item, _i| Box::pin(async move { prev }));
        let out = pipeline(vec![0u64, 7u64], vec![stage1, stage2]).await;
        assert_eq!(out, vec![None, Some(json!(7))]);
    }

    #[tokio::test]
    async fn agent_returns_structured_value() {
        let provider = MockProvider::new(vec![MockRule {
            needle: "structured_output".to_string(),
            reply: MockReply::ToolCall {
                name: "structured_output".to_string(),
                input: json!({"point": "fast"}),
            },
        }]);
        let ctx = ctx_with(Arc::new(provider), 4);
        let schema = json!({"type": "object", "required": ["point"]});
        let out = agent(&ctx, AgentSpec::new("say a point", "p").with_schema(schema)).await;
        assert!(out.is_success());
        assert_eq!(out.output, Some(json!({"point": "fast"})));
    }

    #[tokio::test]
    async fn agent_reports_cancellation_before_starting() {
        let provider = MockProvider::new(vec![]);
        let ctx = ctx_with(Arc::new(provider.clone()), 4);
        ctx.cancel.store(true, Ordering::Relaxed);
        let out = agent(&ctx, AgentSpec::new("anything", "p")).await;
        assert_eq!(out.status, OutcomeStatus::Cancelled);
        assert_eq!(provider.call_count(), 0);
        assert!(ctx.team.roster().is_empty());
        assert_eq!(ctx.outcomes().len(), 1);
    }

    #[tokio::test]
    async fn semaphore_caps_concurrency() {
        // With a limit of 2 and a per-call delay, 4 agents can't all run at once.
        // The mock records total calls; we assert peak in-flight never exceeds the
        // limit by timing: with limit=2 and 4×50ms calls, wall time ≥ ~100ms.
        let provider = MockProvider::new(vec![])
            .with_default(MockReply::Text("ok".into()))
            .with_delay(std::time::Duration::from_millis(50));
        let ctx = ctx_with(Arc::new(provider), 2);
        let thunks: Vec<_> = (0..4)
            .map(|i| {
                let ctx = ctx.clone();
                async move { agent(&ctx, AgentSpec::new("go", format!("a{i}"))).await }
            })
            .collect();
        let start = tokio::time::Instant::now();
        let out = parallel(thunks).await;
        let elapsed = start.elapsed();
        assert_eq!(out.iter().filter(|v| v.is_success()).count(), 4);
        // 4 calls / 2 slots × 50ms = ~100ms floor; a no-cap run would be ~50ms.
        assert!(
            elapsed >= std::time::Duration::from_millis(90),
            "expected serialized batches, got {elapsed:?}"
        );
    }
}
