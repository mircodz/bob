//! The retained view model. Agent events mutate a list of cells; the draw loop
//! turns cells into ratatui Lines each frame. This is the TUI's equivalent of
//! the CLI renderer — a pure subscriber to the event stream.

use bob_core::core::events::AgentEvent;
use bob_core::core::types::{ContentBlock, Message, Role};
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
}

pub enum Cell {
    /// Something the user typed.
    User(String),
    /// Streamed assistant prose (markdown). `open` while still streaming.
    Assistant { text: String, open: bool },
    /// A tool invocation and its (eventual) result.
    Tool {
        id: String,
        name: String,
        input: Value,
        status: ToolStatus,
        output: Option<String>,
    },
    /// A subagent spawn notice, with a running count of tools it has called.
    Subagent {
        agent_id: String,
        task: String,
        tools: usize,
        done: bool,
    },
    /// A compaction notice.
    Compaction { before: usize, after: usize },
    /// End-of-turn usage line.
    Usage {
        input: u64,
        output: u64,
        cached: u64,
    },
    /// A generic dim notice (startup notices, errors).
    Notice(String),
    /// A system event surfaced inline as a bulleted line (model/mode switches),
    /// Cortex-style: "• Model changed to gpt-5.5 medium".
    Event(String),
}

#[derive(Default)]
pub struct ViewModel {
    pub cells: Vec<Cell>,
    /// Whether the agent is currently working (drives the spinner + input lock).
    pub busy: bool,
}

impl ViewModel {
    pub fn new() -> Self {
        ViewModel::default()
    }

    pub fn push_user(&mut self, text: String) {
        self.cells.push(Cell::User(text));
    }

    pub fn push_notice(&mut self, text: String) {
        self.cells.push(Cell::Notice(text));
    }

    /// Push an inline system-event line (model/mode switch), rendered with a
    /// bullet like a tool cell.
    pub fn push_event(&mut self, text: String) {
        self.cells.push(Cell::Event(text));
    }

    /// Rebuild the scrollback from a stored message history (on `--resume`).
    /// Tool results are matched back to their tool_use cell by id.
    pub fn hydrate(&mut self, messages: &[Message]) {
        for m in messages {
            match m.role {
                Role::User => {
                    // Skip synthetic compaction-summary messages.
                    let text = m.text();
                    if text.starts_with("[conversation summary]") {
                        self.cells.push(Cell::Compaction { before: 0, after: 0 });
                        continue;
                    }
                    // A user turn may carry tool_results (role=tool is stored as
                    // its own message, but be defensive).
                    let mut had_text = false;
                    for b in &m.content {
                        if let ContentBlock::Text { text } = b {
                            if !text.is_empty() {
                                self.cells.push(Cell::User(text.clone()));
                                had_text = true;
                            }
                        }
                    }
                    let _ = had_text;
                }
                Role::Assistant => {
                    for b in &m.content {
                        match b {
                            ContentBlock::Text { text } if !text.is_empty() => {
                                self.cells.push(Cell::Assistant {
                                    text: text.clone(),
                                    open: false,
                                });
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                self.cells.push(Cell::Tool {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                    status: ToolStatus::Ok,
                                    output: None,
                                });
                            }
                            _ => {}
                        }
                    }
                }
                Role::Tool => {
                    for b in &m.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = b
                        {
                            if let Some(Cell::Tool { status, output, .. }) =
                                self.find_tool(tool_use_id)
                            {
                                *status = if is_error.unwrap_or(false) {
                                    ToolStatus::Error
                                } else {
                                    ToolStatus::Ok
                                };
                                *output = Some(content.clone());
                            }
                        }
                    }
                }
                Role::System => {}
            }
        }
    }

    /// Index of the currently-open assistant cell, if any.
    fn open_assistant(&mut self) -> Option<&mut Cell> {
        match self.cells.last_mut() {
            Some(c @ Cell::Assistant { .. }) => match c {
                Cell::Assistant { open, .. } if *open => Some(c),
                _ => None,
            },
            _ => None,
        }
    }

    fn find_tool(&mut self, id: &str) -> Option<&mut Cell> {
        self.cells.iter_mut().rev().find(|c| matches!(c, Cell::Tool { id: tid, .. } if tid == id))
    }

    fn find_subagent(&mut self, id: &str) -> Option<&mut Cell> {
        self.cells
            .iter_mut()
            .rev()
            .find(|c| matches!(c, Cell::Subagent { agent_id, .. } if agent_id == id))
    }

    /// Apply one agent event to the model.
    pub fn apply(&mut self, event: &AgentEvent) {
        // Events from spawned subagents (agent_id like "task_1") only update the
        // count/done state of their Subagent cell — their inner chatter is never
        // rendered as its own cells.
        if let Some(id) = subagent_id(event) {
            match event {
                AgentEvent::ToolCall { .. } => {
                    if let Some(Cell::Subagent { tools, .. }) = self.find_subagent(id) {
                        *tools += 1;
                    }
                }
                AgentEvent::TurnEnd { .. } => {
                    if let Some(Cell::Subagent { done, .. }) = self.find_subagent(id) {
                        *done = true;
                    }
                }
                _ => {}
            }
            return;
        }

        match event {
            AgentEvent::TurnStart { .. } => {
                self.busy = true;
            }
            AgentEvent::TextDelta { text, .. } => {
                if let Some(Cell::Assistant { text: buf, .. }) = self.open_assistant() {
                    buf.push_str(text);
                } else {
                    self.cells.push(Cell::Assistant {
                        text: text.clone(),
                        open: true,
                    });
                }
            }
            AgentEvent::Message { .. } => {
                // Close any open assistant cell; tool calls / next turn start fresh.
                if let Some(Cell::Assistant { open, .. }) = self.open_assistant() {
                    *open = false;
                }
            }
            AgentEvent::ToolCall {
                tool_use_id,
                name,
                input,
                ..
            } => {
                if let Some(Cell::Assistant { open, .. }) = self.open_assistant() {
                    *open = false;
                }
                self.cells.push(Cell::Tool {
                    id: tool_use_id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    status: ToolStatus::Running,
                    output: None,
                });
            }
            AgentEvent::ToolResult {
                tool_use_id,
                output,
                is_error,
                ..
            } => {
                if let Some(Cell::Tool { status, output: o, .. }) = self.find_tool(tool_use_id) {
                    *status = if *is_error { ToolStatus::Error } else { ToolStatus::Ok };
                    *o = Some(output.clone());
                }
            }
            AgentEvent::SubagentSpawn { agent_id, task, .. } => {
                self.cells.push(Cell::Subagent {
                    agent_id: agent_id.clone(),
                    task: task.clone(),
                    tools: 0,
                    done: false,
                });
            }
            AgentEvent::Compaction {
                before_tokens,
                after_tokens,
                ..
            } => {
                self.cells.push(Cell::Compaction {
                    before: *before_tokens,
                    after: *after_tokens,
                });
            }
            AgentEvent::TurnEnd { usage, .. } => {
                if let Some(Cell::Assistant { open, .. }) = self.open_assistant() {
                    *open = false;
                }
                self.cells.push(Cell::Usage {
                    input: usage.input_tokens,
                    output: usage.output_tokens,
                    cached: usage.cache_read_input_tokens,
                });
                self.busy = false;
            }
            AgentEvent::Error { message, .. } => {
                self.cells.push(Cell::Notice(format!("error: {}", message)));
                self.busy = false;
            }
            // Usage accounting is handled by the run-loop, not the view.
            AgentEvent::Completion { .. } => {}
        }
    }
}

/// If an event comes from a spawned subagent (agent_id starts with "task_"),
/// return that id; otherwise None (it's the root agent's own event).
fn subagent_id(event: &AgentEvent) -> Option<&str> {
    let id = match event {
        AgentEvent::TurnStart { agent_id }
        | AgentEvent::TextDelta { agent_id, .. }
        | AgentEvent::Message { agent_id, .. }
        | AgentEvent::ToolCall { agent_id, .. }
        | AgentEvent::ToolResult { agent_id, .. }
        | AgentEvent::Compaction { agent_id, .. }
        | AgentEvent::TurnEnd { agent_id, .. }
        | AgentEvent::Completion { agent_id, .. }
        | AgentEvent::Error { agent_id, .. } => agent_id.as_str(),
        AgentEvent::SubagentSpawn { .. } => return None,
    };
    if id.starts_with("task_") {
        Some(id)
    } else {
        None
    }
}
