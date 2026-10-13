//! Content-block conversion must explicitly emit or skip every variant.

use serde_json::Value;

use crate::core::types::{StopReason, ToolSpec};
use crate::tools::schema::CompiledSchema;

pub enum BlockWire {
    Emit(Value),
    Skip,
}

/// Accepts Anthropic and OpenAI stop reasons; unknown values mean `EndTurn`.
pub fn map_stop_reason(raw: &str) -> StopReason {
    match raw {
        "tool_use" | "tool_calls" | "function_call" => StopReason::ToolUse,
        "max_tokens" | "length" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

impl BlockWire {
    pub fn into_value(self) -> Option<Value> {
        match self {
            BlockWire::Emit(v) => Some(v),
            BlockWire::Skip => None,
        }
    }
}

/// Parse-error sentinel recognized by the agent loop before tool execution.
pub const TOOL_ARGS_PARSE_ERROR_KEY: &str = "__bob_tool_args_parse_error__";

pub(crate) fn complete_tool_input(
    id: &str,
    name: &str,
    raw: &str,
    tools: &[ToolSpec],
) -> anyhow::Result<Value> {
    if id.is_empty() || name.is_empty() {
        anyhow::bail!("tool call missing id or name");
    }
    if raw.is_empty() {
        let empty = serde_json::json!({});
        if tools
            .iter()
            .find(|tool| tool.name == name)
            .is_some_and(|tool| {
                CompiledSchema::compile(&tool.input_schema)
                    .and_then(|schema| schema.validate(&empty))
                    .is_ok()
            })
        {
            return Ok(empty);
        }
    }
    let value: Value = serde_json::from_str(raw).map_err(|error| {
        anyhow::anyhow!("incomplete or malformed arguments for {name}: {error}")
    })?;
    if !value.is_object() {
        anyhow::bail!("arguments for {name} must be a JSON object");
    }
    Ok(value)
}

/// Empty arguments become `{}`; malformed JSON retains its raw text and error
/// in a sentinel for the agent loop.
pub fn parse_tool_input(raw: &str) -> Value {
    if raw.is_empty() {
        return serde_json::json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v) => v,
        Err(e) => serde_json::json!({
            TOOL_ARGS_PARSE_ERROR_KEY: {
                "raw": raw,
                "error": e.to_string(),
            }
        }),
    }
}

pub fn tool_input_parse_error(input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    if obj.len() != 1 {
        return None;
    }
    let err = obj.get(TOOL_ARGS_PARSE_ERROR_KEY)?;
    let raw = err.get("raw").and_then(Value::as_str).unwrap_or("");
    let msg = err.get("error").and_then(Value::as_str).unwrap_or("");
    Some(format!(
        "tool arguments were not valid JSON ({msg}); the streamed arguments \
         arrived as: {raw:?}. Re-issue the call with well-formed JSON arguments."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stop_reason_covers_both_provider_vocabularies() {
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("stop_sequence"), StopReason::StopSequence);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("function_call"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("length"), StopReason::MaxTokens);
        assert_eq!(map_stop_reason("stop"), StopReason::EndTurn);
        assert_eq!(map_stop_reason(""), StopReason::EndTurn);
        assert_eq!(map_stop_reason("future_reason"), StopReason::EndTurn);
    }

    #[test]
    fn parse_tool_input_distinguishes_empty_valid_and_broken() {
        let empty = parse_tool_input("");
        assert_eq!(empty, json!({}));
        assert!(tool_input_parse_error(&empty).is_none());

        let ok = parse_tool_input(r#"{"shape":"map_reduce","items":["a"]}"#);
        assert_eq!(ok, json!({"shape":"map_reduce","items":["a"]}));
        assert!(tool_input_parse_error(&ok).is_none());

        let broken = parse_tool_input(r#"{"shape":"map_re"#);
        let explain = tool_input_parse_error(&broken).expect("should be tagged as a parse error");
        assert!(explain.contains("not valid JSON"));
        assert!(explain.contains("shape"));
    }

    #[test]
    fn complete_calls_require_identity_and_object_arguments() {
        for (id, name) in [("", "read_file"), ("call-1", ""), ("", "")] {
            assert_eq!(
                complete_tool_input(id, name, "{}", &[])
                    .unwrap_err()
                    .to_string(),
                "tool call missing id or name"
            );
        }
        for raw in ["null", "[]", "42", "true", "\"text\""] {
            assert_eq!(
                complete_tool_input("call-1", "read_file", raw, &[])
                    .unwrap_err()
                    .to_string(),
                "arguments for read_file must be a JSON object",
                "{raw}"
            );
        }
        for raw in ["", " ", "{", "{\"path\":", "{} trailing", "{\"x\":1,}"] {
            let error = complete_tool_input("call-1", "read_file", raw, &[]).unwrap_err();
            assert!(error
                .to_string()
                .contains("incomplete or malformed arguments for read_file"));
        }
        assert_eq!(
            complete_tool_input("call-1", "list", " {} \n", &[]).unwrap(),
            json!({})
        );
        assert_eq!(
            complete_tool_input(
                "call-2",
                "read_file",
                r#"{"path":"日本語/😀.rs","options":{"lines":[1,3],"literal":"a\nb\"c"}}"#,
                &[],
            )
            .unwrap(),
            json!({"path":"日本語/😀.rs","options":{"lines":[1,3],"literal":"a\nb\"c"}})
        );
    }

    #[test]
    fn empty_arguments_require_an_advertised_schema_that_accepts_an_empty_object() {
        for (input_schema, accepts_empty) in [
            (json!({"type":"object","properties":{}}), true),
            (
                json!({"type":"object","properties":{"limit":{"type":"integer"}}}),
                true,
            ),
            (json!({"type":"object","required":["path"]}), false),
            (json!({"type":"object","minProperties":1}), false),
            (json!({"allOf":[{"required":["path"]}]}), false),
            (
                json!({"$defs":{"input":{"required":["path"]}},"$ref":"#/$defs/input"}),
                false,
            ),
            (json!({"type":"not-a-type"}), false),
            (json!({"$ref":"https://example.invalid/schema"}), false),
        ] {
            let tools = [ToolSpec {
                name: "inspection".into(),
                description: String::new(),
                input_schema,
            }];
            let result = complete_tool_input("call-1", "inspection", "", &tools);
            assert_eq!(result.is_ok(), accepts_empty, "{:?}", tools[0]);
            if let Ok(value) = result {
                assert_eq!(value, json!({}));
            }
            for raw in [" ", "{", "{} trailing", "null", "[]"] {
                assert!(complete_tool_input("call-1", "inspection", raw, &tools).is_err());
            }
            assert!(complete_tool_input("call-1", "unadvertised", "", &tools).is_err());
            assert!(complete_tool_input("", "inspection", "", &tools).is_err());
            assert!(complete_tool_input("call-1", "", "", &tools).is_err());
        }
    }

    #[test]
    fn malformed_arguments_preserve_raw_evidence_and_do_not_tag_ordinary_values() {
        for raw in [" ", "{\"path\":\"日本語", "{} trailing"] {
            let parsed = parse_tool_input(raw);
            assert_eq!(parsed.as_object().unwrap().len(), 1);
            assert_eq!(parsed[TOOL_ARGS_PARSE_ERROR_KEY]["raw"], raw);
            assert!(!parsed[TOOL_ARGS_PARSE_ERROR_KEY]["error"]
                .as_str()
                .unwrap()
                .is_empty());
            assert!(tool_input_parse_error(&parsed)
                .unwrap()
                .contains("Re-issue the call"));
        }
        for value in [
            json!(null),
            json!([]),
            json!({"path":"a.rs"}),
            json!({"__bob_tool_args_parse_error__":{"raw":"{","error":"bad"},"path":"a.rs"}),
        ] {
            assert!(tool_input_parse_error(&value).is_none(), "{value}");
        }
    }
}
