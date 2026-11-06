//! Declarative workflows with scoped `$id.field` references between steps.

use super::validation::{placeholders, reference_parts};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
}

/// Stores its result under `id` for subsequent steps in the same scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    #[serde(flatten)]
    pub op: Op,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Agent(AgentOp),
    FanOut(FanOutOp),
    /// Results are keyed by branch name.
    Parallel(ParallelOp),
    /// Exposes inner results through the loop step's output.
    Loop(LoopOp),
    Conditional(ConditionalOp),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentOp {
    pub prompt: String,
    #[serde(default)]
    pub schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParallelOp {
    /// Named branches run concurrently; each is itself a single-step op.
    pub branches: BTreeMap<String, Op>,
}

/// Each iteration starts from the enclosing scope, without stale prior results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopOp {
    pub steps: Vec<Step>,
    /// A reference to a boolean. Omit to always run `max` iterations.
    #[serde(default)]
    pub until: Option<String>,
    /// Iteration cap (default 5, accepted range 1..=20).
    #[serde(default)]
    pub max: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConditionalOp {
    #[serde(rename = "if")]
    pub condition: String,
    pub then: Vec<Step>,
    #[serde(default, rename = "else", skip_serializing_if = "Option::is_none")]
    pub otherwise: Option<Vec<Step>>,
}

pub(crate) fn preflight(op: &Op, scope: &Map<String, Value>) -> Result<(), String> {
    match op {
        // Only the runner can consult the durable choice before inspecting branch inputs.
        Op::Conditional(_) => Ok(()),
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

pub(crate) fn preflight_prompt(prompt: &str, scope: &Map<String, Value>) -> Result<(), String> {
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

pub(crate) fn resolve_ref<'a>(
    reference: &str,
    scope: &'a Map<String, Value>,
) -> Result<&'a Value, String> {
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

pub(crate) fn interpolate(
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
