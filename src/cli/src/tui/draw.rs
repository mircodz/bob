//! All rendering for the TUI: the top-level `draw` and every `draw_*` /
//! `*_lines` helper. Split out of `mod.rs` to keep that file focused on the
//! app state + event loop. These are methods on `super::App`; because this is a
//! child module, they can access App's private fields directly.

use super::theme::Palette;
use super::{indent_line, render, shimmer_spans, theme, truncate_mid, view, wrap_line, App};
use bob_core::core::permissions::Mode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

impl App {
    /// Build the fully wrapped, prompt-prefixed display lines for the input box,
    /// given the usable text width. Used for BOTH the height calc and rendering
    /// so they never disagree (which is what clipped long lines).
    fn input_lines(&self, width: usize, busy: bool) -> Vec<Line<'static>> {
        let prompt = || {
            Span::styled(
                "› ",
                Style::default()
                    .fg(Palette::ACCENT())
                    .add_modifier(Modifier::BOLD),
            )
        };
        if self.input.text().is_empty() && !busy {
            return vec![Line::from(vec![
                prompt(),
                Span::styled(
                    "send a message...  (Ctrl+J or Shift+Enter for newline)",
                    Style::default().fg(Palette::FAINT()),
                ),
            ])];
        }
        let mut out: Vec<Line<'static>> = Vec::new();
        for (i, l) in self.input.display_lines().iter().enumerate() {
            let prefix = if i == 0 {
                prompt()
            } else {
                Span::styled("  ", Style::default())
            };
            let logical = Line::from(vec![
                prefix,
                Span::styled(l.to_string(), Style::default().fg(Palette::TEXT())),
            ]);
            // Wrap each logical line so long content (pasted code) never clips.
            for wl in wrap_line(logical, width.max(1)) {
                out.push(wl);
            }
        }
        out
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
        // eat the whole screen.
        let text_width = area.width.saturating_sub(2) as usize;
        let wrapped = self
            .input_lines(text_width, self.running || self.view.busy)
            .len();
        let text_rows = (wrapped as u16).clamp(1, 10);
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
        let todo_items = self.todos.as_ref().map(|t| t.items()).unwrap_or_default();
        let todos_height = if todo_items.is_empty() {
            0
        } else {
            // header + one blank line of padding above and below.
            (todo_items.len() as u16 + 3).min(14)
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(prompt_height),
                Constraint::Length(todos_height),
                Constraint::Length(jobs_height),
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
        self.draw_input(f, chunks[4]);
        self.draw_status_bar(f, chunks[5]);

        if !self.menu.is_empty() {
            self.draw_menu(f, chunks[4]);
        }
        if !self.file_menu.is_empty() {
            self.draw_file_menu(f, chunks[4]);
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

    fn draw_scrollback(&mut self, f: &mut ratatui::Frame, full: Rect) {
        // Content is inset with a small left/right margin, EXCEPT the user-message
        // band which spans full width (rendered against `full.width`). Non-user
        // cells get the inset via a left pad inside their lines... simplest: we
        // render at full width and inset all lines that aren't the user band.
        let area = full;

        let mut raw: Vec<Line> = Vec::new();
        // Parallel to `raw`: whether each line should get the 2-col non-user
        // inset (applied after wrapping so continuation lines stay aligned).
        let mut inset_flags: Vec<bool> = Vec::new();
        let width_full = full.width as usize;
        let theme_gen = theme::generation();
        let cells = &self.view.cells;
        // Keep the cache index-aligned with the cell list.
        if self.render_cache.len() != cells.len() {
            self.render_cache.resize(cells.len(), (0, Vec::new()));
        }
        for (i, cell) in cells.iter().enumerate() {
            let is_user = matches!(cell, view::Cell::User(_));

            // Cache key: content + width + theme generation. A hit means this cell
            // renders identically to last frame, so we reuse its Lines and skip
            // markdown/syntax-highlighting entirely.
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                cell.fingerprint().hash(&mut h);
                width_full.hash(&mut h);
                theme_gen.hash(&mut h);
                h.finish()
            };
            let slot = &mut self.render_cache[i];
            if slot.0 != key {
                let mut rendered = Vec::new();
                render::render_cell(cell, width_full, &mut rendered);
                *slot = (key, rendered);
            }
            for line in &slot.1 {
                raw.push(line.clone());
                inset_flags.push(!is_user);
            }
        }

        // While a turn runs, append a live "Working" line: a
        // shimmering label with elapsed seconds and the interrupt hint. It's not
        // a stored cell — it's transient, regenerated each frame.
        if self.running || self.view.busy {
            let secs = self
                .turn_started
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let mut spans: Vec<Span> = vec![Span::styled(
                "  • ",
                Style::default().fg(Palette::RUNNING()),
            )];
            spans.extend(shimmer_spans("Working", self.spinner));
            spans.push(Span::styled(
                format!(" ({}s · esc to interrupt)", secs),
                Style::default().fg(Palette::FAINT()),
            ));
            raw.push(Line::from(spans));
            inset_flags.push(false); // already carries its own leading spaces
        }

        // Manually wrap into display lines so scroll math is exact and wide
        // content (tables/code) is clipped rather than reflowed mid-border.
        // Non-user lines get a 2-col hanging indent: we wrap at width-2 then
        // prefix EVERY wrapped line (including continuations) with the pad, so
        // wrapped text stays aligned under its first line.
        let width = area.width.max(1) as usize;
        let mut lines: Vec<Line> = Vec::new();
        for (idx, l) in raw.into_iter().enumerate() {
            let inset = inset_flags.get(idx).copied().unwrap_or(false);
            let wrap_width = if inset {
                width.saturating_sub(2).max(1)
            } else {
                width
            };
            for mut wl in wrap_line(l, wrap_width) {
                if inset {
                    wl.spans.insert(0, Span::raw("  "));
                }
                lines.push(wl);
            }
        }

        let viewport = area.height as usize;
        let total = lines.len();
        let max_scroll = total.saturating_sub(viewport);
        self.scroll_up = self.scroll_up.min(max_scroll as u16);
        let start = max_scroll.saturating_sub(self.scroll_up as usize);
        let window: Vec<Line> = lines.into_iter().skip(start).take(viewport).collect();

        f.render_widget(Paragraph::new(window), area);

        // Scroll hint when not at the bottom.
        if self.scroll_up > 0 {
            let hint = Rect {
                x: area.x + area.width.saturating_sub(10),
                y: area.y,
                width: 10,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    " ↑ scrolled ",
                    Style::default().fg(Palette::WARN()).bg(Palette::POPUP_BG()),
                )),
                hint,
            );
        }
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
    }

    fn draw_input(&mut self, f: &mut ratatui::Frame, area: Rect) {
        // Full-width band with a lighter background; no border. One blank row of
        // padding above (with status) and below; the middle grows with lines.
        let bg = Block::default().style(Style::default().bg(Palette::INPUT_BG()));
        f.render_widget(bg, area);

        let busy = self.running || self.view.busy;
        // Status sits on the top padding row, right-aligned. Mode is shown in the
        // status bar below the input; the top row only notes queued prompts while
        // a turn is running.
        let status_line = if busy && !self.queue.is_empty() {
            Line::from(Span::styled(
                format!("{} queued ", self.queue.len()),
                Style::default().fg(Palette::DIM()),
            ))
        } else {
            Line::from("")
        };
        // Horizontal breathing room inside the input band.
        const PAD: u16 = 3;
        let status_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width.saturating_sub(PAD),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(status_line)
                .alignment(ratatui::layout::Alignment::Right)
                .style(Style::default().bg(Palette::INPUT_BG())),
            status_area,
        );

        // Text area: rows between the top and bottom pad rows, inset by PAD.
        let text_rows = area.height.saturating_sub(2).max(1);
        let text_area = Rect {
            x: area.x + PAD,
            y: area.y + 1,
            width: area.width.saturating_sub(PAD * 2),
            height: text_rows,
        };

        // Wrapped, prompt-prefixed lines (same builder as the height calc). Show
        // the tail if the content is taller than the visible rows.
        let all = self.input_lines(text_area.width as usize, busy);
        let visible: Vec<Line> = if all.len() > text_rows as usize {
            all[all.len() - text_rows as usize..].to_vec()
        } else {
            all
        };
        f.render_widget(
            Paragraph::new(visible).style(Style::default().bg(Palette::INPUT_BG())),
            text_area,
        );

        // Cursor at its row/col (accounting for the 2-col prompt/indent prefix).
        let (row, col) = self.input.cursor_row_col();
        let row = row.min(text_rows.saturating_sub(1) as usize) as u16;
        let cursor_x = text_area.x + 2 + col as u16;
        if cursor_x < text_area.x + text_area.width {
            f.set_cursor_position((cursor_x, text_area.y + row));
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
