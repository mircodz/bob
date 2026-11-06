use super::{
    composer,
    state::{App, Draft, Focus, Hit, HitAction, Pane},
    style::{bold, inset, prose_wrap, safe, text},
};
use bob_kernel::TodoStatus;
use bob_ui::{
    markdown::render_markdown,
    theme::Palette,
    widgets::{fit_text, BAND_INSET},
};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph},
    Frame,
};

struct Prompt {
    lines: Vec<Line<'static>>,
    title_height: usize,
    selected: Option<std::ops::Range<usize>>,
    cursor: Option<(usize, usize)>,
    scroll: usize,
    inspection_end: Option<usize>,
    action: HitAction,
}

fn hanging(line: Line<'static>, width: usize, indent: usize) -> Vec<Line<'static>> {
    let indent = indent.min(width.saturating_sub(2));
    let mut lines = Vec::new();
    // Wrap the body separately so continuation rows retain the production indent.
    let prefix_width = line
        .spans
        .iter()
        .take(line.spans.len().saturating_sub(1))
        .map(Span::width)
        .sum::<usize>();
    if prefix_width == indent && line.spans.len() > 1 {
        let body = Line::from(line.spans.last().unwrap().clone()).style(line.style);
        for (index, (_, mut row)) in prose_wrap(&body, width.saturating_sub(indent).max(1))
            .into_iter()
            .enumerate()
        {
            let prefix = if index == 0 {
                line.spans[..line.spans.len() - 1].to_vec()
            } else {
                vec![Span::raw(" ".repeat(indent))]
            };
            row.spans.splice(0..0, prefix);
            lines.push(row.style(line.style));
        }
    } else {
        lines.extend(prose_wrap(&line, width).into_iter().map(|(_, line)| line));
    }
    lines
}

impl Prompt {
    fn new(title: &str, detail: &str, width: usize, action: HitAction) -> Self {
        let mut lines = Vec::new();
        for (i, title) in safe(title).split('\n').enumerate() {
            lines.extend(hanging(
                Line::from(vec![
                    Span::styled(
                        if i == 0 { "● " } else { "  " },
                        text().fg(Palette::ACCENT()),
                    ),
                    Span::styled(title.to_owned(), bold()),
                ]),
                width,
                2,
            ));
        }
        let title_height = lines.len();
        lines.push(Line::default());
        if !detail.is_empty() {
            for line in render_markdown(&safe(detail)) {
                for (_, mut row) in prose_wrap(&line, width.saturating_sub(2).max(1)) {
                    row.spans.insert(0, Span::raw("  "));
                    lines.push(row);
                }
            }
            lines.push(Line::default());
        }
        Self {
            lines,
            title_height,
            selected: None,
            cursor: None,
            scroll: 0,
            inspection_end: None,
            action,
        }
    }

    fn option(
        &mut self,
        label: &str,
        index: usize,
        count: usize,
        selected: bool,
        other: bool,
        width: usize,
    ) {
        let start = self.lines.len();
        let style = if selected {
            bold().bg(Palette::SELECTED_BG())
        } else if other {
            text().fg(Palette::DIM())
        } else {
            text()
        };
        let num_width = count.to_string().len();
        let indent = num_width + 5;
        for (i, label) in safe(label).split('\n').enumerate() {
            let mut spans = if i == 0 {
                vec![
                    Span::styled(
                        format!(" {} ", if selected { "❯" } else { " " }),
                        text().fg(Palette::ACCENT()),
                    ),
                    Span::styled(
                        format!("{:>num_width$}. ", index + 1),
                        text().fg(if selected {
                            Palette::ACCENT()
                        } else {
                            Palette::DIM()
                        }),
                    ),
                ]
            } else {
                vec![Span::raw(" ".repeat(indent.min(width.saturating_sub(2))))]
            };
            spans.push(Span::styled(label.to_owned(), style));
            self.lines
                .extend(hanging(Line::from(spans).style(style), width, indent));
        }
        if selected {
            self.selected = Some(start..self.lines.len());
        }
    }

    fn editor(&mut self, draft: &Draft, active: bool, width: usize) {
        let mut copy = Draft::default();
        copy.text = draft.text.clone();
        copy.cursor = draft.cursor;
        let layout = composer::layout(&mut copy, width.saturating_sub(2).max(1));
        let start = self.lines.len();
        let style = text().bg(Palette::SELECTED_BG());
        for (i, row) in layout.rows.iter().enumerate() {
            let mut row = row.clone().style(style);
            row.spans.insert(
                0,
                Span::styled(
                    if i == 0 { "› " } else { "  " },
                    text().fg(Palette::ACCENT()),
                ),
            );
            self.lines.push(row);
        }
        self.selected = Some(start..self.lines.len());
        if active {
            self.cursor = Some((start + layout.cursor_row, layout.cursor_col + 2));
        }
        if draft.limit_reached {
            self.lines.push(Line::styled(
                fit_text(composer::LIMIT_NOTICE, width),
                text().fg(Palette::WARN()),
            ));
        }
    }

    fn hint(&mut self, hint: &str, width: usize) {
        self.lines.push(Line::styled(
            fit_text(hint, width),
            text().fg(Palette::DIM()),
        ));
    }

    fn visible(&self, height: usize) -> Vec<usize> {
        if self.lines.len() <= height {
            return (0..self.lines.len()).collect();
        }
        if height == 0 {
            return Vec::new();
        }
        let footer = self.lines.len() - 1;
        if let Some(end) = self.inspection_end {
            let target = self.cursor.map_or(end, |(row, _)| row);
            if height < 3 {
                let start = target.saturating_sub(height - 1).max(end);
                return (start..(start + height).min(self.lines.len())).collect();
            }
            let title_height = self.title_height.min(height - 2).max(1);
            let control_height = (footer - end).min((height - title_height - 1).max(1));
            let control_start = target
                .saturating_sub(control_height - 1)
                .max(end)
                .min(footer - control_height);
            let body_height = height - title_height - control_height - 1;
            let body_start = title_height
                + self
                    .scroll
                    .min(end.saturating_sub(title_height + body_height));
            return (0..title_height)
                .chain(body_start..(body_start + body_height).min(end))
                .chain(control_start..control_start + control_height)
                .chain(std::iter::once(footer))
                .collect();
        }
        let selection = self.selected.clone().unwrap_or(0..1);
        let target = self.cursor.map_or(selection.start, |(row, _)| row);
        if height < 3 {
            let start = if self.cursor.is_some() {
                target.saturating_sub(height - 1).max(selection.start)
            } else {
                target
            };
            return (start..(start + height).min(self.lines.len())).collect();
        }
        let selected_height = selection.len().min(height - 2);
        let title_height = self.title_height.min(height - selected_height - 1).max(1);
        let body_height = height - title_height - 1;
        let start = if self.cursor.is_some() {
            target.saturating_sub(body_height - 1)
        } else {
            selection
                .start
                .saturating_sub(body_height - selected_height)
        }
        .max(title_height)
        .min(footer.saturating_sub(body_height));
        (0..title_height)
            .chain(start..start + body_height)
            .chain(std::iter::once(footer))
            .collect()
    }
}

fn prompt(app: &App, width: usize) -> Option<Prompt> {
    let pane = app.pane()?;
    if let Some(question) = &pane.question {
        if app.focus != Focus::Plan || pane.plan.is_none() {
            let mut prompt = Prompt::new(
                &question.question.question,
                question.question.detail.as_deref().unwrap_or_default(),
                width,
                HitAction::Question,
            );
            let other = question.selected == Some(question.question.options.len());
            if other {
                prompt.editor(
                    &question.other,
                    app.focus == Focus::Question && !question.pending,
                    width,
                );
            } else {
                let count = question.question.options.len() + 1;
                for (index, label) in question.question.options.iter().enumerate() {
                    prompt.option(
                        label,
                        index,
                        count,
                        question.selected == Some(index),
                        false,
                        width,
                    );
                }
                prompt.option("Other…", count - 1, count, false, true, width);
            }
            prompt.hint(
                if question.pending {
                    "Sending answer…"
                } else if app.focus != Focus::Question {
                    "Tab or click to focus question · no answer sent"
                } else if other {
                    "Enter send · ↑↓ move · esc back"
                } else if question.selected.is_none() {
                    "↑↓ move · 1-9 select · choose an answer before Enter"
                } else {
                    "↑↓ move · 1-9 select · enter confirm · esc back"
                },
                width,
            );
            return Some(prompt);
        }
    }
    if let Some(plan) = &pane.plan {
        let mut prompt = Prompt::new(
            "Approve this plan?",
            &plan.plan.plan,
            width,
            HitAction::Plan,
        );
        prompt.inspection_end = Some(prompt.lines.len());
        prompt.option(
            "Yes, implement this plan",
            0,
            2,
            plan.choice == Some(true),
            false,
            width,
        );
        prompt.option(
            "Refine the plan",
            1,
            2,
            plan.choice == Some(false),
            false,
            width,
        );
        prompt.lines.push(Line::default());
        prompt.lines.push(Line::styled(
            "Refinement feedback",
            text().fg(Palette::DIM()),
        ));
        prompt.editor(
            &plan.feedback,
            app.focus == Focus::Plan && !plan.pending,
            width,
        );
        prompt.scroll = plan.scroll as usize;
        prompt.hint(
            if plan.pending {
                "Sending plan decision…"
            } else if app.focus != Focus::Plan {
                "Tab or click to focus plan · no decision sent"
            } else {
                "PgUp/PgDn inspect · ← approve · → refine · enter confirm · esc back"
            },
            width,
        );
        return Some(prompt);
    }
    if app.focus == Focus::ControlInvalidated {
        let mut prompt = Prompt::new("Prompt changed", "", width, HitAction::Composer);
        prompt.hint("Tab to focus the current prompt · esc back", width);
        return Some(prompt);
    }
    None
}

pub(super) fn prompt_height(app: &App, width: u16) -> u16 {
    if width <= BAND_INSET * 2 {
        return 0;
    }
    prompt(app, width.saturating_sub(BAND_INSET * 2) as usize).map_or(0, |prompt| {
        prompt.lines.len().saturating_add(1).min(24) as u16
    })
}

pub(super) fn draw_prompt(frame: &mut Frame, app: &mut App, area: Rect, hits: &mut Vec<Hit>) {
    let area = area.intersection(frame.area());
    let mut inner = inset(area, BAND_INSET);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let Some(prompt) = prompt(app, inner.width as usize) else {
        return;
    };
    frame.render_widget(Clear, area);
    frame.render_widget(Block::default().style(text().bg(Palette::BG())), area);
    let top_pad = u16::from(area.height > 2);
    inner.y += top_pad;
    inner.height = inner.height.saturating_sub(top_pad);
    let visible = prompt.visible(inner.height as usize);
    let lines: Vec<_> = visible
        .iter()
        .map(|&index| prompt.lines[index].clone())
        .collect();
    frame.render_widget(Paragraph::new(lines).style(text().bg(Palette::BG())), inner);
    if let Some((row, column)) = prompt.cursor {
        if let Some(y) = visible.iter().position(|&index| index == row) {
            frame.set_cursor_position((
                inner.x + (column as u16).min(inner.width - 1),
                inner.y + y as u16,
            ));
        }
    }
    hits.push(Hit {
        area,
        action: prompt.action,
    });
}

pub(super) fn draw_picker(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    composer: Rect,
    hits: &mut Vec<Hit>,
) {
    let Some(picker) = &app.picker else {
        return;
    };
    let area = area.intersection(frame.area());
    let bottom = composer.y.min(area.bottom()).max(area.y);
    let height = picker
        .items
        .len()
        .saturating_add(3)
        .min(24)
        .min(bottom.saturating_sub(area.y) as usize) as u16;
    if height == 0 || area.width == 0 {
        return;
    }
    let menu = Rect::new(area.x, bottom.saturating_sub(height), area.width, height);
    frame.render_widget(Clear, menu);
    frame.render_widget(Block::default().style(text().bg(Palette::POPUP_BG())), menu);
    let inner = inset(menu, BAND_INSET);
    if inner.width == 0 {
        return;
    }
    let top_pad = if height >= 4 {
        2
    } else {
        u16::from(height >= 3)
    };
    if top_pad > 0 {
        frame.render_widget(
            Paragraph::new(fit_text(&safe(&picker.title), inner.width as usize))
                .style(bold().bg(Palette::POPUP_BG())),
            Rect { height: 1, ..inner },
        );
    }
    let rows = height.saturating_sub(top_pad + u16::from(height > 1)) as usize;
    let start = bob_ui::widgets::keep_visible(0, picker.selected, rows, picker.items.len());
    let label_width = picker
        .items
        .iter()
        .map(|item| Line::raw(safe(&item.label)).width())
        .max()
        .unwrap_or(0)
        .min(inner.width.saturating_sub(3) as usize);
    for (row, (index, item)) in picker
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(rows)
        .enumerate()
    {
        let selected = index == picker.selected;
        let bg = if selected {
            Palette::SELECTED_BG()
        } else {
            Palette::POPUP_BG()
        };
        let label = fit_text(&safe(&item.label), label_width);
        let pad = label_width.saturating_sub(Line::raw(label.clone()).width());
        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", if selected { "❯" } else { " " }),
                text().fg(Palette::ACCENT()).bg(bg),
            ),
            Span::styled(
                label,
                if selected {
                    bold().fg(Palette::ACCENT()).bg(bg)
                } else {
                    text().bg(bg)
                },
            ),
            Span::styled(
                format!("{}  {}", " ".repeat(pad), safe(&item.description)),
                text().fg(Palette::FAINT()).bg(bg),
            ),
        ]);
        let rect = Rect::new(inner.x, inner.y + row as u16 + top_pad, inner.width, 1);
        frame.render_widget(Paragraph::new(line).style(text().bg(bg)), rect);
        hits.push(Hit {
            area: rect,
            action: HitAction::Picker(index),
        });
    }
    if height > 1 {
        frame.render_widget(
            Paragraph::new(fit_text(
                "↑↓ move · enter select · esc cancel",
                inner.width as usize,
            ))
            .style(text().fg(Palette::DIM()).bg(Palette::POPUP_BG())),
            Rect::new(inner.x, menu.bottom() - 1, inner.width, 1),
        );
    }
}

pub(super) fn todo_lines(pane: &Pane, width: u16) -> Vec<Line<'static>> {
    let Some(controls) = &pane.controls else {
        return Vec::new();
    };
    let items = &controls.todos;
    if items.is_empty() || width == 0 {
        return Vec::new();
    }
    let done = items
        .iter()
        .filter(|item| item.status == TodoStatus::Completed)
        .count();
    let active = items
        .iter()
        .filter(|item| item.status == TodoStatus::InProgress)
        .count();
    let header = format!(
        "{} task{} ({} done, {} in progress, {} open)",
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        done,
        active,
        items.len() - done - active
    );
    let mut lines = vec![
        Line::default(),
        Line::styled(fit_text(&header, width as usize), text().fg(Palette::DIM())),
    ];
    for item in items {
        let (glyph, color, style) = match item.status {
            TodoStatus::Pending => ("[ ]", Palette::FAINT(), text().fg(Palette::DIM())),
            TodoStatus::InProgress => ("[~]", Palette::ACCENT(), bold()),
            TodoStatus::Completed => ("[x]", Palette::OK(), text().fg(Palette::DIM())),
        };
        let label = if item.status == TodoStatus::InProgress {
            item.active_form.as_deref().unwrap_or(&item.content)
        } else {
            &item.content
        };
        lines.push(Line::from(vec![
            Span::styled(
                fit_text(&format!("{glyph} "), width as usize),
                Style::default().fg(color),
            ),
            Span::styled(
                fit_text(
                    &safe(label).replace('\n', " "),
                    width.saturating_sub(8) as usize,
                ),
                style,
            ),
        ]));
    }
    lines.push(Line::default());
    lines
}
