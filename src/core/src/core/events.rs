//! Every observable thing the agent does is one of these events. This is THE
//! seam between the core and any UI. The core never writes to stdout directly;
//! a frontend subscribes to this stream and renders it however it likes.

use crate::core::types::{Message, Usage};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub enum AgentEvent {
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
