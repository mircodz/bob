//! Every observable thing the agent does is one of these events. This is THE
//! seam between the core and any UI. The core never writes to stdout directly;
//! a frontend subscribes to this stream and renders it however it likes.

use crate::core::types::{Message, Usage};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Every observable agent action, serializable so it can be appended to the
/// per-session event log (see [`crate::core::session`]) and replayed on resume.
///
/// Serialized internally-tagged (`{"kind":"ToolCall", ...}`) with NO
/// `deny_unknown_fields`, so a newer bob can add fields to an existing variant
/// and an older bob ignores them. Unrecognized *variants* decode to
/// [`AgentEvent::Unknown`] (via `#[serde(other)]`) and replay as a no-op — the
/// log is thus forward-compatible across versions.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum AgentEvent {
    /// A user turn was submitted to this agent. Emitted by [`Agent::run`] the
    /// instant the prompt is pushed to history, so the transcript's user line is
    /// part of the event stream (and thus the replay log) rather than a UI-only
    /// side effect. Empty coordination "wake" turns emit nothing.
    UserPrompt {
        agent_id: String,
        text: String,
    },
    TurnStart {
        agent_id: String,
    },
    TextDelta {
        agent_id: String,
        text: String,
    },
    Message {
        agent_id: String,
        message: Message,
    },
    ToolCall {
        agent_id: String,
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        agent_id: String,
        tool_use_id: String,
        output: String,
        is_error: bool,
    },
    SubagentSpawn {
        parent_id: String,
        agent_id: String,
        task: String,
        /// The full instructions the parent gave the subagent (the run prompt).
        /// Surfaced in the drawer so you can see exactly what was delegated;
        /// `task` above is only the short label.
        prompt: String,
    },
    /// A spawned agent finished. `failed` marks an error exit (shown red).
    SubagentDone {
        agent_id: String,
        failed: bool,
    },
    /// A coordination message delivered from one team agent to another.
    AgentMessage {
        to: String,
        from: String,
        text: String,
    },
    Compaction {
        agent_id: String,
        before_tokens: usize,
        after_tokens: usize,
        /// The summary text that replaced the older messages. Carried in the log
        /// so replay can rebuild the agent's compacted working set, not just the
        /// full-history view. Token counts alone are insufficient to reconstruct
        /// it. Defaults empty for events written before this field existed.
        #[serde(default)]
        summary: String,
        /// The `full_history` index up to which older messages were folded into
        /// `summary`. On replay the working set drops `working[..replaced_upto]`
        /// and prepends the synthetic summary message.
        #[serde(default)]
        replaced_upto: usize,
    },
    /// A workflow run entered a new phase. `index`/`total` drive a progress bar; the
    /// `workflow_id` groups this run's agents + phases into one live tree in the UI.
    WorkflowPhase {
        workflow_id: String,
        title: String,
        index: usize,
        total: usize,
    },
    /// A free-form progress line from a workflow run (e.g. "6/7 reports written").
    WorkflowLog {
        workflow_id: String,
        message: String,
    },
    /// The estimated context usage crossed a graded warning threshold (e.g. 70 /
    /// 85 / 95%). Emitted at most once per level per direction so the UI can warn
    /// the user that context is filling up before an auto-compaction kicks in.
    ContextWarning {
        agent_id: String,
        used_tokens: usize,
        context_window: usize,
        /// The crossed threshold as a percentage (70, 85, 95).
        pct: u8,
    },
    /// Emitted once per provider completion (finer than TurnEnd), carrying the
    /// exact token usage for that single model call, tagged with the model.
    Completion {
        agent_id: String,
        model: String,
        usage: Usage,
    },
    TurnEnd {
        agent_id: String,
        usage: Usage,
    },
    Error {
        agent_id: String,
        message: String,
    },
    /// A variant written by a newer bob that this build doesn't recognize.
    /// Decoded here (never constructed locally) so replaying a forward-version
    /// log is a no-op rather than a hard deserialize error.
    #[serde(other)]
    Unknown,
}

impl AgentEvent {
    /// The variant tag, stored in the log's `kind` column for cheap filtering
    /// and debugging without parsing the JSON payload.
    pub fn kind(&self) -> &'static str {
        match self {
            AgentEvent::UserPrompt { .. } => "UserPrompt",
            AgentEvent::TurnStart { .. } => "TurnStart",
            AgentEvent::TextDelta { .. } => "TextDelta",
            AgentEvent::Message { .. } => "Message",
            AgentEvent::ToolCall { .. } => "ToolCall",
            AgentEvent::ToolResult { .. } => "ToolResult",
            AgentEvent::SubagentSpawn { .. } => "SubagentSpawn",
            AgentEvent::SubagentDone { .. } => "SubagentDone",
            AgentEvent::AgentMessage { .. } => "AgentMessage",
            AgentEvent::Compaction { .. } => "Compaction",
            AgentEvent::WorkflowPhase { .. } => "WorkflowPhase",
            AgentEvent::WorkflowLog { .. } => "WorkflowLog",
            AgentEvent::ContextWarning { .. } => "ContextWarning",
            AgentEvent::Completion { .. } => "Completion",
            AgentEvent::TurnEnd { .. } => "TurnEnd",
            AgentEvent::Error { .. } => "Error",
            AgentEvent::Unknown => "Unknown",
        }
    }
}

pub type EventListener = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

/// Tiny synchronous pub/sub. The core owns one; UIs attach listeners.
#[derive(Clone, Default)]
pub struct EventBus {
    listeners: Arc<Mutex<Vec<EventListener>>>,
}

impl EventBus {
    pub fn new() -> Self {
        EventBus::default()
    }

    pub fn on(&self, listener: EventListener) {
        self.listeners.lock().unwrap().push(listener);
    }

    pub fn emit(&self, event: AgentEvent) {
        // Clone the handles out of the lock BEFORE invoking any of them. A listener
        // is free to touch the bus (subscribe, or emit a follow-on event) while it
        // runs; holding the mutex across the callbacks would deadlock on the first
        // such reentrant call and serialize unrelated emits behind slow listeners.
        let listeners = {
            let guard = self.listeners.lock().unwrap();
            guard.clone()
        };
        for l in listeners.iter() {
            l(&event);
        }
    }
}
