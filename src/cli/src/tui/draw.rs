//! All rendering for the TUI: the top-level `draw` and every `draw_*` /
//! `*_lines` helper. Split out of `mod.rs` to keep that file focused on the
//! app state + event loop. These are methods on `super::App`; because this is a
//! child module, they can access App's private fields directly.

use super::theme::Palette;
use super::{indent_line, render, team, truncate_mid, App};
use bob_core::core::permissions::Mode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

/// Horizontal breathing room inside the input band, in columns per side. The
/// height calc and the renderer BOTH inset the text by this, so wrapping agrees
/// and the band grows to fit exactly (mismatched widths clipped long lines).
const INPUT_PAD: u16 = 3;

impl App {
    /// Build the wrapped, prompt-prefixed display lines for the input box, given
    /// the usable text width. Used for BOTH the height calc and rendering so they
    /// never disagree. The first row carries a `›` marker, wrapped/continuation
    /// rows a 2-space indent, so text stays aligned under the marker.
    fn input_lines(&self, width: usize, busy: bool) -> Vec<Line<'static>> {
        // The prompt marker + its continuation indent are both 2 columns wide, so
        // content wraps at `width - 2` and every visual row aligns.
        const PREFIX: usize = 2;
        let text_color = Style::default().fg(Palette::TEXT());
        let marker = || Span::styled("› ", Style::default().fg(Palette::DIM()));
        let indent = || Span::styled("  ", Style::default());

        if self.input.text().is_empty() && !busy {
            return vec![Line::from(vec![
                marker(),
                Span::styled(
                    "send a message...  (Ctrl+J or Shift+Enter for newline)",
                    Style::default().fg(Palette::FAINT()),
                ),
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
        let area = f.area();
        // Force the theme's base background across the whole screen so bob looks
        // identical regardless of the terminal's own background. Themes that want
        // to inherit the terminal use Color::Reset here (a no-op paint).
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        // The input band grows with the number of *wrapped* text lines
        // (1 pad row above + N text rows + 1 pad row below), capped so it can't
        // eat the whole screen. Use the SAME inset width the renderer uses, or the
        // height won't match the wrapped line count.
        let text_width = area.width.saturating_sub(INPUT_PAD * 2) as usize;
        let wrapped = self
            .input_lines(text_width, self.running || self.view.busy)
            .len();
        let text_rows = (wrapped as u16).clamp(1, 12);
        let input_height = text_rows + 2;

        // The band above the input shows either a permission prompt or a user
        // question (they don't co-occur), sized to its content.
        let prompt_height = if self.pending_perm.is_some() {
            (self.permission_lines(area.width as usize).len() as u16 + 1).min(24)
        } else if self.pending_query.is_some() {
            (self.query_lines(area.width as usize).len() as u16 + 1).min(24)
        } else {
            0
        };

        // A pinned background-jobs panel sits just above the input when any
        // jobs exist (one row per job + a header).
        let job_rows = self.jobs.list();
        let jobs_height = if job_rows.is_empty() {
            0
        } else {
            (job_rows.len() as u16 + 1).min(8)
        };

        // A sticky todo panel sits just above the input while the list is
        // non-empty (one row per item + a header), capped so it can't dominate.
        // Hidden when the user toggles it off (Ctrl+L).
        let todo_items = self.todos.as_ref().map(|t| t.items()).unwrap_or_default();
        let todos_height = if todo_items.is_empty() || !self.show_todos {
            0
        } else {
            // header + one blank line of padding above and below.
            (todo_items.len() as u16 + 3).min(14)
        };

        // A pinned "queued messages" panel sits just above the input when messages
        // are waiting to be sent after the current turn (one row per chip + header).
        let queue_height = if self.queue.is_empty() {
            0
        } else {
            (self.queue.len() as u16 + 1).min(6)
        };

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
            .split(area);

        self.draw_scrollback(f, chunks[0]);
        if self.pending_perm.is_some() {
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
        self.draw_input(f, chunks[5]);
        self.draw_status_bar(f, chunks[6]);

        if !self.menu.is_empty() {
            self.draw_menu(f, chunks[5]);
        }
        if !self.file_menu.is_empty() {
            self.draw_file_menu(f, chunks[5]);
        }
        // The team drawer is a full overlay above everything else.
        if self.team_drawer.is_some() {
            self.draw_team_drawer(f, area);
        } else {
            // Drop the stale roster hit-box so clicks don't select a hidden agent.
            self.roster_rect = None;
        }
        if let Some(toast) = self.toast.clone() {
            self.draw_toast(f, area, &toast);
        }
    }

    /// Sticky todo checklist above the input: a header with the done/total count,
    /// then one row per item — ☐ pending (dim), ◐ in-progress (accent, bold), ✓
    /// done (green, struck-through-ish via dim).
    fn draw_todos(
        &self,
        f: &mut ratatui::Frame,
        area: Rect,
        items: &[bob_core::tools::todo::TodoItem],
    ) {
        use bob_core::tools::todo::TodoStatus;
        f.render_widget(Clear, area);
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
            Line::from(Span::styled(
                format!("  {}", header),
                Style::default().fg(Palette::DIM()),
            )),
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
                Span::styled(format!("  {} ", glyph), Style::default().fg(glyph_color)),
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
        let running = jobs
            .iter()
            .filter(|(_, _, _, s)| *s == JobStatus::Running)
            .count();
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!(" background jobs · {} running ", running),
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
                Span::styled(format!("  {} ", glyph), Style::default().fg(color)),
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

    /// Pinned "queued messages" panel: the messages waiting to be sent after the
    /// current turn, shown as chips above the input (not in the transcript). The
    /// last chip can be popped back for editing with Backspace on an empty prompt.
    fn draw_queue(&self, f: &mut ratatui::Frame, area: Rect) {
        f.render_widget(Clear, area);
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!(" queued · {} · sent after this turn ", self.queue.len()),
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
                Span::styled("  › ", Style::default().fg(Palette::DIM())),
                Span::styled(
                    truncate_mid(
                        &msg.replace('\n', " "),
                        area.width.saturating_sub(4) as usize,
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
    /// on the right, the selected agent's live transcript. Toggled with Ctrl+T.
    fn draw_team_drawer(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let Some(drawer) = self.team_drawer.as_ref() else {
            return;
        };
        // Snapshot the fields we need so we can later take a mutable borrow of
        // `self.team_drawer` to write the clamped scroll back without a conflict.
        let sel = drawer.selected;
        let hovered = drawer.hovered;
        let drawer_scroll = drawer.scroll;
        let compose_buf: Option<String> = drawer.composing.clone();
        let composing = compose_buf.is_some();
        // A distinct popup background sets the drawer apart from the main log.
        // Minimal, chrome-free overlay matching the rest of the TUI: base
        // background, dim prose, one colored status dot per agent, the selected
        // agent bold, separation by whitespace (no boxes/borders/dividers/bars).
        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );

        // Header · body · hint, all on the base background.
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // blank + header
                Constraint::Min(1),    // body
                Constraint::Length(1), // hint
            ])
            .split(area);

        let order = self.teams.display_order();
        let running = order
            .iter()
            .filter(|id| {
                matches!(
                    self.teams.get(id).map(|t| t.status),
                    Some(team::ThreadStatus::Running)
                )
            })
            .count();
        // Terse lowercase header, like the jobs/todos panels.
        let header = Line::from(Span::styled(
            format!("  team · {} agents · {} running", order.len(), running),
            Style::default().fg(Palette::DIM()),
        ));
        f.render_widget(
            Paragraph::new(vec![Line::from(""), header]).style(Style::default().bg(Palette::BG())),
            outer[0],
        );

        // Roster on the left, transcript on the right, split by whitespace only.
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(24), Constraint::Min(10)])
            .split(outer[1]);
        // Remember the roster rect so left-clicks can select an agent. The roster
        // has one leading blank line, so agent `i` sits at row `body[0].y + 1 + i`.
        self.roster_rect = Some(body[0]);

        // Left: agent roster (running first, finished at the bottom & dimmed).
        let mut roster: Vec<Line> = vec![Line::from("")];
        for (i, id) in order.iter().enumerate() {
            let Some(t) = self.teams.get(id) else {
                continue;
            };
            let finished = !matches!(t.status, team::ThreadStatus::Running);
            let dot_color = match t.status {
                team::ThreadStatus::Running => Palette::RUNNING(),
                team::ThreadStatus::Done => Palette::OK(),
                team::ThreadStatus::Failed => Palette::ERROR(),
            };
            let selected = i == sel;
            let hover = hovered == Some(i);
            // Names are white (TEXT) and legible; the SELECTED or HOVERED row is
            // bold so the agent under the cursor/selection stands out. Finished
            // agents are faint. No accent color or pointer — keep it plain.
            let name_style = if selected || hover {
                Style::default()
                    .fg(Palette::TEXT())
                    .add_modifier(Modifier::BOLD)
            } else if finished {
                Style::default().fg(Palette::FAINT())
            } else {
                Style::default().fg(Palette::TEXT())
            };
            let depth = self.teams.depth_of(id);
            let indent = "  ".repeat(depth);
            let mut spans = vec![
                Span::styled(format!("  {}• ", indent), Style::default().fg(dot_color)),
                Span::styled(
                    truncate_mid(&t.name, 16usize.saturating_sub(depth * 2)),
                    name_style,
                ),
            ];
            if t.unread > 0 && !finished {
                spans.push(Span::styled(
                    format!(" ({})", t.unread),
                    Style::default().fg(Palette::DIM()),
                ));
            }
            roster.push(Line::from(spans));
        }
        f.render_widget(
            Paragraph::new(roster).style(Style::default().bg(Palette::BG())),
            body[0],
        );

        // Right: the selected agent's transcript, rendered with the same cells as
        // the main scrollback. A faint task subheader, then a blank line.
        let mut transcript: Vec<Line> = vec![Line::from("")];
        if let Some(id) = order.get(sel) {
            if let Some(t) = self.teams.get(id) {
                if !t.task.is_empty() {
                    transcript.push(Line::from(Span::styled(
                        truncate_mid(&t.task, body[1].width.saturating_sub(2) as usize),
                        Style::default().fg(Palette::FAINT()),
                    )));
                    transcript.push(Line::from(""));
                }
                for cell in &t.cells {
                    render::render_cell(cell, body[1].width as usize, &mut transcript);
                }
            }
        }
        // Wrap long lines to the transcript pane width so conversations don't get
        // clipped at the right edge (the same wrapping the main scrollback uses).
        // Wrapping BEFORE the scroll math keeps the offset counted in real rows.
        let tw = body[1].width as usize;
        let transcript: Vec<Line> = transcript
            .into_iter()
            .flat_map(|l| super::wrap_line(l, tw))
            .collect();
        // Apply the drawer's scroll offset, clamping it and WRITING IT BACK so the
        // stored value can't run past the end. Otherwise a fast wheel flick keeps
        // incrementing `scroll` past max, and you have to scroll back down through
        // all that phantom overshoot before the view visibly moves ("runoff").
        let view_h = body[1].height as usize;
        let max_scroll = transcript.len().saturating_sub(view_h);
        let scroll = (drawer_scroll as usize).min(max_scroll);
        if let Some(d) = self.team_drawer.as_mut() {
            d.scroll = scroll as u16;
        }
        let visible: Vec<Line> = transcript.into_iter().skip(scroll).collect();
        f.render_widget(
            Paragraph::new(visible).style(Style::default().bg(Palette::BG())),
            body[1],
        );

        // Terse dim hint / compose line — no filled bar.
        let hint = if composing {
            let buf = compose_buf.as_deref().unwrap_or("");
            Line::from(vec![
                Span::styled("  › ", Style::default().fg(Palette::ACCENT())),
                Span::styled(buf.to_string(), Style::default().fg(Palette::TEXT())),
            ])
        } else {
            Line::from(Span::styled(
                "  ↑↓ select · i message · esc close",
                Style::default().fg(Palette::FAINT()),
            ))
        };
        f.render_widget(
            Paragraph::new(hint).style(Style::default().bg(Palette::BG())),
            outer[2],
        );
    }

    fn draw_scrollback(&mut self, f: &mut ratatui::Frame, full: Rect) {
        let working = self.running || self.view.busy;
        let secs = self
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        self.scrollback
            .render(f, full, &self.view, working, self.spinner, secs);
    }

    /// A one-line status bar below the input: cwd · branch · mode.
    fn draw_status_bar(&self, f: &mut ratatui::Frame, area: Rect) {
        const PAD: u16 = 3;
        let sep = || Span::styled("  ", Style::default().fg(Palette::FAINT()));
        let mut spans: Vec<Span> = Vec::new();

        // cwd
        if !self.cwd_label.is_empty() {
            spans.push(Span::styled(
                self.cwd_label.clone(),
                Style::default().fg(Palette::ACCENT()),
            ));
        }
        // git branch
        if let Some(b) = &self.branch {
            spans.push(sep());
            spans.push(Span::styled(
                format!("\u{2387} {b}"), // ⎇ branch glyph
                Style::default().fg(Palette::LINK()),
            ));
        }
        // interaction mode (color-coded; normal is dim, others pop)
        let mode = self.permissions.mode();
        let (mode_text, mode_color) = match mode {
            Mode::Normal => ("normal", Palette::DIM()),
            Mode::AutoAccept => ("auto-accept", Palette::OK()),
            Mode::Plan => ("plan", Palette::WARN()),
        };
        spans.push(sep());
        spans.push(Span::styled(mode_text, Style::default().fg(mode_color)));

        // LSP health: one colored dot + name per configured server. Starting is
        // dim, Indexing amber (with % when known), Ready green, Failed red.
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
        f.render_widget(Paragraph::new(Line::from(spans)), bar);

        // Right-aligned key hints: team drawer (only when agents exist) + the
        // todo-panel toggle. Kept terse so they don't crowd the status info.
        let hint = Style::default().fg(Palette::FAINT());
        let mut hints: Vec<Span> = Vec::new();
        if !self.teams.is_empty() {
            hints.push(Span::styled("^T team", hint));
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
        if !hints.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(hints)).alignment(ratatui::layout::Alignment::Right),
                bar,
            );
        }
    }

    fn draw_input(&mut self, f: &mut ratatui::Frame, area: Rect) {
        // Full-width band with a lighter background; no border. One blank row of
        // padding above (with status) and below; the middle grows with lines.
        let bg = Block::default().style(Style::default().bg(Palette::INPUT_BG()));
        f.render_widget(bg, area);

        let busy = self.running || self.view.busy;
        // Status sits on the top padding row, right-aligned. While a turn runs, hint
        // that Enter queues and Alt+Enter steers — the queued chips themselves show
        // in their own panel above.
        let status_line = if busy {
            Line::from(Span::styled(
                "enter queues · alt+enter steers ",
                Style::default().fg(Palette::FAINT()),
            ))
        } else {
            Line::from("")
        };
        // Horizontal breathing room inside the input band.
        let status_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width.saturating_sub(INPUT_PAD),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(status_line)
                .alignment(ratatui::layout::Alignment::Right)
                .style(Style::default().bg(Palette::INPUT_BG())),
            status_area,
        );

        // Text area: rows between the top and bottom pad rows, inset by INPUT_PAD.
        let text_rows = area.height.saturating_sub(2).max(1);
        let text_area = Rect {
            x: area.x + INPUT_PAD,
            y: area.y + 1,
            width: area.width.saturating_sub(INPUT_PAD * 2),
            height: text_rows,
        };

        // Wrapped, prompt-prefixed lines (same builder + width as the height calc).
        // Show the tail if the content is taller than the visible rows. Each line
        // is padded to the full text width so the input background fills edge to
        // edge (an unpadded line leaves the tail of the row the terminal's bg).
        let all = self.input_lines(text_area.width as usize, busy);
        let total_rows = all.len();
        let mut visible: Vec<Line> = if all.len() > text_rows as usize {
            all[all.len() - text_rows as usize..].to_vec()
        } else {
            all
        };
        let ibg = Style::default().bg(Palette::INPUT_BG());
        for line in &mut visible {
            let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            let pad = (text_area.width as usize).saturating_sub(used);
            if pad > 0 {
                line.spans.push(Span::styled(" ".repeat(pad), ibg));
            }
        }
        f.render_widget(Paragraph::new(visible).style(ibg), text_area);

        // Cursor position from the SAME wrap the renderer used, so they agree.
        // `wrapped` gives (row, col) in content coordinates; add the 2-col prefix
        // for x, and shift the row up by the tail-scroll offset applied above.
        const PREFIX: u16 = 2;
        let content_width = (text_area.width as usize).saturating_sub(PREFIX as usize);
        let (_, cur_row, cur_col) = self.input.wrapped(content_width);
        let scrolled = total_rows.saturating_sub(text_rows as usize);
        let vis_row = cur_row.saturating_sub(scrolled) as u16;
        let cursor_x = text_area.x + PREFIX + cur_col as u16;
        if vis_row < text_rows && cursor_x < text_area.x + text_area.width {
            f.set_cursor_position((cursor_x, text_area.y + vis_row));
        }
    }

    fn draw_menu(&mut self, f: &mut ratatui::Frame, input_area: Rect) {
        // Borderless select, matching the permission prompt. Sits directly above
        // the input band; no box, no background fill.
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
        // Fill with the theme popup background so no terminal-default color shows
        // through under a forced-background theme.
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

    /// The `@file` completion popup — same borderless style as the slash menu.
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

    /// Build all lines for the permission prompt: title, optional preview diff,
    /// numbered options, and the hint. Shared by height calc + render.
    fn permission_lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(p) = &self.pending_perm else {
            return vec![];
        };
        let mut lines: Vec<Line> = Vec::new();

        // Title line, e.g. "Allow write_file?" with the target dimmed after it.
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

        // Preview: render the ```diff / ```lang block the tool produced. Capped
        // so a huge edit doesn't push the options off-screen.
        if let Some(preview) = &p.preview {
            let rendered = render::render_markdown_like(preview);
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
            let selected = i == p.selected;
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
        lines.push(Line::from(Span::styled(
            "↑↓ move · 1-9 pick · enter confirm · esc deny",
            Style::default().fg(Palette::FAINT()),
        )));
        lines
    }

    fn draw_permission(&mut self, f: &mut ratatui::Frame, area: Rect) {
        if self.pending_perm.is_none() {
            return;
        }
        f.render_widget(Clear, area);
        let lines = self.permission_lines(area.width as usize);
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            area,
        );
    }

    /// Build the lines for a user question (ask_user / exit_plan): the question,
    /// optional Markdown detail (e.g. the plan), then a numbered select with an
    /// "Other…" row, or a free-text field when the user chose Other.
    fn query_lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(q) = &self.pending_query else {
            return vec![];
        };
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            q.query.title.clone(),
            Style::default()
                .fg(Palette::ACCENT())
                .add_modifier(Modifier::BOLD),
        )));
        if !q.query.detail.is_empty() {
            for l in render::render_markdown_like(&q.query.detail)
                .into_iter()
                .take(12)
            {
                lines.push(indent_line(l));
            }
        }
        lines.push(Line::from(""));

        // Free-text entry mode.
        if let Some(buf) = &q.other_text {
            lines.push(Line::from(vec![
                Span::styled(" > ", Style::default().fg(Palette::ACCENT())),
                Span::styled(buf.clone(), Style::default().fg(Palette::TEXT())),
                Span::styled("_", Style::default().fg(Palette::FAINT())),
            ]));
            lines.push(Line::from(Span::styled(
                "type your answer · enter to send · esc to go back",
                Style::default().fg(Palette::FAINT()),
            )));
            return lines;
        }

        // Build the full row list (options + optional "Other"), then window it
        // around the selection so a long list (e.g. 39 models) stays visible and
        // never hides the selected row behind the input.
        let n_opts = q.query.options.len();
        let total_rows = n_opts + if q.query.allow_other { 1 } else { 0 };
        let mut rows: Vec<(usize, String, bool)> = Vec::with_capacity(total_rows);
        for (i, opt) in q.query.options.iter().enumerate() {
            rows.push((i, opt.clone(), false));
        }
        if q.query.allow_other {
            rows.push((n_opts, "Other…".to_string(), true));
        }

        const VISIBLE: usize = 10;
        let start = if total_rows <= VISIBLE {
            0
        } else if q.selected < VISIBLE / 2 {
            0
        } else if q.selected >= total_rows - VISIBLE / 2 {
            total_rows - VISIBLE
        } else {
            q.selected - VISIBLE / 2
        };
        let end = (start + VISIBLE).min(total_rows);

        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("   ↑ {} more", start),
                Style::default().fg(Palette::FAINT()),
            )));
        }
        for (i, label, is_other) in &rows[start..end] {
            let selected = *i == q.selected;
            let marker = if selected { "❯" } else { " " };
            let base = if *is_other {
                Palette::DIM()
            } else {
                Palette::TEXT()
            };
            let style = if selected {
                Style::default().fg(base).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(base)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", marker),
                    Style::default().fg(Palette::ACCENT()),
                ),
                Span::styled(format!("{}. ", i + 1), Style::default().fg(Palette::DIM())),
                Span::styled(label.clone(), style),
            ]));
        }
        if end < total_rows {
            lines.push(Line::from(Span::styled(
                format!("   ↓ {} more", total_rows - end),
                Style::default().fg(Palette::FAINT()),
            )));
        }
        lines.push(Line::from(Span::styled(
            "↑↓ move · enter confirm · esc dismiss",
            Style::default().fg(Palette::FAINT()),
        )));
        let _ = width;
        lines
    }

    fn draw_query(&mut self, f: &mut ratatui::Frame, area: Rect) {
        if self.pending_query.is_none() {
            return;
        }
        f.render_widget(Clear, area);
        let lines = self.query_lines(area.width as usize);
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            area,
        );
    }

    fn draw_toast(&mut self, f: &mut ratatui::Frame, area: Rect, text: &str) {
        let w = (text.len() as u16 + 4).clamp(10, area.width);
        let toast = Rect {
            x: area.x + area.width.saturating_sub(w).saturating_sub(1),
            y: area.y + 1,
            width: w,
            height: 1,
        };
        f.render_widget(Clear, toast);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(" {} ", text),
                Style::default()
                    .fg(Palette::TEXT())
                    .bg(Palette::SELECTED_BG()),
            )),
            toast,
        );
    }
}
