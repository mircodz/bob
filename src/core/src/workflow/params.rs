//! Parameterized workflows: the model picks a *shape* and fills its blanks for the
//! CURRENT task (the item list, the per-item prompt, schemas, a find prompt). The
//! engine runs a canned control-flow around them. This is the safe, no-sandbox path
//! to "use pattern X on this task": every child is a normal subagent whose tool
//! calls are still permission-gated.
//!
//! The shapes mirror the canonical workflow patterns the model can't express with
//! plain parameters otherwise:
//!   - `fan_out`    : one agent per item, return ordered results and item outcomes.
//!   - `map_reduce` : fan_out, then reduce only if every map succeeds;
//!     classify-and-act and generate-and-filter are special cases.
//!   - `loop`       : re-run a finder until a valid round turns up nothing new or
//!     the round limit is reached, retaining findings on failure or cancellation.
//!
//! Failed items retain null result slots. Shape output stays in `WorkflowRun.output`
//! on failure, with an error describing why the workflow could not complete.
//!
//! `{item}` in a prompt is substituted with each item's text.

use super::{
    agent, parallel, AgentOutcome, AgentSpec, OutcomeStatus, WorkflowContext, WorkflowRun,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Which parameterized shape to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    FanOut,
    MapReduce,
    Loop,
}

/// A model-supplied parameterized workflow spec (the `workflow` tool's input). Only
/// the fields relevant to the chosen `shape` are read; the rest may be omitted.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowParams {
    pub shape: Shape,
    /// Short label for the run, shown in the workflow view (e.g. "review-files").
    #[serde(default)]
    pub title: String,
    /// The things to operate on — one subagent per entry. Required for fan_out /
    /// map_reduce.
    #[serde(default)]
    pub items: Vec<String>,
    /// Per-item prompt (fan_out / map_reduce). `{item}` is substituted.
    #[serde(default)]
    pub map_prompt: String,
    /// Optional JSON Schema forcing each map agent's structured output.
    #[serde(default)]
    pub map_schema: Option<Value>,
    /// map_reduce: how to synthesize the collected results (appended as JSON).
    #[serde(default)]
    pub reduce_prompt: Option<String>,
    /// map_reduce: JSON Schema forcing the reduce agent's structured output.
    #[serde(default)]
    pub reduce_schema: Option<Value>,
    /// loop: the finder prompt run each round. It should return {findings: [..]},
    /// excluding anything in the seen list (substituted). Loops until a round
    /// returns nothing new, or `max_rounds` (default 5) is hit.
    #[serde(default)]
    pub find_prompt: Option<String>,
    #[serde(default)]
    pub max_rounds: Option<usize>,
}

/// Substitute `{item}` in a prompt template.
fn fill(template: &str, item: &str) -> String {
    template.replace("{item}", item)
}

/// Run a parameterized workflow, dispatching on its shape.
pub async fn run(ctx: WorkflowContext, params: WorkflowParams) -> WorkflowRun {
    match params.shape {
        Shape::FanOut | Shape::MapReduce => map_reduce(ctx, params).await,
        Shape::Loop => loop_until_done(ctx, params).await,
    }
}

/// fan_out / map_reduce: one agent per item, optionally followed by a reduce agent.
async fn map_reduce(ctx: WorkflowContext, params: WorkflowParams) -> WorkflowRun {
    let is_reduce = params.shape == Shape::MapReduce;
    ctx.phase("Map", 0, if is_reduce { 2 } else { 1 });
    let outcomes = map_items(&ctx, &params.items, &params.map_prompt, &params.map_schema).await;
    let results: Vec<Value> = outcomes.iter().map(result_value).collect();
    let item_outcomes: Vec<Value> = params
        .items
        .iter()
        .zip(&outcomes)
        .map(|(item, outcome)| {
            json!({
                "input": item,
                "id": outcome.id,
                "label": outcome.label,
                "status": match outcome.status {
                    OutcomeStatus::Success => "success",
                    OutcomeStatus::Failed => "failed",
                    OutcomeStatus::Cancelled => "cancelled",
                },
                "output": result_value(outcome),
                "error": outcome.error,
            })
        })
        .collect();
    let mut output = json!({ "results": results, "outcomes": item_outcomes });
    if let Some(failed) = outcomes.iter().find(|outcome| !outcome.is_success()) {
        return WorkflowRun::failed(output, failed.failure_message());
    }
    if ctx.is_cancelled() {
        return WorkflowRun::failed(output, "Workflow cancelled after map phase");
    }
    if !is_reduce {
        return WorkflowRun::success(output);
    }

    ctx.phase("Reduce", 1, 2);
    let reduce_prompt = params
        .reduce_prompt
        .clone()
        .unwrap_or_else(|| "Synthesize these results into a single summary.".to_string());
    let results_json = serde_json::to_string_pretty(&results).unwrap();
    let full = format!("{reduce_prompt}\n\nResults:\n{results_json}");
    let mut spec = AgentSpec::new(full, "reduce");
    if let Some(s) = params.reduce_schema.clone() {
        spec = spec.with_schema(s);
    }
    let reduced = agent(&ctx, spec).await;
    output["reduced"] = result_value(&reduced);
    if !reduced.is_success() {
        WorkflowRun::failed(output, reduced.failure_message())
    } else if ctx.is_cancelled() {
        WorkflowRun::failed(output, "Workflow cancelled during reduce phase")
    } else {
        WorkflowRun::success(output)
    }
}

/// loop: re-run the finder each round until a round surfaces nothing new (or the
/// round cap is hit), deduping against everything already found.
async fn loop_until_done(ctx: WorkflowContext, params: WorkflowParams) -> WorkflowRun {
    let find_prompt = params.find_prompt.clone().unwrap_or_else(|| {
        "Find items not already in the seen list; return {findings:[..]}, empty if none new."
            .to_string()
    });
    let max_rounds = params.max_rounds.unwrap_or(5);
    let mut seen: Vec<String> = Vec::new();
    for round in 0..max_rounds {
        if ctx.is_cancelled() {
            return WorkflowRun::failed(
                loop_output(&seen, round, "cancelled"),
                "Workflow cancelled before finder round",
            );
        }
        ctx.phase(&format!("Round {}", round + 1), round, max_rounds);
        let seen_list = if seen.is_empty() {
            "nothing yet".to_string()
        } else {
            seen.join("\n- ")
        };
        let prompt = format!(
            "{find_prompt}\n\nAlready found (do NOT repeat):\n- {seen_list}\n\nReport {{findings}} \
             as an array of strings; empty array if nothing new.",
        );
        let found = agent(
            &ctx,
            AgentSpec::new(prompt, format!("find:r{}", round + 1))
                .with_schema(list_schema("findings")),
        )
        .await;
        let rounds = round + 1;
        if !found.is_success() {
            let reason = match found.status {
                OutcomeStatus::Cancelled => "cancelled",
                _ => "failed",
            };
            return WorkflowRun::failed(
                loop_output(&seen, rounds, reason),
                found.failure_message(),
            );
        }
        let findings = match strings(&found.output, "findings") {
            Ok(findings) => findings,
            Err(error) => {
                return WorkflowRun::failed(
                    loop_output(&seen, rounds, "invalid_output"),
                    format!("Finder round {rounds}: {error}"),
                );
            }
        };
        let previous_count = seen.len();
        for finding in findings {
            if !seen.contains(&finding) {
                seen.push(finding);
            }
        }
        if ctx.is_cancelled() {
            return WorkflowRun::failed(
                loop_output(&seen, rounds, "cancelled"),
                "Workflow cancelled during finder round",
            );
        }
        if seen.len() == previous_count {
            return WorkflowRun::success(loop_output(&seen, rounds, "dry"));
        }
    }
    WorkflowRun::success(loop_output(&seen, max_rounds, "max_rounds"))
}

fn loop_output(findings: &[String], rounds: usize, reason: &str) -> Value {
    json!({ "findings": findings, "rounds": rounds, "termination_reason": reason })
}

fn result_value(outcome: &AgentOutcome) -> Value {
    if outcome.is_success() {
        outcome.output.clone().unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

// --- shared helpers --------------------------------------------------------

/// Run one agent per item concurrently, preserving every outcome in input order.
async fn map_items(
    ctx: &WorkflowContext,
    items: &[String],
    map_prompt: &str,
    map_schema: &Option<Value>,
) -> Vec<AgentOutcome> {
    let thunks: Vec<_> = items
        .iter()
        .map(|item| {
            let ctx = ctx.clone();
            let prompt = fill(map_prompt, item);
            let mut spec = AgentSpec::new(prompt, label(item)).with_input(json!(item));
            if let Some(s) = map_schema.clone() {
                spec = spec.with_schema(s);
            }
            async move { agent(&ctx, spec).await }
        })
        .collect();
    parallel(thunks).await
}

fn list_schema(key: &str) -> Value {
    json!({
        "type": "object",
        "required": [key],
        "properties": { key: { "type": "array", "items": { "type": "string" } } }
    })
}

fn strings(v: &Option<Value>, key: &str) -> Result<Vec<String>, String> {
    let values = v
        .as_ref()
        .and_then(|v| v.get(key))
        .and_then(Value::as_array)
        .ok_or_else(|| format!("expected `{key}` to be an array of strings"))?;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("expected `{key}[{index}]` to be a string"))
        })
        .collect()
}

/// A short, tree-friendly label for an item (truncated, newlines flattened).
fn label(item: &str) -> String {
    let clean = item.replace('\n', " ");
    if clean.chars().count() > 40 {
        format!("{}…", clean.chars().take(40).collect::<String>())
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::EventBus;
    use crate::core::types::{Completion, GenerateOptions, Role, StreamEvent};
    use crate::providers::mock::{MockProvider, MockReply, MockRule};
    use crate::providers::provider::Provider;
    use crate::tools::registry::ToolRegistry;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    struct TestProvider {
        mock: MockProvider,
        fail_on: Option<&'static str>,
        cancel_on: Option<(&'static str, Arc<AtomicBool>)>,
        prompts: Mutex<Vec<String>>,
    }

    impl TestProvider {
        fn new(mock: MockProvider) -> Self {
            Self {
                mock,
                fail_on: None,
                cancel_on: None,
                prompts: Mutex::new(Vec::new()),
            }
        }

        fn before(&self, opts: &GenerateOptions) -> anyhow::Result<()> {
            let prompt = opts
                .messages
                .iter()
                .rev()
                .find(|message| message.role != Role::Assistant)
                .map(|message| message.text())
                .unwrap_or_default();
            self.prompts.lock().unwrap().push(prompt.clone());
            if let Some((needle, cancel)) = &self.cancel_on {
                if prompt.contains(*needle) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            if self.fail_on.is_some_and(|needle| prompt.contains(needle)) {
                anyhow::bail!("scripted provider failure");
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Provider for TestProvider {
        fn name(&self) -> &str {
            "workflow-test"
        }

        fn model(&self) -> &str {
            self.mock.model()
        }

        async fn generate(&self, opts: GenerateOptions) -> anyhow::Result<Completion> {
            self.before(&opts)?;
            self.mock.generate(opts).await
        }

        async fn stream(
            &self,
            opts: GenerateOptions,
        ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
            self.before(&opts)?;
            self.mock.stream(opts).await
        }
    }

    fn structured_rule(needle: &str, output: Value) -> MockRule {
        MockRule {
            needle: needle.into(),
            reply: MockReply::ToolCall {
                name: "structured_output".into(),
                input: output,
            },
        }
    }

    fn params(shape: Shape) -> WorkflowParams {
        WorkflowParams {
            shape,
            title: "test".into(),
            items: vec!["a".into(), "b".into(), "c".into()],
            map_prompt: "handle {item}".into(),
            map_schema: None,
            reduce_prompt: Some("combine".into()),
            reduce_schema: None,
            find_prompt: Some("find bugs".into()),
            max_rounds: Some(5),
        }
    }

    fn ctx_with(provider: Arc<dyn Provider>) -> WorkflowContext {
        WorkflowContext::new(
            "wf-test".to_string(),
            provider,
            EventBus::new(),
            ".".to_string(),
            ToolRegistry::new(None),
            None,
            None,
            crate::tools::jobs::JobRegistry::new(),
            Arc::new(AtomicBool::new(false)),
            4,
        )
    }

    #[test]
    fn fill_substitutes_item() {
        assert_eq!(fill("review {item} now", "a.rs"), "review a.rs now");
        assert_eq!(fill("no marker", "x"), "no marker");
    }

    #[test]
    fn label_truncates_long_items() {
        assert_eq!(label("short"), "short");
        let long = "a".repeat(60);
        assert!(label(&long).ends_with('…'));
        assert_eq!(label(&long).chars().count(), 41);
    }

    #[tokio::test]
    async fn map_reduce_runs_map_then_reduce() {
        let provider = MockProvider::new(vec![MockRule {
            needle: "structured_output".to_string(),
            reply: MockReply::ToolCall {
                name: "structured_output".to_string(),
                input: json!({"ok": true}),
            },
        }]);
        let schema = json!({"type": "object", "required": ["ok"]});
        let params = WorkflowParams {
            shape: Shape::MapReduce,
            title: "t".into(),
            items: vec!["a".into(), "b".into(), "c".into()],
            map_prompt: "handle {item}".into(),
            map_schema: Some(schema.clone()),
            reduce_prompt: Some("combine".into()),
            reduce_schema: Some(schema),
            find_prompt: None,
            max_rounds: None,
        };
        let out = run(ctx_with(Arc::new(provider)), params).await;
        assert!(out.error.is_none());
        assert_eq!(
            out.output["results"],
            json!([{"ok": true}, {"ok": true}, {"ok": true}])
        );
        assert_eq!(out.output["reduced"], json!({"ok": true}));
        let outcomes = out.output["outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 3);
        for (outcome, item) in outcomes.iter().zip(["a", "b", "c"]) {
            assert_eq!(outcome["input"], item);
            assert_eq!(outcome["status"], "success");
        }
    }

    #[tokio::test]
    async fn loop_stops_when_a_round_is_dry() {
        // Finder always returns an empty array → the first round is dry, loop ends.
        let provider = MockProvider::new(vec![MockRule {
            needle: "structured_output".to_string(),
            reply: MockReply::ToolCall {
                name: "structured_output".to_string(),
                input: json!({"findings": []}),
            },
        }]);
        let params = WorkflowParams {
            shape: Shape::Loop,
            title: String::new(),
            items: vec![],
            map_prompt: String::new(),
            map_schema: None,
            reduce_prompt: None,
            reduce_schema: None,
            find_prompt: Some("find bugs".into()),
            max_rounds: Some(5),
        };
        let out = run(ctx_with(Arc::new(provider)), params).await;
        assert!(out.error.is_none());
        assert_eq!(out.output["rounds"], json!(1));
        assert_eq!(out.output["findings"], json!([]));
        assert_eq!(out.output["termination_reason"], "dry");
    }

    #[tokio::test]
    async fn mixed_map_failures_preserve_full_inputs_and_skip_reducer() {
        let long = format!("{}\noriginal item", "a".repeat(60));
        for shape in [Shape::FanOut, Shape::MapReduce] {
            let mut provider = TestProvider::new(MockProvider::new(vec![
                MockRule {
                    needle: long.clone(),
                    reply: MockReply::Text("first".into()),
                },
                MockRule {
                    needle: "handle c".into(),
                    reply: MockReply::Text("third".into()),
                },
            ]));
            provider.fail_on = Some("handle b");
            let provider = Arc::new(provider);
            let mut params = params(shape);
            params.items[0] = long.clone();
            let out = run(ctx_with(provider.clone()), params).await;
            assert!(out
                .error
                .as_deref()
                .unwrap()
                .contains("scripted provider failure"));
            assert_eq!(
                out.output["results"],
                json!([{"text": "first"}, null, {"text": "third"}])
            );
            let outcomes = out.output["outcomes"].as_array().unwrap();
            assert_eq!(outcomes.len(), 3);
            for (index, (input, status)) in [
                (long.as_str(), "success"),
                ("b", "failed"),
                ("c", "success"),
            ]
            .into_iter()
            .enumerate()
            {
                assert_eq!(outcomes[index]["input"], input);
                assert_eq!(outcomes[index]["status"], status);
                assert_eq!(outcomes[index]["output"], out.output["results"][index]);
                assert!(!outcomes[index]["id"].as_str().unwrap().is_empty());
            }
            assert_ne!(outcomes[0]["id"], outcomes[1]["id"]);
            assert_ne!(outcomes[1]["id"], outcomes[2]["id"]);
            assert_eq!(outcomes[0]["label"], label(&long));
            assert!(outcomes[1]["error"]
                .as_str()
                .unwrap()
                .contains("scripted provider failure"));
            assert!(out.output.get("reduced").is_none());
            assert!(!provider
                .prompts
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.starts_with("combine")));
        }
    }

    #[tokio::test]
    async fn map_outcomes_record_untruncated_input() {
        let ctx = ctx_with(Arc::new(MockProvider::new(vec![])));
        let item = format!("{}\nsecond line", "x".repeat(60));
        let outcomes = map_items(&ctx, std::slice::from_ref(&item), "handle {item}", &None).await;
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].is_success());
        assert_eq!(outcomes[0].input, Some(json!(item)));
        assert_eq!(outcomes[0].label, label(&item));
    }

    #[tokio::test]
    async fn reduce_failure_retains_map_results() {
        let mut provider = TestProvider::new(MockProvider::new(vec![]));
        provider.fail_on = Some("combine");
        let out = run(ctx_with(Arc::new(provider)), params(Shape::MapReduce)).await;
        assert!(out.error.is_some());
        assert_eq!(
            out.output["results"],
            json!([{"text": "ok"}, {"text": "ok"}, {"text": "ok"}])
        );
        assert_eq!(out.output["reduced"], Value::Null);
        let outcomes = out.output["outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 3);
        for (outcome, input) in outcomes.iter().zip(["a", "b", "c"]) {
            assert_eq!(outcome["input"], input);
            assert_eq!(outcome["status"], "success");
        }
    }

    #[tokio::test]
    async fn cancelled_map_preserves_slots_and_skips_reducer() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let ctx = ctx_with(provider.clone());
        ctx.cancel.store(true, Ordering::Relaxed);
        let out = run(ctx, params(Shape::MapReduce)).await;
        assert!(out.error.is_some());
        assert_eq!(out.output["results"], json!([null, null, null]));
        let outcomes = out.output["outcomes"].as_array().unwrap();
        assert_eq!(outcomes.len(), 3);
        for (outcome, input) in outcomes.iter().zip(["a", "b", "c"]) {
            assert_eq!(outcome["input"], input);
            assert_eq!(outcome["status"], "cancelled");
        }
        assert!(out.output.get("reduced").is_none());
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn failed_finder_retains_partial_findings_and_counts_failed_round() {
        let mut provider = TestProvider::new(MockProvider::new(vec![structured_rule(
            "nothing yet",
            json!({"findings": ["first"]}),
        )]));
        provider.fail_on = Some("- first");
        let out = run(ctx_with(Arc::new(provider)), params(Shape::Loop)).await;
        assert!(out
            .error
            .as_deref()
            .unwrap()
            .contains("scripted provider failure"));
        assert_eq!(out.output["findings"], json!(["first"]));
        assert_eq!(out.output["rounds"], 2);
        assert_eq!(out.output["termination_reason"], "failed");
    }

    #[tokio::test]
    async fn malformed_finder_is_not_a_dry_round_or_partial_success() {
        let provider = MockProvider::new(vec![
            structured_rule("nothing yet", json!({"findings": ["first"]})),
            structured_rule("- first", json!({"findings": ["discard this", 42]})),
        ]);
        let out = run(ctx_with(Arc::new(provider)), params(Shape::Loop)).await;
        assert!(out.error.as_deref().unwrap().contains("findings[1]"));
        assert_eq!(out.output["findings"], json!(["first"]));
        assert_eq!(out.output["rounds"], 2);
        assert_eq!(out.output["termination_reason"], "invalid_output");
    }

    #[test]
    fn finder_requires_an_array_and_every_entry_must_be_a_string() {
        for output in [
            None,
            Some(json!({})),
            Some(json!({"findings": "bad"})),
            Some(json!({"findings": ["ok", null]})),
        ] {
            assert!(strings(&output, "findings").is_err());
        }
        assert_eq!(
            strings(&Some(json!({"findings": []})), "findings"),
            Ok(vec![])
        );
    }

    #[tokio::test]
    async fn repeated_findings_are_dry_and_deduplicated() {
        let provider = MockProvider::new(vec![structured_rule(
            "structured_output",
            json!({"findings": ["first", "first"]}),
        )]);
        let out = run(ctx_with(Arc::new(provider)), params(Shape::Loop)).await;
        assert!(out.error.is_none());
        assert_eq!(out.output["findings"], json!(["first"]));
        assert_eq!(out.output["rounds"], 2);
        assert_eq!(out.output["termination_reason"], "dry");
    }

    #[tokio::test]
    async fn round_limit_is_distinguished_from_a_dry_round() {
        let provider = MockProvider::new(vec![
            structured_rule("nothing yet", json!({"findings": ["first"]})),
            structured_rule("- first", json!({"findings": ["second"]})),
        ]);
        let mut params = params(Shape::Loop);
        params.max_rounds = Some(2);
        let out = run(ctx_with(Arc::new(provider)), params).await;
        assert!(out.error.is_none());
        assert_eq!(out.output["findings"], json!(["first", "second"]));
        assert_eq!(out.output["rounds"], 2);
        assert_eq!(out.output["termination_reason"], "max_rounds");
    }

    #[tokio::test]
    async fn cancelled_loop_does_not_start_a_round() {
        let provider = Arc::new(MockProvider::new(vec![]));
        let ctx = ctx_with(provider.clone());
        ctx.cancel.store(true, Ordering::Relaxed);
        let out = run(ctx, params(Shape::Loop)).await;
        assert!(out.error.is_some());
        assert_eq!(out.output["findings"], json!([]));
        assert_eq!(out.output["rounds"], 0);
        assert_eq!(out.output["termination_reason"], "cancelled");
        assert_eq!(provider.call_count(), 0);
    }

    #[tokio::test]
    async fn cancelled_finder_retains_completed_rounds() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut provider = TestProvider::new(MockProvider::new(vec![
            structured_rule("nothing yet", json!({"findings": ["first"]})),
            structured_rule("- first", json!({"findings": []})),
        ]));
        provider.cancel_on = Some(("- first", cancel.clone()));
        let mut ctx = ctx_with(Arc::new(provider));
        ctx.cancel = cancel;
        let out = run(ctx, params(Shape::Loop)).await;
        assert!(out.error.is_some());
        assert_eq!(out.output["findings"], json!(["first"]));
        assert_eq!(out.output["rounds"], 2);
        assert_eq!(out.output["termination_reason"], "cancelled");
    }
}
