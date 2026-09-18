//! All rendering for the TUI: the top-level `draw` and every `draw_*` /
//! `*_lines` helper. Split out of `mod.rs` to keep that file focused on the
//! app state + event loop. These are methods on `super::App`; because this is a
//! child module, they can access App's private fields directly.

use super::theme::Palette;
use super::widgets::{inset, BAND_INSET};
use super::{indent_line, render, team, truncate_mid, App};
use bob_core::core::permissions::Mode;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

/// Horizontal breathing room INSIDE the input band (columns per side), on top of
/// the band's own 2-col float. Kept at 1 so the input text aligns with the floating
/// user-message bubble in the transcript (band inset 2 + this).
const INPUT_PAD: u16 = 1;

/// Width of the collapsible info sidebar (agents/LSP/MCP), in columns.
const SIDEBAR_W: u16 = 44;

impl App {
    /// Build the wrapped, prompt-prefixed display lines for the input box, given
    /// the usable text width. Used for BOTH the height calc and rendering so they
    /// never disagree. The first row carries a `›` marker, wrapped/continuation
    /// rows a 2-space indent, so text stays aligned under the marker.
    fn input_lines(&self, width: usize, busy: bool) -> Vec<Line<'static>> {
        // Marker convention: `›` (DIM) = a text-input/prompt line; `❯` (WARN) = the
        // selected row of a list/menu. Don't cross them.
        // Marker + continuation indent are both 2 cols, so content wraps at width-2.
        const PREFIX: usize = 2;
        let text_color = Style::default().fg(Palette::TEXT());
        let marker = || Span::styled("› ", Style::default().fg(Palette::DIM()));
        let indent = || Span::styled("  ", Style::default());

        if self.input.text().is_empty() && !busy {
            // A focused subagent conversation looks IDENTICAL to root — the sidebar
            // bold is the only indicator of which agent you're in.
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
        // Force the theme's base background across the whole screen so bob looks
        // identical regardless of the terminal's own background. Themes that want
        // to inherit the terminal use Color::Reset here (a no-op paint).
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        // Input band height = 1 pad + wrapped text rows + 1 pad, capped. Compute the
        // wrap width with the SAME inset the renderer uses (BAND_INSET + INPUT_PAD),
        // or the height won't match the line count. Narrower when the sidebar is open.
        let content_w = if self.sidebar_open {
            area.width.saturating_sub(SIDEBAR_W)
        } else {
            area.width
        };
        let text_width = content_w.saturating_sub(BAND_INSET * 2 + INPUT_PAD * 2) as usize;
        let wrapped = self
            .input_lines(text_width, self.running || self.view.busy)
            .len();
        let text_rows = (wrapped as u16).clamp(1, 12);
        let input_height = text_rows + 2;

        // The band above the input shows either a permission prompt or a user
        // question (they don't co-occur), sized to its content.
        let prompt_height = if !self.perm_queue.is_empty() {
            // Count lines at the SAME padded width the renderer uses (BAND_INSET each
            // side), +2 for the top padding row and a bottom breathing row.
            let inner_w = area.width.saturating_sub(BAND_INSET * 2) as usize;
            (self.permission_lines(inner_w).len() as u16 + 2).min(24)
        } else if self.pending_query.is_some() {
            let inner_w = area.width.saturating_sub(BAND_INSET * 2) as usize;
            (self.query_lines(inner_w).len() as u16 + 1).min(24)
        } else {
            0
        };

        // Only background shell commands belong in the pinned jobs panel.
        let job_rows = shell_jobs(&self.jobs);
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

        // When the info sidebar is open, carve a FULL-HEIGHT column off the right of
        // the screen first (top → bottom), and lay everything else out in the left
        // column. Collapsed → the content uses the whole width.
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

        if !self.menu.is_empty() {
            self.draw_menu(f, input_area);
        }
        if !self.file_menu.is_empty() {
            self.draw_file_menu(f, input_area);
        }
        if self.workflow_view.is_some() {
            self.draw_workflow_view(f, area);
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

    /// Pinned "queued messages" panel: the messages waiting to be sent after the
    /// current turn, shown as chips above the input (not in the transcript). The
    /// last chip can be popped back for editing with Backspace on an empty prompt.
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
    /// Full-screen workflow view — a single scrollable pane with a collapsible
    /// phase/agent tree. Phase headers (`▾ Map 4/4`) collapse/expand their agents;
    /// Enter on an agent opens its transcript in the main conversation.
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

        // Header: everything on ONE line — title · N/N agents · time · status. The
        // status reads "cancelled" when the shared cancel flag is set (an
        // interrupted run whose agents are winding down).
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

        // Flatten the tree into selectable rows (phase headers + agents, honoring
        // collapse). The cursor `sel` indexes into this list.
        let rows = workflow_rows(phases, &collapsed);

        // Build one line per row.
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
                    // Keep the workflow tree compact with a duration or tool count.
                    let dur = a
                        .duration_secs
                        .map(super::fmt_duration)
                        .unwrap_or_else(|| format!("{} tools", a.tools));
                    // Fixed left chrome: "    ├─ • " = 4 + 2 + 1 + 1 + 1 = 9 cols.
                    const LEFT_CHROME: usize = 9;
                    // Reserve room for the duration + a 2-col gap; truncate the label
                    // to whatever's left so nothing overflows into the divider and the
                    // duration always lands flush-right.
                    let dur_w = dur.chars().count();
                    let avail_label = tree_w.saturating_sub(LEFT_CHROME + dur_w + 2).max(4);
                    let label = truncate_mid(&a.label, avail_label);
                    let used = LEFT_CHROME + label.chars().count() + dur_w;
                    let pad = tree_w.saturating_sub(used).max(1);
                    // A still-running agent in a cancelled run is winding down → dim
                    // grey dot (not the red "failed" it will momentarily report).
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

        // Scroll + hit-test via the shared SelectList math (rows == lines here, so
        // the cursor index maps 1:1 to a line). Seed it from the view's state, run
        // the window, write the resolved scroll back.
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

        // Click hit-boxes: screen row → agent id (agent rows only).
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
        // When focused on a subagent, the left pane shows THAT agent's transcript
        // (rendered directly from its captured thread) instead of the root
        // conversation. Selecting "main" clears focus and restores the root view.
        if let Some(id) = self.focused_agent.clone() {
            self.draw_focused_agent(f, full, &id);
            return;
        }
        let working = self.running || self.view.busy;
        let secs = self
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
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

    /// Render a focused agent's transcript in the main pane — laid out exactly like
    /// the root conversation (same side padding, `render_cell`), so a subagent chat
    /// is indistinguishable from root. The sidebar's bold row is the only indicator
    /// of which agent you're in.
    fn draw_focused_agent(&mut self, f: &mut ratatui::Frame, full: Rect, id: &str) {
        // Render the subagent transcript through the SAME ScrollbackRenderer the
        // root conversation uses, so padding, wrapping, caching, and scrolling are
        // identical by construction (not a hand-rolled copy that drifts). An empty
        // slice + a bump-on-miss revision cleanly handles the no-transcript case.
        let (cells, revision): (&[super::view::Cell], u64) = match self.teams.get(id) {
            Some(t) => (&t.cells, t.revision),
            None => (&[], 0),
        };
        let working = self.running || self.view.busy;
        let secs = self
            .turn_started
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        self.focused_scrollback
            .render(f, full, cells, revision, working, self.spinner, secs);
    }

    /// The sidebar lists all agents, active first, and windows around selection.
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
        // interaction mode (color-coded; normal is dim, others pop). YOLO
        // overrides the label with a loud red badge — a bypass-all state must be
        // impossible to miss.
        let (mode_text, mode_color) = if self.permissions.bypass() {
            ("YOLO", Palette::ERROR())
        } else {
            match self.permissions.mode() {
                Mode::Normal => ("normal", Palette::DIM()),
                Mode::AutoAccept => ("auto-accept", Palette::OK()),
                Mode::Plan => ("plan", Palette::WARN()),
            }
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

        // Right-aligned key hints: agents sidebar (only when agents exist) + the
        // todo-panel toggle. Kept terse so they don't crowd the status info.
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
        if !hints.is_empty() {
            f.render_widget(
                Paragraph::new(Line::from(hints)).alignment(ratatui::layout::Alignment::Right),
                bar,
            );
        }
    }

    fn draw_input(&mut self, f: &mut ratatui::Frame, area: Rect) {
        // Float the band to the SAME left edge as the transcript's user bubble and
        // every panel above the input: BAND_INSET (SIDE_PAD + BAND_MARGIN) columns
        // in. The input reads as the newest message in the conversation's column.
        let area = inset(area, BAND_INSET);
        // Full-width band with a lighter background; no border. One blank row of
        // padding above (with status) and below; the middle grows with lines.
        let bg = Block::default().style(Style::default().bg(Palette::INPUT_BG()));
        f.render_widget(bg, area);

        let busy = self.running || self.view.busy;
        // The top padding row stays blank — the queued-input chips in their own
        // panel above already convey that Enter queues / Alt+Enter steers.
        let status_line = Line::from("");
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

        // Show the tail when content exceeds the visible rows. Each line is padded
        // to the full text width so the band background fills edge-to-edge (an
        // unpadded row leaves its tail the terminal's own bg).
        let all = self.input_lines(text_area.width as usize, busy);
        let total_rows = all.len();
        let mut visible: Vec<Line> = if all.len() > text_rows as usize {
            all[all.len() - text_rows as usize..].to_vec()
        } else {
            all
        };
        let ibg = Style::default().bg(Palette::INPUT_BG());
        for line in &mut visible {
            // Pad to the band width in DISPLAY columns (wide glyphs count as 2), so
            // the input background fills edge-to-edge even with CJK/emoji text.
            let used = line.width();
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
        let Some(p) = self.perm_queue.front() else {
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
        // Base hint; when other prompts are waiting behind this one, show a counter
        // so it's clear more approvals are queued (e.g. a workflow fan-out).
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
        // Paint the panel background across the whole band, then render the prompt
        // into a padded inner rect (a column of margin each side + a blank top row)
        // so the prompt doesn't hug the edges.
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        // Align the prompt's left edge to the shared band column (BAND_INSET), the
        // same edge as the input box + user bubbles, with a blank top row.
        let inner = Rect {
            x: area.x + BAND_INSET,
            y: area.y + 1,
            width: area.width.saturating_sub(BAND_INSET * 2),
            height: area.height.saturating_sub(1),
        };
        let lines = self.permission_lines(inner.width as usize);
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            inner,
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
            for l in render::render_markdown_snippet(&q.query.detail)
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
                Span::styled("› ", Style::default().fg(Palette::DIM())),
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

        // Window the rows around the selection with the shared SelectList math
        // (keep-visible), so a long list (e.g. 39 models) stays on screen and never
        // hides the selected row behind the input. A local list seeded from the
        // stored cursor + scroll keeps the offset stable across redraws.
        const VISIBLE: usize = 10;
        let mut list = super::widgets::SelectList {
            selected: q.list.selected,
            scroll: q.list.scroll,
        };
        let range = list.window(total_rows, VISIBLE);
        let (start, end) = (range.start, range.end);

        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("   ↑ {} more", start),
                Style::default().fg(Palette::FAINT()),
            )));
        }
        // Model-picker rows are "id\t<ctx label>"; align the id column across all
        // rows so the context-window column lines up, and right-align the ctx
        // labels so their units stack. id in the row color, window in dim accent.
        let split_rows: Vec<Option<(&str, &str)>> =
            rows.iter().map(|(_, l, _)| l.split_once('\t')).collect();
        let id_col_w = split_rows
            .iter()
            .filter_map(|s| s.map(|(id, _)| id.chars().count()))
            .max()
            .unwrap_or(0);
        let ctx_col_w = split_rows
            .iter()
            .filter_map(|s| s.map(|(_, ctx)| ctx.chars().count()))
            .max()
            .unwrap_or(0);

        // Width of the widest 1-based index, so "5." and "10." both start the id
        // column at the same offset (otherwise a 2-digit number shifts the row).
        let num_w = format!("{}", total_rows).chars().count();

        for (row_idx, (i, label, is_other)) in rows[start..end].iter().enumerate() {
            let selected = *i == q.list.selected;
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
            let mut spans = vec![
                Span::styled(
                    format!(" {} ", marker),
                    Style::default().fg(Palette::ACCENT()),
                ),
                Span::styled(
                    format!("{:>width$}. ", i + 1, width = num_w),
                    Style::default().fg(Palette::DIM()),
                ),
            ];
            if let Some((id, ctx)) = split_rows[start + row_idx] {
                let id_pad = id_col_w.saturating_sub(id.chars().count());
                let ctx_pad = ctx_col_w.saturating_sub(ctx.chars().count());
                spans.push(Span::styled(id.to_string(), style));
                // Gap between columns + left-pad so ctx labels are right-aligned.
                spans.push(Span::raw(" ".repeat(id_pad + 2 + ctx_pad)));
                spans.push(Span::styled(
                    ctx.to_string(),
                    Style::default()
                        .fg(Palette::ACCENT())
                        .add_modifier(Modifier::DIM),
                ));
            } else {
                spans.push(Span::styled(label.clone(), style));
            }
            lines.push(Line::from(spans));
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
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        // Align the query to the shared band column, like the permission prompt.
        let inner = inset(area, BAND_INSET);
        let lines = self.query_lines(inner.width as usize);
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

// --- workflow-view helpers -------------------------------------------------

/// A selectable row in the collapsible workflow tree: a phase header, or an agent
/// (with its phase + agent indices). Shared by the draw + the key/click handlers so
/// the cursor maps to the same rows both see.
#[derive(Clone, Copy)]
pub(super) enum WfRow {
    Phase(usize),
    Agent(usize, usize),
}

/// Flatten the phase/agent tree into an ordered list of selectable rows, honoring
/// which phases are `collapsed` (their agents are hidden).
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

/// Aggregate status of a phase from its agents: Failed if any failed, Running if
/// any still running, else Done.
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
    use super::super::tests::test_app;
    use super::super::{KeyCode, KeyModifiers};
    use super::*;
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
        app.on_terminal_event(mouse(MouseEventKind::Moved, 10, 5), &mut dirty);
        assert!(dirty);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_ne!(&bottom, terminal.backend().buffer());
        assert_matches_fresh_frame(&mut app, &terminal);

        for (key, open) in [
            ('t', true),
            ('g', true),
            ('t', false),
            ('g', false),
            ('t', true),
            ('t', false),
        ] {
            dirty = false;
            app.on_terminal_event(
                CtEvent::Key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL)),
                &mut dirty,
            );
            app.on_terminal_event(mouse(MouseEventKind::Moved, 10, 5), &mut dirty);
            assert!(dirty);
            assert_eq!(app.sidebar_open, open);
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
            dirty = false;
            terminal.backend_mut().resize(width, height);
            app.on_terminal_event(CtEvent::Resize(width, height), &mut dirty);
            app.on_terminal_event(mouse(MouseEventKind::Moved, 10, 5), &mut dirty);
            assert!(dirty);
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
                output: "hello\n".repeat(20),
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
    fn connection_events_use_full_contrast_text() {
        let mut app = test_app();
        let notice = "MCP 'github': 3 tool(s)";
        app.view.push_event(notice.into());
        let mut lines = Vec::new();
        render::render_cell(&app.view.cells[0], 80, &mut lines);
        assert!(lines
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content.contains(notice) && span.style.fg == Some(Palette::TEXT())));
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
        let row = sidebar_row(
            &"界".repeat(60),
            16,
            thread_color(ThreadStatus::Cancelled),
            true,
            false,
            42,
        );
        assert_eq!(row.width(), 42);
        assert!(!row.to_string().contains("Cancelled"));
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
}
