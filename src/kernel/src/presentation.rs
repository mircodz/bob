use serde::{Deserialize, Serialize};

macro_rules! vocabulary {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $value)] $variant),+
        }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl std::str::FromStr for $name {
            type Err = anyhow::Error;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => anyhow::bail!("unknown {}: {value}", stringify!($name)),
                }
            }
        }
    };
}

vocabulary!(ThreadState {
    Idle => "idle", Held => "held", Active => "active", Stopping => "stopping",
    Interrupted => "interrupted", Completed => "completed", Failed => "failed",
    Cancelled => "cancelled",
});
vocabulary!(OperationState {
    Pending => "pending", Preparing => "preparing", Ready => "ready", Approval => "approval",
    Started => "started", Waiting => "waiting", Done => "done", Failed => "failed",
    Cancelled => "cancelled", Unknown => "unknown", Resolved => "resolved", Interrupted => "interrupted",
});
vocabulary!(EntryKind {
    Submission => "submission", Assistant => "assistant", ToolCall => "tool_call",
    ToolResult => "tool_result", ToolError => "tool_error", Coordination => "coordination",
    Recovery => "recovery", Notice => "notice", AgentStarted => "agent_started",
});

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ThreadActivity {
    Model,
    Tool { name: String },
    Approval,
    Agent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPresentation {
    pub title: String,
    pub target: Option<String>,
    pub body: ToolBody,
    pub details: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolBody {
    Text {
        text: String,
    },
    Command {
        command: String,
    },
    Edit {
        old_string: String,
        new_string: String,
    },
    Create {
        content: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultPresentation {
    pub summary: Option<String>,
    pub body_preview: Option<String>,
    pub exit_code: Option<i32>,
}
