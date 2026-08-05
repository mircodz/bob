//! The `structured_output` tool: the mechanism that turns a subagent's free-form
//! run into a single schema-validated JSON object. Workflow orchestration code
//! needs *data* it can branch on (filter, map, loop-until-count), not prose — so
//! `Agent::run_structured` hands the model a one-off tool whose `input_schema` IS
//! the caller's schema and instructs it to finish by calling it. The tool captures
//! the (validated) input into a shared sink the caller reads once the run ends.
//!
//! Validation is intentionally lightweight — top-level `required` keys + a coarse
//! JSON type check — enough to force a well-shaped object and drive a retry without
//! pulling in a full JSON-Schema dependency. It can be upgraded later behind the
//! same interface.

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// A shared slot the tool writes the captured, validated object into. `None` until
/// the model calls the tool with a conforming payload.
pub type OutputSink = Arc<Mutex<Option<Value>>>;

/// A tool that records one schema-conforming JSON object. Built per
/// `run_structured` call with the caller's schema, so its `input_schema` shown to
/// the model IS exactly what the caller wants back.
pub struct StructuredOutputTool {
    schema: Value,
    sink: OutputSink,
}

impl StructuredOutputTool {
    pub fn new(schema: Value, sink: OutputSink) -> Self {
        StructuredOutputTool { schema, sink }
    }
}

#[async_trait]
impl Tool for StructuredOutputTool {
    /// Read-only: recording the result mutates no workspace state, so it can run in
    /// the loop's concurrent lane without ordering concerns.
    fn is_read_only(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "structured_output".to_string(),
            description: "Record your FINAL answer as a single JSON object matching this \
                tool's schema. Call this exactly once, as your last action, when you have \
                everything needed — its arguments ARE the deliverable. Do not wrap it in prose."
                .to_string(),
            input_schema: self.schema.clone(),
        }
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        if let Err(problem) = validate_against(&self.schema, &input) {
            // A typed error the loop feeds back so the model can correct and retry.
            return Err(ToolError::invalid_input(format!(
                "output does not match the schema: {problem}. Re-call structured_output with a \
                 corrected object."
            )));
        }
        *self.sink.lock().unwrap() = Some(input);
        Ok("recorded".to_string())
    }
}

/// Coarse structural validation: the value is the right top-level JSON type and,
/// for objects, carries every `required` key with a roughly-correct type. Returns
/// `Ok(())` when acceptable, or `Err(reason)` naming the first problem.
fn validate_against(schema: &Value, value: &Value) -> Result<(), String> {
    let expected = schema.get("type").and_then(Value::as_str);
    if let Some(t) = expected {
        if !type_matches(t, value) {
            return Err(format!("expected top-level type '{t}'"));
        }
    }
    // For objects, enforce required keys and each property's declared type.
    if expected == Some("object") {
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if value.get(key).is_none() {
                    return Err(format!("missing required key '{key}'"));
                }
            }
        }
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            for (key, pschema) in props {
                if let (Some(v), Some(t)) =
                    (value.get(key), pschema.get("type").and_then(Value::as_str))
                {
                    if !type_matches(t, v) {
                        return Err(format!("key '{key}' should be type '{t}'"));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Whether a JSON value satisfies a JSON-Schema primitive type name. `integer`
/// accepts any JSON number (the model may emit `2.0`); `null` is permissive.
fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "number" | "integer" => v.is_number(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        _ => true, // unknown/omitted → don't reject
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_conforming_object() {
        let schema = json!({
            "type": "object",
            "required": ["title", "count"],
            "properties": { "title": {"type": "string"}, "count": {"type": "integer"} }
        });
        assert!(validate_against(&schema, &json!({"title": "x", "count": 3})).is_ok());
    }

    #[test]
    fn rejects_missing_required_key() {
        let schema = json!({ "type": "object", "required": ["title"] });
        let err = validate_against(&schema, &json!({"other": 1})).unwrap_err();
        assert!(err.contains("title"));
    }

    #[test]
    fn rejects_wrong_property_type() {
        let schema = json!({
            "type": "object",
            "properties": { "count": {"type": "integer"} }
        });
        assert!(validate_against(&schema, &json!({"count": "nope"})).is_err());
    }

    #[test]
    fn integer_accepts_json_float() {
        let schema = json!({ "type": "object", "properties": { "n": {"type": "integer"} } });
        assert!(validate_against(&schema, &json!({"n": 2.0})).is_ok());
    }
}
