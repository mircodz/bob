//! Core provider-agnostic vocabulary. Providers translate to/from these;
//! agents, tools, and the orchestrator only ever deal in these shapes.

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

/// A single piece of content within a message.
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
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
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

    /// Concatenate all text blocks.
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

/// Provider-agnostic description of a callable tool.
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
    /// Accumulate another usage into this one (all four counters).
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

/// What a provider returns for one turn of generation.
#[derive(Clone, Debug)]
pub struct Completion {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Streaming events, normalized across providers.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    TextDelta { text: String },
    ToolUseStart { id: String, name: String },
    ToolUseInputDelta { id: String, partial_json: String },
    MessageStop { completion: Completion },
}

/// Reasoning intensity for models that support it (OpenAI Responses
/// `reasoning.effort`, Anthropic extended thinking). Off = don't request any.
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

    /// Parse a label back into a variant (for CLI/picker input).
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

/// Options for a single generation.
#[derive(Clone, Debug, Default)]
pub struct GenerateOptions {
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Ask the provider to insert prompt-cache breakpoints (Anthropic).
    pub cache: bool,
    /// Reasoning intensity, if the model supports it.
    pub reasoning: ReasoningEffort,
}
