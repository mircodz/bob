use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    FanOut,
    MapReduce,
    Loop,
}

/// Only fields relevant to the chosen shape are read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowParams {
    pub shape: Shape,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<usize>,
    /// Required for fan-out and map-reduce.
    #[serde(default)]
    pub items: Vec<String>,
    /// Per-item prompt (fan_out / map_reduce). `{item}` is substituted.
    #[serde(default)]
    pub map_prompt: String,
    #[serde(default)]
    pub map_schema: Option<Value>,
    /// map_reduce: how to synthesize the collected results (appended as JSON).
    #[serde(default)]
    pub reduce_prompt: Option<String>,
    #[serde(default)]
    pub reduce_schema: Option<Value>,
    /// Finder output is `{findings: [..]}`; the seen list is appended each round.
    #[serde(default)]
    pub find_prompt: Option<String>,
    #[serde(default)]
    pub max_rounds: Option<usize>,
}

pub(crate) fn validate(params: &WorkflowParams) -> Result<(), String> {
    super::validation::validate_concurrency(params.max_concurrency)?;
    match params.shape {
        Shape::FanOut | Shape::MapReduce => {
            if params.items.is_empty() || params.items.len() > super::validation::MAX_ITEMS {
                return Err(format!(
                    "workflow requires 1..={} items",
                    super::validation::MAX_ITEMS
                ));
            }
            super::validation::validate_agent(&params.map_prompt, params.map_schema.as_ref())?;
            if params.shape == Shape::MapReduce {
                super::validation::validate_agent(
                    params
                        .reduce_prompt
                        .as_deref()
                        .unwrap_or("Synthesize these results"),
                    params.reduce_schema.as_ref(),
                )?;
            }
        }
        Shape::Loop => {
            super::validation::validate_count(params.max_rounds.unwrap_or(5))?;
            super::validation::validate_agent(
                params.find_prompt.as_deref().unwrap_or("Find new items"),
                None,
            )?;
        }
    }
    Ok(())
}
