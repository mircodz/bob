//! Turn view-model cells into ratatui Lines for the scrollback viewport.

use super::diffview::{diff_header, render_diff};
use super::highlight::highlight_line;
use super::indent_line;
use super::markdown::render_markdown;
use super::theme::Palette;
use super::view::{Cell, ToolStatus};
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
    let mut cur = String::new();
    let mut col = 0usize;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if col + w > width && col > 0 {
            rows.push(std::mem::take(&mut cur));
            col = 0;
        }
        cur.push(ch);
        col += w;
    }
    rows.push(cur);
    rows
}

/// Render one cell into zero or more display lines. `width` is the render width,
/// used to fill full-width background bands (the user message).
pub fn render_cell(cell: &Cell, width: usize, out: &mut Vec<Line<'static>>) {
    match cell {
        Cell::User(text) => {
            // A full-width band with the input background: a blank padded row above
            // and below, and the message wrapped to `width` with EACH row padded out
            // so the bg fills edge to edge. Wrapping here (rather than letting an
            // outer pass split an over-wide pre-padded line) keeps the band intact
            // on long messages. Styled like the live prompt — a dim `› ` marker.
            use unicode_width::UnicodeWidthStr;
            let bg = Style::default().bg(Palette::INPUT_BG());
            let pad_row = |w: usize| Line::from(Span::styled(" ".repeat(w), bg));
            out.push(pad_row(width));

            let prefix = "› ";
            let indent = "  "; // continuation rows align under the marker
            let content_w = width.saturating_sub(prefix.width()).max(1);
            let rows = wrap_plain(text, content_w);
            for (i, row) in rows.iter().enumerate() {
                let lead = if i == 0 { prefix } else { indent };
                let used = lead.width() + row.width();
                let trailing = width.saturating_sub(used);
                out.push(Line::from(vec![
                    Span::styled(lead, bg.fg(Palette::DIM())),
                    Span::styled(row.clone(), bg.fg(Palette::TEXT())),
                    Span::styled(" ".repeat(trailing), bg),
                ]));
            }
            out.push(pad_row(width));
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
            parent_id,
            task,
            tools,
            done,
            failed,
        } => {
            let _ = parent_id;
            // One line per subagent: a status dot — orange while running, green on
            // success, red on failure — + "Spawned <name> agent", then the tool
            // count while running, or "finished" / "failed" when done.
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
    }
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
    // Neither `spawn_agent` nor `todo_write` renders a tool cell in the
    // transcript. `spawn_agent`'s visible artifact is the separate `Subagent`
    // cell (the "• Spawned <name> agent" line, from the SubagentSpawn event);
    // `todo_write` is shown by the sticky todo panel above the input. A tool line
    // for either would just be noise.
    if name == "spawn_agent" || name == "todo_write" {
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
            // Syntax-highlight the shell command.
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
    for line in body.iter().take(limit) {
        for (r, row) in wrap_plain(line, content_w).into_iter().enumerate() {
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
        }
    }
    let extra = total.saturating_sub(limit);
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

/// Render a small markdown snippet (used for the permission preview diff). The
/// snippet is typically a header line plus a ```diff / ```lang fence, so this
/// just delegates to the markdown pre-renderer.
pub fn render_markdown_like(md: &str) -> Vec<Line<'static>> {
    render_markdown(md)
}
