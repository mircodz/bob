//! Captures schema-validated subagent output for workflow orchestration.

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Latest validated output; invalid submissions clear the slot.
pub type OutputSink = Arc<Mutex<Option<Value>>>;

pub struct StructuredOutputTool {
    schema: Value,
    sink: OutputSink,
    validator: Result<crate::tools::schema::CompiledSchema, String>,
}

impl StructuredOutputTool {
    pub fn new(schema: Value, sink: OutputSink) -> Self {
        let validator = crate::tools::schema::CompiledSchema::compile(&schema);
        StructuredOutputTool {
            schema,
            sink,
            validator,
        }
    }
}

#[async_trait]
impl Tool for StructuredOutputTool {
    /// Only the per-run output sink is mutated, not workspace state.
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
        let validation = self
            .validator
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|validator| validator.validate(&input));
        if let Err(problem) = validation {
            *self.sink.lock().unwrap() = None;
            return Err(ToolError::invalid_input(format!(
                "output does not match the schema: {problem}. Re-call structured_output with a \
                 corrected object."
            )));
        }
        *self.sink.lock().unwrap() = Some(input);
        Ok("recorded".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files::FileFixture;
    use crate::tools::registry::ToolErrorKind;
    use serde_json::json;

    #[tokio::test]
    async fn structured_output_captures_clears_invalid_and_recovers() {
        let fixture = FileFixture::new();
        let schema = json!({
            "type":"object", "required":["title", "count"],
            "properties":{"title":{"type":"string"}, "count":{"type":"integer"}}
        });
        let sink = Arc::new(Mutex::new(None));
        let tool = StructuredOutputTool::new(schema.clone(), sink.clone());
        assert_eq!(tool.spec().input_schema, schema);
        assert!(tool.is_read_only());
        let first = json!({"title":"first", "count":3});
        assert_eq!(
            tool.execute(first.clone(), &fixture.ctx)
                .await
                .unwrap()
                .as_text()
                .unwrap(),
            "recorded"
        );
        assert_eq!(*sink.lock().unwrap(), Some(first));

        let error = tool
            .execute(json!({"count":3}), &fixture.ctx)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
        assert!(error.message.contains("title"));
        assert_eq!(*sink.lock().unwrap(), None);

        let corrected = json!({"title":"corrected", "count":2.0});
        assert_eq!(
            tool.execute(corrected.clone(), &fixture.ctx)
                .await
                .unwrap()
                .as_text()
                .unwrap(),
            "recorded"
        );
        assert_eq!(*sink.lock().unwrap(), Some(corrected));
    }

    #[tokio::test]
    async fn structured_output_invalid_schema_clears_previous_capture() {
        let fixture = FileFixture::new();
        let sink = Arc::new(Mutex::new(Some(json!({"previous":true}))));
        let tool = StructuredOutputTool::new(json!({"type":"not-a-type"}), sink.clone());
        let error = tool.execute(json!({}), &fixture.ctx).await.unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
        assert!(error.message.contains("output does not match the schema"));
        assert_eq!(*sink.lock().unwrap(), None);
    }

    #[tokio::test]
    async fn nested_validation_clears_capture_without_affecting_another_sink() {
        let fixture = FileFixture::new();
        let schema = json!({
            "type":"object", "required":["rows"], "additionalProperties":false,
            "properties":{"rows":{"type":"array", "minItems":1, "items":{
                "type":"object", "required":["count"],
                "properties":{"count":{"type":"integer", "minimum":1}}
            }}}
        });
        let sink = Arc::new(Mutex::new(None));
        let other_sink = Arc::new(Mutex::new(None));
        let tool = StructuredOutputTool::new(schema.clone(), sink.clone());
        let other = StructuredOutputTool::new(schema, other_sink.clone());
        let valid = json!({"rows":[{"count":2}]});
        other.execute(valid.clone(), &fixture.ctx).await.unwrap();
        for invalid in [
            Value::Null,
            json!({"rows":[]}),
            json!({"rows":[{"count":0}]}),
            json!({"rows":[{"count":1.5}]}),
            json!({"rows":[{"count":1}],"extra":true}),
        ] {
            tool.execute(valid.clone(), &fixture.ctx).await.unwrap();
            let error = tool.execute(invalid, &fixture.ctx).await.unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::InvalidInput);
            assert!(error.message.contains("Re-call structured_output"));
            assert_eq!(*sink.lock().unwrap(), None);
            assert_eq!(*other_sink.lock().unwrap(), Some(valid.clone()));
        }
        let corrected = json!({"rows":[{"count":3},{"count":4}]});
        tool.execute(corrected.clone(), &fixture.ctx).await.unwrap();
        assert_eq!(sink.lock().unwrap().take(), Some(corrected));
        assert_eq!(*sink.lock().unwrap(), None);
    }
}
