use super::theme::Palette;
use super::widgets::{inset, BAND_INSET};
use super::{indent_line, render, team, truncate_mid, App};
use bob_core::core::permissions::Mode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

/// Inner padding in display columns; aligns input with transcript user text.
const INPUT_PAD: u16 = 1;

const SIDEBAR_W: u16 = 52;

impl App {
    /// Shared wrapping for input height and rendering; continuation rows align with the prefix.
    fn input_lines(&self, width: usize, busy: bool) -> Vec<Line<'static>> {
        // Marker + continuation indent are both 2 cols, so content wraps at width-2.
        const PREFIX: usize = 2;
        let text_color = Style::default().fg(Palette::TEXT());
        let marker = || Span::styled("› ", Style::default().fg(Palette::DIM()));
        let indent = || Span::styled("  ", Style::default());

        if self.input.text().is_empty() && !busy {
            let placeholder = match &self.focused_agent {
                Some(_) => "send a message...  (esc → main)",
                None => "send a message...  (Ctrl+J or Shift+Enter for newline)",
            };
            return vec![Line::from(vec![
                marker(),
                Span::styled(placeholder, Style::default().fg(Palette::FAINT())),
            ])];
        }

        let content_width = width.saturating_sub(PREFIX).max(1);
        let (rows, _, _) = self.input.wrapped(content_width);
        rows.into_iter()
            .enumerate()
            .map(|(i, row)| {
                let prefix = if i == 0 { marker() } else { indent() };
                Line::from(vec![prefix, Span::styled(row, text_color)])
            })
            .collect()
    }

    pub(super) fn draw(&mut self, f: &mut ratatui::Frame) {
        self.draw_content(f);
        sanitize_buffer(f.buffer_mut());
    }

    fn draw_content(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        // Explicit background keeps theme contrast independent of terminal colors.
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        // Height calculation and rendering must use the same padded wrap width.
        let (content_area, sidebar_area) = if self.sidebar_open {
            let split = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(24), Constraint::Length(SIDEBAR_W)])
                .split(area);
            (split[0], Some(split[1]))
        } else {
            self.sidebar_rows = None;
            self.sidebar_rect = None;
            (area, None)
        };
        let content_w = content_area.width;
        let text_width = content_w.saturating_sub(BAND_INSET * 2 + INPUT_PAD * 2) as usize;
        let wrapped = self
            .input_lines(text_width, self.focused_activity().0)
            .len();
        let text_rows = (wrapped as u16).clamp(1, 12);
        let input_height = text_rows + 2;

        let prompt_height = if self.active_prompt() == super::prompts::ActivePrompt::Permission {
            // Match render width, plus top and bottom padding rows.
            let inner_w = content_w.saturating_sub(BAND_INSET * 2) as usize;
            (self.permission_lines(inner_w).len() as u16 + 2).min(24)
        } else if self.pending_query.is_some() {
            let inner_w = content_w.saturating_sub(BAND_INSET * 2) as usize;
            self.query_lines(inner_w).len().saturating_add(1).min(24) as u16
        } else {
            0
        };

        let job_rows = shell_jobs(&self.jobs);
        let jobs_height = if job_rows.is_empty() {
            0
        } else {
            (job_rows.len() as u16 + 1).min(8)
        };

        let todo_items = self.todos.as_ref().map(|t| t.items()).unwrap_or_default();
        let todos_height = if todo_items.is_empty() || !self.show_todos {
            0
        } else {
            // header + one blank line of padding above and below.
            (todo_items.len() as u16 + 3).min(14)
        };

        let queue_height = if self.queue.is_empty() {
            0
        } else {
            (self.queue.len() as u16 + 1).min(6)
        };

        let prompt_height = prompt_height.min(content_area.height.saturating_sub(4));
        let auxiliary_budget = content_area.height.saturating_sub(prompt_height + 5);
        let todos_height = todos_height.min(auxiliary_budget / 3);
        let jobs_height = jobs_height.min(auxiliary_budget / 3);
        let queue_height = queue_height.min(auxiliary_budget / 3);
        let input_height = input_height
            .min(
                content_area
                    .height
                    .saturating_sub(prompt_height + todos_height + jobs_height + queue_height + 2),
            )
            .max(1);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(prompt_height),
                Constraint::Length(todos_height),
                Constraint::Length(jobs_height),
                Constraint::Length(queue_height),
                Constraint::Length(input_height),
                Constraint::Length(1), // status bar below the input
            ])
            .split(content_area);

        self.draw_scrollback(f, chunks[0]);
        if let Some(sb) = sidebar_area {
            self.draw_sidebar(f, sb);
        }
        if !self.perm_queue.is_empty() {
            self.draw_permission(f, chunks[1]);
        } else if self.pending_query.is_some() {
            self.draw_query(f, chunks[1]);
        }
        if todos_height > 0 {
            self.draw_todos(f, chunks[2], &todo_items);
        }
        if jobs_height > 0 {
            self.draw_jobs(f, chunks[3], &job_rows);
        }
        if queue_height > 0 {
            self.draw_queue(f, chunks[4]);
        }
        let input_area = chunks[5];
        self.draw_input(f, input_area);
        self.draw_status_bar(f, chunks[6]);

        if self.active_prompt() == super::prompts::ActivePrompt::Composer && !self.menu.is_empty() {
            self.draw_menu(f, input_area);
        }
        if self.active_prompt() == super::prompts::ActivePrompt::Composer
            && !self.file_menu.is_empty()
        {
            self.draw_file_menu(f, input_area);
        }
        if self.workflow_view.is_some() {
            self.draw_workflow_view(f, area);
        }
    }

    fn draw_todos(
        &self,
        f: &mut ratatui::Frame,
        area: Rect,
        items: &[bob_core::tools::todo::TodoItem],
    ) {
        use bob_core::tools::todo::TodoStatus;
        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        let area = inset(area, BAND_INSET);
        let done = items
            .iter()
            .filter(|i| i.status == TodoStatus::Completed)
            .count();
        let in_progress = items
            .iter()
            .filter(|i| i.status == TodoStatus::InProgress)
            .count();
        let open = items.len() - done - in_progress;
        let header = format!(
            "{} task{} ({} done, {} in progress, {} open)",
            items.len(),
            if items.len() == 1 { "" } else { "s" },
            done,
            in_progress,
            open,
        );
        let mut lines: Vec<Line> = vec![
            Line::from(""),
            Line::from(Span::styled(header, Style::default().fg(Palette::DIM()))),
        ];
        for item in items.iter().take(area.height.saturating_sub(3) as usize) {
            let (glyph, glyph_color, text_style) = match item.status {
                TodoStatus::Pending => {
                    ("[ ]", Palette::FAINT(), Style::default().fg(Palette::DIM()))
                }
                TodoStatus::InProgress => (
                    "[~]",
                    Palette::ACCENT(),
                    Style::default()
                        .fg(Palette::TEXT())
                        .add_modifier(Modifier::BOLD),
                ),
                TodoStatus::Completed => {
                    ("[x]", Palette::OK(), Style::default().fg(Palette::DIM()))
                }
            };
            let width = area.width.saturating_sub(8) as usize;
            let text: String = if item.label().chars().count() > width {
                format!(
                    "{}…",
                    item.label()
                        .chars()
                        .take(width.saturating_sub(1))
                        .collect::<String>()
                )
            } else {
                item.label().to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", glyph), Style::default().fg(glyph_color)),
                Span::styled(text, text_style),
            ]));
        }
        lines.push(Line::from(""));
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            area,
        );
    }

    fn draw_jobs(
        &self,
        f: &mut ratatui::Frame,
        area: Rect,
        jobs: &[(String, String, String, bob_core::tools::jobs::JobStatus)],
    ) {
        use bob_core::tools::jobs::JobStatus;
        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        let area = inset(area, BAND_INSET);
        let running = jobs
            .iter()
            .filter(|(_, _, _, s)| *s == JobStatus::Running)
            .count();
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!("background shell commands · {} running", running),
            Style::default()
                .fg(Palette::ACCENT())
                .add_modifier(Modifier::BOLD),
        ))];
        for (id, kind, desc, status) in jobs.iter().take(area.height.saturating_sub(1) as usize) {
            let (glyph, color) = match status {
                JobStatus::Running => ("•", Palette::RUNNING()),
                JobStatus::Done => ("•", Palette::OK()),
                JobStatus::Failed => ("•", Palette::ERROR()),
                JobStatus::Cancelled => ("•", Palette::FAINT()),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", glyph), Style::default().fg(color)),
                Span::styled(format!("{} ", id), Style::default().fg(Palette::DIM())),
                Span::styled(
                    format!("[{}] ", kind),
                    Style::default().fg(Palette::FAINT()),
                ),
                Span::styled(
                    truncate_mid(desc, area.width as usize / 2),
                    Style::default().fg(Palette::TEXT()),
                ),
            ]));
        }
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            area,
        );
    }

    fn draw_queue(&self, f: &mut ratatui::Frame, area: Rect) {
        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        let area = inset(area, BAND_INSET);
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!("queued · {} · sent after this turn", self.queue.len()),
            Style::default()
                .fg(Palette::ACCENT())
                .add_modifier(Modifier::BOLD),
        ))];
        for msg in self
            .queue
            .iter()
            .take(area.height.saturating_sub(1) as usize)
        {
            lines.push(Line::from(vec![
                Span::styled("› ", Style::default().fg(Palette::DIM())),
                Span::styled(
                    truncate_mid(
                        &msg.replace('\n', " "),
                        area.width.saturating_sub(2) as usize,
                    ),
                    Style::default().fg(Palette::TEXT()),
                ),
            ]));
        }
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            area,
        );
    }
    fn draw_workflow_view(&mut self, f: &mut ratatui::Frame, area: Rect) {
        use super::view::WfStatus;
        let Some(vw) = self.workflow_view.as_ref() else {
            return;
        };
        let sel = vw.list.selected;
        let scroll = vw.list.scroll;
        let collapsed = vw.collapsed.clone();
        let Some((title, phases, done)) = self.view.workflow_by_id(&vw.run_id) else {
            self.workflow_view = None;
            self.wf_view_agents = None;
            return;
        };
        let title = title.to_string();

        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // blank + one-line header
                Constraint::Min(1),    // body
                Constraint::Length(1), // hint
            ])
            .split(area);

        let total_agents: usize = phases.iter().map(|p| p.agents.len()).sum();
        let done_agents: usize = phases
            .iter()
            .flat_map(|p| &p.agents)
            .filter(|a| a.status != WfStatus::Running)
            .count();
        let total_secs: u64 = phases
            .iter()
            .flat_map(|p| &p.agents)
            .filter_map(|a| a.duration_secs)
            .sum();
        let cancelled = self.cancel.load(std::sync::atomic::Ordering::Relaxed);
        let status = if cancelled {
            "cancelled"
        } else if done {
            "done"
        } else {
            "running"
        };
        f.render_widget(
            Paragraph::new(vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled(
                        format!("  {title}"),
                        Style::default()
                            .fg(Palette::TEXT())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "    {done_agents}/{total_agents} agents · {} · {status}",
                            super::fmt_duration(total_secs)
                        ),
                        Style::default().fg(Palette::DIM()),
                    ),
                ]),
            ])
            .style(Style::default().bg(Palette::BG())),
            outer[0],
        );

        let tree_area = inset(outer[1], 1);
        let tree_w = tree_area.width as usize;

        let rows = workflow_rows(phases, &collapsed);

        let mut lines: Vec<Line> = Vec::new();
        for (ri, row) in rows.iter().enumerate() {
            let is_sel = ri == sel;
            match row {
                WfRow::Phase(pi) => {
                    let p = &phases[*pi];
                    let pstatus = phase_status(&p.agents);
                    let done_n = p
                        .agents
                        .iter()
                        .filter(|a| a.status != WfStatus::Running)
                        .count();
                    let caret = if collapsed.contains(pi) { "▸" } else { "▾" };
                    let name_style = if is_sel {
                        Style::default()
                            .fg(Palette::TEXT())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Palette::TEXT())
                    };
                    // "  ▾ • " chrome = 6 cols; reserve the count on the right.
                    let count = format!("  {}/{}", done_n, p.agents.len());
                    let avail = tree_w.saturating_sub(6 + count.chars().count()).max(4);
                    let title = truncate_mid(&p.title, avail);
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {caret} "), Style::default().fg(Palette::DIM())),
                        render::wf_dot(pstatus),
                        Span::styled(format!(" {}", title), name_style),
                        Span::styled(count, Style::default().fg(Palette::DIM())),
                    ]));
                }
                WfRow::Agent(pi, ai) => {
                    let a = &phases[*pi].agents[*ai];
                    let is_last = *ai + 1 == phases[*pi].agents.len();
                    let branch = if is_last { "└─" } else { "├─" };
                    let label_style = if is_sel {
                        Style::default()
                            .fg(Palette::TEXT())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Palette::TEXT())
                    };
                    let dur = a
                        .duration_secs
                        .map(super::fmt_duration)
                        .unwrap_or_else(|| format!("{} tools", a.tools));
                    // "    ├─ • " occupies nine display columns.
                    const LEFT_CHROME: usize = 9;
                    let dur_w = dur.chars().count();
                    let avail_label = tree_w.saturating_sub(LEFT_CHROME + dur_w + 2).max(4);
                    let label = truncate_mid(&a.label, avail_label);
                    let used = LEFT_CHROME + label.chars().count() + dur_w;
                    let pad = tree_w.saturating_sub(used).max(1);
                    let dot = if cancelled && a.status == WfStatus::Running {
                        Span::styled("•".to_string(), Style::default().fg(Palette::FAINT()))
                    } else {
                        render::wf_dot(a.status)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("    {branch} "),
                            Style::default().fg(Palette::FAINT()),
                        ),
                        dot,
                        Span::styled(format!(" {}", label), label_style),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(dur, Style::default().fg(Palette::DIM())),
                    ]));
                }
            }
        }

        // Selectable rows and rendered lines map one-to-one.
        let view_h = tree_area.height as usize;
        let mut list = super::widgets::SelectList {
            selected: sel,
            scroll,
        };
        let range = list.window(rows.len(), view_h);
        let scroll = list.scroll;
        if let Some(v) = self.workflow_view.as_mut() {
            v.list.scroll = scroll;
        }

        let mut hit: Vec<(u16, String)> = Vec::new();
        for ri in range.clone() {
            let screen = tree_area.y + (ri - scroll) as u16;
            if let WfRow::Agent(pi, ai) = rows[ri] {
                hit.push((screen, phases[pi].agents[ai].agent_id.clone()));
            }
        }
        self.wf_view_agents = Some(hit);

        let visible: Vec<Line> = lines.into_iter().skip(scroll).collect();
        f.render_widget(
            Paragraph::new(visible).style(Style::default().bg(Palette::BG())),
            tree_area,
        );

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  ↑↓ move · enter toggle phase / open agent · esc close",
                Style::default().fg(Palette::FAINT()),
            )))
            .style(Style::default().bg(Palette::BG())),
            outer[2],
        );
    }

    fn draw_scrollback(&mut self, f: &mut ratatui::Frame, full: Rect) {
        if let Some(id) = self.focused_agent.clone() {
            self.draw_focused_agent(f, full, &id);
            return;
        }
        let (working, secs) = self.focused_activity();
        self.scrollback.render(
            f,
            full,
            &self.view.cells,
            self.view.revision,
            working,
            self.spinner,
            secs,
        );
    }

    fn draw_focused_agent(&mut self, f: &mut ratatui::Frame, full: Rect, id: &str) {
        let (cells, revision): (&[super::view::Cell], u64) = match self.teams.get(id) {
            Some(t) => (&t.cells, t.revision),
            None => (&[], 0),
        };
        let (working, secs) = self.focused_activity();
        self.focused_scrollback
            .set_interrupt_hint("^X stop · esc main");
        self.focused_scrollback
            .render(f, full, cells, revision, working, self.spinner, secs);
    }

    fn draw_sidebar(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let bg = Style::default().bg(Palette::INPUT_BG());
        f.render_widget(Clear, area);
        f.render_widget(Block::default().style(bg), area);
        let pane = inset(area, 1);
        let order = self.teams.display_order();
        self.sync_sidebar_selection(&order);
        let range = self
            .sidebar
            .window(order.len() + 1, pane.height.saturating_sub(4) as usize);
        let mut lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  AGENTS · {} running", self.teams.running_ids().len()),
                bg.fg(Palette::DIM()),
            )),
        ];
        let mut hit = Vec::new();
        for idx in range {
            let (id, label, depth, color, finished) = if idx == 0 {
                (
                    "",
                    "main",
                    0,
                    root_color(self.running || self.view.busy),
                    false,
                )
            } else {
                let id = &order[idx - 1];
                let t = self.teams.get(id).unwrap();
                let label = if t.task.trim().is_empty() {
                    t.display_label()
                } else {
                    &t.task
                };
                (
                    id.as_str(),
                    label,
                    self.teams.depth_of(id) + 1,
                    thread_color(t.status),
                    t.status != team::ThreadStatus::Running,
                )
            };
            hit.push((pane.y + lines.len() as u16, id.to_string()));
            lines.push(sidebar_row(
                label,
                depth,
                color,
                finished,
                idx == self.sidebar.selected,
                pane.width as usize,
            ));
        }
        self.sidebar_rows = Some(hit);
        self.sidebar_rect = Some(area);
        lines.push(Line::from(""));
        let notice = self
            .focused_agent
            .as_deref()
            .and_then(|id| self.control_notice(id));
        lines.push(Line::from(Span::styled(
            notice.unwrap_or("  ↑↓ select · ^X stop · esc main · ⌃t close"),
            bg.fg(if notice.is_some() {
                Palette::WARN()
            } else {
                Palette::FAINT()
            }),
        )));
        f.render_widget(Paragraph::new(lines).style(bg), pane);
    }

    fn draw_status_bar(&self, f: &mut ratatui::Frame, area: Rect) {
        const PAD: u16 = 3;
        let sep = || Span::styled("  ", Style::default().fg(Palette::FAINT()));
        let mut spans: Vec<Span> = Vec::new();

        // Safety mode and recipient take precedence over path decoration.
        let (mode_text, mode_color) = if self.permissions.mode() == Mode::Plan {
            ("plan", Palette::WARN())
        } else if self.permissions.bypass() {
            ("YOLO", Palette::ERROR())
        } else {
            match self.permissions.mode() {
                Mode::Normal => ("normal", Palette::DIM()),
                Mode::AutoAccept => ("auto-accept", Palette::OK()),
                Mode::Plan => ("plan", Palette::WARN()),
            }
        };
        spans.push(Span::styled(mode_text, Style::default().fg(mode_color)));
        if let Some(id) = self.focused_agent.as_deref() {
            spans.push(sep());
            spans.push(Span::styled(
                format!(
                    "to:{}",
                    super::widgets::fit_text(id, area.width.saturating_sub(20) as usize)
                ),
                Style::default().fg(Palette::ACCENT()),
            ));
        } else if let Some((used, window)) = self.context_usage {
            spans.push(sep());
            spans.push(Span::styled(
                format!("ctx {}%", used.saturating_mul(100) / window.max(1)),
                Style::default().fg(Palette::DIM()),
            ));
        }
        if !self.cwd_label.is_empty() {
            spans.push(sep());
            spans.push(Span::styled(
                self.cwd_label.clone(),
                Style::default().fg(Palette::ACCENT()),
            ));
        }
        if let Some(branch) = &self.branch {
            spans.push(sep());
            spans.push(Span::styled(
                format!("⎇ {branch}"),
                Style::default().fg(Palette::LINK()),
            ));
        }

        if let Some(lsp) = &self.lsp {
            for (name, health) in lsp.statuses() {
                use bob_core::lsp::Health;
                let (glyph, label, color) = match health {
                    Health::Starting => ("\u{25CB}".to_string(), name.clone(), Palette::DIM()),
                    Health::Indexing(Some(p)) => (
                        "\u{25D0}".to_string(),
                        format!("{name} {p}%"),
                        Palette::WARN(),
                    ),
                    Health::Indexing(None) => {
                        ("\u{25D0}".to_string(), name.clone(), Palette::WARN())
                    }
                    Health::Ready => ("\u{25CF}".to_string(), name.clone(), Palette::OK()),
                    Health::Failed(_) => ("\u{25CF}".to_string(), name.clone(), Palette::ERROR()),
                };
                spans.push(sep());
                spans.push(Span::styled(
                    format!("{glyph} {label}"),
                    Style::default().fg(color),
                ));
            }
        }

        let bar = Rect {
            x: area.x + PAD,
            y: area.y,
            width: area.width.saturating_sub(PAD * 2),
            height: 1,
        };

        let hint = Style::default().fg(Palette::FAINT());
        let mut hints: Vec<Span> = Vec::new();
        if !self.teams.is_empty() {
            hints.push(Span::styled("^T agents", hint));
            hints.push(Span::styled("  ", hint));
        }
        let todos_present = self
            .todos
            .as_ref()
            .map(|t| !t.items().is_empty())
            .unwrap_or(false);
        if todos_present {
            let label = if self.show_todos {
                "^L hide todos"
            } else {
                "^L show todos"
            };
            hints.push(Span::styled(label, hint));
        }
        let hint_line = Line::from(hints);
        let hints_width = hint_line.width();
        let reserve = if hints_width + 24 <= bar.width as usize {
            hints_width + 2
        } else {
            0
        };
        let left = Rect {
            width: bar.width.saturating_sub(reserve as u16),
            ..bar
        };
        f.render_widget(Paragraph::new(Line::from(spans)), left);
        if reserve > 0 {
            let right = Rect {
                x: left.right() + 2,
                width: hints_width as u16,
                ..bar
            };
            f.render_widget(Paragraph::new(hint_line), right);
        }
    }

    fn draw_input(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let area = inset(area, BAND_INSET);
        let bg = Block::default().style(Style::default().bg(Palette::INPUT_BG()));
        f.render_widget(bg, area);

        let busy = self.focused_activity().0;
        if area.width < 3 || area.height < 3 {
            return;
        }

        let text_rows = area.height.saturating_sub(2).max(1);
        let text_area = Rect {
            x: area.x + INPUT_PAD,
            y: area.y + 1,
            width: area.width.saturating_sub(INPUT_PAD * 2),
            height: text_rows,
        };

        let all = self.input_lines(text_area.width as usize, busy);
        let content_width = (text_area.width as usize).saturating_sub(2).max(1);
        let (_, cur_row, cur_col) = self.input.wrapped(content_width);
        self.input_scroll =
            super::widgets::keep_visible(self.input_scroll, cur_row, text_rows as usize, all.len());
        let mut visible: Vec<Line> = all
            .into_iter()
            .skip(self.input_scroll)
            .take(text_rows as usize)
            .collect();
        let ibg = Style::default().bg(Palette::INPUT_BG());
        for line in &mut visible {
            // Pad by display columns, not bytes, so wide glyphs fill the band correctly.
            let used = line.width();
            let pad = (text_area.width as usize).saturating_sub(used);
            if pad > 0 {
                line.spans.push(Span::styled(" ".repeat(pad), ibg));
            }
        }
        f.render_widget(Paragraph::new(visible).style(ibg), text_area);

        // Use the render wrap for cursor coordinates, then add the prefix and scroll offset.
        const PREFIX: u16 = 2;
        let vis_row = cur_row.saturating_sub(self.input_scroll) as u16;
        let cursor_x = text_area.x + PREFIX + cur_col as u16;
        if self.active_prompt() == super::prompts::ActivePrompt::Composer
            && vis_row < text_rows
            && cursor_x < text_area.x + text_area.width
        {
            f.set_cursor_position((cursor_x, text_area.y + vis_row));
        }
    }

    fn draw_menu(&mut self, f: &mut ratatui::Frame, input_area: Rect) {
        let h = self.menu.len() as u16;
        if h == 0 {
            return;
        }
        let width = input_area.width;
        let area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(h),
            width,
            height: h,
        };
        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::POPUP_BG())),
            area,
        );

        let lines: Vec<Line> = self
            .menu
            .iter()
            .enumerate()
            .map(|(i, (cmd, desc))| {
                let selected = i == self.menu_sel;
                let row_bg = if selected {
                    Palette::SELECTED_BG()
                } else {
                    Palette::POPUP_BG()
                };
                let marker = if selected { "❯" } else { " " };
                let cmd_style = if selected {
                    Style::default()
                        .fg(Palette::ACCENT())
                        .bg(row_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Palette::TEXT()).bg(row_bg)
                };
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", marker),
                        Style::default().fg(Palette::ACCENT()).bg(row_bg),
                    ),
                    Span::styled(format!("{:<8}", cmd), cmd_style),
                    Span::styled(
                        format!("  {}", desc),
                        Style::default().fg(Palette::FAINT()).bg(row_bg),
                    ),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::POPUP_BG())),
            area,
        );
    }

    fn draw_file_menu(&mut self, f: &mut ratatui::Frame, input_area: Rect) {
        let h = self.file_menu.len() as u16;
        if h == 0 {
            return;
        }
        let area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(h),
            width: input_area.width,
            height: h,
        };
        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::POPUP_BG())),
            area,
        );

        let lines: Vec<Line> = self
            .file_menu
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let selected = i == self.file_sel;
                let row_bg = if selected {
                    Palette::SELECTED_BG()
                } else {
                    Palette::POPUP_BG()
                };
                let marker = if selected { "❯" } else { " " };
                let path_style = if selected {
                    Style::default()
                        .fg(Palette::ACCENT())
                        .bg(row_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Palette::TEXT()).bg(row_bg)
                };
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", marker),
                        Style::default().fg(Palette::ACCENT()).bg(row_bg),
                    ),
                    Span::styled(path.clone(), path_style),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::POPUP_BG())),
            area,
        );
    }

    /// Shared by permission height calculation and rendering.
    fn permission_lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(p) = self.perm_queue.front() else {
            return vec![];
        };
        let mut lines: Vec<Line> = Vec::new();

        let mut title_spans = vec![Span::styled(
            p.title.clone(),
            Style::default()
                .fg(Palette::WARN())
                .add_modifier(Modifier::BOLD),
        )];
        if !p.detail.is_empty() {
            title_spans.push(Span::styled(
                format!(
                    "  {}",
                    truncate_mid(&p.detail, width.saturating_sub(p.title.len() + 2))
                ),
                Style::default().fg(Palette::DIM()),
            ));
        }
        lines.push(Line::from(title_spans));

        // Cap previews so large edits leave room for approval options.
        if let Some(preview) = &p.preview {
            let rendered = render::render_markdown_snippet(preview);
            let cap = 14usize;
            for l in rendered.iter().take(cap) {
                lines.push(indent_line(l.clone()));
            }
            if rendered.len() > cap {
                lines.push(Line::from(Span::styled(
                    format!("   ... {} more diff lines", rendered.len() - cap),
                    Style::default().fg(Palette::FAINT()),
                )));
            }
            lines.push(Line::from(""));
        }

        for (i, opt) in p.options.iter().enumerate() {
            let selected = i == p.list.selected;
            let marker = if selected { "❯" } else { " " };
            let base = if opt.allow {
                if opt.grant.is_some() {
                    Palette::OK()
                } else {
                    Palette::TEXT()
                }
            } else {
                Palette::ERROR()
            };
            let label_style = if selected {
                Style::default().fg(base).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(base)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", marker),
                    Style::default().fg(Palette::WARN()),
                ),
                Span::styled(format!("{}. ", i + 1), Style::default().fg(Palette::DIM())),
                Span::styled(opt.label.clone(), label_style),
            ]));
        }
        let mut hint = "↑↓ move · 1-9 pick · enter confirm · esc deny".to_string();
        if self.perm_queue.len() > 1 {
            hint.push_str(&format!("   ·   1 of {} pending", self.perm_queue.len()));
        }
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().fg(Palette::FAINT()),
        )));
        lines
    }

    fn draw_permission(&mut self, f: &mut ratatui::Frame, area: Rect) {
        if self.perm_queue.is_empty() {
            return;
        }
        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        let top_pad = u16::from(area.height > 2);
        let inner = Rect {
            x: area.x + BAND_INSET,
            y: area.y + top_pad,
            width: area.width.saturating_sub(BAND_INSET * 2),
            height: area.height.saturating_sub(top_pad),
        };
        let lines = super::widgets::prompt_window(
            self.permission_lines(inner.width as usize),
            inner.height as usize,
        );
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            inner,
        );
    }

    fn query_lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(q) = &self.pending_query else {
            return vec![];
        };
        if width == 0 {
            return vec![];
        }
        let mut lines: Vec<Line> = Vec::new();
        let title_style = Style::default()
            .fg(Palette::TEXT())
            .add_modifier(Modifier::BOLD);
        for (i, title) in q.query.title.split('\n').enumerate() {
            lines.extend(super::wrap_line_hanging(
                Line::from(vec![
                    Span::styled(
                        if i == 0 { "● " } else { "  " },
                        Style::default().fg(Palette::ACCENT()),
                    ),
                    Span::styled(title.to_string(), title_style),
                ]),
                width,
                2.min(width.saturating_sub(2)),
            ));
        }
        lines.push(Line::from(""));
        if !q.query.detail.is_empty() {
            for line in render::render_markdown_snippet(&q.query.detail) {
                lines.extend(super::wrap_line_hanging(
                    Line::from(
                        std::iter::once(Span::raw("  "))
                            .chain(line.spans)
                            .collect::<Vec<_>>(),
                    ),
                    width,
                    2.min(width.saturating_sub(2)),
                ));
            }
            lines.push(Line::from(""));
        }

        if let Some(buf) = &q.other_text {
            let style = Style::default()
                .fg(Palette::TEXT())
                .bg(Palette::SELECTED_BG());
            for (i, text) in format!("{buf}_").split('\n').enumerate() {
                lines.extend(
                    super::wrap_line_hanging(
                        Line::from(vec![
                            Span::styled(
                                if i == 0 { "› " } else { "  " },
                                Style::default().fg(Palette::ACCENT()),
                            ),
                            Span::styled(text.to_string(), style),
                        ]),
                        width,
                        2.min(width.saturating_sub(2)),
                    )
                    .into_iter()
                    .map(|line| line.style(style)),
                );
            }
            lines.push(Line::from(Span::styled(
                super::widgets::fit_text("PgUp/PgDn inspect · enter send · esc back", width),
                Style::default().fg(Palette::DIM()),
            )));
            return lines;
        }

        let n_opts = q.query.options.len();
        let total_rows = n_opts + if q.query.allow_other { 1 } else { 0 };
        let mut rows: Vec<(usize, String, bool)> = Vec::with_capacity(total_rows);
        for (i, opt) in q.query.options.iter().enumerate() {
            rows.push((i, opt.clone(), false));
        }
        if q.query.allow_other {
            rows.push((n_opts, "Other…".to_string(), true));
        }

        // Model rows encode "id\t<context label>"; align their columns separately.
        let split_rows: Vec<Option<(&str, &str)>> =
            rows.iter().map(|(_, l, _)| l.split_once('\t')).collect();
        let id_col_w = split_rows
            .iter()
            .filter_map(|s| s.map(|(id, _)| Span::raw(id).width()))
            .max()
            .unwrap_or(0);

        // Reserve the widest index so single- and double-digit rows align.
        let num_w = format!("{}", total_rows).chars().count();

        for (row_idx, (i, label, is_other)) in rows.iter().enumerate() {
            let selected = *i == q.list.selected;
            let marker = if selected { "❯" } else { " " };
            let base = if *is_other && !selected {
                Palette::DIM()
            } else {
                Palette::TEXT()
            };
            let style = if selected {
                Style::default()
                    .fg(base)
                    .bg(Palette::SELECTED_BG())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(base)
            };
            let mut spans = vec![
                Span::styled(
                    format!(" {} ", marker),
                    Style::default().fg(Palette::ACCENT()),
                ),
                Span::styled(
                    format!("{:>width$}. ", i + 1, width = num_w),
                    Style::default().fg(if selected {
                        Palette::ACCENT()
                    } else {
                        Palette::DIM()
                    }),
                ),
            ];
            if let Some((id, ctx)) = split_rows[row_idx] {
                let budget = width.saturating_sub(num_w + 5);
                let ctx_w = Span::raw(ctx).width();
                if budget > ctx_w + 8 {
                    let id_width = id_col_w.min(budget - ctx_w - 2);
                    let label = super::widgets::fit_text(id, id_width);
                    let id_pad = id_width.saturating_sub(Span::raw(&label).width());
                    spans.push(Span::styled(label, style));
                    spans.push(Span::raw(" ".repeat(id_pad + 2)));
                    spans.push(Span::styled(
                        ctx.to_string(),
                        Style::default().fg(Palette::ACCENT()),
                    ));
                } else {
                    spans.push(Span::styled(super::widgets::fit_text(id, budget), style));
                }
                lines.push(Line::from(spans).style(style));
            } else {
                for (line_idx, text) in label.split('\n').enumerate() {
                    let mut row = if line_idx == 0 {
                        spans.clone()
                    } else {
                        vec![Span::raw(
                            " ".repeat((num_w + 5).min(width.saturating_sub(2))),
                        )]
                    };
                    row.push(Span::styled(text.to_string(), style));
                    lines.extend(
                        super::wrap_line_hanging(
                            Line::from(row),
                            width,
                            (num_w + 5).min(width.saturating_sub(2)),
                        )
                        .into_iter()
                        .map(|line| line.style(style)),
                    );
                }
            }
        }
        lines.push(Line::from(Span::styled(
            super::widgets::fit_text(
                "PgUp/PgDn inspect · ↑↓ move · 1-9 pick · enter confirm · esc dismiss",
                width,
            ),
            Style::default().fg(Palette::DIM()),
        )));
        lines
    }

    fn draw_query(&mut self, f: &mut ratatui::Frame, area: Rect) {
        if self.pending_query.is_none() {
            return;
        }
        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        let mut inner = inset(area, BAND_INSET);
        let top_pad = u16::from(area.height > 2);
        inner.y += top_pad;
        inner.height = inner.height.saturating_sub(top_pad);
        let mut lines = self.query_lines(inner.width as usize);
        let height = inner.height as usize;
        let q = self.pending_query.as_mut().unwrap();
        let scroll = &mut q.inspection;
        scroll.page_size = height.saturating_sub(1).max(1);
        scroll.max_offset = lines.len().saturating_sub(1 + scroll.page_size);
        scroll.follow_start = 0;
        if let Some(offset) = scroll.offset.as_mut().filter(|_| lines.len() > height) {
            *offset = (*offset).min(scroll.max_offset);
            let hint = lines.last().cloned();
            lines = lines
                .into_iter()
                .skip(*offset)
                .take(scroll.page_size)
                .collect();
            if height > 1 {
                lines.extend(hint);
            }
        } else if lines.len() > height {
            let selected = lines
                .iter()
                .position(|line| line.style.bg == Some(Palette::SELECTED_BG()))
                .unwrap_or(0);
            let selected_end = lines
                .iter()
                .rposition(|line| line.style.bg == Some(Palette::SELECTED_BG()))
                .map_or(selected + 1, |i| i + 1);
            let free_text = q.other_text.is_some();
            if height < 3 {
                let start = if free_text {
                    selected_end.saturating_sub(height).max(selected)
                } else {
                    selected
                };
                scroll.follow_start = start.min(scroll.max_offset);
                scroll.page_size = height;
                lines = lines.into_iter().skip(start).take(height).collect();
            } else {
                let selected_height = (selected_end - selected).min(height - 2);
                let title_height = lines.iter().position(|line| line.width() == 0).unwrap_or(1);
                let title_height = title_height.min(height - selected_height - 1).max(1);
                let body_height = height - title_height - 1;
                let start = if free_text {
                    selected_end.saturating_sub(body_height)
                } else {
                    selected.saturating_sub(body_height - selected_height)
                }
                .max(title_height);
                scroll.follow_start = start.min(scroll.max_offset);
                scroll.page_size = body_height;
                let mut visible = lines[..title_height].to_vec();
                visible.extend(lines.iter().skip(start).take(body_height).cloned());
                visible.push(lines.last().unwrap().clone());
                lines = visible;
            }
        }
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            inner,
        );
    }
}

// Cell symbols are printed verbatim by the backend. Guard every pane, including
// labels and inputs that do not pass through the transcript renderer.
fn sanitize_buffer(buffer: &mut ratatui::buffer::Buffer) {
    for cell in &mut buffer.content {
        if cell.symbol().chars().any(char::is_control) {
            let clean: String = cell
                .symbol()
                .chars()
                .filter(|ch| !ch.is_control())
                .collect();
            cell.set_symbol(if clean.is_empty() { " " } else { &clean });
        }
    }
}

pub(super) fn shell_jobs(
    jobs: &bob_core::tools::jobs::JobRegistry,
) -> Vec<(String, String, String, bob_core::tools::jobs::JobStatus)> {
    jobs.list()
        .into_iter()
        .filter(|(_, kind, _, status)| {
            kind == "bash" && *status == bob_core::tools::jobs::JobStatus::Running
        })
        .collect()
}

fn thread_color(status: team::ThreadStatus) -> ratatui::style::Color {
    match status {
        team::ThreadStatus::Running => Palette::RUNNING(),
        team::ThreadStatus::Done => Palette::OK(),
        team::ThreadStatus::Failed => Palette::ERROR(),
        team::ThreadStatus::Cancelled => Palette::FAINT(),
    }
}

fn root_color(running: bool) -> ratatui::style::Color {
    if running {
        Palette::RUNNING()
    } else {
        Palette::FAINT()
    }
}

fn agent_name_style(finished: bool, selected: bool) -> Style {
    let mut style = if finished {
        Style::default()
            .fg(Palette::DIM())
            .add_modifier(Modifier::CROSSED_OUT)
    } else {
        Style::default().fg(Palette::TEXT())
    };
    if selected {
        style = style.add_modifier(Modifier::BOLD);
    }
    style
}

fn sidebar_row(
    label: &str,
    depth: usize,
    color: ratatui::style::Color,
    finished: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let bg = Style::default().bg(Palette::INPUT_BG());
    let depth = depth.min(width.saturating_sub(8) / 2);
    let lead = format!("  {}• ", "  ".repeat(depth));
    let budget = width.saturating_sub(lead.width());
    let clean = label.replace(['\n', '\r', '\t'], " ");
    let label = if clean.width() <= budget {
        clean
    } else {
        let mut short = String::new();
        let mut used = 0;
        for ch in clean.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > budget.saturating_sub(1) {
                break;
            }
            short.push(ch);
            used += w;
        }
        if budget > 0 {
            short.push('…');
        }
        short
    };
    let pad = width.saturating_sub(lead.width() + label.width());
    Line::from(vec![
        Span::styled(lead, bg.fg(color)),
        Span::styled(
            label,
            agent_name_style(finished, selected).bg(Palette::INPUT_BG()),
        ),
        Span::styled(" ".repeat(pad), bg),
    ])
}

/// Row indices shared by rendering and key/click handlers.
#[derive(Clone, Copy)]
pub(super) enum WfRow {
    Phase(usize),
    Agent(usize, usize),
}

pub(super) fn workflow_rows(
    phases: &[super::view::WfPhase],
    collapsed: &std::collections::HashSet<usize>,
) -> Vec<WfRow> {
    let mut rows = Vec::new();
    for (pi, p) in phases.iter().enumerate() {
        rows.push(WfRow::Phase(pi));
        if !collapsed.contains(&pi) {
            for ai in 0..p.agents.len() {
                rows.push(WfRow::Agent(pi, ai));
            }
        }
    }
    rows
}

fn phase_status(agents: &[super::view::WfAgent]) -> super::view::WfStatus {
    use super::view::WfStatus;
    if agents.iter().any(|a| a.status == WfStatus::Failed) {
        WfStatus::Failed
    } else if agents.is_empty() || agents.iter().any(|a| a.status == WfStatus::Running) {
        WfStatus::Running
    } else {
        WfStatus::Done
    }
}

#[cfg(test)]
mod tests {
    use super::super::{KeyCode, KeyModifiers};
    use super::*;
    use crate::tui::test_support::test_app;
    use bob_core::tools::jobs::{JobRegistry, JobStatus};
    use ratatui::{backend::TestBackend, Terminal};

    fn visible_buffer(buffer: &ratatui::buffer::Buffer) -> ratatui::buffer::Buffer {
        let mut visible = buffer.clone();
        for y in buffer.area.y..buffer.area.bottom() {
            let mut x = buffer.area.x;
            while x < buffer.area.right() {
                let width = Span::raw(buffer[(x, y)].symbol()).width().max(1) as u16;
                // Ratatui skips hidden wide-glyph continuation slots when diffing;
                // TestBackend retains arbitrary old values there, unlike a real screen.
                for hidden in x + 1..x.saturating_add(width).min(buffer.area.right()) {
                    visible[(hidden, y)].reset();
                }
                x = x.saturating_add(width);
            }
        }
        visible
    }

    fn assert_matches_fresh_frame(app: &mut App, terminal: &Terminal<TestBackend>) {
        let actual = terminal.backend().buffer();
        let mut fresh =
            Terminal::new(TestBackend::new(actual.area.width, actual.area.height)).unwrap();
        fresh.draw(|f| app.draw(f)).unwrap();
        assert_eq!(
            visible_buffer(actual),
            visible_buffer(fresh.backend().buffer()),
            "incremental frame must match a fresh frame, including styles"
        );
    }

    fn mouse(
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
    ) -> super::super::CtEvent {
        super::super::CtEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn sidebar_uses_52_columns_when_space_allows() {
        let mut app = test_app();
        app.sidebar_open = true;
        for width in [120, 100, 76] {
            let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(app.sidebar_rect.unwrap().width, 52);
            assert!(width - app.sidebar_rect.unwrap().width >= 24);
        }
        let mut terminal = Terminal::new(TestBackend::new(60, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(60 - app.sidebar_rect.unwrap().width >= 24);
    }

    #[test]
    fn frames_repaint_scrolling_sidebar_toggles_and_resize_without_stale_colors() {
        use super::super::CtEvent;
        use crossterm::event::{KeyEvent, MouseEventKind};
        let mut app = test_app();
        for i in 0..40 {
            app.view.push_user(format!("message {i} 界"));
            app.view.push_event(format!("result {i}"));
        }
        app.teams.on_spawn("reviewer", "root", "Review parser", "");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let bottom = terminal.backend().buffer().clone();
        let mut dirty = false;
        app.on_terminal_event(mouse(MouseEventKind::ScrollUp, 10, 5), &mut dirty);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_ne!(&bottom, terminal.backend().buffer());
        assert_matches_fresh_frame(&mut app, &terminal);

        for open in [true, false] {
            app.on_terminal_event(
                CtEvent::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
                &mut dirty,
            );
            terminal.draw(|f| app.draw(f)).unwrap();
            assert_eq!(
                terminal.backend().buffer()[(99, 0)].bg,
                if open {
                    Palette::INPUT_BG()
                } else {
                    Palette::BG()
                }
            );
            assert_matches_fresh_frame(&mut app, &terminal);
        }
        for (width, height) in [(70, 18), (120, 35), (45, 12), (100, 30)] {
            terminal.backend_mut().resize(width, height);
            app.on_terminal_event(CtEvent::Resize(width, height), &mut dirty);
            terminal.draw(|f| app.draw(f)).unwrap();
            assert_eq!(
                terminal.backend().buffer().area,
                Rect::new(0, 0, width, height)
            );
            assert_matches_fresh_frame(&mut app, &terminal);
        }
        app.active_scrollback().stick_to_bottom();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(
            visible_buffer(&bottom),
            visible_buffer(terminal.backend().buffer())
        );
    }

    #[test]
    fn final_buffer_guard_removes_controls_without_changing_styles_or_unicode() {
        let mut buffer = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 3, 1));
        let style = Style::default()
            .fg(Palette::ERROR())
            .bg(Palette::INPUT_BG());
        buffer[(0, 0)]
            .set_symbol("e\u{301}\x1b\u{9b}\n")
            .set_style(style);
        buffer[(1, 0)].set_symbol("界").set_style(style);
        buffer[(2, 0)].set_symbol("\x07\t");
        sanitize_buffer(&mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "e\u{301}");
        assert_eq!(buffer[(1, 0)].symbol(), "界");
        assert_eq!(buffer[(2, 0)].symbol(), " ");
        assert_eq!(buffer[(0, 0)].fg, Palette::ERROR());
        assert_eq!(buffer[(0, 0)].bg, Palette::INPUT_BG());
    }

    #[test]
    fn focused_agent_skips_the_hidden_main_transcript() {
        let mut app = test_app();
        for i in 0..100 {
            app.view.push_notice(format!("root transcript row {i}"));
        }
        app.teams
            .on_spawn("reviewer", "root", "Review parser", "Start here");
        app.teams.on_done("reviewer", false);
        app.open_sidebar_on("reviewer");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(
            app.scrollback.hit_test_offset(5).is_none(),
            "covered root transcript must not be prepared"
        );
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("Start here"));
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.scrollback.hit_test_offset(5).is_some());
        app.open_sidebar_on("reviewer");
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let main_row = app
            .sidebar_rows
            .as_ref()
            .unwrap()
            .iter()
            .find(|(_, id)| id.is_empty())
            .unwrap()
            .0;
        app.click_sidebar(app.sidebar_rect.unwrap().x + 2, main_row);
        assert!(app.focused_agent.is_none());
        assert_eq!(app.sidebar.selected, 0);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert_matches_fresh_frame(&mut app, &terminal);
    }

    #[test]
    fn workflow_agent_click_and_enter_focus_the_main_transcript() {
        use bob_core::core::events::AgentEvent;
        let mut app = test_app();
        app.apply_agent_event(&AgentEvent::WorkflowPhase {
            workflow_id: "wf-review".into(),
            title: "Review".into(),
            index: 0,
            total: 1,
        });
        app.apply_agent_event(&AgentEvent::SubagentSpawn {
            agent_id: "reviewer".into(),
            parent_id: "wf-review".into(),
            task: "Review code".into(),
            prompt: "Inspect this code".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let agent_row = (0..30)
            .find(|row| {
                app.scrollback
                    .hit_test_offset(*row)
                    .is_some_and(|(idx, offset)| {
                        app.view.cells[idx].workflow_agent_at(offset) == Some("reviewer")
                    })
            })
            .expect("workflow agent row must be clickable");
        assert!(app.click_scrollback(5, agent_row));
        assert!(app.sidebar_open);
        assert_eq!(app.focused_agent.as_deref(), Some("reviewer"));
        assert_eq!(app.sidebar.selected, 1);
        assert_eq!(app.teams.get("reviewer").unwrap().unread, 0);
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        app.open_workflow_view("wf-review".into());
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.workflow_view.as_ref().unwrap().collapsed.contains(&0));
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.workflow_view.as_ref().unwrap().collapsed.is_empty());
        app.on_key(KeyCode::Down, KeyModifiers::NONE);
        assert!(app.focused_agent.is_none());
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.workflow_view.is_none());
        assert_eq!(app.focused_agent.as_deref(), Some("reviewer"));
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        app.open_workflow_view("wf-review".into());
        terminal.draw(|f| app.draw(f)).unwrap();
        let row = app.wf_view_agents.as_ref().unwrap()[0].0;
        app.click_workflow_view(5, row);
        assert!(app.workflow_view.is_none());
        assert!(app.wf_view_agents.is_none());
        assert_eq!(app.focused_agent.as_deref(), Some("reviewer"));
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_matches_fresh_frame(&mut app, &terminal);
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("Inspect this code"));
    }

    #[test]
    fn shell_command_click_expands_and_reflows_in_the_conversation() {
        use bob_core::core::events::AgentEvent;
        let mut app = test_app();
        let command = format!(
            "printf '%s' '{}'\nprintf command_end",
            "abcdefgh".repeat(45)
        );
        app.apply_agent_event(&AgentEvent::ToolCall {
            agent_id: "root".into(),
            tool_use_id: "shell".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": command}),
        });
        app.apply_agent_event(&AgentEvent::ToolResult {
            agent_id: "root".into(),
            tool_use_id: "shell".into(),
            output: bob_core::core::types::ShellOutput {
                exit_code: Some(7),
                ..Default::default()
            }
            .into(),
            is_error: true,
        });
        let mut terminal = Terminal::new(TestBackend::new(56, 40)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let row = (0..40)
            .find(|row| {
                let line: String = (0..56)
                    .map(|x| terminal.backend().buffer()[(x, *row)].symbol())
                    .collect();
                line.contains("click to expand")
            })
            .expect("collapsed command offers expansion");
        let owner = app.scrollback.hit_test_offset(row).unwrap().0;
        assert!(app.click_scrollback(5, row));
        assert!(matches!(
            &app.view.cells[owner],
            super::super::view::Cell::Tool { expanded: true, .. }
        ));
        for width in [56, 40, 80] {
            terminal.backend_mut().resize(width, 40);
            terminal.draw(|f| app.draw(f)).unwrap();
            let buffer = terminal.backend().buffer();
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("printf command_end"), "{width}: {text}");
            assert!(text.contains("exit 7"));
            assert_matches_fresh_frame(&mut app, &terminal);
        }
        let row = (0..40)
            .find(|row| {
                app.scrollback
                    .hit_test_offset(*row)
                    .is_some_and(|(idx, _)| idx == owner)
            })
            .unwrap();
        assert!(app.click_scrollback(5, row));
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(
            matches!(&app.view.cells[owner], super::super::view::Cell::Tool { expanded: false, input, .. } if input["command"] == command)
        );
        assert_matches_fresh_frame(&mut app, &terminal);
    }

    fn frame_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect()
            })
            .collect()
    }

    fn unexpanded_tool_data(cells: &[super::super::view::Cell]) -> Vec<serde_json::Value> {
        cells
            .iter()
            .filter_map(|cell| match cell {
                super::super::view::Cell::Tool {
                    id,
                    name,
                    input,
                    status,
                    output,
                    expanded,
                } => {
                    assert!(!*expanded, "group clicks must not expand stored tool {id}");
                    Some(serde_json::json!({
                        "id": id,
                        "name": name,
                        "input": input,
                        "status": format!("{status:?}"),
                        "output": output,
                    }))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn exploration_group_clicks_stay_display_only_across_sidebar_resize() {
        use bob_core::core::events::AgentEvent;
        use serde_json::json;
        let mut app = test_app();
        app.view.push_user("Inspect the parser".into());
        for (index, (name, input)) in [
            ("read_file", json!({"path": "src/parser.rs"})),
            ("read_file", json!({"path": "src/parser.rs", "offset": 40})),
            ("read_file", json!({"path": "src/token.rs"})),
            ("grep", json!({"pattern": "parse", "path": "src"})),
            ("glob", json!({"pattern": "**/*.rs"})),
            ("list_dir", json!({"path": "src"})),
            ("list_dir", json!({"path": "src"})),
        ]
        .into_iter()
        .enumerate()
        {
            let tool_use_id = format!("inspect-{index}");
            app.apply_agent_event(&AgentEvent::ToolCall {
                agent_id: "root".into(),
                tool_use_id: tool_use_id.clone(),
                name: name.into(),
                input,
            });
            app.apply_agent_event(&AgentEvent::ToolResult {
                agent_id: "root".into(),
                tool_use_id,
                output: "successful exploration body\n".repeat(12).into(),
                is_error: false,
            });
        }
        let revision = app.view.revision;
        let stored = unexpanded_tool_data(&app.view.cells);
        assert_eq!(stored.len(), 7);
        let summary = "Explored 2 files · 2 searches · 1 directory";
        let headers = [
            "Read src/parser.rs",
            "Read src/parser.rs",
            "Read src/token.rs",
            "Grep parse",
            "Glob **/*.rs",
            "List src",
            "List src",
        ];
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let rows = frame_rows(&terminal);
        let row = rows.iter().position(|line| line.contains(summary)).unwrap();
        assert!(!rows.iter().any(|line| line.contains(headers[0])));
        assert!(app.click_scrollback(5, row as u16));
        assert_eq!(app.view.revision, revision);
        assert_eq!(unexpanded_tool_data(&app.view.cells), stored);

        for (width, sidebar_open) in [(140, false), (124, true), (104, false)] {
            app.sidebar_open = sidebar_open;
            terminal.backend_mut().resize(width, 36);
            terminal.draw(|f| app.draw(f)).unwrap();
            let rows = frame_rows(&terminal);
            let summary_row = rows.iter().position(|line| line.contains(summary)).unwrap();
            for (offset, header) in headers.iter().enumerate() {
                let row = summary_row + offset + 1;
                assert!(
                    rows[row].contains(*header),
                    "expected {header}: {}",
                    rows[row]
                );
                assert_eq!(
                    rows[row].find('•').unwrap(),
                    rows[summary_row].find('•').unwrap() + 2,
                );
                assert!(app.click_scrollback(5, row as u16));
            }
            assert!(!rows
                .iter()
                .any(|line| line.contains("successful exploration body")));
            assert_eq!(app.view.revision, revision);
            assert_eq!(unexpanded_tool_data(&app.view.cells), stored);
            assert_matches_fresh_frame(&mut app, &terminal);
        }

        let rows = frame_rows(&terminal);
        let row = rows.iter().position(|line| line.contains(summary)).unwrap();
        assert!(app.click_scrollback(5, row as u16));
        terminal.draw(|f| app.draw(f)).unwrap();
        let rows = frame_rows(&terminal);
        assert!(rows.iter().any(|line| line.contains(summary)));
        assert!(!rows.iter().any(|line| line.contains(headers[0])));
        assert_eq!(app.view.revision, revision);
        assert_eq!(unexpanded_tool_data(&app.view.cells), stored);
        assert_matches_fresh_frame(&mut app, &terminal);
    }

    #[test]
    fn focused_exploration_keeps_errors_and_running_calls_visible_when_collapsed() {
        use bob_core::core::events::AgentEvent;
        use serde_json::json;
        let mut app = test_app();
        app.view.push_user("Main transcript stays unchanged".into());
        let root_revision = app.view.revision;
        app.teams.on_spawn("reviewer", "root", "Review parser", "");
        for (id, name, input, result) in [
            (
                "read",
                "read_file",
                json!({"path": "src/parser.rs"}),
                Some(("successful agent body", false)),
            ),
            (
                "grep",
                "grep",
                json!({"pattern": "parse", "path": "missing"}),
                Some(("missing: permission denied", true)),
            ),
            ("list", "list_dir", json!({"path": "src"}), None),
        ] {
            app.teams.apply(
                &AgentEvent::ToolCall {
                    agent_id: "reviewer".into(),
                    tool_use_id: id.into(),
                    name: name.into(),
                    input,
                },
                None,
            );
            if let Some((output, is_error)) = result {
                app.teams.apply(
                    &AgentEvent::ToolResult {
                        agent_id: "reviewer".into(),
                        tool_use_id: id.into(),
                        output: output.into(),
                        is_error,
                    },
                    None,
                );
            }
        }
        let revision = app.teams.get("reviewer").unwrap().revision;
        let stored = unexpanded_tool_data(&app.teams.get("reviewer").unwrap().cells);
        assert_eq!(stored.len(), 3);
        assert!(app.open_sidebar_on("reviewer"));
        let summary = "Exploring 1 file · 1 search · 1 directory";
        let mut terminal = Terminal::new(TestBackend::new(140, 36)).unwrap();
        for (width, expanded) in [(140, false), (124, true), (132, false)] {
            terminal.backend_mut().resize(width, 36);
            terminal.draw(|f| app.draw(f)).unwrap();
            let rows = frame_rows(&terminal);
            let summary_row = rows.iter().position(|line| line.contains(summary)).unwrap();
            assert_eq!(
                rows.iter().any(|line| line.contains("Read src/parser.rs")),
                expanded
            );
            assert!(rows
                .iter()
                .any(|line| line.contains("missing: permission denied")));
            assert!(!rows
                .iter()
                .any(|line| line.contains("successful agent body")));
            if expanded {
                assert!(rows[summary_row + 1].contains("Read src/parser.rs"));
                assert!(rows[summary_row + 2].contains("Grep parse"));
            }
            for header in ["Grep parse", "List src"] {
                let row = rows.iter().position(|line| line.contains(header)).unwrap();
                assert!(app.click_scrollback(5, row as u16));
            }
            assert_eq!(app.teams.get("reviewer").unwrap().revision, revision);
            assert_eq!(
                unexpanded_tool_data(&app.teams.get("reviewer").unwrap().cells),
                stored
            );
            assert_eq!(app.view.revision, root_revision);
            assert_matches_fresh_frame(&mut app, &terminal);
            assert!(app.click_scrollback(5, summary_row as u16));
            assert_eq!(app.teams.get("reviewer").unwrap().revision, revision);
            assert_eq!(
                unexpanded_tool_data(&app.teams.get("reviewer").unwrap().cells),
                stored
            );
            assert_eq!(app.view.revision, root_revision);
        }
    }

    #[test]
    fn exploration_child_mouse_click_preserves_scrolled_viewport() {
        use bob_core::core::events::AgentEvent;
        use crossterm::event::{MouseButton, MouseEventKind};
        for agent_id in ["root", "reviewer"] {
            let mut app = test_app();
            if agent_id != "root" {
                app.teams.on_spawn(agent_id, "root", "Review files", "");
            }
            for index in 0..60 {
                for event in [
                    AgentEvent::ToolCall {
                        agent_id: agent_id.into(),
                        tool_use_id: index.to_string(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": format!("src/file_{index}.rs")}),
                    },
                    AgentEvent::ToolResult {
                        agent_id: agent_id.into(),
                        tool_use_id: index.to_string(),
                        output: "hidden content".into(),
                        is_error: false,
                    },
                ] {
                    app.apply_agent_event(&event);
                }
            }
            if agent_id != "root" {
                app.open_sidebar_on(agent_id);
            }
            let mut terminal = Terminal::new(TestBackend::new(120, 25)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let summary = frame_rows(&terminal)
                .iter()
                .position(|row| row.contains("Explored 60 files"))
                .unwrap();
            assert!(app.click_scrollback(5, summary as u16));
            terminal.draw(|f| app.draw(f)).unwrap();
            app.active_scrollback().scroll_down(10);
            terminal.draw(|f| app.draw(f)).unwrap();
            assert!(!app.active_scrollback().at_bottom());
            let row = frame_rows(&terminal)
                .iter()
                .position(|row| row.contains("Read src/file_"))
                .unwrap() as u16;
            let owner = app.active_scrollback().hit_test_offset(row);
            let before = terminal.backend().buffer().clone();
            let mut dirty = false;
            app.on_terminal_event(
                mouse(MouseEventKind::Down(MouseButton::Left), 5, row),
                &mut dirty,
            );
            terminal.draw(|f| app.draw(f)).unwrap();
            assert!(dirty);
            assert!(!app.active_scrollback().at_bottom());
            assert_eq!(app.active_scrollback().hit_test_offset(row), owner);
            assert_eq!(terminal.backend().buffer(), &before);
        }
    }

    #[test]
    fn focused_tool_click_uses_its_renderer_and_bumps_only_its_revision() {
        use bob_core::core::events::AgentEvent;
        let mut app = test_app();
        app.view.push_user("root message".into());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let root_revision = app.view.revision;
        app.teams.on_spawn("reviewer", "root", "Review code", "");
        app.teams.apply(
            &AgentEvent::ToolCall {
                agent_id: "reviewer".into(),
                tool_use_id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hello"}),
            },
            None,
        );
        app.teams.apply(
            &AgentEvent::ToolResult {
                agent_id: "reviewer".into(),
                tool_use_id: "t1".into(),
                output: "hello\n".repeat(20).into(),
                is_error: false,
            },
            None,
        );
        app.teams.on_done("reviewer", false);
        app.open_sidebar_on("reviewer");
        terminal.draw(|f| app.draw(f)).unwrap();
        let revision = app.teams.get("reviewer").unwrap().revision;
        for expanded in [true, false] {
            let row = (0..30)
                .find(|row| {
                    app.focused_scrollback
                        .hit_test_offset(*row)
                        .is_some_and(|(idx, _)| idx == 0)
                })
                .unwrap();
            assert!(app.click_scrollback(5, row));
            assert!(matches!(&app.teams.get("reviewer").unwrap().cells[0],
            super::super::view::Cell::Tool { expanded: value, .. } if *value == expanded));
            terminal.draw(|f| app.draw(f)).unwrap();
            assert_matches_fresh_frame(&mut app, &terminal);
        }
        assert_eq!(app.teams.get("reviewer").unwrap().revision, revision + 2);
        assert_eq!(app.view.revision, root_revision);
        assert!(!app.teams.toggle_tool("missing", 0));
        assert!(!app.teams.toggle_tool("reviewer", 99));
        assert_eq!(app.teams.get("reviewer").unwrap().revision, revision + 2);
    }

    #[test]
    fn focused_scroll_position_survives_agent_reordering() {
        use bob_core::core::events::AgentEvent;
        let mut app = test_app();
        app.teams.on_spawn("a", "root", "Review a", "");
        app.teams.on_spawn("b", "root", "Review b", "");
        for i in 0..40 {
            app.teams
                .push_message("a", "user", &format!("message {i}"), None);
        }
        app.open_sidebar_on("a");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        app.focused_scrollback.scroll_up(10);
        terminal.draw(|f| app.draw(f)).unwrap();
        let anchor = app.focused_scrollback.hit_test_offset(5);
        assert!(anchor.is_some());
        assert!(!app.focused_scrollback.at_bottom());
        app.apply_agent_event(&AgentEvent::SubagentDone {
            agent_id: "a".into(),
            failed: false,
            cancelled: false,
        });
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.focused_agent.as_deref(), Some("a"));
        assert_eq!(app.sidebar.selected, 2);
        assert_eq!(app.focused_scrollback.hit_test_offset(5), anchor);
        assert!(!app.focused_scrollback.at_bottom());
        assert_matches_fresh_frame(&mut app, &terminal);
    }

    #[test]
    fn jobs_panel_shows_only_running_shell_commands() {
        let jobs = JobRegistry::new();
        for (id, kind) in [
            ("job_1", "task"),
            ("job_2", "turn"),
            ("task_3", "bash"),
            ("job_4", "bash"),
            ("job_5", "bash"),
            ("job_6", "bash"),
        ] {
            jobs.register_tracking(id.into(), kind, format!("{kind} work"));
        }
        jobs.finish("task_3", JobStatus::Done, "finished".into());
        jobs.finish("job_5", JobStatus::Failed, "failed".into());
        jobs.cancel("job_6");
        let rows = shell_jobs(&jobs);
        assert_eq!(
            rows.iter()
                .map(|(id, _, _, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["job_4"]
        );
        assert_eq!(rows[0].3, JobStatus::Running);
        assert_eq!(
            jobs.output_of("task_3"),
            Some((JobStatus::Done, "finished".into()))
        );
        jobs.finish("job_4", JobStatus::Done, "last finished".into());
        assert!(shell_jobs(&jobs).is_empty());
    }

    #[test]
    fn shell_panel_disappears_on_completion_even_when_root_is_cancelled() {
        for status in [JobStatus::Done, JobStatus::Failed, JobStatus::Cancelled] {
            let mut app = test_app();
            app.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            app.jobs
                .register_tracking("job_1".into(), "bash", "shell-smoke-command".into());
            assert!(app.refresh_job_panel());
            assert!(!app.refresh_job_panel());
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("background shell commands"));
            app.jobs.finish("job_1", status, "retained output".into());
            assert!(app.refresh_job_panel());
            assert!(!app.refresh_job_panel());
            terminal.draw(|f| app.draw(f)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(!text.contains("background shell commands"));
            assert!(!text.contains("shell-smoke-command"));
            assert!(!app.running);
            assert!(app.cancel.load(std::sync::atomic::Ordering::Relaxed));
            assert_eq!(
                app.jobs.output_of("job_1"),
                Some((status, "retained output".into()))
            );
        }
    }

    #[test]
    fn sidebar_uses_color_only_and_strikes_terminal_agents() {
        use team::ThreadStatus;
        for (status, color) in [
            (ThreadStatus::Running, Palette::RUNNING()),
            (ThreadStatus::Done, Palette::OK()),
            (ThreadStatus::Failed, Palette::ERROR()),
            (ThreadStatus::Cancelled, Palette::FAINT()),
        ] {
            for selected in [false, true] {
                let finished = status != ThreadStatus::Running;
                let row = sidebar_row(
                    "Review parser",
                    1,
                    thread_color(status),
                    finished,
                    selected,
                    42,
                );
                assert_eq!(row.to_string().trim(), "• Review parser");
                assert_eq!(row.spans[0].style.fg, Some(color));
                let style = row.spans[1].style;
                assert_eq!(
                    style.fg,
                    Some(if finished {
                        Palette::DIM()
                    } else {
                        Palette::TEXT()
                    })
                );
                assert_eq!(style.add_modifier.contains(Modifier::CROSSED_OUT), finished);
                assert_eq!(style.add_modifier.contains(Modifier::BOLD), selected);
                assert_eq!(row.width(), 42);
            }
        }
        for (running, color) in [(true, Palette::RUNNING()), (false, Palette::FAINT())] {
            let row = sidebar_row("main", 0, root_color(running), false, false, 42);
            assert_eq!(row.to_string().trim(), "• main");
            assert_eq!(row.spans[0].style.fg, Some(color));
            assert!(!row.spans[1]
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT));
        }
    }

    #[test]
    fn sidebar_rows_clip_unicode_and_depth_without_losing_selection_style() {
        for (label, depth, width, lead, visible, padding) in [
            ("界e\u{301}ab", 99, 8, "  • ", "界e\u{301}…", ""),
            ("界e\u{301}ab", 99, 12, "      • ", "界e\u{301}…", ""),
            ("A\nB\tC\rD", 0, 10, "  • ", "A B C…", ""),
            ("界e\u{301}", 0, 12, "  • ", "界e\u{301}", "     "),
            ("hidden", 99, 4, "  • ", "", ""),
        ] {
            let row = sidebar_row(label, depth, Palette::ERROR(), true, true, width);
            assert_eq!(row.spans.len(), 3);
            assert_eq!(row.spans[0].content, lead);
            assert_eq!(row.spans[1].content, visible);
            assert_eq!(row.spans[2].content, padding);
            assert_eq!(row.width(), width);
            assert_eq!(
                row.spans[1].style,
                Style::default()
                    .fg(Palette::DIM())
                    .bg(Palette::INPUT_BG())
                    .add_modifier(Modifier::BOLD | Modifier::CROSSED_OUT)
            );
            assert_eq!(row.spans[0].style.fg, Some(Palette::ERROR()));
            assert!(row
                .spans
                .iter()
                .all(|span| span.style.bg == Some(Palette::INPUT_BG())));
            assert!(!row.to_string().chars().any(char::is_control));
        }
    }

    #[test]
    fn workflow_rows_preserve_phase_positions_and_status_precedence() {
        use super::super::view::{WfAgent, WfPhase, WfStatus};
        let agent = |status| WfAgent {
            agent_id: "worker".into(),
            label: "Worker".into(),
            status,
            tools: 0,
            model: None,
            tokens: 0,
            duration_secs: None,
            started_unix: 0,
        };
        for (statuses, expected) in [
            (vec![], WfStatus::Running),
            (vec![WfStatus::Done, WfStatus::Done], WfStatus::Done),
            (vec![WfStatus::Running, WfStatus::Done], WfStatus::Running),
            (vec![WfStatus::Done, WfStatus::Running], WfStatus::Running),
            (vec![WfStatus::Running, WfStatus::Failed], WfStatus::Failed),
            (vec![WfStatus::Failed, WfStatus::Running], WfStatus::Failed),
            (vec![WfStatus::Done, WfStatus::Failed], WfStatus::Failed),
        ] {
            assert_eq!(
                phase_status(&statuses.into_iter().map(&agent).collect::<Vec<_>>()),
                expected
            );
        }
        let phases = vec![
            WfPhase {
                title: "first".into(),
                index: 7,
                total: 9,
                agents: vec![agent(WfStatus::Done)],
            },
            WfPhase {
                title: "empty".into(),
                index: 8,
                total: 9,
                agents: vec![],
            },
            WfPhase {
                title: "last".into(),
                index: 9,
                total: 9,
                agents: vec![agent(WfStatus::Running), agent(WfStatus::Failed)],
            },
        ];
        let positions = |rows: Vec<WfRow>| {
            rows.into_iter()
                .map(|row| match row {
                    WfRow::Phase(phase) => (phase, None),
                    WfRow::Agent(phase, agent) => (phase, Some(agent)),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            positions(workflow_rows(&phases, &std::collections::HashSet::new())),
            [
                (0, None),
                (0, Some(0)),
                (1, None),
                (2, None),
                (2, Some(0)),
                (2, Some(1))
            ]
        );
        assert_eq!(
            positions(workflow_rows(
                &phases,
                &std::collections::HashSet::from([0, 2, 99])
            )),
            [(0, None), (1, None), (2, None)]
        );
    }

    #[test]
    fn sidebar_windows_completed_agents_and_clicks_match_display_order() {
        let mut app = test_app();
        app.sidebar_open = true;
        for i in 0..30 {
            let id = format!("job_{i}");
            app.teams
                .on_spawn(&id, "root", &format!("Review module {i}"), "");
            app.teams.on_done(&id, false);
        }
        app.teams.on_spawn("reviewer", "root", "Check new work", "");
        let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
        let area = Rect::new(16, 0, 44, 10);
        terminal.draw(|f| app.draw_sidebar(f, area)).unwrap();
        assert_eq!(app.sidebar_rows.as_ref().unwrap()[1].1, "reviewer");
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(rendered.contains("Check new work"));
        assert!(rendered.contains("AGENTS · 1 running"));
        for _ in 0..31 {
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
        }
        terminal.draw(|f| app.draw_sidebar(f, area)).unwrap();
        assert_eq!(app.focused_agent.as_deref(), Some("job_29"));
        assert!(app.sidebar.scroll > 0);
        let hit = app.sidebar_rows.clone().unwrap();
        assert!(hit.iter().any(|(_, id)| id == "job_29"));
        let (row, id) = &hit[0];
        app.click_sidebar(0, *row);
        assert_eq!(app.focused_agent.as_deref(), Some("job_29"));
        app.click_sidebar(20, *row);
        assert_eq!(app.focused_agent.as_ref(), Some(id));
        assert_eq!(app.teams.display_order()[app.sidebar.selected - 1], *id);
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        terminal.draw(|f| app.draw_sidebar(f, area)).unwrap();
        assert_eq!(app.sidebar.scroll, 0);
        assert_eq!(app.sidebar_rows.as_ref().unwrap()[0].1, "");
    }

    mod routing {
        use crate::tui::test_support::test_app;
        use crate::tui::*;

        #[test]
        fn redraw_requests_survive_noop_input_until_the_frame_is_drawn() {
            use crossterm::event::{KeyEvent, MouseEvent};
            let mut app = test_app();
            let mouse = |kind| {
                CtEvent::Mouse(MouseEvent {
                    kind,
                    column: 0,
                    row: 0,
                    modifiers: KeyModifiers::NONE,
                })
            };
            let release = CtEvent::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ));
            let noops = [
                mouse(MouseEventKind::Moved),
                mouse(MouseEventKind::Up(MouseButton::Left)),
                release,
                CtEvent::FocusLost,
            ];
            for trigger in [mouse(MouseEventKind::ScrollUp), CtEvent::Resize(90, 20)] {
                let mut dirty = false;
                app.on_terminal_event(trigger, &mut dirty);
                assert!(dirty);
                for event in noops.clone() {
                    app.on_terminal_event(event, &mut dirty);
                    assert!(dirty);
                }
                dirty = false;
                for event in noops.clone() {
                    app.on_terminal_event(event, &mut dirty);
                    assert!(!dirty);
                }
            }
        }

        #[test]
        fn ctrl_x_targets_only_selected_agents_and_preserves_composition() {
            use bob_core::agent::team::mailbox;
            let mut app = test_app();
            let (_root_inbox, root_tx) = mailbox();
            let root = app
                .agent_team
                .register("root".into(), 0, String::new(), root_tx);
            let (_a_inbox, a_tx) = mailbox();
            let a = app.agent_team.register("a".into(), 1, "root".into(), a_tx);
            let (_b_inbox, b_tx) = mailbox();
            let b = app.agent_team.register("b".into(), 1, "root".into(), b_tx);
            app.teams.on_spawn("a", "root", "review a", "");
            app.teams.on_spawn("b", "root", "review b", "");
            app.running = true;
            app.sidebar_open = true;
            app.on_key(KeyCode::Char('x'), KeyModifiers::CONTROL);
            assert!(!root.cancellation_requested());
            assert!(!a.cancellation_requested());
            assert!(app.agent_control_notice.is_none());

            app.open_sidebar_on("a");
            app.input.set("draft");
            app.on_key(KeyCode::Char('x'), KeyModifiers::CONTROL);
            assert_eq!(app.input.text(), "draft");
            assert!(a.cancellation_requested());
            assert!(!b.cancellation_requested());
            assert!(app
                .control_notice("a")
                .unwrap()
                .contains("cancellation requested"));
            app.set_focused_agent(Some("b".into()));
            app.sidebar_open = false;
            app.input.set("keep this message");
            app.on_key(KeyCode::Char('x'), KeyModifiers::CONTROL);
            assert!(b.cancellation_requested());
            assert_eq!(app.input.text(), "keep this message");
            assert!(!root.cancellation_requested());
            assert!(!app.cancel.load(std::sync::atomic::Ordering::Relaxed));
        }

        #[test]
        fn ctrl_x_surfaces_unreachable_agent_errors_without_changing_status() {
            let mut app = test_app();
            app.teams.on_spawn("old", "root", "review old", "");
            app.teams.on_done("old", false);
            app.open_sidebar_on("old");
            app.on_key(KeyCode::Char('x'), KeyModifiers::CONTROL);
            assert!(app.control_notice("old").is_some());
            assert!(!app
                .control_notice("old")
                .unwrap()
                .contains("cancellation requested"));
            assert_eq!(
                app.teams.get("old").unwrap().status,
                team::ThreadStatus::Done
            );
            app.set_focused_agent(Some("old".into()));
            assert_eq!(app.stop_target().as_deref(), Some("old"));
        }

        #[test]
        fn closed_worker_prompts_are_removed_without_dropping_live_requests() {
            let mut app = test_app();
            let mut receivers = Vec::new();
            for id in ["closed-front", "live", "closed-back"] {
                let (resp, rx) = oneshot::channel();
                app.perm_queue.push_back(PendingPerm {
                    title: id.into(),
                    detail: String::new(),
                    preview: None,
                    options: Vec::new(),
                    list: widgets::SelectList::new(),
                    resp,
                });
                if id == "live" {
                    receivers.push(rx);
                }
            }
            let (resp, rx) = oneshot::channel();
            app.pending_query = Some(PendingQuery {
                query: bob_core::tools::registry::UserQuery {
                    title: "question".into(),
                    detail: String::new(),
                    options: vec![],
                    allow_other: true,
                },
                list: widgets::SelectList::new(),
                other_text: Some("draft".into()),
                inspection: Default::default(),
                purpose: QueryPurpose::Tool(resp),
            });
            assert!(app.clear_closed_prompts());
            assert_eq!(app.perm_queue.len(), 1);
            assert_eq!(app.perm_queue.front().unwrap().title, "live");
            assert!(app.pending_query.is_some());
            app.sidebar_open = true;
            app.set_focused_agent(Some("live".into()));
            assert!(
                app.stop_target().is_none(),
                "free-text answers are compose mode"
            );
            app.pending_query.as_mut().unwrap().other_text = None;
            assert_eq!(app.stop_target().as_deref(), Some("live"));
            assert!(!app.clear_closed_prompts());
            drop(rx);
            assert!(app.clear_closed_prompts());
            assert!(app.pending_query.is_none());
            app.open_reasoning_picker(bob_core::core::types::ReasoningEffort::Off);
            assert!(!app.clear_closed_prompts());
            assert!(app.pending_query.is_some());
            drop(receivers);
            assert!(app.clear_closed_prompts());
            assert!(app.perm_queue.is_empty());
        }

        #[test]
        fn sidebar_keeps_agent_identity_when_status_reorders_rows() {
            let mut app = test_app();
            app.sidebar_open = true;
            for id in ["task_1", "job_2", "reviewer"] {
                app.apply_agent_event(&AgentEvent::SubagentSpawn {
                    agent_id: id.into(),
                    parent_id: "root".into(),
                    task: format!("Review {id}"),
                    prompt: String::new(),
                });
            }
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
            app.open_sidebar_on("task_1");
            app.apply_agent_event(&AgentEvent::SubagentDone {
                agent_id: "task_1".into(),
                failed: false,
                cancelled: false,
            });
            assert_eq!(app.teams.display_order(), ["job_2", "reviewer", "task_1"]);
            assert_eq!(app.focused_agent.as_deref(), Some("task_1"));
            assert_eq!(app.sidebar.selected, 3);
            app.on_key(KeyCode::Up, KeyModifiers::NONE);
            assert_eq!(app.focused_agent.as_deref(), Some("reviewer"));
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
            assert_eq!(app.focused_agent.as_deref(), Some("task_1"));
            app.apply_agent_event(&AgentEvent::SubagentSpawn {
                agent_id: "new-reviewer".into(),
                parent_id: "root".into(),
                task: "Check tests".into(),
                prompt: String::new(),
            });
            assert_eq!(app.sidebar.selected, 4);
            assert_eq!(app.focused_agent.as_deref(), Some("task_1"));
        }

        #[test]
        fn ctrl_t_toggles_sidebar_and_preserves_focus() {
            let mut app = test_app();
            app.on_key(KeyCode::Char('t'), KeyModifiers::CONTROL);
            assert!(app.sidebar_open);
            app.on_key(KeyCode::Char('t'), KeyModifiers::CONTROL);
            assert!(!app.sidebar_open);
            app.teams.on_spawn("a", "root", "review a", "");
            app.on_key(KeyCode::Char('t'), KeyModifiers::CONTROL);
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
            assert_eq!(app.focused_agent.as_deref(), Some("a"));
            app.input.set("draft");
            app.on_key(KeyCode::Char('t'), KeyModifiers::CONTROL);
            assert!(!app.sidebar_open);
            assert_eq!(app.focused_agent.as_deref(), Some("a"));
            assert_eq!(app.input.text(), "draft");
            app.running = true;
            app.on_key(KeyCode::Esc, KeyModifiers::NONE);
            assert!(app.focused_agent.is_none());
            assert_eq!(app.sidebar.selected, 0);
            assert!(!app.cancel.load(std::sync::atomic::Ordering::Relaxed));
        }

        #[test]
        fn focused_input_messages_the_agent_even_with_alt_enter_while_root_runs() {
            use bob_core::agent::team::mailbox;
            for mods in [KeyModifiers::NONE, KeyModifiers::ALT] {
                let mut app = test_app();
                let (mut root_inbox, root_tx) = mailbox();
                app.agent_team
                    .register("root".into(), 0, String::new(), root_tx);
                let (mut inbox, tx) = mailbox();
                app.agent_team.register("a".into(), 1, "root".into(), tx);
                app.teams.on_spawn("a", "root", "review", "");
                app.open_sidebar_on("a");
                app.running = true;
                app.input.set("check the tests");
                let KeyOutcome::Submit(text) = app.on_key(KeyCode::Enter, mods) else {
                    panic!("focused input must submit to the selected agent");
                };
                let (events_tx, mut events_rx) = mpsc::unbounded_channel();
                let _subscription = app.bus.subscribe(Arc::new(move |event| {
                    let _ = events_tx.send(event.clone());
                }));
                app.send_agent_message("a", &text);
                app.apply_agent_event(&events_rx.try_recv().unwrap());
                let messages = inbox.drain();
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].from, "user");
                assert_eq!(messages[0].text, "check the tests");
                assert!(root_inbox.drain().is_empty());
                assert!(app.queue.is_empty());
                assert!(app.view.cells.is_empty());
                assert!(matches!(&app.teams.get("a").unwrap().cells[0],
                view::Cell::AgentMsg { from, text } if from == "user" && text == "check the tests"));
                assert_eq!(app.teams.get("a").unwrap().unread, 0);
            }
        }

        #[test]
        fn unreachable_agent_message_is_not_echoed_as_delivered() {
            let mut app = test_app();
            app.teams.on_spawn("old", "root", "review", "");
            app.teams.on_done("old", false);
            app.open_sidebar_on("old");
            app.send_agent_message("old", "hello");
            assert!(app.teams.get("old").unwrap().cells.is_empty());
            assert_eq!(app.control_notice("old"), Some("old is not reachable"));
        }
    }

    mod prompts {
        use crate::tui::prompts::ActivePrompt;
        use crate::tui::test_support::test_app;
        use crate::tui::theme::Palette;
        use crate::tui::*;
        use ratatui::{backend::TestBackend, Terminal};

        fn query(title: &str) -> (PendingQuery, oneshot::Receiver<Option<String>>) {
            let (resp, rx) = oneshot::channel();
            (
                PendingQuery {
                    query: bob_core::tools::registry::UserQuery {
                        title: title.into(),
                        detail: "detail\n".repeat(30),
                        options: vec!["first".into(), "second".into()],
                        allow_other: true,
                    },
                    list: widgets::SelectList::new(),
                    other_text: None,
                    inspection: Default::default(),
                    purpose: QueryPurpose::Tool(resp),
                },
                rx,
            )
        }

        fn permission() -> (PendingPerm, oneshot::Receiver<Option<usize>>) {
            let (resp, rx) = oneshot::channel();
            (
                PendingPerm {
                    title: "Allow edit?".into(),
                    detail: String::new(),
                    preview: None,
                    options: vec![PermissionOption {
                        label: "Allow once".into(),
                        allow: true,
                        grant: None,
                    }],
                    list: widgets::SelectList::new(),
                    resp,
                },
                rx,
            )
        }

        #[test]
        fn visible_permission_owns_keys_and_preserves_questions() {
            for permission_first in [false, true] {
                let mut app = test_app();
                let (p, mut permission_rx) = permission();
                let (q, mut query_rx) = query("Question");
                if permission_first {
                    app.perm_queue.push_back(p);
                    app.enqueue_query(q);
                } else {
                    app.enqueue_query(q);
                    app.perm_queue.push_back(p);
                }
                assert_eq!(app.active_prompt(), ActivePrompt::Permission);
                app.on_key(KeyCode::Enter, KeyModifiers::NONE);
                assert_eq!(permission_rx.try_recv().unwrap(), Some(0));
                assert!(query_rx.try_recv().is_err());
                assert_eq!(app.active_prompt(), ActivePrompt::Query);
                app.on_key(KeyCode::Enter, KeyModifiers::NONE);
                assert_eq!(query_rx.try_recv().unwrap(), Some("first".into()));
            }
        }

        #[test]
        fn questions_and_picker_results_queue_without_dropping_senders() {
            let mut app = test_app();
            let (one, mut one_rx) = query("One");
            let (two, mut two_rx) = query("Two");
            app.enqueue_query(one);
            app.enqueue_query(two);
            app.open_reasoning_picker(bob_core::core::types::ReasoningEffort::High);
            assert_eq!(app.query_queue.len(), 2);
            app.on_key(KeyCode::Enter, KeyModifiers::NONE);
            assert_eq!(one_rx.try_recv().unwrap(), Some("first".into()));
            assert!(two_rx.try_recv().is_err());
            assert_eq!(app.pending_query.as_ref().unwrap().query.title, "Two");
            app.on_key(KeyCode::Esc, KeyModifiers::NONE);
            assert_eq!(two_rx.try_recv().unwrap(), None);
            assert_eq!(app.pending_query.as_ref().unwrap().list.selected, 3);
        }

        #[test]
        fn late_model_list_queues_and_preselects_current_model() {
            let mut app = test_app();
            app.model_label = "current".into();
            let (q, _rx) = query("Keep this question");
            app.enqueue_query(q);
            app.open_model_picker(vec!["other\t128k ctx".into(), "current\t400k ctx".into()]);
            assert_eq!(
                app.pending_query.as_ref().unwrap().query.title,
                "Keep this question"
            );
            assert_eq!(app.query_queue.front().unwrap().list.selected, 1);
            app.on_key(KeyCode::Esc, KeyModifiers::NONE);
            assert!(matches!(
                app.pending_query.as_ref().unwrap().purpose,
                QueryPurpose::ModelPicker
            ));
        }

        #[test]
        fn paste_only_changes_the_visible_composer() {
            let mut app = test_app();
            app.input.set("root draft");
            let (mut q, _query_rx) = query("Question");
            q.other_text = Some("answer ".into());
            app.enqueue_query(q);
            app.paste_active("pasted");
            assert_eq!(
                app.pending_query.as_ref().unwrap().other_text.as_deref(),
                Some("answer pasted")
            );
            assert_eq!(app.input.text(), "root draft");
            let (p, _permission_rx) = permission();
            app.perm_queue.push_back(p);
            app.paste_active("invisible");
            assert_eq!(
                app.pending_query.as_ref().unwrap().other_text.as_deref(),
                Some("answer pasted")
            );
            assert_eq!(app.input.text(), "root draft");
        }

        #[test]
        fn escape_dismisses_completions_before_interrupting_or_dropping_queue() {
            let mut app = test_app();
            app.running = true;
            app.queue.push_back("queued".into());
            app.menu.push(("/models".into(), "models".into()));
            app.on_key(KeyCode::Esc, KeyModifiers::NONE);
            assert!(app.menu.is_empty());
            assert!(!app.cancel.load(std::sync::atomic::Ordering::Relaxed));
            assert_eq!(app.queue.len(), 1);
            app.on_key(KeyCode::Esc, KeyModifiers::NONE);
            assert!(app.cancel.load(std::sync::atomic::Ordering::Relaxed));
            assert!(app.queue.is_empty());
        }

        #[test]
        fn query_wraps_unicode_without_changing_numbered_answers() {
            let title = "选择实现方式：保留完整问题和选项 e\u{301}";
            let answer = "保留完整标签和所有选项e\u{301}，继续实现。";
            for width in [16, 24, 40] {
                let mut app = test_app();
                let (mut q, mut rx) = query(title);
                q.query.detail.clear();
                q.query.options[1] = answer.into();
                q.list.selected = 1;
                app.enqueue_query(q);
                let lines = app.query_lines(width);
                assert!(lines.iter().all(|line| line.width() <= width));
                let rendered_title: String = lines
                    .iter()
                    .take_while(|line| line.width() > 0)
                    .map(|line| line.to_string().chars().skip(2).collect::<String>())
                    .collect();
                assert_eq!(rendered_title, title);
                let selected: Vec<_> = lines
                    .iter()
                    .filter(|line| line.style.bg == Some(Palette::SELECTED_BG()))
                    .collect();
                assert!(selected.len() > 1);
                assert!(selected[0].to_string().starts_with(" ❯ 2. "));
                let rendered_answer: String = selected
                    .iter()
                    .map(|line| line.to_string().chars().skip(6).collect::<String>())
                    .collect();
                assert_eq!(rendered_answer, answer);
                assert!(lines
                    .iter()
                    .any(|line| line.to_string().contains("3. Other…")));
                app.on_key(KeyCode::Char('2'), KeyModifiers::NONE);
                assert_eq!(rx.try_recv().unwrap(), Some(answer.into()));
            }
        }

        #[test]
        fn query_viewport_keeps_selected_continuations_and_free_text_tail() {
            let mut app = test_app();
            let (mut q, mut rx) = query("Choose an implementation");
            q.query.options[1] =
                "Keep the full selected option visible through its final word".into();
            q.list.selected = 1;
            app.enqueue_query(q);
            let expected: Vec<_> = app
                .query_lines(24)
                .into_iter()
                .filter(|line| line.style.bg == Some(Palette::SELECTED_BG()))
                .map(|line| line.to_string().trim().to_string())
                .collect();
            assert!(expected.len() > 1);
            let mut terminal = Terminal::new(TestBackend::new(32, 9)).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    app.draw_query(frame, area);
                })
                .unwrap();
            let content: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            for row in expected {
                assert!(content.contains(&row), "{row:?}: {content}");
            }
            app.on_key(KeyCode::Char('3'), KeyModifiers::NONE);
            let answer = format!("{}\n最后回答 END", "long answer ".repeat(20));
            app.paste_active(&answer);
            assert!(app.query_lines(24).iter().all(|line| line.width() <= 24));
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    app.draw_query(frame, area);
                })
                .unwrap();
            let content: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(content.contains("END_"), "{content}");
            app.on_key(KeyCode::Enter, KeyModifiers::NONE);
            assert_eq!(rx.try_recv().unwrap(), Some(answer));
        }

        fn query_screen(app: &mut App, terminal: &mut Terminal<TestBackend>) -> String {
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    app.draw_query(frame, area);
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect()
        }

        #[test]
        fn inspection_exposes_full_question_and_options_without_changing_answer() {
            let mut app = test_app();
            let title = format!("{}Remove production data?", "Question context\n".repeat(35));
            let answer = format!(
                "{}including all backups",
                "Remove stored records\n".repeat(40)
            );
            let (mut q, mut rx) = query(&title);
            q.query.detail = "This cannot be undone".into();
            q.query.options = vec!["Leave unchanged".into(); 11];
            q.query.options[0] = answer.clone();
            q.query.options[10] = "Keep backups instead".into();
            app.enqueue_query(q);
            let mut terminal = Terminal::new(TestBackend::new(72, 24)).unwrap();
            let initial = query_screen(&mut app, &mut terminal);
            assert!(initial.contains("Remove stored records"));
            assert!(!initial.contains("including all backups"));
            app.on_key(KeyCode::PageDown, KeyModifiers::NONE);
            let suffix = query_screen(&mut app, &mut terminal);
            assert!(suffix.contains("including all backups"));

            let (p, _permission_rx) = permission();
            app.perm_queue.push_back(p);
            let offset = app.pending_query.as_ref().unwrap().inspection.offset;
            app.on_key(KeyCode::PageUp, KeyModifiers::NONE);
            assert_eq!(
                app.pending_query.as_ref().unwrap().inspection.offset,
                offset
            );
            app.on_key(KeyCode::Esc, KeyModifiers::NONE);
            for _ in 0..4 {
                app.on_key(KeyCode::PageUp, KeyModifiers::NONE);
                query_screen(&mut app, &mut terminal);
            }
            assert_eq!(
                app.pending_query.as_ref().unwrap().inspection.offset,
                Some(0)
            );
            let mut inspected = String::new();
            for _ in 0..8 {
                inspected.push_str(&query_screen(&mut app, &mut terminal));
                app.on_key(KeyCode::PageDown, KeyModifiers::NONE);
            }
            for text in [
                "Remove production data?",
                "This cannot be undone",
                "Keep backups instead",
            ] {
                assert!(inspected.contains(text), "{text}");
            }
            assert_eq!(app.pending_query.as_ref().unwrap().list.selected, 0);
            assert!(rx.try_recv().is_err());
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
            assert!(app
                .pending_query
                .as_ref()
                .unwrap()
                .inspection
                .offset
                .is_none());
            app.on_key(KeyCode::Up, KeyModifiers::NONE);
            assert_eq!(query_screen(&mut app, &mut terminal), initial);
            app.on_key(KeyCode::PageDown, KeyModifiers::NONE);
            query_screen(&mut app, &mut terminal);
            let (next, _next_rx) = query("Next question");
            app.enqueue_query(next);
            app.on_key(KeyCode::Enter, KeyModifiers::NONE);
            assert_eq!(rx.try_recv().unwrap(), Some(answer));
            assert!(app
                .pending_query
                .as_ref()
                .unwrap()
                .inspection
                .offset
                .is_none());
        }

        #[test]
        fn large_answer_paste_keeps_panel_height_and_follows_edits() {
            let mut app = test_app();
            let (mut q, mut rx) = query("Question");
            q.query.detail.clear();
            app.enqueue_query(q);
            app.on_key(KeyCode::Char('3'), KeyModifiers::NONE);
            let answer = format!("{}KEEP BACKUPS", "\n".repeat(65_532));
            app.paste_active(&answer);
            let mut terminal = Terminal::new(TestBackend::new(72, 40)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(app.pending_query.as_ref().unwrap().inspection.page_size, 21);
            let content: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(content.contains("KEEP BACKUPS_"));
            app.on_key(KeyCode::PageUp, KeyModifiers::NONE);
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(
                app.pending_query.as_ref().unwrap().other_text.as_deref(),
                Some(answer.as_str())
            );
            app.on_key(KeyCode::Char('!'), KeyModifiers::NONE);
            assert!(app
                .pending_query
                .as_ref()
                .unwrap()
                .inspection
                .offset
                .is_none());
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(app.pending_query.as_ref().unwrap().inspection.page_size, 21);
            app.on_key(KeyCode::Enter, KeyModifiers::NONE);
            assert_eq!(rx.try_recv().unwrap(), Some(format!("{answer}!")));
        }

        #[test]
        fn model_picker_keeps_display_aligned_metadata_and_exact_ids() {
            let mut app = test_app();
            app.open_model_picker(vec!["界界-model\t128k ctx".into(), "abc\t400k ctx".into()]);
            let columns: Vec<_> = app
                .query_lines(48)
                .iter()
                .filter(|line| {
                    line.spans
                        .last()
                        .is_some_and(|span| span.content.ends_with("ctx"))
                })
                .map(|line| {
                    line.spans[..line.spans.len() - 1]
                        .iter()
                        .map(Span::width)
                        .sum::<usize>()
                })
                .collect();
            assert_eq!(columns.len(), 2);
            assert_eq!(columns[0], columns[1]);
            assert!(app.query_lines(16).iter().all(|line| line.width() <= 16));
            assert!(matches!(
                app.on_key(KeyCode::Enter, KeyModifiers::NONE),
                KeyOutcome::SwitchModel(id) if id == "界界-model"
            ));
        }

        #[test]
        fn short_prompt_keeps_selected_option_visible_and_hides_completion_menu() {
            let mut app = test_app();
            let (mut q, _rx) = query("Question");
            q.list.selected = 1;
            app.enqueue_query(q);
            app.menu.push(("/invisible".into(), String::new()));
            let mut terminal = Terminal::new(TestBackend::new(48, 10)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let content: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(content.contains("second"), "{content}");
            assert!(!content.contains("/invisible"));
        }

        #[test]
        fn hidden_sidebar_still_names_recipient_and_uses_child_activity() {
            let mut app = test_app();
            app.teams.on_spawn("reviewer", "root", "Review", "");
            app.teams.on_done("reviewer", false);
            app.sidebar_open = false;
            app.running = true;
            let mut terminal = Terminal::new(TestBackend::new(48, 12)).unwrap();
            for focused in [None, Some("reviewer"), None] {
                app.set_focused_agent(focused.map(str::to_owned));
                assert_eq!(app.focused_activity().0, focused.is_none());
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let content: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                assert_eq!(
                    content.contains("to:reviewer"),
                    focused.is_some(),
                    "{content}"
                );
                if focused.is_some() {
                    assert!(!content.contains("Working"));
                }
            }
        }

        #[test]
        fn input_viewport_tracks_cursor_not_tail() {
            let mut app = test_app();
            app.input.set(&"long input ".repeat(100));
            let mut terminal = Terminal::new(TestBackend::new(48, 18)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert!(app.input_scroll > 0);
            app.input.move_home();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(app.input_scroll, 0);
        }

        #[test]
        fn user_messages_replay_once_and_context_reset_clears_both_views() {
            let mut app = test_app();
            let events = vec![
                AgentEvent::UserMessageQueued {
                    to: "root".into(),
                    text: "steer".into(),
                },
                AgentEvent::UserMessageQueued {
                    to: "child".into(),
                    text: "check".into(),
                },
            ];
            for event in &events {
                app.apply_agent_event(event);
            }
            assert!(matches!(&app.view.cells[0], Cell::User(text) if text == "steer"));
            let replay = team::AgentTranscripts::replay(&events);
            assert_eq!(replay.get("child").unwrap().cells.len(), 1);
            app.apply_agent_event(&AgentEvent::ContextReset {
                agent_id: "root".into(),
                epoch: 1,
            });
            assert!(app.view.cells.is_empty());
            assert!(app.teams.is_empty());
        }
    }

    mod session {
        use crate::tui::test_support::test_app;
        use crate::tui::*;

        #[test]
        fn compaction_waits_for_event_and_outcome_before_releasing_turn() {
            for event_first in [false, true] {
                let mut app = test_app();
                app.running = true;
                let cell = app.view.begin_compaction();
                let event = AgentEvent::Compaction {
                    agent_id: "root".into(),
                    before_tokens: 900,
                    after_tokens: 100,
                    summary: "summary".into(),
                    replaced_upto: 3,
                };
                if event_first {
                    app.apply_agent_event(&event);
                    assert!(app.turn_done_rx.try_recv().is_err());
                }
                app.finish_compaction(cell, Ok((true, 900, 100)));
                if !event_first {
                    assert!(app.turn_done_rx.try_recv().is_err());
                    assert!(app.running);
                    app.apply_agent_event(&event);
                }
                assert_eq!(
                    app.turn_done_rx.try_recv().unwrap(),
                    TurnCompletion::default()
                );
                assert!(app.turn_done_rx.try_recv().is_err());
                assert!(app.compaction_completion.is_none());
                let next = app.view.begin_compaction();
                app.finish_compaction(next, Ok((false, 100, 100)));
                assert_eq!(
                    app.turn_done_rx.try_recv().unwrap(),
                    TurnCompletion::default()
                );
                assert!(!app
                    .view
                    .cells
                    .iter()
                    .any(|cell| matches!(cell, Cell::Compaction { done: false, .. })));
            }
        }

        #[test]
        fn reset_ui_clears_recipient_and_old_activity() {
            let mut app = test_app();
            app.teams
                .on_spawn("old", "root", "old task", "instructions");
            app.set_focused_agent(Some("old".into()));
            app.queue.push_back("queued".into());
            app.view.push_user("old prompt".into());
            app.context_usage = Some((50, 100));
            app.reset_session_ui();
            assert!(app.focused_agent.is_none());
            assert!(app.view.cells.is_empty());
            assert!(app.teams.is_empty());
            assert!(app.queue.is_empty());
            assert!(app.context_usage.is_none());
        }
    }
}
