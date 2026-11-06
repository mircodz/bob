use super::*;
use bob_ui::{sanitize::safe_display_text, theme::Palette};
use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

pub(super) fn single_line(text: &str) -> String {
    safe_display_text(text).replace(['\n', '\r', '\t'], " ")
}

fn clipped(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        if used + grapheme.width() > width - 1 {
            break;
        }
        result.push_str(grapheme);
        used += grapheme.width();
    }
    result.push('…');
    result
}

fn query_tail(text: &str, width: usize) -> String {
    let mut start = text.len();
    let mut used = 0;
    for (index, grapheme) in text.grapheme_indices(true).rev() {
        if used + grapheme.width() > width {
            break;
        }
        used += grapheme.width();
        start = index;
    }
    text[start..].into()
}

fn display_path(path: &str) -> String {
    let shortened = dirs::home_dir().and_then(|home| {
        Path::new(path).strip_prefix(home).ok().map(|relative| {
            if relative.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~{}{}", std::path::MAIN_SEPARATOR, relative.display())
            }
        })
    });
    single_line(shortened.as_deref().unwrap_or(path))
}

fn path_label(path: &str, width: usize) -> String {
    let path = display_path(path);
    if path.width() <= width || width < 5 {
        return clipped(&path, width);
    }
    let prefix = clipped(&path, width / 3);
    format!("{prefix}{}", query_tail(&path, width - prefix.width()))
}

fn row(area: Rect, index: u16) -> Rect {
    Rect::new(
        area.x,
        area.y.saturating_add(index),
        area.width,
        u16::from(index < area.height),
    )
}

fn count(list: &SessionList) -> String {
    if !list.loaded {
        return String::new();
    }
    format!(
        " {}{}",
        list.items.len(),
        if list.before.is_some() { "+" } else { "" }
    )
}

pub(super) fn draw_picker(
    frame: &mut Frame,
    lists: &[SessionList; 2],
    filtered: &[usize],
    state: &mut PickerState,
    workspace: &Path,
    loading: Option<Scope>,
) {
    let full = frame.area();
    let base = Style::default().fg(Palette::TEXT()).bg(Palette::BG());
    frame.render_widget(Block::default().style(base), full);
    if full.width < 24 || full.height < 8 {
        state.page_size = 0;
        let error = state
            .error
            .as_ref()
            .or(lists[state.scope.index()].error.as_ref());
        let message = error
            .map(|text| {
                format!(
                    "{}\nResize terminal; Esc starts a new session.",
                    single_line(text)
                )
            })
            .unwrap_or_else(|| {
                "Resize terminal to choose a session. Esc starts a new session.".into()
            });
        frame.render_widget(
            Paragraph::new(message)
                .style(Style::default().fg(if error.is_some() {
                    Palette::ERROR()
                } else {
                    Palette::DIM()
                }))
                .wrap(Wrap { trim: false }),
            full,
        );
        return;
    }
    let area = full.inner(Margin {
        horizontal: if full.width >= 60 {
            2
        } else {
            u16::from(full.width > 2)
        },
        vertical: u16::from(full.height >= 14),
    });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(if area.height >= 10 { 3 } else { 2 }),
            Constraint::Length(if area.height >= 12 { 3 } else { 1 }),
            Constraint::Min(0),
            Constraint::Length(if area.height >= 6 { 2 } else { 1 }),
        ])
        .split(area);
    let list = &lists[state.scope.index()];
    let accent = Style::default()
        .fg(Palette::ACCENT())
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Palette::DIM());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Resume session", accent),
            Span::styled(
                if !list.loaded && list.error.is_none() {
                    "   Loading…".into()
                } else if state.query.is_empty() {
                    format!(
                        "   {}{} {}",
                        list.items.len(),
                        if list.before.is_some() { "+" } else { "" },
                        if list.items.len() == 1 && list.before.is_none() {
                            "session"
                        } else {
                            "sessions"
                        }
                    )
                } else {
                    format!(
                        "   {} {}",
                        filtered.len(),
                        if filtered.len() == 1 {
                            "match"
                        } else {
                            "matches"
                        }
                    )
                },
                dim,
            ),
        ])),
        row(chunks[0], 0),
    );
    let tabs: Vec<_> = [Scope::Local, Scope::All]
        .into_iter()
        .flat_map(|scope| {
            let style = if state.scope == scope {
                accent.bg(Palette::SELECTED_BG())
            } else {
                dim
            };
            [
                Span::styled(
                    format!(" {}{} ", scope.label(), count(&lists[scope.index()])),
                    style,
                ),
                Span::raw("  "),
            ]
        })
        .collect();
    frame.render_widget(Paragraph::new(Line::from(tabs)), row(chunks[0], 1));
    let scope_text = match state.scope {
        Scope::Local => format!(
            "Current folder · {}",
            display_path(&workspace.to_string_lossy())
        ),
        Scope::All => "All workspaces in this database".into(),
    };
    frame.render_widget(
        Paragraph::new(clipped(&scope_text, area.width as usize)).style(dim),
        row(chunks[0], 2),
    );

    let search = chunks[1];
    let input = if search.height >= 3 {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Palette::RULE()))
            .title(Span::styled(" Search ", dim));
        let inner = block.inner(search).inner(Margin {
            horizontal: 1,
            vertical: 0,
        });
        frame.render_widget(block, search);
        inner
    } else {
        search
    };
    let visible_query = query_tail(&state.query, input.width.saturating_sub(1) as usize);
    let search_text = if state.query.is_empty() {
        Span::styled("Title, folder, or session ID…", dim)
    } else {
        Span::styled(visible_query.clone(), base)
    };
    frame.render_widget(Paragraph::new(Line::from(search_text)), input);
    if input.width > 0 && input.height > 0 {
        frame.set_cursor_position((input.x + visible_query.width() as u16, input.y));
    }

    let body = chunks[2];
    let wide = body.width >= 108;
    let (list_area, detail_area) = if wide {
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(body);
        let mut list = split[0];
        list.width = list.width.saturating_sub(2);
        (list, split[1])
    } else if body.height >= 10 {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(5)])
            .split(body);
        (split[0], split[1])
    } else {
        (body, Rect::default())
    };
    let row_height = if list_area.height >= 2 { 2 } else { 1 };
    let visible = (list_area.height as usize / row_height).max(1);
    state.page_size = visible;
    if state.selected < state.scroll {
        state.scroll = state.selected;
    } else if state.selected >= state.scroll + visible {
        state.scroll = state.selected + 1 - visible;
    }
    state.scroll = state.scroll.min(filtered.len().saturating_sub(visible));

    if filtered.is_empty() {
        let message = if list.error.is_some() {
            "Sessions could not be loaded."
        } else if loading == Some(state.scope) || list.needs_page(state, filtered.len()) {
            "Looking for sessions…"
        } else if !state.query.is_empty() {
            "No matching sessions. Try a title, folder, or ID."
        } else if state.scope == Scope::Local {
            "No sessions in this folder. Press Tab to browse All."
        } else {
            "No saved sessions in this database."
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(dim)
                .wrap(Wrap { trim: false }),
            list_area,
        );
    }
    for (position, &index) in filtered.iter().enumerate().skip(state.scroll).take(visible) {
        let summary = &list.items[index].summary;
        let selected = position == state.selected;
        let rect = Rect::new(
            list_area.x,
            list_area.y + ((position - state.scroll) * row_height) as u16,
            list_area.width,
            row_height as u16,
        );
        let style = if selected {
            base.bg(Palette::SELECTED_BG())
        } else {
            base
        };
        frame.render_widget(Block::default().style(style), rect);
        let width = rect.width.saturating_sub(2) as usize;
        let age = if width >= 32 {
            time_ago(summary.updated_at)
        } else {
            String::new()
        };
        let title_width = width.saturating_sub(age.width() + usize::from(!age.is_empty()) * 2);
        let title = clipped(&single_line(&summary.title), title_width);
        let gap = width.saturating_sub(title.width() + age.width());
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(if selected { "› " } else { "  " }, accent),
                Span::styled(
                    title,
                    if selected {
                        style.add_modifier(Modifier::BOLD)
                    } else {
                        style
                    },
                ),
                Span::raw(" ".repeat(gap)),
                Span::styled(age, dim),
            ]))
            .style(style),
            row(rect, 0),
        );
        if row_height == 2 {
            frame.render_widget(
                Paragraph::new(format!("  {}", path_label(&summary.workspace, width)))
                    .style(dim.bg(style.bg.unwrap_or(Palette::BG()))),
                row(rect, 1),
            );
        }
    }
    if let Some(&index) = filtered.get(state.selected) {
        details(frame, detail_area, &list.items[index].summary, wide);
    }
    let status = state
        .error
        .as_ref()
        .or(list.error.as_ref())
        .cloned()
        .unwrap_or_else(|| {
            if loading == Some(state.scope) {
                "Loading older sessions…".into()
            } else if list.before.is_some() {
                "More sessions load as you scroll; search also checks older sessions.".into()
            } else if filtered.is_empty() {
                String::new()
            } else {
                format!(
                    "{} of {} · Enter resumes in the selected working directory",
                    state.selected + 1,
                    filtered.len()
                )
            }
        });
    if chunks[3].height >= 2 {
        let style = if state.error.is_some() || list.error.is_some() {
            Style::default().fg(Palette::ERROR())
        } else {
            dim
        };
        frame.render_widget(
            Paragraph::new(clipped(&single_line(&status), area.width as usize)).style(style),
            row(chunks[3], 0),
        );
    }
    let hint = if area.width >= 64 {
        "↑/↓ move   Tab Local/All   Enter resume   Esc new session   Ctrl+R refresh"
    } else if area.width >= 42 {
        "↑/↓ move · Tab scope · Enter open · Esc new"
    } else {
        "Tab scope · Esc new"
    };
    frame.render_widget(
        Paragraph::new(hint).style(dim),
        row(chunks[3], chunks[3].height.saturating_sub(1)),
    );
}

fn details(frame: &mut Frame, area: Rect, summary: &SessionSummary, wide: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let dim = Style::default().fg(Palette::DIM());
    let block = Block::default()
        .borders(if wide { Borders::LEFT } else { Borders::TOP })
        .border_style(Style::default().fg(Palette::RULE()))
        .title(Span::styled(
            if wide { " Conversation " } else { " Preview " },
            dim,
        ));
    let mut inner = block.inner(area);
    if wide && inner.width > 0 {
        inner.x += 1;
        inner.width -= 1;
    }
    frame.render_widget(block, area);
    let mut lines = Vec::new();
    if wide {
        lines.push(Line::styled(
            single_line(&summary.title),
            Style::default()
                .fg(Palette::TEXT())
                .add_modifier(Modifier::BOLD),
        ));
        lines.push(Line::default());
        lines.push(Line::styled(display_path(&summary.workspace), dim));
        lines.push(Line::default());
    }
    lines.push(Line::styled(
        format!(
            "{} {} · {}",
            summary.message_count,
            if summary.message_count == 1 {
                "message"
            } else {
                "messages"
            },
            single_line(&summary.model)
        ),
        dim,
    ));
    if wide {
        lines.push(Line::styled(
            format!("ID {}", single_line(&summary.id.0)),
            dim,
        ));
        lines.push(Line::default());
        lines.push(Line::styled(
            "Recent messages",
            Style::default().fg(Palette::TEXT()),
        ));
    }
    if summary.human_previews.is_empty() {
        lines.push(Line::styled("No messages yet", dim));
    } else {
        lines.extend(
            summary
                .human_previews
                .iter()
                .map(|text| Line::styled(single_line(text), dim)),
        );
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
