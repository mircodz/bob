use super::*;
use crate::ui::style::{inset, safe, text, Palette, BAND_INSET};
use bob_kernel::{WorkflowArtifactKind, WorkflowNode, WorkflowTermination};
use ratatui::{layout::Rect, style::Color, text::Span};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RowIdentity {
    action: Action,
    role: String,
}

struct Model {
    identity: RowIdentity,
    parent: Option<usize>,
    body: bool,
    text: String,
    color: Color,
    action: Action,
    expansion: Option<String>,
    expansion_column: u16,
}
impl Model {
    fn new(text: impl Into<String>, action: Action) -> Self {
        let text = text.into();
        Self {
            identity: RowIdentity {
                action: action.clone(),
                role: text.clone(),
            },
            parent: None,
            body: false,
            text,
            color: Palette::TEXT(),
            action,
            expansion: None,
            expansion_column: 0,
        }
    }
    fn role(mut self, role: impl Into<String>) -> Self {
        self.identity.role = role.into();
        self
    }
    fn body(text: impl Into<String>) -> Self {
        Self {
            body: true,
            ..Self::new(text, Action::None)
        }
    }
}

pub(super) struct Cache {
    revision: u64,
    width: u16,
    theme: u64,
    models: Vec<Model>,
    pub actions: Vec<Action>,
    pub expansions: Vec<Option<String>>,
    pub parents: Vec<Option<usize>>,
    body_start: Option<usize>,
    window: Option<(usize, usize, usize, bool)>,
    visible: Vec<usize>,
    lines: Vec<Line<'static>>,
}

impl Cache {
    pub(super) fn identity(&self, selected: usize) -> Option<RowIdentity> {
        self.models
            .get(selected)
            .filter(|row| row.action != Action::None)
            .map(|row| row.identity.clone())
    }
    pub(super) fn scroll_len(&self) -> usize {
        self.models
            .len()
            .saturating_sub(self.body_start.unwrap_or(0))
    }
}

fn terminal(state: WorkflowState) -> bool {
    matches!(
        state,
        WorkflowState::Denied
            | WorkflowState::NotSelected
            | WorkflowState::Skipped
            | WorkflowState::Cancelled
            | WorkflowState::Completed
            | WorkflowState::Failed
    )
}
fn color(state: WorkflowState) -> Color {
    match state {
        WorkflowState::Failed | WorkflowState::Denied => Palette::ERROR(),
        WorkflowState::Completed => Palette::OK(),
        WorkflowState::Approval
        | WorkflowState::WaitingPermission
        | WorkflowState::WaitingHuman
        | WorkflowState::Blocked
        | WorkflowState::Unknown => Palette::WARN(),
        WorkflowState::Running | WorkflowState::Stopping => Palette::RUNNING(),
        _ => Palette::DIM(),
    }
}
fn label(state: WorkflowState) -> &'static str {
    match state {
        WorkflowState::Unadmitted => "not admitted",
        WorkflowState::Approval => "approval needed",
        WorkflowState::Denied => "denied",
        WorkflowState::Pending => "pending",
        WorkflowState::Future => "future step",
        WorkflowState::Running => "running",
        WorkflowState::WaitingCapacity => "waiting for capacity",
        WorkflowState::WaitingPermission => "waiting for permission",
        WorkflowState::WaitingHuman => "waiting for guidance",
        WorkflowState::Blocked => "blocked",
        WorkflowState::NotSelected => "not selected",
        WorkflowState::Skipped => "skipped",
        WorkflowState::Stopping => "stopping",
        WorkflowState::Cancelled => "cancelled",
        WorkflowState::Completed => "completed",
        WorkflowState::Failed => "failed",
        WorkflowState::Unknown => "unknown outcome",
    }
}
fn termination(value: WorkflowTermination) -> &'static str {
    match value {
        WorkflowTermination::ConditionMet => "condition met",
        WorkflowTermination::MaxRounds => "iteration cap reached; goal not confirmed",
        WorkflowTermination::Dry => "no new items",
        WorkflowTermination::Unknown => "termination unknown",
    }
}
fn kind_label(kind: WorkflowNodeKind) -> &'static str {
    match kind {
        WorkflowNodeKind::Agent => "agent",
        WorkflowNodeKind::FanOut => "fan-out",
        WorkflowNodeKind::Parallel => "parallel",
        WorkflowNodeKind::Loop => "loop",
        WorkflowNodeKind::Conditional => "conditional",
        WorkflowNodeKind::Branch => "branch",
        WorkflowNodeKind::Iteration => "iteration",
        WorkflowNodeKind::Unknown => "unknown kind",
    }
}
fn artifact_label(kind: WorkflowArtifactKind) -> &'static str {
    match kind {
        WorkflowArtifactKind::NodeResult => "Node output",
        WorkflowArtifactKind::IterationResult => "Iteration output",
        WorkflowArtifactKind::ChildReport => "Agent report",
        WorkflowArtifactKind::Aggregate => "Aggregate output",
        WorkflowArtifactKind::ToolOutput => "Tool output",
    }
}
fn iteration_label(node: &WorkflowNode) -> String {
    if node.instantiated
        && matches!(
            node.kind,
            WorkflowNodeKind::Loop | WorkflowNodeKind::Iteration
        )
    {
        format!(" · iteration {}", node.iteration + 1)
    } else {
        String::new()
    }
}
fn node_route(operation: &OperationId, path: &str) -> Route {
    Route::Node {
        operation: operation.clone(),
        path: path.into(),
        iterations: false,
        cursor: None,
    }
}
fn text_route(operation: &OperationId, source: WorkflowTextSource) -> Route {
    Route::Artifact {
        operation: operation.clone(),
        source,
        offset: 0,
    }
}
fn artifact(operation: &OperationId, id: &ArtifactId) -> Action {
    Action::Open(text_route(
        operation,
        WorkflowTextSource::Artifact {
            artifact: id.clone(),
        },
    ))
}
fn node_rows(
    ui: &WorkflowUi,
    operation: &OperationId,
    nodes: &[WorkflowNode],
    depth: usize,
    parent: Option<usize>,
    rows: &mut Vec<Model>,
) {
    for node in nodes {
        let expanded = ui.expanded.contains(&node.path);
        let prefix = if !node.expandable {
            "  "
        } else if expanded {
            "− "
        } else {
            "+ "
        };
        let mut row = Model::new(
            format!(
                "{}{}{} · {}{}{}",
                "  ".repeat(depth.min(12)),
                prefix,
                node.label,
                label(node.state),
                iteration_label(node),
                if node.instantiated {
                    ""
                } else {
                    " · definition"
                }
            ),
            Action::Open(node_route(operation, &node.path)),
        );
        row.identity.role = "node".into();
        row.parent = parent;
        row.color = color(node.state);
        row.expansion = node.expandable.then(|| node.path.clone());
        row.expansion_column = 2 + 2 * depth.min(12) as u16;
        let node_index = rows.len();
        rows.push(row);
        if let Some(branch) = &node.selected_branch {
            rows.push(Model::new(
                format!("{}  Chosen branch: {branch}", "  ".repeat(depth.min(12))),
                Action::None,
            ));
        }
        if let Some(end) = node.termination {
            rows.push(Model::new(
                format!("{}  {}", "  ".repeat(depth.min(12)), termination(end)),
                Action::None,
            ));
        }
        if expanded && depth < EXPANSION_LIMIT {
            let query = WorkflowUi::expansion_query(operation, &node.path);
            match ui.data(&query) {
                Some(WorkflowQueryResult::Overview(overview)) => {
                    node_rows(
                        ui,
                        operation,
                        &overview.nodes.rows,
                        depth + 1,
                        Some(node_index),
                        rows,
                    );
                    if let Some(cursor) = &overview.nodes.next_cursor {
                        rows.push(Model::new(
                            "Next page · container nodes…",
                            Action::Open(Route::Workflow {
                                operation: operation.clone(),
                                container: Some(node.path.clone()),
                                cursor: Some(cursor.clone()),
                            }),
                        ));
                        rows.last_mut().unwrap().parent = Some(node_index);
                    }
                }
                _ => {
                    let error = ui
                        .cached(&query)
                        .and_then(|entry| entry.result.as_ref().err());
                    rows.push(Model::new(
                        format!(
                            "    {}",
                            error.map(String::as_str).unwrap_or("Loading container…")
                        ),
                        Action::None,
                    ));
                    if error.is_some() {
                        rows.push(Model {
                            parent: Some(node_index),
                            ..Model::new("Retry container · r", Action::Refresh)
                                .role(format!("retry-container:{}", node.path))
                        });
                    }
                }
            }
        }
    }
}
fn models(ui: &WorkflowUi, width: u16) -> Vec<Model> {
    let mut rows = vec![Model::new("Back · Esc", Action::Back).role("back")];
    if ui.previous_page() {
        rows.push(Model::new("Previous page", Action::Back).role("previous-page"));
    }
    if !ui.notice.is_empty() {
        rows.push(Model {
            color: Palette::WARN(),
            ..Model::new(ui.notice.clone(), Action::None)
        });
    }
    let Some(route) = &ui.route else {
        return rows;
    };
    let Some(query) = route.query() else {
        return rows;
    };
    let Some(entry) = ui.cached(&query) else {
        rows.push(Model::new("Loading workflow…", Action::None));
        return rows;
    };
    let data = match &entry.result {
        Ok(data) => data,
        Err(error) => {
            rows.push(Model {
                color: Palette::ERROR(),
                ..Model::new(format!("Inspection unavailable: {error}"), Action::None)
            });
            rows.push(Model::new("Refresh inspection · r", Action::Refresh));
            return rows;
        }
    };
    match data.as_ref() {
        WorkflowQueryResult::Overview(overview) => {
            let workflow = &overview.workflow;
            rows.push(Model {
                color: color(workflow.state),
                ..Model::new(
                    format!("{} · {}", workflow.title, label(workflow.state)),
                    Action::None,
                )
            });
            rows.push(Model::new(
                format!(
                    "{} executions · {} active agents · max concurrency {}",
                    workflow.executions, workflow.active_agents, workflow.max_concurrency
                ),
                Action::None,
            ));
            rows.push(Model::new(
                "↑↓ select · Enter inspect · ← collapse / → expand · PgUp/PgDn scroll",
                Action::None,
            ));
            rows.push(Model::new(
                "Open owning conversation / approvals",
                Action::Open(Route::Conversation(workflow.owner.clone())),
            ));
            if overview.definition_available {
                rows.push(Model::new(
                    "Original definition / full input",
                    Action::Open(text_route(
                        &workflow.operation,
                        WorkflowTextSource::Definition,
                    )),
                ));
            }
            if let Some(error) = &overview.definition_error {
                rows.push(Model {
                    color: Palette::ERROR(),
                    ..Model::new(error.clone(), Action::None)
                });
            }
            if let Some(result) = &workflow.result {
                rows.push(Model::new(
                    "Aggregate output artifact",
                    artifact(&workflow.operation, result),
                ));
            }
            if ui.stop_job(&workflow.operation).is_some() {
                rows.push(Model {
                    color: Palette::WARN(),
                    ..Model::new(
                        "Stop workflow · Ctrl+X",
                        Action::Stop(workflow.operation.clone()),
                    )
                });
            }
            rows.push(Model::new("", Action::None));
            node_rows(
                ui,
                &workflow.operation,
                &overview.nodes.rows,
                0,
                None,
                &mut rows,
            );
            if let Some(cursor) = &overview.nodes.next_cursor {
                rows.push(Model::new(
                    "Next page · nodes…",
                    Action::Open(Route::Workflow {
                        operation: workflow.operation.clone(),
                        container: overview.container.clone(),
                        cursor: Some(cursor.clone()),
                    }),
                ));
            }
        }
        WorkflowQueryResult::Node(details) => {
            let operation = &details.operation;
            let node = &details.node;
            rows.push(Model {
                color: color(node.state),
                ..Model::new(
                    format!("{} · {}", node.label, label(node.state)),
                    Action::None,
                )
            });
            rows.push(Model::new(
                format!(
                    "{} · {}{}",
                    node.path,
                    kind_label(node.kind),
                    iteration_label(node)
                ),
                Action::None,
            ));
            if let Some(branch) = &node.selected_branch {
                rows.push(Model::new(format!("Chosen branch: {branch}"), Action::None));
            }
            if let Some(end) = node.termination {
                rows.push(Model::new(termination(end), Action::None));
            }
            if let Some(error) = &node.error_preview {
                rows.push(Model {
                    color: Palette::ERROR(),
                    ..Model::new(format!("Error: {error}"), Action::None)
                });
            }
            if let Some(thread) = &node.child {
                rows.push(Model::new(
                    "Open agent conversation",
                    Action::Open(Route::Conversation(thread.clone())),
                ));
            }
            if node.expandable {
                rows.push(Model::new(
                    "Open ordered children",
                    Action::Open(Route::Workflow {
                        operation: operation.clone(),
                        container: Some(node.path.clone()),
                        cursor: None,
                    }),
                ));
            }
            if node.kind == WorkflowNodeKind::Loop {
                rows.push(Model::new(
                    "Iteration history",
                    Action::Open(Route::Node {
                        operation: operation.clone(),
                        path: node.path.clone(),
                        iterations: true,
                        cursor: None,
                    }),
                ));
            }
            rows.push(Model::new(
                "Specification / full input",
                Action::Open(text_route(
                    operation,
                    WorkflowTextSource::NodeSpecification {
                        path: node.path.clone(),
                    },
                )),
            ));
            if let Some(result) = &node.result {
                rows.push(Model::new("Output artifact", artifact(operation, result)));
            }
            for link in &details.artifacts {
                if node.result.as_ref() != Some(&link.artifact) {
                    rows.push(Model::new(
                        format!("{} artifact", artifact_label(link.kind)),
                        artifact(operation, &link.artifact),
                    ));
                }
            }
            for (index, blocker) in details.blockers.iter().enumerate() {
                let action = blocker
                    .thread
                    .as_ref()
                    .map(|thread| Action::Open(Route::Conversation(thread.clone())))
                    .or_else(|| {
                        blocker
                            .path
                            .as_ref()
                            .map(|path| Action::Open(node_route(operation, path)))
                    })
                    .unwrap_or(Action::None);
                rows.push(Model {
                    color: color(blocker.state),
                    ..Model::new(
                        format!("Blocker · {}: {}", label(blocker.state), blocker.message),
                        action,
                    )
                    .role(format!("blocker:{index}"))
                });
            }
            for dependency in &details.dependencies {
                rows.push(Model {
                    color: color(dependency.state),
                    ..Model::new(
                        format!(
                            "Dependency: {} · {}",
                            dependency.path,
                            label(dependency.state)
                        ),
                        Action::Open(node_route(operation, &dependency.path)),
                    )
                    .role("dependency")
                });
            }
            if details.dependencies_truncated {
                rows.push(Model::new(
                    "More dependencies retained in full specification",
                    Action::Open(text_route(
                        operation,
                        WorkflowTextSource::NodeSpecification {
                            path: node.path.clone(),
                        },
                    )),
                ));
            }
            rows.push(Model::new("Specification preview:", Action::None));
            for source in safe(&details.specification_preview).split('\n') {
                rows.extend(
                    crate::ui::style::wrap(
                        &Line::raw(source.to_owned()),
                        width.saturating_sub(2).max(1) as usize,
                    )
                    .into_iter()
                    .map(|(_, line)| Model::new(line.to_string(), Action::None)),
                );
            }
            if details.specification_truncated {
                rows.push(Model::new(
                    "Full specification available above",
                    Action::None,
                ));
            }
        }
        WorkflowQueryResult::Iterations(page) => {
            let Some(operation) = route.operation() else {
                return rows;
            };
            rows.push(Model::new(
                "Retained iterations · Enter opens full output",
                Action::None,
            ));
            for iteration in &page.rows {
                rows.push(
                    Model::new(
                        format!(
                            "Iteration {} · {}{}",
                            iteration.iteration + 1,
                            iteration.path,
                            iteration
                                .termination
                                .map_or(String::new(), |end| format!(" · {}", termination(end)))
                        ),
                        artifact(operation, &iteration.result),
                    )
                    .role(format!(
                        "iteration:{}:{}",
                        iteration.path, iteration.iteration
                    )),
                );
            }
            if let (Some(cursor), Route::Node { path, .. }) = (&page.next_cursor, route) {
                rows.push(Model::new(
                    "Next page · iterations…",
                    Action::Open(Route::Node {
                        operation: operation.clone(),
                        path: path.clone(),
                        iterations: true,
                        cursor: Some(cursor.clone()),
                    }),
                ));
            }
        }
        WorkflowQueryResult::Text(page) => {
            rows.push(Model::new(
                format!(
                    "Retained content · bytes {}–{} of {}",
                    page.offset,
                    page.offset + page.text.len(),
                    page.total_bytes
                ),
                Action::None,
            ));
            if let Some(offset) = page.next_offset {
                rows.push(Model::new(
                    "Next page · text…",
                    Action::Open(Route::Artifact {
                        operation: page.operation.clone(),
                        source: page.source.clone(),
                        offset,
                    }),
                ));
            }
            rows.push(Model::new(
                "↑↓ / PgUp / PgDn scroll · ←→ actions · Enter open · Esc back",
                Action::None,
            ));
            for source in safe(&page.text).split('\n') {
                rows.extend(
                    crate::ui::style::wrap(
                        &Line::raw(source.to_owned()),
                        width.saturating_sub(2).max(1) as usize,
                    )
                    .into_iter()
                    .map(|(_, line)| Model::body(line.to_string())),
                );
            }
        }
        _ => {}
    }
    rows
}

pub(super) fn prepare(ui: &mut WorkflowUi, width: Option<u16>) {
    let width = width.unwrap_or_else(|| ui.rendered.as_ref().map_or(80, |cache| cache.width));
    let theme = bob_ui::theme::generation();
    if ui.rendered.as_ref().is_none_or(|cache| {
        cache.revision != ui.revision || cache.width != width || cache.theme != theme
    }) {
        let identity = ui.selection_restore.take().or_else(|| {
            ui.rendered
                .as_ref()
                .and_then(|cache| cache.identity(ui.selected))
        });
        let models = models(ui, width);
        if let Some(identity) = identity {
            ui.selected = models
                .iter()
                .position(|row| row.identity == identity)
                .unwrap_or(0);
        }
        let eligible: Vec<_> = models
            .iter()
            .enumerate()
            .filter_map(|(index, row)| (row.action != Action::None).then_some(index))
            .collect();
        ui.selected = bob_ui::widgets::eligible_selection(
            Some(ui.selected),
            &eligible,
            bob_ui::widgets::SelectionMove::Nearest,
        )
        .unwrap_or(0);
        ui.rendered = Some(Cache {
            revision: ui.revision,
            width,
            theme,
            actions: models.iter().map(|row| row.action.clone()).collect(),
            expansions: models.iter().map(|row| row.expansion.clone()).collect(),
            parents: models.iter().map(|row| row.parent).collect(),
            body_start: models.iter().position(|row| row.body),
            models,
            window: None,
            visible: Vec::new(),
            lines: Vec::new(),
        });
        #[cfg(test)]
        {
            ui.frame_builds += 1;
        }
    }
}

pub(in crate::ui) fn draw(frame: &mut Frame, app: &mut App, area: Rect, hits: &mut Vec<Hit>) {
    let area = inset(area, BAND_INSET);
    let focused = app.focus == Focus::Workflow;
    let ui = &mut app.workflows;
    prepare(ui, Some(area.width));
    let cache = ui.rendered.as_mut().unwrap();
    ui.selected = ui.selected.min(cache.models.len().saturating_sub(1));
    let pinned = cache
        .body_start
        .map_or(0, |start| start.min(area.height.saturating_sub(1) as usize));
    ui.height = (area.height as usize).saturating_sub(pinned);
    if cache.body_start.is_none()
        && cache
            .window
            .is_some_and(|(top, selected, _, _)| selected != ui.selected && top == ui.top)
    {
        ui.top = bob_ui::widgets::keep_visible(ui.top, ui.selected, ui.height, cache.models.len());
    }
    ui.top = bob_ui::widgets::scroll_offset(ui.top, 0, cache.scroll_len(), ui.height);
    let window = (ui.top, ui.selected, area.height as usize, focused);
    if cache.window != Some(window) {
        cache.visible.clear();
        if let Some(body_start) = cache.body_start {
            let start = bob_ui::widgets::keep_visible(0, ui.selected, pinned, body_start);
            cache
                .visible
                .extend(start..start.saturating_add(pinned).min(body_start));
            cache.visible.extend(
                (body_start + ui.top)..(body_start + ui.top + ui.height).min(cache.models.len()),
            );
        } else {
            cache
                .visible
                .extend(ui.top..(ui.top + ui.height).min(cache.models.len()));
        }
        cache.lines = cache
            .visible
            .iter()
            .map(|&index| {
                let row = &cache.models[index];
                let selected = focused && index == ui.selected && row.action != Action::None;
                let style = if selected {
                    text().fg(row.color).bg(Palette::SELECTED_BG())
                } else {
                    text().fg(row.color)
                };
                let label = bob_ui::widgets::fit_text(
                    &safe(&row.text),
                    area.width.saturating_sub(2) as usize,
                );
                Line::from(vec![
                    Span::styled(if selected { "› " } else { "  " }, style),
                    Span::styled(label, style),
                ])
            })
            .collect();
        cache.window = Some(window);
        #[cfg(test)]
        {
            ui.formatted_rows += cache.lines.len();
        }
    }
    for (index, line) in cache.lines.iter().enumerate() {
        let row = cache.visible[index];
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        frame.render_widget(Paragraph::new(line.clone()), rect);
        hits.push(Hit {
            area: rect,
            action: if cache.actions[row] == Action::None {
                HitAction::WorkflowRow(ui.selected, Action::None)
            } else {
                HitAction::WorkflowRow(row, cache.actions[row].clone())
            },
        });
        if let Some(path) = &cache.expansions[row] {
            let column = cache.models[row].expansion_column;
            if column < rect.width.saturating_sub(1) {
                hits.push(Hit {
                    area: Rect::new(rect.x + column, rect.y, 2.min(rect.width - column), 1),
                    action: HitAction::WorkflowRow(row, Action::Expand(path.clone())),
                });
            }
        }
    }
}

pub(super) struct SidebarItem {
    pub action: Action,
    label: String,
    depth: usize,
    color: Color,
    done: bool,
    selected: bool,
}
pub(super) fn sidebar_items(app: &App) -> Vec<SidebarItem> {
    let query = app.workflows.list_query();
    let entry = app.workflows.cached(&query);
    let empty = bob_kernel::WorkflowPage {
        rows: Vec::new(),
        next_cursor: None,
    };
    let page = match app.workflows.data(&query) {
        Some(WorkflowQueryResult::List(page)) => page,
        _ if entry.is_some() || app.workflows.list_cursor.is_some() => &empty,
        _ => return Vec::new(),
    };
    let threads = app.visible_threads();
    let details_by_thread: std::collections::HashMap<_, _> = app
        .thread_details
        .iter()
        .map(|details| (&details.thread, details))
        .collect();
    let operations: std::collections::HashSet<_> =
        page.rows.iter().map(|summary| &summary.operation).collect();
    let group_for = |thread: &ThreadSummary| {
        details_by_thread
            .get(&thread.id)
            .and_then(|details| details.workflow_operation.as_ref())
            .filter(|operation| operations.contains(operation))
    };
    let thread_row = |thread: &ThreadSummary, grouped: bool| {
        let details = details_by_thread.get(&thread.id).copied();
        SidebarItem {
            action: Action::Open(Route::Conversation(thread.id.clone())),
            label: if thread.parent_thread.is_none() {
                "main".into()
            } else {
                details
                    .filter(|details| !details.task.trim().is_empty())
                    .map_or_else(|| thread.name.clone(), |details| details.task.clone())
            },
            depth: details
                .map_or(usize::from(thread.parent_thread.is_some()), |details| {
                    details.depth
                })
                .max(usize::from(grouped) + 1)
                .saturating_sub(usize::from(thread.parent_thread.is_none())),
            color: if thread.needs_approval {
                Palette::WARN()
            } else if thread.parent_thread.is_none() && thread.state != ThreadState::Active {
                Palette::FAINT()
            } else {
                crate::ui::style::thread_color(&thread.state)
            },
            done: thread.parent_thread.is_some()
                && crate::ui::style::terminal_thread(&thread.state),
            selected: !app.workflows.inspecting() && app.selected.as_ref() == Some(&thread.id),
        }
    };
    let mut rows = Vec::new();
    let mut grouped: std::collections::HashMap<_, Vec<_>> = std::collections::HashMap::new();
    for thread in &threads {
        if let Some(operation) = group_for(thread) {
            grouped.entry(operation).or_default().push(*thread);
        } else {
            rows.push(thread_row(thread, false));
        }
    }
    let mut groups: Vec<_> = page.rows.iter().collect();
    groups.sort_by_key(|summary| terminal(summary.state));
    for summary in groups {
        rows.push(SidebarItem {
            action: Action::Open(Route::Workflow {
                operation: summary.operation.clone(),
                container: None,
                cursor: None,
            }),
            label: format!("Workflow: {}", summary.title),
            depth: 1,
            color: color(summary.state),
            done: terminal(summary.state),
            selected: app.workflows.route.as_ref().and_then(Route::operation)
                == Some(&summary.operation),
        });
        if let Some(threads) = grouped.get(&summary.operation) {
            rows.extend(threads.iter().map(|thread| thread_row(thread, true)));
        }
    }
    if let Some(error) = entry.and_then(|entry| entry.result.as_ref().err()) {
        rows.push(SidebarItem {
            action: Action::None,
            label: format!("Workflow list unavailable: {error}"),
            depth: 1,
            color: Palette::ERROR(),
            done: false,
            selected: false,
        });
        rows.push(SidebarItem {
            action: Action::Refresh,
            label: "Retry workflows · r".into(),
            depth: 1,
            color: Palette::ACCENT(),
            done: false,
            selected: false,
        });
    }
    if let Some(cursor) = &page.next_cursor {
        rows.push(SidebarItem {
            action: Action::ListPage(Some(cursor.clone())),
            label: "Next page · workflows…".into(),
            depth: 1,
            color: Palette::ACCENT(),
            done: false,
            selected: false,
        });
    }
    if app.workflows.list_cursor.is_some() {
        rows.push(SidebarItem {
            action: Action::ListPage(app.workflows.list_history.back().cloned().flatten()),
            label: "Previous page · workflows".into(),
            depth: 1,
            color: Palette::ACCENT(),
            done: false,
            selected: false,
        });
    }
    rows
}

fn sidebar_selection(app: &App, rows: &[SidebarItem]) -> Option<usize> {
    match &app.workflows.sidebar_selected {
        Some(action) => rows
            .iter()
            .position(|row| row.action != Action::None && &row.action == action),
        None => rows
            .iter()
            .position(|row| row.selected && row.action != Action::None)
            .or_else(|| rows.iter().position(|row| row.action != Action::None)),
    }
}

pub(in crate::ui) fn draw_sidebar(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    hits: &mut Vec<Hit>,
) -> bool {
    let rows = sidebar_items(app);
    if rows.is_empty() {
        return false;
    }
    let bg = Style::default().bg(Palette::INPUT_BG());
    frame.render_widget(ratatui::widgets::Clear, area);
    frame.render_widget(Block::default().style(bg), area);
    let selected = if app.focus == Focus::Sidebar {
        let selected = sidebar_selection(app, &rows);
        if app.workflows.sidebar_selected.is_none() {
            app.workflows.sidebar_selected = selected.map(|index| rows[index].action.clone());
        }
        selected
    } else {
        rows.iter().position(|row| row.selected)
    };
    let height = area.height.saturating_sub(4) as usize;
    app.workflows.sidebar_height = height;
    if app.workflows.sidebar_position != selected {
        if let Some(selected) = selected {
            app.sidebar_scroll =
                bob_ui::widgets::keep_visible(app.sidebar_scroll, selected, height, rows.len());
        }
        app.workflows.sidebar_position = selected;
    }
    let start = bob_ui::widgets::scroll_offset(app.sidebar_scroll, 0, rows.len(), height);
    let mut lines = vec![
        Line::default(),
        Line::styled("  AGENTS · workflows", bg.fg(Palette::DIM())),
    ];
    for (index, row) in rows.iter().enumerate().skip(start).take(height) {
        lines.push(bob_ui::sidebar::row(
            &safe(&row.label),
            row.depth,
            row.color,
            row.done,
            Some(index) == selected && app.focus == Focus::Sidebar,
            area.width.saturating_sub(2) as usize,
        ));
        if row.action != Action::None {
            hits.push(Hit {
                area: Rect::new(
                    area.x + 1,
                    area.y + 2 + (index - start) as u16,
                    area.width.saturating_sub(2),
                    1,
                ),
                action: HitAction::WorkflowSidebar(row.action.clone()),
            });
        }
    }
    lines.push(Line::default());
    lines.push(Line::styled(
        "  ↑↓ select · PgUp/PgDn scroll · Enter open · ⌃t close",
        bg.fg(Palette::FAINT()),
    ));
    frame.render_widget(Paragraph::new(lines), inset(area, 1));
    app.sidebar_scroll = start;
    true
}

impl App {
    pub(in crate::ui) fn workflow_sidebar_scroll(&mut self, delta: isize) {
        let rows = sidebar_items(self);
        self.sidebar_scroll = bob_ui::widgets::scroll_offset(
            self.sidebar_scroll,
            delta,
            rows.len(),
            self.workflows.sidebar_height,
        );
        self.workflows.visual_changed();
    }

    pub(super) fn workflow_sidebar_key(
        &mut self,
        key: KeyEvent,
        jobs: &mut JoinSet<Reply>,
    ) -> bool {
        if key.code == KeyCode::Char('r') && key.modifiers.is_empty() {
            self.workflow_action(Action::Refresh, jobs);
            return true;
        }
        let rows = sidebar_items(self);
        if rows.is_empty() {
            return self.workflows.sidebar_selected.is_some()
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('x' | 'X'));
        }
        let index = sidebar_selection(self, &rows);
        let action = index.map(|index| rows[index].action.clone());
        if self.workflows.sidebar_selected.is_none() {
            self.workflows.sidebar_selected = action.clone();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('x' | 'X'))
        {
            if let Some(Action::Open(Route::Workflow { operation, .. })) = action {
                self.workflow_action(Action::Stop(operation), jobs);
            }
            return true;
        }
        match key.code {
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown => {
                use bob_ui::widgets::{eligible_selection, SelectionMove};
                let eligible: Vec<_> = rows
                    .iter()
                    .enumerate()
                    .filter_map(|(index, row)| (row.action != Action::None).then_some(index))
                    .collect();
                let movement = match key.code {
                    KeyCode::Up => SelectionMove::Previous,
                    KeyCode::Down => SelectionMove::Next,
                    KeyCode::Home => SelectionMove::First,
                    KeyCode::End => SelectionMove::Last,
                    _ => SelectionMove::Nearest,
                };
                let height = self.workflows.sidebar_height.max(1);
                let target = match key.code {
                    KeyCode::PageUp => Some(index.unwrap_or(0).saturating_sub(height)),
                    KeyCode::PageDown => Some(index.unwrap_or(0).saturating_add(height)),
                    _ => index,
                };
                if let Some(next) = eligible_selection(target, &eligible, movement) {
                    self.workflows.sidebar_selected = Some(rows[next].action.clone());
                    self.sidebar_scroll = bob_ui::widgets::keep_visible(
                        self.sidebar_scroll,
                        next,
                        height,
                        rows.len(),
                    );
                    self.workflows.visual_changed();
                }
            }
            KeyCode::Enter => {
                if let Some(action) = action {
                    self.workflow_action(action, jobs);
                }
            }
            _ => return false,
        }
        true
    }
}
