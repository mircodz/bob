use super::exploration::is_exploration;
use bob_core::core::events::AgentEvent;
use bob_core::core::types::{ContentBlock, Message, Role, ToolOutput};
use serde_json::Value;

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RetryState {
    Waiting,
    Recovering,
    Streaming,
    Done,
}

pub enum Cell {
    User(String),
    /// Streamed assistant prose (markdown). `open` while still streaming.
    Assistant {
        text: String,
        open: bool,
    },
    Tool {
        id: String,
        name: String,
        input: Value,
        status: ToolStatus,
        output: Option<ToolOutput>,
        expanded: bool,
    },
    Subagent {
        agent_id: String,
        task: String,
        tools: usize,
        done: bool,
        failed: bool,
    },
    Compaction {
        before: usize,
        after: usize,
        done: bool,
    },
    Retry {
        attempt: u32,
        max_attempts: u32,
        state: RetryState,
    },
    Notice(String),
    Plan(String),
    Event(String),
    AgentMsg {
        from: String,
        text: String,
    },
    Workflow {
        /// The workflow run id (matches the `parent_id` on its agents' spawns).
        id: String,
        title: String,
        phases: Vec<WfPhase>,
        done: bool,
    },
}

#[derive(Clone)]
pub struct WfPhase {
    pub title: String,
    /// Zero-based phase index.
    pub index: usize,
    pub total: usize,
    pub agents: Vec<WfAgent>,
}

#[derive(Clone)]
pub struct WfAgent {
    pub agent_id: String,
    pub label: String,
    pub status: WfStatus,
    pub tools: usize,
    pub model: Option<String>,
    /// Total input tokens across this agent's completions.
    pub tokens: u64,
    pub duration_secs: Option<u64>,
    /// Unix seconds used to compute `duration_secs`.
    pub started_unix: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum WfStatus {
    Running,
    Done,
    Failed,
}

impl WfStatus {
    fn as_str(self) -> &'static str {
        match self {
            WfStatus::Running => "running",
            WfStatus::Done => "done",
            WfStatus::Failed => "failed",
        }
    }
    fn from_str(s: &str) -> Self {
        match s {
            "failed" => WfStatus::Failed,
            "running" => WfStatus::Running,
            _ => WfStatus::Done,
        }
    }
}

impl Cell {
    pub fn is_visible(&self) -> bool {
        match self {
            Cell::Assistant { text, .. } => !text.is_empty(),
            Cell::Retry { state, .. } => *state == RetryState::Waiting,
            Cell::Tool { name, status, .. } => {
                !matches!(
                    name.as_str(),
                    "job_status" | "job_output" | "todo_write" | "workflow"
                ) && (!matches!(name.as_str(), "task" | "explore" | "spawn_agent")
                    || *status == ToolStatus::Error)
            }
            _ => true,
        }
    }

    /// Hash only fields that affect rendered output.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        if !self.is_visible() {
            return 0;
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        std::mem::discriminant(self).hash(&mut h);
        match self {
            Cell::User(t) => t.hash(&mut h),
            Cell::Assistant { text, .. } => text.hash(&mut h),
            Cell::Tool {
                name,
                input,
                status,
                output,
                expanded,
                ..
            } => {
                name.hash(&mut h);
                input.to_string().hash(&mut h);
                (*status as u8).hash(&mut h);
                if *expanded
                    || *status == ToolStatus::Error
                    || !is_exploration(name)
                    || matches!(output, Some(ToolOutput::Shell(_)))
                {
                    output.is_some().hash(&mut h);
                    if let Some(output) = output {
                        std::mem::discriminant(output).hash(&mut h);
                        match output {
                            ToolOutput::Text(text) => text.hash(&mut h),
                            ToolOutput::Shell(shell) => {
                                shell.exit_code.hash(&mut h);
                                shell.signal.hash(&mut h);
                                shell.core_dumped.hash(&mut h);
                                std::mem::discriminant(&shell.outcome).hash(&mut h);
                                for stream in [&shell.stdout, &shell.stderr] {
                                    stream.text.hash(&mut h);
                                    stream.total_bytes.hash(&mut h);
                                    stream.truncated.hash(&mut h);
                                    stream.output_id.hash(&mut h);
                                }
                                shell
                                    .page
                                    .as_ref()
                                    .map(|page| {
                                        (
                                            &page.job_id,
                                            page.offset,
                                            page.next_offset,
                                            page.retained_bytes,
                                        )
                                    })
                                    .hash(&mut h);
                            }
                        }
                    }
                }
                expanded.hash(&mut h);
            }
            Cell::Subagent { agent_id, task, .. } => {
                if task.trim().is_empty() {
                    agent_id.hash(&mut h);
                } else {
                    task.hash(&mut h);
                }
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
            Cell::Retry {
                attempt,
                max_attempts,
                ..
            } => {
                attempt.hash(&mut h);
                max_attempts.hash(&mut h);
            }
            Cell::Notice(t) | Cell::Event(t) | Cell::Plan(t) => t.hash(&mut h),
            Cell::AgentMsg { from, text } => {
                from.hash(&mut h);
                text.hash(&mut h);
            }
            Cell::Workflow {
                id,
                title,
                phases,
                done,
            } => {
                id.hash(&mut h);
                title.hash(&mut h);
                done.hash(&mut h);
                for p in phases {
                    p.title.hash(&mut h);
                    p.index.hash(&mut h);
                    p.total.hash(&mut h);
                    for a in &p.agents {
                        a.agent_id.hash(&mut h);
                        a.label.hash(&mut h);
                        (a.status as u8).hash(&mut h);
                        a.tools.hash(&mut h);
                    }
                }
            }
        }
        h.finish()
    }

    /// Logical row offsets must match `render::render_workflow`; completed runs have no agent rows.
    pub fn workflow_agent_at(&self, offset: usize) -> Option<&str> {
        let Cell::Workflow { phases, done, .. } = self else {
            return None;
        };
        if *done {
            return None;
        }
        let mut row = 1usize;
        for phase in phases {
            if offset == row {
                return None;
            }
            row += 1;
            for agent in &phase.agents {
                if offset == row {
                    return Some(&agent.agent_id);
                }
                row += 1;
            }
        }
        None
    }
}

fn open_assistant_in(cells: &mut [Cell]) -> Option<&mut Cell> {
    match cells
        .iter_mut()
        .rev()
        .find(|cell| !matches!(cell, Cell::Retry { .. }))
    {
        Some(c @ Cell::Assistant { .. }) => match c {
            Cell::Assistant { open, .. } if *open => Some(c),
            _ => None,
        },
        _ => None,
    }
}

fn find_tool_in<'a>(cells: &'a mut [Cell], id: &str) -> Option<&'a mut Cell> {
    cells
        .iter_mut()
        .rev()
        .find(|c| matches!(c, Cell::Tool { id: tid, .. } if tid == id))
}

fn retry_draft_in(cells: &mut [Cell]) -> Option<&mut Cell> {
    let cell = cells
        .iter_mut()
        .rev()
        .find(|cell| matches!(cell, Cell::Assistant { .. } | Cell::Tool { .. }))?;
    matches!(cell, Cell::Assistant { open: true, .. }).then_some(cell)
}

fn pending_retry_in(cells: &mut [Cell]) -> Option<&mut Cell> {
    for cell in cells.iter_mut().rev() {
        match cell {
            Cell::Retry { state, .. } => {
                return (*state != RetryState::Done).then_some(cell);
            }
            Cell::Assistant { open: false, .. } | Cell::Tool { .. } => {
                return None;
            }
            _ => {}
        }
    }
    None
}

pub fn finish_retries(cells: &mut [Cell]) -> bool {
    let mut finished = false;
    for cell in cells {
        if let Cell::Retry { state, .. } = cell {
            finished |= *state != RetryState::Done;
            *state = RetryState::Done;
        }
    }
    finished
}

/// Shared content reducer. Include coordination messages only in per-agent threads.
pub fn apply_content_event(cells: &mut Vec<Cell>, event: &AgentEvent, include_messages: bool) {
    if matches!(
        event,
        AgentEvent::TurnStart { .. }
            | AgentEvent::TurnEnd { .. }
            | AgentEvent::ToolCall { .. }
            | AgentEvent::Error { .. }
    ) {
        let retried = finish_retries(cells);
        if retried && matches!(event, AgentEvent::Error { .. } | AgentEvent::TurnEnd { .. }) {
            if let Some(Cell::Assistant { text, .. }) = retry_draft_in(cells) {
                text.clear();
            }
        }
    }
    match event {
        AgentEvent::TextDelta { text, .. } => {
            let mut replacing_draft = false;
            if let Some(Cell::Retry { state, .. }) = pending_retry_in(cells) {
                if matches!(*state, RetryState::Waiting | RetryState::Recovering) {
                    *state = RetryState::Streaming;
                    replacing_draft = true;
                }
            }
            if replacing_draft {
                if let Some(Cell::Assistant { text, .. }) = retry_draft_in(cells) {
                    text.clear();
                }
            }
            if let Some(Cell::Assistant { text: buf, .. }) = open_assistant_in(cells) {
                buf.push_str(text);
            } else {
                cells.push(Cell::Assistant {
                    text: text.clone(),
                    open: true,
                });
            }
        }
        AgentEvent::Message { message, .. } => {
            let retried = finish_retries(cells);
            let draft = if retried {
                retry_draft_in(cells)
            } else {
                open_assistant_in(cells)
            };
            if let Some(Cell::Assistant { text, open }) = draft {
                if retried {
                    *text = message.text();
                }
                *open = false;
            } else if retried && !message.text().is_empty() {
                cells.push(Cell::Assistant {
                    text: message.text(),
                    open: false,
                });
            }
        }
        AgentEvent::Completion { .. } => {
            for cell in cells.iter_mut() {
                if let Cell::Retry { state, .. } = cell {
                    if *state == RetryState::Waiting {
                        *state = RetryState::Streaming;
                    }
                }
            }
        }
        AgentEvent::TurnEnd { .. } => {
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
            if let Some(Cell::Retry { state, .. }) = pending_retry_in(cells) {
                *state = RetryState::Recovering;
            }
            let compacted = Cell::Compaction {
                before: *before_tokens,
                after: *after_tokens,
                done: true,
            };
            if let Some(pending) = cells
                .iter_mut()
                .rev()
                .find(|cell| matches!(cell, Cell::Compaction { done: false, .. }))
            {
                *pending = compacted;
            } else {
                cells.push(compacted);
            }
        }
        AgentEvent::ContextWarning {
            used_tokens,
            context_window,
            ..
        } => {
            let pct = used_tokens.saturating_mul(100) / (*context_window).max(1);
            let window_k = (*context_window as f64 / 1000.0).round() as usize;
            let used_k = (*used_tokens as f64 / 1000.0).round() as usize;
            cells.push(Cell::Notice(format!(
                "context ~{}% full (~{}k/{}k) — auto-compaction enabled; /compact to summarize now",
                pct, used_k, window_k
            )));
        }
        AgentEvent::StreamRetry {
            attempt,
            max_attempts,
            ..
        } => {
            let retry = Cell::Retry {
                attempt: *attempt,
                max_attempts: *max_attempts,
                state: RetryState::Waiting,
            };
            if let Some(current) = cells.iter_mut().rev().find(
                |cell| matches!(cell, Cell::Retry { state, .. } if *state != RetryState::Done),
            ) {
                *current = retry;
            } else {
                cells.push(retry);
            }
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
    pub busy: bool,
    /// Bump on cell changes to invalidate cached rendering, not on scrolling.
    pub revision: u64,
    active_workflow: Option<String>,
}

impl ViewModel {
    pub fn new() -> Self {
        ViewModel::default()
    }

    pub fn push_user(&mut self, text: String) {
        self.cells.push(Cell::User(text));
        self.revision += 1;
    }

    /// Finalize work whose terminating event is missing from the replay log.
    /// Interrupted work resumes as finished, not failed.
    pub fn reset_after_replay(&mut self) {
        self.busy = false;
        self.active_workflow = None;
        finish_retries(&mut self.cells);
        for cell in &mut self.cells {
            match cell {
                Cell::Subagent { done, .. } => *done = true,
                Cell::Tool { status, output, .. } if *status == ToolStatus::Running => {
                    *status = ToolStatus::Ok;
                    if output.is_none() {
                        *output = Some("(interrupted)".into());
                    }
                }
                Cell::Workflow { done, phases, .. } => {
                    *done = true;
                    for phase in phases {
                        for agent in &mut phase.agents {
                            if agent.status == WfStatus::Running {
                                agent.status = WfStatus::Done;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.revision += 1;
    }

    pub fn push_notice(&mut self, text: String) {
        self.cells.push(Cell::Notice(text));
        self.revision += 1;
    }

    pub fn push_plan(&mut self, plan: String) {
        self.cells.push(Cell::Plan(plan));
        self.revision += 1;
    }

    pub fn begin_compaction(&mut self) -> usize {
        self.cells.push(Cell::Compaction {
            before: 0,
            after: 0,
            done: false,
        });
        self.revision += 1;
        self.cells.len() - 1
    }

    /// Successful compaction is finalized by its persisted event, regardless of
    /// whether that event or the background outcome arrives first.
    pub fn finish_compaction(&mut self, idx: usize, result: Result<(bool, usize, usize), String>) {
        if !matches!(&result, Ok((true, _, _))) {
            finish_retries(&mut self.cells);
        }
        if let Some(cell) = self.cells.get_mut(idx) {
            match result {
                Ok((true, _, _)) => return,
                Ok((false, _, _)) => {
                    *cell = Cell::Notice("nothing to compact yet.".into());
                }
                Err(error) => {
                    *cell = Cell::Notice(format!("error: {error}"));
                }
            }
        }
        self.revision += 1;
    }

    pub fn push_event(&mut self, text: String) {
        self.cells.push(Cell::Event(text));
        self.revision += 1;
    }

    pub fn clear(&mut self) {
        self.cells.clear();
        self.revision += 1;
    }

    pub fn workflow_by_id(&self, id: &str) -> Option<(&str, &[WfPhase], bool)> {
        self.cells.iter().find_map(|c| match c {
            Cell::Workflow {
                id: wid,
                title,
                phases,
                done,
            } if wid == id => Some((title.as_str(), phases.as_slice(), *done)),
            _ => None,
        })
    }

    /// Persist completed trees in transcript order; hydration pairs them with hand-offs by position.
    pub fn to_persisted_workflows(&self) -> Vec<bob_core::core::session::PersistedWorkflow> {
        self.cells
            .iter()
            .filter_map(|c| match c {
                Cell::Workflow {
                    id,
                    title,
                    phases,
                    done: true,
                } => Some(bob_core::core::session::PersistedWorkflow {
                    id: id.clone(),
                    title: title.clone(),
                    phases: phases
                        .iter()
                        .map(|p| bob_core::core::session::PersistedWfPhase {
                            title: p.title.clone(),
                            index: p.index,
                            total: p.total,
                            agents: p
                                .agents
                                .iter()
                                .map(|a| bob_core::core::session::PersistedWfAgent {
                                    agent_id: a.agent_id.clone(),
                                    label: a.label.clone(),
                                    status: a.status.as_str().to_string(),
                                    tools: a.tools,
                                    model: a.model.clone(),
                                    tokens: a.tokens,
                                    duration_secs: a.duration_secs,
                                })
                                .collect(),
                        })
                        .collect(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Toggle output expansion and invalidate the render cache.
    pub fn toggle_tool_expanded(&mut self, idx: usize) -> bool {
        if let Some(Cell::Tool { expanded, .. }) = self.cells.get_mut(idx) {
            *expanded = !*expanded;
            self.revision += 1;
            true
        } else {
            false
        }
    }

    /// Match tool results by id and restore workflow trees in hand-off order.
    pub fn hydrate(
        &mut self,
        messages: &[Message],
        workflows: &[bob_core::core::session::PersistedWorkflow],
    ) {
        let mut wf_iter = workflows.iter();
        for m in messages {
            match m.role {
                Role::User => {
                    let text = m.text();
                    if text.starts_with("[conversation summary]") {
                        self.cells.push(Cell::Compaction {
                            before: 0,
                            after: 0,
                            done: true,
                        });
                        continue;
                    }
                    if bob_core::agent::team::is_coord_message(&text) {
                        continue;
                    }
                    if bob_core::workflow::is_handoff(&text) {
                        if let Some(pw) = wf_iter.next() {
                            self.cells.push(persisted_workflow_to_cell(pw));
                        }
                        continue;
                    }
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
                                if name == "workflow" {
                                    if let Some(pw) = wf_iter.next() {
                                        self.cells.push(persisted_workflow_to_cell(pw));
                                    }
                                    continue;
                                }
                                self.cells.push(Cell::Tool {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                    status: ToolStatus::Ok,
                                    output: None,
                                    expanded: false,
                                });
                                // Message history lacks spawn events; reconstruct notices from tool input.
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

    /// Reconstructed agents are finished, with unknown tool counts.
    fn hydrate_subagents(&mut self, name: &str, input: &Value) {
        let push = |cells: &mut Vec<Cell>, task: &str| {
            cells.push(Cell::Subagent {
                agent_id: String::new(),
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
                        push(&mut self.cells, desc);
                    }
                }
            }
            "spawn_agent" => {
                let desc = input.get("task").and_then(|t| t.as_str()).unwrap_or("");
                push(&mut self.cells, desc);
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

    fn find_wf_agent(&mut self, id: &str) -> Option<&mut WfAgent> {
        self.cells.iter_mut().rev().find_map(|c| match c {
            Cell::Workflow { phases, .. } => phases
                .iter_mut()
                .flat_map(|p| p.agents.iter_mut())
                .find(|a| a.agent_id == id),
            _ => None,
        })
    }

    fn find_workflow(&mut self, id: &str) -> Option<&mut Cell> {
        self.cells
            .iter_mut()
            .rev()
            .find(|c| matches!(c, Cell::Workflow { id: wid, .. } if wid == id))
    }

    fn is_workflow_parent(&self, parent_id: &str) -> bool {
        self.active_workflow.as_deref() == Some(parent_id)
            || parent_id.starts_with("wf-")
            || self
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Workflow { id, .. } if id == parent_id))
    }

    pub fn apply(&mut self, event: &AgentEvent) {
        self.revision += 1;
        // Subagent content belongs in its own thread; only metadata updates the root view.
        if let Some(id) = subagent_id(event) {
            match event {
                AgentEvent::ToolCall { .. } => {
                    if let Some(a) = self.find_wf_agent(id) {
                        a.tools += 1;
                    } else if let Some(Cell::Subagent { tools, .. }) = self.find_subagent(id) {
                        *tools += 1;
                    }
                }
                AgentEvent::TurnEnd { .. } => {
                    if let Some(a) = self.find_wf_agent(id) {
                        // SubagentDone owns failure status; TurnEnd only settles running rows.
                        if a.status == WfStatus::Running {
                            a.status = WfStatus::Done;
                        }
                    } else if let Some(Cell::Subagent { done, .. }) = self.find_subagent(id) {
                        *done = true;
                    }
                }
                AgentEvent::Completion { model, usage, .. } => {
                    if let Some(a) = self.find_wf_agent(id) {
                        a.model = Some(model.clone());
                        a.tokens += usage.total_input();
                    }
                }
                _ => {}
            }
            return;
        }

        match event {
            AgentEvent::UserPrompt { text, .. } => {
                self.push_user(text.clone());
            }
            AgentEvent::TurnStart { .. } => {
                apply_content_event(&mut self.cells, event, false);
                self.busy = true;
                // The root hand-off turn begins after the workflow finishes.
                if let Some(id) = self.active_workflow.take() {
                    if let Some(Cell::Workflow { done, .. }) = self.find_workflow(&id) {
                        *done = true;
                    }
                }
            }
            AgentEvent::SubagentSpawn {
                agent_id,
                parent_id,
                task,
                ..
            } => {
                if self.is_workflow_parent(parent_id) {
                    self.active_workflow = Some(parent_id.clone());
                    if let Some(Cell::Workflow { phases, .. }) = self.find_workflow(parent_id) {
                        let agent = WfAgent {
                            agent_id: agent_id.clone(),
                            label: task.clone(),
                            status: WfStatus::Running,
                            tools: 0,
                            model: None,
                            tokens: 0,
                            duration_secs: None,
                            started_unix: unix_secs(),
                        };
                        if let Some(last) = phases.last_mut() {
                            last.agents.push(agent);
                        } else {
                            phases.push(WfPhase {
                                title: String::new(),
                                index: 0,
                                total: 1,
                                agents: vec![agent],
                            });
                        }
                    }
                } else {
                    self.cells.push(Cell::Subagent {
                        agent_id: agent_id.clone(),
                        task: task.clone(),
                        tools: 0,
                        done: false,
                        failed: false,
                    });
                }
            }
            AgentEvent::SubagentDone {
                agent_id, failed, ..
            } => {
                if let Some(a) = self.find_wf_agent(agent_id) {
                    a.status = if *failed {
                        WfStatus::Failed
                    } else {
                        WfStatus::Done
                    };
                    a.duration_secs = Some(unix_secs().saturating_sub(a.started_unix));
                } else if let Some(Cell::Subagent {
                    done, failed: f, ..
                }) = self.find_subagent(agent_id)
                {
                    *done = true;
                    *f = *failed;
                }
            }
            AgentEvent::TurnEnd { .. } => {
                apply_content_event(&mut self.cells, event, false);
                self.busy = false;
            }
            AgentEvent::Error { .. } => {
                apply_content_event(&mut self.cells, event, false);
                self.busy = false;
            }
            AgentEvent::UserMessageQueued { to, text } if to == "root" => {
                self.cells.push(Cell::User(text.clone()));
            }
            AgentEvent::ContextReset { agent_id, .. } if agent_id == "root" => self.clear(),
            AgentEvent::WorkflowPhase {
                workflow_id,
                title,
                index,
                total,
            } => {
                self.active_workflow = Some(workflow_id.clone());
                if self.find_workflow(workflow_id).is_none() {
                    let display = workflow_id
                        .strip_prefix("wf-")
                        .unwrap_or(workflow_id)
                        .to_string();
                    self.cells.push(Cell::Workflow {
                        id: workflow_id.clone(),
                        title: display,
                        phases: Vec::new(),
                        done: false,
                    });
                }
                if let Some(Cell::Workflow { phases, .. }) = self.find_workflow(workflow_id) {
                    phases.push(WfPhase {
                        title: title.clone(),
                        index: *index,
                        total: *total,
                        agents: Vec::new(),
                    });
                }
            }
            AgentEvent::WorkflowLog { message, .. } => {
                self.cells.push(Cell::Event(message.clone()));
            }
            // The synchronous workflow tool returns before the next root turn starts.
            AgentEvent::ToolResult { .. } => {
                if let Some(id) = self.active_workflow.take() {
                    if let Some(Cell::Workflow { done, .. }) = self.find_workflow(&id) {
                        *done = true;
                    }
                }
                apply_content_event(&mut self.cells, event, false);
            }
            _ => apply_content_event(&mut self.cells, event, false),
        }
    }
}

fn persisted_workflow_to_cell(pw: &bob_core::core::session::PersistedWorkflow) -> Cell {
    Cell::Workflow {
        id: pw.id.clone(),
        title: pw.title.clone(),
        done: true,
        phases: pw
            .phases
            .iter()
            .map(|p| WfPhase {
                title: p.title.clone(),
                index: p.index,
                total: p.total,
                agents: p
                    .agents
                    .iter()
                    .map(|a| WfAgent {
                        agent_id: a.agent_id.clone(),
                        label: a.label.clone(),
                        status: WfStatus::from_str(&a.status),
                        tools: a.tools,
                        model: a.model.clone(),
                        tokens: a.tokens,
                        duration_secs: a.duration_secs,
                        started_unix: 0,
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn subagent_id(event: &AgentEvent) -> Option<&str> {
    let id = match event {
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
        | AgentEvent::Error { agent_id, .. } => agent_id.as_str(),
        AgentEvent::SubagentSpawn { .. } => return None,
        AgentEvent::SubagentDone { .. } => return None,
        AgentEvent::WorkflowPhase { .. } => return None,
        AgentEvent::WorkflowLog { .. } => return None,
        AgentEvent::AgentMessage { .. } => return None,
        AgentEvent::Unknown
        | AgentEvent::UserMessageQueued { .. }
        | AgentEvent::ContextReset { .. }
        | AgentEvent::ContextArchive { .. } => return None,
    };
    if id != "root" {
        Some(id)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_reducer_closes_streams_and_matches_the_latest_tool_id() {
        let mut cells = Vec::new();
        let delta = |text: &str| AgentEvent::TextDelta {
            agent_id: "root".into(),
            text: text.into(),
        };
        let call = || AgentEvent::ToolCall {
            agent_id: "root".into(),
            tool_use_id: "same".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "run"}),
        };
        let result = |id: &str, text: &str, is_error| AgentEvent::ToolResult {
            agent_id: "root".into(),
            tool_use_id: id.into(),
            output: text.into(),
            is_error,
        };
        for event in [
            delta("前"),
            delta("e\u{301}"),
            call(),
            result("same", "first", false),
            delta("after tool"),
            call(),
            result("missing", "orphan", true),
            result("same", "second", true),
        ] {
            apply_content_event(&mut cells, &event, false);
        }
        assert_eq!(cells.len(), 4);
        assert!(matches!(&cells[0], Cell::Assistant { text, open: false } if text == "前e\u{301}"));
        assert!(
            matches!(&cells[1], Cell::Tool { status: ToolStatus::Ok, output: Some(text), expanded: false, .. } if text.as_text() == Some("first"))
        );
        assert!(matches!(&cells[2], Cell::Assistant { text, open: false } if text == "after tool"));
        assert!(
            matches!(&cells[3], Cell::Tool { status: ToolStatus::Error, output: Some(text), expanded: false, .. } if text.as_text() == Some("second"))
        );

        apply_content_event(&mut cells, &delta("new message"), false);
        apply_content_event(
            &mut cells,
            &AgentEvent::Message {
                agent_id: "root".into(),
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "new message".into(),
                    }],
                },
            },
            false,
        );
        apply_content_event(&mut cells, &delta("next"), false);
        assert_eq!(cells.len(), 6);
        assert!(
            matches!(&cells[4], Cell::Assistant { text, open: false } if text == "new message")
        );
        assert!(matches!(&cells[5], Cell::Assistant { text, open: true } if text == "next"));
    }

    #[test]
    fn retries_replace_drafts_but_preserve_authoritative_cancelled_or_failed_partials() {
        use super::super::team::AgentTranscripts;
        use bob_core::core::types::Usage;

        for (replacement, final_text, failed, compacted) in [
            (Some("complete "), Some("complete answer"), false, false),
            (None, Some("partial answer"), false, false),
            (None, Some("partial answer"), true, false),
            (Some("latest partial"), Some("latest partial"), true, false),
            (None, None, true, false),
            (Some("complete "), Some("complete answer"), false, true),
            (None, Some("partial answer"), true, true),
        ] {
            let mut events = Vec::new();
            for agent_id in ["root", "worker"] {
                events.extend([
                    AgentEvent::TextDelta {
                        agent_id: agent_id.into(),
                        text: "partial answer".into(),
                    },
                    AgentEvent::StreamRetry {
                        agent_id: agent_id.into(),
                        attempt: 1,
                        max_attempts: 10,
                    },
                ]);
                if agent_id == "worker" {
                    events.push(AgentEvent::UserMessageQueued {
                        to: agent_id.into(),
                        text: "keep checking".into(),
                    });
                }
                if compacted {
                    events.push(AgentEvent::Compaction {
                        agent_id: agent_id.into(),
                        before_tokens: 100,
                        after_tokens: 20,
                        summary: "Recovered context".into(),
                        replaced_upto: 1,
                    });
                }
                if let Some(text) = replacement {
                    events.push(AgentEvent::TextDelta {
                        agent_id: agent_id.into(),
                        text: text.into(),
                    });
                }
                if let Some(text) = final_text {
                    events.push(AgentEvent::Message {
                        agent_id: agent_id.into(),
                        message: Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::Text { text: text.into() }],
                        },
                    });
                }
                if failed {
                    events.push(AgentEvent::Error {
                        agent_id: agent_id.into(),
                        message: "connection failed".into(),
                    });
                }
                events.push(AgentEvent::TurnEnd {
                    agent_id: agent_id.into(),
                    usage: Usage::default(),
                });
            }
            let events: Vec<AgentEvent> =
                serde_json::from_str(&serde_json::to_string(&events).unwrap()).unwrap();
            let mut root = ViewModel::new();
            let mut teams = AgentTranscripts::new();
            for event in &events {
                root.apply(event);
                teams.apply(event, None);
                if let AgentEvent::TextDelta { agent_id, text } = event {
                    if replacement == Some(text.as_str()) {
                        let cells = if agent_id == "root" {
                            &root.cells
                        } else {
                            &teams.get(agent_id).unwrap().cells
                        };
                        assert!(cells.iter().any(|cell| {
                            matches!(cell, Cell::Assistant { text: draft, open: true } if draft == text)
                        }));
                    }
                }
            }
            let replayed = AgentTranscripts::replay(&events);
            for cells in [
                &root.cells,
                &teams.get("worker").unwrap().cells,
                &replayed.get("worker").unwrap().cells,
            ] {
                assert!(cells.iter().all(|cell| {
                    !matches!(cell, Cell::Retry { state, .. } if *state != RetryState::Done)
                }));
                let text: Vec<_> = cells
                    .iter()
                    .filter(|cell| cell.is_visible())
                    .filter_map(|cell| match cell {
                        Cell::Assistant { text, open } => {
                            assert!(!open);
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(text, final_text.into_iter().collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn retry_lifecycle_is_routed_and_interrupted_replay_is_not_live() {
        use super::super::team::AgentTranscripts;
        use bob_core::core::types::Usage;

        let retry = |id: &str| AgentEvent::StreamRetry {
            agent_id: id.into(),
            attempt: 3,
            max_attempts: 10,
        };
        let visible_retry = |cells: &[Cell]| {
            cells
                .iter()
                .any(|cell| matches!(cell, Cell::Retry { .. }) && cell.is_visible())
        };
        let mut root = ViewModel::new();
        let mut teams = AgentTranscripts::new();
        let events = [retry("root"), retry("worker")];
        for event in &events {
            root.apply(event);
            teams.apply(event, None);
        }
        let persisted = teams.to_persisted();
        assert!(visible_retry(&root.cells));
        assert!(visible_retry(&teams.get("worker").unwrap().cells));
        root.apply(&AgentEvent::TurnEnd {
            agent_id: "worker".into(),
            usage: Usage::default(),
        });
        assert!(visible_retry(&root.cells));
        teams.apply(
            &AgentEvent::SubagentDone {
                agent_id: "worker".into(),
                failed: false,
                cancelled: true,
            },
            None,
        );
        assert!(!visible_retry(&teams.get("worker").unwrap().cells));
        root.reset_after_replay();
        assert!(!visible_retry(&root.cells));
        let replayed = AgentTranscripts::replay(&events);
        assert!(!visible_retry(&replayed.get("worker").unwrap().cells));
        let restored = AgentTranscripts::from_persisted(&persisted);
        assert!(!visible_retry(&restored.get("worker").unwrap().cells));

        for terminal in [
            AgentEvent::Completion {
                agent_id: "root".into(),
                model: "test".into(),
                usage: Usage::default(),
            },
            AgentEvent::Compaction {
                agent_id: "root".into(),
                before_tokens: 100,
                after_tokens: 20,
                summary: String::new(),
                replaced_upto: 0,
            },
            AgentEvent::Error {
                agent_id: "root".into(),
                message: "connection failed".into(),
            },
            AgentEvent::TurnEnd {
                agent_id: "root".into(),
                usage: Usage::default(),
            },
        ] {
            root.apply(&retry("root"));
            root.apply(&terminal);
            assert!(!visible_retry(&root.cells));
        }
        let compaction = root.begin_compaction();
        root.apply(&retry("root"));
        root.finish_compaction(compaction, Err("connection failed".into()));
        assert!(!visible_retry(&root.cells));
    }

    #[test]
    fn hydration_matches_out_of_order_results_without_rendering_internal_history() {
        let messages = vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "system secret".into(),
                }],
            },
            Message::user_text("inspect 界"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "checking".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "read".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "界.rs"}),
                    },
                    ContentBlock::ToolUse {
                        id: "shell".into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": "false"}),
                    },
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "shell".into(),
                        content: "failed".into(),
                        is_error: Some(true),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "orphan".into(),
                        content: "ignore me".into(),
                        is_error: None,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "read".into(),
                        content: "raw\x1b[31m source".into(),
                        is_error: None,
                    },
                ],
            },
            Message::user_text("[conversation summary]\nprivate summary"),
        ];
        let mut view = ViewModel::new();
        view.hydrate(&messages, &[]);
        assert_eq!(view.revision, 1);
        assert!(!view.busy);
        assert_eq!(view.cells.len(), 5);
        assert!(matches!(&view.cells[0], Cell::User(text) if text == "inspect 界"));
        assert!(
            matches!(&view.cells[1], Cell::Assistant { text, open: false } if text == "checking")
        );
        for (index, expected_id, expected_status, expected_output, expected_input) in [
            (
                2,
                "read",
                ToolStatus::Ok,
                "raw\x1b[31m source",
                serde_json::json!({"path": "界.rs"}),
            ),
            (
                3,
                "shell",
                ToolStatus::Error,
                "failed",
                serde_json::json!({"command": "false"}),
            ),
        ] {
            let Cell::Tool {
                id,
                status,
                output,
                input,
                expanded,
                ..
            } = &view.cells[index]
            else {
                panic!("expected tool at {index}");
            };
            assert_eq!(id, expected_id);
            assert_eq!(*status, expected_status);
            assert_eq!(
                output.as_ref().and_then(ToolOutput::as_text),
                Some(expected_output)
            );
            assert_eq!(input, &expected_input);
            assert!(!expanded);
        }
        assert!(matches!(
            view.cells[4],
            Cell::Compaction {
                before: 0,
                after: 0,
                done: true
            }
        ));
    }

    #[test]
    fn structured_outputs_survive_replay_without_exposing_hidden_jobs() {
        use super::super::render::render_cell;
        use super::super::team::AgentTranscripts;
        use bob_core::core::types::{ShellOutput, ShellStream};

        let render = |cells: &[Cell]| {
            let mut lines = Vec::new();
            for cell in cells {
                render_cell(cell, 160, &mut lines);
            }
            lines
        };
        let shell = |code, text: &str| {
            ToolOutput::from(ShellOutput {
                exit_code: Some(code),
                stdout: ShellStream {
                    text: text.into(),
                    total_bytes: text.len() as u64,
                    ..Default::default()
                },
                ..Default::default()
            })
        };
        let cases = [
            ("bash", shell(0, "success output"), false),
            ("bash", shell(7, "failure output"), true),
            ("bash", ToolOutput::from("permission denied"), true),
            ("grep", ToolOutput::from("source.rs:42:match"), false),
            ("job_status", shell(7, "hidden job status"), true),
            ("job_output", shell(0, "hidden job output"), false),
        ];
        let mut messages = Vec::new();
        let mut events = Vec::new();
        let mut live = ViewModel::new();
        let mut teams = AgentTranscripts::new();
        teams.on_spawn("worker", "root", "check", "");
        for (index, (name, output, is_error)) in cases.iter().enumerate() {
            let id = index.to_string();
            let input = serde_json::json!({"command": "check"});
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: id.clone(),
                    name: (*name).into(),
                    input: input.clone(),
                }],
            });
            messages.push(Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: output.clone(),
                    is_error: Some(*is_error),
                }],
            });
            for agent_id in ["root", "worker"] {
                for event in [
                    AgentEvent::ToolCall {
                        agent_id: agent_id.into(),
                        tool_use_id: id.clone(),
                        name: (*name).into(),
                        input: input.clone(),
                    },
                    AgentEvent::ToolResult {
                        agent_id: agent_id.into(),
                        tool_use_id: id.clone(),
                        output: output.clone(),
                        is_error: *is_error,
                    },
                ] {
                    live.apply(&event);
                    teams.apply(&event, None);
                    events.push(event);
                }
            }
        }
        let events: Vec<AgentEvent> =
            serde_json::from_str(&serde_json::to_string(&events).unwrap()).unwrap();
        let mut replay = ViewModel::new();
        for event in &events {
            replay.apply(event);
        }
        replay.reset_after_replay();
        let team_replay = AgentTranscripts::replay(&events);
        let messages: Vec<Message> =
            serde_json::from_str(&serde_json::to_string(&messages).unwrap()).unwrap();
        let mut hydrated = ViewModel::new();
        hydrated.hydrate(&messages, &[]);
        let persisted = serde_json::to_string(&teams.to_persisted()).unwrap();
        let restored =
            AgentTranscripts::from_persisted(&serde_json::from_str::<Vec<_>>(&persisted).unwrap());
        let expected = render(&live.cells);
        let text = expected
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("    exit 7"));
        assert!(text.contains("  │ permission denied"));
        assert!(text.contains("• Grep"));
        assert!(!text.contains("source.rs:42:match"));
        assert!(!text.contains("hidden job"));
        for cells in [
            &live.cells,
            &replay.cells,
            &hydrated.cells,
            &teams.get("worker").unwrap().cells,
            &team_replay.get("worker").unwrap().cells,
            &restored.get("worker").unwrap().cells,
        ] {
            assert_eq!(render(cells), expected);
            assert_eq!(cells.len(), cases.len());
            for (cell, (name, expected_output, _)) in cells.iter().zip(&cases) {
                let Cell::Tool { output, .. } = cell else {
                    panic!("expected tool")
                };
                assert_eq!(output.as_ref(), Some(expected_output));
                assert_eq!(cell.is_visible(), matches!(*name, "bash" | "grep"));
            }
        }
    }

    mod fingerprint_tests {
        use super::*;
        use serde_json::json;

        fn tool(name: &str, status: ToolStatus) -> Cell {
            Cell::Tool {
                id: "tool".into(),
                name: name.into(),
                input: json!({"path": "file.rs"}),
                status,
                output: None,
                expanded: false,
            }
        }

        #[test]
        fn visibility_and_fingerprints_ignore_hidden_payloads() {
            for name in [
                "job_status",
                "job_output",
                "todo_write",
                "workflow",
                "task",
                "explore",
                "spawn_agent",
            ] {
                for status in [ToolStatus::Running, ToolStatus::Ok, ToolStatus::Error] {
                    let mut cell = tool(name, status);
                    let visible = matches!(name, "task" | "explore" | "spawn_agent")
                        && status == ToolStatus::Error;
                    assert_eq!(cell.is_visible(), visible);
                    let before = cell.fingerprint();
                    if let Cell::Tool {
                        input,
                        output,
                        expanded,
                        ..
                    } = &mut cell
                    {
                        *input = json!({"large": "hidden input".repeat(1000)});
                        *output = Some("hidden output".repeat(1000).into());
                        *expanded = true;
                    }
                    if visible {
                        assert_ne!(cell.fingerprint(), before);
                    } else {
                        assert_eq!(cell.fingerprint(), before);
                        assert_eq!(before, 0);
                    }
                }
            }
        }

        #[test]
        fn collapsed_read_and_list_fingerprints_ignore_output_only() {
            for name in ["read_file", "list_dir", "grep", "glob"] {
                let mut cell = tool(name, ToolStatus::Ok);
                assert!(cell.is_visible());
                let before = cell.fingerprint();
                if let Cell::Tool { output, .. } = &mut cell {
                    *output = Some("huge output".repeat(1000).into());
                }
                assert_eq!(cell.fingerprint(), before);
                if let Cell::Tool { input, .. } = &mut cell {
                    *input = json!({"path": "other.rs"});
                }
                assert_ne!(cell.fingerprint(), before);
                let before = cell.fingerprint();
                if let Cell::Tool { status, .. } = &mut cell {
                    *status = ToolStatus::Error;
                }
                assert_ne!(cell.fingerprint(), before);
                let before = cell.fingerprint();
                if let Cell::Tool { output, .. } = &mut cell {
                    *output = Some("failure details".into());
                }
                assert_ne!(cell.fingerprint(), before);
                let before = cell.fingerprint();
                if let Cell::Tool { expanded, .. } = &mut cell {
                    *expanded = true;
                }
                assert_ne!(cell.fingerprint(), before);
                let before = cell.fingerprint();
                if let Cell::Tool { output, .. } = &mut cell {
                    *output = Some("visible changed output".into());
                }
                assert_ne!(cell.fingerprint(), before);
            }
            let mut cell = tool("bash", ToolStatus::Ok);
            let before = cell.fingerprint();
            if let Cell::Tool { output, .. } = &mut cell {
                *output = Some("visible preview".into());
            }
            assert_ne!(cell.fingerprint(), before);
        }

        #[test]
        fn static_launch_notice_fingerprints_ignore_lifecycle() {
            let mut cell = Cell::Subagent {
                agent_id: "agent".into(),
                task: "Review parser".into(),
                tools: 0,
                done: false,
                failed: false,
            };
            let before = cell.fingerprint();
            if let Cell::Subagent {
                agent_id,
                tools,
                done,
                failed,
                ..
            } = &mut cell
            {
                *agent_id = "other".into();
                *tools = 50;
                *done = true;
                *failed = true;
            }
            assert_eq!(cell.fingerprint(), before);
            if let Cell::Subagent { task, .. } = &mut cell {
                *task = "Review renderer".into();
            }
            assert_ne!(cell.fingerprint(), before);
            if let Cell::Subagent { task, .. } = &mut cell {
                *task = "  ".into();
            }
            let before = cell.fingerprint();
            if let Cell::Subagent { agent_id, .. } = &mut cell {
                *agent_id = "fallback label".into();
            }
            assert_ne!(cell.fingerprint(), before);
        }
    }

    mod compaction_view_tests {
        use super::*;

        fn compaction() -> AgentEvent {
            AgentEvent::Compaction {
                agent_id: "root".into(),
                before_tokens: 900,
                after_tokens: 100,
                summary: "saved context".into(),
                replaced_upto: 5,
            }
        }

        #[test]
        fn manual_compaction_has_one_cell_in_either_delivery_order() {
            for event_first in [false, true] {
                let mut view = ViewModel::new();
                let cell = view.begin_compaction();
                if event_first {
                    view.apply(&compaction());
                }
                view.finish_compaction(cell, Ok((true, 900, 100)));
                if !event_first {
                    view.apply(&compaction());
                }
                assert_eq!(view.cells.len(), 1);
                assert!(matches!(
                    view.cells[0],
                    Cell::Compaction {
                        before: 900,
                        after: 100,
                        done: true,
                    }
                ));
            }
        }

        #[test]
        fn compaction_failure_is_not_reported_as_unnecessary() {
            let mut view = ViewModel::new();
            let cell = view.begin_compaction();
            view.finish_compaction(cell, Err("compaction failed: HTTP 400".into()));
            assert!(
                matches!(&view.cells[cell], Cell::Notice(text) if text == "error: compaction failed: HTTP 400")
            );
            let cell = view.begin_compaction();
            view.finish_compaction(cell, Ok((false, 10, 10)));
            assert!(
                matches!(&view.cells[cell], Cell::Notice(text) if text == "nothing to compact yet.")
            );
        }

        #[test]
        fn replay_compaction_and_overflow_warning_use_actual_counts() {
            let mut view = ViewModel::new();
            view.apply(&compaction());
            assert!(matches!(view.cells[0], Cell::Compaction { done: true, .. }));
            view.apply(&AgentEvent::ContextWarning {
                agent_id: "root".into(),
                used_tokens: 1_139_249,
                context_window: 1_050_000,
                pct: 95,
            });
            assert!(matches!(&view.cells[1], Cell::Notice(text) if text.contains("~108% full")));
        }
    }

    mod message_previews {
        use super::*;
        use crate::tui::{render, team::AgentTranscripts};
        use serde_json::json;

        fn send_events(from: &str, text: &str) -> Vec<AgentEvent> {
            vec![
                AgentEvent::ToolCall {
                    agent_id: from.into(),
                    tool_use_id: "message-1".into(),
                    name: "send_message".into(),
                    input: json!({"to":"reviewer", "message":text}),
                },
                AgentEvent::ToolResult {
                    agent_id: from.into(),
                    tool_use_id: "message-1".into(),
                    output: "delivered to 'reviewer'".into(),
                    is_error: false,
                },
                AgentEvent::AgentMessage {
                    to: "reviewer".into(),
                    from: from.into(),
                    text: text.into(),
                },
            ]
        }

        fn rendered(cells: &[Cell]) -> String {
            let mut lines = Vec::new();
            for cell in cells {
                render::render_cell(cell, 100, &mut lines);
            }
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        }

        #[test]
        fn root_message_previews_survive_replay_without_truncating_stored_input() {
            let text = format!(
                "{}\nFinal instruction.",
                "Check\n\tthe parser and preserve 日本語 café. ".repeat(20)
            );
            for delivery_before_result in [false, true] {
                let mut events = send_events("root", &text);
                if delivery_before_result {
                    events.swap(1, 2);
                }
                let mut live = ViewModel::new();
                for event in &events {
                    live.apply(event);
                    assert_eq!(
                        live.cells.iter().filter(|cell| cell.is_visible()).count(),
                        1
                    );
                }
                assert!(matches!(live.cells.as_slice(),
                    [Cell::Tool { name, input, status: ToolStatus::Ok, output: Some(output), .. }]
                        if name == "send_message"
                            && input["to"] == "reviewer"
                            && input["message"].as_str() == Some(text.as_str())
                            && output.as_text() == Some("delivered to 'reviewer'")
                ));
                let preview = rendered(&live.cells);
                assert!(preview.contains("Message to reviewer"));
                assert!(preview.contains("Check the parser and preserve 日本語 café."));
                assert!(preview.len() < text.len());
                let serialized = serde_json::to_string(&events).unwrap();
                let replayed: Vec<AgentEvent> = serde_json::from_str(&serialized).unwrap();
                let mut replay = ViewModel::new();
                for event in &replayed {
                    replay.apply(event);
                }
                replay.reset_after_replay();
                assert_eq!(rendered(&replay.cells), preview);
                assert!(live.toggle_tool_expanded(0));
                assert!(replay.toggle_tool_expanded(0));
                let expanded = rendered(&live.cells);
                assert!(expanded.contains("Final instruction."));
                assert_eq!(rendered(&replay.cells), expanded);
            }
        }

        #[test]
        fn agent_to_agent_message_preview_appears_in_sender_thread_and_replay() {
            let mut events = Vec::new();
            for id in ["implementer", "reviewer"] {
                events.push(AgentEvent::SubagentSpawn {
                    parent_id: "root".into(),
                    agent_id: id.into(),
                    task: id.into(),
                    prompt: String::new(),
                });
            }
            events.extend(send_events("implementer", "Review\n the parser changes."));
            let mut live = AgentTranscripts::new();
            for event in &events {
                live.apply(event, None);
            }
            let sender = live.get("implementer").unwrap();
            let text = rendered(&sender.cells);
            assert!(text.contains("Message to reviewer"));
            assert!(text.contains("Review the parser changes."));
            assert!(sender.cells.iter().any(|cell| matches!(cell,
                Cell::Tool { name, status: ToolStatus::Ok, output: Some(output), .. }
                    if name == "send_message" && output.as_text() == Some("delivered to 'reviewer'")
            )));
            let serialized = serde_json::to_string(&events).unwrap();
            let replayed: Vec<AgentEvent> = serde_json::from_str(&serialized).unwrap();
            let replay = AgentTranscripts::replay(&replayed);
            assert_eq!(text, rendered(&replay.get("implementer").unwrap().cells));
            assert!(live.get("reviewer").unwrap().cells.is_empty());
            assert!(replay.get("reviewer").unwrap().cells.is_empty());
        }
    }

    mod workflow_view_tests {
        use super::*;
        use bob_core::core::events::AgentEvent;

        fn phase(id: &str, title: &str, index: usize, total: usize) -> AgentEvent {
            AgentEvent::WorkflowPhase {
                workflow_id: id.into(),
                title: title.into(),
                index,
                total,
            }
        }
        fn spawn(parent: &str, agent: &str, task: &str) -> AgentEvent {
            AgentEvent::SubagentSpawn {
                parent_id: parent.into(),
                agent_id: agent.into(),
                task: task.into(),
                prompt: String::new(),
            }
        }
        fn done(agent: &str, failed: bool) -> AgentEvent {
            AgentEvent::SubagentDone {
                agent_id: agent.into(),
                failed,
                cancelled: false,
            }
        }

        #[test]
        fn builds_a_phase_grouped_tree() {
            let mut vm = ViewModel::new();
            vm.apply(&phase("wf-demo", "Gather", 0, 2));
            vm.apply(&spawn("wf-demo", "wf-demo.1-a", "gather-1"));
            vm.apply(&spawn("wf-demo", "wf-demo.2-b", "gather-2"));
            vm.apply(&done("wf-demo.1-a", false));
            vm.apply(&done("wf-demo.2-b", false));
            vm.apply(&phase("wf-demo", "Synthesize", 1, 2));
            vm.apply(&spawn("wf-demo", "wf-demo.3-s", "synthesize"));
            vm.apply(&done("wf-demo.3-s", false));

            let wf = vm
                .cells
                .iter()
                .find_map(|c| match c {
                    Cell::Workflow { phases, .. } => Some(phases),
                    _ => None,
                })
                .expect("a workflow cell");
            assert_eq!(wf.len(), 2);
            assert_eq!(wf[0].title, "Gather");
            assert_eq!(wf[0].agents.len(), 2);
            assert!(wf[0].agents.iter().all(|a| a.status == WfStatus::Done));
            assert_eq!(wf[1].title, "Synthesize");
            assert_eq!(wf[1].agents.len(), 1);
            assert_eq!(wf[1].agents[0].agent_id, "wf-demo.3-s");
        }

        #[test]
        fn failed_agent_marks_row_failed() {
            let mut vm = ViewModel::new();
            vm.apply(&phase("wf-x", "Only", 0, 1));
            vm.apply(&spawn("wf-x", "wf-x.1-a", "a"));
            vm.apply(&done("wf-x.1-a", true));
            let a = vm.cells.iter().find_map(|c| match c {
                Cell::Workflow { phases, .. } => phases[0].agents.first(),
                _ => None,
            });
            assert_eq!(a.unwrap().status, WfStatus::Failed);
        }

        #[test]
        fn non_workflow_spawn_still_makes_a_flat_subagent_cell() {
            let mut vm = ViewModel::new();
            vm.apply(&spawn("root", "task_1", "review"));
            assert!(vm
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Subagent { agent_id, .. } if agent_id == "task_1")));
            assert!(!vm.cells.iter().any(|c| matches!(c, Cell::Workflow { .. })));
        }

        #[test]
        fn replaying_events_rebuilds_the_transcript() {
            let log = vec![
                AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "fix the bug".into(),
                },
                AgentEvent::TurnStart {
                    agent_id: "root".into(),
                },
                AgentEvent::TextDelta {
                    agent_id: "root".into(),
                    text: "on it".into(),
                },
                AgentEvent::ToolCall {
                    agent_id: "root".into(),
                    tool_use_id: "t1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                },
                AgentEvent::ToolResult {
                    agent_id: "root".into(),
                    tool_use_id: "t1".into(),
                    output: "fn main() {}".into(),
                    is_error: false,
                },
                AgentEvent::Error {
                    agent_id: "root".into(),
                    message: "flaky network".into(),
                },
                AgentEvent::TurnEnd {
                    agent_id: "root".into(),
                    usage: Default::default(),
                },
            ];
            let mut vm = ViewModel::new();
            for e in &log {
                vm.apply(e);
            }
            vm.reset_after_replay();

            assert!(matches!(&vm.cells[0], Cell::User(t) if t == "fix the bug"));
            assert!(vm
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Assistant { text, .. } if text == "on it")));
            assert!(vm.cells.iter().any(
                |c| matches!(c, Cell::Tool { name, status: ToolStatus::Ok, output: Some(o), .. }
                if name == "read_file" && o.as_text() == Some("fn main() {}"))
            ));
            assert!(vm
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Notice(t) if t.contains("flaky network"))));
            assert!(!vm.busy);
        }

        #[test]
        fn workflow_persists_and_rehydrates() {
            let mut vm = ViewModel::new();
            vm.apply(&phase("wf-demo", "Gather", 0, 1));
            vm.apply(&spawn("wf-demo", "wf-demo.1-a", "gather-1"));
            vm.apply(&done("wf-demo.1-a", false));
            vm.apply(&AgentEvent::TurnStart {
                agent_id: "root".into(),
            });

            let persisted = vm.to_persisted_workflows();
            assert_eq!(persisted.len(), 1);
            assert_eq!(persisted[0].phases[0].agents[0].status, "done");

            let messages = vec![
                Message::user_text("do the thing"),
                Message::user_text(bob_core::workflow::handoff_prompt("demo", "{}")),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "summary".into(),
                    }],
                },
            ];
            let mut restored = ViewModel::new();
            restored.hydrate(&messages, &persisted);

            assert!(restored
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Workflow { done: true, .. })));
            assert!(!restored
                .cells
                .iter()
                .any(|c| matches!(c, Cell::User(t) if bob_core::workflow::is_handoff(t))));
        }

        #[test]
        fn tool_path_workflow_persists_and_rehydrates() {
            let mut vm = ViewModel::new();
            vm.apply(&phase("wf-hunt-1", "Round 1", 0, 1));
            vm.apply(&spawn("wf-hunt-1", "wf-hunt-1.1-a", "find:r1"));
            vm.apply(&done("wf-hunt-1.1-a", false));
            vm.apply(&AgentEvent::ToolResult {
                agent_id: "root".into(),
                tool_use_id: "tu1".into(),
                output: "{}".into(),
                is_error: false,
            });
            let persisted = vm.to_persisted_workflows();
            assert_eq!(persisted.len(), 1);

            let messages = vec![
                Message::user_text("hunt for bugs"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "tu1".into(),
                        name: "workflow".into(),
                        input: serde_json::json!({"shape": "loop"}),
                    }],
                },
            ];
            let mut restored = ViewModel::new();
            restored.hydrate(&messages, &persisted);
            assert!(restored
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Workflow { done: true, .. })));
            assert!(!restored
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Tool { name, .. } if name == "workflow")));
        }

        #[test]
        fn reset_after_replay_finalizes_running_stragglers() {
            let mut vm = ViewModel::new();
            vm.apply(&AgentEvent::TurnStart {
                agent_id: "root".into(),
            });
            vm.apply(&spawn("root", "task_1", "do work"));
            vm.apply(&spawn("root", "task_2", "failed work"));
            vm.apply(&done("task_2", true));
            for (id, status, output) in [
                ("missing", ToolStatus::Running, None),
                ("partial", ToolStatus::Running, Some("partial output")),
                ("failed", ToolStatus::Error, Some("permission denied")),
                ("finished", ToolStatus::Ok, Some("complete")),
            ] {
                vm.apply(&AgentEvent::ToolCall {
                    agent_id: "root".into(),
                    tool_use_id: id.into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "work"}),
                });
                if status != ToolStatus::Running {
                    vm.apply(&AgentEvent::ToolResult {
                        agent_id: "root".into(),
                        tool_use_id: id.into(),
                        output: output.unwrap().into(),
                        is_error: status == ToolStatus::Error,
                    });
                } else if let Some(Cell::Tool { output: stored, .. }) = vm.cells.last_mut() {
                    *stored = output.map(ToolOutput::from);
                }
                assert!(vm.toggle_tool_expanded(vm.cells.len() - 1));
            }
            vm.apply(&phase("wf-x", "Round 1", 0, 1));
            vm.apply(&spawn("wf-x", "running", "find"));
            vm.apply(&spawn("wf-x", "failed", "check"));
            vm.apply(&done("failed", true));
            let revision = vm.revision;

            for pass in 1..=2 {
                vm.reset_after_replay();
                assert_eq!(vm.revision, revision + pass);
                assert!(!vm.busy);
                assert!(vm.active_workflow.is_none());
                assert_eq!(vm.cells.len(), 7);
                assert!(matches!(
                    &vm.cells[0],
                    Cell::Subagent {
                        done: true,
                        failed: false,
                        ..
                    }
                ));
                assert!(matches!(
                    &vm.cells[1],
                    Cell::Subagent {
                        done: true,
                        failed: true,
                        ..
                    }
                ));
                for (index, expected_id, expected_status, expected_output) in [
                    (2, "missing", ToolStatus::Ok, "(interrupted)"),
                    (3, "partial", ToolStatus::Ok, "partial output"),
                    (4, "failed", ToolStatus::Error, "permission denied"),
                    (5, "finished", ToolStatus::Ok, "complete"),
                ] {
                    let Cell::Tool {
                        id,
                        status,
                        output,
                        expanded,
                        input,
                        ..
                    } = &vm.cells[index]
                    else {
                        panic!("expected tool at {index}");
                    };
                    assert_eq!(id, expected_id);
                    assert_eq!(*status, expected_status);
                    assert_eq!(
                        output.as_ref().and_then(ToolOutput::as_text),
                        Some(expected_output)
                    );
                    assert!(*expanded);
                    assert_eq!(input, &serde_json::json!({"command": "work"}));
                }
                let Cell::Workflow { done, phases, .. } = &vm.cells[6] else {
                    panic!("expected workflow");
                };
                assert!(*done);
                assert_eq!(phases.len(), 1);
                assert_eq!(phases[0].agents.len(), 2);
                assert_eq!(phases[0].agents[0].status, WfStatus::Done);
                assert_eq!(phases[0].agents[1].status, WfStatus::Failed);
            }
        }
    }
}
