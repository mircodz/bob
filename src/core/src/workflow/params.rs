//! Parameterized workflows: the model picks a *shape* and fills its blanks for the
//! CURRENT task (the item list, the per-item prompt, schemas, a find prompt). The
//! engine runs a canned control-flow around them. This is the safe, no-sandbox path
//! to "use pattern X on this task": every child is a normal subagent whose tool
//! calls are still permission-gated.
//!
//! The shapes mirror the canonical workflow patterns the model can't express with
//! plain parameters otherwise:
//!   - `fan_out`    : one agent per item, return all results.
//!   - `map_reduce` : fan_out, then one reduce agent (fanout-and-synthesize;
//!                    classify-and-act and generate-and-filter are special cases).
//!   - `loop`       : re-run a finder each round until a round turns up nothing new
//!                    (loop-until-done — the model can't express cross-round control
//!                    flow, so it lives here).
//!
//! `{item}` in a prompt is substituted with each item's text.

use super::{agent, parallel, AgentSpec, WorkflowContext};
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
pub async fn run(ctx: WorkflowContext, params: WorkflowParams) -> Value {
    match params.shape {
        Shape::FanOut | Shape::MapReduce => map_reduce(ctx, params).await,
        Shape::Loop => loop_until_done(ctx, params).await,
    }
}

/// fan_out / map_reduce: one agent per item, optionally followed by a reduce agent.
async fn map_reduce(ctx: WorkflowContext, params: WorkflowParams) -> Value {
    let is_reduce = params.shape == Shape::MapReduce;
    ctx.phase("Map", 0, if is_reduce { 2 } else { 1 });
    let results = map_items(&ctx, &params.items, &params.map_prompt, &params.map_schema).await;

    if !is_reduce {
        return json!({ "results": results });
    }

    ctx.phase("Reduce", 1, 2);
    let reduce_prompt = params
        .reduce_prompt
        .clone()
        .unwrap_or_else(|| "Synthesize these results into a single summary.".to_string());
    let results_json = serde_json::to_string_pretty(&results).unwrap_or_else(|_| "[]".to_string());
    let full = format!("{reduce_prompt}\n\nResults:\n{results_json}");
    let mut spec = AgentSpec::new(full, "reduce");
    if let Some(s) = params.reduce_schema.clone() {
        spec = spec.with_schema(s);
    }
    let reduced = agent(&ctx, spec).await;
    json!({ "results": results, "reduced": reduced })
}

/// loop: re-run the finder each round until a round surfaces nothing new (or the
/// round cap is hit), deduping against everything already found.
async fn loop_until_done(ctx: WorkflowContext, params: WorkflowParams) -> Value {
    let find_prompt = params.find_prompt.clone().unwrap_or_else(|| {
        "Find items not already in the seen list; return {findings:[..]}, empty if none new."
            .to_string()
    });
    let max_rounds = params.max_rounds.unwrap_or(5).clamp(1, 20);
    let mut seen: Vec<String> = Vec::new();
    let mut round = 0;
    let mut dry = false;
    while !dry && round < max_rounds {
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
        let fresh: Vec<String> = strings(&found, "findings")
            .into_iter()
            .filter(|f| !seen.contains(f))
            .collect();
        if fresh.is_empty() {
            dry = true;
        } else {
            seen.extend(fresh);
        }
        round += 1;
    }
    json!({ "findings": seen, "rounds": round })
}

// --- shared helpers --------------------------------------------------------

/// Run one agent per item concurrently, returning the (non-null) results.
async fn map_items(
    ctx: &WorkflowContext,
    items: &[String],
    map_prompt: &str,
    map_schema: &Option<Value>,
) -> Vec<Value> {
    let thunks: Vec<_> = items
        .iter()
        .map(|item| {
            let ctx = ctx.clone();
            let prompt = fill(map_prompt, item);
            let mut spec = AgentSpec::new(prompt, label(item));
            if let Some(s) = map_schema.clone() {
                spec = spec.with_schema(s);
            }
            async move { agent(&ctx, spec).await }
        })
        .collect();
    parallel(thunks).await.into_iter().flatten().collect()
}

fn list_schema(key: &str) -> Value {
    json!({
        "type": "object",
        "required": [key],
        "properties": { key: { "type": "array", "items": { "type": "string" } } }
    })
}

fn strings(v: &Option<Value>, key: &str) -> Vec<String> {
    v.as_ref()
        .and_then(|v| v.get(key))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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
    use crate::providers::mock::{MockProvider, MockReply, MockRule};
    use crate::providers::provider::Provider;
    use crate::tools::registry::ToolRegistry;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

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
        assert_eq!(out["results"].as_array().unwrap().len(), 3);
        assert_eq!(out["reduced"], json!({"ok": true}));
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
        assert_eq!(out["rounds"], json!(1));
        assert_eq!(out["findings"].as_array().unwrap().len(), 0);
    }
}
