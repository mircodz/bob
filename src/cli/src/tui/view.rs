//! The retained view model. Agent events mutate a list of cells; the draw loop
//! turns cells into ratatui Lines each frame. This is the TUI's equivalent of
//! the CLI renderer — a pure subscriber to the event stream.

use bob_core::core::events::AgentEvent;
use bob_core::core::types::{ContentBlock, Message, Role};
use serde_json::Value;

/// Current unix time in whole seconds (for workflow-agent timing). Best-effort:
/// a clock error yields 0, which just means a 0s duration.
fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
    /// A live workflow run: its phases, each with the agents that ran in it. Built
    /// from the workflow's own event stream (WorkflowPhase + the run's
    /// SubagentSpawn/Done) so the run reads as one navigable tree instead of a flat
    /// list of lines. Clicking an agent row opens that agent's transcript in the
    /// team drawer.
    Workflow {
        /// The workflow run id (matches the `parent_id` on its agents' spawns).
        id: String,
        title: String,
        phases: Vec<WfPhase>,
        done: bool,
    },
}

/// One phase of a workflow run and the agents that ran under it.
#[derive(Clone)]
pub struct WfPhase {
    pub title: String,
    /// 0-based phase index and the declared total, for a "2/3" progress readout.
    pub index: usize,
    pub total: usize,
    pub agents: Vec<WfAgent>,
}

/// One agent within a workflow phase — its id (for drill-in), display label, live
/// status, and per-run metadata (tools, model, tokens, timing) so the workflow
/// view can show a rich master-detail without re-deriving from the event stream.
#[derive(Clone)]
pub struct WfAgent {
    pub agent_id: String,
    pub label: String,
    pub status: WfStatus,
    pub tools: usize,
    /// Model that answered (from the agent's `Completion` events), if seen yet.
    pub model: Option<String>,
    /// Total input tokens across this agent's completions.
    pub tokens: u64,
    /// Wall-clock spawn→done in whole seconds, filled when the agent finishes.
    pub duration_secs: Option<u64>,
    /// Monotonic-ish spawn stamp (unix secs) used to compute `duration_secs`. Not
    /// rendered directly.
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

    /// For a `Cell::Workflow`, map a rendered-line `offset` (0 = the cell's first
    /// line) to the agent id on that row, if the row is an agent row. Mirrors the
    /// line emission order in `render::render_workflow` for the RUNNING (expanded)
    /// layout: line 0 is the header, then per phase one branch line followed by one
    /// line per agent. Returns None for the header/phase/blank rows, for a collapsed
    /// (done) run, or for any non-workflow cell.
    pub fn workflow_agent_at(&self, offset: usize) -> Option<&str> {
        let Cell::Workflow { phases, done, .. } = self else {
            return None;
        };
        if *done {
            return None; // collapsed to a single summary line
        }
        // Row 0 = header. Walk phases accumulating rows.
        let mut row = 1usize;
        for phase in phases {
            if offset == row {
                return None; // the phase branch line
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
    /// The workflow-run id currently receiving events, so its phase/agent events
    /// land in the right `Cell::Workflow` instead of flat lines. Set on the first
    /// `WorkflowPhase`/`SubagentSpawn` of a run; cleared when the run ends.
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

    /// Find a workflow run's cell by id (for the full-screen view). Returns the
    /// (title, phases, done) so the view can render + navigate it.
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

    /// Extract the finished workflow trees for session persistence, in transcript
    /// order (matches the order of their `[workflow result]` hand-off messages, so
    /// hydrate can pair them up by position). Only `done` runs are persisted — a
    /// run still in flight isn't saved (it'll re-run or be abandoned).
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
    /// Tool results are matched back to their tool_use cell by id. `workflows` are
    /// the persisted workflow trees, re-inserted in order at each `[workflow result]`
    /// hand-off message so a resumed session shows past runs' trees.
    pub fn hydrate(
        &mut self,
        messages: &[Message],
        workflows: &[bob_core::core::session::PersistedWorkflow],
    ) {
        let mut wf_iter = workflows.iter();
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
                    // A workflow hand-off message: don't render the literal prompt;
                    // instead re-insert the persisted workflow tree that preceded it.
                    if bob_core::workflow::is_handoff(&text) {
                        if let Some(pw) = wf_iter.next() {
                            self.cells.push(persisted_workflow_to_cell(pw));
                        }
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
                                // A `workflow` tool call's visible artifact is the
                                // live tree cell, not a tool line — so on resume,
                                // re-insert the persisted tree here (paired in order),
                                // exactly as the live path renders it.
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

    /// The `WfAgent` with the given id inside any live workflow cell, if present.
    /// Workflow agents are keyed by their full id (e.g. "wf-demo.1-gather-1").
    fn find_wf_agent(&mut self, id: &str) -> Option<&mut WfAgent> {
        self.cells.iter_mut().rev().find_map(|c| match c {
            Cell::Workflow { phases, .. } => phases
                .iter_mut()
                .flat_map(|p| p.agents.iter_mut())
                .find(|a| a.agent_id == id),
            _ => None,
        })
    }

    /// The `Cell::Workflow` for the given run id, if present.
    fn find_workflow(&mut self, id: &str) -> Option<&mut Cell> {
        self.cells
            .iter_mut()
            .rev()
            .find(|c| matches!(c, Cell::Workflow { id: wid, .. } if wid == id))
    }

    /// Whether `parent_id` names a live workflow run (its spawns group into the
    /// tree). True for the active run or any existing workflow cell.
    fn is_workflow_parent(&self, parent_id: &str) -> bool {
        self.active_workflow.as_deref() == Some(parent_id)
            || parent_id.starts_with("wf-")
            || self
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Workflow { id, .. } if id == parent_id))
    }

    /// Apply one agent event to the model.
    pub fn apply(&mut self, event: &AgentEvent) {
        // Any applied event may mutate cells (append, or update a tool/subagent
        // cell in place). Bump the revision so the renderer rebuilds its line cache;
        // this is per-EVENT, not per-frame, so scrolling stays free.
        self.revision += 1;
        // Events from spawned subagents (agent_id like "task_1") only update the
        // count/done state of their Subagent cell — their inner chatter is never
        // rendered as its own cells. A workflow agent (id "wf-…") updates its row in
        // the workflow tree instead.
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
                        // A workflow agent's completion is authoritatively marked by
                        // SubagentDone (which carries failed); TurnEnd only nudges a
                        // still-Running row toward done as a fallback.
                        if a.status == WfStatus::Running {
                            a.status = WfStatus::Done;
                        }
                    } else if let Some(Cell::Subagent { done, .. }) = self.find_subagent(id) {
                        *done = true;
                    }
                }
                AgentEvent::Completion { model, usage, .. } => {
                    // Capture the agent's model + accumulate its input tokens for the
                    // workflow view's per-agent metadata.
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
            AgentEvent::TurnStart { .. } => {
                self.busy = true;
                // A root TurnStart while a workflow is active is the hand-off turn
                // that runs AFTER the workflow finished — so mark the run done and
                // stop routing further events into its tree.
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
                    // Attach the agent to the current phase of its workflow's tree
                    // rather than pushing a standalone Subagent cell.
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
                        // Land it in the last (current) phase; if a run somehow
                        // spawned before any phase, create an implicit one.
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
                        parent_id: parent_id.clone(),
                        task: task.clone(),
                        tools: 0,
                        done: false,
                        failed: false,
                    });
                }
            }
            AgentEvent::SubagentDone { agent_id, failed } => {
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
            // A workflow phase boundary: ensure the run's Cell::Workflow exists and
            // append this phase to its tree. Subsequent spawns attach to it.
            AgentEvent::WorkflowPhase {
                workflow_id,
                title,
                index,
                total,
            } => {
                self.active_workflow = Some(workflow_id.clone());
                if self.find_workflow(workflow_id).is_none() {
                    // Derive a display title from the id ("wf-demo" → "demo").
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
            // A freeform workflow log line stays a simple event line.
            AgentEvent::WorkflowLog { message, .. } => {
                self.cells.push(Cell::Event(message.clone()));
            }
            // A root ToolResult while a workflow is active is the `workflow` tool
            // returning (it runs synchronously in the turn) — mark the run done so
            // its tree collapses/finalizes without waiting for the next turn.
            AgentEvent::ToolResult { .. } => {
                if let Some(id) = self.active_workflow.take() {
                    if let Some(Cell::Workflow { done, .. }) = self.find_workflow(&id) {
                        *done = true;
                    }
                }
                apply_content_event(&mut self.cells, event, false);
            }
            // All other content events reduce via the shared helper (messages off).
            _ => apply_content_event(&mut self.cells, event, false),
        }
    }
}

/// Rebuild a `Cell::Workflow` from its persisted form (always `done`, since only
/// finished runs are persisted).
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
        AgentEvent::WorkflowPhase { .. } => return None,
        AgentEvent::WorkflowLog { .. } => return None,
        AgentEvent::AgentMessage { .. } => return None,
    };
    if id != "root" {
        Some(id)
    } else {
        None
    }
}

#[cfg(test)]
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

        // Exactly one workflow cell, two phases, agents attached to the right phase.
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
        // A normal task_* subagent (parent "root") must NOT be swallowed by the tree.
        vm.apply(&spawn("root", "task_1", "review"));
        assert!(vm
            .cells
            .iter()
            .any(|c| matches!(c, Cell::Subagent { agent_id, .. } if agent_id == "task_1")));
        assert!(!vm.cells.iter().any(|c| matches!(c, Cell::Workflow { .. })));
    }

    #[test]
    fn click_offset_maps_to_agent_id() {
        let mut vm = ViewModel::new();
        vm.apply(&phase("wf-demo", "Gather", 0, 1));
        vm.apply(&spawn("wf-demo", "wf-demo.1-a", "gather-1"));
        let cell = vm
            .cells
            .iter()
            .find(|c| matches!(c, Cell::Workflow { .. }))
            .unwrap();
        // Row 0 = header, row 1 = phase branch, row 2 = the agent.
        assert_eq!(cell.workflow_agent_at(0), None);
        assert_eq!(cell.workflow_agent_at(1), None);
        assert_eq!(cell.workflow_agent_at(2), Some("wf-demo.1-a"));
    }

    #[test]
    fn workflow_persists_and_rehydrates() {
        // Build a finished run, extract it, then rehydrate from a message history
        // whose hand-off turn should re-materialize the tree in place.
        let mut vm = ViewModel::new();
        vm.apply(&phase("wf-demo", "Gather", 0, 1));
        vm.apply(&spawn("wf-demo", "wf-demo.1-a", "gather-1"));
        vm.apply(&done("wf-demo.1-a", false));
        // A root TurnStart marks the run done (the hand-off turn).
        vm.apply(&AgentEvent::TurnStart {
            agent_id: "root".into(),
        });

        let persisted = vm.to_persisted_workflows();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].phases[0].agents[0].status, "done");

        // Rehydrate: a user turn, then the hand-off message, then the agent's reply.
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

        // The hand-off text must NOT appear as a user cell; the tree must be back.
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
        // The `workflow` TOOL path: the run is marked done by the root ToolResult,
        // and on resume the tree re-inserts at the workflow tool_use in history.
        let mut vm = ViewModel::new();
        vm.apply(&phase("wf-hunt-1", "Round 1", 0, 1));
        vm.apply(&spawn("wf-hunt-1", "wf-hunt-1.1-a", "find:r1"));
        vm.apply(&done("wf-hunt-1.1-a", false));
        // The root's ToolResult (the workflow tool returning) marks the run done.
        vm.apply(&AgentEvent::ToolResult {
            agent_id: "root".into(),
            tool_use_id: "tu1".into(),
            output: "{}".into(),
            is_error: false,
        });
        let persisted = vm.to_persisted_workflows();
        assert_eq!(persisted.len(), 1);

        // History: a user turn, then an assistant turn whose tool_use is `workflow`.
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
        // The tree is back, and NO bare `workflow` tool cell was rendered.
        assert!(restored
            .cells
            .iter()
            .any(|c| matches!(c, Cell::Workflow { done: true, .. })));
        assert!(!restored
            .cells
            .iter()
            .any(|c| matches!(c, Cell::Tool { name, .. } if name == "workflow")));
    }
}
