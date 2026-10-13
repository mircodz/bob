//! Shared execution primitives for parameterized shapes and the JSON steps interpreter.

use crate::agent::agent::{build_subagent, SubagentSpec, SUBAGENT_MAX_TURNS};
use crate::agent::team::{mailbox, AgentRegistry, AgentStatus, MAX_TEAM_SIZE};
use crate::core::events::{AgentEvent, EventBus};
use crate::providers::provider::Provider;
use crate::tools::registry::ToolRegistry;
use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Semaphore;

const ADMISSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone)]
pub struct WorkflowContext {
    /// Groups this run's events in the workflow view.
    pub id: String,
    pub provider: Arc<dyn Provider>,
    pub bus: EventBus,
    pub cwd: String,
    /// Excludes coordination tools; workflow agents cannot spawn their own team.
    pub tools: ToolRegistry,
    pub system: Option<String>,
    pub hooks: crate::agent::hooks::Hooks,
    pub memory: Option<Arc<dyn crate::core::memory::MemoryRepository>>,
    parent: String,
    depth: usize,
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    pub jobs: crate::tools::jobs::JobRegistry,
    pub team: AgentRegistry,
    cancelled_agents: Arc<std::sync::Mutex<Vec<String>>>,
    outcomes: Arc<std::sync::Mutex<Vec<(usize, AgentOutcome)>>>,
    /// Shared with every agent as `parent_cancel` to stop the whole workflow.
    pub cancel: Arc<AtomicBool>,
    limiter: Arc<Semaphore>,
    counter: Arc<AtomicUsize>,
}

impl WorkflowContext {
    /// A zero `limit` uses `MAX_TEAM_SIZE`; larger limits are capped there.
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
        let limit = if limit == 0 {
            MAX_TEAM_SIZE
        } else {
            limit.min(MAX_TEAM_SIZE)
        };
        WorkflowContext {
            id,
            provider,
            bus,
            cwd,
            tools,
            system,
            hooks: Default::default(),
            memory: None,
            parent: "root".into(),
            depth: 1,
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

    pub fn with_hooks(mut self, hooks: crate::agent::hooks::Hooks) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn with_memory(
        mut self,
        memory: Option<Arc<dyn crate::core::memory::MemoryRepository>>,
    ) -> Self {
        self.memory = memory;
        self
    }

    pub fn with_parent(mut self, parent: String, depth: usize) -> Self {
        self.parent = parent;
        self.depth = depth;
        self
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

    fn admission_cancelled(&self) -> bool {
        self.is_cancelled()
            || self.team.get(&self.parent).is_some_and(|parent| {
                parent.cancellation_requested() || parent.status() != AgentStatus::Running
            })
    }

    async fn wait_for_admission_cancel(&self) {
        while !self.admission_cancelled() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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

    pub fn log(&self, message: impl Into<String>) {
        self.bus.emit(AgentEvent::WorkflowLog {
            workflow_id: self.id.clone(),
            message: message.into(),
        });
    }
}

/// Identifies synthetic handoffs on resume so the UI can anchor the workflow tree.
pub const HANDOFF_PREFIX: &str = "[workflow result]";

pub fn handoff_prompt(workflow_name: &str, result_json: &str) -> String {
    format!(
        "{HANDOFF_PREFIX} The `{workflow_name}` workflow finished. Its structured result is \
         below. Use it to continue the task — summarize the key findings for me and take any \
         obvious next step.\n\n```json\n{result_json}\n```"
    )
}

pub fn is_handoff(text: &str) -> bool {
    text.starts_with(HANDOFF_PREFIX)
}

#[derive(Clone)]
pub struct AgentSpec {
    pub prompt: String,
    /// Without a schema, final text is wrapped as `{"text": "..."}`.
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
    if sequence > 1024 {
        result.status = OutcomeStatus::Failed;
        result.error = Some("workflow exceeds 1024 agent executions".into());
        return ctx.record(sequence, result);
    }
    if ctx.is_cancelled() {
        return ctx.record(sequence, result);
    }
    if let Some(schema) = &spec.schema {
        if let Err(error) = crate::tools::schema::CompiledSchema::compile(schema) {
            result.status = OutcomeStatus::Failed;
            result.error = Some(error);
            return ctx.record(sequence, result);
        }
    }
    let _permit = tokio::select! {
        biased;
        _ = ctx.wait_for_admission_cancel() => return ctx.record(sequence, result),
        permit = ctx.limiter.acquire() => permit.expect("workflow semaphore remains open"),
    };

    let (inbox, tx) = mailbox();
    let handle = match ctx
        .team
        .register_when_available(
            name.clone(),
            ctx.depth,
            ctx.parent.clone(),
            tx,
            || ctx.admission_cancelled(),
            ADMISSION_TIMEOUT,
        )
        .await
    {
        Ok(Some(handle)) => handle,
        Ok(None) => return ctx.record(sequence, result),
        Err(error) => {
            result.status = OutcomeStatus::Failed;
            result.error = Some(error);
            return ctx.record(sequence, result);
        }
    };
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
        depth: ctx.depth,
        inbox: Some(inbox),
        team: Some(ctx.team.clone()),
        parent_cancel: Some(ctx.cancel.clone()),
        hooks: ctx.hooks.clone(),
        memory: ctx.memory.clone(),
    });

    let child_cancel = child.cancel_handle();
    let work = async {
        match spec.schema.clone() {
            Some(schema) => child.run_structured(&spec.prompt, schema).await,
            None => child
                .run(&spec.prompt)
                .await
                .map(|text| serde_json::json!({ "text": text })),
        }
    };
    let outcome =
        crate::core::panic::catch(handle.run_until_stopped(ctx.cancel.clone(), child_cancel, work))
            .await;
    let mutations_settled = child.settle_owned_mutations().await;
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
    if !mutations_settled {
        error = Some("memory mutation cleanup timed out; changes may still complete".into());
    }
    let status = lifecycle.finish(error.is_some() && !cancelled, cancelled);
    result.status = match status {
        AgentStatus::Cancelled => OutcomeStatus::Cancelled,
        AgentStatus::Failed => OutcomeStatus::Failed,
        _ => OutcomeStatus::Success,
    };
    if result.status == OutcomeStatus::Cancelled {
        result.error = error.or_else(|| Some("agent cancelled".into()));
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

/// Items advance independently, without a barrier between stages.
/// A stage returning `None` skips that item's remaining stages, preserving its slot.
pub async fn pipeline<I>(items: Vec<I>, stages: Vec<Stage<I>>) -> Vec<Option<Value>>
where
    I: Clone + Send + 'static,
{
    let stages = Arc::new(stages);
    let handles: Vec<_> = items
        .into_iter()
        .map(|item| {
            let stages = stages.clone();
            async move {
                let mut current: Option<Value> = None;
                for (i, stage) in stages.iter().enumerate() {
                    current = stage(current.clone(), item.clone(), i).await;
                    if current.is_none() {
                        break;
                    }
                }
                current
            }
        })
        .collect();
    futures::future::join_all(handles).await
}

/// A pipeline stage: `(previous_result, original_item, stage_index) -> next_result`.
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
mod tests {
    use super::*;
    use crate::core::types::{
        Completion, ContentBlock, GenerateOptions, Role, StreamEvent, ToolSpec,
    };
    use crate::providers::mock::{MockReply, MockRule};
    use crate::tools::registry::{Tool, ToolContext, ToolResult};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot, Notify};
    use tokio::time::timeout;

    const DEADLINE: Duration = Duration::from_secs(5);

    #[derive(Clone)]
    pub(super) struct MockProvider(Arc<Script>);

    struct Script {
        mock: crate::providers::mock::MockProvider,
        rules: Vec<MockRule>,
        unexpected: Mutex<Vec<String>>,
    }

    impl Drop for Script {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                assert!(
                    self.unexpected.get_mut().unwrap().is_empty(),
                    "unexpected provider calls: {:?}",
                    self.unexpected.get_mut().unwrap()
                );
            }
        }
    }

    impl MockProvider {
        pub(super) fn new(rules: Vec<MockRule>) -> Self {
            Self(Arc::new(Script {
                mock: crate::providers::mock::MockProvider::new(rules.clone()),
                rules,
                unexpected: Mutex::new(Vec::new()),
            }))
        }

        pub(super) fn call_count(&self) -> usize {
            self.0.mock.call_count()
        }

        fn check(&self, opts: &GenerateOptions) -> anyhow::Result<()> {
            let message = opts
                .messages
                .iter()
                .rev()
                .find(|message| message.role != Role::Assistant)
                .expect("provider requires a prompt");
            let prompt = message.text();
            let scripted = self
                .0
                .rules
                .iter()
                .any(|rule| prompt.contains(&rule.needle));
            let tool_reply = !message.content.is_empty() && message.content.iter().all(|block| {
            let ContentBlock::ToolResult { tool_use_id, .. } = block else { return false; };
            opts.messages.iter().flat_map(|message| &message.content).any(|block| {
                matches!(block, ContentBlock::ToolUse { id, name, .. }
                    if id == tool_use_id && self.0.rules.iter().any(|rule|
                        matches!(&rule.reply, MockReply::ToolCall { name: allowed, .. } if allowed == name)))
            })
        });
            if !scripted && !tool_reply {
                self.0.unexpected.lock().unwrap().push(prompt.clone());
                anyhow::bail!("unexpected provider call: {prompt}");
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        fn name(&self) -> &str {
            "workflow-script"
        }
        fn model(&self) -> &str {
            self.0.mock.model()
        }

        async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
            self.check(&opts)?;
            self.0.mock.generate(opts).await
        }

        async fn stream(
            &self,
            opts: GenerateOptions,
        ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
            self.check(&opts)?;
            self.0.mock.stream(opts).await
        }
    }

    pub(super) fn text(needle: &str) -> MockRule {
        MockRule {
            needle: needle.into(),
            reply: MockReply::Text("ok".into()),
        }
    }

    pub(super) fn context(provider: Arc<dyn Provider>, limit: usize) -> WorkflowContext {
        WorkflowContext::new(
            "wf-runtime".into(),
            provider,
            EventBus::new(),
            ".".into(),
            ToolRegistry::new(None),
            None,
            None,
            crate::tools::jobs::JobRegistry::new(),
            Arc::new(AtomicBool::new(false)),
            limit,
        )
    }

    pub(super) fn structured(needle: &str, output: Value) -> MockRule {
        MockRule {
            needle: needle.into(),
            reply: MockReply::ToolCall {
                name: "structured_output".into(),
                input: output,
            },
        }
    }

    pub(super) fn spec(steps: Value) -> dsl::Spec {
        let input = json!({"steps": steps});
        input::validate(&input).expect("runtime fixtures must be structurally valid");
        let spec = serde_json::from_value(input).unwrap();
        validation::validate(&spec).expect("runtime fixtures must pass reference validation");
        spec
    }

    pub(super) async fn run_checked(ctx: &WorkflowContext, spec: dsl::Spec) -> WorkflowRun {
        timeout(DEADLINE, dsl::run(ctx.clone(), spec))
            .await
            .expect("workflow did not finish within the deadline")
    }

    pub(super) fn assert_error(run: &WorkflowRun, expected: &str) {
        let error = run.error.as_deref().expect("workflow should fail");
        assert!(
            error.contains(expected),
            "expected {expected:?}, got {error:?}"
        );
    }

    pub(super) fn check_loop() -> dsl::Spec {
        spec(json!([
            {"id": "rounds", "loop": {
                "max": 3,
                "until": "$check.done",
                "steps": [{"id": "check", "agent": {
                    "prompt": "check condition",
                    "schema": {"type": "object"}
                }}]
            }},
            {"id": "after", "agent": {"prompt": "after loop"}}
        ]))
    }

    #[tokio::test]
    async fn public_workflow_entry_points_enforce_limits_before_work() {
        let provider = MockProvider::new(vec![]);
        let ctx = context(Arc::new(provider.clone()), 1);
        for steps in [
            json!([{"id":"looped","loop":{"max":0,"steps":[{"id":"inner","agent":{"prompt":"go"}}]}}]),
            json!([{"id":"batch","fan_out":{"repeat":21,"over":["x"],"prompt":"go"}}]),
            json!([{"id":"first","agent":{"prompt":"go"}}, {"id":"bad","agent":{"prompt":"go","schema":{"type":"invalid"}}}]),
            json!([{"id":"batch","fan_out":{"over":vec!["x";257],"prompt":"go"}}]),
        ] {
            let invalid: dsl::Spec = serde_json::from_value(json!({"steps":steps})).unwrap();
            assert!(dsl::run(ctx.clone(), invalid).await.error.is_some());
        }
        for input in [
            json!({"shape":"loop","max_rounds":0}),
            json!({"shape":"loop","max_rounds":21}),
            json!({"shape":"fan_out","items":vec!["x";257],"map_prompt":"go"}),
            json!({"shape":"map_reduce","items":["x"],"map_prompt":"go","reduce_schema":{"type":"invalid"}}),
        ] {
            let invalid: params::WorkflowParams = serde_json::from_value(input).unwrap();
            assert!(params::run(ctx.clone(), invalid).await.error.is_some());
        }
        assert_eq!(provider.call_count(), 0);
        assert!(ctx.team.roster().is_empty());
    }

    #[tokio::test]
    async fn workflow_launches_wait_for_shared_capacity_and_cancel_without_launching() {
        for stop_parent in [false, true] {
            let provider = MockProvider::new(vec![]);
            let ctx = context(Arc::new(provider.clone()), 1).with_parent("parent".into(), 2);
            for index in 0..MAX_TEAM_SIZE {
                let (_inbox, tx) = mailbox();
                ctx.team.register(
                    if index == 0 {
                        "parent".into()
                    } else {
                        format!("existing-{index}")
                    },
                    1,
                    "root".into(),
                    tx,
                );
            }
            let waiting = agent(&ctx, AgentSpec::new("must not run", "queued"));
            tokio::pin!(waiting);
            assert!(futures::poll!(waiting.as_mut()).is_pending());
            assert_eq!(provider.call_count(), 0);
            if stop_parent {
                ctx.team.stop("root", "parent").unwrap();
            } else {
                ctx.cancel.store(true, Ordering::Relaxed);
            }
            let result = timeout(DEADLINE, waiting).await.unwrap();
            assert_eq!(result.status, OutcomeStatus::Cancelled);
            assert_eq!(provider.call_count(), 0);
            assert_eq!(ctx.team.roster().len(), MAX_TEAM_SIZE);
            assert_eq!(ctx.limiter.available_permits(), 1);
        }
    }

    struct GatedTool {
        entered: mpsc::UnboundedSender<oneshot::Sender<()>>,
        calls: AtomicUsize,
        active: AtomicUsize,
        peak: AtomicUsize,
    }

    struct ActiveCall<'a>(&'a AtomicUsize);

    impl Drop for ActiveCall<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl Tool for GatedTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "gated_edit".into(),
                description: "Record an edit attempt and wait for release".into(),
                input_schema: json!({"type":"object"}),
            }
        }

        async fn execute(&self, _input: Value, ctx: &ToolContext) -> ToolResult {
            assert!(ctx.coord.as_ref().unwrap().team.active_len() <= MAX_TEAM_SIZE);
            self.calls.fetch_add(1, Ordering::Relaxed);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            let _active = ActiveCall(&self.active);
            self.peak.fetch_max(active, Ordering::SeqCst);
            let (release, released) = oneshot::channel();
            self.entered.send(release).unwrap();
            released.await.unwrap();
            Ok("released".into())
        }
    }

    fn gated_context(
        mut rules: Vec<MockRule>,
    ) -> (
        WorkflowContext,
        Arc<GatedTool>,
        mpsc::UnboundedReceiver<oneshot::Sender<()>>,
    ) {
        rules.push(MockRule {
            needle: "edit workspace".into(),
            reply: MockReply::ToolCall {
                name: "gated_edit".into(),
                input: json!({}),
            },
        });
        let mut ctx = context(Arc::new(MockProvider::new(rules)), 0);
        let (entered, rx) = mpsc::unbounded_channel();
        let tool = Arc::new(GatedTool {
            entered,
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        ctx.tools.add(tool.clone());
        (ctx, tool, rx)
    }

    #[tokio::test]
    async fn large_fan_outs_queue_behind_root_and_other_delegates() {
        for other_delegates in [0, 6] {
            let (ctx, tool, mut entered) = gated_context(vec![]);
            let (_root_inbox, tx) = mailbox();
            ctx.team.register("root".into(), 0, String::new(), tx);
            for index in 0..other_delegates {
                let (_, tx) = mailbox();
                ctx.team
                    .register(format!("delegate-{index}"), 1, "root".into(), tx);
            }
            let workflow = spec(json!([{"id":"batch","fan_out":{
                "over": (0..32).collect::<Vec<_>>(), "repeat":2, "prompt":"edit workspace {item}"
            }}]));
            let available = MAX_TEAM_SIZE - 1 - other_delegates;
            let (result, ()) = timeout(DEADLINE, async {
                tokio::join!(dsl::run(ctx.clone(), workflow), async {
                    let mut gates = Vec::new();
                    for _ in 0..available {
                        gates.push(entered.recv().await.unwrap());
                    }
                    assert_eq!(tool.calls.load(Ordering::Relaxed), available);
                    assert_eq!(ctx.team.active_len(), MAX_TEAM_SIZE);
                    assert!(ctx.outcomes().is_empty());
                    for gate in gates {
                        gate.send(()).unwrap();
                    }
                    for _ in available..64 {
                        entered.recv().await.unwrap().send(()).unwrap();
                    }
                })
            })
            .await
            .expect("large fan-out did not drain the shared capacity queue");
            assert!(result.error.is_none(), "{:?}", result.error);
            assert_eq!(result.output["batch"].as_array().unwrap().len(), 32);
            for repeated in result.output["batch"].as_array().unwrap() {
                assert_eq!(repeated, &json!([{"text":"ok"}, {"text":"ok"}]));
            }
            assert_eq!(tool.calls.load(Ordering::Relaxed), 64);
            assert_eq!(ctx.outcomes().len(), 64);
            assert!(ctx.outcomes().iter().all(AgentOutcome::is_success));
            assert_eq!(ctx.team.active_len(), 1 + other_delegates);
            assert_eq!(ctx.limiter.available_permits(), MAX_TEAM_SIZE);
        }
    }

    #[tokio::test]
    async fn cancellation_stops_active_and_capacity_queued_fan_out_leaves() {
        let (ctx, tool, mut entered) = gated_context(vec![]);
        let (_root_inbox, tx) = mailbox();
        ctx.team.register("root".into(), 0, String::new(), tx);
        let workflow = spec(json!([{"id":"batch","fan_out":{
            "over": (0..64).collect::<Vec<_>>(), "prompt":"edit workspace {item}"
        }}]));
        let (result, gates) = timeout(DEADLINE, async {
            tokio::join!(dsl::run(ctx.clone(), workflow), async {
                let mut gates = Vec::new();
                for _ in 0..MAX_TEAM_SIZE - 1 {
                    gates.push(entered.recv().await.unwrap());
                }
                ctx.cancel.store(true, Ordering::Relaxed);
                gates
            })
        })
        .await
        .expect("capacity-queued fan-out did not cancel");
        assert_error(&result, "cancelled");
        assert_eq!(tool.calls.load(Ordering::Relaxed), MAX_TEAM_SIZE - 1);
        assert_eq!(ctx.outcomes().len(), 64);
        assert!(ctx
            .outcomes()
            .iter()
            .all(|outcome| outcome.status == OutcomeStatus::Cancelled));
        assert_eq!(ctx.team.roster().len(), MAX_TEAM_SIZE);
        assert_eq!(ctx.team.active_len(), 1);
        assert_eq!(ctx.limiter.available_permits(), MAX_TEAM_SIZE);
        drop(gates);
    }

    #[tokio::test]
    async fn literal_item_paths_fail_validation_before_any_side_effects() {
        let (ctx, tool, _entered) = gated_context(vec![]);
        let input = json!({"steps":[
            {"id":"earlier","agent":{"prompt":"edit workspace"}},
            {"id":"batch","parallel":{"branches":{
                "a_edit":{"agent":{"prompt":"edit workspace"}},
                "z_invalid":{"fan_out":{"over":[{}],"prompt":"inspect {item.path}"}}
            }}}
        ]});
        assert!(input::validate(&input)
            .unwrap_err()
            .contains("missing item field"));
        let workflow: dsl::Spec = serde_json::from_value(input).unwrap();
        assert!(validation::validate(&workflow)
            .unwrap_err()
            .contains("missing item field"));
        let result = run_checked(&ctx, workflow).await;
        assert_error(&result, "missing item field");
        assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
        assert!(ctx.outcomes().is_empty());
        assert!(ctx.team.roster().is_empty());
    }

    #[tokio::test]
    async fn parallel_preflights_all_resolvable_inputs_before_launching_any_branch() {
        for invalid in [
            json!({"fan_out":{"over":"$source.items","prompt":"inspect {item.path}"}}),
            json!({"fan_out":{"over":"$source.scalar","prompt":"inspect {item}"}}),
            json!({"fan_out":{"over":"$source.oversized","prompt":"inspect {item}"}}),
            json!({"agent":{"prompt":"inspect {$source.missing}"}}),
            json!({"loop":{"steps":[
                {"id":"local","agent":{"prompt":"edit workspace"}},
                {"id":"later","fan_out":{"over":"$local.items","prompt":"{$local} {$source.missing} {item}"}}
            ]}}),
            json!({"parallel":{"branches":{"nested":{"fan_out":{
                "over":"$source.items","prompt":"inspect {item.path}"
            }}}}}),
        ] {
            let (ctx, tool, _entered) = gated_context(vec![structured(
                "produce source",
                json!({"items":[{}],"scalar":3,"oversized":vec!["x";257]}),
            )]);
            let workflow = spec(json!([
                {"id":"source","agent":{"prompt":"produce source","schema":{"type":"object"}}},
                {"id":"batch","parallel":{"branches":{
                    "a_edit":{"agent":{"prompt":"edit workspace"}}, "z_invalid":invalid
                }}}
            ]));
            let result = run_checked(&ctx, workflow).await;
            assert_error(&result, "branch 'z_invalid'");
            assert_eq!(tool.calls.load(Ordering::Relaxed), 0);
            assert_eq!(ctx.outcomes().len(), 1);
            assert_eq!(ctx.outcomes()[0].label, "source");
            assert_eq!(ctx.team.roster().len(), 1);
            assert_eq!(result.output["batch"], Value::Null);
        }
    }

    #[tokio::test]
    async fn parallel_preflight_defers_local_outputs_and_preserves_repeated_phases() {
        let provider = MockProvider::new(vec![
            structured("produce source", json!({"metadata":"shared"})),
            structured("produce local", json!({"items":[{"path":"a"}]})),
            text("consume a"),
            text("independent work"),
        ]);
        let ctx = context(Arc::new(provider), 2);
        let workflow = spec(json!([
            {"id":"source","agent":{"prompt":"produce source","schema":{"type":"object"}}},
            {"id":"group","parallel":{"branches":{
                "rounds":{"loop":{"max":2,"steps":[
                    {"id":"local","agent":{"prompt":"produce local","schema":{"type":"object"}}},
                    {"id":"batch","fan_out":{
                        "over":"$local.items","repeat":2,
                        "prompt":"consume {item.path} {$local.items} {$source.metadata}"
                    }}
                ]}},
                "sibling":{"agent":{"prompt":"independent work"}}
            }}}
        ]));
        let result = run_checked(&ctx, workflow).await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(result.output["group"]["rounds"]["_iterations"], 2);
        assert_eq!(
            result.output["group"]["rounds"]["batch"],
            json!([[{"text":"ok"},{"text":"ok"}]])
        );
        assert_eq!(ctx.outcomes().len(), 8);
        assert!(ctx.outcomes().iter().all(AgentOutcome::is_success));
    }

    #[derive(Default)]
    struct StalledProvider {
        calls: AtomicUsize,
        started: Notify,
        streams: Mutex<Vec<mpsc::UnboundedSender<StreamEvent>>>,
    }

    #[async_trait]
    impl Provider for StalledProvider {
        fn name(&self) -> &str {
            "stalled-test"
        }
        fn model(&self) -> &str {
            "mock-model"
        }

        async fn generate(&self, _opts: GenerateOptions) -> anyhow::Result<Completion> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.started.notify_one();
            std::future::pending().await
        }

        async fn stream(
            &self,
            _opts: GenerateOptions,
        ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = mpsc::unbounded_channel();
            self.streams.lock().unwrap().push(tx);
            self.started.notify_one();
            Ok(rx)
        }
    }

    #[tokio::test]
    async fn cancellation_before_loop_starts_no_agents() {
        let provider = MockProvider::new(vec![]);
        let ctx = context(Arc::new(provider.clone()), 1);
        ctx.cancel.store(true, Ordering::Relaxed);
        let result = run_checked(&ctx, check_loop()).await;

        assert_error(&result, "cancelled before step 'rounds'");
        assert_eq!(result.output, json!({}));
        assert_eq!(provider.call_count(), 0);
        assert!(ctx.outcomes().is_empty());
        assert!(ctx.team.roster().is_empty());
    }

    #[tokio::test]
    async fn cancellation_during_loop_interrupts_a_stalled_provider() {
        let provider = Arc::new(StalledProvider::default());
        let ctx = context(provider.clone(), 1);
        let (result, ()) = timeout(DEADLINE, async {
            tokio::join!(dsl::run(ctx.clone(), check_loop()), async {
                provider.started.notified().await;
                ctx.cancel.store(true, Ordering::Relaxed);
            })
        })
        .await
        .expect("loop failed to stop after cancellation");

        assert_error(&result, "cancelled");
        assert_eq!(
            result.output,
            json!({"rounds": {
                "check": null, "_iterations": 1, "_stop_reason": "cancelled"
            }})
        );
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
        let outcomes = ctx.outcomes();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].label, "check");
        assert_eq!(outcomes[0].status, OutcomeStatus::Cancelled);
        assert!(outcomes[0].output.is_none());
        assert_eq!(ctx.cancelled_agents(), vec![outcomes[0].id.clone()]);
        assert_eq!(
            ctx.team.roster(),
            vec![(outcomes[0].id.clone(), 1, AgentStatus::Cancelled)]
        );
        assert_eq!(ctx.limiter.available_permits(), 1);
    }

    #[tokio::test]
    async fn queued_agents_cancel_without_provider_calls_after_parent_cancel() {
        let provider = MockProvider::new(vec![]);
        let ctx = context(Arc::new(provider.clone()), 1);
        let permit = ctx.limiter.acquire().await.unwrap();
        let batch = parallel(
            (0..3)
                .map(|index| {
                    agent(
                        &ctx,
                        AgentSpec::new("queued work", format!("queued-{index}"))
                            .with_input(json!(index)),
                    )
                })
                .collect(),
        );
        tokio::pin!(batch);
        assert!(futures::poll!(batch.as_mut()).is_pending());
        assert_eq!(provider.call_count(), 0);
        assert!(ctx.outcomes().is_empty());
        assert!(ctx.team.roster().is_empty());

        ctx.cancel.store(true, Ordering::Relaxed);
        let outcomes = timeout(DEADLINE, batch)
            .await
            .expect("queued agents did not finish");

        assert_eq!(provider.call_count(), 0);
        assert!(ctx.team.roster().is_empty());
        assert_eq!(outcomes.len(), 3);
        for (index, outcome) in outcomes.iter().enumerate() {
            assert_eq!(outcome.label, format!("queued-{index}"));
            assert_eq!(outcome.input, Some(json!(index)));
            assert_eq!(outcome.status, OutcomeStatus::Cancelled);
            assert!(outcome.output.is_none());
            assert_eq!(outcome.error.as_deref(), Some("cancelled before starting"));
        }
        assert_eq!(
            serde_json::to_value(ctx.outcomes()).unwrap(),
            serde_json::to_value(&outcomes).unwrap()
        );
        assert_eq!(
            ctx.cancelled_agents(),
            outcomes
                .iter()
                .map(|outcome| outcome.id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(ctx.limiter.available_permits(), 0);
        drop(permit);
        assert_eq!(ctx.limiter.available_permits(), 1);
    }

    #[derive(Default)]
    struct PanickingProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Provider for PanickingProvider {
        fn name(&self) -> &str {
            "panicking-test"
        }
        fn model(&self) -> &str {
            "mock-model"
        }

        async fn generate(&self, _opts: GenerateOptions) -> anyhow::Result<Completion> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            panic!("runtime provider panic");
        }

        async fn stream(
            &self,
            _opts: GenerateOptions,
        ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            panic!("runtime provider panic");
        }
    }

    #[tokio::test]
    async fn provider_panic_becomes_failed_outcome_and_finishes_lifecycle_once() {
        for structured in [false, true] {
            let provider = Arc::new(PanickingProvider::default());
            let ctx = context(provider.clone(), 1);
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            ctx.bus.on(Arc::new(move |event| {
                if matches!(
                    event,
                    AgentEvent::SubagentSpawn { .. } | AgentEvent::SubagentDone { .. }
                ) {
                    captured.lock().unwrap().push(event.clone());
                }
            }));
            let mut spec = AgentSpec::new("panic in provider", "panic");
            if structured {
                spec = spec.with_schema(json!({"type": "object"}));
            }
            let outcome = timeout(DEADLINE, agent(&ctx, spec))
                .await
                .expect("panicking agent did not complete");

            assert_eq!(outcome.status, OutcomeStatus::Failed);
            assert!(outcome.output.is_none());
            assert_eq!(
                outcome.error.as_deref(),
                Some("agent panicked: runtime provider panic")
            );
            assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
            assert_eq!(
                serde_json::to_value(ctx.outcomes()).unwrap(),
                json!([outcome.clone()])
            );
            assert_eq!(
                ctx.team.roster(),
                vec![(outcome.id.clone(), 1, AgentStatus::Failed)]
            );
            assert!(!ctx.is_cancelled());
            assert!(ctx.cancelled_agents().is_empty());
            assert_eq!(ctx.limiter.available_permits(), 1);
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 2);
            assert!(
                matches!(&events[0], AgentEvent::SubagentSpawn { agent_id, .. } if agent_id == &outcome.id)
            );
            assert!(matches!(&events[1], AgentEvent::SubagentDone {
            agent_id, failed: true, cancelled: false
        } if agent_id == &outcome.id));
        }
    }

    #[tokio::test]
    async fn parallel_preserves_failed_outcomes_and_input_order() {
        let provider = Arc::new(MockProvider::new(vec![
            text("first"),
            text("last"),
            text("no structured result"),
            text("You did not record a result."),
        ]));
        let ctx = context(provider, 2);
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
        let order = Arc::new(Mutex::new(Vec::<u64>::new()));
        let release_slow = Arc::new(Notify::new());
        let gate = release_slow.clone();
        let stage1: Stage<u64> = Box::new(move |_prev, item, _i| {
            let gate = gate.clone();
            Box::pin(async move {
                if item == 1 {
                    gate.notified().await;
                }
                Some(json!(item))
            })
        });
        let completed = order.clone();
        let stage2: Stage<u64> = Box::new(move |prev, item, _i| {
            let completed = completed.clone();
            let release_slow = release_slow.clone();
            Box::pin(async move {
                assert_eq!(prev, Some(json!(item)));
                completed.lock().unwrap().push(item);
                if item == 2 {
                    release_slow.notify_one();
                }
                prev
            })
        });

        let out = timeout(DEADLINE, pipeline(vec![1u64, 2u64], vec![stage1, stage2]))
            .await
            .expect("fast item must reach stage two while slow item waits in stage one");
        assert_eq!(out, vec![Some(json!(1)), Some(json!(2))]);
        assert_eq!(*order.lock().unwrap(), [2, 1]);
    }

    #[tokio::test]
    async fn pipeline_threads_values_but_keeps_original_items_and_skips_after_a_middle_drop() {
        let first: Stage<&'static str> = Box::new(|previous, item, index| {
            Box::pin(async move {
                assert!(previous.is_none());
                assert_eq!(index, 0);
                Some(json!({"source": item, "score": 7}))
            })
        });
        let filter: Stage<&'static str> = Box::new(|previous, item, index| {
            Box::pin(async move {
                assert_eq!(index, 1);
                assert_eq!(previous, Some(json!({"source": item, "score": 7})));
                if item == "skip" {
                    None
                } else {
                    Some(json!({"score": 11}))
                }
            })
        });
        let last: Stage<&'static str> = Box::new(|previous, item, index| {
            Box::pin(async move {
                assert_eq!(item, "keep", "filtered items must not reach the last stage");
                assert_eq!(index, 2);
                assert_eq!(previous, Some(json!({"score": 11})));
                Some(json!({"original": item, "final": 13}))
            })
        });
        let result = pipeline(vec!["skip", "keep"], vec![first, filter, last]).await;
        assert_eq!(
            result,
            vec![None, Some(json!({"original": "keep", "final": 13}))]
        );
    }

    #[tokio::test]
    async fn pipeline_empty_inputs_do_not_invoke_stages_and_empty_stages_preserve_slots() {
        let unexpected: Stage<u8> =
            Box::new(|_, _, _| panic!("empty input must not invoke a stage"));
        assert!(pipeline(Vec::new(), vec![unexpected]).await.is_empty());
        assert_eq!(pipeline(vec![1u8, 2], Vec::new()).await, vec![None, None]);
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
        let downstream = Arc::new(Mutex::new(Vec::new()));
        let calls = downstream.clone();
        let stage2: Stage<u64> = Box::new(move |prev, item, index| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.lock().unwrap().push((item, index));
                prev
            })
        });
        let calls = downstream.clone();
        let stage3: Stage<u64> = Box::new(move |prev, item, index| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.lock().unwrap().push((item, index));
                prev
            })
        });
        let out = pipeline(vec![0u64, 7u64], vec![stage1, stage2, stage3]).await;
        assert_eq!(out, vec![None, Some(json!(7))]);
        assert_eq!(*downstream.lock().unwrap(), [(7, 1), (7, 2)]);
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
        let ctx = context(Arc::new(provider), 4);
        let schema = json!({"type": "object", "required": ["point"]});
        let out = agent(&ctx, AgentSpec::new("say a point", "p").with_schema(schema)).await;
        assert!(out.is_success());
        assert_eq!(out.output, Some(json!({"point": "fast"})));
    }

    #[tokio::test]
    async fn agent_reports_cancellation_before_starting() {
        let provider = MockProvider::new(vec![]);
        let ctx = context(Arc::new(provider.clone()), 4);
        ctx.cancel.store(true, Ordering::Relaxed);
        let out = agent(&ctx, AgentSpec::new("anything", "p")).await;
        assert_eq!(out.status, OutcomeStatus::Cancelled);
        assert_eq!(provider.call_count(), 0);
        assert!(ctx.team.roster().is_empty());
        assert_eq!(ctx.outcomes().len(), 1);
    }

    #[tokio::test]
    async fn semaphore_caps_concurrency() {
        let (mut ctx, tool, mut entered) = gated_context(vec![]);
        ctx.limiter = Arc::new(Semaphore::new(2));
        let thunks = (0..4)
            .map(|i| agent(&ctx, AgentSpec::new("edit workspace", format!("a{i}"))))
            .collect();
        let batch = parallel(thunks);
        tokio::pin!(batch);
        let mut gates = Vec::new();
        for _ in 0..2 {
            gates.push(
                timeout(DEADLINE, async {
                    tokio::select! {
                        _ = &mut batch => panic!("batch finished before release"),
                        gate = entered.recv() => gate.unwrap(),
                    }
                })
                .await
                .unwrap(),
            );
        }
        assert!(futures::poll!(batch.as_mut()).is_pending());
        assert_eq!(tool.calls.load(Ordering::Relaxed), 2);
        assert_eq!(tool.active.load(Ordering::SeqCst), 2);
        assert_eq!(ctx.limiter.available_permits(), 0);
        assert_eq!(ctx.team.active_len(), 2);
        assert!(ctx.outcomes().is_empty());
        assert!(entered.try_recv().is_err());

        for expected_calls in [3, 4] {
            gates.remove(0).send(()).unwrap();
            let gate = timeout(DEADLINE, async {
                tokio::select! {
                    _ = &mut batch => panic!("batch finished with unreleased agents"),
                    gate = entered.recv() => gate.unwrap(),
                }
            })
            .await
            .unwrap();
            gates.push(gate);
            assert!(futures::poll!(batch.as_mut()).is_pending());
            assert_eq!(tool.calls.load(Ordering::Relaxed), expected_calls);
            assert_eq!(tool.active.load(Ordering::SeqCst), 2);
            assert!(entered.try_recv().is_err());
        }
        for gate in gates {
            gate.send(()).unwrap();
        }
        let out = timeout(DEADLINE, batch).await.unwrap();
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(AgentOutcome::is_success));
        assert_eq!(
            out.iter()
                .map(|outcome| outcome.label.as_str())
                .collect::<Vec<_>>(),
            ["a0", "a1", "a2", "a3"]
        );
        assert!(out
            .iter()
            .all(|outcome| outcome.output == Some(json!({"text": "ok"}))));
        assert_eq!(tool.peak.load(Ordering::SeqCst), 2);
        assert_eq!(tool.active.load(Ordering::SeqCst), 0);
        assert_eq!(ctx.limiter.available_permits(), 2);
    }
}
