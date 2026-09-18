//! Model-authored workflows use the existing permission-gated subagent engine.
//! Validate the complete plan before starting work, then return data and an
//! explicit execution report without hiding unsuccessful agents.

use crate::agent::env::AgentEnv;
use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use crate::workflow::params::{self, WorkflowParams};
use crate::workflow::{dsl, input, validation, WorkflowContext};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static WORKFLOW_RUN_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct WorkflowTool {
    pub env: AgentEnv,
}

#[async_trait]
impl Tool for WorkflowTool {
    fn spec(&self) -> ToolSpec {
        let examples = json!([
            {
                "title": "review-crates", "read_only": true,
                "shape": "map_reduce", "items": ["src/cli", "src/core"],
                "map_prompt": "Review only {item} for error-handling bugs. Report concrete evidence.",
                "reduce_prompt": "Summarize the verified findings without overstating coverage."
            },
            {
                "title": "fix-error-handling",
                "steps": [
                    {"id": "files", "agent": {
                        "prompt": "Find Rust files with swallowed I/O errors. Return their paths.",
                        "schema": {"type": "object", "required": ["paths"], "properties": {
                            "paths": {"type": "array", "items": {"type": "string"}}
                        }}
                    }},
                    {"id": "fixes", "fan_out": {
                        "over": "$files.paths",
                        "prompt": "Fix swallowed I/O errors in {item}. Edit only that file."
                    }},
                    {"id": "summary", "agent": {"prompt": "Summarize these changes: {$fixes}"}}
                ]
            },
            {
                "title": "repair-tests",
                "steps": [{"id": "repair", "loop": {
                    "max": 3, "until": "$check.pass",
                    "steps": [
                        {"id": "fix", "agent": {"prompt": "Fix failing unit tests in src/cli, editing only src/cli."}},
                        {"id": "check", "agent": {
                            "prompt": "Run cargo test -p bob-cli. Return whether the command passed, not a prediction.",
                            "schema": {"type": "object", "required": ["pass"], "properties": {"pass": {"type": "boolean"}}}
                        }}
                    ]
                }}]
            }
        ]);
        ToolSpec {
            name: "workflow".into(),
            description: format!(
                "Run repeated structured work or an agent pipeline with explicit data dependencies. \
                Prefer direct tools for ordinary coding, explore for investigation, and task for \
                independent deliverables; complexity alone does not require a workflow. \
                Supply exactly one form: shape (fan_out/map_reduce/loop) or steps. \
                The input schema defines every operation and nesting rule.\n\
                Step data stays unwrapped: reference a prior output with $id or $id.field, \
                or interpolate {{$id.field}} in prompts. Fan-out over must resolve to an array; \
                use {{item}} or {{item.field.nested}} in its prompt. Array indexing is not supported. \
                Loop until must reference a boolean. Inner results are available afterward as \
                $loop_id.inner_id.field; parallel branches cannot reference one another. \
                Step names must be unique within their scope and cannot shadow enclosing names. \
                Missing references and incorrect runtime types fail explicitly. Loops default to \
                5 rounds and accept 1..20; fan-out repeat defaults to 1 and accepts 1..20.\n\
                Workflows retain normal edit and shell tools, subject to session permissions. \
                Set read_only:true only when the entire run should use read-only tools; default false. \
                Children do not inherit your conversation: supply facts, boundaries, and deliverables. \
                Assign non-overlapping write ownership and one owner for workspace-wide verification.\n\
                Results contain status, output, agents, and error. Agent records include input, \
                status (success/failed/cancelled), output, and error; no failed item is dropped. \
                Failed batches skip dependent steps and synthesis. Incomplete runs return a tool \
                error retaining their partial output and agent reports. A failed finder is not an \
                empty finding set. Loop termination distinguishes the condition being met from \
                reaching the limit; reaching the limit does not mean the goal passed.\nExamples:\n{examples}"
            ),
            input_schema: input::schema(),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        input::validate(&input).map_err(ToolError::invalid_input)?;
        enum Plan {
            Steps(dsl::Spec),
            Shape(WorkflowParams),
        }
        let plan = if input.get("steps").is_some() {
            let spec: dsl::Spec = serde_json::from_value(input.clone())
                .map_err(|error| ToolError::invalid_input(format!("invalid steps: {error}")))?;
            validation::validate(&spec).map_err(ToolError::invalid_input)?;
            Plan::Steps(spec)
        } else {
            Plan::Shape(
                serde_json::from_value(input.clone())
                    .map_err(|error| ToolError::invalid_input(format!("invalid shape: {error}")))?,
            )
        };
        let read_only = input["read_only"].as_bool().unwrap_or(false);
        let (system, tools) = self
            .env
            .resolve_child(None, read_only)
            .map_err(ToolError::invalid_input)?;
        let cwd = if self.env.cwd.is_empty() {
            ctx.cwd.clone()
        } else {
            self.env.cwd.clone()
        };
        let slug = input["title"]
            .as_str()
            .filter(|title| !title.is_empty())
            .unwrap_or("workflow");
        let n = WORKFLOW_RUN_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
        let workflow = WorkflowContext::new(
            format!("wf-{slug}-{n}"),
            self.env.provider.clone(),
            self.env.bus.clone(),
            cwd,
            tools,
            system,
            self.env.lsp.clone(),
            self.env.jobs.clone(),
            self.env.parent_cancel.clone(),
            0,
        )
        .with_team(self.env.team.clone());
        let mut run = match plan {
            Plan::Steps(spec) => dsl::run(workflow.clone(), spec).await,
            Plan::Shape(params) => params::run(workflow.clone(), params).await,
        };
        let status = if workflow.is_cancelled() {
            run.error.get_or_insert_with(|| "workflow cancelled".into());
            "cancelled"
        } else if run.error.is_some() {
            "failed"
        } else {
            "completed"
        };
        let report = json!({
            "status": status,
            "output": run.output,
            "agents": workflow.outcomes(),
            "error": run.error,
        });
        let text = serde_json::to_string_pretty(&report).expect("workflow report is JSON");
        if status == "completed" {
            Ok(text)
        } else {
            Err(ToolError::failed(format!("workflow incomplete: {text}")))
        }
    }
}

#[cfg(test)]
mod tests;
