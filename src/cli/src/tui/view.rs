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
        /// Whether the user has clicked to expand this cell's output to its full
        /// text (default false = short preview). Toggled by clicking the cell.
        expanded: bool,
    },
    /// A subagent spawn notice, with a running count of tools it has called.
    Subagent {
        agent_id: String,
        /// Who spawned it ("root" or another agent's name), for nesting depth.
        parent_id: String,
        task: String,
        tools: usize,
        done: bool,
        failed: bool,
    },
    /// A compaction notice. `done: false` renders as a live "Compacting…"
    /// indicator; once the summary returns it flips to `done: true` ("Compacted").
    Compaction {
        before: usize,
        after: usize,
        done: bool,
    },
    /// A generic dim notice (startup notices, errors).
    Notice(String),
    /// A system event surfaced inline as a bulleted line (model/mode switches),
    /// e.g. "• Model changed to gpt-5.5 medium".
    Event(String),
    /// A message delivered to/from an agent (shown only in the team drawer's
    /// per-agent threads). `from` is the sender ("root", "user", or an agent name).
    AgentMsg { from: String, text: String },
}

impl Cell {
    /// A content fingerprint used to cache a cell's rendered lines. Two cells
    /// with the same fingerprint render identically, so the draw loop can reuse
    /// cached Lines instead of re-running markdown/syntax highlighting every
    /// frame. Only fields that affect rendering are hashed.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::mem::discriminant(self).hash(&mut h);
        match self {
            Cell::User(t) => t.hash(&mut h),
            Cell::Assistant { text, open } => {
                text.hash(&mut h);
                open.hash(&mut h);
            }
            Cell::Tool {
                name,
                input,
                status,
                output,
                expanded,
                ..
            } => {
                name.hash(&mut h);
                // Value isn't Hash; its stable string form is good enough.
                input.to_string().hash(&mut h);
                (*status as u8).hash(&mut h);
                output.hash(&mut h);
                expanded.hash(&mut h);
            }
            Cell::Subagent {
                agent_id,
                parent_id,
                task,
                tools,
                done,
                failed,
            } => {
                agent_id.hash(&mut h);
                parent_id.hash(&mut h);
                task.hash(&mut h);
                tools.hash(&mut h);
                done.hash(&mut h);
                failed.hash(&mut h);
            }
            Cell::Compaction {
                before,
                after,
                done,
            } => {
                before.hash(&mut h);
                after.hash(&mut h);
                done.hash(&mut h);
            }
            Cell::Notice(t) | Cell::Event(t) => t.hash(&mut h),
            Cell::AgentMsg { from, text } => {
                from.hash(&mut h);
                text.hash(&mut h);
            }
        }
        h.finish()
    }
}

/// The index of a currently-open (streaming) assistant cell at the tail of
/// `cells`, if any. Shared by the main ViewModel and per-agent threads.
fn open_assistant_in(cells: &mut [Cell]) -> Option<&mut Cell> {
    match cells.last_mut() {
        Some(c @ Cell::Assistant { .. }) => match c {
            Cell::Assistant { open, .. } if *open => Some(c),
            _ => None,
        },
        _ => None,
    }
}

/// The most recent Tool cell with the given tool_use_id, if any.
fn find_tool_in<'a>(cells: &'a mut [Cell], id: &str) -> Option<&'a mut Cell> {
    cells
        .iter_mut()
        .rev()
        .find(|c| matches!(c, Cell::Tool { id: tid, .. } if tid == id))
}

/// Apply one agent event to a bare list of content cells — the reduction shared
/// by the main transcript and each per-agent thread. Handles the *content*
/// events (text, tool call/result, compaction, error, inter-agent messages);
/// callers layer their own handling for spawn cells / busy state on top.
///
/// `include_messages` controls whether `AgentMessage` events append an `AgentMsg`
/// cell: true for per-agent threads (where the conversation should show), false
/// for the main transcript (which keeps coordination chatter internal).
pub fn apply_content_event(cells: &mut Vec<Cell>, event: &AgentEvent, include_messages: bool) {
    match event {
        AgentEvent::TextDelta { text, .. } => {
            if let Some(Cell::Assistant { text: buf, .. }) = open_assistant_in(cells) {
                buf.push_str(text);
            } else {
                cells.push(Cell::Assistant {
                    text: text.clone(),
                    open: true,
                });
            }
        }
        AgentEvent::Message { .. } => {
            if let Some(Cell::Assistant { open, .. }) = open_assistant_in(cells) {
                *open = false;
            }
        }
        AgentEvent::ToolCall {
            tool_use_id,
            name,
            input,
            ..
        } => {
            if let Some(Cell::Assistant { open, .. }) = open_assistant_in(cells) {
                *open = false;
            }
            cells.push(Cell::Tool {
                id: tool_use_id.clone(),
                name: name.clone(),
                input: input.clone(),
                status: ToolStatus::Running,
                output: None,
                expanded: false,
            });
        }
        AgentEvent::ToolResult {
            tool_use_id,
            output,
            is_error,
            ..
        } => {
            if let Some(Cell::Tool {
                status, output: o, ..
            }) = find_tool_in(cells, tool_use_id)
            {
                *status = if *is_error {
                    ToolStatus::Error
                } else {
                    ToolStatus::Ok
                };
                *o = Some(output.clone());
            }
        }
        AgentEvent::Compaction {
            before_tokens,
            after_tokens,
            ..
        } => {
            cells.push(Cell::Compaction {
                before: *before_tokens,
                after: *after_tokens,
                done: true,
            });
        }
        AgentEvent::ContextWarning {
            used_tokens,
            context_window,
            pct,
            ..
        } => {
            let window_k = (*context_window as f64 / 1000.0).round() as usize;
            let used_k = (*used_tokens as f64 / 1000.0).round() as usize;
            cells.push(Cell::Notice(format!(
                "context {}% full (~{}k/{}k) — will auto-compact soon; /compact to summarize now",
                pct, used_k, window_k
            )));
        }
        AgentEvent::Error { message, .. } => {
            cells.push(Cell::Notice(format!("error: {}", message)));
        }
        AgentEvent::AgentMessage { from, text, .. } if include_messages => {
            cells.push(Cell::AgentMsg {
                from: from.clone(),
                text: text.clone(),
            });
        }
        _ => {}
    }
}

#[derive(Default)]
pub struct ViewModel {
    pub cells: Vec<Cell>,
    /// Whether the agent is currently working (drives the spinner + input lock).
    pub busy: bool,
    /// Monotonic counter bumped whenever `cells` changes. The renderer compares it
    /// to decide whether to rebuild its flattened line cache — a pure scroll (which
    /// never mutates the view) leaves it untouched, so scrolling does no rebuild
    /// work regardless of transcript length.
    pub revision: u64,
}

impl ViewModel {
    pub fn new() -> Self {
        ViewModel::default()
    }

    pub fn push_user(&mut self, text: String) {
        self.cells.push(Cell::User(text));
        self.revision += 1;
    }

    pub fn push_notice(&mut self, text: String) {
        self.cells.push(Cell::Notice(text));
        self.revision += 1;
    }

    /// Push a live "Compacting…" indicator and return its cell index, so the
    /// caller can flip it to "Compacted" via `finish_compaction` when the
    /// summarization network call returns.
    pub fn begin_compaction(&mut self) -> usize {
        self.cells.push(Cell::Compaction {
            before: 0,
            after: 0,
            done: false,
        });
        self.revision += 1;
        self.cells.len() - 1
    }

    /// Flip the live compaction cell at `idx` to its finished state, recording the
    /// before/after token estimate. If nothing was compacted, drop the indicator.
    pub fn finish_compaction(&mut self, idx: usize, before: usize, after: usize, did: bool) {
        if let Some(cell) = self.cells.get_mut(idx) {
            if did {
                *cell = Cell::Compaction {
                    before,
                    after,
                    done: true,
                };
            } else {
                *cell = Cell::Notice("nothing to compact yet.".into());
            }
        }
        self.revision += 1;
    }

    /// Push an inline system-event line (model/mode switch), rendered with a
    /// bullet like a tool cell.
    pub fn push_event(&mut self, text: String) {
        self.cells.push(Cell::Event(text));
        self.revision += 1;
    }

    /// Clear the whole transcript (the `/clear` command).
    pub fn clear(&mut self) {
        self.cells.clear();
        self.revision += 1;
    }

    /// Toggle a tool cell's expanded/collapsed output at `idx`. Returns true if a
    /// tool cell was actually toggled. Going through this method (rather than poking
    /// `cells` directly) guarantees the render-cache revision is bumped, so the
    /// "forgot to bump → stale render" bug can't happen.
    pub fn toggle_tool_expanded(&mut self, idx: usize) -> bool {
        if let Some(Cell::Tool { expanded, .. }) = self.cells.get_mut(idx) {
            *expanded = !*expanded;
            self.revision += 1;
            true
        } else {
            false
        }
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
                        self.cells.push(Cell::Compaction {
                            before: 0,
                            after: 0,
                            done: true,
                        });
                        continue;
                    }
                    // Skip inter-agent coordination messages folded into history
                    // — they're internal, not user turns. (Shared marker so this
                    // can't drift from the injector; see agent::team.)
                    if bob_core::agent::team::is_coord_message(&text) {
                        continue;
                    }
                    // A user turn may carry tool_results (role=tool is stored as
                    // its own message, but be defensive).
                    for b in &m.content {
                        if let ContentBlock::Text { text } = b {
                            if !text.is_empty() {
                                self.cells.push(Cell::User(text.clone()));
                            }
                        }
                    }
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
                                    expanded: false,
                                });
                                // Subagent tree cells come from live SubagentSpawn
                                // *events*, which aren't in the message history. On
                                // resume, reconstruct them from the tool's input so
                                // spawned agents still show after a reload.
                                self.hydrate_subagents(name, input);
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
        self.revision += 1;
    }

    /// Reconstruct Subagent tree cells from a persisted `task`/`spawn_agent` tool
    /// call, so spawned agents still appear after a session is resumed. They're
    /// marked done (the work is in the past) with an unknown tool count.
    fn hydrate_subagents(&mut self, name: &str, input: &Value) {
        let push = |cells: &mut Vec<Cell>, parent: &str, task: &str| {
            cells.push(Cell::Subagent {
                agent_id: String::new(),
                parent_id: parent.to_string(),
                task: task.to_string(),
                tools: 0,
                done: true,
                failed: false,
            });
        };
        match name {
            "task" => {
                if let Some(tasks) = input.get("tasks").and_then(|t| t.as_array()) {
                    for t in tasks {
                        let desc = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
                        push(&mut self.cells, "root", desc);
                    }
                }
            }
            "spawn_agent" => {
                let desc = input.get("task").and_then(|t| t.as_str()).unwrap_or("");
                push(&mut self.cells, "root", desc);
            }
            _ => {}
        }
    }

    fn find_tool(&mut self, id: &str) -> Option<&mut Cell> {
        self.cells
            .iter_mut()
            .rev()
            .find(|c| matches!(c, Cell::Tool { id: tid, .. } if tid == id))
    }

    fn find_subagent(&mut self, id: &str) -> Option<&mut Cell> {
        self.cells
            .iter_mut()
            .rev()
            .find(|c| matches!(c, Cell::Subagent { agent_id, .. } if agent_id == id))
    }

    /// Apply one agent event to the model.
    pub fn apply(&mut self, event: &AgentEvent) {
        // Any applied event may mutate cells (append, or update a tool/subagent
        // cell in place). Bump the revision so the renderer rebuilds its line cache;
        // this is per-EVENT, not per-frame, so scrolling stays free.
        self.revision += 1;
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
            AgentEvent::SubagentSpawn {
                agent_id,
                parent_id,
                task,
                ..
            } => {
                self.cells.push(Cell::Subagent {
                    agent_id: agent_id.clone(),
                    parent_id: parent_id.clone(),
                    task: task.clone(),
                    tools: 0,
                    done: false,
                    failed: false,
                });
            }
            AgentEvent::SubagentDone { agent_id, failed } => {
                if let Some(Cell::Subagent {
                    done, failed: f, ..
                }) = self.find_subagent(agent_id)
                {
                    *done = true;
                    *f = *failed;
                }
            }
            AgentEvent::TurnEnd { .. } => {
                if let Some(Cell::Assistant { open, .. }) = open_assistant_in(&mut self.cells) {
                    *open = false;
                }
                // Per-turn token counts are intentionally not rendered as a cell —
                // they're noise in the transcript. Session + all-time totals live
                // in the status bar and the /usage command.
                self.busy = false;
            }
            AgentEvent::Error { .. } => {
                apply_content_event(&mut self.cells, event, false);
                self.busy = false;
            }
            // Usage accounting is handled by the run-loop, not the view.
            AgentEvent::Completion { .. } => {}
            // Inter-agent coordination chatter is internal. In the MAIN transcript
            // we only surface when the root agent SENDS a message to a subagent, as
            // a simple one-liner — we don't care about the precise content (it's the
            // agent's own output) or messages between subagents.
            AgentEvent::AgentMessage { to, from, .. } if from == "root" => {
                self.cells
                    .push(Cell::Event(format!("Sent a message to {}", to)));
            }
            // All other content events reduce via the shared helper (messages off).
            _ => apply_content_event(&mut self.cells, event, false),
        }
    }
}

/// If an event comes from any spawned agent (agent_id other than "root"), return
/// that id; otherwise None (it's the root agent's own event). This covers both
/// `task_*` subagents and named coordinated agents — none of their inner activity
/// belongs in the main transcript.
fn subagent_id(event: &AgentEvent) -> Option<&str> {
    let id = match event {
        AgentEvent::TurnStart { agent_id }
        | AgentEvent::TextDelta { agent_id, .. }
        | AgentEvent::Message { agent_id, .. }
        | AgentEvent::ToolCall { agent_id, .. }
        | AgentEvent::ToolResult { agent_id, .. }
        | AgentEvent::Compaction { agent_id, .. }
        | AgentEvent::ContextWarning { agent_id, .. }
        | AgentEvent::TurnEnd { agent_id, .. }
        | AgentEvent::Completion { agent_id, .. }
        | AgentEvent::Error { agent_id, .. } => agent_id.as_str(),
        AgentEvent::SubagentSpawn { .. } => return None,
        AgentEvent::SubagentDone { .. } => return None,
        AgentEvent::AgentMessage { .. } => return None,
    };
    if id != "root" {
        Some(id)
    } else {
        None
    }
}
