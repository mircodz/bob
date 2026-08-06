//! All rendering for the TUI: the top-level `draw` and every `draw_*` /
//! `*_lines` helper. Split out of `mod.rs` to keep that file focused on the
//! app state + event loop. These are methods on `super::App`; because this is a
//! child module, they can access App's private fields directly.

use super::theme::Palette;
use super::widgets::{divider_col, inset, BAND_INSET};
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

/// Team-drawer roster column width, and the name budget inside it once the
/// `"  • "` dot chrome (~8 cols) is subtracted. Nesting eats 2 cols per depth.
const ROSTER_W: u16 = 24;
const ROSTER_NAME_W: usize = 16;

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
        // The team drawer is a full overlay above everything else; the full-screen
        // workflow view is another (they're mutually exclusive in practice).
        if self.team_drawer.is_some() {
            self.draw_team_drawer(f, area);
        } else {
            // Drop the stale roster hit-box so clicks don't select a hidden agent.
            self.roster_rect = None;
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
            Line::from(Span::styled(
                header,
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
            format!("background jobs · {} running", running),
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
    /// on the right, the selected agent's live transcript. Toggled with Ctrl+T.
    fn draw_team_drawer(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let Some(drawer) = self.team_drawer.as_ref() else {
            return;
        };
        // Snapshot the fields we need so we can later take a mutable borrow of
        // `self.team_drawer` to write the clamped scroll back without a conflict.
        let sel = drawer.list.selected;
        let hovered = drawer.hovered;
        let drawer_scroll = drawer.scroll;
        let compose_buf: Option<String> = drawer.composing.clone();
        let composing = compose_buf.is_some();
        // Chrome-free overlay: base background, one status dot per agent, selected
        // agent bold, whitespace separation (no boxes/borders/bars).
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

        // Roster on the left, a subtle vertical divider, then the transcript.
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(ROSTER_W),
                Constraint::Length(2), // divider column (│ + a space of breathing room)
                Constraint::Min(10),
            ])
            .split(outer[1]);
        // Remember the roster rect so left-clicks can select an agent. The roster
        // has one leading blank line, so agent `i` sits at row `body[0].y + 1 + i`.
        self.roster_rect = Some(body[0]);
        // Draw the faint divider down the middle column.
        {
            let dcol = body[1];
            f.render_widget(
                Paragraph::new(divider_col(dcol)).style(Style::default().bg(Palette::BG())),
                dcol,
            );
        }

        // Left: agent roster (running first, finished at the bottom & dimmed).
        // Window it so a large team scrolls to keep the selection visible instead of
        // overflowing the pane. One leading blank line, so the roster body is
        // `height - 1` rows; the resolved scroll is written back for the click map.
        let roster_h = (body[0].height as usize).saturating_sub(1);
        let range = {
            let mut list = super::widgets::SelectList {
                selected: sel,
                scroll: self
                    .team_drawer
                    .as_ref()
                    .map(|d| d.list.scroll)
                    .unwrap_or(0),
            };
            let r = list.window(order.len(), roster_h);
            if let Some(d) = self.team_drawer.as_mut() {
                d.list.scroll = list.scroll;
            }
            r
        };
        self.roster_scroll = range.start;
        let mut roster: Vec<Line> = vec![Line::from("")];
        for i in range.clone() {
            let id = &order[i];
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
            // bold so the agent under the cursor/selection stands out. FINISHED
            // agents are faint AND struck through so done work is unmistakable.
            let mut name_style = if selected || hover {
                Style::default()
                    .fg(Palette::TEXT())
                    .add_modifier(Modifier::BOLD)
            } else if finished {
                Style::default().fg(Palette::FAINT())
            } else {
                Style::default().fg(Palette::TEXT())
            };
            if finished {
                name_style = name_style.add_modifier(Modifier::CROSSED_OUT);
            }
            let depth = self.teams.depth_of(id);
            let indent = "  ".repeat(depth);
            let mut spans = vec![
                Span::styled(format!("  {}• ", indent), Style::default().fg(dot_color)),
                Span::styled(
                    truncate_mid(t.display_label(), ROSTER_NAME_W.saturating_sub(depth * 2)),
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
        // the main scrollback. Inset the pane horizontally so content has breathing
        // room on both sides (and doesn't hug the divider), and so the full-width
        // user-message band wraps/pads to the SAME width it's rendered at.
        let pane = inset(body[2], 1);
        let pane_w = pane.width as usize;
        let mut transcript: Vec<Line> = vec![Line::from("")];
        if let Some(id) = order.get(sel) {
            if let Some(t) = self.teams.get(id) {
                for cell in &t.cells {
                    render::render_cell(cell, pane_w, &mut transcript);
                }
            }
        }
        // Wrap long lines to the pane width so conversations don't get clipped at
        // the right edge (the same wrapping the main scrollback uses). Wrapping
        // BEFORE the scroll math keeps the offset counted in real rows.
        let transcript: Vec<Line> = transcript
            .into_iter()
            .flat_map(|l| super::wrap_line(l, pane_w))
            .collect();
        // Apply the drawer's scroll offset, clamping it and WRITING IT BACK so the
        // stored value can't run past the end. Otherwise a fast wheel flick keeps
        // incrementing `scroll` past max, and you have to scroll back down through
        // all that phantom overshoot before the view visibly moves ("runoff").
        let view_h = pane.height as usize;
        let max_scroll = transcript.len().saturating_sub(view_h);
        let scroll = (drawer_scroll as usize).min(max_scroll);
        if let Some(d) = self.team_drawer.as_mut() {
            d.scroll = scroll as u16;
        }
        let visible: Vec<Line> = transcript.into_iter().skip(scroll).collect();
        f.render_widget(
            Paragraph::new(visible).style(Style::default().bg(Palette::BG())),
            pane,
        );

        // Terse dim hint / compose line — no filled bar.
        let hint = if composing {
            let buf = compose_buf.as_deref().unwrap_or("");
            Line::from(vec![
                Span::styled("› ", Style::default().fg(Palette::DIM())),
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

    /// Full-screen workflow view — a single scrollable pane with a collapsible
    /// phase/agent tree. Phase headers (`▾ Map 4/4`) collapse/expand their agents;
    /// the selected agent expands INLINE to show its Prompt / Activity / Outcome.
    /// Chrome-free (no borders), full width so the detail reads well. ↑↓ move the
    /// cursor, Enter toggles (collapse a phase / expand an agent), Esc closes.
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

        // Flatten the tree into selectable rows (phase headers + agents, honoring
        // Split body: left = the collapsible tree, right = the selected agent's
        // detail. A faint ` │` divider separates them (chrome-free, like the team
        // drawer).
        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(42),
                Constraint::Length(2),
                Constraint::Min(20),
            ])
            .split(outer[1]);
        let tree_area = Rect {
            x: body[0].x + 1,
            y: body[0].y,
            width: body[0].width.saturating_sub(1),
            height: body[0].height,
        };
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
                    // Duration only in the tree (model·tokens live in the detail pane).
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

        // Divider — full height: spans the header rows down through the body (stops
        // above the hint line), so the two columns read as one continuous split.
        let dcol = Rect {
            x: body[1].x,
            y: area.y,
            width: body[1].width,
            height: outer[0].height + outer[1].height,
        };
        f.render_widget(
            Paragraph::new(divider_col(dcol)).style(Style::default().bg(Palette::BG())),
            dcol,
        );

        // Right pane: the selected agent's detail (Prompt / Activity / Outcome). A
        // phase-header row shows a short phase summary instead.
        // 1-col gap off the divider on the left, ~3 cols reserved on the right so
        // wrapped text doesn't hug the terminal edge (the body lines carry their own
        // 4-col indent, so we don't add more on the left).
        let detail = Rect {
            x: body[2].x + 1,
            y: body[2].y,
            width: body[2].width.saturating_sub(4),
            height: body[2].height,
        };
        let dw = detail.width as usize;
        let sel_agent = rows.get(sel).and_then(|r| match r {
            WfRow::Agent(pi, ai) => phases[*pi].agents.get(*ai),
            _ => None,
        });
        let mut dlines = detail_lines(self, sel_agent, dw);
        // Body text (Prompt/Outcome) is indented 4 cols; wrap with a matching
        // hanging indent so continuation rows stay aligned instead of hugging the
        // divider.
        let dlines: Vec<Line> = dlines
            .drain(..)
            .flat_map(|l| super::wrap_line_hanging(l, dw, 4))
            .collect();
        f.render_widget(
            Paragraph::new(dlines).style(Style::default().bg(Palette::BG())),
            detail,
        );

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "  ↑↓ move · enter collapse phase · esc close",
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

    /// The collapsible right info sidebar (lighter background). AGENTS section: a
    /// tree with "main" (the root conversation) plus each running agent, indented by
    /// spawn depth. The selected row is the focused conversation. Records screen-row
    /// → agent-id hit-boxes so clicks select. (LSP/MCP sections come later.)
    fn draw_sidebar(&mut self, f: &mut ratatui::Frame, area: Rect) {
        // Lighter panel background to set the sidebar apart (opencode-style).
        let bg = Style::default().bg(Palette::INPUT_BG());
        f.render_widget(Clear, area);
        f.render_widget(Block::default().style(bg), area);
        let pane = inset(area, 1);
        let w = pane.width as usize;

        let running = self.teams.running_ids();
        let mut lines: Vec<Line> = Vec::new();
        let mut hit: Vec<(u16, String)> = Vec::new();

        // Section header.
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  AGENTS · {} running", running.len()),
            bg.fg(Palette::DIM()),
        )));

        // Rows: index 0 = "main" (empty id), then the running agents.
        let render_row = |lines: &mut Vec<Line>,
                          hit: &mut Vec<(u16, String)>,
                          idx: usize,
                          id: &str,
                          label: &str,
                          depth: usize,
                          meta: &str| {
            let is_sel = idx == self.sidebar.selected;
            let indent = "  ".repeat(depth);
            // No chevron — the SELECTED (current) agent is just bold.
            let name_style = if is_sel {
                bg.fg(Palette::TEXT()).add_modifier(Modifier::BOLD)
            } else {
                bg.fg(Palette::TEXT())
            };
            let left = format!("  {indent}• {label}");
            let pad = w.saturating_sub(left.chars().count() + meta.chars().count());
            let row = pane.y + lines.len() as u16;
            if row < pane.y + pane.height {
                hit.push((row, id.to_string()));
            }
            lines.push(Line::from(vec![
                Span::styled(format!("  {indent}• "), bg.fg(Palette::RUNNING())),
                Span::styled(label.to_string(), name_style),
                Span::styled(" ".repeat(pad.max(1)), bg),
                Span::styled(meta.to_string(), bg.fg(Palette::DIM())),
            ]));
        };

        render_row(&mut lines, &mut hit, 0, "", "main", 0, "");
        for (i, id) in running.iter().enumerate() {
            let (label, meta) = self
                .teams
                .get(id)
                .map(|t| (t.name.clone(), String::new()))
                .unwrap_or_else(|| (id.clone(), String::new()));
            let depth = self.teams.depth_of(id) + 1;
            render_row(&mut lines, &mut hit, i + 1, id, &label, depth, &meta);
        }

        self.sidebar_rows = Some(hit);

        // Footer hint.
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  ↑↓ select · esc main · ⌃g close",
            bg.fg(Palette::FAINT()),
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

/// The right-aligned metadata string for an agent row: model · tokens · duration,
/// omitting parts not known yet.
fn agent_meta(a: &super::view::WfAgent) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(m) = &a.model {
        parts.push(m.clone());
    }
    if a.tokens > 0 {
        parts.push(format!("{} tok", super::fmt_tokens(a.tokens)));
    }
    match a.status {
        super::view::WfStatus::Running => parts.push(format!("{} tools", a.tools)),
        _ => {
            if let Some(d) = a.duration_secs {
                parts.push(super::fmt_duration(d));
            }
        }
    }
    parts.join(" · ")
}

/// Build the Detail-pane lines for one workflow agent: a status/meta line, then its
/// Prompt / Activity / Outcome distilled from its team-drawer transcript cells.
fn detail_lines(
    app: &App,
    agent: Option<&super::view::WfAgent>,
    _width: usize,
) -> Vec<Line<'static>> {
    use super::view::{Cell, WfStatus};
    let mut out: Vec<Line> = vec![Line::from("")];
    let Some(agent) = agent else {
        out.push(Line::from(Span::styled(
            "  (no agent selected)",
            Style::default().fg(Palette::DIM()),
        )));
        return out;
    };

    // Header: label + status + meta.
    let status_word = match agent.status {
        WfStatus::Running => "running",
        WfStatus::Done => "done",
        WfStatus::Failed => "failed",
    };
    out.push(Line::from(Span::styled(
        format!("  {}", agent.label),
        Style::default()
            .fg(Palette::TEXT())
            .add_modifier(Modifier::BOLD),
    )));
    out.push(Line::from(vec![
        Span::raw("  "),
        render::wf_dot(agent.status),
        Span::styled(
            format!(" {} · {}", status_word, agent_meta(agent)),
            Style::default().fg(Palette::DIM()),
        ),
    ]));

    // The agent's transcript (Prompt/Activity/Outcome) lives in the team store.
    let thread = app.teams.get(&agent.agent_id);
    let Some(thread) = thread else {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            "  (transcript not captured)",
            Style::default().fg(Palette::FAINT()),
        )));
        return out;
    };

    let section = |out: &mut Vec<Line>, name: &str| {
        out.push(Line::from(""));
        out.push(Line::from(Span::styled(
            format!("  {name}"),
            Style::default()
                .fg(Palette::ACCENT())
                .add_modifier(Modifier::BOLD),
        )));
    };

    // Prompt: the first User cell (the delegated instructions).
    if let Some(Cell::User(text)) = thread.cells.iter().find(|c| matches!(c, Cell::User(_))) {
        section(&mut out, "Prompt");
        for l in text.lines().take(12) {
            out.push(Line::from(Span::styled(
                format!("    {l}"),
                Style::default().fg(Palette::TEXT()),
            )));
        }
    }

    // Activity: the tool calls, as `Name(arg)`.
    let tools: Vec<&Cell> = thread
        .cells
        .iter()
        .filter(|c| matches!(c, Cell::Tool { .. }))
        .collect();
    if !tools.is_empty() {
        section(&mut out, "Activity");
        for c in tools {
            if let Cell::Tool { name, input, .. } = c {
                let arg = input
                    .get("path")
                    .or_else(|| input.get("pattern"))
                    .or_else(|| input.get("command"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                out.push(Line::from(Span::styled(
                    format!("    {name}({arg})"),
                    Style::default().fg(Palette::DIM()),
                )));
            }
        }
    }

    // Outcome: the last assistant cell (the agent's final answer / structured out).
    if let Some(Cell::Assistant { text, .. }) = thread
        .cells
        .iter()
        .rev()
        .find(|c| matches!(c, Cell::Assistant { .. }))
    {
        section(&mut out, "Outcome");
        for l in text.lines().take(20) {
            out.push(Line::from(Span::styled(
                format!("    {l}"),
                Style::default().fg(Palette::TEXT()),
            )));
        }
    }

    out
}
