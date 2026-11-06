use super::style::*;
use super::*;
use bob_kernel::Entry;
use ratatui::{layout::Rect, text::Span};
use std::{
    collections::{hash_map::DefaultHasher, HashSet},
    hash::{Hash, Hasher},
};

pub(super) struct InspectorCache {
    key: (u64, u16, u64, usize, usize, bool),
    lines: Vec<Line<'static>>,
}

pub(super) struct RenderCache {
    pub key: u64,
    pub rows: Vec<Row>,
}
fn cache_key(pane: &Pane, root: bool, width: u16) -> u64 {
    let mut hash = DefaultHasher::new();
    root.hash(&mut hash);
    width.hash(&mut hash);
    bob_ui::theme::generation().hash(&mut hash);
    pane.before.hash(&mut hash);
    for notice in pane.visible_notices() {
        notice.hash(&mut hash);
    }
    pane.content_key.hash(&mut hash);
    pane.inline.revision().hash(&mut hash);
    pane.submission_revision.hash(&mut hash);
    let mut expanded: Vec<_> = pane.expanded.iter().collect();
    expanded.sort_by_key(|id| &id.0);
    expanded.hash(&mut hash);
    for id in expanded {
        pane.full_inputs.contains_key(id).hash(&mut hash);
        pane.pending_inputs.contains(id).hash(&mut hash);
        pane.input_errors.get(id).hash(&mut hash);
        pane.expanded_outputs.contains_key(id).hash(&mut hash);
        pane.pending_outputs.contains_key(id).hash(&mut hash);
        pane.output_errors.get(id).hash(&mut hash);
    }
    hash.finish()
}

pub(super) fn content_key(view: &View) -> u64 {
    let mut hash = DefaultHasher::new();
    {
        view.error.hash(&mut hash);
        for entry in &view.entries {
            entry.sequence.hash(&mut hash);
            entry.kind.hash(&mut hash);
            entry.text.hash(&mut hash);
            entry.operation.hash(&mut hash);
            entry.sender.hash(&mut hash);
            entry.parent_guidance.hash(&mut hash);
            entry
                .submission
                .as_ref()
                .map(|submission| (&submission.id, submission.state))
                .hash(&mut hash);
            entry.is_preview.hash(&mut hash);
            entry.visible_text_bytes.hash(&mut hash);
            if let Some(tool) = &entry.tool {
                tool.id.hash(&mut hash);
                tool.name.hash(&mut hash);
                tool.state.hash(&mut hash);
                preview(&tool.input_preview).hash(&mut hash);
                tool.result.hash(&mut hash);
                format!("{:?}", tool.presentation).hash(&mut hash);
            }
        }
    }
    hash.finish()
}

pub(super) struct Row {
    pub anchor: Anchor,
    pub line: Line<'static>,
    pub band: bool,
    pub hit: HitAction,
}
pub(super) fn append(
    rows: &mut Vec<Row>,
    identity: impl Into<AnchorId>,
    lines: Vec<Line<'static>>,
    band: bool,
    hit: HitAction,
    width: u16,
) {
    let identity = identity.into();
    let width = width.saturating_sub(BAND_INSET * 2).max(1) as usize;
    let base = rows
        .last()
        .filter(|row| row.anchor.identity == identity)
        .map_or(0, |row| row.anchor.line + 1);
    for (line, content) in lines.into_iter().enumerate() {
        let line = base + line;
        let wrapped = if content.style.bg.is_some() {
            wrap(&content, width)
        } else {
            prose_wrap(&content, width)
        };
        for (offset, content) in wrapped {
            rows.push(Row {
                anchor: Anchor {
                    identity: identity.clone(),
                    line,
                    offset,
                },
                line: content,
                band,
                hit: hit.clone(),
            });
        }
    }
}
fn append_output(
    rows: &mut Vec<Row>,
    entry: &Entry,
    source: &str,
    width: u16,
    limit: usize,
) -> bool {
    if source.is_empty() {
        return false;
    }
    let body_width = width.saturating_sub(BAND_INSET * 2 + 4).max(1) as usize;
    let base = rows
        .last()
        .filter(|row| row.anchor.identity == AnchorId::Entry(entry.sequence))
        .map_or(0, |row| row.anchor.line + 1);
    let mut count = 0;
    for (line, source) in safe(source).split('\n').enumerate() {
        for (offset, mut content) in wrap(&Line::styled(source.to_owned(), text()), body_width) {
            if count == limit {
                return true;
            }
            content.spans.insert(0, Span::raw("    "));
            rows.push(Row {
                anchor: Anchor {
                    identity: AnchorId::Entry(entry.sequence),
                    line: base + line,
                    offset,
                },
                line: content,
                band: false,
                hit: entry
                    .operation
                    .as_ref()
                    .map_or(HitAction::Entry(entry.sequence), |id| {
                        HitAction::Tool(id.clone())
                    }),
            });
            count += 1;
        }
    }
    false
}

fn preview(input: &serde_json::Value) -> String {
    [
        "command",
        "path",
        "pattern",
        "name",
        "description",
        "task",
        "text",
    ]
    .into_iter()
    .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
    .map(|s| clipped(s.lines().next().unwrap_or_default(), 160))
    .unwrap_or_default()
}
fn header(name: String, state: OperationState, detail: String) -> Line<'static> {
    let status = match state {
        OperationState::Done => "",
        OperationState::Started => " · running",
        OperationState::Preparing => " · preparing",
        OperationState::Ready => " · ready",
        OperationState::Approval => " · approval needed",
        OperationState::Failed => " · failed",
        OperationState::Unknown => " · unknown outcome",
        OperationState::Interrupted => " · interrupted",
        OperationState::Cancelled => " · cancelled",
        OperationState::Resolved => " · resolved with evidence",
        OperationState::Pending | OperationState::Waiting => " · waiting",
    };
    Line::from(vec![
        Span::styled("• ", Style::default().fg(state_color(&state))),
        Span::styled(name, bold()),
        Span::styled(
            if detail.is_empty() {
                String::new()
            } else {
                format!(" {detail}")
            },
            text(),
        ),
        Span::styled(status, Style::default().fg(state_color(&state))),
    ])
}
fn agent_failure(entry: &Entry) -> Option<(&str, &str)> {
    if entry.kind != EntryKind::Coordination {
        return None;
    }
    let (id, report) = entry.text.strip_prefix("Agent ")?.split_once(' ')?;
    uuid::Uuid::parse_str(id).ok()?;
    let (state, body) = report.split_once(": ")?;
    matches!(state, "failed" | "cancelled").then_some((state, body))
}

fn append_notice(rows: &mut Vec<Row>, notice: &state::Notice, width: u16) {
    append(
        rows,
        AnchorId::Notice {
            id: notice.id,
            sequence: notice.sequence,
        },
        safe(&clipped(&notice.text, 2000))
            .lines()
            .map(|line| Line::styled(line.to_owned(), text()))
            .chain(std::iter::once(Line::default()))
            .collect(),
        false,
        HitAction::Composer,
        width,
    );
}

#[cfg(test)]
pub(super) fn transcript(pane: &Pane, root: bool, width: u16) -> Vec<Row> {
    transcript_range(pane, root, width, None)
}

fn transcript_window(pane: &Pane, root: bool, width: u16) -> Vec<Row> {
    transcript_range(pane, root, width, super::inline::entry_window(pane))
}

fn transcript_range(
    pane: &Pane,
    root: bool,
    width: u16,
    range: Option<std::ops::Range<usize>>,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let Some(view) = &pane.view else {
        append(
            &mut rows,
            0,
            vec![Line::styled("Loading conversation…", text())],
            false,
            HitAction::Composer,
            width,
        );
        return rows;
    };
    let notices: Vec<_> = pane.visible_notices().collect();
    let mut groups = Vec::new();
    let mut start = 0;
    for notice in &notices {
        let end = view
            .entries
            .partition_point(|entry| entry.sequence <= notice.sequence);
        groups.extend(exploration::groups(&view.entries[start..end]));
        start = end;
    }
    groups.extend(exploration::groups(&view.entries[start..]));
    let mut notices = notices.into_iter().peekable();
    let mut seen = HashSet::new();
    let range = range.unwrap_or(0..view.entries.len());
    if range.start > 0 {
        rows.push(super::inline::entry_gap(
            &view.entries[range.start - 1],
            true,
        ));
    }
    for (index, entry) in view.entries.iter().enumerate() {
        if !range.contains(&index) {
            continue;
        }
        while notices
            .peek()
            .is_some_and(|notice| notice.sequence < entry.sequence)
        {
            let notice = notices.next().unwrap();
            if range.start == 0 || notice.sequence >= view.entries[range.start].sequence {
                append_notice(&mut rows, notice, width);
            }
        }
        let agent_failure = agent_failure(entry);
        if root && !entry.visible_in_root() {
            continue;
        }
        if visibility::hidden_tool(entry) || !pane.submission_visible(entry) {
            continue;
        }
        if let Some(tool) = &entry.tool {
            if !seen.insert(tool.id.clone()) {
                continue;
            }
            if let Some(group) = groups.iter().find(|group| group.first() == &tool.id) {
                group.render(pane, &mut rows, width);
                seen.extend(
                    group
                        .entries
                        .iter()
                        .map(|entry| entry.tool.as_ref().unwrap().id.clone()),
                );
                continue;
            }
            let result = view.entries.iter().find(|candidate| {
                candidate.operation.as_ref() == Some(&tool.id)
                    && matches!(candidate.kind, EntryKind::ToolResult | EntryKind::ToolError)
            });
            let output_page = pane.expanded_outputs.get(&tool.id);
            let mut full_entry;
            let shown = if pane.full_inputs.contains_key(&tool.id) || output_page.is_some() {
                full_entry = entry.clone();
                let shown_tool = full_entry.tool.as_mut().unwrap();
                if let Some(input) = pane.full_inputs.get(&tool.id) {
                    shown_tool.input_preview = input.clone();
                }
                if output_page.is_some() {
                    if let Some(presentation) = &mut shown_tool.presentation {
                        presentation.body_preview = None;
                    }
                }
                &full_entry
            } else {
                entry
            };
            let mut full_result;
            let result = if let (Some(page), Some(result)) = (output_page, result) {
                full_result = result.clone();
                full_result.text = page.text.clone();
                full_result.is_preview = page.next_offset.is_some();
                Some(&full_result)
            } else {
                result
            };
            let input_state = if pane.pending_inputs.contains(&tool.id) {
                tool_render::BashInputState::Loading
            } else if let Some(error) = pane.input_errors.get(&tool.id) {
                tool_render::BashInputState::Failed(error)
            } else {
                tool_render::BashInputState::Unavailable
            };
            tool_render::tool_with_input_state(
                &mut rows,
                shown,
                result,
                pane.expanded.contains(&tool.id),
                width,
                input_state,
            );
            if pane.expanded.contains(&tool.id) {
                let status = if pane.pending_outputs.contains_key(&tool.id) {
                    Some(("Loading full output…".into(), Palette::DIM()))
                } else {
                    pane.output_errors.get(&tool.id).map(|error| {
                        (
                            format!("Full output unavailable: {}", safe(error)),
                            Palette::ERROR(),
                        )
                    })
                };
                if let Some((status, color)) = status {
                    append(
                        &mut rows,
                        entry.sequence,
                        vec![
                            Line::styled(format!("    {status}"), text().fg(color)),
                            Line::default(),
                        ],
                        false,
                        HitAction::Output(tool.id.clone()),
                        width,
                    );
                }
            }
            continue;
        }
        if super::inline::long(entry) {
            rows.extend(super::inline::rows(pane, entry, width));
            rows.push(Row {
                anchor: Anchor {
                    identity: AnchorId::Entry(entry.sequence),
                    line: 0,
                    offset: 0,
                },
                line: Line::default(),
                band: false,
                hit: HitAction::Entry(entry.sequence),
            });
            continue;
        }
        if entry.kind == EntryKind::Assistant {
            if !entry.text.trim().is_empty() {
                rows.extend(super::live::complete_rows(pane, entry, width));
                rows.push(Row {
                    anchor: Anchor {
                        identity: AnchorId::Entry(entry.sequence),
                        line: 0,
                        offset: 0,
                    },
                    line: Line::default(),
                    band: false,
                    hit: HitAction::Entry(entry.sequence),
                });
            }
            continue;
        }
        let kind = if !root && entry.kind == EntryKind::Coordination && entry.parent_guidance {
            EntryKind::Submission
        } else {
            entry.kind
        };
        let band = kind == EntryKind::Submission;
        let mut lines = match kind {
            EntryKind::Coordination => {
                if let Some((state, body)) = agent_failure {
                    let mut lines = vec![header(
                        "Agent report".into(),
                        if state == "failed" {
                            OperationState::Failed
                        } else {
                            OperationState::Cancelled
                        },
                        String::new(),
                    )];
                    lines.extend(
                        safe(body)
                            .lines()
                            .map(|line| Line::styled(line.to_owned(), text())),
                    );
                    lines
                } else if let Some(sender) = &entry.sender {
                    let name = sender
                        .name
                        .as_deref()
                        .filter(|name| !name.trim().is_empty())
                        .unwrap_or(&sender.thread.0);
                    let name = safe(name).split_whitespace().collect::<Vec<_>>().join(" ");
                    let mut lines = vec![Line::styled(format!("Message from {name}"), bold())];
                    lines.extend(markdown::markdown(&entry.text).into_iter().map(|mut line| {
                        line.spans.insert(0, Span::raw("   "));
                        line
                    }));
                    lines
                } else {
                    safe(&entry.text)
                        .lines()
                        .map(|line| Line::styled(line.to_owned(), text()))
                        .collect()
                }
            }
            EntryKind::ToolError => {
                append(
                    &mut rows,
                    entry.sequence,
                    vec![header(
                        "Tool".into(),
                        OperationState::Unknown,
                        "Details unavailable · inspect".into(),
                    )],
                    false,
                    HitAction::Entry(entry.sequence),
                    width,
                );
                let truncated = append_output(&mut rows, entry, &entry.text, width, 6);
                if truncated {
                    vec![Line::styled(
                        "  … click to inspect retained output",
                        text().fg(Palette::DIM()),
                    )]
                } else {
                    Vec::new()
                }
            }
            EntryKind::ToolResult => {
                let truncated = append_output(&mut rows, entry, &entry.text, width, 6);
                if truncated {
                    vec![Line::styled(
                        "  … click to inspect retained output",
                        text().fg(Palette::DIM()),
                    )]
                } else {
                    Vec::new()
                }
            }
            EntryKind::Assistant if entry.text.trim().is_empty() && !entry.is_preview => continue,
            EntryKind::Assistant => markdown::markdown(&entry.text),
            EntryKind::Submission => {
                append(
                    &mut rows,
                    entry.sequence,
                    vec![Line::default()],
                    true,
                    HitAction::Entry(entry.sequence),
                    width,
                );
                let body_width = width.saturating_sub(BAND_INSET * 2 + 4).max(1) as usize;
                for (line, source) in safe(&entry.text).split('\n').enumerate() {
                    for (offset, mut content) in
                        prose_wrap(&Line::styled(source.to_owned(), text()), body_width)
                    {
                        let marker = if line == 0 && offset == 0 {
                            "› "
                        } else {
                            "  "
                        };
                        content
                            .spans
                            .insert(0, Span::styled(marker, text().fg(Palette::DIM())));
                        rows.push(Row {
                            anchor: Anchor {
                                identity: AnchorId::Entry(entry.sequence),
                                line: line + 1,
                                offset,
                            },
                            line: content,
                            band: true,
                            hit: HitAction::Entry(entry.sequence),
                        });
                    }
                }
                vec![Line::default()]
            }
            EntryKind::ToolCall => vec![header(
                "Tool".into(),
                OperationState::Unknown,
                "Details unavailable · inspect".into(),
            )],
            EntryKind::AgentStarted => vec![header(
                "Started agent:".into(),
                OperationState::Done,
                entry
                    .text
                    .strip_prefix("Started agent ")
                    .unwrap_or(&entry.text)
                    .split(" (")
                    .next()
                    .unwrap_or("agent")
                    .to_owned(),
            )],
            EntryKind::Recovery => {
                vec![Line::styled(safe(&entry.text), text().fg(Palette::WARN()))]
            }
            _ => safe(&entry.text)
                .lines()
                .map(|line| Line::styled(line.to_owned(), text()))
                .collect(),
        };
        if entry.is_preview {
            lines.push(Line::styled(
                "… preview · click to inspect retained content",
                Style::default().fg(Palette::DIM()),
            ));
        }
        append(
            &mut rows,
            entry.sequence,
            lines,
            band,
            HitAction::Entry(entry.sequence),
            width,
        );
        append(
            &mut rows,
            entry.sequence,
            vec![Line::default()],
            false,
            HitAction::Entry(entry.sequence),
            width,
        );
    }
    if range.end < view.entries.len() {
        rows.push(super::inline::entry_gap(&view.entries[range.end], false));
    }
    for notice in notices {
        if (range.start == 0 || notice.sequence >= view.entries[range.start].sequence)
            && (range.end == view.entries.len()
                || notice.sequence < view.entries[range.end].sequence)
        {
            append_notice(&mut rows, notice, width);
        }
    }
    if let Some(error) = &view.error {
        append(
            &mut rows,
            i64::MAX - 1,
            std::iter::once(Line::styled("Kernel error", bold().fg(Palette::ERROR())))
                .chain(
                    safe(error)
                        .lines()
                        .map(|line| Line::styled(line.to_owned(), text())),
                )
                .chain(std::iter::once(Line::default()))
                .collect(),
            false,
            HitAction::Composer,
            width,
        );
    }
    rows
}

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    app.sync_workflow_scope();
    app.focus_active_prompt();
    let area = frame.area();
    app.size = area;
    app.hits.clear();
    frame.render_widget(Block::default().style(text().bg(Palette::BG())), area);
    if area.width < 12 || area.height < 5 {
        let rejected = app.draft_mut().limit_reached;
        let layout = composer::layout(app.draft_mut(), area.width.max(1) as usize);
        let error = app
            .live_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.error.as_ref());
        let lines = if let Some(error) = error {
            vec![Line::styled(
                format!("Kernel error: {}", safe(error)),
                bold().fg(Palette::ERROR()),
            )]
        } else if rejected {
            vec![Line::styled(
                composer::LIMIT_NOTICE,
                text().fg(Palette::WARN()),
            )]
        } else {
            layout
                .rows
                .iter()
                .skip(
                    layout
                        .cursor_row
                        .saturating_sub(area.height.saturating_sub(1) as usize),
                )
                .take(area.height as usize)
                .cloned()
                .collect()
        };
        frame.render_widget(Paragraph::new(lines).style(text()), area);
        sanitize_buffer(frame.buffer_mut());
        return;
    }
    let side = if app.sidebar {
        52.min(area.width.saturating_sub(24))
    } else {
        0
    };
    let main = Rect::new(area.x, area.y, area.width.saturating_sub(side), area.height);
    let input_width = main.width.saturating_sub(BAND_INSET * 2 + 4).max(1) as usize;
    let rejected = app.draft_mut().limit_reached;
    let input = composer::layout(app.draft_mut(), input_width);
    let cursor_row = input.cursor_row;
    let cursor_col = input.cursor_col;
    let content_height = input
        .rows
        .len()
        .clamp(1, 12)
        .min(main.height.saturating_sub(4).max(1) as usize);
    let has_permission = app.pane().is_some_and(|p| p.permission.is_some());
    let controls_height = if has_permission {
        0
    } else {
        control_render::prompt_height(app, main.width)
    };
    let has_prompt = has_permission || controls_height > 0;
    let composer_height = if has_prompt && main.height < 12 {
        0
    } else {
        (content_height as u16 + 2).min(main.height.saturating_sub(if has_prompt { 7 } else { 1 }))
    };
    let content_height = content_height.min(composer_height.saturating_sub(2) as usize);
    let composer_y = main.bottom().saturating_sub(1 + composer_height);
    let composer = Rect::new(main.x, composer_y, main.width, composer_height);
    let todo_lines = if app.show_todos {
        app.pane()
            .map(|pane| control_render::todo_lines(pane, main.width.saturating_sub(BAND_INSET * 2)))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let todos_height = (todo_lines.len() as u16)
        .min(14)
        .min(composer_y.saturating_sub(main.y + if has_prompt { 7 } else { 2 }) / 3);
    let panel_width = main.width.saturating_sub(BAND_INSET * 2) as usize;
    let job_lines = app
        .pane()
        .map(|pane| jobs::panel_lines(&pane.jobs, panel_width))
        .unwrap_or_default();
    let queue_lines = app
        .pane()
        .map(|pane| jobs::queue_lines(&pane.visible_queue(), panel_width))
        .unwrap_or_default();
    let auxiliary_budget = composer_y.saturating_sub(main.y + if has_prompt { 7 } else { 2 }) / 3;
    let jobs_height = (job_lines.len() as u16).min(8).min(auxiliary_budget);
    let queue_height = (queue_lines.len() as u16).min(6).min(auxiliary_budget);
    let prompt_bottom = composer_y.saturating_sub(todos_height + jobs_height + queue_height);
    let invalidated_permission = app.focus == Focus::PermissionInvalidated
        && !has_prompt
        && app.pane().is_none_or(|pane| !pane.decision_followup);
    let permission_height = if has_permission {
        9.min(prompt_bottom.saturating_sub(main.y))
    } else if invalidated_permission {
        3.min(prompt_bottom.saturating_sub(main.y))
    } else {
        controls_height.min(prompt_bottom.saturating_sub(main.y))
    };
    let permission_area = Rect::new(
        main.x,
        prompt_bottom.saturating_sub(permission_height),
        main.width,
        permission_height,
    );
    let live_snapshot = app.live_snapshot.clone();
    let selected_id = app.selected.clone();
    let selected_thread = app
        .threads
        .iter()
        .find(|thread| Some(&thread.id) == selected_id.as_ref());
    let activity_line = selected_thread
        .filter(|_| {
            !has_prompt
                && app.focus != Focus::Inspector
                && !app.workflows.inspecting()
                && app.pane().is_some_and(|pane| pane.before.is_none())
        })
        .and_then(|thread| {
            let attempt = live_snapshot
                .as_ref()
                .and_then(|snapshot| {
                    snapshot.attempts.iter().rev().find(|attempt| {
                        attempt.thread == thread.id
                            && thread.run.as_ref() == Some(&attempt.run)
                            && matches!(thread.state, ThreadState::Active | ThreadState::Stopping)
                            && attempt.settled_sequence.is_none()
                            && app.pane().is_none_or(|pane| {
                                !pane.committed_operations.contains(&attempt.operation)
                            })
                    })
                })
                .map(std::sync::Arc::as_ref);
            activity::line(
                thread,
                attempt,
                app.activity.since(&thread.id),
                app.started_at.elapsed(),
                if app.focus == Focus::Composer
                    && (app.picker.is_some() || !app.completion.items.is_empty())
                {
                    "^X stop · esc close"
                } else if app.workflows.has_back() {
                    "^X stop · esc back"
                } else if thread.parent_thread.is_some() {
                    "^X stop · esc main"
                } else if app.focus == Focus::Sidebar {
                    "esc to compose"
                } else {
                    "esc to interrupt"
                },
            )
        });
    let notice_line = live_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.error.as_ref())
        .map(|error| {
            Line::styled(
                format!("Kernel error: {}", safe(error)),
                bold().fg(Palette::ERROR()),
            )
        })
        .or_else(|| {
            app.pane()
                .and_then(|pane| pane.inline.error.as_ref())
                .map(|error| {
                    Line::styled(
                        format!("Reply unavailable: {}", safe(error)),
                        bold().fg(Palette::ERROR()),
                    )
                })
        })
        .or_else(|| {
            rejected.then(|| Line::styled(composer::LIMIT_NOTICE, text().fg(Palette::WARN())))
        })
        .or_else(|| {
            app.pane()
                .and_then(|pane| pane.receipt.as_ref())
                .filter(|(_, when)| {
                    activity_line.is_none() && when.elapsed() < Duration::from_secs(3)
                })
                .map(|(receipt, _)| Line::styled(receipt.clone(), text().fg(Palette::DIM())))
        });
    let mut activity_rows = Vec::new();
    if let Some(line) = activity_line.filter(|_| notice_line.is_none()) {
        append(
            &mut activity_rows,
            AnchorId::Activity,
            vec![line, Line::default()],
            false,
            HitAction::Live,
            main.width,
        );
    }
    let activity_height = u16::from(notice_line.is_some() && permission_area.y > main.y);
    let transcript_area = Rect::new(
        main.x,
        main.y,
        main.width,
        permission_area.y.saturating_sub(main.y + activity_height),
    );
    if let Some(line) = notice_line.filter(|_| activity_height > 0) {
        frame.render_widget(
            Paragraph::new(line),
            inset(
                Rect::new(main.x, transcript_area.bottom(), main.width, 1),
                BAND_INSET,
            ),
        );
    }
    let root = app
        .threads
        .iter()
        .find(|t| Some(&t.id) == app.selected.as_ref())
        .is_none_or(|t| t.parent_thread.is_none());
    let mut hits = Vec::new();
    if app.focus == Focus::Inspector {
        if let Some(pane) = app.pane_mut() {
            if let Some(inspector) = pane.inspector.as_mut() {
                let key = (
                    pane.inspection_epoch,
                    transcript_area.width,
                    bob_ui::theme::generation(),
                    inspector.text.as_ptr() as usize,
                    inspector.text.len(),
                    inspector.pending,
                );
                let cache = match pane
                    .inspector_rendered
                    .take()
                    .filter(|cache| cache.key == key)
                {
                    Some(cache) => cache,
                    None => {
                        let mut lines = vec![
                            Line::styled(safe(&inspector.title), bold()),
                            Line::styled(
                                if matches!(
                                    inspector.target,
                                    InspectionTarget::Entry(_)
                                        | InspectionTarget::ToolOutput(_)
                                        | InspectionTarget::Job(_)
                                ) {
                                    format!(
                                        "Bytes {}–{} of {} · ↑/↓ scroll · n/p pages · Esc closes",
                                        inspector.offset,
                                        inspector.offset + inspector.text.len(),
                                        inspector.total
                                    )
                                } else {
                                    format!(
                                        "Bytes {}–{} · artifact slice · n/p pages · Esc closes",
                                        inspector.offset,
                                        inspector.offset + inspector.text.len()
                                    )
                                },
                                Style::default().fg(Palette::DIM()),
                            ),
                            Line::default(),
                        ];
                        lines.extend(
                            safe(&inspector.text)
                                .lines()
                                .map(|line| Line::styled(line.to_owned(), text())),
                        );
                        let lines: Vec<_> = lines
                            .iter()
                            .flat_map(|line| {
                                wrap(
                                    line,
                                    transcript_area.width.saturating_sub(BAND_INSET * 2).max(1)
                                        as usize,
                                )
                                .into_iter()
                                .map(|(_, line)| line)
                            })
                            .collect();
                        #[cfg(test)]
                        {
                            pane.inspector_builds += 1;
                        }
                        InspectorCache { key, lines }
                    }
                };
                let max = cache
                    .lines
                    .len()
                    .saturating_sub(transcript_area.height as usize)
                    .min(u16::MAX as usize) as u16;
                inspector.scroll = inspector.scroll.min(max);
                frame.render_widget(
                    Paragraph::new(
                        cache
                            .lines
                            .iter()
                            .skip(inspector.scroll as usize)
                            .take(transcript_area.height as usize)
                            .cloned()
                            .collect::<Vec<_>>(),
                    ),
                    inset(transcript_area, BAND_INSET),
                );
                pane.inspector_rendered = Some(cache);
            }
        }
    } else if app.workflows.inspecting() {
        workflow::draw(frame, app, transcript_area, &mut hits);
    } else if let Some(pane) = app.pane_mut() {
        super::inline::resize(pane, transcript_area.width);
        super::inline::handoff(pane);
        if pane.content_key.is_none() {
            pane.content_key = pane.view.as_ref().map(content_key);
        }
        let key = cache_key(pane, root, transcript_area.width);
        let cache = match pane.rendered.take().filter(|cache| cache.key == key) {
            Some(cache) => cache,
            None => {
                let rows = transcript_window(pane, root, transcript_area.width);
                pane.viewport.rows = rows.iter().map(|row| row.anchor.clone()).collect();
                #[cfg(test)]
                {
                    pane.render_builds += 1;
                }
                RenderCache { key, rows }
            }
        };
        if let Some(thread) = &selected_id {
            live::rows(
                pane,
                live_snapshot.as_deref(),
                thread,
                transcript_area.width,
            );
        }
        let live_rows = pane
            .live_rendered
            .as_ref()
            .map_or(&[][..], |cache| cache.rows.as_slice());
        pane.viewport.rows.truncate(cache.rows.len());
        pane.viewport.rows.extend(
            live_rows
                .iter()
                .chain(&activity_rows)
                .map(|row| row.anchor.clone()),
        );
        if pane.viewport.anchor.is_none() {
            pane.pinned_before = None;
        }
        let top = pane.viewport.position(transcript_area.height as usize);
        for (y, row) in cache
            .rows
            .iter()
            .chain(live_rows)
            .chain(&activity_rows)
            .skip(top)
            .take(transcript_area.height as usize)
            .enumerate()
        {
            let rect = Rect::new(
                transcript_area.x,
                transcript_area.y + y as u16,
                transcript_area.width,
                1,
            );
            if row.band {
                frame.render_widget(
                    Block::default().style(Style::default().bg(Palette::INPUT_BG())),
                    inset(rect, BAND_INSET),
                );
            }
            frame.render_widget(
                Paragraph::new(row.line.clone()).style(if row.band {
                    text().bg(Palette::INPUT_BG())
                } else {
                    text()
                }),
                inset(rect, if row.band { BAND_INSET + 1 } else { BAND_INSET }),
            );
            hits.push(Hit {
                area: rect,
                action: row.hit.clone(),
            });
        }
        let needs_layout = super::inline::visible(pane, top, transcript_area.height as usize);
        if !needs_layout {
            pane.rendered = Some(cache);
        }
        if pane.viewport.anchor.is_some() && transcript_area.height > 0 {
            let label = " Reading · Shift+↓ to follow ";
            let width = (label.len() as u16).min(transcript_area.width);
            frame.render_widget(
                Paragraph::new(label).style(text().fg(Palette::DIM()).bg(Palette::INPUT_BG())),
                Rect::new(
                    transcript_area.x + transcript_area.width.saturating_sub(width) / 2,
                    transcript_area.bottom() - 1,
                    width,
                    1,
                ),
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new(safe(&app.status))
                .style(text())
                .wrap(Wrap { trim: false }),
            inset(transcript_area, BAND_INSET),
        );
    }
    if todos_height > 0 {
        frame.render_widget(
            Paragraph::new(
                todo_lines
                    .into_iter()
                    .take(todos_height as usize)
                    .collect::<Vec<_>>(),
            )
            .style(text()),
            inset(
                Rect::new(main.x, prompt_bottom, main.width, todos_height),
                BAND_INSET,
            ),
        );
    }
    if jobs_height > 0 {
        frame.render_widget(
            Paragraph::new(job_lines).style(text()),
            inset(
                Rect::new(
                    main.x,
                    prompt_bottom + todos_height,
                    main.width,
                    jobs_height,
                ),
                BAND_INSET,
            ),
        );
    }
    if queue_height > 0 {
        frame.render_widget(
            Paragraph::new(queue_lines).style(text()),
            inset(
                Rect::new(main.x, composer_y - queue_height, main.width, queue_height),
                BAND_INSET,
            ),
        );
    }
    if !has_permission && controls_height > 0 {
        control_render::draw_prompt(frame, app, permission_area, &mut hits);
    }
    if invalidated_permission {
        frame.render_widget(
            Block::default().style(text().bg(Palette::INPUT_BG())),
            inset(permission_area, BAND_MARGIN),
        );
        frame.render_widget(
            Paragraph::new(
                "Permission changed or closed.\nTab to review current permission · Esc to compose",
            )
            .style(text().fg(Palette::WARN()))
            .wrap(Wrap { trim: false }),
            inset(permission_area, BAND_INSET),
        );
    } else if has_permission && permission_height > 0 {
        let focused = app.focus == Focus::Permission;
        if let Some(permission) = app.pane_mut().and_then(|p| p.permission.as_mut()) {
            frame.render_widget(
                Block::default().style(text().bg(Palette::BG())),
                permission_area,
            );
            let top_pad = u16::from(permission_area.height > 2);
            let content = inset(
                Rect {
                    y: permission_area.y + top_pad,
                    height: permission_area.height.saturating_sub(top_pad),
                    ..permission_area
                },
                BAND_INSET,
            );
            let choice_rows =
                (permission.options.len().max(1) as u16).min(content.height.saturating_sub(2));
            let hint_rows = 1;
            let header_rows = u16::from(content.height > choice_rows + hint_rows);
            let height = content
                .height
                .saturating_sub(header_rows + choice_rows + hint_rows);
            if header_rows > 0 {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        approval::title(&permission.decision),
                        bold().fg(Palette::WARN()),
                    )),
                    Rect::new(content.x, content.y, content.width, 1),
                );
            }
            if permission.rendered.as_ref().is_none_or(|cache| {
                cache.width != content.width
                    || cache.details != permission.details
                    || cache.theme != bob_ui::theme::generation()
            }) {
                permission.rendered = Some(approval::Cache {
                    width: content.width,
                    details: permission.details,
                    theme: bob_ui::theme::generation(),
                    rows: approval::rows(
                        &permission.decision,
                        permission.details,
                        content.width as usize,
                    ),
                });
            }
            let details = &permission.rendered.as_ref().unwrap().rows;
            permission.scroll = permission.scroll.min(
                details
                    .len()
                    .saturating_sub(height as usize)
                    .min(u16::MAX as usize) as u16,
            );
            frame.render_widget(
                Paragraph::new(
                    details
                        .iter()
                        .skip(permission.scroll as usize)
                        .take(height as usize)
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                Rect::new(content.x, content.y + header_rows, content.width, height),
            );
            let start = permission
                .choice
                .unwrap_or(0)
                .saturating_sub(choice_rows.saturating_sub(1) as usize);
            let mut choices = permission
                .options
                .iter()
                .enumerate()
                .skip(start)
                .take(choice_rows as usize)
                .map(|(index, option)| {
                    approval::option_line(
                        option,
                        index,
                        focused && permission.choice == Some(index),
                    )
                })
                .collect::<Vec<_>>();
            if permission.pending {
                choices = vec![Line::styled(
                    "Decision pending…",
                    text().fg(Palette::WARN()),
                )];
            } else if !permission.options_loaded {
                choices = vec![Line::styled(
                    "Loading approval options…",
                    text().fg(Palette::DIM()),
                )];
            }
            let choice_y = content.bottom().saturating_sub(choice_rows + hint_rows);
            frame.render_widget(
                Paragraph::new(choices),
                Rect::new(content.x, choice_y, content.width, choice_rows),
            );
            let hint = if permission.pending || !permission.options_loaded {
                "Waiting for current approval options"
            } else if focused {
                if content.width < 32 {
                    "↑↓ · 1-9 · Enter · Esc deny"
                } else {
                    "↑↓ move · 1-9 pick · enter confirm · esc deny · PgUp/PgDn inspect"
                }
            } else {
                "Tab to choose · Esc deny · draft is separate"
            };
            frame.render_widget(
                Paragraph::new(hint).style(text().fg(Palette::FAINT())),
                Rect::new(
                    content.x,
                    content.bottom().saturating_sub(hint_rows),
                    content.width,
                    hint_rows,
                ),
            );
            hits.push(Hit {
                area: permission_area,
                action: HitAction::Permission,
            });
        }
    }
    let band = inset(composer, BAND_INSET);
    frame.render_widget(Block::default().style(text().bg(Palette::INPUT_BG())), band);
    if app.workflows.inspecting() && composer.height > 0 {
        let destination = app
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == app.selected.as_ref())
            .map_or("conversation", |thread| {
                if thread.parent_thread.is_none() {
                    "main"
                } else {
                    thread.name.as_str()
                }
            });
        frame.render_widget(
            Paragraph::new(bob_ui::widgets::fit_text(
                &format!("Message to {} · Tab compose · Esc back", safe(destination)),
                band.width as usize,
            ))
            .style(text().fg(Palette::ACCENT()).bg(Palette::INPUT_BG())),
            Rect::new(band.x, composer.y, band.width, 1),
        );
    }
    let start_row = bob_ui::widgets::keep_visible(
        app.draft_mut().scroll,
        cursor_row,
        content_height,
        input.rows.len(),
    );
    app.draft_mut().scroll = start_row;
    let input_x = band.x + 1;
    let input_y = composer.y + u16::from(composer.height > 1);
    if composer.height > 1 {
        frame.render_widget(
            Paragraph::new(if start_row == 0 { "›" } else { " " })
                .style(text().fg(Palette::DIM()).bg(Palette::INPUT_BG())),
            Rect::new(input_x, input_y, 1.min(main.width), 1),
        );
        let empty = app.draft_mut().text.is_empty();
        let busy = app
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == app.selected.as_ref())
            .is_some_and(|thread| {
                matches!(thread.state, ThreadState::Active | ThreadState::Stopping)
            });
        let input_rows = if empty && !busy {
            vec![Line::styled(
                if app
                    .threads
                    .iter()
                    .find(|thread| Some(&thread.id) == app.selected.as_ref())
                    .is_some_and(|thread| thread.parent_thread.is_some())
                {
                    "send a message...  (esc → main)"
                } else {
                    "send a message...  (Ctrl+J or Shift+Enter for newline)"
                },
                text().fg(Palette::FAINT()),
            )]
        } else {
            input
                .rows
                .iter()
                .skip(start_row)
                .take(content_height)
                .cloned()
                .collect::<Vec<_>>()
        };
        frame.render_widget(
            Paragraph::new(input_rows).style(text().bg(Palette::INPUT_BG())),
            Rect::new(
                input_x + 2,
                input_y,
                input_width.min(main.width.saturating_sub(BAND_INSET + 2) as usize) as u16,
                content_height as u16,
            ),
        );
        if app.focus == Focus::Composer && main.width > BAND_INSET + 2 {
            let x =
                (input_x + 2 + cursor_col as u16).min(main.right().saturating_sub(BAND_INSET + 1));
            let y = (input_y + cursor_row.saturating_sub(start_row) as u16)
                .min(composer.bottom().saturating_sub(1));
            frame.set_cursor_position((x, y));
        }
    }
    hits.push(Hit {
        area: composer,
        action: HitAction::Composer,
    });
    let selected = app
        .threads
        .iter()
        .find(|t| Some(&t.id) == app.selected.as_ref());
    if main.height > 0 {
        let mut spans = Vec::new();
        let separator = || Span::styled("  ", text().fg(Palette::FAINT()));
        if let Some(thread) = selected {
            if let Some(session) = app.pane().and_then(|pane| pane.session.as_ref()) {
                let (mode, color) = if session.permissions.mode == bob_kernel::PermissionMode::Plan
                {
                    ("plan", Palette::WARN())
                } else if session.policy.yolo {
                    ("YOLO", Palette::ERROR())
                } else if session.permissions.mode == bob_kernel::PermissionMode::AutoAccept {
                    ("auto-accept", Palette::OK())
                } else {
                    ("normal", Palette::DIM())
                };
                spans.push(Span::styled(mode, text().fg(color)));
            } else if let Some(policy) = app
                .pane()
                .and_then(|p| p.view.as_ref())
                .and_then(|v| v.policies.iter().find(|p| p.session == thread.session))
            {
                spans.push(Span::styled(
                    if policy.policy.yolo { "YOLO" } else { "normal" },
                    text().fg(if policy.policy.yolo {
                        Palette::ERROR()
                    } else {
                        Palette::DIM()
                    }),
                ));
            }
            if thread.parent_thread.is_some() {
                spans.push(separator());
                spans.push(Span::styled(
                    format!(
                        "to:{}",
                        bob_ui::widgets::fit_text(
                            &safe(&thread.name),
                            main.width.saturating_sub(20) as usize
                        )
                    ),
                    text().fg(Palette::ACCENT()),
                ));
            } else if let Some(context) = app.pane().and_then(|pane| pane.context.as_ref()) {
                spans.push(separator());
                spans.push(Span::styled(
                    format!(
                        "ctx {}%",
                        context.budget.estimated_input_tokens.saturating_mul(100)
                            / context.budget.context_window.max(1)
                    ),
                    text().fg(Palette::DIM()),
                ));
            }
        } else {
            spans.push(Span::styled("loading", text().fg(Palette::DIM())));
        }
        if let Some(workspace) = &app.workspace {
            spans.push(separator());
            spans.push(Span::styled(
                safe(
                    &dirs::home_dir()
                        .and_then(|home| {
                            workspace.strip_prefix(home).ok().map(|rest| {
                                if rest.as_os_str().is_empty() {
                                    "~".to_owned()
                                } else {
                                    format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
                                }
                            })
                        })
                        .unwrap_or_else(|| workspace.display().to_string()),
                ),
                text().fg(Palette::ACCENT()),
            ));
        }
        if let Some(branch) = &app.branch {
            spans.push(separator());
            spans.push(Span::styled(
                format!("⎇ {}", safe(branch)),
                text().fg(Palette::LINK()),
            ));
        }
        if let Some(services) = &app.services {
            for (name, health) in services.lsp_statuses() {
                use bob_core::lsp::Health;
                let (glyph, label, color) = match health {
                    Health::Starting => ("○", name, Palette::DIM()),
                    Health::Indexing(Some(percent)) => {
                        ("◐", format!("{name} {percent}%"), Palette::WARN())
                    }
                    Health::Indexing(None) => ("◐", name, Palette::WARN()),
                    Health::Ready => ("●", name, Palette::OK()),
                    Health::Failed(_) => ("●", name, Palette::ERROR()),
                };
                spans.push(separator());
                spans.push(Span::styled(
                    format!("{glyph} {}", safe(&label)),
                    text().fg(color),
                ));
            }
        }
        let bar = inset(Rect::new(main.x, main.bottom() - 1, main.width, 1), 3);
        let mut hint = String::new();
        if app
            .visible_threads()
            .iter()
            .any(|thread| thread.parent_thread.is_some())
        {
            hint.push_str("^T agents  ");
        }
        if app
            .pane()
            .and_then(|pane| pane.controls.as_ref())
            .is_some_and(|controls| !controls.todos.is_empty())
        {
            hint.push_str(if app.show_todos {
                "^L hide todos"
            } else {
                "^L show todos"
            });
        }
        let reserve = if hint.len() + 24 <= bar.width as usize {
            hint.len() as u16 + 2
        } else {
            0
        };
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                width: bar.width.saturating_sub(reserve),
                ..bar
            },
        );
        if reserve > 0 {
            frame.render_widget(
                Paragraph::new(hint.as_str()).style(text().fg(Palette::FAINT())),
                Rect::new(bar.right() - hint.len() as u16, bar.y, hint.len() as u16, 1),
            );
        }
    }
    if side > 0
        && !workflow::draw_sidebar(
            frame,
            app,
            Rect::new(main.right(), area.y, side, area.height),
            &mut hits,
        )
    {
        let sidebar = Rect::new(main.right(), area.y, side, area.height);
        let bg = Style::default().bg(Palette::INPUT_BG());
        frame.render_widget(ratatui::widgets::Clear, sidebar);
        frame.render_widget(Block::default().style(bg), sidebar);
        let mut lines = vec![
            Line::default(),
            Line::styled(
                format!(
                    "  AGENTS · {} running",
                    app.visible_threads()
                        .iter()
                        .filter(|thread| thread.parent_thread.is_some()
                            && matches!(
                                thread.state,
                                ThreadState::Active | ThreadState::Held | ThreadState::Stopping
                            ))
                        .count()
                ),
                bg.fg(Palette::DIM()),
            ),
        ];
        let threads = app.visible_threads();
        let selected = threads
            .iter()
            .position(|t| Some(&t.id) == app.selected.as_ref())
            .unwrap_or(0);
        let height = sidebar.height.saturating_sub(4) as usize;
        let start = if height == 0 {
            0
        } else {
            bob_ui::widgets::keep_visible(app.sidebar_scroll, selected, height, threads.len())
        };
        for (y, thread) in threads.into_iter().skip(start).take(height).enumerate() {
            let done = thread.parent_thread.is_some() && terminal_thread(&thread.state);
            let color = if thread.needs_approval {
                Palette::WARN()
            } else if thread.parent_thread.is_none() && thread.state != ThreadState::Active {
                Palette::FAINT()
            } else {
                thread_color(&thread.state)
            };
            let details = app
                .thread_details
                .iter()
                .find(|details| details.thread == thread.id);
            let depth = details.map_or_else(
                || {
                    let mut depth = 0;
                    let mut parent = thread.parent_thread.as_ref();
                    while let Some(id) = parent {
                        depth += 1;
                        parent = app
                            .threads
                            .iter()
                            .find(|candidate| &candidate.id == id)
                            .and_then(|candidate| candidate.parent_thread.as_ref());
                        if depth >= app.threads.len() {
                            break;
                        }
                    }
                    depth
                },
                |details| details.depth,
            );
            let label = details
                .filter(|details| !details.task.trim().is_empty())
                .map_or(thread.name.as_str(), |details| details.task.as_str());
            let line = bob_ui::sidebar::row(
                if thread.parent_thread.is_none() {
                    "main"
                } else {
                    label
                },
                depth,
                color,
                done,
                app.selected.as_ref() == Some(&thread.id),
                sidebar.width.saturating_sub(2) as usize,
            );
            let rect = Rect::new(
                sidebar.x + 1,
                sidebar.y + 2 + y as u16,
                sidebar.width.saturating_sub(2),
                1,
            );
            lines.push(line);
            hits.push(Hit {
                area: rect,
                action: HitAction::Thread(thread.id.clone()),
            });
        }
        app.sidebar_scroll = start;
        lines.push(Line::default());
        lines.push(Line::styled(
            "  ↑↓ select · ^X stop · esc main · ⌃t close",
            bg.fg(Palette::FAINT()),
        ));
        frame.render_widget(Paragraph::new(lines).style(bg), inset(sidebar, 1));
    }
    if app.focus == Focus::Composer && app.picker.is_none() && !app.completion.items.is_empty() {
        let height = (app.completion.items.len() as u16).min(composer.y.saturating_sub(main.y));
        let start = app
            .completion
            .selected
            .saturating_sub(height.saturating_sub(1) as usize);
        let menu = Rect::new(
            main.x,
            composer.y.saturating_sub(height),
            main.width,
            height,
        );
        frame.render_widget(ratatui::widgets::Clear, menu);
        frame.render_widget(Block::default().style(text().bg(Palette::POPUP_BG())), menu);
        for (row, (index, item)) in app
            .completion
            .items
            .iter()
            .enumerate()
            .skip(start)
            .take(height as usize)
            .enumerate()
        {
            let selected = index == app.completion.selected;
            let bg = if selected {
                Palette::SELECTED_BG()
            } else {
                Palette::POPUP_BG()
            };
            let command_style = if selected {
                bold().fg(Palette::ACCENT()).bg(bg)
            } else {
                text().bg(bg)
            };
            let mut spans = vec![Span::styled(
                format!(" {} ", if selected { "❯" } else { " " }),
                text().fg(Palette::ACCENT()).bg(bg),
            )];
            if app.completion.kind == Some(completion::Kind::File) {
                spans.push(Span::styled(safe(&item.label), command_style));
            } else {
                spans.push(Span::styled(
                    format!("{:<8}", safe(&item.label)),
                    command_style,
                ));
                spans.push(Span::styled(
                    format!("  {}", safe(&item.description)),
                    text().fg(Palette::FAINT()).bg(bg),
                ));
            }
            let line = Line::from(spans);
            let rect = Rect::new(menu.x, menu.y + row as u16, menu.width, 1);
            frame.render_widget(
                Paragraph::new(line).style(text().bg(Palette::POPUP_BG())),
                rect,
            );
            hits.push(Hit {
                area: rect,
                action: HitAction::Completion(index),
            });
        }
    }
    control_render::draw_picker(frame, app, main, composer, &mut hits);
    app.hits = hits;
    sanitize_buffer(frame.buffer_mut());
}
