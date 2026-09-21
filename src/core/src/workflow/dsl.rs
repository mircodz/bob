//! Declarative workflows with scoped `$id.field` references between steps.

use super::validation::{placeholders, reference_parts};
use super::{agent, parallel, AgentSpec, WorkflowContext, WorkflowRun};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize)]
pub struct Spec {
    pub steps: Vec<Step>,
}

/// Stores its result under `id` for subsequent steps in the same scope.
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    pub id: String,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Agent(AgentOp),
    FanOut(FanOutOp),
    /// Results are keyed by branch name.
    Parallel(ParallelOp),
    /// Exposes inner results through the loop step's output.
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

/// Returns every step's output keyed by id, retaining partial results on failure.
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

/// Boxed for recursive loop and parallel steps.
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
    if items.len() > super::validation::MAX_ITEMS {
        return Err(format!(
            "fan-out exceeds {} items",
            super::validation::MAX_ITEMS
        ));
    }
    super::validation::validate_count(op.repeat.unwrap_or(1))?;
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
    for (name, branch) in &op.branches {
        if let Err(error) = preflight(branch, scope) {
            return WorkflowRun::failed(Value::Null, format!("branch '{name}': {error}"));
        }
    }
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

// Branch-local results remain unknown until their producing steps execute. Check
// each shared-scope reference independently, even alongside an unknown local one.
fn preflight(op: &Op, scope: &Map<String, Value>) -> Result<(), String> {
    match op {
        Op::Agent(agent) => preflight_prompt(&agent.prompt, scope),
        Op::FanOut(fan) => {
            preflight_prompt(&fan.prompt, scope)?;
            let value = match fan.over.as_str() {
                Some(reference) => resolve_available_ref(reference, scope)?,
                None => Some(&fan.over),
            };
            if let Some(value) = value {
                let items = value
                    .as_array()
                    .ok_or_else(|| "fan-out 'over' must resolve to an array".to_string())?;
                super::validation::validate_items(&fan.prompt, items)?;
            }
            Ok(())
        }
        Op::Parallel(parallel) => {
            for (name, branch) in &parallel.branches {
                preflight(branch, scope).map_err(|error| format!("branch '{name}': {error}"))?;
            }
            Ok(())
        }
        Op::Loop(loop_op) => {
            for step in &loop_op.steps {
                preflight(&step.op, scope)
                    .map_err(|error| format!("step '{}': {error}", step.id))?;
            }
            if let Some(reference) = &loop_op.until {
                if resolve_available_ref(reference, scope)?.is_some_and(|value| !value.is_boolean())
                {
                    return Err(format!("loop condition '{reference}' must be a boolean"));
                }
            }
            Ok(())
        }
    }
}

fn preflight_prompt(prompt: &str, scope: &Map<String, Value>) -> Result<(), String> {
    for (_, key) in placeholders(prompt)? {
        if key.starts_with('$') {
            resolve_available_ref(key, scope)?;
        }
    }
    Ok(())
}

fn resolve_available_ref<'a>(
    reference: &str,
    scope: &'a Map<String, Value>,
) -> Result<Option<&'a Value>, String> {
    let parts = reference_parts(reference)?;
    if scope.contains_key(parts[0]) {
        resolve_ref(reference, scope).map(Some)
    } else {
        Ok(None)
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
            let item = item.ok_or_else(|| format!("'{{{key}}}' requires a fan-out item"))?;
            super::validation::item_value(key, item)?
        };
        out.push_str(&value_to_text(value));
        cursor = range.end;
    }
    out.push_str(&template[cursor..]);
    Ok(out)
}

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
    fn reference_paths_distinguish_missing_null_and_numeric_object_keys() {
        let scope = Map::from_iter([(
            "source".into(),
            json!({
                "object": {"0": null}, "array": ["value"], "scalar": false
            }),
        )]);
        assert_eq!(resolve_ref("$source.object.0", &scope), Ok(&Value::Null));
        for reference in [
            "$source.array.0",
            "$source.scalar.field",
            "$source.object.1",
        ] {
            let error = resolve_ref(reference, &scope).unwrap_err();
            assert!(error.contains(reference), "{error}");
        }
        assert_eq!(resolve_available_ref("$future.value", &scope), Ok(None));
        assert!(resolve_available_ref("$source.missing", &scope).is_err());
        assert!(resolve_available_ref("$future..value", &scope).is_err());
    }

    #[test]
    fn interpolation_is_single_pass_and_preserves_unicode_and_literal_braces() {
        let scope = Map::from_iter([(
            "source".into(),
            json!({
                "text": "{$missing} {item.nope}", "flag": false, "empty": null
            }),
        )]);
        let item = json!({"text": "{$source.flag} λ"});
        assert_eq!(interpolate(
        "é {$source.text}|{item.text}|{$source.flag}|{$source.empty}|{$source.text} {items} {unknown}",
        &scope, Some(&item)
    ).unwrap(), "é {$missing} {item.nope}|{$source.flag} λ|false|null|{$missing} {item.nope} {items} {unknown}");
        assert_eq!(
            interpolate("{item}", &scope, Some(&json!([1, null]))).unwrap(),
            "[\n  1,\n  null\n]"
        );
        assert!(interpolate("{item}", &scope, None)
            .unwrap_err()
            .contains("requires a fan-out item"));
    }

    #[test]
    fn preflight_defers_only_unknown_roots_not_invalid_known_values() {
        let scope = Map::from_iter([("source".into(), json!({"done": "true"}))]);
        for prompt in ["{$local} {$source.missing}", "{$source.missing} {$local}"] {
            assert!(preflight_prompt(prompt, &scope)
                .unwrap_err()
                .contains("$source.missing"));
        }
        let op: Op = serde_json::from_value(json!({"loop": {
            "steps": [{"id": "local", "agent": {"prompt": "work"}}],
            "until": "$source.done"
        }}))
        .unwrap();
        assert!(preflight(&op, &scope)
            .unwrap_err()
            .contains("must be a boolean"));
    }

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

    use crate::core::events::EventBus;
    use crate::providers::mock::{MockReply, MockRule};
    use crate::providers::provider::Provider;
    use crate::tools::registry::ToolRegistry;
    use crate::workflow::tests::{
        assert_error, check_loop, context, run_checked, spec, structured, text, MockProvider,
    };
    use crate::workflow::{input, AgentOutcome, OutcomeStatus};
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

    // Containers must not hold permits while their leaf agents await admission.
    #[tokio::test]
    async fn nested_parallel_does_not_deadlock_under_tight_limit() {
        let provider = MockProvider::new((1..=9).map(|item| text(&format!("do {item}"))).collect());
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
        let fut = run(ctx_with_limit(Arc::new(provider), 2), spec);
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), fut)
            .await
            .expect("workflow deadlocked (timed out)");
        assert!(out.error.is_none(), "{:?}", out.error);
        for b in ["a", "b", "c"] {
            assert_eq!(out.output["fan"][b].as_array().unwrap().len(), 3);
        }
    }

    #[tokio::test]
    async fn crates_tests_reviews_fix_loop_pipeline() {
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
            text("address this review:"),
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

        assert_eq!(out["crates"]["list"], json!(["core", "cli"]));
        let improve = &out["improve"];
        assert_eq!(improve["tests"].as_array().unwrap().len(), 2);
        let reviews = improve["reviews"].as_array().unwrap();
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[0].as_array().unwrap().len(), 2);
        assert_eq!(improve["_iterations"], 2);
        assert_eq!(improve["_stop_reason"], "max_rounds");
    }

    #[tokio::test]
    async fn invalid_nested_output_never_reaches_dependent_steps() {
        let provider = MockProvider::new(vec![
            structured("source data", json!({"rows":[{"count":1.5}]})),
            text("You did not record a result."),
        ]);
        let ctx = context(Arc::new(provider.clone()), 1);
        let result = run_checked(
        &ctx,
        spec(json!([
            {"id":"source","agent":{"prompt":"source data","schema":{
                "type":"object","required":["rows"],"properties":{"rows":{"type":"array","items":{
                    "type":"object","required":["count"],"properties":{"count":{"type":"integer"}}
                }}}
            }}},
            {"id":"dependent","agent":{"prompt":"consume {$source}"}}
        ])),
    )
    .await;
        assert!(result.error.is_some());
        assert!(result.output.get("dependent").is_none());
        assert_eq!(ctx.outcomes().len(), 1);
        assert_eq!(ctx.outcomes()[0].status, OutcomeStatus::Failed);
    }

    #[tokio::test]
    async fn dynamic_fan_out_limits_are_checked_before_any_item_runs() {
        let provider = MockProvider::new(vec![structured(
            "produce source",
            json!({"items":vec!["x";257]}),
        )]);
        let ctx = context(Arc::new(provider), 1);
        let result = run_checked(
            &ctx,
            spec(json!([
                {"id":"source","agent":{"prompt":"produce source","schema":{"type":"object"}}},
                {"id":"batch","fan_out":{"over":"$source.items","prompt":"consume {item}"}}
            ])),
        )
        .await;
        assert_error(&result, "exceeds 256 items");
        assert_eq!(ctx.outcomes().len(), 1);
    }

    #[tokio::test]
    async fn missing_runtime_output_field_stops_before_dependent_agent() {
        let provider = MockProvider::new(vec![structured("produce source", json!({"present": 7}))]);
        let ctx = context(Arc::new(provider.clone()), 2);
        let result = run_checked(
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
            let result = run_checked(
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
    async fn repeated_fan_out_failure_keeps_item_groups_and_skips_following_steps() {
        let provider = MockProvider::new(vec![
            structured("inspect alpha", json!({"name": "alpha"})),
            text("inspect broken"),
            text("You did not record a result."),
            structured("inspect omega", json!({"name": "omega"})),
        ]);
        let ctx = context(Arc::new(provider), 2);
        let result = run_checked(
            &ctx,
            spec(json!([
                {"id": "batch", "fan_out": {
                    "over": ["alpha", "broken", "omega"], "repeat": 2,
                    "prompt": "inspect {item}", "schema": {"type": "object"}
                }},
                {"id": "dependent", "agent": {"prompt": "summarize {$batch}"}}
            ])),
        )
        .await;
        assert_error(&result, "fan-out incomplete");
        assert_eq!(
            result.output,
            json!({"batch": [
                [{"name": "alpha"}, {"name": "alpha"}],
                [null, null],
                [{"name": "omega"}, {"name": "omega"}]
            ]})
        );
        let outcomes = ctx.outcomes();
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.input.clone())
                .collect::<Vec<_>>(),
            vec![
                Some(json!("alpha")),
                Some(json!("alpha")),
                Some(json!("broken")),
                Some(json!("broken")),
                Some(json!("omega")),
                Some(json!("omega")),
            ]
        );
        assert_eq!(
            outcomes
                .iter()
                .map(|outcome| outcome.status)
                .collect::<Vec<_>>(),
            [
                OutcomeStatus::Success,
                OutcomeStatus::Success,
                OutcomeStatus::Failed,
                OutcomeStatus::Failed,
                OutcomeStatus::Success,
                OutcomeStatus::Success,
            ]
        );
    }

    #[tokio::test]
    async fn empty_fan_out_arrays_are_successful_and_allow_following_steps() {
        for repeat in [1, 2] {
            let provider = MockProvider::new(vec![
                structured("produce source", json!({"items": []})),
                text("after empty batches"),
            ]);
            let ctx = context(Arc::new(provider.clone()), 2);
            let result = run_checked(
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
            let input = json!({"steps": steps});
            if !referenced {
                assert!(input::validate(&input)
                    .unwrap_err()
                    .contains("missing item field"));
            }
            let workflow = serde_json::from_value(input).unwrap();
            let result = run_checked(&ctx, workflow).await;

            assert_error(
                &result,
                "fan-out item 2: missing item field in '{item.details.name}'",
            );
            assert_eq!(
                result.output.get("batch"),
                referenced.then_some(&Value::Null)
            );
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
    async fn loop_uses_each_fresh_result_and_stops_when_false_changes_to_true() {
        use crate::core::types::ContentBlock;
        use crate::test_support::agent::{text as reply_text, ScriptProvider, Step as Reply};

        let mut replies = Vec::new();
        for (draft, done) in [("draft one", false), ("draft two", true)] {
            replies.push(Reply::Complete(vec![reply_text(draft)]));
            replies.push(Reply::Complete(vec![ContentBlock::ToolUse {
                id: "verdict".into(),
                name: "structured_output".into(),
                input: json!({"done": done}),
            }]));
            replies.push(Reply::Complete(vec![reply_text("recorded")]));
        }
        replies.push(Reply::Complete(vec![reply_text("finished")]));
        let provider = Arc::new(ScriptProvider::new(replies));
        let ctx = context(provider.clone(), 1);
        let result = run_checked(&ctx, spec(json!([
        {"id": "rounds", "loop": {"max": 3, "until": "$check.done", "steps": [
            {"id": "draft", "agent": {"prompt": "make a draft"}},
            {"id": "check", "agent": {"prompt": "review {$draft.text}", "schema": {"type": "object"}}}
        ]}},
        {"id": "after", "agent": {"prompt": "publish {$rounds.draft.text}"}}
    ]))).await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(
            result.output,
            json!({
                "rounds": {"draft": {"text": "draft two"}, "check": {"done": true},
                    "_iterations": 2, "_stop_reason": "condition_met"},
                "after": {"text": "finished"}
            })
        );
        provider.assert_exhausted();
        let requests = provider.requests.lock().unwrap();
        let prompts: Vec<_> = requests
            .iter()
            .map(|messages| {
                messages
                    .iter()
                    .find(|message| message.role == crate::core::types::Role::User)
                    .unwrap()
                    .text()
            })
            .collect();
        assert!(prompts
            .iter()
            .any(|prompt| prompt.starts_with("review draft one")));
        assert!(prompts
            .iter()
            .any(|prompt| prompt.starts_with("review draft two")));
        assert_eq!(prompts.last().unwrap(), "publish draft two");
    }

    #[tokio::test]
    async fn failed_second_pass_does_not_export_skipped_results_from_first_pass() {
        use crate::test_support::agent::{text as reply_text, ScriptProvider, Step as Reply};

        let provider = Arc::new(ScriptProvider::new(vec![
            Reply::Complete(vec![reply_text("first input")]),
            Reply::Complete(vec![reply_text("first dependent")]),
            Reply::Events(vec![crate::core::types::StreamEvent::Error {
                message: "HTTP 401: scripted second-pass failure".into(),
            }]),
        ]));
        let ctx = context(provider.clone(), 1);
        let result = run_checked(
            &ctx,
            spec(json!([
                {"id": "rounds", "loop": {"max": 3, "steps": [
                    {"id": "draft", "agent": {"prompt": "make draft"}},
                    {"id": "dependent", "agent": {"prompt": "use {$draft.text}"}}
                ]}},
                {"id": "after", "agent": {"prompt": "must not run"}}
            ])),
        )
        .await;
        assert_error(&result, "scripted second-pass failure");
        assert_eq!(
            result.output,
            json!({"rounds": {
                "draft": null, "_iterations": 2, "_stop_reason": "failed"
            }})
        );
        provider.assert_exhausted();
        assert_eq!(provider.requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn loop_condition_is_checked_after_a_pass_even_when_outer_condition_is_true() {
        let provider = MockProvider::new(vec![
            structured("seed condition", json!({"done": true, "private": "outer"})),
            text("perform pass"),
        ]);
        let ctx = context(Arc::new(provider), 1);
        let result = run_checked(
            &ctx,
            spec(json!([
                {"id": "seed", "agent": {"prompt": "seed condition", "schema": {"type": "object"}}},
                {"id": "rounds", "loop": {"until": "$seed.done", "max": 1, "steps": [
                    {"id": "work", "agent": {"prompt": "perform pass {$seed.private}"}}
                ]}}
            ])),
        )
        .await;
        assert!(result.error.is_none(), "{:?}", result.error);
        assert_eq!(
            result.output["rounds"],
            json!({
                "work": {"text": "ok"}, "_iterations": 1, "_stop_reason": "condition_met"
            })
        );
    }

    #[tokio::test]
    async fn boolean_loop_conditions_stop_on_true_or_reach_max_rounds_on_false() {
        for (done, iterations, reason) in [(true, 1, "condition_met"), (false, 3, "max_rounds")] {
            let provider = MockProvider::new(vec![
                structured("check condition", json!({"done": done})),
                text("after loop"),
            ]);
            let ctx = context(Arc::new(provider.clone()), 1);
            let result = run_checked(&ctx, check_loop()).await;

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
            let result = run_checked(&ctx, check_loop()).await;

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
        let provider = MockProvider::new(vec![
            text("prepare draft"),
            text("check without a structured reply"),
            text("You did not record a result."),
        ]);
        let ctx = context(Arc::new(provider.clone()), 1);
        let result = run_checked(
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
            text("no structured reply"),
            text("You did not record a result."),
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
        let result = run_checked(&ctx, spec(json!([
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
}
