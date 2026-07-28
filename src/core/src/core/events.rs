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
    },
    Compaction {
        agent_id: String,
        before_tokens: usize,
        after_tokens: usize,
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
        let listeners = self.listeners.lock().unwrap();
        for l in listeners.iter() {
            l(&event);
        }
    }
}
