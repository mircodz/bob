//! Turn view-model cells into ratatui Lines for the scrollback viewport.

use super::diffview::{diff_header, render_diff};
use super::highlight::highlight_line;
use super::indent_line;
use super::markdown::render_markdown;
pub(super) use super::markdown::render_markdown as render_markdown_snippet;
use super::theme::Palette;
use super::view::{Cell, ToolStatus, WfPhase, WfStatus};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// Pretty display name for a tool + its most salient argument.
/// write_file {path:"a.py"} → ("Write", "a.py")
fn tool_display(name: &str, input: &Value) -> (String, String) {
    let arg = |k: &str| {
        input
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    match name {
        "read_file" => ("Read".into(), arg("path")),
        "write_file" => ("Write".into(), arg("path")),
        "edit_file" => ("Edit".into(), arg("path")),
        "multi_edit" => ("Edit".into(), arg("path")),
        "list_dir" => ("List".into(), arg("path")),
        "glob" => ("Glob".into(), arg("pattern")),
        "grep" => ("Grep".into(), arg("pattern")),
        "bash" => ("Bash".into(), arg("command")),
        "web_fetch" => ("Fetch".into(), arg("url")),
        "web_search" => ("Search".into(), arg("query")),
        "todo_write" => ("Plan".into(), String::new()),
        "memory" => ("Memory".into(), arg("content")),
        "task" => ("Task".into(), String::new()),
        "workflow" => ("Workflow".into(), arg("title")),
        "enter_plan" => ("Plan mode".into(), String::new()),
        "exit_plan" => ("Plan".into(), String::new()),
        "explore" => ("Explore".into(), arg("description")),
        // spawn_agent's tool cell is suppressed in render_tool; its visible
        // artifact is the separate "• Spawned <name> agent" Subagent line.
        "spawn_agent" => ("Agent".into(), arg("name")),
        "send_message" => ("Message".into(), arg("to")),
        "list_agents" => ("Agents".into(), String::new()),
        other => (other.to_string(), String::new()),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let clean = s.replace('\n', " ");
    if clean.chars().count() > n {
        format!("{}...", clean.chars().take(n).collect::<String>())
    } else {
        clean
    }
}

/// Wrap a plain string to `width` DISPLAY columns (width-aware, char-level).
/// Returns at least one row. Used by the user-message band so a long message stays
/// inside its background band on every row instead of overflowing into a broken
/// second line.
fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut rows: Vec<String> = Vec::new();
    // Split on explicit newlines FIRST, then width-wrap each segment. A `\n` has
    // zero display width, so folding it into a row would leave the row string
    // wider than the visible line — the band's trailing-pad math then under-fills
    // and the background stops mid-row (visible on multi-line subagent prompts).
    for segment in text.split('\n') {
        let mut cur = String::new();
        let mut col = 0usize;
        for ch in segment.chars() {
            let w = ch.width().unwrap_or(0);
            if col + w > width && col > 0 {
                rows.push(std::mem::take(&mut cur));
                col = 0;
            }
            cur.push(ch);
            col += w;
        }
        rows.push(cur);
    }
    rows
}

/// Render one cell into zero or more display lines. `width` is the render width,
/// used to fill full-width background bands (the user message).
pub fn render_cell(cell: &Cell, width: usize, out: &mut Vec<Line<'static>>) {
    match cell {
        Cell::User(text) => {
            // A floating band: the colored (input-bg) block is inset from the
            // transcript edges by MARGIN cols on each side, so base-bg shows in the
            // gutter and the message reads like a chat bubble rather than a
            // full-width strip. Inside the band there's a 1-col text inset + the
            // dim `›` marker.
            use unicode_width::UnicodeWidthStr;
            const MARGIN: usize = super::widgets::BAND_MARGIN as usize;
            let bg = Style::default().bg(Palette::INPUT_BG());
            let gutter = || Span::raw(" ".repeat(MARGIN)); // base-bg on both sides
            let band_w = width.saturating_sub(MARGIN * 2).max(1);
            // A blank band row (gutter + colored fill + gutter).
            let pad_row = || {
                Line::from(vec![
                    gutter(),
                    Span::styled(" ".repeat(band_w), bg),
                    gutter(),
                ])
            };
            out.push(pad_row());

            let prefix = " › "; // 1-col inset inside the band + the marker
            let indent = "   ";
            let content_w = band_w.saturating_sub(prefix.width() + 1).max(1);
            let rows = wrap_plain(text, content_w);
            for (i, row) in rows.iter().enumerate() {
                let lead = if i == 0 { prefix } else { indent };
                let used = lead.width() + row.width();
                let trailing = band_w.saturating_sub(used);
                out.push(Line::from(vec![
                    gutter(),
                    Span::styled(lead, bg.fg(Palette::DIM())),
                    Span::styled(row.clone(), bg.fg(Palette::TEXT())),
                    Span::styled(" ".repeat(trailing), bg),
                    gutter(),
                ]));
            }
            out.push(pad_row());
            out.push(Line::from(""));
        }
        Cell::Assistant { text, .. } => {
            for l in render_markdown(text) {
                out.push(l);
            }
            out.push(Line::from(""));
        }
        Cell::Tool {
            name,
            input,
            status,
            output,
            expanded,
            ..
        } => {
            render_tool(
                name,
                input,
                *status,
                output.as_deref(),
                *expanded,
                width,
                out,
            );
        }
        Cell::Subagent {
            agent_id,
            parent_id: _,
            task,
            tools,
            done,
            failed,
        } => {
            // One line per subagent: a status dot (running/done/failed) + "Spawned
            // <name> agent", then the live tool count or the terminal state.
            let dot_color = if *failed {
                Palette::ERROR()
            } else if *done {
                Palette::OK()
            } else {
                Palette::RUNNING()
            };
            let label = if agent_id.is_empty() || agent_id.starts_with("task_") {
                truncate(task, 48)
            } else {
                agent_id.clone()
            };
            let trailing = if *failed {
                "  failed".to_string()
            } else if *done {
                "  finished".to_string()
            } else if *tools == 1 {
                "  (1 tool)".to_string()
            } else {
                format!("  ({} tools)", tools)
            };
            out.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(dot_color)),
                Span::styled("Spawned ", Style::default().fg(Palette::DIM())),
                Span::styled(
                    label,
                    Style::default()
                        .fg(Palette::TEXT())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" agent", Style::default().fg(Palette::DIM())),
                Span::styled(trailing, Style::default().fg(Palette::DIM())),
            ]));
            out.push(Line::from(""));
        }
        Cell::Compaction {
            before,
            after,
            done,
        } => {
            if *done {
                out.push(Line::from(vec![
                    Span::styled("• ", Style::default().fg(Palette::OK())),
                    Span::styled(
                        "Compacted",
                        Style::default()
                            .fg(Palette::TEXT())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  ~{} → ~{} tokens", before, after),
                        Style::default().fg(Palette::DIM()),
                    ),
                ]));
            } else {
                out.push(Line::from(vec![
                    Span::styled("• ", Style::default().fg(Palette::RUNNING())),
                    Span::styled(
                        "Compacting",
                        Style::default()
                            .fg(Palette::TEXT())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        " summarizing conversation…",
                        Style::default().fg(Palette::DIM()),
                    ),
                ]));
            }
            out.push(Line::from(""));
        }
        Cell::Notice(text) => {
            let color = if text.starts_with("error") {
                Palette::ERROR()
            } else {
                Palette::DIM()
            };
            out.push(Line::from(Span::styled(
                format!("  {}", text),
                Style::default().fg(color),
            )));
        }
        Cell::Event(text) => {
            out.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(Palette::ACCENT())),
                Span::styled(text.clone(), Style::default().fg(Palette::TEXT())),
            ]));
            out.push(Line::from(""));
        }
        Cell::AgentMsg { from, text } => {
            // A message to/from an agent, shown in the team drawer's per-agent
            // thread: a dim "<from> ›" prefix then the message text.
            out.push(Line::from(vec![
                Span::styled(
                    format!("{} › ", from),
                    Style::default()
                        .fg(Palette::ACCENT())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(text.clone(), Style::default().fg(Palette::TEXT())),
            ]));
            out.push(Line::from(""));
        }
        Cell::Workflow {
            title,
            phases,
            done,
            ..
        } => {
            render_workflow(title, phases, *done, out);
        }
    }
}

/// Small colored status dot for a workflow agent/phase — same glyph the subagent
/// cells use (`•`), colored orange=running, green=done, red=failed.
pub(super) fn wf_dot(status: WfStatus) -> Span<'static> {
    let color = match status {
        WfStatus::Running => Palette::RUNNING(),
        WfStatus::Done => Palette::OK(),
        WfStatus::Failed => Palette::ERROR(),
    };
    Span::styled("•".to_string(), Style::default().fg(color))
}

/// Render a workflow run as a phase/agent tree. Running shows the full tree; when
/// `done`, it collapses to a single summary line (title + agent count). Each agent
/// row is one line, in the SAME order as the phases/agents vectors, so a click can
/// be mapped back to an agent id by counting rows (see `workflow_row_agent`).
fn render_workflow(title: &str, phases: &[WfPhase], done: bool, out: &mut Vec<Line<'static>>) {
    let total_agents: usize = phases.iter().map(|p| p.agents.len()).sum();

    if done {
        out.push(Line::from(vec![
            wf_dot(WfStatus::Done),
            Span::styled(" Workflow ", Style::default().fg(Palette::DIM())),
            Span::styled(
                title.to_string(),
                Style::default()
                    .fg(Palette::TEXT())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " · {} phase{} · {} agent{}",
                    phases.len(),
                    if phases.len() == 1 { "" } else { "s" },
                    total_agents,
                    if total_agents == 1 { "" } else { "s" },
                ),
                Style::default().fg(Palette::DIM()),
            ),
        ]));
        out.push(Line::from(""));
        return;
    }

    out.push(Line::from(vec![
        wf_dot(WfStatus::Running),
        Span::styled(" Workflow ", Style::default().fg(Palette::DIM())),
        Span::styled(
            title.to_string(),
            Style::default()
                .fg(Palette::TEXT())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · running", Style::default().fg(Palette::DIM())),
    ]));

    // Three-level indent: workflow (col 0) → phase (col 2) → agents (col 4). Phases
    // are group labels (no status dot); agents carry the colored dot + status.
    for phase in phases {
        let done_count = phase
            .agents
            .iter()
            .filter(|a| a.status != WfStatus::Running)
            .count();
        out.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                phase.title.clone(),
                Style::default()
                    .fg(Palette::TEXT())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}/{}", done_count, phase.agents.len().max(phase.total)),
                Style::default().fg(Palette::DIM()),
            ),
        ]));
        for agent in &phase.agents {
            let trailing = match agent.status {
                WfStatus::Running if agent.tools == 1 => "  (1 tool)".to_string(),
                WfStatus::Running => format!("  ({} tools)", agent.tools),
                WfStatus::Done => "  done".to_string(),
                WfStatus::Failed => "  failed".to_string(),
            };
            out.push(Line::from(vec![
                Span::styled("    ", Style::default()),
                wf_dot(agent.status),
                Span::styled(
                    format!(" {}", agent.label),
                    Style::default().fg(Palette::TEXT()),
                ),
                Span::styled(trailing, Style::default().fg(Palette::DIM())),
            ]));
        }
    }
    out.push(Line::from(""));
}

fn render_tool(
    name: &str,
    input: &Value,
    status: ToolStatus,
    output: Option<&str>,
    expanded: bool,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    // Neither `spawn_agent`, `todo_write`, nor `workflow` renders a tool cell in
    // the transcript. `spawn_agent`'s visible artifact is the separate `Subagent`
    // cell; `todo_write` is shown by the sticky todo panel; `workflow`'s is the live
    // workflow tree cell (from its WorkflowPhase/Subagent events). A tool line for
    // any of them would just be noise.
    if name == "spawn_agent" || name == "todo_write" || name == "workflow" {
        return;
    }
    let (display, arg) = tool_display(name, input);
    let (bullet, bullet_color) = match status {
        ToolStatus::Running => ("•", Palette::RUNNING()),
        ToolStatus::Ok => ("•", Palette::OK()),
        ToolStatus::Error => ("•", Palette::ERROR()),
    };

    let mut header = vec![
        Span::styled(format!("{} ", bullet), Style::default().fg(bullet_color)),
        Span::styled(
            display,
            Style::default()
                .fg(Palette::TEXT())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if !arg.is_empty() {
        if name == "bash" {
            header.push(Span::raw(" "));
            for s in highlight_line(&truncate(&arg, 100), "sh") {
                header.push(s);
            }
        } else {
            header.push(Span::styled(
                format!(" {}", truncate(&arg, 72)),
                Style::default().fg(Palette::DIM()),
            ));
        }
    }
    out.push(Line::from(header));

    // `task`, `explore`, and `spawn_agent` are followed by their own Subagent
    // cells (the "• Spawned <name> agent" lines). Emit a blank spacer after the
    // header so the subagent block is separated, then suppress the tool output (the
    // Subagent cells carry it).
    if name == "task" || name == "explore" || name == "spawn_agent" {
        out.push(Line::from(""));
        return;
    }

    // Output preview / diff.
    let Some(output) = output else {
        out.push(Line::from(""));
        return;
    };
    // Read and List show only the path in the header — no content dump — UNLESS
    // the user expanded the cell (then show the full content below).
    if (name == "read_file" || name == "list_dir") && !expanded {
        out.push(Line::from(""));
        return;
    }
    // `todo_write` renders as the checklist panel — don't dump its raw result.
    if name == "todo_write" {
        out.push(Line::from(""));
        return;
    }
    if let Some((head, lang, body)) = parse_diff_output(output) {
        // Edit tools: "edited x (+a -b)" then a ```diff <path> body.
        out.push(indent_line(diff_header(&head)));
        for l in render_diff(&body, &lang) {
            out.push(indent_line(l));
        }
        out.push(Line::from(""));
        return;
    }

    // Generic output preview: a short preview (first few lines) by default, or the
    // FULL output when expanded. Error coloring comes from the tool's real status.
    // Bash output gets a subtle `│` gutter tying it to its command line; other
    // tools use a plain indent.
    let is_error = status == ToolStatus::Error;
    let color = if is_error {
        Palette::ERROR()
    } else {
        Palette::FAINT()
    };
    // Drop trailing blank lines so the cell doesn't end in dead rows.
    let body: Vec<&str> = {
        let mut v: Vec<&str> = output.split('\n').collect();
        while matches!(v.last(), Some(l) if l.trim().is_empty()) {
            v.pop();
        }
        v
    };
    const PREVIEW: usize = 6;
    const EXPANDED_MAX: usize = 500;
    let limit = if expanded { EXPANDED_MAX } else { PREVIEW };
    let total = body.len();
    let gutter = name == "bash";
    // Pre-wrap each output line to the content width so a long line breaks with the
    // gutter/indent repeated on EVERY visual row (the outer scrollback wrap would
    // otherwise split it and drop the `│`). Reserve columns for the scrollback's
    // own 2-col hanging indent plus our lead (`  │ ` or `    `, 4 cols).
    let lead: &str = if gutter { "  │ " } else { "    " };
    let content_w = width.saturating_sub(2 + lead.chars().count()).max(8);
    // Cap the number of VISUAL rows we emit, not just logical lines: a single very
    // long line (e.g. a minified JSON blob or a one-line coverage report) wraps
    // into many rows, so limiting by logical line alone lets one line fill the
    // screen. We count wrapped rows and stop at `limit`, then report how many
    // logical lines never got shown.
    let mut rows_used = 0usize;
    let mut shown_lines = 0usize;
    for line in body.iter() {
        if rows_used >= limit {
            break;
        }
        for (r, row) in wrap_plain(line, content_w).into_iter().enumerate() {
            if rows_used >= limit {
                break;
            }
            // Continuation rows align under the text (keep the gutter bar for bash,
            // blank the marker so only the bar shows), so wrapped output stays tidy.
            let this_lead = if r == 0 {
                lead.to_string()
            } else if gutter {
                "  │ ".to_string()
            } else {
                "    ".to_string()
            };
            out.push(Line::from(vec![
                Span::styled(this_lead, Style::default().fg(Palette::FAINT())),
                Span::styled(row, Style::default().fg(color)),
            ]));
            rows_used += 1;
        }
        shown_lines += 1;
    }
    let extra = total.saturating_sub(shown_lines);
    if extra > 0 {
        out.push(Line::from(Span::styled(
            format!("{lead}... {extra} more lines (click to expand)"),
            Style::default().fg(Palette::FAINT()),
        )));
    }
    // Trailing blank line separates consecutive tool cells.
    out.push(Line::from(""));
}

/// If `output` is an edit result ("edited … (+/-)\n```diff <path>\n…\n```"),
/// split it into (header, lang/path, diff_body).
fn parse_diff_output(output: &str) -> Option<(String, String, String)> {
    let fence = output.find("```diff")?;
    let header = output[..fence].trim().to_string();
    // The rest of the fence line after "```diff" is the lang/path tag.
    let after_fence = &output[fence + "```diff".len()..];
    let nl = after_fence.find('\n')?;
    let lang = after_fence[..nl].trim().to_string();
    let rest = &after_fence[nl + 1..];
    let end = rest.find("```").unwrap_or(rest.len());
    let body = rest[..end].trim_end_matches('\n').to_string();
    Some((header, lang, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wrap_plain_breaks_on_embedded_newlines() {
        // A multi-line prompt (as a subagent task is) must split at each `\n`, so
        // every visible row is a separate string the band can pad to full width.
        // Regression: `\n` (zero display width) used to fold into a row, leaving the
        // band background clipped after the first segment on wrap.
        let rows = wrap_plain("line one\nline two", 40);
        assert_eq!(rows, vec!["line one".to_string(), "line two".to_string()]);

        // Width-wrapping still applies WITHIN each newline-delimited segment.
        let rows = wrap_plain("aaaa\nbbbbbb", 3);
        assert_eq!(
            rows,
            vec![
                "aaa".to_string(),
                "a".to_string(),
                "bbb".to_string(),
                "bbb".to_string()
            ]
        );

        // A trailing newline yields an empty final row (a blank band line), not a
        // dropped one.
        assert_eq!(wrap_plain("x\n", 10), vec!["x".to_string(), "".to_string()]);
    }

    #[test]
    fn tool_display_maps_names_and_args() {
        assert_eq!(
            tool_display("write_file", &json!({"path": "a.py"})),
            ("Write".into(), "a.py".into())
        );
        assert_eq!(
            tool_display("multi_edit", &json!({"path": "b.rs"})),
            ("Edit".into(), "b.rs".into())
        );
        assert_eq!(
            tool_display("web_search", &json!({"query": "cats"})),
            ("Search".into(), "cats".into())
        );
        // A missing arg key yields an empty string, not a panic.
        assert_eq!(tool_display("grep", &json!({})), ("Grep".into(), "".into()));
        // Unknown tools pass through their raw name with no arg.
        assert_eq!(
            tool_display("mystery_tool", &json!({})),
            ("mystery_tool".into(), "".into())
        );
    }

    #[test]
    fn truncate_appends_ellipsis_and_flattens_newlines() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello...");
        // Newlines collapse to spaces so a cell stays one line.
        assert_eq!(truncate("a\nb", 10), "a b");
    }

    #[test]
    fn parse_diff_output_splits_fence() {
        let out = "edited foo.rs (+2/-1)\n```diff foo.rs\n+added\n-removed\n```";
        let (header, lang, body) = parse_diff_output(out).unwrap();
        assert_eq!(header, "edited foo.rs (+2/-1)");
        assert_eq!(lang, "foo.rs");
        assert_eq!(body, "+added\n-removed");
        // No fence → None.
        assert!(parse_diff_output("plain output, no diff").is_none());
    }
}
