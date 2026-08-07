//! Shared discipline for message → wire-format translation.
//!
//! The three provider wire formats (Anthropic content-block arrays, OpenAI
//! role-split `tool_calls`, the Responses `input`-item stream) are genuinely
//! different serializations — a codec trait can't dedupe the *bodies*. What it
//! CAN do is make the one recurring bug impossible: silently dropping a
//! `ContentBlock` variant.
//!
//! Historically each provider converted blocks with a `filter_map` ending in
//! `_ => None`, so a block a provider didn't recognize just vanished — with no
//! error and no test. OpenAI dropped `Thinking`/`RedactedThinking`/`ReasoningItem`
//! this way.
//!
//! The fix is [`BlockWire`]: every conversion is an EXHAUSTIVE `match` returning
//! an explicit decision per block — `Emit` it, or deliberately `Skip` it with a
//! stated reason. A new `ContentBlock` variant then fails to compile in each
//! provider until someone consciously decides how to handle it. The shared
//! conformance test below runs every variant through every provider so a
//! regression to silent-drop can't slip back in.

use serde_json::Value;

use crate::core::types::StopReason;

/// A provider's decision about one [`ContentBlock`](crate::core::types::ContentBlock)
/// when building a request. `Skip` is a *deliberate* omission (the format can't
/// represent this block), distinct from an accidental drop — the reason is
/// documented at the call site.
pub enum BlockWire {
    Emit(Value),
    Skip,
}

/// Map a provider's finish/stop-reason string to our [`StopReason`]. Accepts the
/// union of both vocabularies — Anthropic (`tool_use`, `stop_sequence`) and
/// OpenAI/chat (`tool_calls`, `function_call`, `length`) — so the two providers
/// share one mapper instead of maintaining near-identical copies. Unknown strings
/// fall back to `EndTurn`.
pub fn map_stop_reason(raw: &str) -> StopReason {
    match raw {
        "tool_use" | "tool_calls" | "function_call" => StopReason::ToolUse,
        "max_tokens" | "length" => StopReason::MaxTokens,
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

impl BlockWire {
    /// The emitted JSON, or `None` when this block was deliberately skipped.
    pub fn into_value(self) -> Option<Value> {
        match self {
            BlockWire::Emit(v) => Some(v),
            BlockWire::Skip => None,
        }
    }
}

/// The key under which [`parse_tool_input`] tags a tool-argument JSON that
/// failed to parse. The agent loop detects this and returns a real error to the
/// model instead of silently invoking the tool with empty args.
pub const TOOL_ARGS_PARSE_ERROR_KEY: &str = "__bob_tool_args_parse_error__";

/// Turn a streamed tool_use's *accumulated argument string* into its input JSON.
///
/// Every provider streams tool arguments as a sequence of partial-JSON deltas
/// that we concatenate into one string, then parse once the block closes. The
/// three providers historically each did `serde_json::from_str(raw).unwrap_or(json!({}))`
/// — so a raw string that was empty (a dropped/mis-indexed delta stream) OR
/// merely unparseable BOTH collapsed to `{}` with no error. The tool then ran
/// with empty args and failed downstream ("missing field …"), and the model saw
/// only that misleading error — never that its arguments had been dropped.
///
/// This makes the three cases distinct:
/// - empty accumulator → `{}` (a genuine no-argument call; legitimate).
/// - valid JSON        → the parsed value.
/// - unparseable JSON  → a tagged object `{ TOOL_ARGS_PARSE_ERROR_KEY: { raw, error } }`
///   so the loss is *visible* and the agent loop can surface it to the model.
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

/// If `input` is a parse-error sentinel from [`parse_tool_input`], return a
/// human-readable explanation (for a tool-result error). `None` otherwise.
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
pub(crate) mod conformance {
    use crate::core::types::ContentBlock;
    use serde_json::json;

    /// One of every [`ContentBlock`] variant. The shared test harness feeds this
    /// through each provider's block converter to prove none are silently lost.
    /// Adding a variant to `ContentBlock` without extending this list fails the
    /// exhaustiveness check below.
    pub fn all_content_blocks() -> Vec<ContentBlock> {
        let blocks = vec![
            ContentBlock::Text { text: "hi".into() },
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read_file".into(),
                input: json!({"path": "a.rs"}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "ok".into(),
                is_error: Some(false),
            },
            ContentBlock::Thinking {
                thinking: "hmm".into(),
                signature: "sig".into(),
            },
            ContentBlock::RedactedThinking { data: "xxx".into() },
            ContentBlock::ReasoningItem {
                item: json!({"id": "r1"}),
            },
        ];
        // Exhaustiveness guard: if a ContentBlock variant is added and NOT
        // included above, this match fails to compile — forcing the sample list
        // (and thus every provider's conformance test) to cover it.
        for b in &blocks {
            match b {
                ContentBlock::Text { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::ReasoningItem { .. } => {}
            }
        }
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::map_stop_reason;
    use crate::core::types::StopReason;

    #[test]
    fn stop_reason_covers_both_provider_vocabularies() {
        // Anthropic vocabulary.
        assert_eq!(map_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("stop_sequence"), StopReason::StopSequence);
        assert_eq!(map_stop_reason("max_tokens"), StopReason::MaxTokens);
        // OpenAI / chat vocabulary.
        assert_eq!(map_stop_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("function_call"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("length"), StopReason::MaxTokens);
        // Anything else is a normal end-of-turn.
        assert_eq!(map_stop_reason("stop"), StopReason::EndTurn);
        assert_eq!(map_stop_reason(""), StopReason::EndTurn);
    }

    #[test]
    fn parse_tool_input_distinguishes_empty_valid_and_broken() {
        use super::{parse_tool_input, tool_input_parse_error};
        use serde_json::json;

        // Empty accumulator → a legitimate no-argument call, not an error.
        let empty = parse_tool_input("");
        assert_eq!(empty, json!({}));
        assert!(tool_input_parse_error(&empty).is_none());

        // Valid JSON round-trips.
        let ok = parse_tool_input(r#"{"shape":"map_reduce","items":["a"]}"#);
        assert_eq!(ok["shape"], "map_reduce");
        assert!(tool_input_parse_error(&ok).is_none());

        // Truncated / malformed JSON (the real bug) is TAGGED, not silently {} —
        // and the tag carries the raw text + parser error for the model to see.
        let broken = parse_tool_input(r#"{"shape":"map_re"#);
        let explain = tool_input_parse_error(&broken).expect("should be tagged as a parse error");
        assert!(explain.contains("not valid JSON"));
        assert!(explain.contains("shape")); // includes the raw fragment

        // A normal object that merely happens to have one key is NOT mistaken for
        // the sentinel.
        assert!(tool_input_parse_error(&json!({"path": "a.rs"})).is_none());
    }
}
