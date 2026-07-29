//! Turn view-model cells into ratatui Lines for the scrollback viewport.

use super::diffview::{diff_header, render_diff};
use super::highlight::highlight_line;
use super::markdown::render_markdown;
use super::theme::Palette;
use super::view::{Cell, ToolStatus};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// Glyph for a still-running subagent (a filled dot; the spinner in the input
/// band already animates, so this stays static in the transcript).

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
        "todo_write" => ("Plan".into(), String::new()),
        "task" => ("Task".into(), String::new()),
        // spawn_agent renders like task: the "Agent" label, and its subagent cell
        // (the ├─/╰─ line) shows the description + status below. The verbose tool
        // output ("spawned agent…") is suppressed in render_tool.
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

/// Render one cell into zero or more display lines. `width` is the render width,
/// used to fill full-width background bands (the user message).
pub fn render_cell(cell: &Cell, width: usize, out: &mut Vec<Line<'static>>) {
    match cell {
        Cell::User(text) => {
            // A full-width band with the input background: blank padded row above
            // and below, and the message row padded out to `width` so the bg
            // fills edge-to-edge (no ragged right side).
            let bg = Style::default().bg(Palette::INPUT_BG());
            let pad_row = |w: usize| Line::from(Span::styled(" ".repeat(w), bg));
            out.push(pad_row(width));
            let prefix = "› ";
            let used = prefix.chars().count() + text.chars().count();
            let trailing = width.saturating_sub(used);
            out.push(Line::from(vec![
                Span::styled(
                    prefix,
                    bg.fg(Palette::ACCENT()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    text.clone(),
                    bg.fg(Palette::USER()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ".repeat(trailing), bg),
            ]));
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
            ..
        } => {
            render_tool(name, input, *status, output.as_deref(), out);
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
        Cell::Compaction { before, after } => {
            out.push(Line::from(Span::styled(
                format!("  ⟲ compacted history: ~{} → ~{} tokens", before, after),
                Style::default().fg(Palette::DIM()),
            )));
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
    }
}

fn render_tool(
    name: &str,
    input: &Value,
    status: ToolStatus,
    output: Option<&str>,
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
            // Syntax-highlight the shell command (like Codex does).
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

    // `task` and `spawn_agent` are immediately followed by their subagent cells
    // (the ├─/╰─ tree), so they emit NO trailing blank and suppress their tool
    // output — the tree butts directly under the header and shows the status. The
    // last subagent in the group provides the spacing below.
    if name == "task" || name == "spawn_agent" {
        return;
    }

    // Output preview / diff.
    let Some(output) = output else {
        out.push(Line::from(""));
        return;
    };
    // Read and List show only the path in the header — no content dump.
    if name == "read_file" || name == "list_dir" {
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
        out.push(indent(diff_header(&head)));
        for l in render_diff(&body, &lang) {
            out.push(indent(l));
        }
        out.push(Line::from(""));
        return;
    }

    // Generic: show a short dim preview (first few lines).
    let is_error = output.starts_with("error:");
    let color = if is_error {
        Palette::ERROR()
    } else {
        Palette::FAINT()
    };
    for line in output.split('\n').take(6) {
        out.push(Line::from(Span::styled(
            format!("    {}", truncate(line, 100)),
            Style::default().fg(color),
        )));
    }
    let extra = output.split('\n').count().saturating_sub(6);
    if extra > 0 {
        out.push(Line::from(Span::styled(
            format!("    ... {} more lines", extra),
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

fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("   ")];
    spans.extend(line.spans);
    Line::from(spans)
}

/// Render a small markdown snippet (used for the permission preview diff). The
/// snippet is typically a header line plus a ```diff / ```lang fence, so this
/// just delegates to the markdown pre-renderer.
pub fn render_markdown_like(md: &str) -> Vec<Line<'static>> {
    render_markdown(md)
}
