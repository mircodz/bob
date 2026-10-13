//! Provider-neutral message and request types; no engine dependencies.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: ToolOutput,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    /// Anthropic requires the signature unchanged alongside tool calls. Not displayed.
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Encrypted/redacted thinking — opaque bytes to be passed back unchanged.
    RedactedThinking {
        data: String,
    },
    /// Opaque Responses reasoning, echoed unchanged for `store: false` continuity.
    /// Other providers skip it.
    ReasoningItem {
        item: Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ToolOutput {
    Text(String),
    Read {
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        loader_context: Option<String>,
    },
    Shell(Box<ShellOutput>),
}

impl ToolOutput {
    pub fn model_text(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Text(text) => std::borrow::Cow::Borrowed(text),
            Self::Read {
                body,
                loader_context,
            } => match loader_context {
                Some(context) if !context.is_empty() => {
                    std::borrow::Cow::Owned(format!("{body}\n\n{context}"))
                }
                _ => std::borrow::Cow::Borrowed(body),
            },
            Self::Shell(shell) => std::borrow::Cow::Owned(
                serde_json::to_string(shell).expect("shell output is serializable"),
            ),
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Read { .. } | Self::Shell(_) => None,
        }
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for ToolOutput {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<ShellOutput> for ToolOutput {
    fn from(output: ShellOutput) -> Self {
        Self::Shell(Box::new(output))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellOutcome {
    #[default]
    Exited,
    TimedOut,
    Cancelled,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellOutput {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub core_dumped: bool,
    pub outcome: ShellOutcome,
    pub stdout: ShellStream,
    pub stderr: ShellStream,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<ShellPage>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellStream {
    pub text: String,
    pub total_bytes: u64,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellPage {
    pub job_id: String,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub retained_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Text blocks only; excludes tool results and reasoning.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }

    /// Total tokens billed as input this turn (fresh + cache write + cache read).
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
}

#[derive(Clone, Debug)]
pub struct Completion {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Clone, Debug)]
pub enum StreamEvent {
    TextDelta {
        text: String,
    },
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseInputDelta {
        id: String,
        partial_json: String,
    },
    MessageStop {
        completion: Completion,
    },
    /// Stream failure, not assistant content.
    Error {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReasoningEffort {
    #[default]
    Off,
    Low,
    Medium,
    High,
    Max,
}

impl ReasoningEffort {
    /// The wire value for OpenAI Responses `reasoning.effort` (None = Off).
    pub fn as_str(&self) -> Option<&'static str> {
        match self {
            ReasoningEffort::Off => None,
            ReasoningEffort::Low => Some("low"),
            ReasoningEffort::Medium => Some("medium"),
            ReasoningEffort::High => Some("high"),
            // OpenAI's highest tier is "high"; Anthropic gets a bigger budget.
            ReasoningEffort::Max => Some("high"),
        }
    }

    /// A thinking-token budget for Anthropic extended thinking (None = Off).
    pub fn thinking_budget(&self) -> Option<u32> {
        match self {
            ReasoningEffort::Off => None,
            ReasoningEffort::Low => Some(4_000),
            ReasoningEffort::Medium => Some(10_000),
            ReasoningEffort::High => Some(24_000),
            ReasoningEffort::Max => Some(48_000),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ReasoningEffort::Off => "off",
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(ReasoningEffort::Off),
            "low" | "minimal" => Some(ReasoningEffort::Low),
            "medium" | "med" => Some(ReasoningEffort::Medium),
            "high" => Some(ReasoningEffort::High),
            "max" | "xhigh" | "extra" => Some(ReasoningEffort::Max),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct GenerateOptions {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Ask the provider to insert prompt-cache breakpoints (Anthropic).
    pub cache: bool,
    pub reasoning: ReasoningEffort,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roles_have_stable_lowercase_wire_names() {
        for (role, wire) in [
            (Role::System, "system"),
            (Role::User, "user"),
            (Role::Assistant, "assistant"),
            (Role::Tool, "tool"),
        ] {
            assert_eq!(serde_json::to_value(role).unwrap(), json!(wire));
            assert_eq!(serde_json::from_value::<Role>(json!(wire)).unwrap(), role);
        }
        for invalid in [json!("Assistant"), json!("developer"), json!(null)] {
            assert!(serde_json::from_value::<Role>(invalid).is_err());
        }
    }

    #[test]
    fn message_fixture_decodes_fields_and_encodes_the_documented_shape() {
        let fixture = json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "Checking…\n"},
                {"type": "tool_use", "id": "call-7", "name": "lookup",
                 "input": {"path": "a/b", "limits": [1, null, true]}},
                {"type": "tool_result", "tool_use_id": "call-7", "content": {"type": "text", "data": "found"}}
            ]
        });
        let decoded: Message = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(decoded.role, Role::Assistant);
        assert_eq!(decoded.content.len(), 3);
        assert!(
            matches!(&decoded.content[0], ContentBlock::Text { text } if text == "Checking…\n")
        );
        assert!(matches!(
            &decoded.content[1],
            ContentBlock::ToolUse { id, name, input }
                if id == "call-7" && name == "lookup"
                    && input == &json!({"path": "a/b", "limits": [1, null, true]})
        ));
        assert!(matches!(
            &decoded.content[2],
            ContentBlock::ToolResult { tool_use_id, content, is_error: None }
                if tool_use_id == "call-7" && content.as_text() == Some("found")
        ));

        let constructed = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Checking…\n".into(),
                },
                ContentBlock::ToolUse {
                    id: "call-7".into(),
                    name: "lookup".into(),
                    input: json!({"path": "a/b", "limits": [1, null, true]}),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call-7".into(),
                    content: "found".into(),
                    is_error: None,
                },
            ],
        };
        assert_eq!(serde_json::to_value(constructed).unwrap(), fixture);
    }

    #[test]
    fn tool_output_tags_preserve_shell_data_and_model_text() {
        let shell = ShellOutput {
            exit_code: Some(7),
            stdout: ShellStream {
                text: "[exit: 0] is ordinary output\n".into(),
                total_bytes: 99,
                truncated: true,
                output_id: Some("output_12".into()),
            },
            stderr: ShellStream {
                text: "failed\n".into(),
                total_bytes: 7,
                ..Default::default()
            },
            ..Default::default()
        };
        for output in [
            ToolOutput::from("plain result"),
            ToolOutput::Read {
                body: "     1\t<project-instruction>ordinary file text</project-instruction>"
                    .into(),
                loader_context: Some("[!] auxiliary instructions unavailable\n日本語".into()),
            },
            ToolOutput::from(shell),
        ] {
            let encoded = serde_json::to_value(&output).unwrap();
            let restored: ToolOutput = serde_json::from_value(encoded.clone()).unwrap();
            assert_eq!(restored, output);
            match &output {
                ToolOutput::Text(text) => {
                    assert_eq!(encoded, json!({"type": "text", "data": text}));
                    assert_eq!(restored.model_text(), text.as_str());
                }
                ToolOutput::Read {
                    body,
                    loader_context,
                } => {
                    assert_eq!(encoded["type"], "read");
                    assert_eq!(encoded["data"]["body"], body.as_str());
                    assert_eq!(
                        encoded["data"]["loader_context"],
                        loader_context.as_deref().unwrap()
                    );
                    assert_eq!(restored.as_text(), None);
                    assert_eq!(
                        restored.model_text(),
                        format!("{body}\n\n{}", loader_context.as_deref().unwrap())
                    );
                }
                ToolOutput::Shell(shell) => {
                    assert_eq!(encoded["type"], "shell");
                    assert_eq!(encoded["data"]["exit_code"], 7);
                    assert_eq!(encoded["data"]["stdout"]["output_id"], "output_12");
                    assert_eq!(
                        serde_json::from_str::<ShellOutput>(&restored.model_text()).unwrap(),
                        **shell
                    );
                }
            }
        }
    }

    #[test]
    fn tool_result_error_flag_distinguishes_absent_false_and_true() {
        for flag in [None, Some(false), Some(true)] {
            let mut fixture = json!({
                "type": "tool_result", "tool_use_id": "call", "content": {"type": "text", "data": "output"}
            });
            if let Some(flag) = flag {
                fixture["is_error"] = json!(flag);
            }
            let block: ContentBlock = serde_json::from_value(fixture.clone()).unwrap();
            assert!(matches!(block, ContentBlock::ToolResult { is_error, .. } if is_error == flag));
            let constructed = ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "output".into(),
                is_error: flag,
            };
            assert_eq!(serde_json::to_value(constructed).unwrap(), fixture);
        }
        let block: ContentBlock = serde_json::from_value(json!({
            "type": "tool_result", "tool_use_id": "call", "content": {"type": "text", "data": "output"}, "is_error": null
        }))
        .unwrap();
        assert!(matches!(
            block,
            ContentBlock::ToolResult { is_error: None, .. }
        ));
    }

    #[test]
    fn opaque_reasoning_survives_without_interpretation() {
        let fixtures = [
            json!({"type": "thinking", "thinking": "private\n思考", "signature": " sig+/=\n\u{0000}"}),
            json!({"type": "redacted_thinking", "data": " encrypted+/=\n\u{0000}"}),
            json!({"type": "reasoning_item", "item": {
                "id": "rs_1", "type": "reasoning", "encrypted_content": "opaque+/=",
                "summary": [{"type": "summary_text", "text": "not public text"}],
                "future_field": {"nested": [null, true, 42, "λ"]}
            }}),
        ];
        for fixture in fixtures {
            let block: ContentBlock = serde_json::from_value(fixture.clone()).unwrap();
            match &block {
                ContentBlock::Thinking {
                    thinking,
                    signature,
                } => {
                    assert_eq!(thinking, "private\n思考");
                    assert_eq!(signature, " sig+/=\n\u{0000}");
                }
                ContentBlock::RedactedThinking { data } => {
                    assert_eq!(data, " encrypted+/=\n\u{0000}");
                }
                ContentBlock::ReasoningItem { item } => assert_eq!(item, &fixture["item"]),
                other => panic!("wrong reasoning variant: {other:?}"),
            }
            assert_eq!(serde_json::to_value(&block).unwrap(), fixture);
            assert_eq!(
                Message {
                    role: Role::Assistant,
                    content: vec![block]
                }
                .text(),
                ""
            );
        }
    }

    #[test]
    fn malformed_content_does_not_silently_become_empty_text() {
        for fixture in [
            json!({"type": "unknown", "text": "hello"}),
            json!({"text": "hello"}),
            json!({"type": "text"}),
            json!({"type": "tool_use", "name": "lookup", "input": {}}),
            json!({"type": "tool_result", "tool_use_id": "call", "content": {"type": "text", "data": "x"}, "is_error": "false"}),
            json!({"type": "thinking", "thinking": "unsigned"}),
            json!({"type": "redacted_thinking"}),
        ] {
            assert!(
                serde_json::from_value::<ContentBlock>(fixture.clone()).is_err(),
                "{fixture}"
            );
        }
    }

    #[test]
    fn text_extraction_joins_only_text_in_order_without_normalizing() {
        let message: Message = serde_json::from_value(json!({
            "role": "assistant",
            "content": [
                {"type": "text", "text": "  café"},
                {"type": "tool_use", "id": "call", "name": "hidden", "input": {"text": "hidden"}},
                {"type": "text", "text": ""},
                {"type": "tool_result", "tool_use_id": "call", "content": {"type": "text", "data": "hidden"}},
                {"type": "thinking", "thinking": "hidden", "signature": "sig"},
                {"type": "redacted_thinking", "data": "hidden"},
                {"type": "reasoning_item", "item": {"text": "hidden"}},
                {"type": "text", "text": "\n世界  "}
            ]
        }))
        .unwrap();
        assert_eq!(message.text(), "  café\n世界  ");
        assert_eq!(
            Message {
                role: Role::Tool,
                content: vec![]
            }
            .text(),
            ""
        );
    }

    #[test]
    fn usage_legacy_fixture_defaults_only_cache_counters() {
        let usage: Usage =
            serde_json::from_value(json!({"input_tokens": 17, "output_tokens": 23})).unwrap();
        assert_eq!(usage.input_tokens, 17);
        assert_eq!(usage.output_tokens, 23);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(
            serde_json::to_value(usage).unwrap(),
            json!({
                "input_tokens": 17, "output_tokens": 23,
                "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
            })
        );
        for invalid in [
            json!({"input_tokens": 1}),
            json!({"output_tokens": 1}),
            json!({"input_tokens": -1, "output_tokens": 2}),
            json!({"input_tokens": 1, "output_tokens": 2, "cache_read_input_tokens": null}),
        ] {
            assert!(serde_json::from_value::<Usage>(invalid).is_err());
        }
        assert_eq!(
            serde_json::to_value(Usage::default()).unwrap(),
            json!({
                "input_tokens": 0, "output_tokens": 0,
                "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0
            })
        );
    }

    #[test]
    fn usage_accumulates_every_counter_and_excludes_output_from_total_input() {
        let mut usage: Usage = serde_json::from_value(json!({
            "input_tokens": 4_294_967_300_u64, "output_tokens": 11,
            "cache_creation_input_tokens": 17, "cache_read_input_tokens": 23
        }))
        .unwrap();
        let other = Usage {
            input_tokens: 31,
            output_tokens: 37,
            cache_creation_input_tokens: 41,
            cache_read_input_tokens: 43,
        };
        usage.add(&other);
        usage.add(&Usage::default());
        assert_eq!(
            serde_json::to_value(usage).unwrap(),
            json!({
                "input_tokens": 4_294_967_331_u64, "output_tokens": 48,
                "cache_creation_input_tokens": 58, "cache_read_input_tokens": 66
            })
        );
        assert_eq!(usage.total_input(), 4_294_967_455);
        assert_eq!(
            serde_json::to_value(other).unwrap(),
            json!({
                "input_tokens": 31, "output_tokens": 37,
                "cache_creation_input_tokens": 41, "cache_read_input_tokens": 43
            })
        );
        assert_eq!(Usage::default().total_input(), 0);
    }

    #[test]
    fn reasoning_mappings_are_explicit_for_every_level() {
        for (effort, wire, budget, label) in [
            (ReasoningEffort::Off, None, None, "off"),
            (ReasoningEffort::Low, Some("low"), Some(4_000), "low"),
            (
                ReasoningEffort::Medium,
                Some("medium"),
                Some(10_000),
                "medium",
            ),
            (ReasoningEffort::High, Some("high"), Some(24_000), "high"),
            (ReasoningEffort::Max, Some("high"), Some(48_000), "max"),
        ] {
            assert_eq!(effort.as_str(), wire, "{effort:?}");
            assert_eq!(effort.thinking_budget(), budget, "{effort:?}");
            assert_eq!(effort.label(), label, "{effort:?}");
            assert_eq!(ReasoningEffort::parse(label), Some(effort));
        }
    }

    #[test]
    fn reasoning_parser_accepts_aliases_and_rejects_unknown_labels() {
        for (label, expected) in [
            (" NONE ", ReasoningEffort::Off),
            ("\tMinimal\n", ReasoningEffort::Low),
            (" MED ", ReasoningEffort::Medium),
            (" HIGH ", ReasoningEffort::High),
            (" XHIGH ", ReasoningEffort::Max),
            ("Extra", ReasoningEffort::Max),
        ] {
            assert_eq!(ReasoningEffort::parse(label), Some(expected));
        }
        for label in ["", " \n", "auto", "low high", "maximum", "high!", "médium"] {
            assert_eq!(ReasoningEffort::parse(label), None, "{label:?}");
        }
    }
}
