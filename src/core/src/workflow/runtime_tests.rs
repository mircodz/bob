use super::*;
use crate::core::types::{Completion, GenerateOptions, StreamEvent};
use crate::providers::mock::{MockProvider, MockReply, MockRule};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(5);

fn context(provider: Arc<dyn Provider>, limit: usize) -> WorkflowContext {
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

fn structured(needle: &str, output: Value) -> MockRule {
    MockRule {
        needle: needle.into(),
        reply: MockReply::ToolCall {
            name: "structured_output".into(),
            input: output,
        },
    }
}

fn spec(steps: Value) -> dsl::Spec {
    let input = json!({"steps": steps});
    input::validate(&input).expect("runtime fixtures must be structurally valid");
    let spec = serde_json::from_value(input).unwrap();
    validation::validate(&spec).expect("runtime fixtures must pass reference validation");
    spec
}

async fn run(ctx: &WorkflowContext, spec: dsl::Spec) -> WorkflowRun {
    timeout(DEADLINE, dsl::run(ctx.clone(), spec))
        .await
        .expect("workflow did not finish within the deadline")
}

fn assert_error(run: &WorkflowRun, expected: &str) {
    let error = run.error.as_deref().expect("workflow should fail");
    assert!(
        error.contains(expected),
        "expected {expected:?}, got {error:?}"
    );
}

fn check_loop() -> dsl::Spec {
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
async fn missing_runtime_output_field_stops_before_dependent_agent() {
    let provider = MockProvider::new(vec![structured("produce source", json!({"present": 7}))]);
    let ctx = context(Arc::new(provider.clone()), 2);
    let result = run(
        &ctx,
        spec(json!([
            {"id": "source", "agent": {
                "prompt": "produce source", "schema": {"type": "object"}
            }},
            {"id": "dependent", "agent": {"prompt": "consume {$source.missing}"}},
            {"id": "after", "agent": {"prompt": "must not run"}}
        ])),
    )
    .await;

    assert_error(
        &result,
        "missing field 'missing' in reference '$source.missing'",
    );
    assert_eq!(
        result.output,
        json!({"source": {"present": 7}, "dependent": null})
    );
    assert_eq!(provider.call_count(), 2);
    let outcomes = ctx.outcomes();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].label, "source");
    assert_eq!(outcomes[0].status, OutcomeStatus::Success);
    assert_eq!(outcomes[0].output, Some(json!({"present": 7})));
}

#[tokio::test]
async fn fan_out_rejects_references_to_null_scalars_and_objects() {
    for items in [
        json!(null),
        json!(false),
        json!(42),
        json!("items"),
        json!({"item": 1}),
    ] {
        let provider =
            MockProvider::new(vec![structured("produce source", json!({"items": items}))]);
        let ctx = context(Arc::new(provider.clone()), 2);
        let result = run(
            &ctx,
            spec(json!([
                {"id": "source", "agent": {
                    "prompt": "produce source", "schema": {"type": "object"}
                }},
                {"id": "batch", "fan_out": {"over": "$source.items", "prompt": "consume {item}"}},
                {"id": "after", "agent": {"prompt": "must not run"}}
            ])),
        )
        .await;

        assert_error(&result, "fan-out 'over' must resolve to an array");
        assert_eq!(
            result.output,
            json!({"source": {"items": items}, "batch": null})
        );
        assert_eq!(provider.call_count(), 2);
        let outcomes = ctx.outcomes();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].label, "source");
        assert_eq!(outcomes[0].status, OutcomeStatus::Success);
    }
}

#[tokio::test]
async fn empty_fan_out_arrays_are_successful_and_allow_following_steps() {
    for repeat in [1, 2] {
        let provider = MockProvider::new(vec![structured("produce source", json!({"items": []}))]);
        let ctx = context(Arc::new(provider.clone()), 2);
        let result = run(
            &ctx,
            spec(json!([
                {"id": "source", "agent": {
                    "prompt": "produce source", "schema": {"type": "object"}
                }},
                {"id": "referenced", "fan_out": {
                    "over": "$source.items", "prompt": "consume {item}", "repeat": repeat
                }},
                {"id": "inline", "fan_out": {
                    "over": [], "prompt": "consume {item}", "repeat": repeat
                }},
                {"id": "after", "agent": {"prompt": "after empty batches"}}
            ])),
        )
        .await;

        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(
            result.output,
            json!({
                "source": {"items": []}, "referenced": [], "inline": [], "after": {"text": "ok"}
            })
        );
        assert_eq!(provider.call_count(), 3);
        let outcomes = ctx.outcomes();
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.label.as_str())
                .collect::<Vec<_>>(),
            ["source", "after"]
        );
        assert!(outcomes.iter().all(AgentOutcome::is_success));
    }
}

#[tokio::test]
async fn fan_out_validates_every_item_before_starting_any_batch_agent() {
    for referenced in [false, true] {
        let items = json!([{"details": {"name": "valid"}}, {"details": {}}]);
        let provider =
            MockProvider::new(vec![structured("produce source", json!({"items": items}))]);
        let ctx = context(Arc::new(provider.clone()), 2);
        let mut steps = Vec::new();
        if referenced {
            steps.push(json!({"id": "source", "agent": {
                "prompt": "produce source", "schema": {"type": "object"}
            }}));
        }
        steps.push(json!({"id": "batch", "fan_out": {
            "over": if referenced { json!("$source.items") } else { items },
            "prompt": "consume {item.details.name}",
            "repeat": 2
        }}));
        steps.push(json!({"id": "after", "agent": {"prompt": "must not run"}}));
        let result = run(&ctx, spec(Value::Array(steps))).await;

        assert_error(
            &result,
            "fan-out item 2: missing item field in '{item.details.name}'",
        );
        assert_eq!(result.output.get("batch"), Some(&Value::Null));
        assert!(result.output.get("after").is_none());
        assert_eq!(provider.call_count(), if referenced { 2 } else { 0 });
        let outcomes = ctx.outcomes();
        assert_eq!(outcomes.len(), usize::from(referenced));
        assert!(outcomes
            .iter()
            .all(|outcome| outcome.label == "source" && outcome.is_success()));
        assert_eq!(ctx.team.roster().len(), usize::from(referenced));
    }
}

#[tokio::test]
async fn boolean_loop_conditions_stop_on_true_or_reach_max_rounds_on_false() {
    for (done, iterations, reason) in [(true, 1, "condition_met"), (false, 3, "max_rounds")] {
        let provider =
            MockProvider::new(vec![structured("check condition", json!({"done": done}))]);
        let ctx = context(Arc::new(provider.clone()), 1);
        let result = run(&ctx, check_loop()).await;

        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(
            result.output,
            json!({
                "rounds": {"check": {"done": done}, "_iterations": iterations, "_stop_reason": reason},
                "after": {"text": "ok"}
            })
        );
        assert_eq!(provider.call_count(), iterations * 2 + 1);
        let outcomes = ctx.outcomes();
        assert_eq!(outcomes.len(), iterations + 1);
        assert!(outcomes.iter().all(AgentOutcome::is_success));
        assert!(outcomes[..iterations]
            .iter()
            .all(|outcome| outcome.label == "check"));
        assert_eq!(outcomes[iterations].label, "after");
        let ids: std::collections::BTreeSet<_> =
            outcomes.iter().map(|outcome| &outcome.id).collect();
        assert_eq!(ids.len(), outcomes.len());
    }
}

#[tokio::test]
async fn missing_or_non_boolean_loop_conditions_fail_after_one_check() {
    for output in [
        json!({}),
        json!({"done": null}),
        json!({"done": "true"}),
        json!({"done": "false"}),
        json!({"done": ""}),
        json!({"done": 0}),
        json!({"done": 1}),
        json!({"done": []}),
        json!({"done": {}}),
    ] {
        let provider = MockProvider::new(vec![structured("check condition", output.clone())]);
        let ctx = context(Arc::new(provider.clone()), 1);
        let result = run(&ctx, check_loop()).await;

        assert_error(
            &result,
            if output.get("done").is_none() {
                "missing field 'done' in reference '$check.done'"
            } else {
                "loop condition '$check.done' must be a boolean"
            },
        );
        assert_eq!(
            result.output,
            json!({"rounds": {
                "check": output, "_iterations": 1, "_stop_reason": "failed"
            }})
        );
        assert_eq!(provider.call_count(), 2);
        let outcomes = ctx.outcomes();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].label, "check");
        assert_eq!(outcomes[0].status, OutcomeStatus::Success);
        assert_eq!(outcomes[0].output, Some(output));
    }
}

#[tokio::test]
async fn failed_check_stops_loop_and_preserves_prior_step_output() {
    let provider = MockProvider::new(vec![]);
    let ctx = context(Arc::new(provider.clone()), 1);
    let result = run(
        &ctx,
        spec(json!([
            {"id": "rounds", "loop": {
                "max": 3, "until": "$check.done",
                "steps": [
                    {"id": "draft", "agent": {"prompt": "prepare draft"}},
                    {"id": "check", "agent": {
                        "prompt": "check without a structured reply", "schema": {"type": "object"}
                    }},
                    {"id": "later", "agent": {"prompt": "must not run"}}
                ]
            }},
            {"id": "after", "agent": {"prompt": "must not run"}}
        ])),
    )
    .await;

    assert_error(&result, "did not produce a structured result");
    assert_eq!(
        result.output,
        json!({"rounds": {
            "draft": {"text": "ok"}, "check": null, "_iterations": 1, "_stop_reason": "failed"
        }})
    );
    assert_eq!(provider.call_count(), 3);
    let outcomes = ctx.outcomes();
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].label, "draft");
    assert_eq!(outcomes[0].status, OutcomeStatus::Success);
    assert_eq!(outcomes[1].label, "check");
    assert_eq!(outcomes[1].status, OutcomeStatus::Failed);
    assert!(outcomes[1].output.is_none());
    assert!(outcomes[1]
        .error
        .as_deref()
        .unwrap()
        .contains("structured result"));
}

#[tokio::test]
async fn parallel_preserves_independent_successes_when_a_sibling_fails() {
    let provider = MockProvider::new(vec![
        MockRule {
            needle: "left success".into(),
            reply: MockReply::Text("left output".into()),
        },
        MockRule {
            needle: "right success".into(),
            reply: MockReply::Text("right output".into()),
        },
    ]);
    let ctx = context(Arc::new(provider.clone()), 1);
    let result = run(&ctx, spec(json!([
        {"id": "group", "parallel": {"branches": {
            "a_failed": {"agent": {"prompt": "no structured reply", "schema": {"type": "object"}}},
            "b_left": {"agent": {"prompt": "left success"}},
            "c_right": {"agent": {"prompt": "right success"}}
        }}},
        {"id": "after", "agent": {"prompt": "must not run"}}
    ]))).await;

    assert_error(&result, "branch 'a_failed'");
    assert_error(&result, "structured result");
    assert_eq!(
        result.output,
        json!({"group": {
            "a_failed": null, "b_left": {"text": "left output"}, "c_right": {"text": "right output"}
        }})
    );
    assert_eq!(provider.call_count(), 4);
    let outcomes = ctx.outcomes();
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.label.as_str())
            .collect::<Vec<_>>(),
        ["group.a_failed", "group.b_left", "group.c_right"]
    );
    assert_eq!(
        outcomes
            .iter()
            .map(|outcome| outcome.status)
            .collect::<Vec<_>>(),
        [
            OutcomeStatus::Failed,
            OutcomeStatus::Success,
            OutcomeStatus::Success
        ]
    );
    assert_eq!(outcomes[1].output, Some(json!({"text": "left output"})));
    assert_eq!(outcomes[2].output, Some(json!({"text": "right output"})));
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
    let result = run(&ctx, check_loop()).await;

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
    drop(permit);
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
