//! Per-agent transcript capture + team-drawer state. Every spawned agent gets an
//! [`AgentThread`] — a mini transcript built from the same event stream and the
//! same [`view::Cell`] reduction as the main view, so the drawer can show an
//! agent's full live activity and (via the coordination team) let you message it.

use super::view::{apply_content_event, Cell, ToolStatus};
use bob_core::core::events::AgentEvent;
use bob_core::core::session::{PersistedCell, PersistedThread};

/// One spawned agent's live thread: who it is, its status, and its transcript
/// (streamed text + tool calls + messages to/from it).
pub struct AgentThread {
    pub name: String,
    /// Who spawned it ("root" or another agent's name).
    pub parent_id: String,
    /// Short task/description label from the spawn event.
    pub task: String,
    pub status: ThreadStatus,
    pub cells: Vec<Cell>,
    /// Content cells appended since the drawer last showed this thread.
    pub unread: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadStatus {
    Running,
    Done,
    Failed,
}

impl AgentThread {
    fn new(name: String, parent_id: String, task: String) -> Self {
        AgentThread {
            name,
            parent_id,
            task,
            status: ThreadStatus::Running,
            cells: Vec::new(),
            unread: 0,
        }
    }
}

/// The set of per-agent threads, keyed by agent id (== the agent's team name for
/// `spawn_agent` children; `task_N` for `task`-spawned ones). Fed by the event
/// loop with every subagent event that the main transcript drops.
#[derive(Default)]
pub struct AgentTranscripts {
    /// Insertion order of agent ids, so the drawer roster is stable.
    order: Vec<String>,
    threads: std::collections::HashMap<String, AgentThread>,
}

impl AgentTranscripts {
    pub fn new() -> Self {
        AgentTranscripts::default()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// Ids in DISPLAY order: still-running agents first (in spawn order), then
    /// finished ones (done/failed) at the bottom. The drawer navigates and renders
    /// through this so selection stays aligned with what's shown.
    pub fn display_order(&self) -> Vec<String> {
        let mut running: Vec<String> = Vec::new();
        let mut finished: Vec<String> = Vec::new();
        for id in &self.order {
            match self.threads.get(id).map(|t| t.status) {
                Some(ThreadStatus::Running) => running.push(id.clone()),
                Some(_) => finished.push(id.clone()),
                None => {}
            }
        }
        running.extend(finished);
        running
    }

    /// Nesting depth of an agent (root's direct children = 0). Walks `parent_id`
    /// up the chain; capped to avoid a cycle hang.
    pub fn depth_of(&self, id: &str) -> usize {
        let mut depth = 0;
        let mut cur = id.to_string();
        for _ in 0..16 {
            let Some(t) = self.threads.get(&cur) else {
                break;
            };
            if t.parent_id.is_empty() || t.parent_id == "root" {
                break;
            }
            // Only count parents that are themselves tracked agents.
            if !self.threads.contains_key(&t.parent_id) {
                break;
            }
            depth += 1;
            cur = t.parent_id.clone();
        }
        depth
    }

    pub fn get(&self, id: &str) -> Option<&AgentThread> {
        self.threads.get(id)
    }

    /// Clear the unread counter for an agent (called when the drawer shows it).
    pub fn mark_read(&mut self, id: &str) {
        if let Some(t) = self.threads.get_mut(id) {
            t.unread = 0;
        }
    }

    fn ensure(&mut self, id: &str) -> &mut AgentThread {
        if !self.threads.contains_key(id) {
            self.order.push(id.to_string());
            self.threads.insert(
                id.to_string(),
                AgentThread::new(id.to_string(), String::new(), String::new()),
            );
        }
        self.threads.get_mut(id).unwrap()
    }

    /// Record a spawn: register the thread with its label + parent up front so it
    /// appears in the roster even before it emits any content. `prompt` is the
    /// full instruction the parent gave; it's recorded as the first message cell
    /// so the drawer shows exactly what was delegated (from the parent).
    pub fn on_spawn(&mut self, agent_id: &str, parent_id: &str, task: &str, prompt: &str) {
        let already = self.threads.contains_key(agent_id);
        let t = self.ensure(agent_id);
        t.parent_id = parent_id.to_string();
        t.task = task.to_string();
        // Seed the transcript with the delegated instructions once, rendered the
        // same way as user input (a `Cell::User` band) so it reads like the opening
        // prompt of the conversation.
        if !already && !prompt.trim().is_empty() {
            t.cells.push(Cell::User(prompt.to_string()));
        }
    }

    /// Record completion, setting the final status.
    pub fn on_done(&mut self, agent_id: &str, failed: bool) {
        let t = self.ensure(agent_id);
        t.status = if failed {
            ThreadStatus::Failed
        } else {
            ThreadStatus::Done
        };
    }

    /// Append a message line to an agent's thread (chat to/from it). `showing` is
    /// the id the drawer is currently displaying, so unread isn't bumped for it.
    pub fn push_message(&mut self, agent_id: &str, from: &str, text: &str, showing: Option<&str>) {
        let is_showing = showing == Some(agent_id);
        let t = self.ensure(agent_id);
        t.cells.push(Cell::AgentMsg {
            from: from.to_string(),
            text: text.to_string(),
        });
        if !is_showing {
            t.unread += 1;
        }
    }

    /// Feed one subagent event into its thread. `showing` is the id the drawer is
    /// currently displaying (its unread counter isn't bumped). Returns the agent
    /// id the event was routed to, if any.
    pub fn apply(&mut self, event: &AgentEvent, showing: Option<&str>) {
        match event {
            AgentEvent::SubagentSpawn {
                agent_id,
                parent_id,
                task,
                prompt,
            } => {
                self.on_spawn(agent_id, parent_id, task, prompt);
            }
            AgentEvent::SubagentDone { agent_id, failed } => {
                self.on_done(agent_id, *failed);
            }
            AgentEvent::AgentMessage { .. } => {
                // Inter-agent messages are NOT shown in the drawer: an agent's
                // outgoing message just duplicates its own final assistant output,
                // so the line is redundant. (A message YOU send via the drawer is
                // appended directly by send_drawer_message, not through here.)
            }
            _ => {
                if let Some(id) = event_agent_id(event) {
                    if id == "root" {
                        return;
                    }
                    let is_showing = showing == Some(id);
                    let before;
                    let after;
                    {
                        let t = self.ensure(id);
                        before = t.cells.len();
                        apply_content_event(&mut t.cells, event, true);
                        after = t.cells.len();
                    }
                    if !is_showing && after > before {
                        if let Some(t) = self.threads.get_mut(id) {
                            t.unread += after - before;
                        }
                    }
                }
            }
        }
    }

    /// Serialize every thread for session persistence.
    pub fn to_persisted(&self) -> Vec<PersistedThread> {
        self.order
            .iter()
            .filter_map(|id| self.threads.get(id))
            .map(|t| PersistedThread {
                id: t.name.clone(),
                name: t.name.clone(),
                parent_id: t.parent_id.clone(),
                task: t.task.clone(),
                status: match t.status {
                    ThreadStatus::Running => "running",
                    ThreadStatus::Done => "done",
                    ThreadStatus::Failed => "failed",
                }
                .to_string(),
                cells: t.cells.iter().filter_map(cell_to_persisted).collect(),
            })
            .collect()
    }

    /// Rebuild threads from persisted state (on resume). Restored threads that
    /// were still "running" at save time are marked done — the work is over.
    pub fn from_persisted(threads: &[PersistedThread]) -> Self {
        let mut out = AgentTranscripts::new();
        for pt in threads {
            let status = match pt.status.as_str() {
                "failed" => ThreadStatus::Failed,
                _ => ThreadStatus::Done,
            };
            let thread = AgentThread {
                name: pt.name.clone(),
                parent_id: pt.parent_id.clone(),
                task: pt.task.clone(),
                status,
                cells: pt.cells.iter().map(cell_from_persisted).collect(),
                unread: 0,
            };
            out.order.push(pt.id.clone());
            out.threads.insert(pt.id.clone(), thread);
        }
        out
    }
}

/// Map a live `Cell` to its persisted form. Returns None for cells the drawer
/// never produces in a thread (Subagent/Compaction/Event).
fn cell_to_persisted(cell: &Cell) -> Option<PersistedCell> {
    match cell {
        Cell::User(text) => Some(PersistedCell::User { text: text.clone() }),
        Cell::Assistant { text, .. } => Some(PersistedCell::Assistant { text: text.clone() }),
        Cell::Tool {
            name,
            input,
            output,
            status,
            ..
        } => Some(PersistedCell::Tool {
            name: name.clone(),
            input: input.clone(),
            output: output.clone().unwrap_or_default(),
            is_error: matches!(status, ToolStatus::Error),
        }),
        Cell::AgentMsg { from, text } => Some(PersistedCell::Message {
            from: from.clone(),
            text: text.clone(),
        }),
        Cell::Notice(text) => Some(PersistedCell::Notice { text: text.clone() }),
        _ => None,
    }
}

/// Rebuild a live `Cell` from its persisted form.
fn cell_from_persisted(pc: &PersistedCell) -> Cell {
    match pc {
        PersistedCell::User { text } => Cell::User(text.clone()),
        PersistedCell::Assistant { text } => Cell::Assistant {
            text: text.clone(),
            open: false,
        },
        PersistedCell::Tool {
            name,
            input,
            output,
            is_error,
        } => Cell::Tool {
            id: String::new(),
            name: name.clone(),
            input: input.clone(),
            status: if *is_error {
                ToolStatus::Error
            } else {
                ToolStatus::Ok
            },
            output: Some(output.clone()),
            expanded: false,
        },
        PersistedCell::Message { from, text } => Cell::AgentMsg {
            from: from.clone(),
            text: text.clone(),
        },
        PersistedCell::Notice { text } => Cell::Notice(text.clone()),
    }
}

/// The agent id carried by a content event (None for spawn/done/message, which
/// carry named fields handled separately).
fn event_agent_id(event: &AgentEvent) -> Option<&str> {
    match event {
        AgentEvent::TurnStart { agent_id }
        | AgentEvent::TextDelta { agent_id, .. }
        | AgentEvent::Message { agent_id, .. }
        | AgentEvent::ToolCall { agent_id, .. }
        | AgentEvent::ToolResult { agent_id, .. }
        | AgentEvent::Compaction { agent_id, .. }
        | AgentEvent::TurnEnd { agent_id, .. }
        | AgentEvent::Completion { agent_id, .. }
        | AgentEvent::Error { agent_id, .. } => Some(agent_id.as_str()),
        AgentEvent::SubagentSpawn { .. }
        | AgentEvent::SubagentDone { .. }
        | AgentEvent::AgentMessage { .. } => None,
    }
}

/// Drawer UI state: which agent is selected, scroll offset, and (when chatting)
/// the compose buffer. `None` on the App means the drawer is closed.
pub struct TeamDrawer {
    pub selected: usize,
    pub scroll: u16,
    /// Roster index the mouse is currently hovering (for a hover highlight), or
    /// `None` when the cursor isn't over a roster row.
    pub hovered: Option<usize>,
    /// `Some` while composing a message to the selected agent.
    pub composing: Option<String>,
}

impl Default for TeamDrawer {
    fn default() -> Self {
        TeamDrawer {
            selected: 0,
            scroll: 0,
            hovered: None,
            composing: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bob_core::core::types::Usage;

    fn tool_call(agent: &str, id: &str, name: &str) -> AgentEvent {
        AgentEvent::ToolCall {
            agent_id: agent.into(),
            tool_use_id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        }
    }

    #[test]
    fn builds_thread_from_event_stream() {
        let mut t = AgentTranscripts::new();
        t.apply(
            &AgentEvent::SubagentSpawn {
                parent_id: "root".into(),
                agent_id: "reviewer".into(),
                task: "review".into(),
                prompt: String::new(),
            },
            None,
        );
        t.apply(&tool_call("reviewer", "t1", "grep"), None);
        t.apply(
            &AgentEvent::TextDelta {
                agent_id: "reviewer".into(),
                text: "hello".into(),
            },
            None,
        );
        t.apply(
            &AgentEvent::SubagentDone {
                agent_id: "reviewer".into(),
                failed: false,
            },
            None,
        );

        let thread = t.get("reviewer").unwrap();
        assert_eq!(thread.task, "review");
        assert_eq!(thread.status, ThreadStatus::Done);
        // one Tool cell + one Assistant cell
        assert_eq!(thread.cells.len(), 2);
        assert!(thread.unread >= 2);
    }

    #[test]
    fn root_events_are_ignored() {
        let mut t = AgentTranscripts::new();
        t.apply(&tool_call("root", "t1", "bash"), None);
        assert!(t.is_empty());
    }

    #[test]
    fn inter_agent_message_adds_no_drawer_cells() {
        // Inter-agent messages are redundant with the sender's own output, so the
        // drawer must NOT append cells for them (only user-sent messages, via
        // send_drawer_message, appear).
        let mut t = AgentTranscripts::new();
        t.on_spawn("a", "root", "task a", "");
        t.on_spawn("b", "root", "task b", "");
        t.apply(
            &AgentEvent::AgentMessage {
                to: "b".into(),
                from: "a".into(),
                text: "check X".into(),
            },
            None,
        );
        assert_eq!(t.get("b").unwrap().cells.len(), 0);
        assert_eq!(t.get("a").unwrap().cells.len(), 0);
    }

    #[test]
    fn showing_thread_does_not_accrue_unread() {
        let mut t = AgentTranscripts::new();
        t.on_spawn("a", "root", "task", "");
        t.apply(
            &AgentEvent::TextDelta {
                agent_id: "a".into(),
                text: "hi".into(),
            },
            Some("a"),
        );
        assert_eq!(t.get("a").unwrap().unread, 0);
    }

    #[test]
    fn spawn_prompt_seeds_first_message_cell() {
        let mut t = AgentTranscripts::new();
        t.on_spawn("a", "root", "review", "look at src/foo.rs and report bugs");
        let cells = &t.get("a").unwrap().cells;
        assert_eq!(cells.len(), 1);
        match &cells[0] {
            Cell::User(text) => assert_eq!(text, "look at src/foo.rs and report bugs"),
            _ => panic!("expected a User cell"),
        }
        // A duplicate spawn event must not seed a second copy.
        t.on_spawn("a", "root", "review", "look at src/foo.rs and report bugs");
        assert_eq!(t.get("a").unwrap().cells.len(), 1);
    }

    #[test]
    fn completion_event_is_noop_content() {
        let mut t = AgentTranscripts::new();
        t.on_spawn("a", "root", "task", "");
        t.apply(
            &AgentEvent::Completion {
                agent_id: "a".into(),
                model: "m".into(),
                usage: Usage::default(),
            },
            None,
        );
        assert_eq!(t.get("a").unwrap().cells.len(), 0);
    }
}
