use super::view::{apply_content_event, finish_retries, Cell, ToolStatus};
use bob_core::core::events::AgentEvent;
use bob_core::core::session::{PersistedCell, PersistedThread};

pub struct AgentThread {
    pub name: String,
    /// Who spawned it ("root" or another agent's name).
    pub parent_id: String,
    pub task: String,
    pub status: ThreadStatus,
    pub started: std::time::Instant,
    pub cells: Vec<Cell>,
    /// Content cells appended since the main pane last showed this thread.
    pub unread: usize,
    /// Cache invalidation includes in-place cell mutations, not just appends.
    pub revision: u64,
}

impl AgentThread {
    pub fn display_label(&self) -> &str {
        let looks_auto = self
            .name
            .strip_prefix("task_")
            .or_else(|| self.name.strip_prefix("job_"))
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if looks_auto && !self.task.trim().is_empty() {
            &self.task
        } else {
            &self.name
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThreadStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl AgentThread {
    fn new(name: String, parent_id: String, task: String) -> Self {
        AgentThread {
            name,
            parent_id,
            task,
            status: ThreadStatus::Running,
            started: std::time::Instant::now(),
            cells: Vec::new(),
            unread: 0,
            revision: 0,
        }
    }
}

#[derive(Default)]
pub struct AgentTranscripts {
    /// Insertion order of agent ids, so the sidebar roster is stable.
    order: Vec<String>,
    threads: std::collections::HashMap<String, AgentThread>,
}

impl AgentTranscripts {
    pub fn new() -> Self {
        AgentTranscripts::default()
    }

    pub fn replay(events: &[AgentEvent]) -> Self {
        let mut threads = Self::new();
        for event in events {
            threads.apply(event, None);
        }
        for id in threads.running_ids() {
            threads.finish_thread(&id, ThreadStatus::Cancelled);
        }
        threads
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Shared by sidebar rendering and navigation; running agents precede finished ones.
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

    pub fn running_ids(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| {
                matches!(
                    self.threads.get(*id).map(|t| t.status),
                    Some(ThreadStatus::Running)
                )
            })
            .cloned()
            .collect()
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

    pub fn toggle_tool(&mut self, id: &str, cell_idx: usize) -> bool {
        let Some(thread) = self.threads.get_mut(id) else {
            return false;
        };
        let Some(Cell::Tool { expanded, .. }) = thread.cells.get_mut(cell_idx) else {
            return false;
        };
        *expanded = !*expanded;
        thread.revision += 1;
        true
    }

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

    pub fn on_spawn(&mut self, agent_id: &str, parent_id: &str, task: &str, prompt: &str) {
        let already = self.threads.contains_key(agent_id);
        let t = self.ensure(agent_id);
        let restarting = t.status != ThreadStatus::Running;
        t.status = ThreadStatus::Running;
        if restarting {
            t.started = std::time::Instant::now();
        }
        t.parent_id = parent_id.to_string();
        t.task = task.to_string();
        if (!already || restarting) && !prompt.trim().is_empty() {
            t.cells.push(Cell::User(prompt.to_string()));
            t.revision += 1;
        }
    }

    pub fn on_done(&mut self, agent_id: &str, failed: bool) {
        self.finish_thread(
            agent_id,
            if failed {
                ThreadStatus::Failed
            } else {
                ThreadStatus::Done
            },
        );
    }

    fn finish_thread(&mut self, agent_id: &str, status: ThreadStatus) {
        let t = self.ensure(agent_id);
        t.status = status;
        finish_retries(&mut t.cells);
        for cell in &mut t.cells {
            match cell {
                Cell::Assistant { open, .. } => *open = false,
                Cell::Tool {
                    status: tool_status,
                    output,
                    ..
                } if *tool_status == ToolStatus::Running => {
                    *tool_status = ToolStatus::Error;
                    output.get_or_insert_with(|| match status {
                        ThreadStatus::Cancelled => "(cancelled)".into(),
                        _ => "(agent stopped before the tool returned)".into(),
                    });
                }
                _ => {}
            }
        }
        t.revision += 1;
    }

    /// Messages to the displayed thread do not increment unread.
    pub fn push_message(&mut self, agent_id: &str, from: &str, text: &str, showing: Option<&str>) {
        let is_showing = showing == Some(agent_id);
        let t = self.ensure(agent_id);
        t.cells.push(Cell::AgentMsg {
            from: from.to_string(),
            text: text.to_string(),
        });
        t.revision += 1;
        if !is_showing {
            t.unread += 1;
        }
    }

    /// Route subagent events without incrementing unread for the displayed thread.
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
            AgentEvent::SubagentDone {
                agent_id,
                failed,
                cancelled,
            } => {
                if *cancelled {
                    self.finish_thread(agent_id, ThreadStatus::Cancelled);
                } else {
                    self.on_done(agent_id, *failed);
                }
            }
            AgentEvent::UserMessageQueued { to, text } => {
                if to != "root" {
                    self.push_message(to, "user", text, showing);
                }
            }
            AgentEvent::ContextReset { agent_id, .. } if agent_id == "root" => {
                *self = Self::new();
            }
            AgentEvent::AgentMessage { to, from, text } => {
                if from == "user" {
                    return;
                }
                if to != "root" {
                    self.push_message(to, from, text, showing);
                }
                if to != from {
                    if let Some(sender) = self.threads.get_mut(from) {
                        sender
                            .cells
                            .push(Cell::Notice(format!("Message received by {to}")));
                        sender.revision += 1;
                        if showing != Some(from.as_str()) {
                            sender.unread += 1;
                        }
                    }
                }
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
                        // Older background tasks never emitted lifecycle events.
                        if t.parent_id.is_empty() {
                            match event {
                                AgentEvent::TurnEnd { .. } if t.status == ThreadStatus::Running => {
                                    t.status = ThreadStatus::Done
                                }
                                AgentEvent::Error { .. } => t.status = ThreadStatus::Failed,
                                _ => {}
                            }
                        }
                        before = t.cells.len();
                        apply_content_event(&mut t.cells, event, true);
                        after = t.cells.len();
                        t.revision += 1;
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
                    ThreadStatus::Cancelled => "cancelled",
                }
                .to_string(),
                cells: t.cells.iter().filter_map(cell_to_persisted).collect(),
            })
            .collect()
    }

    /// Interrupted runs resume as cancelled; completed outcomes retain their state.
    pub fn from_persisted(threads: &[PersistedThread]) -> Self {
        let mut out = AgentTranscripts::new();
        for pt in threads {
            let status = match pt.status.as_str() {
                "failed" => ThreadStatus::Failed,
                "running" | "cancelled" => ThreadStatus::Cancelled,
                _ => ThreadStatus::Done,
            };
            let thread = AgentThread {
                name: pt.name.clone(),
                parent_id: pt.parent_id.clone(),
                task: pt.task.clone(),
                status,
                started: std::time::Instant::now(),
                cells: pt.cells.iter().map(cell_from_persisted).collect(),
                unread: 0,
                revision: 0,
            };
            out.order.push(pt.id.clone());
            out.threads.insert(pt.id.clone(), thread);
        }
        out
    }
}

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
            output: output.clone().unwrap_or_else(|| "".into()),
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
        | AgentEvent::UserPrompt { agent_id, .. }
        | AgentEvent::TextDelta { agent_id, .. }
        | AgentEvent::Message { agent_id, .. }
        | AgentEvent::ToolCall { agent_id, .. }
        | AgentEvent::ToolResult { agent_id, .. }
        | AgentEvent::Compaction { agent_id, .. }
        | AgentEvent::ContextWarning { agent_id, .. }
        | AgentEvent::StreamRetry { agent_id, .. }
        | AgentEvent::TurnEnd { agent_id, .. }
        | AgentEvent::Completion { agent_id, .. }
        | AgentEvent::Error { agent_id, .. } => Some(agent_id.as_str()),
        AgentEvent::SubagentSpawn { .. }
        | AgentEvent::SubagentDone { .. }
        | AgentEvent::WorkflowPhase { .. }
        | AgentEvent::WorkflowLog { .. }
        | AgentEvent::AgentMessage { .. } => None,
        AgentEvent::Unknown
        | AgentEvent::UserMessageQueued { .. }
        | AgentEvent::ContextReset { .. }
        | AgentEvent::ContextArchive { .. } => None,
    }
}

pub struct WorkflowView {
    pub run_id: String,
    /// Indexes visible phase and agent rows, rebuilt each draw.
    pub list: super::widgets::SelectList,
    /// Phase indices the user has collapsed (their agents are hidden).
    pub collapsed: std::collections::HashSet<usize>,
}

impl Default for WorkflowView {
    fn default() -> Self {
        WorkflowView {
            run_id: String::new(),
            list: super::widgets::SelectList::new(),
            collapsed: std::collections::HashSet::new(),
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
                cancelled: false,
            },
            None,
        );

        let thread = t.get("reviewer").unwrap();
        assert_eq!(thread.task, "review");
        assert_eq!(thread.status, ThreadStatus::Done);
        assert_eq!(thread.cells.len(), 2);
        assert!(thread.unread >= 2);
    }

    #[test]
    fn non_content_events_do_not_create_root_or_coordination_cells() {
        for (label, event) in [
            ("root tool call", tool_call("root", "t1", "bash")),
            (
                "agent completion",
                AgentEvent::Completion {
                    agent_id: "a".into(),
                    model: "m".into(),
                    usage: Usage::default(),
                },
            ),
        ] {
            let mut threads = AgentTranscripts::new();
            threads.on_spawn("a", "root", "task a", "");
            threads.on_spawn("b", "root", "task b", "");
            threads.apply(&event, None);
            assert!(threads.get("root").is_none(), "{label}: no root thread");
            assert_eq!(threads.display_order(), vec!["a", "b"], "{label}: roster");
            for id in ["a", "b"] {
                let thread = threads.get(id).unwrap();
                assert!(thread.cells.is_empty(), "{label}: {id} content");
                assert_eq!(thread.unread, 0, "{label}: {id} unread");
            }
        }
    }

    #[test]
    fn coordination_receipts_and_nested_messages_survive_replay_and_snapshots() {
        let mut threads = AgentTranscripts::new();
        threads.on_spawn("parent", "root", "parent", "");
        threads.on_spawn("child", "parent", "child", "");
        let mut events = Vec::new();
        for (to, from, text) in [
            ("parent", "root", "First instruction"),
            ("child", "parent", "Nested instruction"),
            ("parent", "child", "Nested answer"),
            ("root", "child", "Finished report"),
        ] {
            let event = AgentEvent::AgentMessage {
                to: to.into(),
                from: from.into(),
                text: text.into(),
            };
            threads.apply(&event, Some("child"));
            events.push(event);
        }
        assert!(threads.get("root").is_none());
        let parent = threads.get("parent").unwrap();
        assert!(
            matches!(&parent.cells[0], Cell::AgentMsg { from, text } if from == "root" && text == "First instruction")
        );
        assert!(
            matches!(&parent.cells[1], Cell::Notice(text) if text == "Message received by child")
        );
        assert!(
            matches!(&parent.cells[2], Cell::AgentMsg { from, text } if from == "child" && text == "Nested answer")
        );
        assert_eq!(parent.unread, 3);
        let child = threads.get("child").unwrap();
        assert_eq!(child.unread, 0);
        assert!(
            matches!(&child.cells[0], Cell::AgentMsg { from, text } if from == "parent" && text == "Nested instruction")
        );
        assert!(
            matches!(&child.cells[2], Cell::Notice(text) if text == "Message received by root")
        );
        threads.on_done("child", false);
        let report = AgentEvent::AgentMessage {
            to: "root".into(),
            from: "child".into(),
            text: "Final report".into(),
        };
        threads.apply(&report, None);
        assert_eq!(threads.get("child").unwrap().status, ThreadStatus::Done);
        events.push(report);
        threads.on_done("parent", false);
        let saved = threads.to_persisted();
        let restored = AgentTranscripts::from_persisted(&saved);
        assert_eq!(
            serde_json::to_value(restored.to_persisted()).unwrap(),
            serde_json::to_value(saved).unwrap()
        );
        let serialized = serde_json::to_string(&events).unwrap();
        let replay = AgentTranscripts::replay(
            &serde_json::from_str::<Vec<AgentEvent>>(&serialized).unwrap(),
        );
        for id in ["parent", "child"] {
            let visible = |thread: &AgentThread| {
                thread
                    .cells
                    .iter()
                    .map(Cell::fingerprint)
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                visible(threads.get(id).unwrap()),
                visible(replay.get(id).unwrap())
            );
        }
    }

    #[test]
    fn queued_human_messages_render_once_without_deduplicating_repeated_text() {
        let mut threads = AgentTranscripts::new();
        threads.on_spawn("child", "root", "child", "");
        for _ in 0..2 {
            threads.apply(
                &AgentEvent::UserMessageQueued {
                    to: "child".into(),
                    text: "continue".into(),
                },
                None,
            );
            threads.apply(
                &AgentEvent::AgentMessage {
                    to: "child".into(),
                    from: "user".into(),
                    text: "continue".into(),
                },
                None,
            );
        }
        let child = threads.get("child").unwrap();
        assert_eq!(child.cells.len(), 2);
        assert_eq!(child.unread, 2);
        assert!(child.cells.iter().all(|cell| matches!(cell, Cell::AgentMsg { from, text } if from == "user" && text == "continue")));
        assert!(threads.get("user").is_none());
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
    fn display_label_prefers_description_for_auto_ids() {
        let mut t = AgentTranscripts::new();
        t.on_spawn("task_7", "root", "review src/core", "audit it");
        assert_eq!(t.get("task_7").unwrap().display_label(), "review src/core");
        t.on_spawn("job_3", "root", "check tests", "run tests");
        assert_eq!(t.get("job_3").unwrap().display_label(), "check tests");
        t.on_spawn("researcher", "root", "dig into perf", "profile it");
        assert_eq!(t.get("researcher").unwrap().display_label(), "researcher");
        t.on_spawn("task_9", "root", "", "");
        assert_eq!(t.get("task_9").unwrap().display_label(), "task_9");
    }

    #[test]
    fn terminal_states_survive_event_replay_and_persistence() {
        let mut live = AgentTranscripts::new();
        let mut replayed = AgentTranscripts::new();
        for (id, failed, cancelled, expected) in [
            ("job_1", false, false, ThreadStatus::Done),
            ("job_2", true, false, ThreadStatus::Failed),
            ("job_3", false, true, ThreadStatus::Cancelled),
        ] {
            for event in [
                AgentEvent::SubagentSpawn {
                    parent_id: "root".into(),
                    agent_id: id.into(),
                    task: "review".into(),
                    prompt: "review code".into(),
                },
                AgentEvent::SubagentDone {
                    agent_id: id.into(),
                    failed,
                    cancelled,
                },
            ] {
                live.apply(&event, None);
                let serialized = serde_json::to_string(&event).unwrap();
                let restored = serde_json::from_str(&serialized).unwrap();
                replayed.apply(&restored, None);
            }
            assert_eq!(live.get(id).unwrap().status, expected);
            assert_eq!(replayed.get(id).unwrap().status, expected);
        }
        live.on_spawn("job_4", "root", "still working", "continue");
        assert_eq!(
            live.display_order(),
            vec!["job_4", "job_1", "job_2", "job_3"]
        );
        let restored = AgentTranscripts::from_persisted(&live.to_persisted());
        assert_eq!(restored.display_order().len(), 4);
        assert_eq!(restored.get("job_1").unwrap().status, ThreadStatus::Done);
        assert_eq!(restored.get("job_2").unwrap().status, ThreadStatus::Failed);
        assert_eq!(
            restored.get("job_3").unwrap().status,
            ThreadStatus::Cancelled
        );
        assert_eq!(
            restored.get("job_4").unwrap().status,
            ThreadStatus::Cancelled
        );
    }

    #[test]
    fn replay_cancels_unfinished_agents_without_duplicating_their_transcripts() {
        let events = vec![
            AgentEvent::SubagentSpawn {
                parent_id: "root".into(),
                agent_id: "job_1".into(),
                task: "review".into(),
                prompt: "review code".into(),
            },
            tool_call("job_1", "tool_1", "read_file"),
        ];
        let threads = AgentTranscripts::replay(&events);
        let thread = threads.get("job_1").unwrap();
        assert_eq!(thread.status, ThreadStatus::Cancelled);
        assert_eq!(thread.cells.len(), 2);
        assert!(
            matches!(&thread.cells[1], Cell::Tool { status: ToolStatus::Error, output: Some(output), .. } if output.as_text() == Some("(cancelled)"))
        );
    }

    #[test]
    fn cancelled_agent_closes_streaming_and_pending_cells() {
        let mut threads = AgentTranscripts::new();
        threads.on_spawn("job_1", "root", "review", "");
        threads.apply(&tool_call("job_1", "tool_1", "read_file"), None);
        threads.apply(
            &AgentEvent::TextDelta {
                agent_id: "job_1".into(),
                text: "checking".into(),
            },
            None,
        );
        threads.apply(
            &AgentEvent::SubagentDone {
                agent_id: "job_1".into(),
                failed: false,
                cancelled: true,
            },
            None,
        );
        let thread = threads.get("job_1").unwrap();
        assert_eq!(thread.status, ThreadStatus::Cancelled);
        assert!(
            matches!(&thread.cells[0], Cell::Tool { status: ToolStatus::Error, output: Some(output), .. } if output.as_text() == Some("(cancelled)"))
        );
        assert!(matches!(
            &thread.cells[1],
            Cell::Assistant { open: false, .. }
        ));
    }

    #[test]
    fn legacy_background_task_finishes_without_explicit_lifecycle_events() {
        let mut threads = AgentTranscripts::new();
        threads.apply(
            &AgentEvent::TurnStart {
                agent_id: "job_1".into(),
            },
            None,
        );
        threads.apply(
            &AgentEvent::TurnEnd {
                agent_id: "job_1".into(),
                usage: Usage::default(),
            },
            None,
        );
        assert_eq!(threads.get("job_1").unwrap().status, ThreadStatus::Done);
        threads.apply(
            &AgentEvent::Error {
                agent_id: "job_2".into(),
                message: "failed".into(),
            },
            None,
        );
        threads.apply(
            &AgentEvent::TurnEnd {
                agent_id: "job_2".into(),
                usage: Usage::default(),
            },
            None,
        );
        assert_eq!(threads.get("job_2").unwrap().status, ThreadStatus::Failed);
    }

    #[test]
    fn reused_agent_name_becomes_running_again() {
        let mut threads = AgentTranscripts::new();
        threads.on_spawn("reviewer", "root", "first", "first prompt");
        threads.on_done("reviewer", false);
        threads.on_spawn("reviewer", "root", "second", "second prompt");
        let thread = threads.get("reviewer").unwrap();
        assert_eq!(thread.status, ThreadStatus::Running);
        assert_eq!(thread.task, "second");
        assert_eq!(thread.cells.len(), 2);
    }

    #[test]
    fn depth_tracks_known_ancestors_and_bounds_cycles() {
        let mut threads = AgentTranscripts::new();
        for (id, parent) in [
            ("a", "root"),
            ("ab", "a"),
            ("abc", "ab"),
            ("orphan", "missing"),
            ("cycle-a", "cycle-b"),
            ("cycle-b", "cycle-a"),
        ] {
            threads.on_spawn(id, parent, "", "");
        }
        for (label, id, expected) in [
            ("root child", "a", 0),
            ("grandchild", "ab", 1),
            ("great grandchild", "abc", 2),
            ("unknown agent", "missing", 0),
            ("untracked parent", "orphan", 0),
            ("cycle cap", "cycle-a", 16),
        ] {
            assert_eq!(threads.depth_of(id), expected, "{label}");
        }
    }

    #[test]
    fn in_place_tool_updates_invalidate_revision_without_inflating_unread() {
        let mut threads = AgentTranscripts::new();
        threads.on_spawn("worker", "root", "task", "");
        threads.apply(&tool_call("worker", "t1", "read_file"), None);
        let thread = threads.get("worker").unwrap();
        assert_eq!(thread.unread, 1);
        assert_eq!(thread.cells.len(), 1);
        let revision = thread.revision;
        threads.apply(
            &AgentEvent::ToolResult {
                agent_id: "worker".into(),
                tool_use_id: "t1".into(),
                output: "result".into(),
                is_error: false,
            },
            None,
        );
        let thread = threads.get("worker").unwrap();
        assert_eq!(thread.cells.len(), 1);
        assert_eq!(thread.unread, 1);
        assert_eq!(thread.revision, revision + 1);
        assert!(matches!(&thread.cells[0], Cell::Tool {
        status: ToolStatus::Ok, output: Some(output), ..
    } if output.as_text() == Some("result")));
        threads.mark_read("worker");
        assert_eq!(threads.get("worker").unwrap().unread, 0);
        assert!(threads.toggle_tool("worker", 0));
        assert!(matches!(
            &threads.get("worker").unwrap().cells[0],
            Cell::Tool { expanded: true, .. }
        ));
        assert_eq!(threads.get("worker").unwrap().revision, revision + 2);
        assert!(!threads.toggle_tool("worker", 1));
        assert!(!threads.toggle_tool("missing", 0));
        assert_eq!(threads.get("worker").unwrap().revision, revision + 2);
    }

    #[test]
    fn managed_agent_turn_end_waits_for_explicit_lifecycle_completion() {
        let mut threads = AgentTranscripts::new();
        threads.on_spawn("worker", "root", "task", "");
        threads.apply(
            &AgentEvent::TurnEnd {
                agent_id: "worker".into(),
                usage: Usage::default(),
            },
            None,
        );
        assert_eq!(threads.get("worker").unwrap().status, ThreadStatus::Running);
        threads.apply(
            &AgentEvent::SubagentDone {
                agent_id: "worker".into(),
                failed: true,
                cancelled: true,
            },
            None,
        );
        assert_eq!(
            threads.get("worker").unwrap().status,
            ThreadStatus::Cancelled
        );
        assert!(threads.running_ids().is_empty());
        threads.apply(
            &AgentEvent::ContextReset {
                agent_id: "worker".into(),
                epoch: 1,
            },
            None,
        );
        assert_eq!(threads.display_order(), vec!["worker"]);
        threads.apply(
            &AgentEvent::ContextReset {
                agent_id: "root".into(),
                epoch: 1,
            },
            None,
        );
        assert!(threads.is_empty());
        assert!(threads.get("worker").is_none());
        assert!(threads.to_persisted().is_empty());
    }

    #[test]
    fn display_order_runs_before_finished() {
        let mut t = AgentTranscripts::new();
        t.on_spawn("a", "root", "", "");
        t.on_spawn("b", "root", "", "");
        t.on_spawn("c", "root", "", "");
        t.on_done("a", false);
        assert_eq!(t.display_order(), vec!["b", "c", "a"]);
        assert_eq!(t.running_ids(), vec!["b", "c"]);
    }
}
