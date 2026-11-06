use super::*;

#[path = "history.rs"]
mod history;
#[cfg(test)]
#[path = "message_queue_reply_tests.rs"]
mod message_queue_reply_tests;
#[cfg(test)]
#[path = "title_tests.rs"]
mod title_tests;

use bob_kernel::{Decision, TextPage};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::collections::{HashSet, VecDeque};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Default)]
pub(super) struct Draft {
    pub text: String,
    pub cursor: usize,
    pub scroll: usize,
    pub revision: u64,
    pub layout: Option<std::sync::Arc<super::composer::Layout>>,
    pub limit_reached: bool,
    #[cfg(test)]
    pub layout_builds: usize,
    history: VecDeque<String>,
    history_index: Option<usize>,
    saved: String,
    pastes: Vec<(String, String)>,
    paste_sequence: u64,
}
impl Draft {
    pub fn insert(&mut self, text: &str) {
        if self.replace(self.cursor..self.cursor, text) {
            self.history_index = None;
        }
    }
    pub fn replace(&mut self, range: std::ops::Range<usize>, text: &str) -> bool {
        if text.len() > super::composer::INPUT_LIMIT.saturating_sub(self.text.len() - range.len()) {
            self.limit_reached = true;
            return false;
        }
        if !self.pastes.is_empty() {
            let mut candidate = self.text.clone();
            candidate.replace_range(range.clone(), text);
            if self.resolve(candidate).is_none() {
                self.limit_reached = true;
                return false;
            }
        }
        self.limit_reached = false;
        self.text.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        self.revision += 1;
        true
    }
    pub fn resolved_text(&self) -> Option<String> {
        self.resolve(self.text.clone())
    }
    fn resolve(&self, mut text: String) -> Option<String> {
        for (marker, content) in &self.pastes {
            let count = text.matches(marker).count();
            let length = text
                .len()
                .saturating_sub(count * marker.len())
                .saturating_add(count.saturating_mul(content.len()));
            if length > composer::INPUT_LIMIT {
                return None;
            }
            text = text.replace(marker, content);
        }
        Some(text)
    }
    pub fn paste(&mut self, text: &str) {
        let normalized = bob_ui::terminal::normalize_paste(text);
        let text = normalized.as_ref();
        self.pastes.retain(|(marker, _)| self.text.contains(marker));
        let retained: usize = self.pastes.iter().map(|(_, text)| text.len()).sum();
        if text.len()
            > super::composer::INPUT_LIMIT.saturating_sub(
                self.resolved_text()
                    .map_or(composer::INPUT_LIMIT, |text| text.len()),
            )
            || text.len() > super::composer::INPUT_LIMIT.saturating_sub(retained)
        {
            self.limit_reached = true;
            return;
        }
        if !text.contains('\n') && text.chars().count() <= 120 {
            self.insert(text);
        } else {
            self.paste_sequence += 1;
            let marker = format!(
                "[Pasted text #{} +{} lines]",
                self.paste_sequence,
                text.split('\n').count()
            );
            if self.replace(self.cursor..self.cursor, &marker) {
                self.pastes.push((marker, text.to_owned()));
                self.history_index = None;
            }
        }
    }
    pub fn clear(&mut self) {
        let resolved = self.resolved_text().unwrap_or_default().trim().to_owned();
        if !resolved.is_empty() && resolved.len() <= 65536 && self.history.back() != Some(&resolved)
        {
            self.history.push_back(resolved);
            while self.history.len() > 50
                || self.history.iter().map(String::len).sum::<usize>() > 65536
            {
                self.history.pop_front();
            }
        }
        self.text = String::new();
        self.limit_reached = false;
        self.cursor = 0;
        self.scroll = 0;
        self.revision += 1;
        self.history_index = None;
        self.saved.clear();
        self.pastes.clear();
    }
    fn forget_history(&mut self) {
        self.history = VecDeque::new();
        self.history_index = None;
        self.saved = String::new();
    }
    fn history(&mut self, older: bool) {
        if self.history.is_empty() {
            return;
        }
        if self.history_index.is_none() {
            if !older {
                return;
            }
            self.saved = self.text.clone();
        }
        self.history_index = if older {
            Some(
                self.history_index
                    .map_or(self.history.len() - 1, |i| i.saturating_sub(1)),
            )
        } else {
            self.history_index
                .and_then(|i| (i + 1 < self.history.len()).then_some(i + 1))
        };
        self.text = self
            .history_index
            .map_or_else(|| self.saved.clone(), |i| self.history[i].clone());
        self.cursor = self.text.len();
        self.revision += 1;
    }
    fn word_left(&self) -> usize {
        let mut cursor = self.cursor;
        let mut word = false;
        for (offset, ch) in self.text[..self.cursor].char_indices().rev() {
            if word && ch.is_whitespace() {
                break;
            }
            word |= !ch.is_whitespace();
            cursor = offset;
        }
        cursor
    }
    fn word_right(&self) -> usize {
        let mut word = false;
        for (offset, ch) in self.text[self.cursor..].char_indices() {
            if word && ch.is_whitespace() {
                return self.cursor + offset;
            }
            word |= !ch.is_whitespace();
        }
        self.text.len()
    }
    pub fn key(&mut self, key: KeyEvent) {
        let old = self.text.clone();
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let code = match key.code {
            KeyCode::Char(ch) if control => KeyCode::Char(ch.to_ascii_lowercase()),
            other => other,
        };
        match code {
            KeyCode::Left if control || alt => self.cursor = self.word_left(),
            KeyCode::Right if control || alt => self.cursor = self.word_right(),
            KeyCode::Char('b') if alt => self.cursor = self.word_left(),
            KeyCode::Char('f') if alt => self.cursor = self.word_right(),
            KeyCode::Char('w') if control => {
                let start = self.word_left();
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Backspace if alt => {
                let start = self.word_left();
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Char('d') if alt => {
                let end = self.word_right();
                self.text.replace_range(self.cursor..end, "");
            }
            KeyCode::Char('p') if control => self.history(true),
            KeyCode::Char('n') if control => self.history(false),
            KeyCode::Char('j') if control => self.insert("\n"),
            KeyCode::Left | KeyCode::Char('b') if code == KeyCode::Left || control => {
                self.cursor = self.text[..self.cursor]
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(0, |(i, _)| i)
            }
            KeyCode::Right | KeyCode::Char('f') if code == KeyCode::Right || control => {
                self.cursor += self.text[self.cursor..]
                    .graphemes(true)
                    .next()
                    .map_or(0, str::len)
            }
            KeyCode::Home | KeyCode::Char('a') if code == KeyCode::Home || control => {
                self.cursor = 0
            }
            KeyCode::End | KeyCode::Char('e') if code == KeyCode::End || control => {
                self.cursor = self.text.len()
            }
            KeyCode::Char('u') if control => {
                self.text.replace_range(..self.cursor, "");
                self.cursor = 0;
            }
            KeyCode::Char('k') if control => self.text.truncate(self.cursor),
            KeyCode::Backspace | KeyCode::Char('\u{8}' | '\u{7f}' | 'h')
                if self.cursor > 0
                    && (code == KeyCode::Backspace
                        || control
                        || matches!(code, KeyCode::Char('\u{8}' | '\u{7f}'))) =>
            {
                let start = self.text[..self.cursor]
                    .grapheme_indices(true)
                    .next_back()
                    .unwrap()
                    .0;
                self.text.replace_range(start..self.cursor, "");
                self.cursor = start;
            }
            KeyCode::Delete | KeyCode::Char('d')
                if self.cursor < self.text.len() && (code == KeyCode::Delete || control) =>
            {
                let end = self.cursor
                    + self.text[self.cursor..]
                        .graphemes(true)
                        .next()
                        .unwrap()
                        .len();
                self.text.replace_range(self.cursor..end, "");
            }
            KeyCode::Up => self.history(true),
            KeyCode::Down => self.history(false),
            KeyCode::Char(c)
                if !c.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert(&c.to_string())
            }
            _ => {}
        }
        if self.text != old {
            self.limit_reached = false;
            self.revision += 1;
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Focus {
    #[default]
    Composer,
    Sidebar,
    Inspector,
    Workflow,
    Permission,
    PermissionInvalidated,
    Question,
    Plan,
    ControlInvalidated,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AnchorId {
    Activity,
    Entry(i64),
    Body(i64),
    BodyGap {
        sequence: i64,
        ordinal: usize,
    },
    Notice {
        id: u64,
        sequence: i64,
    },
    Live {
        instance: String,
        operation: OperationId,
    },
    Reasoning {
        instance: String,
        operation: OperationId,
    },
}
impl From<i64> for AnchorId {
    fn from(sequence: i64) -> Self {
        Self::Entry(sequence)
    }
}
impl AnchorId {
    pub fn sequence(&self) -> Option<i64> {
        match self {
            Self::Entry(sequence)
            | Self::Body(sequence)
            | Self::BodyGap { sequence, .. }
            | Self::Notice { sequence, .. } => Some(*sequence),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Anchor {
    pub identity: AnchorId,
    pub line: usize,
    pub offset: usize,
}
#[derive(Default)]
pub(super) struct Viewport {
    pub anchor: Option<Anchor>,
    pub rows: Vec<Anchor>,
    pub top: usize,
    pub height: usize,
    pending_scroll: isize,
    pending_follow: bool,
}
impl Viewport {
    pub fn scroll(&mut self, delta: isize) {
        let max = self.rows.len().saturating_sub(self.height);
        self.top = self.top.saturating_add_signed(delta).min(max);
        self.anchor = if self.top == max {
            None
        } else {
            self.rows.get(self.top).cloned()
        };
    }
    pub fn position(&mut self, height: usize) -> usize {
        let rows = &self.rows;
        let max = rows.len().saturating_sub(height);
        self.top = self
            .anchor
            .as_ref()
            .and_then(|anchor| {
                rows.iter()
                    .enumerate()
                    .filter(|(_, key)| {
                        if matches!(
                            anchor.identity,
                            AnchorId::Body(_)
                                | AnchorId::BodyGap { .. }
                                | AnchorId::Live { .. }
                                | AnchorId::Reasoning { .. }
                        ) {
                            (if matches!(
                                anchor.identity,
                                AnchorId::Body(_) | AnchorId::BodyGap { .. }
                            ) {
                                matches!(key.identity, AnchorId::Body(_))
                                    && key.identity.sequence() == anchor.identity.sequence()
                            } else {
                                key.identity == anchor.identity && key.line < usize::MAX - 5
                            }) && (key.offset, key.line) <= (anchor.offset, anchor.line)
                        } else {
                            key.identity == anchor.identity
                                && key.line == anchor.line
                                && key.offset <= anchor.offset
                        }
                    })
                    .map(|(i, _)| i)
                    .next_back()
                    .or_else(|| rows.iter().position(|key| key.identity == anchor.identity))
                    .or_else(|| {
                        let sequence = anchor.identity.sequence()?;
                        rows.iter()
                            .position(|key| key.identity.sequence().is_some_and(|s| s >= sequence))
                    })
            })
            .unwrap_or(max)
            .min(max);
        self.height = height;
        if self.pending_scroll != 0 {
            let old_top = self.top;
            self.top = self.top.saturating_add_signed(self.pending_scroll).min(max);
            self.pending_scroll -= self.top as isize - old_top as isize;
            self.anchor = if self.top == max && self.pending_follow {
                self.pending_scroll = 0;
                None
            } else {
                self.rows.get(self.top).cloned()
            };
        }
        self.top
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DecisionKey {
    pub thread: ThreadId,
    pub operation: OperationId,
    pub revision: i64,
}
impl From<&Decision> for DecisionKey {
    fn from(d: &Decision) -> Self {
        Self {
            thread: d.thread.clone(),
            operation: d.operation.clone(),
            revision: d.revision,
        }
    }
}
pub(super) struct Permission {
    pub decision: Decision,
    pub choice: Option<usize>,
    pub options: Vec<bob_kernel::ApprovalOption>,
    pub options_loaded: bool,
    pub options_pending: bool,
    pub scroll: u16,
    pub pending: bool,
    pub details: bool,
    pub rendered: Option<approval::Cache>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum InspectionTarget {
    Job(bob_kernel::JobId),
    Entry(i64),
    ToolOutput(OperationId),
    Artifact(ArtifactId),
}
pub(super) struct Inspector {
    pub target: InspectionTarget,
    pub previous: VecDeque<usize>,
    pub title: String,
    pub text: String,
    pub offset: usize,
    pub next: Option<usize>,
    pub total: usize,
    pub scroll: u16,
    pub pending: bool,
}
impl Inspector {
    pub fn navigation(&self, forward: bool) -> Option<(InspectionTarget, usize, VecDeque<usize>)> {
        if self.pending {
            return None;
        }
        let mut previous = self.previous.clone();
        let offset = if forward {
            let next = self.next?;
            previous.push_back(self.offset);
            if previous.len() > 256 {
                previous.pop_front();
            }
            next
        } else {
            previous.pop_back()?
        };
        Some((self.target.clone(), offset, previous))
    }
}
pub(super) struct QuestionPrompt {
    pub question: bob_kernel::HumanQuestion,
    pub selected: Option<usize>,
    pub other: Draft,
    pub pending: bool,
}
pub(super) struct PlanPrompt {
    pub plan: bob_kernel::PlanProposal,
    pub choice: Option<bool>,
    pub feedback: Draft,
    pub pending: bool,
    pub scroll: u16,
}
#[derive(Hash)]
pub(super) struct Notice {
    pub id: u64,
    pub sequence: i64,
    pub text: String,
}

#[derive(Default)]
pub(super) struct Pane {
    pub context: Option<bob_kernel::ContextSnapshot>,
    pub controls: Option<bob_kernel::ControlSnapshot>,
    pub session: Option<bob_kernel::SessionSnapshot>,
    pub jobs: Vec<bob_kernel::JobSummary>,
    pub queue: Vec<bob_kernel::QueuedInput>,
    pub submissions: HashMap<CommandId, bob_kernel::EntrySubmission>,
    pub submission_revision: u64,
    pub queue_sequence: i64,
    pub recalled: Option<String>,
    pub question: Option<QuestionPrompt>,
    pub plan: Option<PlanPrompt>,
    metadata_key: Option<(u64, i64)>,
    metadata_tracked: HashSet<CommandId>,
    metadata_pending: bool,
    metadata_epoch: u64,
    metadata_runtime: u64,
    answered: HashSet<(OperationId, i64)>,
    pub draft: Draft,
    history: history::History,
    pub before: Option<i64>,
    pub loaded_before: Option<Option<i64>>,
    pub force_refresh: bool,
    pub pinned_before: Option<i64>,
    pub rendered: Option<render::RenderCache>,
    pub inline: super::inline::Inline,
    pub content_key: Option<u64>,
    pub live_rendered: Option<live::RenderCache>,
    pub live_segments: live::Segments,
    pub committed_operations: HashSet<OperationId>,
    pub receipt: Option<(String, std::time::Instant)>,
    #[cfg(test)]
    pub render_builds: usize,
    pub newer: VecDeque<Option<i64>>,
    pub generation: u64,
    pub viewport: Viewport,
    pub view: Option<View>,
    pub entry_sequence: i64,
    pub notice: String,
    pub notices: VecDeque<Notice>,
    notice_id: u64,
    pub inspector: Option<Inspector>,
    pub inspector_rendered: Option<render::InspectorCache>,
    #[cfg(test)]
    pub inspector_builds: usize,
    pub inspection_epoch: u64,
    pub permission: Option<Permission>,
    pub decided: Option<DecisionKey>,
    pub expected_permission: Option<DecisionKey>,
    pub decision_confirmed: bool,
    pub decision_followup: bool,
    pub decision_refresh: bool,
    pub pending: Option<u64>,
    pub action_epoch: u64,
    pub expanded: HashSet<OperationId>,
    pub full_inputs: HashMap<OperationId, serde_json::Value>,
    pub input_errors: HashMap<OperationId, String>,
    pub pending_inputs: HashSet<OperationId>,
    pub expanded_outputs: HashMap<OperationId, TextPage>,
    pub pending_outputs: HashMap<OperationId, u64>,
    pub output_errors: HashMap<OperationId, String>,
    pub touched: u64,
}
impl Pane {
    pub(super) fn tracked_submissions(&mut self) -> Vec<CommandId> {
        let mut ids: HashSet<_> = self.queue.iter().map(|input| input.id.clone()).collect();
        if let Some(view) = &self.view {
            ids.extend(view.entries.iter().filter_map(|entry| {
                entry
                    .submission
                    .as_ref()
                    .map(|submission| submission.id.clone())
            }));
        }
        self.submissions.retain(|id, _| ids.contains(id));
        self.metadata_tracked
            .retain(|id| self.submissions.contains_key(id));
        ids.into_iter().collect()
    }

    pub fn notify_current(&mut self, text: impl Into<String>) {
        self.entry_sequence = self
            .entry_sequence
            .max(self.view.as_ref().map_or(0, |view| view.entry_sequence));
        self.notify(self.entry_sequence, text);
    }
    pub fn notify(&mut self, sequence: i64, text: impl Into<String>) {
        let text = clipped(&text.into(), 2000);
        if text.is_empty() {
            return;
        }
        self.notice.clone_from(&text);
        self.notice_id += 1;
        self.notices.push_back(Notice {
            id: self.notice_id,
            sequence,
            text,
        });
        if self.notices.len() > 64 {
            self.notices.pop_front();
        }
        self.rendered = None;
    }
    pub fn clear_notices(&mut self) {
        self.notice.clear();
        self.notices.clear();
        self.rendered = None;
    }
    pub fn visible_notices(&self) -> impl Iterator<Item = &Notice> {
        self.notices.iter().filter(|notice| {
            self.view.as_ref().is_some_and(|view| {
                let first = view.entries.first().map(|entry| entry.sequence);
                let before = self.before.or(self.pinned_before);
                (self.history.older_exhausted || first.is_none_or(|first| notice.sequence >= first))
                    && notice.sequence <= view.entry_sequence
                    && before.is_none_or(|before| notice.sequence < before)
            })
        })
    }
    pub fn invalidate_metadata(&mut self) {
        self.metadata_pending = false;
        self.metadata_key = None;
        self.metadata_epoch += 1;
    }
    pub fn scroll(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }
        let following = self.viewport.anchor.is_none();
        let old_top = self.viewport.top;
        if self.history.direction != Some(delta < 0) {
            self.history.debt = 0;
            self.viewport.pending_scroll = 0;
            self.viewport.pending_follow = false;
        }
        self.history.direction = Some(delta < 0);
        if live::scroll(self, delta) || super::inline::scroll(self, delta) {
            return;
        }
        self.viewport.scroll(delta);
        if self.viewport.anchor.is_none() && self.inline.focus.is_some() {
            self.inline.reset();
            self.rendered = None;
        }
        let travelled = self.viewport.top as isize - old_top as isize;
        if (delta < 0 && !self.history.older_exhausted) || (delta > 0 && self.before.is_some()) {
            self.history.debt = self
                .history
                .debt
                .saturating_add(delta - travelled)
                .clamp(-200, 200);
            if self.viewport.anchor.is_none() {
                self.viewport.anchor = self.viewport.rows.get(self.viewport.top).cloned();
            }
        }
        if self
            .viewport
            .anchor
            .as_ref()
            .is_none_or(|a| a.identity.sequence().is_none())
        {
            self.pinned_before = None;
        } else if following {
            self.pinned_before = self
                .view
                .as_ref()
                .and_then(|v| v.entries.last())
                .and_then(|e| e.sequence.checked_add(1));
        }
    }
    pub fn query_before(&self) -> Option<i64> {
        self.before.or_else(|| {
            self.viewport.anchor.as_ref().and_then(|anchor| {
                anchor.identity.sequence()?;
                self.pinned_before
                    .or_else(|| self.view.as_ref()?.entries.last()?.sequence.checked_add(1))
            })
        })
    }
}
#[derive(Clone)]
pub(super) enum HitAction {
    Entry(i64),
    Inline(i64, usize, usize),
    Tool(OperationId),
    WorkflowRow(usize, workflow::Action),
    WorkflowSidebar(workflow::Action),
    Output(OperationId),
    Thread(ThreadId),
    Permission,
    Live,
    Composer,
    Completion(usize),
    Picker(usize),
    Question,
    Plan,
}
pub(super) struct Hit {
    pub area: Rect,
    pub action: HitAction,
}
pub(super) struct App {
    pub titles: title::SessionTitles,
    pub workflows: workflow::WorkflowUi,
    pub handle: Option<Handle>,
    pub live_receiver:
        Option<tokio::sync::watch::Receiver<std::sync::Arc<bob_kernel::LiveSnapshot>>>,
    pub live_snapshot: Option<std::sync::Arc<bob_kernel::LiveSnapshot>>,
    pub live_instance: Option<String>,
    pub runtime_epoch: u64,
    pub started_at: std::time::Instant,
    pub activity: activity::Clock,
    pub threads: Vec<ThreadSummary>,
    pub thread_details: Vec<bob_kernel::ThreadDetails>,
    pub selected: Option<ThreadId>,
    pub panes: HashMap<ThreadId, Pane>,
    pub inline_cache: super::inline::Shared,
    pub loading_draft: Draft,
    pub status: String,
    pub startup_error: Option<String>,
    pub workspace: Option<PathBuf>,
    pub branch: Option<String>,
    pub services: Option<std::sync::Arc<bob::services::Services>>,
    pub provider: Option<bob::provider::ProviderService>,
    pub defaults: commands::DefaultsWriter,
    pub picker: Option<commands::Picker>,
    pub theme_name: String,
    theme_preview: Option<String>,
    pending_theme: Option<(ThreadId, u64)>,
    pub model: Option<String>,
    pub completion: completion::Completion,
    pub custom_commands: Vec<bob_core::core::commands::CustomCommand>,
    files: Vec<String>,
    files_loading: bool,
    files_loaded: bool,
    workspace_epoch: u64,
    workspace_loading: bool,
    workspace_loaded: bool,
    pub sidebar: bool,
    pub sidebar_scroll: usize,
    pub show_todos: bool,
    pub focus: Focus,
    pub focus_epoch: u64,
    pub page_size: usize,
    pub reading: bool,
    reading_request: Option<(ThreadId, u64)>,
    pub sequence_floor: i64,
    pub hits: Vec<Hit>,
    pub size: Rect,
    clock: u64,
    output_epoch: u64,
}
impl App {
    pub fn new(page_size: usize) -> Self {
        Self {
            titles: title::SessionTitles::default(),
            workflows: workflow::WorkflowUi::default(),
            handle: None,
            live_receiver: None,
            live_snapshot: None,
            live_instance: None,
            runtime_epoch: 0,
            started_at: std::time::Instant::now(),
            activity: activity::Clock::default(),
            threads: Vec::new(),
            thread_details: Vec::new(),
            selected: None,
            panes: HashMap::new(),
            inline_cache: Default::default(),
            loading_draft: Draft::default(),
            status: "Initializing Copilot and kernel; you can type while loading.".into(),
            startup_error: None,
            workspace: None,
            branch: None,
            services: None,
            provider: None,
            defaults: commands::DefaultsWriter::default(),
            picker: None,
            theme_name: "dark".into(),
            theme_preview: None,
            pending_theme: None,
            model: None,
            completion: completion::Completion::default(),
            custom_commands: Vec::new(),
            files: Vec::new(),
            files_loading: false,
            files_loaded: false,
            workspace_epoch: 0,
            workspace_loading: false,
            workspace_loaded: false,
            sidebar: false,
            sidebar_scroll: 0,
            show_todos: true,
            focus: Focus::Composer,
            focus_epoch: 0,
            page_size: page_size.clamp(1, 200),
            reading: false,
            reading_request: None,
            sequence_floor: 0,
            hits: Vec::new(),
            size: Rect::default(),
            clock: 0,
            output_epoch: 0,
        }
    }
    pub fn set_services(&mut self, services: std::sync::Arc<bob::services::Services>) {
        self.services = Some(services);
    }
    pub fn configure(&mut self, workspace: PathBuf, model: String) {
        self.dismiss_picker();
        self.workspace = Some(workspace);
        self.sidebar_scroll = 0;
        self.branch = None;
        self.model = (!model.is_empty()).then_some(model);
        self.workspace_epoch += 1;
        self.workspace_loaded = false;
        self.workspace_loading = false;
        self.files.clear();
        self.files_loading = false;
        self.files_loaded = false;
        self.custom_commands.clear();
        self.refresh_completion();
    }
    fn apply_theme(&mut self, name: &str) {
        bob_ui::theme::set_theme(bob_ui::theme::Theme::by_name(name));
        for pane in self.panes.values_mut() {
            pane.rendered = None;
            pane.live_rendered = None;
            if let Some(permission) = &mut pane.permission {
                permission.rendered = None;
            }
        }
    }
    pub fn dismiss_picker(&mut self) {
        self.picker = None;
        self.pending_theme = None;
        if let Some(original) = self.theme_preview.take() {
            self.apply_theme(&original);
        }
    }
    pub(super) fn focus_active_prompt(&mut self) {
        let Some(pane) = self.pane() else { return };
        let focus = if pane.permission.is_some() {
            if self.focus == Focus::PermissionInvalidated {
                Focus::PermissionInvalidated
            } else {
                Focus::Permission
            }
        } else if pane.question.is_some() || pane.plan.is_some() {
            if self.focus == Focus::ControlInvalidated {
                Focus::ControlInvalidated
            } else if pane.plan.is_some() && (self.focus == Focus::Plan || pane.question.is_none())
            {
                Focus::Plan
            } else {
                Focus::Question
            }
        } else {
            return;
        };
        self.dismiss_picker();
        self.focus = focus;
    }
    fn open_picker(&mut self, picker: commands::Picker) {
        self.dismiss_picker();
        self.picker = Some(if picker.is_theme() {
            self.theme_preview = Some(self.theme_name.clone());
            commands::Picker::theme(&self.theme_name)
        } else {
            picker
        });
    }
    fn preview_theme(&mut self) {
        let name = self
            .picker
            .as_ref()
            .filter(|picker| picker.is_theme())
            .and_then(|picker| picker.items.get(picker.selected))
            .map(|item| item.label.clone());
        if let Some(name) = name {
            self.apply_theme(&name);
        }
    }
    fn confirm_picker(&mut self, jobs: &mut JoinSet<Reply>) -> bool {
        let command = self.picker.take().and_then(|picker| picker.command());
        if let Some(command) = command.filter(|_| self.draft_mut().text.is_empty()) {
            self.draft_mut().insert(&command);
            let exit = self.submit(jobs);
            if self.pending_theme.is_none() {
                self.dismiss_picker();
            }
            exit
        } else {
            self.dismiss_picker();
            false
        }
    }
    pub fn refresh_completion(&mut self) {
        let mut completion = std::mem::take(&mut self.completion);
        let draft = self.pane().map_or(&self.loading_draft, |pane| &pane.draft);
        completion.refresh(draft, &self.custom_commands, &self.files);
        self.completion = completion;
    }
    fn complete(&mut self) -> Option<completion::Kind> {
        let mut completion = std::mem::take(&mut self.completion);
        let accepted = completion.accept(self.draft_mut());
        self.completion = completion;
        accepted
    }
    fn load_workspace(&mut self, jobs: &mut JoinSet<Reply>) {
        if !self.files_loading && !self.files_loaded && self.workspace_loaded {
            let has_token =
                completion::at_token(self.pane().map_or(&self.loading_draft, |pane| &pane.draft))
                    .is_some();
            if has_token {
                if let Some(workspace) = self.workspace.clone() {
                    self.files_loading = true;
                    let epoch = self.workspace_epoch;
                    jobs.spawn(async move {
                        Reply::Files {
                            epoch,
                            files: completion::gather_files(&workspace).await,
                        }
                    });
                }
            }
        }
        if self.workspace_loading || self.workspace_loaded {
            return;
        }
        let Some(workspace) = self.workspace.clone() else {
            return;
        };
        let epoch = self.workspace_epoch;
        self.workspace_loading = true;
        jobs.spawn(async move {
            let branch = completion::branch(&workspace).await;
            let commands = tokio::task::spawn_blocking(move || {
                bob_core::core::commands::load_custom_commands(&workspace)
            })
            .await
            .unwrap_or_default();
            Reply::Workspace {
                epoch,
                files: Vec::new(),
                commands,
                branch,
            }
        });
    }
    pub fn pane(&self) -> Option<&Pane> {
        self.selected.as_ref().and_then(|id| self.panes.get(id))
    }
    pub fn pane_mut(&mut self) -> Option<&mut Pane> {
        self.selected.as_ref().and_then(|id| self.panes.get_mut(id))
    }
    pub fn draft_mut(&mut self) -> &mut Draft {
        if let Some(id) = &self.selected {
            &mut self.panes.entry(id.clone()).or_default().draft
        } else {
            &mut self.loading_draft
        }
    }
    pub fn select(&mut self, id: ThreadId) {
        let session = |id: &ThreadId| {
            self.threads
                .iter()
                .find(|thread| &thread.id == id)
                .map(|thread| &thread.session)
        };
        if self.selected.as_ref().and_then(session) != session(&id) {
            self.sidebar_scroll = 0;
        }
        if let Some(session) = session(&id).cloned() {
            self.titles.select(session);
        }
        let switched = self.selected.as_ref() != Some(&id);
        if switched {
            self.dismiss_picker();
            self.dismiss_inspector();
        }
        if let Some(permission) = self.pane_mut().and_then(|p| p.permission.as_mut()) {
            permission.choice = None;
        }
        self.clock += 1;
        let pane = self.panes.entry(id.clone()).or_default();
        pane.touched = self.clock;
        pane.inline.cache = self.inline_cache.clone();
        self.selected = Some(id.clone());
        self.workflow_conversation_selected(id);
        if let Some(permission) = self.pane_mut().and_then(|p| p.permission.as_mut()) {
            permission.choice = None;
        }
        self.hits.clear();
        if self.pane().is_some_and(|pane| pane.permission.is_some()) {
            self.focus = Focus::PermissionInvalidated;
        } else if self.focus == Focus::Permission
            || (switched && self.focus == Focus::PermissionInvalidated)
        {
            self.focus = Focus::Composer;
        }
        if matches!(
            self.focus,
            Focus::Question | Focus::Plan | Focus::ControlInvalidated
        ) {
            self.focus = Focus::Composer;
        }
        if let Some(pane) = self.pane_mut() {
            if let Some(question) = &mut pane.question {
                question.selected = None;
            }
            if let Some(plan) = &mut pane.plan {
                plan.choice = None;
            }
        }
        self.evict();
        self.refresh_completion();
        self.focus_active_prompt();
    }
    fn evict(&mut self) {
        let mut cached: Vec<_> = self
            .panes
            .iter()
            .filter(|(id, pane)| {
                Some(*id) != self.selected.as_ref()
                    && (pane.view.is_some() || pane.rendered.is_some())
            })
            .map(|(id, pane)| (pane.touched, id.clone()))
            .collect();
        cached.sort_by_key(|(age, _)| *age);
        let count = cached.len().saturating_sub(7);
        for (_, id) in cached.into_iter().take(count) {
            let pane = self.panes.get_mut(&id).unwrap();
            if pane.viewport.anchor.is_some() {
                pane.pinned_before = pane.query_before();
            }
            pane.view = None;
            pane.rendered = None;
            pane.inline.epoch = pane.inline.epoch.wrapping_add(1);
            pane.inline.invalidate();
            pane.content_key = None;
            pane.live_rendered = None;
            pane.live_segments = Default::default();
            pane.committed_operations.clear();
            pane.viewport.rows = Vec::new();
            pane.expanded = HashSet::new();
            pane.full_inputs.clear();
            pane.input_errors.clear();
            pane.pending_inputs.clear();
            pane.expanded_outputs.clear();
            pane.pending_outputs.clear();
            pane.output_errors.clear();
            pane.jobs.clear();
            pane.queue.clear();
            pane.submissions.clear();
            pane.submission_revision += 1;
            pane.queue_sequence = 0;
            pane.invalidate_metadata();
            pane.newer = VecDeque::new();
            pane.history = history::History::default();
            pane.loaded_before = None;
            pane.draft.forget_history();
            pane.draft.layout = None;
            pane.inspector = None;
            pane.inspector_rendered = None;
            pane.permission = None;
            pane.generation += 1;
        }
        self.panes.retain(|id, pane| {
            Some(id) == self.selected.as_ref()
                || pane.view.is_some()
                || !pane.draft.text.is_empty()
                || pane.pending.is_some()
                || pane.recalled.is_some()
                || !pane.notices.is_empty()
                || !pane.notice.is_empty()
                || pane.decided.is_some()
                || pane.viewport.anchor.is_some()
                || pane.before.is_some()
        });
    }
    pub fn select_initial(&mut self, thread: ThreadId) {
        self.select(thread.clone());
        self.panes.get_mut(&thread).unwrap().draft = std::mem::take(&mut self.loading_draft);
    }
    pub fn visible_threads(&self) -> Vec<&ThreadSummary> {
        let session = self
            .threads
            .iter()
            .find(|t| Some(&t.id) == self.selected.as_ref())
            .map(|t| &t.session);
        let mut threads: Vec<_> = self
            .threads
            .iter()
            .filter(|t| session == Some(&t.session))
            .collect();
        threads.reverse();
        let launches: HashMap<_, _> = self
            .thread_details
            .iter()
            .map(|details| (&details.thread, details.launch_sequence))
            .collect();
        threads.sort_by_key(|t| {
            (
                t.parent_thread.is_some(),
                style::terminal_thread(&t.state),
                launches.get(&t.id).copied().unwrap_or(0),
            )
        });
        threads
    }
    pub fn cycle(&mut self, step: isize) {
        let threads = self.visible_threads();
        if threads.is_empty() {
            return;
        }
        let index = threads
            .iter()
            .position(|t| Some(&t.id) == self.selected.as_ref())
            .unwrap_or(0);
        let next = (index as isize + step).rem_euclid(threads.len() as isize) as usize;
        let id = threads[next].id.clone();
        self.select(id);
    }
    pub fn dismiss_inspector(&mut self) {
        if let Some(pane) = self.pane_mut() {
            pane.inspection_epoch += 1;
            pane.inspector = None;
            pane.inspector_rendered = None;
        }
        if self.focus == Focus::Inspector {
            self.focus = Focus::Composer;
        }
        self.hits.clear();
    }
    pub fn page(&mut self, older: bool) {
        self.dismiss_inspector();
        if let Some(pane) = self.pane_mut() {
            let step = pane.viewport.height.saturating_sub(2).max(1) as isize;
            pane.scroll(if older { -step } else { step });
        }
    }
    fn refresh_metadata(&mut self, jobs: &mut JoinSet<Reply>) {
        let (Some(handle), Some(thread)) = (self.handle.clone(), self.selected.clone()) else {
            return;
        };
        let Some(summary) = self.threads.iter().find(|summary| summary.id == thread) else {
            return;
        };
        let session = summary.session.clone();
        let sequence = self.sequence_floor.max(
            self.live_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.sequence),
        );
        let runtime = self.runtime_epoch;
        let pane = self.panes.get_mut(&thread).unwrap();
        let key = (runtime, sequence);
        let tracked = pane.tracked_submissions();
        let tracked_set = tracked.iter().cloned().collect();
        if pane.metadata_pending
            || (pane.metadata_key == Some(key) && pane.metadata_tracked == tracked_set)
        {
            return;
        }
        pane.metadata_tracked = tracked_set;
        pane.metadata_pending = true;
        pane.metadata_key = Some(key);
        pane.metadata_epoch += 1;
        let epoch = pane.metadata_epoch;
        spawn_reply(jobs, runtime, async move {
            let result = async {
                let context = handle.context(thread.clone()).await?;
                let controls = handle.controls(thread.clone()).await?;
                let details = handle.thread_details(session.clone()).await?;
                let jobs = handle.jobs(session.clone()).await?;
                let queue = handle.queue_snapshot(thread.clone(), tracked).await?;
                let session = handle.session(session).await?;
                Ok::<_, anyhow::Error>(Metadata {
                    context,
                    controls,
                    session,
                    details,
                    jobs,
                    queue,
                })
            }
            .await
            .map_err(|error| format!("{error:#}"));
            Reply::Metadata {
                thread,
                key,
                epoch,
                result,
            }
        });
    }
    fn invalidate_control_focus(&mut self) {
        let runtime = self.runtime_epoch;
        let stale = self
            .pane()
            .is_some_and(|pane| pane.metadata_runtime != runtime);
        if stale {
            if let Some(pane) = self.pane_mut() {
                pane.question = None;
                pane.plan = None;
                pane.metadata_pending = false;
                pane.metadata_runtime = runtime;
                pane.metadata_key = None;
            }
            if matches!(self.focus, Focus::Question | Focus::Plan) {
                self.focus = Focus::ControlInvalidated;
            }
        }
    }
    fn answer_control(&mut self, jobs: &mut JoinSet<Reply>) {
        self.invalidate_control_focus();
        let (Some(handle), Some(thread)) = (self.handle.clone(), self.selected.clone()) else {
            return;
        };
        let focus = self.focus;
        let pane = self.panes.get_mut(&thread).unwrap();
        let (operation, revision, command) = match focus {
            Focus::Question => {
                let Some(prompt) = pane.question.as_mut().filter(|prompt| !prompt.pending) else {
                    return;
                };
                let Some(selected) = prompt.selected else {
                    return;
                };
                let Some(answer) = (if selected < prompt.question.options.len() {
                    Some(prompt.question.options[selected].clone())
                } else {
                    prompt.other.resolved_text()
                }) else {
                    return;
                };
                if answer.trim().is_empty() || answer.len() > composer::INPUT_LIMIT {
                    return;
                }
                prompt.pending = true;
                (
                    prompt.question.operation.clone(),
                    prompt.question.revision,
                    Command::AnswerQuestion {
                        id: CommandId::new(),
                        operation: prompt.question.operation.clone(),
                        revision: prompt.question.revision,
                        answer,
                    },
                )
            }
            Focus::Plan => {
                let Some(prompt) = pane.plan.as_mut().filter(|prompt| !prompt.pending) else {
                    return;
                };
                let Some(approve) = prompt.choice else {
                    return;
                };
                let Some(feedback) = prompt.feedback.resolved_text() else {
                    return;
                };
                if feedback.len() > composer::INPUT_LIMIT {
                    return;
                }
                prompt.pending = true;
                (
                    prompt.plan.operation.clone(),
                    prompt.plan.revision,
                    Command::DecidePlan {
                        id: CommandId::new(),
                        operation: prompt.plan.operation.clone(),
                        revision: prompt.plan.revision,
                        approve,
                        feedback: (!feedback.trim().is_empty()).then_some(feedback),
                    },
                )
            }
            _ => return,
        };
        if !pane.answered.insert((operation.clone(), revision)) {
            return;
        }
        pane.metadata_epoch += 1;
        pane.metadata_pending = false;
        self.focus = Focus::ControlInvalidated;
        spawn_reply(jobs, self.runtime_epoch, async move {
            Reply::HumanDecision {
                thread,
                operation,
                revision,
                result: handle
                    .command(command)
                    .await
                    .map_err(|error| format!("{error:#}")),
            }
        });
    }
    fn refresh_approval_options(&mut self, jobs: &mut JoinSet<Reply>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let Some(permission) = self.pane_mut().and_then(|pane| pane.permission.as_mut()) else {
            return;
        };
        if permission.pending || permission.options_pending || permission.options_loaded {
            return;
        }
        permission.options_pending = true;
        let key = DecisionKey::from(&permission.decision);
        spawn_reply(jobs, self.runtime_epoch, async move {
            let result = handle
                .approval_options(key.operation.clone(), key.revision)
                .await
                .map_err(|error| format!("{error:#}"));
            Reply::ApprovalOptions { key, result }
        });
    }
    pub fn refresh(&mut self, jobs: &mut JoinSet<Reply>) {
        self.refresh_inline(jobs);
        self.refresh_workflows(jobs);
        self.refresh_expanded_outputs(jobs);
        self.refresh_approval_options(jobs);
        self.invalidate_control_focus();
        self.refresh_metadata(jobs);
        self.load_workspace(jobs);
        if self.reading {
            return;
        }
        let (Some(handle), Some(thread), Some(pane)) = (&self.handle, &self.selected, self.pane())
        else {
            return;
        };
        if !pane.force_refresh
            && !pane.decision_refresh
            && pane.history_query().is_none()
            && self.live_receiver.is_some()
            && pane.loaded_before == Some(pane.query_before())
            && pane.view.as_ref().is_some_and(|view| {
                view.sequence
                    >= self.sequence_floor.max(
                        self.live_snapshot
                            .as_ref()
                            .map_or(0, |snapshot| snapshot.sequence),
                    )
            })
        {
            return;
        }
        let handle = handle.clone();
        let thread = thread.clone();
        let query = pane.history_query().unwrap_or_else(|| pane.refresh_query());
        let limit = self.page_size.min(100);
        self.reading = true;
        let pane = self.panes.get_mut(&thread).unwrap();
        if pane.view.is_none() {
            pane.history = history::History::default();
        }
        pane.generation += 1;
        let generation = pane.generation;
        pane.history.pending = Some(query);
        pane.loaded_before = Some(pane.query_before());
        self.reading_request = Some((thread.clone(), generation));
        spawn_reply(jobs, self.runtime_epoch, async move {
            let result = query
                .read(&handle, thread.clone(), limit)
                .await
                .map_err(|e| format!("{e:#}"));
            Reply::View {
                thread,
                generation,
                result,
            }
        });
    }
    pub(super) fn application_shortcut(
        &mut self,
        request: commands::Request,
        jobs: &mut JoinSet<Reply>,
    ) {
        let (Some(handle), Some(thread)) = (self.handle.clone(), self.selected.clone()) else {
            return;
        };
        let Some(summary) = self
            .threads
            .iter()
            .find(|summary| summary.id == thread)
            .cloned()
        else {
            return;
        };
        let pane = self.panes.get_mut(&thread).unwrap();
        if pane.pending.is_some() {
            return;
        }
        pane.action_epoch += 1;
        let epoch = pane.action_epoch;
        pane.pending = Some(epoch);
        let workspace = self.workspace.clone();
        let services = self.services.clone();
        let provider = self.provider.clone();
        let defaults = self.defaults.clone();
        spawn_reply(jobs, self.runtime_epoch, async move {
            let result = commands::execute_with_defaults(
                request, &handle, &summary, workspace, services, provider, &defaults,
            )
            .await
            .map_err(|error| format!("{error:#}"));
            Reply::Shortcut {
                thread,
                epoch,
                result,
            }
        });
    }
    pub fn submit(&mut self, jobs: &mut JoinSet<Reply>) -> bool {
        self.submit_with_mode(jobs, false)
    }
    fn submit_with_mode(&mut self, jobs: &mut JoinSet<Reply>, steer: bool) -> bool {
        let text = self.draft_mut().text.clone();
        let Some(resolved) = self.draft_mut().resolved_text() else {
            self.draft_mut().limit_reached = true;
            return false;
        };
        if resolved.len() > super::composer::INPUT_LIMIT {
            self.draft_mut().limit_reached = true;
            return false;
        }
        if matches!(text.trim(), "/quit" | "/exit") {
            return true;
        }
        let (Some(handle), Some(thread)) = (&self.handle, &self.selected) else {
            self.status = "Not ready; draft preserved. Ctrl+C exits.".into();
            return false;
        };
        let handle = handle.clone();
        let thread = thread.clone();
        let pane = self.panes.get_mut(&thread).unwrap();
        if pane.pending.is_some() || text.trim().is_empty() {
            return false;
        }
        let Some(summary) = self.threads.iter().find(|s| s.id == thread) else {
            return false;
        };
        let policy = pane
            .view
            .as_ref()
            .and_then(|v| v.policies.iter().find(|p| p.session == summary.session));
        let workspace = self.workspace.clone();
        let services = self.services.clone();
        let provider = self.provider.clone();
        let defaults = self.defaults.clone();
        let action = match commands::route(&resolved, summary, policy, &self.custom_commands) {
            Ok(action) => action,
            Err(e) => {
                pane.notify_current(format!("{e:#}"));
                return false;
            }
        };
        let action =
            if steer && !text.trim_start().starts_with('/') && summary.state == ThreadState::Active
            {
                match action {
                    Action::Command(Command::Submit { id, thread, text }) => {
                        Action::Command(Command::Steer { id, thread, text })
                    }
                    other => other,
                }
            } else {
                action
            };
        let summary = summary.clone();
        pane.action_epoch += 1;
        let epoch = pane.action_epoch;
        pane.pending = Some(epoch);
        if matches!(
            &action,
            Action::Application(commands::Request::Theme(Some(_)))
        ) {
            self.pending_theme = Some((thread.clone(), epoch));
        }
        let focus_epoch = self.focus_epoch;
        let revision = pane.draft.revision;
        let inspection_epoch = pane.inspection_epoch;
        pane.receipt = Some(("Sending…".into(), std::time::Instant::now()));
        spawn_reply(jobs, self.runtime_epoch, async move {
            let result = match action {
                Action::Command(command) => {
                    let yolo = match &command {
                        Command::SetPolicy { policy, .. } if text.trim() == "/yolo" => {
                            Some(policy.yolo)
                        }
                        _ => None,
                    };
                    handle.command(command).await.map(|mut receipt| {
                        if let Some(enabled) = yolo {
                            receipt.detail = if enabled {
                                "YOLO on — permission prompts bypassed; explicit denials and plan mode still apply. /yolo to stop."
                            } else {
                                "YOLO off — permission prompts restored."
                            }.into();
                        }
                        Outcome::Receipt(receipt)
                    })
                },
                Action::Application(request) => {
                    commands::execute_with_defaults(request, &handle, &summary, workspace, services, provider, &defaults)
                        .await
                }
                Action::Artifact(id, offset) => handle
                    .artifact(id.clone(), offset, 16384)
                    .await
                    .map(|text| Outcome::Artifact(id, offset, text)),
            }
            .map_err(|e| format!("{e:#}"));
            Reply::Command {
                thread,
                submitted: text,
                revision,
                focus_epoch,
                epoch,
                inspection_epoch,
                result,
            }
        });
        false
    }
    pub fn take_decision(&mut self, approve: bool) -> Option<(DecisionKey, Command)> {
        let index = self
            .pane()?
            .permission
            .as_ref()?
            .options
            .iter()
            .position(|option| option.approve == approve && option.grant.is_none())?;
        self.take_option(index)
    }
    pub fn take_option(&mut self, index: usize) -> Option<(DecisionKey, Command)> {
        let pane = self.pane_mut()?;
        let permission = pane.permission.as_mut()?;
        let option = permission.options.get(index)?.clone();
        if permission.pending || !permission.options_loaded {
            return None;
        }
        let key = DecisionKey::from(&permission.decision);
        if pane.decided.as_ref() == Some(&key) {
            return None;
        }
        permission.pending = true;
        pane.decided = Some(key.clone());
        pane.decision_confirmed = false;
        pane.decision_followup = true;
        pane.expected_permission = pane.view.as_ref().and_then(|view| {
            view.decisions
                .iter()
                .find(|decision| {
                    decision.thread == key.thread && DecisionKey::from(*decision) != key
                })
                .map(DecisionKey::from)
        });
        Some((
            key.clone(),
            Command::DecideWithGrant {
                id: CommandId::new(),
                operation: key.operation,
                revision: key.revision,
                approve: option.approve,
                grant: option.grant,
            },
        ))
    }
    fn decide(&mut self, approve: bool, jobs: &mut JoinSet<Reply>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let Some((key, command)) = self.take_decision(approve) else {
            return;
        };
        self.focus = Focus::PermissionInvalidated;
        spawn_reply(jobs, self.runtime_epoch, async move {
            Reply::Decision {
                key,
                result: handle.command(command).await.map_err(|e| format!("{e:#}")),
            }
        });
    }
    fn confirm_option(&mut self, index: usize, jobs: &mut JoinSet<Reply>) {
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let Some((key, command)) = self.take_option(index) else {
            return;
        };
        self.focus = Focus::PermissionInvalidated;
        spawn_reply(jobs, self.runtime_epoch, async move {
            Reply::Decision {
                key,
                result: handle
                    .command(command)
                    .await
                    .map_err(|error| format!("{error:#}")),
            }
        });
    }
    fn sync_permission(
        pane: &mut Pane,
        thread: &ThreadId,
        decisions: &[Decision],
        focus: Option<&mut Focus>,
    ) {
        let decision = decisions
            .iter()
            .find(|d| &d.thread == thread && pane.decided.as_ref() != Some(&DecisionKey::from(*d)));
        let previous = pane
            .permission
            .as_ref()
            .map(|p| DecisionKey::from(&p.decision));
        let next = decision.map(DecisionKey::from);
        if previous == next {
            return;
        }
        if pane.decision_followup {
            let submitted = previous == pane.decided
                && pane
                    .permission
                    .as_ref()
                    .is_some_and(|permission| permission.pending);
            if let Some(next) = &next {
                let expected = pane.expected_permission.as_ref() == Some(next);
                let subsequent = pane.expected_permission.is_none()
                    && (previous.is_none() || submitted)
                    && pane
                        .decided
                        .as_ref()
                        .is_some_and(|decided| decided.operation != next.operation);
                if expected || subsequent {
                    pane.expected_permission = Some(next.clone());
                } else {
                    pane.expected_permission = None;
                    pane.decision_followup = false;
                }
            } else if previous.is_some() && !submitted {
                pane.expected_permission = None;
                pane.decision_followup = false;
            }
        }
        let expected = next.is_some() && pane.expected_permission == next && pane.decision_followup;
        let fresh = previous.is_none()
            && next.as_ref().is_some_and(|next| {
                pane.decided
                    .as_ref()
                    .is_none_or(|decided| decided.operation != next.operation)
            });
        if let Some(focus) = focus {
            *focus = if (expected && pane.decision_confirmed)
                || (fresh && *focus != Focus::PermissionInvalidated)
            {
                Focus::Permission
            } else {
                Focus::PermissionInvalidated
            };
        }
        if expected && pane.decision_confirmed {
            pane.expected_permission = None;
            pane.decision_followup = false;
        }
        pane.permission = decision.cloned().map(|decision| Permission {
            decision,
            choice: None,
            options: Vec::new(),
            options_loaded: false,
            options_pending: false,
            scroll: 0,
            pending: false,
            details: false,
            rendered: None,
        });
    }
    fn focus_permission(&mut self) {
        if let Some(permission) = self.pane_mut().and_then(|pane| pane.permission.as_mut()) {
            if permission.pending {
                return;
            }
            if permission.options_loaded && permission.choice.is_none() {
                permission.choice = (!permission.options.is_empty()).then_some(0);
            }
            self.focus = Focus::Permission;
        }
    }
    pub fn inspect(&mut self, sequence: i64, offset: usize, jobs: &mut JoinSet<Reply>) {
        self.open_inspector(
            InspectionTarget::Entry(sequence),
            offset,
            VecDeque::new(),
            jobs,
        );
    }
    pub fn toggle_tool(&mut self, id: OperationId, jobs: &mut JoinSet<Reply>) {
        let handle = self.handle.clone();
        let Some(thread) = self.selected.clone() else {
            return;
        };
        let pane = self.panes.get_mut(&thread).unwrap();
        pane.rendered = None;
        if pane.expanded.remove(&id) {
            pane.full_inputs.remove(&id);
            pane.input_errors.remove(&id);
            pane.pending_inputs.remove(&id);
            pane.expanded_outputs.remove(&id);
            pane.pending_outputs.remove(&id);
            pane.output_errors.remove(&id);
            return;
        }
        pane.expanded.insert(id.clone());
        let tool = pane.view.as_ref().and_then(|view| {
            view.entries
                .iter()
                .find_map(|entry| entry.tool.as_ref().filter(|tool| tool.id == id))
        });
        let shell = tool.is_some_and(|tool| tool.name == "bash");
        let Some(handle) = handle else {
            return;
        };
        if shell {
            pane.pending_inputs.insert(id.clone());
            spawn_reply(jobs, self.runtime_epoch, async move {
                let result = handle
                    .tool_input(id.clone())
                    .await
                    .map_err(|error| format!("{error:#}"));
                Reply::ToolInput {
                    thread,
                    operation: id,
                    result,
                }
            });
        } else {
            self.refresh_expanded_outputs(jobs);
        }
    }
    fn refresh_expanded_outputs(&mut self, jobs: &mut JoinSet<Reply>) {
        let (Some(handle), Some(thread)) = (self.handle.clone(), self.selected.clone()) else {
            return;
        };
        let pane = self.panes.get_mut(&thread).unwrap();
        if pane.expanded.is_empty() {
            return;
        }
        let Some(view) = &pane.view else {
            return;
        };
        for tool in view.entries.iter().filter_map(|entry| entry.tool.as_ref()) {
            let id = &tool.id;
            if !pane.expanded.contains(id)
                || pane.expanded_outputs.contains_key(id)
                || pane.pending_outputs.contains_key(id)
                || pane.output_errors.contains_key(id)
                || !matches!(tool.name.as_str(), "grep" | "glob" | "list_dir")
                || tool.result.is_none()
                || !matches!(
                    tool.state,
                    OperationState::Done
                        | OperationState::Failed
                        | OperationState::Cancelled
                        | OperationState::Unknown
                        | OperationState::Resolved
                        | OperationState::Interrupted
                )
            {
                continue;
            }
            self.output_epoch += 1;
            let epoch = self.output_epoch;
            pane.pending_outputs.insert(id.clone(), epoch);
            pane.rendered = None;
            let handle = handle.clone();
            let thread = thread.clone();
            let operation = id.clone();
            spawn_reply(jobs, self.runtime_epoch, async move {
                let result = handle
                    .tool_output(operation.clone(), 0, 16384)
                    .await
                    .map_err(|error| format!("{error:#}"));
                Reply::ToolOutput {
                    thread,
                    operation,
                    epoch,
                    result,
                }
            });
        }
    }
    pub fn tool_input_inspection(&self, id: &OperationId) -> Option<InspectionTarget> {
        self.pane()?
            .view
            .as_ref()?
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::ToolCall && entry.operation.as_ref() == Some(id))
            .map(|entry| InspectionTarget::Entry(entry.sequence))
    }
    pub fn tool_inspection(&self, id: &OperationId) -> Option<InspectionTarget> {
        let view = self.pane()?.view.as_ref()?;
        if view.entries.iter().any(|entry| {
            entry.operation.as_ref() == Some(id)
                && (matches!(entry.kind, EntryKind::ToolResult | EntryKind::ToolError)
                    || entry
                        .tool
                        .as_ref()
                        .is_some_and(|tool| tool.result.is_some()))
        }) {
            return Some(InspectionTarget::ToolOutput(id.clone()));
        }
        let call = view
            .entries
            .iter()
            .find(|e| e.operation.as_ref() == Some(id))?;
        Some(InspectionTarget::Entry(call.sequence))
    }
    pub fn open_inspector(
        &mut self,
        target: InspectionTarget,
        offset: usize,
        previous: VecDeque<usize>,
        jobs: &mut JoinSet<Reply>,
    ) {
        let (Some(handle), Some(thread)) = (self.handle.clone(), self.selected.clone()) else {
            return;
        };
        let pane = self.panes.get_mut(&thread).unwrap();
        pane.inspection_epoch += 1;
        let epoch = pane.inspection_epoch;
        let title = match &target {
            InspectionTarget::Entry(sequence) => {
                let detail = pane
                    .view
                    .as_ref()
                    .and_then(|v| v.entries.iter().find(|e| e.sequence == *sequence))
                    .map(|entry| {
                        format!(
                            "{} · operation {} · artifact {}",
                            entry.kind,
                            entry.operation.as_ref().map_or("none", |id| id.0.as_str()),
                            entry.artifact.as_ref().map_or("none", |id| id.0.as_str())
                        )
                    })
                    .unwrap_or_default();
                format!("Entry {sequence} · {thread}\n{detail}")
            }
            InspectionTarget::ToolOutput(id) => {
                let name = pane
                    .view
                    .as_ref()
                    .and_then(|view| {
                        view.entries
                            .iter()
                            .find_map(|entry| entry.tool.as_ref().filter(|tool| &tool.id == id))
                    })
                    .map_or("Tool output", |tool| tool.name.as_str());
                format!("{} · full output", name.replace('_', " "))
            }
            InspectionTarget::Job(id) => format!("Job {id} · full output"),
            InspectionTarget::Artifact(id) => format!("Retained output · {id}"),
        };
        pane.inspector = Some(Inspector {
            target: target.clone(),
            previous,
            title,
            text: "Loading retained content…".into(),
            offset,
            next: None,
            total: 0,
            scroll: 0,
            pending: true,
        });
        self.focus = Focus::Inspector;
        self.hits.clear();
        spawn_reply(jobs, self.runtime_epoch, async move {
            let result = match target {
                InspectionTarget::Entry(sequence) => {
                    handle
                        .entry_text(thread.clone(), sequence, offset, 16384)
                        .await
                }
                InspectionTarget::Job(id) => handle.job_output(id, offset, 16384).await,
                InspectionTarget::ToolOutput(id) => handle.tool_output(id, offset, 16384).await,
                InspectionTarget::Artifact(id) => {
                    handle
                        .artifact(id, offset, 16384)
                        .await
                        .map(|text| TextPage {
                            next_offset: (!text.is_empty()).then_some(offset + text.len()),
                            total_bytes: 0,
                            text,
                        })
                }
            }
            .map_err(|e| format!("{e:#}"));
            Reply::Text {
                thread,
                epoch,
                result,
            }
        });
    }
    fn stop(&mut self, jobs: &mut JoinSet<Reply>) {
        let Some(summary) = self
            .threads
            .iter()
            .find(|t| Some(&t.id) == self.selected.as_ref())
        else {
            return;
        };
        let cancellable = self.threads.iter().any(|thread| {
            let live = matches!(
                thread.state,
                ThreadState::Active
                    | ThreadState::Interrupted
                    | ThreadState::Stopping
                    | ThreadState::Held
            ) || (thread.run.is_none() && thread.parent_run.is_some());
            if !live || thread.session != summary.session {
                return false;
            }
            let mut current = Some(thread);
            let mut visited = HashSet::new();
            while let Some(thread) = current {
                if thread.id == summary.id {
                    return true;
                }
                if !visited.insert(&thread.id) {
                    break;
                }
                current = thread
                    .parent_thread
                    .as_ref()
                    .and_then(|parent| self.threads.iter().find(|thread| &thread.id == parent));
            }
            false
        });
        if !cancellable {
            return;
        }
        let Some(handle) = self.handle.clone() else {
            return;
        };
        let thread = summary.id.clone();
        spawn_reply(jobs, self.runtime_epoch, async move {
            Reply::Control {
                thread: thread.clone(),
                result: handle
                    .command(Command::StopThread {
                        id: CommandId::new(),
                        thread,
                    })
                    .await
                    .map_err(|e| format!("{e:#}")),
            }
        });
    }
    pub fn paste(&mut self, text: &str) {
        self.focus_epoch += 1;
        if self.pane().is_some_and(|pane| pane.permission.is_some()) {
            return;
        }
        if self.focus == Focus::Composer {
            self.draft_mut().paste(text);
            self.refresh_completion();
        } else if self.focus == Focus::Question {
            if let Some(prompt) = self
                .pane_mut()
                .and_then(|pane| pane.question.as_mut())
                .filter(|prompt| !prompt.pending)
            {
                if prompt.selected == Some(prompt.question.options.len()) {
                    prompt.other.paste(text);
                }
            }
        } else if self.focus == Focus::Plan {
            if let Some(prompt) = self
                .pane_mut()
                .and_then(|pane| pane.plan.as_mut())
                .filter(|prompt| !prompt.pending)
            {
                prompt.feedback.paste(text);
            }
        }
    }
    pub fn key(&mut self, key: KeyEvent, jobs: &mut JoinSet<Reply>) -> bool {
        self.invalidate_control_focus();
        self.focus_active_prompt();
        self.focus_epoch += 1;
        let has_permission = self.pane().is_some_and(|pane| pane.permission.is_some());
        if has_permission && self.focus == Focus::Composer {
            self.focus = Focus::PermissionInvalidated;
        }
        if (has_permission || self.focus == Focus::PermissionInvalidated)
            && key.kind != crossterm::event::KeyEventKind::Press
            && matches!(
                key.code,
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('1'..='9')
            )
        {
            return false;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && matches!(key.code, KeyCode::Char('c' | 'C')) {
            self.dismiss_picker();
            return true;
        }
        let composer_popup_key = self.focus == Focus::Composer
            && matches!(key.code, KeyCode::Tab | KeyCode::Esc)
            && (self.picker.is_some() || (!control && !self.completion.items.is_empty()));
        if !composer_popup_key && self.workflow_key(key, jobs) {
            return false;
        }
        if key.code == KeyCode::BackTab && self.focus == Focus::Composer {
            self.application_shortcut(commands::Request::CycleMode, jobs);
            return false;
        }
        if control && matches!(key.code, KeyCode::Char('o' | 'O')) && self.focus == Focus::Composer
        {
            self.application_shortcut(commands::Request::Copy, jobs);
            return false;
        }
        if control && matches!(key.code, KeyCode::Char('b' | 'B')) && self.focus == Focus::Composer
        {
            self.application_shortcut(commands::Request::Background, jobs);
            return false;
        }
        if key.code == KeyCode::Backspace
            && key.modifiers.is_empty()
            && self.focus == Focus::Composer
            && self.draft_mut().text.is_empty()
        {
            if let Some(text) = self.pane_mut().and_then(|pane| pane.recalled.take()) {
                self.draft_mut().insert(&text);
                self.refresh_completion();
                return false;
            }
            let recall = self.pane().and_then(|pane| {
                let input = pane.visible_queue().pop()?;
                if let Some(full) = pane.queue.iter().find(|queued| queued.id == input.id) {
                    return Some(commands::Request::Recall {
                        input: full.clone(),
                        entry: None,
                    });
                }
                let entry = pane.view.as_ref()?.entries.iter().find(|entry| {
                    entry.thread == input.thread
                        && entry
                            .submission
                            .as_ref()
                            .is_some_and(|submission| submission.id == input.id)
                })?;
                Some(commands::Request::Recall {
                    input,
                    entry: Some(entry.sequence),
                })
            });
            if let Some(request) = recall {
                self.application_shortcut(request, jobs);
                return false;
            }
        }
        if control && matches!(key.code, KeyCode::Char('x' | 'X')) {
            self.stop(jobs);
            return false;
        }
        if control && matches!(key.code, KeyCode::Char('l' | 'L')) {
            self.show_todos = !self.show_todos;
            return false;
        }
        if control && matches!(key.code, KeyCode::Char('t' | 'T')) {
            self.sidebar = !self.sidebar;
            if self.focus == Focus::Sidebar {
                self.focus = Focus::Composer;
            }
            self.hits.clear();
            return false;
        }
        if self.focus == Focus::Composer && self.picker.is_some() {
            let picker = self.picker.as_mut().unwrap();
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    picker.selected = picker.selected.saturating_sub(1);
                    self.preview_theme();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    picker.selected =
                        (picker.selected + 1).min(picker.items.len().saturating_sub(1));
                    self.preview_theme();
                }
                KeyCode::Esc => self.dismiss_picker(),
                KeyCode::Enter => return self.confirm_picker(jobs),
                KeyCode::Char(ch @ '1'..='9') => {
                    let index = ch as usize - '1' as usize;
                    if index < picker.items.len() {
                        picker.selected = index;
                        return self.confirm_picker(jobs);
                    }
                }
                _ => {}
            }
            return false;
        }
        if self.focus == Focus::Composer && !self.completion.items.is_empty() && !control {
            match key.code {
                KeyCode::Up => {
                    self.completion.selected = self.completion.selected.saturating_sub(1);
                    return false;
                }
                KeyCode::Down => {
                    self.completion.selected =
                        (self.completion.selected + 1).min(self.completion.items.len() - 1);
                    return false;
                }
                KeyCode::Tab => {
                    if self.complete() == Some(completion::Kind::Slash) {
                        self.refresh_completion();
                    }
                    return false;
                }
                KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                    return if self.complete() == Some(completion::Kind::Slash) {
                        self.submit(jobs)
                    } else {
                        false
                    };
                }
                KeyCode::Esc => {
                    self.completion.clear();
                    return false;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Esc {
            if let Some(pane) = self.pane_mut() {
                pane.inspection_epoch += 1;
            }
            if self.focus == Focus::PermissionInvalidated && has_permission {
                return false;
            }
            if matches!(
                self.focus,
                Focus::Question
                    | Focus::Plan
                    | Focus::ControlInvalidated
                    | Focus::PermissionInvalidated
            ) {
                self.focus = Focus::Composer;
            } else if self.focus == Focus::Inspector {
                self.dismiss_inspector();
            } else if self.pane().is_some_and(|p| p.permission.is_some()) {
                self.decide(false, jobs);
            } else if let Some(root) = self
                .threads
                .iter()
                .find(|thread| {
                    Some(&thread.id) == self.selected.as_ref() && thread.parent_thread.is_some()
                })
                .and_then(|selected| {
                    self.threads.iter().find(|thread| {
                        thread.session == selected.session && thread.parent_thread.is_none()
                    })
                })
                .map(|thread| thread.id.clone())
                .filter(|_| {
                    self.pane()
                        .is_none_or(|pane| pane.question.is_none() && pane.plan.is_none())
                })
            {
                self.select(root);
                self.focus = Focus::Composer;
            } else if self.focus == Focus::Sidebar {
                self.focus = Focus::Composer;
            } else {
                self.stop(jobs);
            }
            return false;
        }
        if key.code == KeyCode::Tab {
            if has_permission
                && matches!(self.focus, Focus::Permission | Focus::PermissionInvalidated)
            {
                self.focus_permission();
                return false;
            }
            self.focus = match self.focus {
                Focus::Composer | Focus::PermissionInvalidated
                    if self.pane().is_some_and(|p| p.permission.is_some()) =>
                {
                    Focus::Permission
                }
                Focus::Composer | Focus::ControlInvalidated
                    if self.pane().is_some_and(|pane| {
                        pane.question.as_ref().is_some_and(|prompt| !prompt.pending)
                    }) =>
                {
                    Focus::Question
                }
                Focus::Composer | Focus::ControlInvalidated
                    if self.pane().is_some_and(|pane| {
                        pane.plan.as_ref().is_some_and(|prompt| !prompt.pending)
                    }) =>
                {
                    Focus::Plan
                }
                Focus::Composer if self.sidebar => Focus::Sidebar,
                Focus::Inspector => Focus::Inspector,
                _ => Focus::Composer,
            };
            return false;
        }
        match self.focus {
            Focus::PermissionInvalidated | Focus::ControlInvalidated | Focus::Workflow => {}
            Focus::Question => {
                let mut confirm = false;
                if let Some(prompt) = self
                    .pane_mut()
                    .and_then(|pane| pane.question.as_mut())
                    .filter(|prompt| !prompt.pending)
                {
                    let other = prompt.selected == Some(prompt.question.options.len());
                    match key.code {
                        KeyCode::Up => {
                            prompt.selected = Some(prompt.selected.unwrap_or(1).saturating_sub(1))
                        }
                        KeyCode::Down => {
                            prompt.selected = Some(
                                prompt
                                    .selected
                                    .map_or(0, |selected| selected + 1)
                                    .min(prompt.question.options.len()),
                            )
                        }
                        KeyCode::Enter => confirm = prompt.selected.is_some(),
                        KeyCode::Char(ch @ '1'..='9') if !other => {
                            let index = ch as usize - '1' as usize;
                            if index <= prompt.question.options.len() {
                                prompt.selected = Some(index);
                            }
                        }
                        _ if other => prompt.other.key(key),
                        _ => {}
                    }
                }
                if confirm {
                    self.answer_control(jobs);
                }
            }
            Focus::Plan => {
                let mut confirm = false;
                if let Some(prompt) = self
                    .pane_mut()
                    .and_then(|pane| pane.plan.as_mut())
                    .filter(|prompt| !prompt.pending)
                {
                    match key.code {
                        KeyCode::Left => prompt.choice = Some(true),
                        KeyCode::Right => prompt.choice = Some(false),
                        KeyCode::Enter => confirm = prompt.choice.is_some(),
                        KeyCode::PageUp => prompt.scroll = prompt.scroll.saturating_sub(10),
                        KeyCode::PageDown => prompt.scroll = prompt.scroll.saturating_add(10),
                        _ => prompt.feedback.key(key),
                    }
                }
                if confirm {
                    self.answer_control(jobs);
                }
            }
            Focus::Sidebar => match key.code {
                KeyCode::Up => self.cycle(-1),
                KeyCode::Down => self.cycle(1),
                KeyCode::Enter => self.focus = Focus::Composer,
                _ => {}
            },
            Focus::Permission => {
                let mut confirm = None;
                if let Some(permission) = self
                    .pane_mut()
                    .and_then(|p| p.permission.as_mut())
                    .filter(|p| !p.pending && p.options_loaded)
                {
                    match key.code {
                        KeyCode::Left | KeyCode::Char('a') => {
                            permission.choice = permission
                                .options
                                .iter()
                                .position(|option| option.approve && option.grant.is_none())
                        }
                        KeyCode::Right | KeyCode::Char('d') => {
                            permission.choice =
                                permission.options.iter().position(|option| !option.approve)
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            permission.choice =
                                Some(permission.choice.map_or(0, |index| index.saturating_sub(1)))
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            permission.choice = Some(
                                permission
                                    .choice
                                    .map_or(0, |index| index.saturating_add(1))
                                    .min(permission.options.len().saturating_sub(1)),
                            )
                        }
                        KeyCode::Char(ch @ '1'..='9') => {
                            let index = (ch as u8 - b'1') as usize;
                            if index < permission.options.len() {
                                permission.choice = Some(index);
                                confirm = Some(index);
                            }
                        }
                        KeyCode::PageUp => permission.scroll = permission.scroll.saturating_sub(3),
                        KeyCode::PageDown => {
                            permission.scroll = permission.scroll.saturating_add(3)
                        }
                        KeyCode::Char('i') | KeyCode::F(2) => {
                            permission.details = !permission.details;
                            permission.scroll = 0;
                        }
                        KeyCode::Enter => confirm = permission.choice,
                        _ => {}
                    }
                }
                if let Some(index) = confirm {
                    self.confirm_option(index, jobs);
                }
            }
            Focus::Inspector => {
                let mut next = None;
                if let Some(inspector) = self.pane_mut().and_then(|p| p.inspector.as_mut()) {
                    match key.code {
                        KeyCode::Up | KeyCode::PageUp => {
                            inspector.scroll = inspector.scroll.saturating_sub(10)
                        }
                        KeyCode::Down | KeyCode::PageDown => {
                            inspector.scroll = inspector.scroll.saturating_add(10)
                        }
                        KeyCode::Char('n') => next = inspector.navigation(true),
                        KeyCode::Char('p') => next = inspector.navigation(false),
                        _ => {}
                    }
                }
                if let Some((target, offset, previous)) = next {
                    self.open_inspector(target, offset, previous, jobs);
                }
            }
            Focus::Composer => match key.code {
                KeyCode::Up if self.sidebar && self.draft_mut().text.is_empty() => self.cycle(-1),
                KeyCode::Down if self.sidebar && self.draft_mut().text.is_empty() => self.cycle(1),
                KeyCode::PageUp if control => self.page(true),
                KeyCode::PageDown if control => self.page(false),
                KeyCode::F(2) | KeyCode::Char('i' | 'I')
                    if key.code == KeyCode::F(2) || control =>
                {
                    let sequence = self.pane().and_then(|p| {
                        p.viewport
                            .anchor
                            .as_ref()
                            .and_then(|a| a.identity.sequence())
                            .or_else(|| p.view.as_ref()?.entries.last().map(|e| e.sequence))
                    });
                    if let Some(sequence) = sequence {
                        if let Some(operation) = self.workflow_for_entry(sequence) {
                            self.open_workflow(operation, jobs);
                        } else {
                            self.inspect(sequence, 0, jobs);
                        }
                    }
                }
                KeyCode::F(3) => {
                    let operation = self.pane().and_then(|pane| {
                        let view = pane.view.as_ref()?;
                        let sequence = pane
                            .viewport
                            .anchor
                            .as_ref()
                            .and_then(|anchor| anchor.identity.sequence());
                        view.entries
                            .iter()
                            .rev()
                            .find(|entry| {
                                sequence.is_none_or(|sequence| entry.sequence <= sequence)
                            })?
                            .operation
                            .clone()
                    });
                    if let Some(operation) = operation {
                        let target = if key.modifiers.contains(KeyModifiers::SHIFT) {
                            self.tool_input_inspection(&operation)
                        } else {
                            self.tool_inspection(&operation)
                        };
                        if let Some(target) = target {
                            self.open_inspector(target, 0, VecDeque::new(), jobs);
                        }
                    }
                }
                KeyCode::PageUp => self.page(true),
                KeyCode::PageDown => self.page(false),
                // Macs have no PgUp/PgDn keys; Shift+arrows page the transcript.
                KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => self.page(true),
                KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => self.page(false),
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    self.draft_mut().insert("\n")
                }
                KeyCode::Enter => {
                    return self.submit_with_mode(jobs, key.modifiers.contains(KeyModifiers::ALT))
                }
                _ => self.draft_mut().key(key),
            },
        }
        self.refresh_completion();
        false
    }
    pub fn mouse(&mut self, mouse: MouseEvent, jobs: &mut JoinSet<Reply>) {
        self.focus_epoch += 1;
        if !self.size.contains((mouse.column, mouse.row).into()) {
            return;
        }
        let action = self
            .hits
            .iter()
            .rev()
            .find(|hit| hit.area.contains((mouse.column, mouse.row).into()))
            .map(|hit| hit.action.clone());
        if self.pane().is_some_and(|pane| pane.permission.is_some())
            && matches!(mouse.kind, MouseEventKind::Down(_))
            && !matches!(action, Some(HitAction::Permission))
        {
            return;
        }
        if matches!(
            self.focus,
            Focus::Question | Focus::Plan | Focus::ControlInvalidated
        ) && matches!(mouse.kind, MouseEventKind::Down(_))
            && matches!(
                action,
                Some(
                    HitAction::WorkflowRow(..)
                        | HitAction::WorkflowSidebar(..)
                        | HitAction::Tool(_)
                )
            )
        {
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => match action {
                Some(HitAction::Completion(index))
                    if self.focus == Focus::Composer && index < self.completion.items.len() =>
                {
                    self.completion.selected = index;
                    self.complete();
                }
                Some(HitAction::Picker(index)) if self.focus == Focus::Composer => {
                    if let Some(picker) = self.picker.as_mut() {
                        if index < picker.items.len() {
                            picker.selected = index;
                            self.preview_theme();
                        }
                    }
                }
                Some(HitAction::Thread(id)) => {
                    self.select(id);
                    self.focus = Focus::Sidebar;
                }
                Some(HitAction::Question) => {
                    self.invalidate_control_focus();
                    if self.pane().is_some_and(|pane| {
                        pane.question.as_ref().is_some_and(|prompt| !prompt.pending)
                    }) {
                        self.focus = Focus::Question;
                    }
                }
                Some(HitAction::Plan) => {
                    self.invalidate_control_focus();
                    if self.pane().is_some_and(|pane| {
                        pane.plan.as_ref().is_some_and(|prompt| !prompt.pending)
                    }) {
                        self.focus = Focus::Plan;
                    }
                }
                Some(HitAction::Permission) => self.focus_permission(),
                Some(HitAction::Composer)
                    if self.pane().is_none_or(|pane| pane.permission.is_none()) =>
                {
                    self.focus = Focus::Composer
                }
                Some(HitAction::WorkflowRow(index, action)) => {
                    self.workflows.selected = index;
                    self.focus = Focus::Workflow;
                    self.workflow_action(action, jobs);
                }
                Some(HitAction::WorkflowSidebar(action)) => {
                    self.focus = Focus::Sidebar;
                    self.workflows.sidebar_selected = Some(action.clone());
                    self.workflow_action(action, jobs);
                }
                Some(HitAction::Tool(id)) if self.focus != Focus::Inspector => {
                    if self.is_workflow_tool(&id) {
                        self.open_workflow(id, jobs);
                    } else {
                        self.toggle_tool(id, jobs);
                    }
                }
                Some(HitAction::Output(id)) => {
                    self.open_inspector(InspectionTarget::ToolOutput(id), 0, VecDeque::new(), jobs)
                }
                Some(HitAction::Inline(sequence, offset, ordinal)) => {
                    if let Some(pane) = self.pane_mut() {
                        pane.inline.seek(sequence, offset, ordinal, false);
                        pane.viewport.anchor = Some(Anchor {
                            identity: AnchorId::BodyGap { sequence, ordinal },
                            line: ordinal,
                            offset,
                        });
                        pane.rendered = None;
                    }
                }
                Some(HitAction::Entry(sequence)) => self.inspect(sequence, 0, jobs),
                _ => {}
            },
            MouseEventKind::Down(MouseButton::Right) => {
                let target = match action {
                    Some(HitAction::Entry(sequence)) => Some(InspectionTarget::Entry(sequence)),
                    Some(HitAction::Tool(id)) if mouse.modifiers.contains(KeyModifiers::SHIFT) => {
                        self.tool_input_inspection(&id)
                    }
                    Some(HitAction::Tool(id)) => self.tool_inspection(&id),
                    _ => None,
                };
                if let Some(target) = target {
                    self.open_inspector(target, 0, VecDeque::new(), jobs);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if mouse.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                if matches!(action, Some(HitAction::Permission)) {
                    if let Some(p) = self.pane_mut().and_then(|p| p.permission.as_mut()) {
                        p.scroll = p.scroll.saturating_add_signed(delta);
                    }
                } else if matches!(action, Some(HitAction::WorkflowSidebar(_))) {
                    self.workflow_sidebar_scroll(delta as isize);
                } else if self.focus == Focus::Inspector {
                    if let Some(i) = self.pane_mut().and_then(|p| p.inspector.as_mut()) {
                        i.scroll = i.scroll.saturating_add_signed(delta);
                    }
                } else if self.workflows.inspecting() {
                    self.workflow_scroll(delta as isize);
                } else if let Some(p) = self.pane_mut() {
                    p.scroll(delta as isize);
                }
            }
            _ => {}
        }
    }
    pub fn reply(&mut self, reply: Reply) {
        match reply {
            Reply::Inline(reply) => self.accept_inline(reply),
            Reply::Workflow(reply) => self.accept_workflow(reply),
            Reply::ToolOutput {
                thread,
                operation,
                epoch,
                result,
            } => {
                let Some(pane) = self.panes.get_mut(&thread) else {
                    return;
                };
                if pane.pending_outputs.get(&operation) != Some(&epoch)
                    || !pane.expanded.contains(&operation)
                {
                    return;
                }
                pane.pending_outputs.remove(&operation);
                match result {
                    Ok(page) => {
                        pane.output_errors.remove(&operation);
                        pane.expanded_outputs.insert(operation, page);
                    }
                    Err(error) => {
                        pane.output_errors.insert(operation, clipped(&error, 512));
                    }
                }
                pane.rendered = None;
            }
            Reply::ToolInput {
                thread,
                operation,
                result,
            } => {
                let Some(pane) = self.panes.get_mut(&thread) else {
                    return;
                };
                if !pane.pending_inputs.remove(&operation) || !pane.expanded.contains(&operation) {
                    return;
                }
                match result {
                    Ok(input) => {
                        pane.input_errors.remove(&operation);
                        pane.full_inputs.insert(operation, input);
                    }
                    Err(error) => {
                        pane.input_errors.insert(operation, clipped(&error, 512));
                        pane.notify_current(format!(
                            "Full command unavailable: {error}. Raw input remains inspectable."
                        ));
                    }
                }
                pane.rendered = None;
            }
            Reply::Shortcut {
                thread,
                epoch,
                result,
            } => {
                let Some(pane) = self.panes.get_mut(&thread) else {
                    return;
                };
                if pane.pending != Some(epoch) {
                    return;
                }
                pane.pending = None;
                match result {
                    Ok(Outcome::Receipt(receipt)) => {
                        self.sequence_floor = self.sequence_floor.max(receipt.sequence);
                        pane.notify_current(receipt.detail);
                    }
                    Ok(Outcome::PermissionsUpdated(receipt)) => {
                        self.sequence_floor = self.sequence_floor.max(receipt.sequence);
                        pane.invalidate_metadata();
                    }
                    Ok(Outcome::Recalled(receipt, input)) => {
                        self.sequence_floor = self.sequence_floor.max(receipt.sequence);
                        pane.invalidate_metadata();
                        pane.observe_submission(bob_kernel::EntrySubmission {
                            id: input.id.clone(),
                            state: bob_kernel::SubmissionState::Recalled,
                            ordinal: pane
                                .submissions
                                .get(&input.id)
                                .map_or(0, |submission| submission.ordinal),
                            sequence: receipt.sequence,
                        });
                        pane.queue.retain(|queued| queued.id != input.id);
                        if pane.draft.text.is_empty() {
                            pane.draft.insert(&input.text);
                        } else {
                            pane.recalled = Some(input.text);
                            pane.notify_current(
                                "Recalled input saved; clear draft then Backspace to restore.",
                            );
                        }
                    }
                    Ok(Outcome::Copy(text)) => match bob_ui::clipboard::copy(&text) {
                        Ok(outcome) => pane.notify_current(outcome.notice()),
                        Err(error) => {
                            pane.notify_current(format!("Clipboard copy failed: {error}"))
                        }
                    },
                    Ok(_) => unreachable!("shortcut must return receipt or clipboard content"),
                    Err(error) => {
                        if self.workflows.inspecting() && self.selected.as_ref() == Some(&thread) {
                            self.workflows.notify(format!("Shortcut failed: {error}"));
                        }
                        pane.notify_current(format!("Shortcut failed: {error}"));
                    }
                }
                self.refresh_completion();
            }
            Reply::Files { epoch, files } => {
                if epoch != self.workspace_epoch {
                    return;
                }
                self.files = files;
                self.files_loading = false;
                self.files_loaded = true;
                self.refresh_completion();
            }
            Reply::ApplicationReady(Ok((application, view))) => {
                let session = application.session;
                self.configure(
                    PathBuf::from(&session.workspace),
                    session.profile.model.clone(),
                );
                self.provider = Some(application.provider.clone());
                self.set_services(application.services);
                self.theme_name = application.config.theme.unwrap_or_else(|| "dark".into());
                self.apply_theme(&self.theme_name.clone());
                let thread = session.thread.clone();
                let sequence = view.sequence;
                self.reply(Reply::Ready(Ok((application.handle, view, thread.clone()))));
                self.titles.accept(&session, (self.runtime_epoch, sequence));
                self.titles.select(session.id.clone());
                self.panes.entry(thread).or_default().session = Some(session);
            }
            Reply::ApplicationReady(Err(error)) => {
                self.startup_error = Some(error.clone());
                self.reply(Reply::Ready(Err(error)))
            }
            Reply::Metadata {
                thread,
                key,
                epoch,
                result,
            } => {
                let Some(pane) = self.panes.get_mut(&thread) else {
                    return;
                };
                if pane.metadata_epoch != epoch
                    || pane.metadata_key != Some(key)
                    || key.0 != self.runtime_epoch
                {
                    return;
                }
                pane.metadata_pending = false;
                if key.1 < self.sequence_floor {
                    pane.metadata_key = None;
                    return;
                }
                match result {
                    Ok(Metadata {
                        context,
                        controls,
                        session,
                        details,
                        jobs,
                        queue,
                    }) => {
                        pane.jobs = jobs;
                        pane.accept_queue(queue);
                        if self.selected.as_ref() == Some(&thread) {
                            self.thread_details = details;
                        }
                        let question = controls.questions.iter().find(|question| {
                            !pane
                                .answered
                                .contains(&(question.operation.clone(), question.revision))
                        });
                        let old_question = pane
                            .question
                            .as_ref()
                            .map(|prompt| (&prompt.question.operation, prompt.question.revision));
                        let new_question =
                            question.map(|question| (&question.operation, question.revision));
                        if old_question != new_question {
                            pane.question = question.cloned().map(|question| QuestionPrompt {
                                question,
                                selected: None,
                                other: Draft::default(),
                                pending: false,
                            });
                            if self.selected.as_ref() == Some(&thread)
                                && self.focus == Focus::Question
                            {
                                self.focus = Focus::ControlInvalidated;
                            }
                        }
                        let plan = controls.plans.iter().find(|plan| {
                            !pane
                                .answered
                                .contains(&(plan.operation.clone(), plan.revision))
                        });
                        let old_plan = pane
                            .plan
                            .as_ref()
                            .map(|prompt| (&prompt.plan.operation, prompt.plan.revision));
                        let new_plan = plan.map(|plan| (&plan.operation, plan.revision));
                        if old_plan != new_plan {
                            pane.plan = plan.cloned().map(|plan| PlanPrompt {
                                plan,
                                choice: None,
                                feedback: Draft::default(),
                                pending: false,
                                scroll: 0,
                            });
                            if self.selected.as_ref() == Some(&thread) && self.focus == Focus::Plan
                            {
                                self.focus = Focus::ControlInvalidated;
                            }
                        }
                        pane.context = Some(context);
                        pane.controls = Some(controls);
                        self.titles.accept(&session, key);
                        pane.session = Some(session);
                        pane.metadata_runtime = key.0;
                    }
                    Err(error) => {
                        pane.metadata_key = None;
                        pane.notify_current(format!("Control refresh failed: {error}"));
                        pane.question = None;
                        pane.plan = None;
                        if self.selected.as_ref() == Some(&thread)
                            && matches!(self.focus, Focus::Question | Focus::Plan)
                        {
                            self.focus = Focus::ControlInvalidated;
                        }
                    }
                }
            }
            Reply::HumanDecision {
                thread,
                operation,
                revision,
                result,
            } => {
                let Some(pane) = self.panes.get_mut(&thread) else {
                    return;
                };
                if !pane.answered.contains(&(operation.clone(), revision)) {
                    return;
                }
                pane.metadata_key = None;
                pane.metadata_epoch += 1;
                pane.metadata_pending = false;
                if pane.question.as_ref().is_some_and(|prompt| {
                    prompt.question.operation == operation && prompt.question.revision == revision
                }) {
                    pane.question = None;
                }
                if pane.plan.as_ref().is_some_and(|prompt| {
                    prompt.plan.operation == operation && prompt.plan.revision == revision
                }) {
                    pane.plan = None;
                }
                match result {
                    Ok(receipt) => {
                        self.sequence_floor = self.sequence_floor.max(receipt.sequence);
                        pane.receipt = None;
                    }
                    Err(error) => {
                        pane.answered.remove(&(operation, revision));
                        pane.notify_current(format!(
                            "Answer not confirmed: {error}. Refreshing before retrying."
                        ));
                    }
                }
                if self.selected.as_ref() == Some(&thread)
                    && matches!(self.focus, Focus::Question | Focus::Plan)
                {
                    self.focus = Focus::ControlInvalidated;
                }
            }
            Reply::Workspace {
                epoch,
                files,
                commands,
                branch,
            } => {
                if epoch != self.workspace_epoch {
                    return;
                }
                self.files = files;
                self.custom_commands = commands;
                self.branch = branch;
                self.workspace_loading = false;
                self.workspace_loaded = true;
                self.refresh_completion();
            }
            Reply::Runtime { epoch, reply } => {
                if epoch == self.runtime_epoch {
                    self.reply(*reply);
                }
            }
            Reply::Ready(Ok((handle, mut view, thread))) => {
                let mut receiver = handle.subscribe();
                let snapshot = receiver.borrow_and_update().clone();
                self.live_receiver = Some(receiver);
                self.accept_live(snapshot);
                self.handle = Some(handle);
                self.sequence_floor = view.sequence;
                self.threads = std::mem::take(&mut view.threads);
                self.activity.observe(&self.threads);
                if self.selected.is_none() {
                    self.select_initial(thread.clone());
                }
                let pane = self.panes.entry(thread.clone()).or_default();
                Self::sync_permission(
                    pane,
                    &thread,
                    &view.decisions,
                    (self.selected.as_ref() == Some(&thread)).then_some(&mut self.focus),
                );
                pane.history = history::History::default();
                pane.history.older_exhausted = view.entries.len() < self.page_size.min(100);
                pane.loaded_before = Some(None);
                pane.force_refresh = self
                    .live_snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.error.is_some());
                pane.content_key = Some(render::content_key(&view));
                pane.committed_operations = view
                    .entries
                    .iter()
                    .filter(|entry| entry.kind == EntryKind::Assistant)
                    .filter_map(|entry| entry.operation.clone())
                    .collect();
                pane.entry_sequence = view.entry_sequence;
                pane.view = Some(view);
                pane.reconcile_submissions();
                self.status = "Ready".into();
            }
            Reply::Ready(Err(e)) => {
                self.status = format!("Initialization failed: {e}. Draft preserved; Ctrl+C exits.")
            }
            Reply::View {
                thread,
                generation,
                result,
            } => {
                if self
                    .reading_request
                    .as_ref()
                    .is_some_and(|request| request != &(thread.clone(), generation))
                {
                    return;
                }
                self.reading_request = None;
                self.reading = false;
                let Some(pane) = self.panes.get_mut(&thread) else {
                    return;
                };
                if pane.generation != generation {
                    return;
                }
                let query = pane.history.pending.take();
                match result {
                    Ok(mut view) => {
                        if query.is_none()
                            && pane
                                .loaded_before
                                .is_some_and(|before| before != pane.query_before())
                        {
                            pane.loaded_before = None;
                            return;
                        }
                        if view.sequence < self.sequence_floor {
                            pane.loaded_before = None;
                            return;
                        }
                        if query.is_none()
                            && pane.viewport.anchor.is_some()
                            && pane.before.is_none()
                        {
                            if let Some(before) = pane.query_before() {
                                if view.entries.last().and_then(|e| e.sequence.checked_add(1))
                                    != Some(before)
                                {
                                    return;
                                }
                            }
                        }
                        if let Some(query) = query {
                            pane.stitch_history(&mut view, query, self.page_size.min(100));
                        }
                        pane.force_refresh = false;
                        self.sequence_floor = view.sequence;
                        self.threads = std::mem::take(&mut view.threads);
                        self.activity.observe(&self.threads);
                        if pane.decision_refresh {
                            pane.decided = None;
                            pane.decision_refresh = false;
                        }
                        if let Some(anchor) = pane.viewport.anchor.as_mut() {
                            if let AnchorId::Live { operation, .. } = &anchor.identity {
                                if let Some(entry) = view.entries.iter().find(|entry| {
                                    entry.kind == EntryKind::Assistant
                                        && entry.operation.as_ref() == Some(operation)
                                }) {
                                    if !super::inline::long(entry) {
                                        anchor.identity = AnchorId::Body(entry.sequence);
                                        let offset =
                                            anchor.offset.min(entry.text.len().saturating_sub(1));
                                        if offset != anchor.offset {
                                            anchor.line = 0;
                                        }
                                        anchor.offset = offset;
                                        while !entry.text.is_char_boundary(anchor.offset) {
                                            anchor.offset -= 1;
                                        }
                                    }
                                    pane.pinned_before =
                                        view.entries.last().and_then(|e| e.sequence.checked_add(1));
                                }
                            }
                        }
                        Self::sync_permission(
                            pane,
                            &thread,
                            &view.decisions,
                            (self.selected.as_ref() == Some(&thread)).then_some(&mut self.focus),
                        );
                        pane.expanded.retain(|id| {
                            view.entries
                                .iter()
                                .any(|e| e.operation.as_ref() == Some(id))
                        });
                        pane.full_inputs.retain(|id, _| pane.expanded.contains(id));
                        pane.input_errors.retain(|id, _| pane.expanded.contains(id));
                        pane.pending_inputs.retain(|id| pane.expanded.contains(id));
                        pane.expanded_outputs
                            .retain(|id, _| pane.expanded.contains(id));
                        pane.pending_outputs
                            .retain(|id, _| pane.expanded.contains(id));
                        pane.output_errors
                            .retain(|id, _| pane.expanded.contains(id));
                        pane.content_key = Some(render::content_key(&view));
                        pane.committed_operations = view
                            .entries
                            .iter()
                            .filter(|entry| entry.kind == EntryKind::Assistant)
                            .filter_map(|entry| entry.operation.clone())
                            .collect();
                        pane.loaded_before = Some(pane.query_before());
                        pane.entry_sequence = view.entry_sequence;
                        pane.view = Some(view);
                        pane.reconcile_submissions();
                        self.evict();
                    }
                    Err(e) => {
                        pane.loaded_before = None;
                        pane.notify_current(format!("Refresh failed; display may be stale: {e}"));
                    }
                }
            }
            Reply::Command {
                thread,
                submitted,
                revision,
                focus_epoch,
                epoch,
                inspection_epoch,
                result,
            } => {
                if self.panes.get(&thread).and_then(|pane| pane.pending) != Some(epoch) {
                    return;
                }
                if self.pending_theme.as_ref() == Some(&(thread.clone(), epoch)) {
                    match &result {
                        Ok(Outcome::Theme(name)) => {
                            self.theme_preview = None;
                            self.pending_theme = None;
                            self.theme_name = name.clone();
                            self.apply_theme(name);
                        }
                        Err(_) => self.dismiss_picker(),
                        _ => {}
                    }
                }
                let pane = self.panes.get_mut(&thread).unwrap();
                pane.pending = None;
                match result {
                    Ok(outcome) => {
                        if pane.draft.revision == revision && pane.draft.text == submitted {
                            pane.draft.clear();
                            if pane.view.is_none() {
                                pane.draft.forget_history();
                            }
                        }
                        match outcome {
                            Outcome::Receipt(receipt) => {
                                self.sequence_floor = self.sequence_floor.max(receipt.sequence);
                                if submitted.trim() == "/yolo" {
                                    pane.notify_current(receipt.detail);
                                }
                                pane.receipt = Some((
                                    if submitted.trim_start().starts_with('/') {
                                        "Done"
                                    } else {
                                        "Sent"
                                    }
                                    .into(),
                                    std::time::Instant::now(),
                                ));
                            }
                            Outcome::PermissionsUpdated(receipt) => {
                                self.sequence_floor = self.sequence_floor.max(receipt.sequence);
                                pane.receipt = None;
                                pane.invalidate_metadata();
                            }
                            Outcome::Recalled(_, _) => {
                                unreachable!("recall is a composer shortcut")
                            }
                            Outcome::Notice(text) => pane.notify_current(text),
                            Outcome::Copy(text) => match bob_ui::clipboard::copy(&text) {
                                Ok(outcome) => pane.notify_current(outcome.notice()),
                                Err(error) => {
                                    pane.notify_current(format!("Clipboard copy failed: {error}"))
                                }
                            },
                            Outcome::Theme(name) => {
                                pane.notify_current(format!("Theme: {name}"));
                            }
                            Outcome::Picker(picker) => {
                                if pane.inspection_epoch == inspection_epoch
                                    && self.selected.as_ref() == Some(&thread)
                                    && self.focus_epoch == focus_epoch
                                    && self.focus == Focus::Composer
                                    && pane.draft.text.is_empty()
                                {
                                    self.open_picker(picker);
                                }
                            }
                            Outcome::Session(receipt) => {
                                self.sequence_floor = self.sequence_floor.max(receipt.sequence);
                                if self.selected.as_ref() == Some(&thread)
                                    && self.focus_epoch == focus_epoch
                                    && self.focus == Focus::Composer
                                {
                                    self.titles.select(receipt.session.clone());
                                    self.select(receipt.thread);
                                } else {
                                    pane.notify_current(format!(
                                        "Created session {}",
                                        receipt.session
                                    ));
                                }
                            }
                            Outcome::JobOutput(id, page) => {
                                if pane.inspection_epoch == inspection_epoch
                                    && self.selected.as_ref() == Some(&thread)
                                    && self.focus_epoch == focus_epoch
                                    && self.focus == Focus::Composer
                                {
                                    pane.inspector = Some(Inspector {
                                        target: InspectionTarget::Job(id.clone()),
                                        previous: VecDeque::new(),
                                        title: format!("Job {id} · full output"),
                                        next: page.next_offset,
                                        total: page.total_bytes,
                                        text: page.text,
                                        offset: 0,
                                        scroll: 0,
                                        pending: false,
                                    });
                                    self.focus = Focus::Inspector;
                                }
                            }
                            Outcome::Artifact(id, offset, text)
                                if pane.inspection_epoch == inspection_epoch
                                    && self.selected.as_ref() == Some(&thread)
                                    && self.focus_epoch == focus_epoch
                                    && self.focus == Focus::Composer =>
                            {
                                pane.inspector = Some(Inspector {
                                    target: InspectionTarget::Artifact(id.clone()),
                                    previous: VecDeque::new(),
                                    title: format!("Artifact {id}"),
                                    next: (!text.is_empty()).then_some(offset + text.len()),
                                    total: 0,
                                    text,
                                    offset,
                                    scroll: 0,
                                    pending: false,
                                });
                                self.focus = Focus::Inspector;
                            }
                            Outcome::Artifact(_, _, _) => {}
                        }
                    }
                    Err(e) => {
                        pane.receipt = None;
                        pane.notify_current(format!(
                            "Command failed: {e}\nSubmitted: {}",
                            clipped(&submitted, 1000)
                        ))
                    }
                }
                self.refresh_completion();
            }
            Reply::ApprovalOptions { key, result } => {
                let Some(pane) = self.panes.get_mut(&key.thread) else {
                    return;
                };
                let Some(permission) = pane.permission.as_mut().filter(|permission| {
                    DecisionKey::from(&permission.decision) == key && !permission.pending
                }) else {
                    return;
                };
                permission.options_pending = false;
                let selected = self.selected.as_ref() == Some(&key.thread);
                match result {
                    Ok(options)
                        if options.operation == key.operation
                            && options.thread == key.thread
                            && options.revision == key.revision =>
                    {
                        if permission.options_loaded
                            && approval::same_options(&permission.options, &options.options)
                        {
                            return;
                        }
                        let changed = permission.options_loaded;
                        permission.options = options.options;
                        permission.options_loaded = true;
                        permission.choice = if changed
                            || (selected && self.focus == Focus::PermissionInvalidated)
                        {
                            None
                        } else {
                            (!permission.options.is_empty()).then_some(0)
                        };
                        if changed {
                            pane.expected_permission = None;
                            pane.decision_followup = false;
                            if selected {
                                self.focus = Focus::PermissionInvalidated;
                            }
                        }
                    }
                    result => {
                        permission.options_loaded = false;
                        permission.options.clear();
                        permission.choice = None;
                        pane.expected_permission = None;
                        pane.decision_followup = false;
                        pane.force_refresh = true;
                        if selected {
                            self.focus = Focus::PermissionInvalidated;
                        }
                        if let Err(error) = result {
                            pane.notify_current(format!("Permission changed: {error}"));
                        }
                    }
                }
            }
            Reply::Decision { key, result } => {
                let Some(pane) = self.panes.get_mut(&key.thread) else {
                    return;
                };
                if pane
                    .permission
                    .as_ref()
                    .is_some_and(|p| DecisionKey::from(&p.decision) == key)
                {
                    pane.permission = None;
                }
                match result {
                    Ok(receipt) => {
                        self.sequence_floor = self.sequence_floor.max(receipt.sequence);
                        pane.receipt = None;
                        if pane.decided.as_ref() == Some(&key) && !pane.decision_confirmed {
                            pane.decision_confirmed = true;
                            if let Some(permission) =
                                pane.permission.as_mut().filter(|permission| {
                                    pane.decision_followup
                                        && pane.expected_permission.as_ref()
                                            == Some(&DecisionKey::from(&permission.decision))
                                })
                            {
                                pane.expected_permission = None;
                                pane.decision_followup = false;
                                if self.selected.as_ref() == Some(&key.thread)
                                    && self.focus == Focus::PermissionInvalidated
                                {
                                    self.focus = Focus::Permission;
                                    if permission.options_loaded {
                                        permission.choice =
                                            (!permission.options.is_empty()).then_some(0);
                                    }
                                }
                            } else if pane.permission.is_none()
                                && pane.draft.text.is_empty()
                                && self.selected.as_ref() == Some(&key.thread)
                                && self.focus == Focus::PermissionInvalidated
                            {
                                self.focus = Focus::Composer;
                            }
                        }
                    }
                    Err(e) => {
                        pane.expected_permission = None;
                        pane.decision_confirmed = false;
                        pane.decision_followup = false;
                        pane.decision_refresh = true;
                        pane.generation += 1;
                        pane.loaded_before = None;
                        pane.notify_current(format!(
                            "Decision not confirmed: {e}. Refreshing before retrying."
                        ));
                    }
                }
                if self.selected.as_ref() == Some(&key.thread)
                    && self.focus == Focus::Permission
                    && pane.permission.is_none()
                {
                    self.focus = Focus::PermissionInvalidated;
                }
            }
            Reply::Control { thread, result } => match result {
                Ok(receipt) => {
                    self.sequence_floor = self.sequence_floor.max(receipt.sequence);
                    if let Some(p) = self.panes.get_mut(&thread) {
                        p.receipt = Some(("Stop requested".into(), std::time::Instant::now()));
                    }
                }
                Err(e) => {
                    if let Some(p) = self.panes.get_mut(&thread) {
                        p.notify_current(format!("Stop not confirmed: {e}"));
                    }
                }
            },
            Reply::Text {
                thread,
                epoch,
                result,
            } => {
                if self.selected.as_ref() != Some(&thread) {
                    return;
                }
                let Some(pane) = self.panes.get_mut(&thread) else {
                    return;
                };
                if pane.inspection_epoch != epoch {
                    return;
                }
                let Some(inspector) = pane.inspector.as_mut() else {
                    return;
                };
                inspector.pending = false;
                pane.inspector_rendered = None;
                match result {
                    Ok(page) => {
                        inspector.text = page.text;
                        inspector.next = page.next_offset;
                        inspector.total = page.total_bytes;
                    }
                    Err(e) => inspector.text = format!("Inspection failed: {e}"),
                }
            }
        }
        self.focus_active_prompt();
    }
}
pub(super) enum Action {
    Application(commands::Request),
    Command(Command),
    Artifact(ArtifactId, usize),
}
pub(super) enum Outcome {
    Recalled(Receipt, bob_kernel::QueuedInput),
    JobOutput(bob_kernel::JobId, TextPage),
    Notice(String),
    Copy(String),
    Theme(String),
    Picker(commands::Picker),
    Session(Receipt),
    Receipt(Receipt),
    PermissionsUpdated(Receipt),
    Artifact(ArtifactId, usize, String),
}
pub(super) fn spawn_reply(
    jobs: &mut JoinSet<Reply>,
    epoch: u64,
    future: impl std::future::Future<Output = Reply> + Send + 'static,
) {
    jobs.spawn(async move {
        Reply::Runtime {
            epoch,
            reply: Box::new(future.await),
        }
    });
}

pub(super) struct Metadata {
    context: bob_kernel::ContextSnapshot,
    controls: bob_kernel::ControlSnapshot,
    session: bob_kernel::SessionSnapshot,
    details: Vec<bob_kernel::ThreadDetails>,
    jobs: Vec<bob_kernel::JobSummary>,
    queue: bob_kernel::QueueSnapshot,
}

pub(super) enum Reply {
    Inline(super::inline::PageReply),
    Workflow(workflow::QueryReply),
    ToolOutput {
        thread: ThreadId,
        operation: OperationId,
        epoch: u64,
        result: std::result::Result<TextPage, String>,
    },
    ToolInput {
        thread: ThreadId,
        operation: OperationId,
        result: std::result::Result<serde_json::Value, String>,
    },
    Shortcut {
        thread: ThreadId,
        epoch: u64,
        result: std::result::Result<Outcome, String>,
    },
    Files {
        epoch: u64,
        files: Vec<String>,
    },
    ApplicationReady(std::result::Result<(bob::application::Application, View), String>),
    Metadata {
        thread: ThreadId,
        key: (u64, i64),
        epoch: u64,
        result: std::result::Result<Metadata, String>,
    },
    HumanDecision {
        thread: ThreadId,
        operation: OperationId,
        revision: i64,
        result: std::result::Result<Receipt, String>,
    },
    Workspace {
        epoch: u64,
        files: Vec<String>,
        commands: Vec<bob_core::core::commands::CustomCommand>,
        branch: Option<String>,
    },
    Runtime {
        epoch: u64,
        reply: Box<Reply>,
    },
    Ready(std::result::Result<(Handle, View, ThreadId), String>),
    View {
        thread: ThreadId,
        generation: u64,
        result: std::result::Result<View, String>,
    },
    Command {
        thread: ThreadId,
        submitted: String,
        revision: u64,
        focus_epoch: u64,
        epoch: u64,
        inspection_epoch: u64,
        result: std::result::Result<Outcome, String>,
    },
    ApprovalOptions {
        key: DecisionKey,
        result: std::result::Result<bob_kernel::ApprovalOptions, String>,
    },
    Decision {
        key: DecisionKey,
        result: std::result::Result<Receipt, String>,
    },
    Control {
        thread: ThreadId,
        result: std::result::Result<Receipt, String>,
    },
    Text {
        thread: ThreadId,
        epoch: u64,
        result: std::result::Result<TextPage, String>,
    },
}

#[cfg(test)]
mod input_control_tests {
    use super::*;
    use bob_kernel::{ContextSnapshot, ControlSnapshot, HumanQuestion, SessionId, SessionSnapshot};

    pub(super) fn metadata(app: &mut App, revision: i64) -> Reply {
        let thread = app.selected.clone().unwrap();
        let profile = Profile {
            model: "model".into(),
            instructions: "instructions".into(),
            read_only: false,
            max_requests: 32,
            reasoning: ReasoningEffort::Off,
        };
        let pane = app.pane_mut().unwrap();
        pane.metadata_epoch += 1;
        pane.metadata_key = Some((0, 0));
        Reply::Metadata {
            thread: thread.clone(),
            key: (0, 0),
            epoch: pane.metadata_epoch,
            result: Ok(Metadata {
                context: ContextSnapshot {
                    profile: profile.clone(),
                    thread: thread.clone(),
                    epoch: 0,
                    revision: 0,
                    profile_revision: 0,
                    message_count: 0,
                    checkpoint: None,
                    compact_pending: false,
                    budget: Default::default(),
                },
                controls: ControlSnapshot {
                    questions: vec![HumanQuestion {
                        operation: OperationId("question".into()),
                        revision,
                        question: "Choose?".into(),
                        detail: None,
                        options: vec!["Yes".into(), "No".into()],
                    }],
                    ..Default::default()
                },
                session: SessionSnapshot {
                    title: String::new(),
                    title_revision: 0,
                    id: SessionId("session".into()),
                    thread,
                    workspace: "/workspace".into(),
                    profile,
                    profile_revision: 0,
                    context_epoch: 0,
                    policy: Default::default(),
                    permissions: Default::default(),
                    policy_revision: 0,
                },
                details: Vec::new(),
                jobs: Vec::new(),
                queue: bob_kernel::QueueSnapshot {
                    sequence: 0,
                    pending: Vec::new(),
                    submissions: Vec::new(),
                },
            }),
        }
    }

    #[test]
    fn new_human_prompts_take_focus_over_pickers_and_inspectors() {
        for plan in [false, true] {
            for focus in [
                Focus::Composer,
                Focus::Sidebar,
                Focus::Inspector,
                Focus::PermissionInvalidated,
            ] {
                let mut app = App::new(80);
                app.select(ThreadId("root".into()));
                app.draft_mut().insert("unsent draft");
                app.picker = Some(commands::Picker::theme("dark"));
                app.focus = focus;
                let mut reply = metadata(&mut app, 1);
                if plan {
                    let Reply::Metadata {
                        result: Ok(metadata),
                        ..
                    } = &mut reply
                    else {
                        unreachable!()
                    };
                    metadata.controls.questions.clear();
                    metadata.controls.plans.push(bob_kernel::PlanProposal {
                        operation: OperationId("plan".into()),
                        revision: 1,
                        plan: "Proposed plan".into(),
                    });
                }
                app.reply(reply);
                assert_eq!(app.focus, if plan { Focus::Plan } else { Focus::Question });
                assert!(app.picker.is_none());
                assert_eq!(app.draft_mut().text, "unsent draft");
            }
        }
    }

    #[test]
    fn human_confirmation_is_silent_but_failure_is_not() {
        for failed in [false, true] {
            let mut app = App::new(80);
            let thread = ThreadId("root".into());
            app.select(thread.clone());
            let reply = metadata(&mut app, 1);
            app.reply(reply);
            let operation = OperationId("question".into());
            app.pane_mut()
                .unwrap()
                .answered
                .insert((operation.clone(), 1));
            app.reply(Reply::HumanDecision {
                thread: thread.clone(),
                operation,
                revision: 1,
                result: if failed {
                    Err("storage unavailable".into())
                } else {
                    Ok(Receipt {
                        session: SessionId("session".into()),
                        thread,
                        sequence: 1,
                        detail: "Answer recorded".into(),
                    })
                },
            });
            let pane = app.pane().unwrap();
            assert!(pane.receipt.is_none());
            if failed {
                assert!(pane
                    .notice
                    .contains("Answer not confirmed: storage unavailable"));
            } else {
                assert!(pane.notices.is_empty());
                assert_eq!(app.sequence_floor, 1);
            }
        }
    }

    #[test]
    fn question_revision_change_invalidates_choice_and_focus() {
        let mut app = App::new(80);
        app.select(ThreadId("root".into()));
        let reply = metadata(&mut app, 1);
        app.reply(reply);
        app.focus = Focus::Question;
        app.pane_mut().unwrap().question.as_mut().unwrap().selected = Some(0);
        let reply = metadata(&mut app, 2);
        app.reply(reply);
        assert_eq!(app.focus, Focus::ControlInvalidated);
        assert_eq!(
            app.pane().unwrap().question.as_ref().unwrap().selected,
            None
        );
        let mut jobs = JoinSet::new();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut jobs);
        assert!(!app.pane().unwrap().question.as_ref().unwrap().pending);
    }

    #[test]
    fn metadata_survives_history_reads_but_not_invalidation_or_new_watermarks() {
        let mut app = App::new(80);
        app.select(ThreadId("root".into()));
        let reply = metadata(&mut app, 1);
        app.pane_mut().unwrap().generation += 1;
        app.reply(reply);
        assert!(app.pane().unwrap().question.is_some());

        let reply = metadata(&mut app, 2);
        app.pane_mut().unwrap().invalidate_metadata();
        app.reply(reply);
        assert_eq!(
            app.pane()
                .unwrap()
                .question
                .as_ref()
                .unwrap()
                .question
                .revision,
            1
        );

        let reply = metadata(&mut app, 3);
        app.sequence_floor = 1;
        app.reply(reply);
        assert_eq!(
            app.pane()
                .unwrap()
                .question
                .as_ref()
                .unwrap()
                .question
                .revision,
            1
        );
        assert!(app.pane().unwrap().metadata_key.is_none());
    }

    #[test]
    fn stale_metadata_does_not_replace_new_prompt() {
        let mut app = App::new(80);
        app.select(ThreadId("root".into()));
        let stale = metadata(&mut app, 1);
        let current = metadata(&mut app, 2);
        app.reply(current);
        app.reply(stale);
        assert_eq!(
            app.pane()
                .unwrap()
                .question
                .as_ref()
                .unwrap()
                .question
                .revision,
            2
        );
    }

    #[test]
    fn runtime_change_blocks_old_question_before_next_refresh() {
        let mut app = App::new(80);
        app.select(ThreadId("root".into()));
        let reply = metadata(&mut app, 1);
        app.reply(reply);
        app.focus = Focus::Question;
        app.pane_mut().unwrap().question.as_mut().unwrap().selected = Some(0);
        app.runtime_epoch += 1;
        let mut jobs = JoinSet::new();
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut jobs);
        assert_eq!(app.focus, Focus::ControlInvalidated);
        assert!(app.pane().unwrap().question.is_none());
        assert!(jobs.is_empty());
    }

    #[test]
    fn stale_workspace_completion_cannot_overwrite_reconfigured_root() {
        let mut app = App::new(80);
        app.configure("/first".into(), "first-model".into());
        let old = app.workspace_epoch;
        app.configure("/second".into(), "second-model".into());
        app.reply(Reply::Workspace {
            epoch: old,
            files: vec!["old.rs".into()],
            commands: vec![],
            branch: Some("old".into()),
        });
        assert!(app.files.is_empty());
        assert!(app.branch.is_none());
        assert_eq!(app.model.as_deref(), Some("second-model"));
    }
}
