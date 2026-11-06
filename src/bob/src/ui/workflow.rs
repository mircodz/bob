use super::*;
use bob_kernel::{
    SessionId, WorkflowNodeKind, WorkflowPageRequest, WorkflowQuery, WorkflowQueryResult,
    WorkflowState, WorkflowSummary, WorkflowTextSource,
};
use std::{collections::VecDeque, sync::Arc};

mod navigation;
mod presentation;
#[cfg(test)]
mod tests;
pub(super) use presentation::{draw, draw_sidebar};

const CACHE_BYTES: usize = 512 * 1024;
const CACHE_ENTRIES: usize = 32;
const RETURN_LIMIT: usize = 24;
const EXPANSION_LIMIT: usize = 12;
const TEXT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Route {
    Conversation(ThreadId),
    Workflow {
        operation: OperationId,
        container: Option<String>,
        cursor: Option<String>,
    },
    Node {
        operation: OperationId,
        path: String,
        iterations: bool,
        cursor: Option<String>,
    },
    Artifact {
        operation: OperationId,
        source: WorkflowTextSource,
        offset: usize,
    },
}
impl Route {
    fn query(&self) -> Option<WorkflowQuery> {
        let page = |cursor: &Option<String>| WorkflowPageRequest {
            cursor: cursor.clone(),
            ..Default::default()
        };
        Some(match self {
            Self::Conversation(_) => return None,
            Self::Workflow {
                operation,
                container,
                cursor,
            } => WorkflowQuery::Overview {
                operation: operation.clone(),
                container: container.clone(),
                page: page(cursor),
            },
            Self::Node {
                operation,
                path,
                iterations: false,
                ..
            } => WorkflowQuery::Node {
                operation: operation.clone(),
                path: path.clone(),
            },
            Self::Node {
                operation,
                path,
                cursor,
                ..
            } => WorkflowQuery::Iterations {
                operation: operation.clone(),
                path: path.clone(),
                page: page(cursor),
            },
            Self::Artifact {
                operation,
                source,
                offset,
            } => WorkflowQuery::Text {
                operation: operation.clone(),
                source: source.clone(),
                offset: *offset,
                max_bytes: TEXT_BYTES,
            },
        })
    }
    pub fn operation(&self) -> Option<&OperationId> {
        match self {
            Self::Conversation(_) => None,
            Self::Workflow { operation, .. }
            | Self::Node { operation, .. }
            | Self::Artifact { operation, .. } => Some(operation),
        }
    }
}

#[derive(Clone)]
struct Location {
    route: Route,
    destination: ThreadId,
    selected: usize,
    identity: Option<presentation::RowIdentity>,
    top: usize,
    sidebar_scroll: usize,
    sidebar_selected: Option<Action>,
    list_cursor: Option<String>,
    list_history: VecDeque<Option<String>>,
    expanded: Vec<String>,
    anchor: Option<Anchor>,
    focus: Focus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Open(Route),
    Back,
    Stop(OperationId),
    Expand(String),
    ListPage(Option<String>),
    Refresh,
    None,
}

pub(super) struct QueryReply {
    session: SessionId,
    runtime: u64,
    epoch: u64,
    ticket: u64,
    sequence: i64,
    query: WorkflowQuery,
    result: std::result::Result<WorkflowQueryResult, String>,
}
struct Cached {
    query: WorkflowQuery,
    sequence: i64,
    bytes: usize,
    fingerprint: u64,
    result: std::result::Result<Arc<WorkflowQueryResult>, String>,
}

#[derive(Default)]
pub(super) struct WorkflowUi {
    scope: Option<(SessionId, u64)>,
    pub route: Option<Route>,
    stack: VecDeque<Location>,
    pub selected: usize,
    pub top: usize,
    pub height: usize,
    expanded: Vec<String>,
    cache: VecDeque<Cached>,
    bytes: usize,
    epoch: u64,
    ticket: u64,
    pending: Vec<(u64, WorkflowQuery)>,
    revision: u64,
    visual_revision: u64,
    selection_restore: Option<presentation::RowIdentity>,
    list_cursor: Option<String>,
    list_history: VecDeque<Option<String>>,
    sidebar_height: usize,
    sidebar_position: Option<usize>,
    pub sidebar_selected: Option<Action>,
    notice: String,
    rendered: Option<presentation::Cache>,
    #[cfg(test)]
    frame_builds: usize,
    #[cfg(test)]
    formatted_rows: usize,
}
impl WorkflowUi {
    pub fn inspecting(&self) -> bool {
        self.route
            .as_ref()
            .is_some_and(|route| !matches!(route, Route::Conversation(_)))
    }
    pub fn has_back(&self) -> bool {
        !self.stack.is_empty()
    }
    pub fn notify(&mut self, notice: String) {
        self.notice = clipped(&notice, 2000);
        self.changed();
    }
    pub fn visual_revision(&self) -> u64 {
        self.visual_revision
    }
    fn visual_changed(&mut self) {
        self.visual_revision = self.visual_revision.wrapping_add(1);
    }
    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.visual_changed();
    }
    fn navigate(&mut self, route: Route) {
        self.route = Some(route);
        self.notice.clear();
        self.rendered = None;
        self.selection_restore = None;
        self.selected = 0;
        self.top = 0;
        self.expanded.clear();
        self.epoch = self.epoch.wrapping_add(1);
        self.changed();
    }
    fn synchronize(&mut self, session: SessionId, runtime: u64, thread: ThreadId) {
        if self.scope.as_ref() == Some(&(session.clone(), runtime)) {
            return;
        }
        let epoch = self.epoch.wrapping_add(1);
        *self = Self {
            scope: Some((session, runtime)),
            epoch,
            ticket: self.ticket,
            route: Some(Route::Conversation(thread)),
            ..Default::default()
        };
    }
    fn list_query(&self) -> WorkflowQuery {
        WorkflowQuery::List {
            page: WorkflowPageRequest {
                cursor: self.list_cursor.clone(),
                ..Default::default()
            },
        }
    }
    fn cached(&self, query: &WorkflowQuery) -> Option<&Cached> {
        self.cache.iter().find(|entry| &entry.query == query)
    }
    fn data(&self, query: &WorkflowQuery) -> Option<&WorkflowQueryResult> {
        self.cached(query)?.result.as_ref().ok().map(Arc::as_ref)
    }
    fn summary(&self, operation: &OperationId) -> Option<&WorkflowSummary> {
        self.cache
            .iter()
            .filter_map(|entry| {
                let summary = match entry.result.as_ref().ok()?.as_ref() {
                    WorkflowQueryResult::Overview(overview)
                        if &overview.workflow.operation == operation =>
                    {
                        Some(&overview.workflow)
                    }
                    WorkflowQueryResult::List(page) => page
                        .rows
                        .iter()
                        .find(|summary| &summary.operation == operation),
                    _ => None,
                }?;
                Some((entry.sequence, summary))
            })
            .max_by_key(|(sequence, _)| *sequence)
            .map(|(_, summary)| summary)
    }
    fn accept(&mut self, mut reply: QueryReply) {
        if self.scope.as_ref() != Some(&(reply.session.clone(), reply.runtime)) {
            return;
        }
        if !self
            .pending
            .iter()
            .any(|(ticket, query)| *ticket == reply.ticket && *query == reply.query)
        {
            return;
        }
        self.pending.retain(|(ticket, _)| *ticket != reply.ticket);
        if self.epoch != reply.epoch {
            return;
        }
        if let (
            WorkflowQuery::Text {
                operation,
                source,
                offset,
                ..
            },
            Ok(WorkflowQueryResult::Text(page)),
        ) = (&reply.query, &reply.result)
        {
            if page.session != reply.session
                || &page.operation != operation
                || &page.source != source
                || page.offset != *offset
            {
                reply.result = Err(
                    "Workflow text response did not match the requested session and source.".into(),
                );
            }
        }
        let encoded = match &reply.result {
            Ok(result) => serde_json::to_vec(result).unwrap_or_default(),
            Err(error) => error.as_bytes().to_vec(),
        };
        let bytes = encoded.len() + serde_json::to_vec(&reply.query).map_or(0, |value| value.len());
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        encoded.hash(&mut hash);
        let fingerprint = hash.finish();
        if let Some(entry) = self
            .cache
            .iter_mut()
            .find(|entry| entry.query == reply.query)
        {
            if entry.fingerprint == fingerprint {
                entry.sequence = reply.sequence;
                return;
            }
        }
        if let Some(index) = self
            .cache
            .iter()
            .position(|entry| entry.query == reply.query)
        {
            self.bytes -= self.cache.remove(index).unwrap().bytes;
        }
        let (bytes, result) = if bytes > CACHE_BYTES / 2 {
            (
                128,
                Err(
                    "Workflow page exceeds the inspection cache limit; request a smaller page."
                        .into(),
                ),
            )
        } else {
            (bytes, reply.result.map(Arc::new))
        };
        while self.cache.len() >= CACHE_ENTRIES || self.bytes + bytes > CACHE_BYTES {
            let Some(entry) = self.cache.pop_front() else {
                break;
            };
            self.bytes -= entry.bytes;
        }
        self.bytes += bytes;
        self.cache.push_back(Cached {
            query: reply.query,
            sequence: reply.sequence,
            bytes,
            fingerprint,
            result,
        });
        self.changed();
    }
    fn expansion_query(operation: &OperationId, path: &str) -> WorkflowQuery {
        WorkflowQuery::Overview {
            operation: operation.clone(),
            container: Some(path.into()),
            page: WorkflowPageRequest::default(),
        }
    }
    fn stop_job(&self, operation: &OperationId) -> Option<bob_kernel::JobId> {
        let summary = self.summary(operation)?;
        matches!(
            summary.state,
            WorkflowState::Pending
                | WorkflowState::Running
                | WorkflowState::WaitingCapacity
                | WorkflowState::WaitingPermission
                | WorkflowState::WaitingHuman
                | WorkflowState::Blocked
        )
        .then(|| summary.job.clone())
        .flatten()
    }
}

impl App {
    pub fn sync_workflow_scope(&mut self) {
        if self
            .workflows
            .scope
            .as_ref()
            .is_some_and(|(_, runtime)| *runtime != self.runtime_epoch)
        {
            let epoch = self.workflows.epoch.wrapping_add(1);
            self.workflows = WorkflowUi {
                epoch,
                ticket: self.workflows.ticket,
                ..Default::default()
            };
            if self.focus == Focus::Workflow {
                self.focus = Focus::Composer;
            }
        }
        if let Some(thread) = self
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == self.selected.as_ref())
        {
            self.workflows.synchronize(
                thread.session.clone(),
                self.runtime_epoch,
                thread.id.clone(),
            );
        }
    }
    pub fn workflow_conversation_selected(&mut self, thread: ThreadId) {
        self.sync_workflow_scope();
        self.workflows.navigate(Route::Conversation(thread));
        if self.focus == Focus::Workflow {
            self.focus = Focus::Composer;
        }
    }
    fn save_workflow_location(&mut self) {
        let Some(destination) = self.selected.clone() else {
            return;
        };
        let location = Location {
            route: self
                .workflows
                .route
                .clone()
                .unwrap_or_else(|| Route::Conversation(destination.clone())),
            destination,
            selected: self.workflows.selected,
            identity: self
                .workflows
                .rendered
                .as_ref()
                .and_then(|cache| cache.identity(self.workflows.selected)),
            top: self.workflows.top,
            sidebar_scroll: self.sidebar_scroll,
            sidebar_selected: self.workflows.sidebar_selected.clone(),
            list_cursor: self.workflows.list_cursor.clone(),
            list_history: self.workflows.list_history.clone(),
            expanded: self.workflows.expanded.clone(),
            anchor: self.pane().and_then(|pane| pane.viewport.anchor.clone()),
            focus: self.focus,
        };
        self.workflows.stack.push_back(location);
        if self.workflows.stack.len() > RETURN_LIMIT {
            self.workflows.stack.remove(1);
        }
    }
    pub fn open_workflow(&mut self, operation: OperationId, jobs: &mut JoinSet<Reply>) {
        self.workflow_action(
            Action::Open(Route::Workflow {
                operation,
                container: None,
                cursor: None,
            }),
            jobs,
        );
    }
    pub fn workflow_action(&mut self, action: Action, jobs: &mut JoinSet<Reply>) {
        match action {
            Action::Open(route) => {
                self.sync_workflow_scope();
                self.save_workflow_location();
                self.dismiss_inspector();
                if let Route::Conversation(thread) = &route {
                    self.select(thread.clone());
                }
                self.workflows.navigate(route);
                self.focus = if self.workflows.inspecting() {
                    Focus::Workflow
                } else {
                    Focus::Composer
                };
                self.focus_active_prompt();
            }
            Action::Back => {
                let Some(location) = self.workflows.stack.pop_back() else {
                    if self.workflows.inspecting() {
                        if let Some(thread) = self.selected.clone() {
                            self.workflows.navigate(Route::Conversation(thread));
                            self.focus = Focus::Composer;
                            self.focus_active_prompt();
                            self.hits.clear();
                        }
                    }
                    return;
                };
                self.select(location.destination);
                self.workflows.navigate(location.route);
                self.workflows.selected = location.selected;
                self.workflows.selection_restore = location.identity;
                self.workflows.top = location.top;
                self.sidebar_scroll = location.sidebar_scroll;
                self.workflows.sidebar_selected = location.sidebar_selected;
                self.workflows.list_cursor = location.list_cursor;
                self.workflows.list_history = location.list_history;
                self.workflows.expanded = location.expanded;
                if let Some(pane) = self.pane_mut() {
                    pane.viewport.anchor = location.anchor;
                }
                self.focus = location.focus;
                self.focus_active_prompt();
            }
            Action::Expand(path) => {
                if let Some(index) = self
                    .workflows
                    .expanded
                    .iter()
                    .position(|item| item == &path)
                {
                    self.workflows.expanded.remove(index);
                } else if self.workflows.expanded.len() < EXPANSION_LIMIT {
                    self.workflows.expanded.push(path);
                } else {
                    self.workflows.notify(
                        "Collapse a workflow container before expanding another (12 open maximum)."
                            .into(),
                    );
                }
                self.workflows.changed();
            }
            Action::Stop(operation) => {
                if let Some(job) = self.workflows.stop_job(&operation) {
                    self.application_shortcut(commands::Request::StopJob(job), jobs);
                }
            }
            Action::ListPage(cursor) => {
                if self.workflows.list_history.back() == Some(&cursor) {
                    self.workflows.list_history.pop_back();
                } else if cursor != self.workflows.list_cursor {
                    self.workflows
                        .list_history
                        .push_back(self.workflows.list_cursor.clone());
                    if self.workflows.list_history.len() > RETURN_LIMIT {
                        self.workflows.list_history.pop_front();
                    }
                }
                self.workflows.list_cursor = cursor;
                self.workflows.sidebar_selected = None;
                self.workflows.changed();
            }
            Action::Refresh => {
                self.workflows.list_cursor = None;
                self.workflows.list_history.clear();
                if let Some(Route::Workflow { cursor, .. } | Route::Node { cursor, .. }) =
                    &mut self.workflows.route
                {
                    *cursor = None;
                }
                self.workflows.epoch = self.workflows.epoch.wrapping_add(1);
                self.workflows.notice.clear();
                for entry in &mut self.workflows.cache {
                    entry.sequence = -1;
                }
                self.workflows.changed();
            }
            Action::None => {}
        }
        self.workflows.visual_changed();
        self.hits.clear();
        self.refresh_workflows(jobs);
    }
    pub fn refresh_workflows(&mut self, jobs: &mut JoinSet<Reply>) {
        self.sync_workflow_scope();
        let (Some(handle), Some((session, runtime))) =
            (self.handle.clone(), self.workflows.scope.clone())
        else {
            return;
        };
        let sequence = self.sequence_floor.max(
            self.live_snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.sequence),
        );
        let mut queries = Vec::new();
        if self.sidebar {
            queries.push(self.workflows.list_query());
        }
        if let Some(query) = self.workflows.route.as_ref().and_then(Route::query) {
            queries.push(query);
        }
        if let Some(operation) = self.workflows.route.as_ref().and_then(Route::operation) {
            for path in &self.workflows.expanded {
                queries.push(WorkflowUi::expansion_query(operation, path));
            }
        }
        for query in queries {
            if self.workflows.pending.len() >= 4 {
                break;
            }
            if self
                .workflows
                .pending
                .iter()
                .any(|(_, pending)| pending == &query)
                || self.workflows.cached(&query).is_some_and(|cached| {
                    cached.sequence >= sequence
                        || (cached.sequence >= 0
                            && matches!(query, WorkflowQuery::Text { .. })
                            && cached.result.is_ok())
                })
            {
                continue;
            }
            self.workflows.ticket += 1;
            let ticket = self.workflows.ticket;
            self.workflows.pending.push((ticket, query.clone()));
            let handle = handle.clone();
            let session = session.clone();
            let epoch = self.workflows.epoch;
            state::spawn_reply(jobs, runtime, async move {
                let result = handle
                    .workflow_query(session.clone(), query.clone())
                    .await
                    .map_err(|error| format!("{error:#}"));
                Reply::Workflow(QueryReply {
                    session,
                    runtime,
                    epoch,
                    ticket,
                    sequence,
                    query,
                    result,
                })
            });
        }
    }
    pub fn accept_workflow(&mut self, reply: QueryReply) {
        self.sync_workflow_scope();
        let error = if matches!(reply.query, WorkflowQuery::List { .. }) {
            reply.result.as_ref().err().cloned()
        } else {
            None
        };
        let revision = self.workflows.revision;
        self.workflows.accept(reply);
        if self.workflows.revision != revision {
            if let Some(error) = error {
                if let Some(pane) = self.pane_mut() {
                    pane.notify_current(format!("Workflow list unavailable: {error}"));
                }
            }
        }
    }
    pub fn is_workflow_tool(&self, operation: &OperationId) -> bool {
        self.pane()
            .and_then(|pane| pane.view.as_ref())
            .is_some_and(|view| {
                view.entries.iter().any(|entry| {
                    entry
                        .tool
                        .as_ref()
                        .is_some_and(|tool| &tool.id == operation && tool.name == "workflow")
                })
            })
    }
    pub fn workflow_for_entry(&self, sequence: i64) -> Option<OperationId> {
        self.pane()?
            .view
            .as_ref()?
            .entries
            .iter()
            .find(|entry| entry.sequence == sequence)
            .and_then(|entry| entry.operation.as_ref())
            .filter(|id| self.is_workflow_tool(id))
            .cloned()
    }
    pub fn workflow_key(&mut self, key: KeyEvent, jobs: &mut JoinSet<Reply>) -> bool {
        self.sync_workflow_scope();
        if matches!(
            self.focus,
            Focus::Permission
                | Focus::PermissionInvalidated
                | Focus::Question
                | Focus::Plan
                | Focus::ControlInvalidated
        ) {
            return self.workflows.inspecting()
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('x' | 'X'));
        }
        if key.code == KeyCode::Esc
            && self.focus != Focus::Inspector
            && (self.workflows.has_back() || self.workflows.inspecting())
        {
            self.workflow_action(Action::Back, jobs);
            return true;
        }
        if self.focus == Focus::Sidebar && self.workflow_sidebar_key(key, jobs) {
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('x' | 'X'))
            && self.workflows.inspecting()
        {
            if let Some(operation) = self
                .workflows
                .route
                .as_ref()
                .and_then(Route::operation)
                .cloned()
            {
                self.workflow_action(Action::Stop(operation), jobs);
            }
            return true;
        }
        if !self.workflows.inspecting() {
            return false;
        }
        if key.code == KeyCode::Tab {
            self.focus = if self.focus == Focus::Workflow {
                Focus::Composer
            } else if self.focus == Focus::Composer && self.sidebar {
                Focus::Sidebar
            } else {
                Focus::Workflow
            };
            return true;
        }
        if self.focus != Focus::Workflow {
            return false;
        }
        if key.code == KeyCode::Char('r') {
            self.workflow_action(Action::Refresh, jobs);
            true
        } else {
            self.workflow_navigation_key(key.code, jobs)
        }
    }
    pub fn workflow_scroll(&mut self, delta: isize) {
        self.workflows.scroll(delta);
    }
}
