pub(crate) mod dsl;
mod input;
mod params;
mod runtime;
pub(crate) mod schema;
mod validation;

#[cfg(test)]
mod primitive_tests;
#[cfg(test)]
mod runtime_tests;

use crate::{EffectClass, ToolDescriptor};
pub(crate) use runtime::{cleanup, execute, fail, SCHEMA};

pub(crate) fn tools() -> Vec<ToolDescriptor> {
    vec![ToolDescriptor {
        spec: bob_types::ToolSpec {
            name: "workflow".into(),
            description: GUIDANCE.into(),
            input_schema: input::schema(),
        },
        effect: EffectClass::Read,
    }]
}

const GUIDANCE: &str = r#"Run a durable structured pipeline with steps (agent, fan_out, parallel, loop, conditional) or shape (fan_out, map_reduce, loop). Use direct tools for small tasks; use workflows for repeated structured processing or explicit stage dependencies.

Each child starts with a fresh context, not the parent's transcript or another child's conversation. Give self-contained instructions, paths, constraints, and explicit inputs. References use $step.field; prompts interpolate {$step.field} and fan-out {item} or {item.field}. A child without an output schema returns text, not parsed JSON; supply a schema when later steps need fields or booleans. Schemas validate actual output; they do not repair it or retry effects. Children cannot delegate. Normal edit/shell tools remain available subject to permissions; read_only is opt-in.

Children share the workspace: assign non-overlapping file ownership, avoid speculative parallel writers, and designate one owner for workspace-wide verification after edits settle. Optional max_concurrency (integer 1..64, default 64) is one active-child budget across the entire workflow, still subject to global/session limits. Waiting for guidance and unsettled stopping work occupy slots; the limit is not workspace isolation. Plans allow 256 operations, 16 nesting levels, 256 fan-out items, repeat/loop caps 1..20, and at most 1024 admitted children. Both conditional branches count toward static limits; only admitted children consume execution capacity.

Conditional if must resolve to an actual boolean, never a truthy string or expression. Both branches are structurally checked with scoped references; dynamic inputs are checked only in the selected branch. The choice is durably retained before admitting children and is never reevaluated after guidance or recovery. Missing else is a no-op false branch. Result is {branch: "then"|"else", output: {chosen step results}}; reference it as $decision.output.step.field. Unselected branch outputs do not exist.

Discovery, bounded edits, then single-owner verification:
{"max_concurrency":2,"steps":[{"id":"discover","agent":{"prompt":"Identify independent files needing the requested fix. Return each owned file once; do not edit.","schema":{"type":"object","required":["files"],"properties":{"files":{"type":"array","items":{"type":"string"},"maxItems":256,"uniqueItems":true}}}}},{"id":"edit","fan_out":{"over":"$discover.files","prompt":"Implement the requested fix only in {item}. Own only that file; do not change other files or run workspace-wide checks."}},{"id":"verify","agent":{"prompt":"You are the sole workspace-wide verification owner. All editors settled. Run the relevant checks and report failures. Editor reports: {$edit}"}}]}
Replace generic task descriptions with the actual requirements and repository paths before submitting.

Parallel read-only review:
{"read_only":true,"max_concurrency":2,"steps":[{"id":"review","parallel":{"branches":{"correctness":{"agent":{"prompt":"Review the specified diff for concrete correctness regressions. Do not edit."}},"security":{"agent":{"prompt":"Review the specified diff for security regressions. Do not edit."}}}}}]}

Conditional repair:
{"max_concurrency":1,"steps":[{"id":"check","agent":{"prompt":"Check the requested behavior; return needs_repair and evidence.","schema":{"type":"object","required":["needs_repair","evidence"],"properties":{"needs_repair":{"type":"boolean"},"evidence":{"type":"string"}}}}},{"id":"decision","conditional":{"if":"$check.needs_repair","then":[{"id":"repair","agent":{"prompt":"Own the affected files exclusively and repair this confirmed problem: {$check.evidence}"}}]}}]}

Bounded fix/check loop:
{"max_concurrency":1,"steps":[{"id":"cycle","loop":{"max":3,"steps":[{"id":"fix","agent":{"prompt":"Inspect current files, reproduce remaining failures, and fix only the assigned area. You own these edits; no other writer runs concurrently."}},{"id":"check","agent":{"prompt":"You alone own workspace verification. Check the current files after this fix: {$fix}. Return ok only when all required checks pass.","schema":{"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}}}}],"until":"$check.ok"}}]}
Each loop pass starts from its enclosing scope, not prior-pass results. Inspect current files rather than assuming implicit diagnostic carry-state. Pipeline loops report _iterations and _stop_reason (condition_met, max_rounds or failed); finder shapes report termination_reason (dry, max_rounds or failed). Reaching a cap is not proof the goal was achieved.

Failed siblings and partial outcomes remain inspectable, but dependents do not run after failure; no automatic partial-result synthesis or schema repair is performed. Cancellation retains results. Unknown/interrupted effects are never automatically replayed. A yielded tool result may precede the final durable workflow result; use the workflow/job inspection tools to inspect retained outcomes."#;
