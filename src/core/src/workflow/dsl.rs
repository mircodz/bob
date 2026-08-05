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

use super::{agent, parallel, AgentSpec, WorkflowContext};
use serde::Deserialize;
use serde_json::{json, Map, Value};

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
    /// Re-run an inner sequence of steps until `until` resolves truthy (or the
    /// round cap is hit). The inner steps write to the SAME scope, so `until` can
    /// reference an output produced inside the loop.
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
    pub branches: Map<String, Value>,
}

/// Re-run `steps` until `until` resolves truthy (or `max` iterations). The inner
/// steps share the enclosing scope, so `until` typically references an output a
/// step inside the loop produces (e.g. a coverage check). Loops nest — a `Loop`
/// step may appear inside another loop's `steps`.
#[derive(Debug, Clone, Deserialize)]
pub struct LoopOp {
    pub steps: Vec<Step>,
    /// A `$ref` to a boolean; when truthy the loop stops. Omit to always run `max`.
    #[serde(default)]
    pub until: Option<String>,
    /// Iteration cap (default 5, clamped to 1..=20).
    #[serde(default)]
    pub max: Option<usize>,
}

/// Run a spec to completion: execute its steps in order, threading each step's
/// output into a shared scope. Returns the final scope (every step's output keyed
/// by id).
pub async fn run(ctx: WorkflowContext, spec: Spec) -> Value {
    let mut scope = Map::new();
    run_steps(&ctx, &spec.steps, &mut scope).await;
    Value::Object(scope)
}

/// Run an ordered list of steps, writing each result into `scope` under its id.
/// Each step announces a phase (so the inline tree groups agents by step name),
/// EXCEPT loop steps, which emit their own per-pass phases.
async fn run_steps(ctx: &WorkflowContext, steps: &[Step], scope: &mut Map<String, Value>) {
    let total = steps.len();
    for (i, step) in steps.iter().enumerate() {
        if ctx.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        if !matches!(step.op, Op::Loop(_)) {
            ctx.phase(&step.id, i, total);
        }
        let out = run_step(ctx, step, scope).await;
        scope.insert(step.id.clone(), out);
    }
}

/// Execute one step, returning its output value. Boxed because steps recurse
/// (a `Loop`/`Parallel` step runs sub-steps, which are steps).
fn run_step<'a>(
    ctx: &'a WorkflowContext,
    step: &'a Step,
    scope: &'a Map<String, Value>,
) -> futures::future::BoxFuture<'a, Value> {
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
) -> Value {
    let max = op.max.unwrap_or(5).clamp(1, 20);
    // Start from the enclosing scope so inner steps can read prior outputs.
    let mut scope = outer.clone();
    let mut iterations = 0;
    loop {
        iterations += 1;
        ctx.phase(&format!("{id} · pass {iterations}"), iterations - 1, max);
        run_steps(ctx, &op.steps, &mut scope).await;
        let stop = match &op.until {
            None => iterations >= max,
            Some(cond) => truthy(&resolve_ref(cond, &scope)) || iterations >= max,
        };
        if stop {
            break;
        }
    }
    // Return only the keys the loop's own steps produced, plus the count, so the
    // parent scope isn't polluted with duplicates of the outer keys.
    let mut local = Map::new();
    for step in &op.steps {
        if let Some(v) = scope.get(&step.id) {
            local.insert(step.id.clone(), v.clone());
        }
    }
    local.insert("_iterations".into(), json!(iterations));
    Value::Object(local)
}

async fn run_agent_op(
    ctx: &WorkflowContext,
    id: &str,
    op: &AgentOp,
    scope: &Map<String, Value>,
) -> Value {
    let prompt = interpolate(&op.prompt, scope, None);
    let mut spec = AgentSpec::new(prompt, id.to_string());
    if let Some(s) = &op.schema {
        spec = spec.with_schema(s.clone());
    }
    agent(ctx, spec).await.unwrap_or(Value::Null)
}

async fn run_fan_out(
    ctx: &WorkflowContext,
    id: &str,
    op: &FanOutOp,
    scope: &Map<String, Value>,
) -> Value {
    // `over` is either a $ref to a list, or an inline array.
    let items = as_list(&resolve_value(&op.over, scope));
    let repeat = op.repeat.unwrap_or(1).max(1);
    let schema = op.schema.clone();
    let prompt_tpl = op.prompt.clone();

    let thunks: Vec<_> = items
        .iter()
        .enumerate()
        .flat_map(|(i, item)| (0..repeat).map(move |r| (i, r, item.clone())))
        .map(|(i, r, item)| {
            let ctx = ctx.clone();
            let prompt = interpolate(&prompt_tpl, scope, Some(&item));
            let label = if repeat > 1 {
                format!("{id}:{}#{}", i + 1, r + 1)
            } else {
                format!("{id}:{}", i + 1)
            };
            let mut spec = AgentSpec::new(prompt, label);
            if let Some(s) = schema.clone() {
                spec = spec.with_schema(s);
            }
            async move { agent(&ctx, spec).await }
        })
        .collect();

    let flat = parallel(thunks).await;
    if repeat > 1 {
        // Group by item: [[r1, r2], [r1, r2], …].
        let mut grouped: Vec<Value> = Vec::new();
        for chunk in flat.chunks(repeat) {
            grouped.push(Value::Array(
                chunk
                    .iter()
                    .map(|o| o.clone().unwrap_or(Value::Null))
                    .collect(),
            ));
        }
        Value::Array(grouped)
    } else {
        Value::Array(flat.into_iter().map(|o| o.unwrap_or(Value::Null)).collect())
    }
}

async fn run_parallel(
    ctx: &WorkflowContext,
    id: &str,
    op: &ParallelOp,
    scope: &Map<String, Value>,
) -> Value {
    // Each branch is a single-step op (agent/fan_out/parallel). Run them together
    // and return an object keyed by branch name.
    let names: Vec<String> = op.branches.keys().cloned().collect();
    let mut result = Map::new();
    // Sequentially build sub-steps (cheap), then run their futures concurrently.
    let thunks: Vec<_> = names
        .iter()
        .map(|name| {
            let branch_val = op.branches[name].clone();
            let ctx = ctx.clone();
            let scope = scope.clone();
            let sub_id = format!("{id}.{name}");
            async move {
                // Parse the branch as a step op and run it.
                match serde_json::from_value::<Op>(branch_val) {
                    Ok(sub_op) => {
                        let sub = Step {
                            id: sub_id,
                            op: sub_op,
                        };
                        run_step(&ctx, &sub, &scope).await
                    }
                    Err(_) => Value::Null,
                }
            }
        })
        .collect();
    let outs = futures::future::join_all(thunks).await;
    for (name, out) in names.into_iter().zip(outs) {
        result.insert(name, out);
    }
    Value::Object(result)
}

// --- reference resolution + interpolation ----------------------------------

/// Resolve a value that may be a `$ref` string, otherwise return it as-is.
fn resolve_value(v: &Value, scope: &Map<String, Value>) -> Value {
    match v.as_str() {
        Some(s) if s.starts_with('$') => resolve_ref(s, scope),
        _ => v.clone(),
    }
}

/// Resolve a `$id` / `$id.field.sub` reference against the scope. Missing → Null.
fn resolve_ref(reference: &str, scope: &Map<String, Value>) -> Value {
    let path = reference.trim_start_matches('$');
    let mut cur = Value::Object(scope.clone());
    for seg in path.split('.') {
        cur = cur.get(seg).cloned().unwrap_or(Value::Null);
    }
    cur
}

/// Interpolate `{...}` placeholders in a prompt: `{item}` → the current fan-out
/// item (as compact JSON if not a string), and `{$id.field}` → a scope ref.
fn interpolate(template: &str, scope: &Map<String, Value>, item: Option<&Value>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push('{');
            rest = after;
            continue;
        };
        let key = &after[..close];
        let replacement = if key == "item" {
            item.map(value_to_text).unwrap_or_default()
        } else if let Some(field) = key.strip_prefix("item.") {
            item.and_then(|it| it.get(field))
                .map(value_to_text)
                .unwrap_or_default()
        } else if key.starts_with('$') {
            value_to_text(&resolve_ref(key, scope))
        } else {
            // Unknown placeholder — leave it literally so it's visible.
            format!("{{{key}}}")
        };
        out.push_str(&replacement);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Render a JSON value as prompt text: strings verbatim, everything else pretty JSON.
fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        _ => serde_json::to_string_pretty(v).unwrap_or_default(),
    }
}

/// Coerce a value into a list of items to fan out over. An array stays as-is; a
/// non-array becomes a single-element list; Null becomes empty.
fn as_list(v: &Value) -> Vec<Value> {
    match v {
        Value::Array(a) => a.clone(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

/// Truthiness for `loop_until`: `true`, a non-zero number, or a non-empty string.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty() && s != "false",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_nested_ref() {
        let mut scope = Map::new();
        scope.insert("crates".into(), json!({ "list": ["a", "b"] }));
        assert_eq!(resolve_ref("$crates.list", &scope), json!(["a", "b"]));
        assert_eq!(resolve_ref("$missing.x", &scope), Value::Null);
    }

    #[test]
    fn interpolates_item_and_ref() {
        let mut scope = Map::new();
        scope.insert("cov".into(), json!({ "pct": 87 }));
        let item = json!({ "name": "core", "feedback": "add edge tests" });
        assert_eq!(
            interpolate("fix {item.feedback} in {item.name}", &scope, Some(&item)),
            "fix add edge tests in core"
        );
        assert_eq!(
            interpolate("coverage is {$cov.pct}", &scope, None),
            "coverage is 87"
        );
    }

    #[test]
    fn truthy_covers_common_cases() {
        assert!(truthy(&json!(true)));
        assert!(!truthy(&json!(false)));
        assert!(truthy(&json!(1)));
        assert!(!truthy(&json!(0)));
        assert!(!truthy(&Value::Null));
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
        // All 3 branches × 3 items produced a result.
        for b in ["a", "b", "c"] {
            assert_eq!(out["fan"][b].as_array().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn crates_tests_reviews_fix_loop_pipeline() {
        // The mock keys off distinctive words in each step's prompt. Coverage has no
        // rule → it never produces a structured `pass`, so `$coverage.pass` stays
        // Null (falsy) and the loop runs to its `max` — which lets us assert the
        // full structure the pipeline builds each pass.
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
            rule("address this review", json!({ "done": true })),
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

        // coverage never matches a rule → the loop runs to max=2. We assert the
        // STRUCTURE the pipeline produced each pass.
        let out = run(ctx_with(Arc::new(provider)), spec).await;

        // crates discovered.
        assert_eq!(out["crates"]["list"], json!(["core", "cli"]));
        // The loop produced tests (one per crate = 2) and reviews (2 crates × 2 = a
        // 2-element array of 2-element arrays).
        let improve = &out["improve"];
        assert_eq!(improve["tests"].as_array().unwrap().len(), 2);
        let reviews = improve["reviews"].as_array().unwrap();
        assert_eq!(reviews.len(), 2); // one group per test
        assert_eq!(reviews[0].as_array().unwrap().len(), 2); // repeat: 2
                                                             // The loop ran (iteration count present).
        assert!(improve["_iterations"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn loop_stops_when_until_is_truthy() {
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
        assert_eq!(out["improve"]["_iterations"], json!(1));
    }
}
