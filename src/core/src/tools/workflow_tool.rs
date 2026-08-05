//! The `workflow` tool: lets the MODEL compose a multi-agent workflow on the fly by
//! choosing a shape (`fan_out` / `map_reduce`) and filling its blanks (the item
//! list, per-item prompt, optional reduce prompt + schemas). The engine runs the
//! canned control-flow; each child is a normal subagent whose tool calls are still
//! permission-gated. The result returns as this tool's result, so it feeds straight
//! back into the model's context — no separate hand-off turn needed.
//!
//! This is the safe, no-sandbox path to the "dynamic workflows" idea: the model
//! parameterizes a known-good harness rather than writing arbitrary code.

use crate::core::events::EventBus;
use crate::core::types::ToolSpec;
use crate::providers::provider::Provider;
use crate::tools::jobs::JobRegistry;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolRegistry, ToolResult};
use crate::workflow::params::{self, WorkflowParams};
use crate::workflow::WorkflowContext;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Monotonic counter so each `workflow` tool run gets a distinct render id.
static WORKFLOW_RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The `workflow` tool. Holds the same subagent-building deps as `TaskTool` so it
/// can spin up a `WorkflowContext` and run a parameterized workflow.
pub struct WorkflowTool {
    pub provider: Arc<dyn Provider>,
    pub subagent_tools: ToolRegistry,
    pub bus: EventBus,
    pub cwd: String,
    pub subagent_system: Option<String>,
    pub jobs: JobRegistry,
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    /// The root's cancel flag, so a Cancel cascades into the workflow's agents.
    pub parent_cancel: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Tool for WorkflowTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "workflow".to_string(),
            description: "Run a multi-agent workflow on the fly for a task that is massively \
                parallel, highly structured, adversarial, or benefits from isolated context per \
                item — where doing it in your own context window would risk stopping early \
                (laziness), preferring your own output (bias), or losing the goal (drift). Two \
                forms:\n\
                \n\
                (A) SHAPE — one canned pattern:\n\
                • fan_out: one fresh subagent per item, return all results (\"do X to each of \
                these N things\").\n\
                • map_reduce: fan_out, then one reduce agent synthesizes (classify-and-act / \
                generate-and-filter — put the routing/rubric in reduce_prompt).\n\
                • loop: re-run a finder each round until a round turns up nothing new (set \
                find_prompt).\n\
                \n\
                (B) STEPS — a declarative multi-step pipeline (use for anything the shapes can't \
                express: several stages, data flowing between them, or a conditional loop). \
                Provide `steps`: an ordered list of {id, <op>}. Each op is ONE of:\n\
                • agent:   {prompt, schema?}                       — one subagent.\n\
                • fan_out: {over, prompt, schema?, repeat?}         — one subagent per item in \
                `over`; repeat:N runs N per item (e.g. 2 adversarial reviews each).\n\
                • parallel:{branches:{name:<op>, …}}               — run branches concurrently; \
                result is an object keyed by branch name.\n\
                • loop:    {steps, until?, max?}                    — re-run inner steps until \
                `until` is truthy (or `max`); loops nest.\n\
                DATA FLOW: reference a prior step's output with `$id` or `$id.field` — in a \
                prompt (`{$cov.pct}`), or as a fan_out `over` (`\"over\": \"$crates.list\"`). \
                Inside fan_out use `{item}` / `{item.field}`. Force structured output with \
                `schema` (JSON Schema) so refs resolve to data, not prose.\n\
                \n\
                Example (list crates → gen tests each → 2 adversarial reviews each → fix → loop \
                until coverage passes):\n\
                {\"steps\":[\
                {\"id\":\"crates\",\"agent\":{\"prompt\":\"list all crates\",\"schema\":{\"type\":\"object\",\"required\":[\"list\"]}}},\
                {\"id\":\"improve\",\"loop\":{\"until\":\"$cov.pass\",\"max\":3,\"steps\":[\
                {\"id\":\"tests\",\"fan_out\":{\"over\":\"$crates.list\",\"prompt\":\"write unit tests for {item}\",\"schema\":{...}}},\
                {\"id\":\"reviews\",\"fan_out\":{\"over\":\"$tests\",\"repeat\":2,\"prompt\":\"adversarially review {item}\",\"schema\":{...}}},\
                {\"id\":\"fix\",\"fan_out\":{\"over\":\"$reviews\",\"prompt\":\"address: {item}\"}},\
                {\"id\":\"cov\",\"agent\":{\"prompt\":\"does coverage pass?\",\"schema\":{\"type\":\"object\",\"required\":[\"pass\"]}}}]}}]}\n\
                \n\
                Each subagent starts BLANK — put everything it needs in the prompt. The result \
                returns here; summarize it and act on it. Prefer `task` for a couple of quick \
                lookups; use `workflow` for real scale, adversarial checks, or multi-stage work."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short label for the run (shown in the workflow view), e.g. \"review-pr\"." },
                    "steps": {
                        "type": "array",
                        "description": "Form B: a declarative multi-step pipeline. Each entry is {id, <op>} where op is agent/fan_out/parallel/loop. Reference prior outputs with $id / $id.field. See the tool description for the grammar + example.",
                        "items": { "type": "object" }
                    },
                    "shape": {
                        "type": "string",
                        "enum": ["fan_out", "map_reduce", "loop"],
                        "description": "Form A: a single canned shape. fan_out=one agent per item; map_reduce=+reduce; loop=until-dry."
                    },
                    "items": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "shape fan_out/map_reduce: the things to operate on — one subagent per entry."
                    },
                    "map_prompt": { "type": "string", "description": "shape fan_out/map_reduce: per-item instructions. Use {item}." },
                    "map_schema": { "type": "object", "description": "shape: JSON Schema forcing each map agent's structured output." },
                    "reduce_prompt": { "type": "string", "description": "shape map_reduce: how to synthesize the collected results." },
                    "reduce_schema": { "type": "object", "description": "shape map_reduce: JSON Schema for the reduce agent." },
                    "find_prompt": { "type": "string", "description": "shape loop: the finder prompt run each round." },
                    "max_rounds": { "type": "integer", "description": "shape loop: safety cap on rounds (default 5)." }
                }
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        use crate::workflow::params::Shape;

        let cwd = if self.cwd.is_empty() {
            ctx.cwd.clone()
        } else {
            self.cwd.clone()
        };
        // A distinct, recognizable id so the TUI groups this run's events into one
        // workflow tree (the "wf-" prefix is how the view detects workflow agents).
        let n = WORKFLOW_RUN_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;

        // Two forms: a declarative multi-step `steps` spec (the DSL), or a single
        // `shape`. `steps` wins when present.
        let has_steps = input.get("steps").is_some();
        let slug = input
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if has_steps {
                    "steps".into()
                } else {
                    "shape".into()
                }
            });
        let run_id = format!("wf-{slug}-{n}");

        let make_ctx = || {
            WorkflowContext::new(
                run_id.clone(),
                self.provider.clone(),
                self.bus.clone(),
                cwd.clone(),
                self.subagent_tools.clone(),
                self.subagent_system.clone(),
                self.lsp.clone(),
                self.jobs.clone(),
                self.parent_cancel.clone(),
                0, // default concurrency cap
            )
        };

        let result = if has_steps {
            let spec: crate::workflow::dsl::Spec = serde_json::from_value(input)
                .map_err(|e| ToolError::invalid_input(format!("invalid workflow steps: {e}")))?;
            crate::workflow::dsl::run(make_ctx(), spec).await
        } else {
            let params: WorkflowParams = serde_json::from_value(input)
                .map_err(|e| ToolError::invalid_input(format!("invalid workflow params: {e}")))?;
            // Item-based shapes need a non-empty item list; `loop` drives itself.
            if params.shape != Shape::Loop && params.items.is_empty() {
                return Err(ToolError::invalid_input(
                    "items must not be empty for this shape",
                ));
            }
            params::run(make_ctx(), params).await
        };

        // Return the structured result as pretty JSON so it re-enters the model's
        // context as usable data.
        Ok(serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()))
    }
}
