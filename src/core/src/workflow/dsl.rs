//! A declarative workflow DSL: the model (or a saved file) describes a multi-step
//! pipeline as data — steps that run agents, fan out over prior results, or run
//! sub-steps in parallel — and the interpreter executes it deterministically over
//! the existing engine primitives (`agent`, `parallel`). Cross-step data flow is
//! by reference: a later step reads an earlier step's output via `$id` /
//! `$id.field` in prompts (or `over`), and `loop_until` re-runs the whole pipeline
//! until a referenced boolean is true.
//!
//! This is the safe middle ground between single-shape workflows (`params.rs`) and
//! full scripting: it can express sequencing, fan-out over runtime lists, an
//! adversarial `parallel` step, and a bounded outer loop — without a sandbox or an
//! embedded scripting engine. What it deliberately can NOT do is arbitrary
//! computation between steps (sorting/formulas); that's the scripting escape hatch.

use super::validation::{placeholders, reference_parts};
use super::{agent, parallel, AgentSpec, WorkflowContext, WorkflowRun};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// A whole workflow: an ordered list of named steps. Looping is a step op
/// (`Op::Loop`), so a loop can wrap just PART of the pipeline and can nest — not
/// only the whole thing.
#[derive(Debug, Clone, Deserialize)]
pub struct Spec {
    pub steps: Vec<Step>,
}

/// One named step. Its `op`'s result is stored in the scope under `id` so later
/// steps can reference it.
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub id: String,
    #[serde(flatten)]
    pub op: Op,
}

/// The operations a step can perform. Exactly one variant is present per step
/// (serde picks by which key is set: `agent`, `fan_out`, `parallel`, `loop`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// A single agent. Its `prompt` may contain `$refs` to prior steps.
    Agent(AgentOp),
    /// One agent per item in `over` (a `$ref` to a list, or an inline list). The
    /// per-item `prompt` uses `{item}`; `repeat` runs N agents per item (e.g. 2
    /// adversarial reviews each).
    FanOut(FanOutOp),
    /// Run several named branches concurrently; the step's result is an object of
    /// each branch's result keyed by branch name.
    Parallel(ParallelOp),
    /// Re-run an inner sequence until a boolean condition is true or the cap is
    /// reached. Inner results are exposed through the loop step's output.
    Loop(LoopOp),
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentOp {
    pub prompt: String,
    #[serde(default)]
    pub schema: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FanOutOp {
    /// A `$ref` to a list, or an inline array of strings/objects.
    pub over: Value,
    pub prompt: String,
    #[serde(default)]
    pub schema: Option<Value>,
    /// Run this many agents per item (default 1). >1 gives an array per item.
    #[serde(default)]
    pub repeat: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ParallelOp {
    /// Named branches run concurrently; each is itself a single-step op.
    pub branches: BTreeMap<String, Op>,
}

/// Re-run `steps` until `until` resolves to true, or the iteration cap is reached.
/// Each iteration starts from the enclosing scope, without stale prior results.
#[derive(Debug, Clone, Deserialize)]
pub struct LoopOp {
    pub steps: Vec<Step>,
    /// A reference to a boolean. Omit to always run `max` iterations.
    #[serde(default)]
    pub until: Option<String>,
    /// Iteration cap (default 5, accepted range 1..=20).
    #[serde(default)]
    pub max: Option<usize>,
}

/// Run a spec to completion: execute its steps in order, threading each step's
/// output into a shared scope. Returns the final scope (every step's output keyed
/// by id).
pub async fn run(ctx: WorkflowContext, spec: Spec) -> WorkflowRun {
    let mut scope = Map::new();
    if let Err(error) = super::validation::validate(&spec) {
        return WorkflowRun::failed(Value::Object(scope), error);
    }
    match run_steps(&ctx, &spec.steps, &mut scope).await {
        Ok(()) => WorkflowRun::success(Value::Object(scope)),
        Err(error) => WorkflowRun::failed(Value::Object(scope), error),
    }
}

/// Run an ordered list of steps, writing each result into `scope` under its id.
/// Each step announces a phase (so the inline tree groups agents by step name),
/// EXCEPT loop steps, which emit their own per-pass phases.
async fn run_steps(
    ctx: &WorkflowContext,
    steps: &[Step],
    scope: &mut Map<String, Value>,
) -> Result<(), String> {
    for (i, step) in steps.iter().enumerate() {
        if ctx.is_cancelled() {
            return Err(format!("cancelled before step '{}'", step.id));
        }
        if !matches!(step.op, Op::Loop(_)) {
            ctx.phase(&step.id, i, steps.len());
        }
        let run = run_step(ctx, step, scope).await;
        scope.insert(step.id.clone(), run.output);
        if let Some(error) = run.error {
            return Err(format!("step '{}': {error}", step.id));
        }
    }
    Ok(())
}

/// Execute one step, returning its output value. Boxed because steps recurse
/// (a `Loop`/`Parallel` step runs sub-steps, which are steps).
fn run_step<'a>(
    ctx: &'a WorkflowContext,
    step: &'a Step,
    scope: &'a Map<String, Value>,
) -> futures::future::BoxFuture<'a, WorkflowRun> {
    Box::pin(async move {
        match &step.op {
            Op::Agent(a) => run_agent_op(ctx, &step.id, a, scope).await,
            Op::FanOut(f) => run_fan_out(ctx, &step.id, f, scope).await,
            Op::Parallel(p) => run_parallel(ctx, &step.id, p, scope).await,
            Op::Loop(l) => run_loop(ctx, &step.id, l, scope).await,
        }
    })
}

/// A loop step: re-run its inner steps (into a working copy of the scope) until
/// `until` is truthy or the cap is reached. Returns the loop's local scope plus an
/// `_iterations` count. The inner steps' outputs are visible via `$id.step_id`.
async fn run_loop(
    ctx: &WorkflowContext,
    id: &str,
    op: &LoopOp,
    outer: &Map<String, Value>,
) -> WorkflowRun {
    let max = op.max.unwrap_or(5);
    let mut local = Map::new();
    let mut iterations = 0;
    loop {
        if ctx.is_cancelled() {
            return loop_result(
                local,
                iterations,
                "cancelled",
                Some("workflow cancelled".into()),
            );
        }
        iterations += 1;
        ctx.phase(&format!("{id} · pass {iterations}"), iterations - 1, max);
        let mut scope = outer.clone();
        let result = run_steps(ctx, &op.steps, &mut scope).await;
        local = op
            .steps
            .iter()
            .filter_map(|step| {
                scope
                    .get(&step.id)
                    .map(|value| (step.id.clone(), value.clone()))
            })
            .collect();
        if let Err(error) = result {
            let reason = if ctx.is_cancelled() {
                "cancelled"
            } else {
                "failed"
            };
            return loop_result(local, iterations, reason, Some(error));
        }
        let stop = match op.until.as_deref() {
            None => false,
            Some(reference) => match resolve_ref(reference, &scope).and_then(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| format!("loop condition '{reference}' must be a boolean"))
            }) {
                Ok(stop) => stop,
                Err(error) => return loop_result(local, iterations, "failed", Some(error)),
            },
        };
        if stop || iterations >= max {
            return loop_result(
                local,
                iterations,
                if stop { "condition_met" } else { "max_rounds" },
                None,
            );
        }
    }
}

fn loop_result(
    mut local: Map<String, Value>,
    iterations: usize,
    reason: &str,
    error: Option<String>,
) -> WorkflowRun {
    local.insert("_iterations".into(), json!(iterations));
    local.insert("_stop_reason".into(), json!(reason));
    WorkflowRun {
        output: Value::Object(local),
        error,
    }
}

async fn run_agent_op(
    ctx: &WorkflowContext,
    id: &str,
    op: &AgentOp,
    scope: &Map<String, Value>,
) -> WorkflowRun {
    let prompt = match interpolate(&op.prompt, scope, None) {
        Ok(prompt) => prompt,
        Err(error) => return WorkflowRun::failed(Value::Null, error),
    };
    let mut spec = AgentSpec::new(prompt, id.to_string());
    if let Some(s) = &op.schema {
        spec = spec.with_schema(s.clone());
    }
    let outcome = agent(ctx, spec).await;
    if outcome.is_success() {
        WorkflowRun::success(outcome.output.expect("successful agent has output"))
    } else {
        WorkflowRun::failed(Value::Null, outcome.failure_message())
    }
}

async fn run_fan_out(
    ctx: &WorkflowContext,
    id: &str,
    op: &FanOutOp,
    scope: &Map<String, Value>,
) -> WorkflowRun {
    let items = match prepare_items(op, scope) {
        Ok(items) => items,
        Err(error) => return WorkflowRun::failed(Value::Null, error),
    };
    let repeat = op.repeat.unwrap_or(1);
    let thunks: Vec<_> = items
        .into_iter()
        .enumerate()
        .flat_map(|(i, (item, prompt))| {
            (0..repeat).map(move |r| (i, r, item.clone(), prompt.clone()))
        })
        .map(|(i, r, item, prompt)| {
            let label = if repeat > 1 {
                format!("{id}:{}#{}", i + 1, r + 1)
            } else {
                format!("{id}:{}", i + 1)
            };
            let mut spec = AgentSpec::new(prompt, label).with_input(item);
            if let Some(schema) = &op.schema {
                spec = spec.with_schema(schema.clone());
            }
            agent(ctx, spec)
        })
        .collect();
    let outcomes = parallel(thunks).await;
    let error = outcomes
        .iter()
        .find(|outcome| !outcome.is_success())
        .map(|outcome| format!("fan-out incomplete: {}", outcome.failure_message()));
    let values: Vec<Value> = outcomes
        .into_iter()
        .map(|outcome| {
            if outcome.is_success() {
                outcome.output.unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        })
        .collect();
    let output = if repeat > 1 {
        Value::Array(
            values
                .chunks(repeat)
                .map(|chunk| Value::Array(chunk.to_vec()))
                .collect(),
        )
    } else {
        Value::Array(values)
    };
    WorkflowRun { output, error }
}

fn prepare_items(
    op: &FanOutOp,
    scope: &Map<String, Value>,
) -> Result<Vec<(Value, String)>, String> {
    let value = match op.over.as_str() {
        Some(reference) => resolve_ref(reference, scope)?,
        None => &op.over,
    };
    let items = value
        .as_array()
        .ok_or_else(|| "fan-out 'over' must resolve to an array".to_string())?;
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            interpolate(&op.prompt, scope, Some(item))
                .map(|prompt| (item.clone(), prompt))
                .map_err(|error| format!("fan-out item {}: {error}", i + 1))
        })
        .collect()
}

async fn run_parallel(
    ctx: &WorkflowContext,
    id: &str,
    op: &ParallelOp,
    scope: &Map<String, Value>,
) -> WorkflowRun {
    let thunks = op.branches.iter().map(|(name, op)| {
        let step = Step {
            id: format!("{id}.{name}"),
            op: op.clone(),
        };
        async move { (name.clone(), run_step(ctx, &step, scope).await) }
    });
    let mut result = Map::new();
    let mut errors = Vec::new();
    for (name, run) in futures::future::join_all(thunks).await {
        if let Some(error) = run.error {
            errors.push(format!("branch '{name}': {error}"));
        }
        result.insert(name, run.output);
    }
    WorkflowRun {
        output: Value::Object(result),
        error: if errors.is_empty() {
            None
        } else {
            Some(errors.join("; "))
        },
    }
}

fn resolve_ref<'a>(reference: &str, scope: &'a Map<String, Value>) -> Result<&'a Value, String> {
    let parts = reference_parts(reference)?;
    let mut value = scope
        .get(parts[0])
        .ok_or_else(|| format!("unresolved reference '{reference}'"))?;
    for field in &parts[1..] {
        value = value
            .get(*field)
            .ok_or_else(|| format!("missing field '{field}' in reference '{reference}'"))?;
    }
    Ok(value)
}

fn interpolate(
    template: &str,
    scope: &Map<String, Value>,
    item: Option<&Value>,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut cursor = 0;
    for (range, key) in placeholders(template)? {
        out.push_str(&template[cursor..range.start]);
        let value = if key.starts_with('$') {
            resolve_ref(key, scope)?
        } else {
            let mut value = item.ok_or_else(|| format!("'{{{key}}}' requires a fan-out item"))?;
            for field in key.split('.').skip(1) {
                value = value
                    .get(field)
                    .ok_or_else(|| format!("missing item field in '{{{key}}}'"))?;
            }
            value
        };
        out.push_str(&value_to_text(value));
        cursor = range.end;
    }
    out.push_str(&template[cursor..]);
    Ok(out)
}

/// Render a JSON value as prompt text: strings verbatim, everything else pretty JSON.
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_nested_ref() {
        let mut scope = Map::new();
        scope.insert("crates".into(), json!({ "list": ["a", "b"] }));
        assert_eq!(
            resolve_ref("$crates.list", &scope).unwrap(),
            &json!(["a", "b"])
        );
        assert!(resolve_ref("$missing.x", &scope)
            .unwrap_err()
            .contains("$missing.x"));
        assert!(resolve_ref("$crates.missing", &scope).is_err());
    }

    #[test]
    fn interpolates_item_and_ref() {
        let mut scope = Map::new();
        scope.insert("cov".into(), json!({ "pct": 87 }));
        let item = json!({ "name": "core", "feedback": "add edge tests" });
        assert_eq!(
            interpolate("fix {item.feedback} in {item.name}", &scope, Some(&item)).unwrap(),
            "fix add edge tests in core"
        );
        assert_eq!(
            interpolate("coverage is {$cov.pct}", &scope, None).unwrap(),
            "coverage is 87"
        );
    }

    #[test]
    fn interpolation_preserves_json_and_rejects_missing_item_fields() {
        let scope = Map::from_iter([("data".into(), json!({"value": 2}))]);
        let item = json!({"nested":{"0":{"name":"cli"}}});
        assert_eq!(
            interpolate(
                r#"{"name":"{item.nested.0.name}","value":{$data.value}}"#,
                &scope,
                Some(&item)
            )
            .unwrap(),
            r#"{"name":"cli","value":2}"#
        );
        assert!(interpolate("{item.missing}", &scope, Some(&item)).is_err());
        assert!(interpolate("{$data.missing}", &scope, None).is_err());
    }

    // --- end-to-end: the crates → tests → adversarial reviews → fix → loop
    // pipeline, driven by a MockProvider that answers each step by prompt keyword.
    use crate::core::events::EventBus;
    use crate::providers::mock::{MockProvider, MockReply, MockRule};
    use crate::providers::provider::Provider;
    use crate::tools::registry::ToolRegistry;
    use std::sync::Arc;

    fn rule(needle: &str, out: Value) -> MockRule {
        MockRule {
            needle: needle.to_string(),
            reply: MockReply::ToolCall {
                name: "structured_output".to_string(),
                input: out,
            },
        }
    }

    fn ctx_with(provider: Arc<dyn Provider>) -> WorkflowContext {
        WorkflowContext::new(
            "wf-dsl".to_string(),
            provider,
            EventBus::new(),
            ".".to_string(),
            ToolRegistry::new(None),
            None,
            None,
            crate::tools::jobs::JobRegistry::new(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            8,
        )
    }

    fn ctx_with_limit(provider: Arc<dyn Provider>, limit: usize) -> WorkflowContext {
        WorkflowContext::new(
            "wf-dsl".to_string(),
            provider,
            EventBus::new(),
            ".".to_string(),
            ToolRegistry::new(None),
            None,
            None,
            crate::tools::jobs::JobRegistry::new(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            limit,
        )
    }

    // Repro guard for the "entire TUI froze" deadlock: a `parallel` of `fan_out`s
    // under a TIGHT concurrency limit. If nested parallelism holds a semaphore
    // permit across the inner `agent()` acquisitions, this hangs — the timeout
    // turns a hang into a failing test instead of wedging the runner.
    #[tokio::test]
    async fn nested_parallel_does_not_deadlock_under_tight_limit() {
        let provider = MockProvider::new(vec![]).with_default(MockReply::Text("ok".into()));
        let spec: Spec = serde_json::from_value(json!({
            "steps": [
                { "id": "fan", "parallel": { "branches": {
                    "a": { "fan_out": { "over": ["1","2","3"], "prompt": "do {item}" } },
                    "b": { "fan_out": { "over": ["4","5","6"], "prompt": "do {item}" } },
                    "c": { "fan_out": { "over": ["7","8","9"], "prompt": "do {item}" } }
                } } }
            ]
        }))
        .unwrap();
        // Limit 2 ≪ the 9 concurrent leaf agents this wants to run.
        let fut = run(ctx_with_limit(Arc::new(provider), 2), spec);
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("workflow deadlocked (timed out)");
        assert!(out.error.is_none(), "{:?}", out.error);
        // All 3 branches × 3 items produced a result.
        for b in ["a", "b", "c"] {
            assert_eq!(out.output["fan"][b].as_array().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn crates_tests_reviews_fix_loop_pipeline() {
        // A valid false condition exercises every iteration without hiding a failed check.
        let provider = MockProvider::new(vec![
            rule("list all crates", json!({ "list": ["core", "cli"] })),
            rule(
                "generate unit tests",
                json!({ "file": "tests.rs", "ok": true }),
            ),
            rule(
                "adversarially review",
                json!({ "feedback": "add an edge case" }),
            ),
            rule("does coverage pass", json!({"pass": false})),
        ]);

        let spec: Spec = serde_json::from_value(json!({
            "steps": [
                { "id": "crates", "agent": {
                    "prompt": "list all crates in this repo",
                    "schema": { "type": "object", "required": ["list"] } } },
                { "id": "improve", "loop": {
                    "max": 2,
                    "until": "$coverage.pass",
                    "steps": [
                        { "id": "tests", "fan_out": {
                            "over": "$crates.list",
                            "prompt": "generate unit tests for {item}",
                            "schema": { "type": "object", "required": ["file"] } } },
                        { "id": "reviews", "fan_out": {
                            "over": "$tests", "repeat": 2,
                            "prompt": "adversarially review {item}",
                            "schema": { "type": "object", "required": ["feedback"] } } },
                        { "id": "fixes", "fan_out": {
                            "over": "$reviews",
                            "prompt": "address this review: {item}" } },
                        { "id": "coverage", "agent": {
                            "prompt": "does coverage pass now?",
                            "schema": { "type": "object", "required": ["pass"] } } }
                    ] } }
            ]
        }))
        .unwrap();

        let result = run(ctx_with(Arc::new(provider)), spec).await;
        assert!(result.error.is_none(), "{:?}", result.error);
        let out = result.output;

        // crates discovered.
        assert_eq!(out["crates"]["list"], json!(["core", "cli"]));
        // The loop produced tests (one per crate = 2) and reviews (2 crates × 2 = a
        // 2-element array of 2-element arrays).
        let improve = &out["improve"];
        assert_eq!(improve["tests"].as_array().unwrap().len(), 2);
        let reviews = improve["reviews"].as_array().unwrap();
        assert_eq!(reviews.len(), 2); // one group per test
        assert_eq!(reviews[0].as_array().unwrap().len(), 2); // repeat: 2
        assert_eq!(improve["_iterations"], 2);
        assert_eq!(improve["_stop_reason"], "max_rounds");
    }

    #[tokio::test]
    async fn loop_stops_when_condition_is_true() {
        // Coverage passes immediately → the loop runs exactly one pass.
        let provider = MockProvider::new(vec![rule("coverage", json!({ "pass": true }))]);
        let spec: Spec = serde_json::from_value(json!({
            "steps": [
                { "id": "improve", "loop": {
                    "max": 5, "until": "$check.pass",
                    "steps": [
                        { "id": "check", "agent": {
                            "prompt": "coverage check",
                            "schema": { "type": "object", "required": ["pass"] } } }
                    ] } }
            ]
        }))
        .unwrap();
        let out = run(ctx_with(Arc::new(provider)), spec).await;
        assert!(out.error.is_none(), "{:?}", out.error);
        assert_eq!(out.output["improve"]["_iterations"], json!(1));
        assert_eq!(out.output["improve"]["_stop_reason"], "condition_met");
    }
}
