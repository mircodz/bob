//! Turn view-model cells into ratatui Lines for the scrollback viewport.

use super::diffview::diff_header;
#[cfg(not(test))]
use super::diffview::render_diff;
#[cfg(not(test))]
use super::highlight::highlight_line;
use super::indent_line;
#[cfg(not(test))]
use super::markdown::render_markdown;
pub(super) use super::markdown::render_markdown as render_markdown_snippet;
use super::theme::Palette;
use super::view::{Cell, ToolStatus, WfPhase, WfStatus};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use std::borrow::Cow;

/// Remove terminal controls from display text without changing the stored source.
/// Newlines and tabs remain for text layout; buffer symbols need to exclude those too.
pub(super) fn safe_display_text(text: &str) -> Cow<'_, str> {
    DisplaySanitizer::default().text(text)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ControlState {
    #[default]
    Text,
    Escape,
    EscapeIntermediate,
    Csi,
    String {
        osc: bool,
        escape: bool,
    },
}

#[derive(Default)]
struct DisplaySanitizer {
    state: ControlState,
}

impl DisplaySanitizer {
    fn text<'a>(&mut self, text: &'a str) -> Cow<'a, str> {
        if self.state == ControlState::Text
            && !text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Cow::Borrowed(text);
        }
        let mut clean = String::with_capacity(text.len());
        for c in text.chars() {
            if let ControlState::String { osc, escape } = self.state {
                self.state = if matches!(c, '\u{9c}' | '\u{18}' | '\u{1a}')
                    || (osc && c == '\u{7}')
                    || (escape && c == '\\')
                {
                    ControlState::Text
                } else {
                    ControlState::String {
                        osc,
                        escape: c == '\u{1b}',
                    }
                };
                continue;
            }
            match c {
                '\u{1b}' => self.state = ControlState::Escape,
                '\u{9b}' => self.state = ControlState::Csi,
                '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                    self.state = ControlState::String {
                        osc: c == '\u{9d}',
                        escape: false,
                    };
                }
                '\u{18}' | '\u{1a}' | '\u{9c}' => self.state = ControlState::Text,
                '\n' | '\t' => clean.push(c),
                c if c.is_control() => {}
                _ => match self.state {
                    ControlState::Text => clean.push(c),
                    ControlState::Escape => {
                        self.state = match c {
                            '[' => ControlState::Csi,
                            ']' | 'P' | 'X' | '^' | '_' => ControlState::String {
                                osc: c == ']',
                                escape: false,
                            },
                            '\u{20}'..='\u{2f}' => ControlState::EscapeIntermediate,
                            '\u{30}'..='\u{7e}' => ControlState::Text,
                            _ => {
                                clean.push(c);
                                ControlState::Text
                            }
                        };
                    }
                    ControlState::EscapeIntermediate => match c {
                        '\u{20}'..='\u{2f}' => {}
                        '\u{30}'..='\u{7e}' => self.state = ControlState::Text,
                        _ => {
                            self.state = ControlState::Text;
                            clean.push(c);
                        }
                    },
                    ControlState::Csi => match c {
                        '\u{20}'..='\u{3f}' => {}
                        '\u{40}'..='\u{7e}' => self.state = ControlState::Text,
                        _ => {
                            self.state = ControlState::Text;
                            clean.push(c);
                        }
                    },
                    ControlState::String { .. } => unreachable!(),
                },
            }
        }
        Cow::Owned(clean)
    }
}

/// Keep parser-produced controls and sequences split across spans out of display output.
pub(super) fn sanitize_display_lines(lines: &mut [Line<'_>]) {
    let mut sanitizer = DisplaySanitizer::default();
    for line in lines {
        for span in &mut line.spans {
            if let Cow::Owned(clean) = sanitizer.text(span.content.as_ref()) {
                span.content = Cow::Owned(clean);
            }
        }
    }
}

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
        // Agent creation tools use a separate static Subagent launch notice.
        "spawn_agent" => ("Agent".into(), arg("name")),
        "send_message" => ("Message".into(), arg("to")),
        "stop_agent" => ("Stop agent".into(), arg("name")),
        "list_agents" => ("Agents".into(), String::new()),
        other => (other.to_string(), String::new()),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let clean = safe_display_text(s).replace('\n', " ");
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
    wrap_plain_limited(&safe_display_text(text), width, usize::MAX)
}

fn wrap_plain_limited(text: &str, width: usize, limit: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    if limit == 0 {
        return Vec::new();
    }
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
                if rows.len() == limit {
                    return rows;
                }
                col = 0;
            }
            cur.push(ch);
            col += w;
        }
        rows.push(cur);
        if rows.len() == limit {
            return rows;
        }
    }
    rows
}

/// Prepend `lead` to each styled line, pre-wrapping to `width` so the OUTER
/// scrollback wrapper never re-splits a row and drops the lead. This is the
/// error-prone core behind any left-decorated block (a `▏`/`│` gutter, an accent
/// bar): the decoration must repeat on every VISUAL row, but the outer wrapper only
/// sees column 0, so if a row is wider than the viewport it gets re-split with the
/// lead stranded on the first fragment only. Pre-wrapping here — reserving the lead's
/// own columns — keeps every emitted row within `width`, so downstream wrapping is a
/// no-op and the lead survives on each row.
fn with_left_lead(
    lines: Vec<Line<'static>>,
    lead: Span<'static>,
    width: usize,
) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    let lead_w = lead.content.width();
    let content_w = width.saturating_sub(lead_w).max(1);
    let mut out = Vec::new();
    for l in lines {
        for wl in super::wrap_line(l, content_w) {
            let mut spans = vec![lead.clone()];
            spans.extend(wl.spans);
            out.push(Line::from(spans));
        }
    }
    out
}

const PREVIEW: usize = 6;
const EXPANDED_MAX: usize = 500;

/// Width-independent content owned by the viewport until the cell fingerprint changes.
#[derive(Default)]
pub(super) struct PreparedCell {
    content: Vec<Line<'static>>,
    tool: Option<PreparedTool>,
}

struct PreparedTool {
    header: Vec<Span<'static>>,
    output: PreparedOutput,
}

enum PreparedOutput {
    None,
    Diff(Vec<Line<'static>>),
    Plain { text: String, total: usize },
}

impl PreparedCell {
    pub(super) fn new(cell: &Cell) -> Self {
        if !cell.is_visible() {
            return Self::default();
        }
        match cell {
            Cell::Assistant { text, .. } | Cell::Plan(text) => Self {
                content: render_markdown(text),
                tool: None,
            },
            Cell::AgentMsg { from, text } => Self {
                content: vec![Line::from(vec![
                    Span::styled(
                        format!("{} › ", from),
                        Style::default()
                            .fg(Palette::ACCENT())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(text.clone(), Style::default().fg(Palette::TEXT())),
                ])],
                tool: None,
            },
            Cell::Tool {
                name,
                input,
                output,
                expanded,
                ..
            } => {
                let (display, arg) = tool_display(name, input);
                let mut header = vec![Span::styled(
                    display,
                    Style::default()
                        .fg(Palette::TEXT())
                        .add_modifier(Modifier::BOLD),
                )];
                if !arg.is_empty() {
                    if name == "bash" {
                        header.push(Span::raw(" "));
                        header.extend(highlight_line(&truncate(&arg, 100), "sh"));
                    } else {
                        header.push(Span::styled(
                            format!(" {}", truncate(&arg, 72)),
                            Style::default().fg(Palette::DIM()),
                        ));
                    }
                }
                let output = if matches!(name.as_str(), "read_file" | "list_dir") && !expanded {
                    PreparedOutput::None
                } else if let Some(output) = output {
                    let output = safe_display_text(output);
                    if let Some((head, lang, body)) = parse_diff_output(&output) {
                        let mut lines = vec![diff_header(&head)];
                        lines.extend(render_diff(&body, &lang));
                        PreparedOutput::Diff(lines)
                    } else {
                        let trimmed = output.trim_end();
                        let total = if trimmed.is_empty() {
                            0
                        } else {
                            trimmed.bytes().filter(|b| *b == b'\n').count() + 1
                        };
                        let limit = if *expanded { EXPANDED_MAX } else { PREVIEW };
                        let end = output
                            .match_indices('\n')
                            .nth(limit - 1)
                            .map_or(output.len(), |(i, _)| i);
                        let text = output[..end].to_owned();
                        PreparedOutput::Plain { text, total }
                    }
                } else {
                    PreparedOutput::None
                };
                Self {
                    content: Vec::new(),
                    tool: Some(PreparedTool { header, output }),
                }
            }
            _ => Self::default(),
        }
    }

    pub(super) fn render(&self, cell: &Cell, width: usize, out: &mut Vec<Line<'static>>) {
        if !cell.is_visible() {
            return;
        }
        let start = out.len();
        render_cell_inner(self, cell, width, out);
        sanitize_display_lines(&mut out[start..]);
    }
}

/// Render one cell into zero or more display lines. Retain `PreparedCell` to reuse
/// Markdown and syntax highlighting across viewport widths.
#[cfg(test)]
pub fn render_cell(cell: &Cell, width: usize, out: &mut Vec<Line<'static>>) {
    PreparedCell::new(cell).render(cell, width, out);
}

fn render_cell_inner(
    prepared: &PreparedCell,
    cell: &Cell,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
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
        Cell::Assistant { .. } => {
            out.extend(prepared.content.iter().cloned());
            out.push(Line::from(""));
        }
        Cell::Tool {
            name,
            status,
            expanded,
            ..
        } => {
            render_tool(
                prepared.tool.as_ref().expect("visible tool was prepared"),
                name,
                *status,
                *expanded,
                width,
                out,
            );
        }
        Cell::Subagent { agent_id, task, .. } => {
            let label = if task.trim().is_empty() {
                agent_id
            } else {
                task
            };
            out.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(Palette::OK())),
                Span::styled(
                    "Started agent:",
                    Style::default()
                        .fg(Palette::TEXT())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {}", truncate(label, width.saturating_sub(18).min(72))),
                    Style::default().fg(Palette::TEXT()),
                ),
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
        Cell::Plan(_) => {
            // A proposed plan, set off as a bordered block: a labeled header, then
            // the full plan as Markdown with a left accent bar on every (wrapped) row
            // so it reads as one distinct region the user must approve.
            let bar = || Span::styled(" ▏ ", Style::default().fg(Palette::ACCENT()));
            // Reserve the scrollback's hanging-indent (2) + right margin (2) on top of
            // the bar so pre-wrapping matches the final viewport width.
            let block_w = width.saturating_sub(4).max(8);
            out.push(Line::from(vec![
                bar(),
                Span::styled(
                    "plan proposed",
                    Style::default()
                        .fg(Palette::ACCENT())
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            out.push(Line::from(bar()));
            out.extend(with_left_lead(prepared.content.clone(), bar(), block_w));
            out.push(Line::from(bar()));
            out.push(Line::from(""));
        }
        Cell::Event(text) => {
            out.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(Palette::ACCENT())),
                Span::styled(text.clone(), Style::default().fg(Palette::TEXT())),
            ]));
            out.push(Line::from(""));
        }
        Cell::AgentMsg { .. } => {
            out.extend(prepared.content.iter().cloned());
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
    prepared: &PreparedTool,
    name: &str,
    status: ToolStatus,
    expanded: bool,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let (bullet, bullet_color) = match status {
        ToolStatus::Running => ("•", Palette::RUNNING()),
        ToolStatus::Ok => ("•", Palette::OK()),
        ToolStatus::Error => ("•", Palette::ERROR()),
    };

    let mut header = vec![Span::styled(
        format!("{} ", bullet),
        Style::default().fg(bullet_color),
    )];
    header.extend(prepared.header.iter().cloned());
    out.push(Line::from(header));

    let (body, total) = match &prepared.output {
        PreparedOutput::None => {
            out.push(Line::from(""));
            return;
        }
        PreparedOutput::Diff(lines) => {
            out.extend(lines.iter().cloned().map(indent_line));
            out.push(Line::from(""));
            return;
        }
        PreparedOutput::Plain { text, total } => (text, *total),
    };

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
    let limit = if expanded { EXPANDED_MAX } else { PREVIEW };
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
    for line in body.split('\n').take(total) {
        if rows_used >= limit {
            break;
        }
        for (r, row) in wrap_plain_limited(line, content_w, limit - rows_used)
            .into_iter()
            .enumerate()
        {
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
thread_local! {
    static PREPARE_COUNTS: std::cell::Cell<(usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 0)) };
}

#[cfg(test)]
fn render_markdown(text: &str) -> Vec<Line<'static>> {
    PREPARE_COUNTS.with(|counts| {
        let (md, diff, bash) = counts.get();
        counts.set((md + 1, diff, bash));
    });
    super::markdown::render_markdown(text)
}

#[cfg(test)]
fn render_diff(body: &str, lang: &str) -> Vec<Line<'static>> {
    PREPARE_COUNTS.with(|counts| {
        let (md, diff, bash) = counts.get();
        counts.set((md, diff + 1, bash));
    });
    super::diffview::render_diff(body, lang)
}

#[cfg(test)]
fn highlight_line(text: &str, lang: &str) -> Vec<Span<'static>> {
    PREPARE_COUNTS.with(|counts| {
        let (md, diff, bash) = counts.get();
        counts.set((md, diff, bash + 1));
    });
    super::highlight::highlight_line(text, lang)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str, output: &str, expanded: bool) -> Cell {
        Cell::Tool {
            id: "tool".into(),
            name: name.into(),
            input: json!({"path": "file.rs", "command": "printf '%s' hello"}),
            status: ToolStatus::Ok,
            output: Some(output.into()),
            expanded,
        }
    }

    fn legacy_tool(cell: &Cell, width: usize) -> Vec<Line<'static>> {
        let Cell::Tool {
            name,
            input,
            status,
            output,
            expanded,
            ..
        } = cell
        else {
            unreachable!();
        };
        if !cell.is_visible() {
            return Vec::new();
        }
        let (display, arg) = tool_display(name, input);
        let color = match status {
            ToolStatus::Running => Palette::RUNNING(),
            ToolStatus::Ok => Palette::OK(),
            ToolStatus::Error => Palette::ERROR(),
        };
        let mut header = vec![
            Span::styled("• ", Style::default().fg(color)),
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
                header.extend(highlight_line(&truncate(&arg, 100), "sh"));
            } else {
                header.push(Span::styled(
                    format!(" {}", truncate(&arg, 72)),
                    Style::default().fg(Palette::DIM()),
                ));
            }
        }
        let mut out = vec![Line::from(header)];
        if let Some(output) = output
            .as_deref()
            .filter(|_| *expanded || !matches!(name.as_str(), "read_file" | "list_dir"))
        {
            let output = safe_display_text(output);
            if let Some((head, lang, body)) = parse_diff_output(&output) {
                out.push(indent_line(diff_header(&head)));
                out.extend(render_diff(&body, &lang).into_iter().map(indent_line));
            } else {
                let mut body: Vec<_> = output.split('\n').collect();
                while body.last().is_some_and(|line| line.trim().is_empty()) {
                    body.pop();
                }
                let limit = if *expanded { EXPANDED_MAX } else { PREVIEW };
                let lead = if name == "bash" { "  │ " } else { "    " };
                let content_w = width.saturating_sub(2 + lead.chars().count()).max(8);
                let mut rows_used = 0;
                let mut shown_lines = 0;
                for line in &body {
                    if rows_used >= limit {
                        break;
                    }
                    for row in wrap_plain(line, content_w) {
                        if rows_used >= limit {
                            break;
                        }
                        out.push(Line::from(vec![
                            Span::styled(lead, Style::default().fg(Palette::FAINT())),
                            Span::styled(
                                row,
                                Style::default().fg(if *status == ToolStatus::Error {
                                    Palette::ERROR()
                                } else {
                                    Palette::FAINT()
                                }),
                            ),
                        ]));
                        rows_used += 1;
                    }
                    shown_lines += 1;
                }
                let extra = body.len().saturating_sub(shown_lines);
                if extra > 0 {
                    out.push(Line::from(Span::styled(
                        format!("{lead}... {extra} more lines (click to expand)"),
                        Style::default().fg(Palette::FAINT()),
                    )));
                }
            }
        }
        out.push(Line::from(""));
        sanitize_display_lines(&mut out);
        out
    }

    #[test]
    fn prepared_tools_match_legacy_output_across_widths() {
        for name in ["bash", "read_file", "list_dir", "edit_file", "unknown"] {
            for expanded in [false, true] {
                for output in [
                    "".to_string(),
                    " \n\t\n\u{2003}".to_string(),
                    "one  \n\n\ttwo\u{1b}[31m 日本語\n\u{1b}]title\u{7}three \n\t\n".to_string(),
                    "界e\u{301} ".repeat(200),
                    "line\n".repeat(510),
                    "edited file.rs (+1 -1)\n```diff file.rs\n- 1| let x = 0;\n+ 1| let x = 1;\n```".to_string(),
                ] {
                    let mut cell = tool(name, &output, expanded);
                    for status in [ToolStatus::Running, ToolStatus::Ok, ToolStatus::Error] {
                        if let Cell::Tool { status: current, .. } = &mut cell { *current = status; }
                        let prepared = PreparedCell::new(&cell);
                        for width in [0usize, 1, 8, 24, 80, 160, 24] {
                            let mut actual = Vec::new();
                            prepared.render(&cell, width, &mut actual);
                            assert_eq!(actual, legacy_tool(&cell, width), "{name}, expanded={expanded}, width={width}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn prepared_markdown_matches_existing_layout_across_widths() {
        let text = "# Heading\n\n**Bold** and *italic* 日本語\n\n> a long quoted sentence for narrow widths\n\n```rust\nlet value = 42;\n```\n\n| key | value |\n| --- | --- |\n| a | b |";
        for cell in [
            Cell::Assistant {
                text: text.into(),
                open: false,
            },
            Cell::Plan(text.into()),
        ] {
            let prepared = PreparedCell::new(&cell);
            for width in [0usize, 1, 8, 24, 80, 160, 24] {
                let mut expected = Vec::new();
                if matches!(cell, Cell::Plan(_)) {
                    let bar = || Span::styled(" ▏ ", Style::default().fg(Palette::ACCENT()));
                    expected.push(Line::from(vec![
                        bar(),
                        Span::styled(
                            "plan proposed",
                            Style::default()
                                .fg(Palette::ACCENT())
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    expected.push(Line::from(bar()));
                    expected.extend(with_left_lead(
                        render_markdown(text),
                        bar(),
                        width.saturating_sub(4).max(8),
                    ));
                    expected.push(Line::from(bar()));
                } else {
                    expected.extend(render_markdown(text));
                }
                expected.push(Line::from(""));
                let mut actual = Vec::new();
                prepared.render(&cell, width, &mut actual);
                assert_eq!(actual, expected);
                let mut fresh = Vec::new();
                render_cell(&cell, width, &mut fresh);
                assert_eq!(actual, fresh);
            }
        }
    }

    #[test]
    fn prepared_content_is_not_parsed_or_highlighted_again_on_resize() {
        let cells = [
            Cell::Assistant {
                text: "```rust\nlet n = 1;\n```".into(),
                open: false,
            },
            Cell::Plan("**Plan**\n```sh\necho hello\n```".into()),
            tool(
                "bash",
                "edited file.rs\n```diff file.rs\n+ 1| let n = 2;\n```",
                false,
            ),
            Cell::AgentMsg {
                from: "root".into(),
                text: "Keep **literal** message styling".into(),
            },
        ];
        PREPARE_COUNTS.with(|counts| counts.set((0, 0, 0)));
        let prepared: Vec<_> = cells.iter().map(PreparedCell::new).collect();
        assert_eq!(PREPARE_COUNTS.with(|counts| counts.get()), (2, 1, 1));
        assert!(!prepared[0].content.is_empty());
        assert!(!prepared[1].content.is_empty());
        assert!(matches!(
            prepared[2].tool.as_ref().unwrap().output,
            PreparedOutput::Diff(_)
        ));
        for width in [10, 120, 20, 80, 10] {
            for (cell, prepared) in cells.iter().zip(&prepared) {
                let mut lines = Vec::new();
                prepared.render(cell, width, &mut lines);
                assert!(!lines.is_empty());
            }
        }
        assert_eq!(PREPARE_COUNTS.with(|counts| counts.get()), (2, 1, 1));
    }

    #[test]
    fn preparation_skips_hidden_and_collapsed_payloads() {
        PREPARE_COUNTS.with(|counts| counts.set((0, 0, 0)));
        for name in [
            "job_status",
            "job_output",
            "todo_write",
            "workflow",
            "task",
            "explore",
            "spawn_agent",
            "read_file",
            "list_dir",
        ] {
            let cell = tool(
                name,
                "edited file.rs\n```diff file.rs\n+ 1| let x = 1;\n```",
                false,
            );
            let prepared = PreparedCell::new(&cell);
            if cell.is_visible() {
                assert!(matches!(
                    prepared.tool.unwrap().output,
                    PreparedOutput::None
                ));
            } else {
                assert!(prepared.tool.is_none());
            }
            assert!(prepared.content.is_empty());
        }
        assert_eq!(PREPARE_COUNTS.with(|counts| counts.get()), (0, 0, 0));
    }

    #[test]
    fn generic_preview_retains_only_bounded_logical_lines() {
        for expanded in [false, true] {
            let cell = tool("bash", &"line\n".repeat(10_000), expanded);
            let prepared = PreparedCell::new(&cell);
            let PreparedOutput::Plain { text, total } = &prepared.tool.as_ref().unwrap().output
            else {
                panic!("plain output expected")
            };
            assert_eq!(*total, 10_000);
            assert_eq!(
                text.split('\n').count(),
                if expanded { EXPANDED_MAX } else { PREVIEW }
            );
        }
        assert_eq!(
            wrap_plain_limited(&"x".repeat(1_000_000), 8, 6),
            vec!["xxxxxxxx"; 6]
        );
        for text in ["", "a\n", "日本語e\u{301}\ttext", "a\n\nbbbb\n "] {
            for width in [1, 2, 8] {
                for limit in [0, 1, 2, 6] {
                    assert_eq!(
                        wrap_plain_limited(text, width, limit),
                        wrap_plain(text, width)
                            .into_iter()
                            .take(limit)
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
    }

    #[test]
    fn prepared_workflow_preserves_hit_test_rows() {
        use super::super::view::{ViewModel, WfAgent};
        let phase = |index, label: &str| WfPhase {
            title: format!("Phase {index}"),
            index,
            total: 2,
            agents: vec![WfAgent {
                agent_id: label.into(),
                label: label.into(),
                status: WfStatus::Running,
                tools: 2,
                model: None,
                tokens: 0,
                duration_secs: None,
                started_unix: 0,
            }],
        };
        let mut vm = ViewModel::new();
        vm.cells.push(Cell::Workflow {
            id: "wf".into(),
            title: "Demo".into(),
            phases: vec![phase(0, "first"), phase(1, "second")],
            done: false,
        });
        for done in [false, true] {
            if let Cell::Workflow { done: current, .. } = &mut vm.cells[0] {
                *current = done;
            }
            let cell = &vm.cells[0];
            assert!(cell.is_visible());
            let prepared = PreparedCell::new(cell);
            for width in [8, 24, 80] {
                let mut lines = Vec::new();
                prepared.render(cell, width, &mut lines);
                assert_eq!(lines.len(), if done { 2 } else { 6 });
                for (row, line) in lines.iter().enumerate() {
                    let expected = match (done, row) {
                        (false, 2) => Some("first"),
                        (false, 4) => Some("second"),
                        _ => None,
                    };
                    assert_eq!(cell.workflow_agent_at(row), expected);
                    if let Some(label) = expected {
                        assert!(line.to_string().contains(label));
                    }
                }
            }
        }
    }

    #[test]
    fn safe_display_text_preserves_unicode_and_layout_without_allocating() {
        let raw = "café 日本語 e\u{301} 👩\u{200d}💻\n\tline\n";
        assert!(matches!(safe_display_text(raw), Cow::Borrowed(s) if s == raw));
        assert_eq!(
            safe_display_text("a\0\u{7}\u{8}\r\u{7f}\u{85}b\n\t"),
            "ab\n\t"
        );
    }

    #[test]
    fn safe_display_text_strips_terminal_sequences_and_incomplete_tails() {
        for sequence in [
            "\u{1b}[31m",
            "\u{1b}[?25l",
            "\u{1b}[1;2H",
            "\u{1b}[2J",
            "\u{1b}]52;c;payload\u{7}",
            "\u{1b}]0;title\u{1b}\\",
            "\u{1b}Pdata\nmore\u{1b}\\",
            "\u{1b}Xdata\u{1b}\\",
            "\u{1b}^data\u{1b}\\",
            "\u{1b}_data\u{1b}\\",
            "\u{1b}(B",
            "\u{1b}7",
            "\u{1b}c",
            "\u{9b}31m",
            "\u{9d}52;c;payload\u{9c}",
            "\u{90}data\u{9c}",
            "\u{98}data\u{9c}",
            "\u{9e}data\u{9c}",
            "\u{9f}data\u{9c}",
            "\u{1b}]title\u{9c}",
            "\u{9d}title\u{1b}\\",
        ] {
            let raw = format!("before{sequence}後\n\t");
            assert_eq!(safe_display_text(&raw), "before後\n\t", "{sequence:?}");
        }
        for tail in [
            "\u{1b}",
            "\u{1b}[",
            "\u{1b}[31;",
            "\u{1b}(",
            "\u{1b}]title",
            "\u{90}data\u{1b}",
        ] {
            assert_eq!(safe_display_text(&format!("before{tail}")), "before");
        }
        assert_eq!(safe_display_text("a\u{1b}[31\u{18}b"), "ab");
        assert_eq!(safe_display_text("a\u{1b}[\u{1b}[31mb"), "ab");
        assert_eq!(safe_display_text("a\u{1b}[日本語"), "a日本語");
        assert_eq!(
            safe_display_text("a\u{1b}]hidden\u{1b}[2Jstill hidden\u{7}b"),
            "ab"
        );
    }

    #[test]
    fn display_line_boundary_preserves_styles_and_handles_split_sequences() {
        let bold = Style::default()
            .fg(Palette::TEXT())
            .add_modifier(Modifier::BOLD);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("前\u{1b}[", bold),
                Span::raw("31mred\u{1b}]52;c;"),
            ])
            .style(Style::default().bg(Palette::INPUT_BG())),
            Line::from(vec![Span::raw("payload\u{1b}"), Span::styled("\\後", bold)]),
        ];
        let original = lines.clone();
        sanitize_display_lines(&mut lines);
        assert_eq!(lines[0].to_string(), "前red");
        assert_eq!(lines[1].to_string(), "後");
        for (line, raw) in lines.iter().zip(&original) {
            assert_eq!(line.style, raw.style);
            assert_eq!(line.alignment, raw.alignment);
            assert_eq!(line.spans.len(), raw.spans.len());
            for (span, raw) in line.spans.iter().zip(&raw.spans) {
                assert_eq!(span.style, raw.style);
            }
        }
        assert!(original[0].spans[0].content.contains('\u{1b}'));
    }

    #[test]
    fn render_cell_sanitizes_all_display_text_without_mutating_cells() {
        let raw = "前\u{1b}[31m後\u{1b}]52;c;payload\u{7}";
        let cells = vec![
            Cell::User(raw.into()),
            Cell::Assistant {
                text: raw.into(),
                open: false,
            },
            Cell::Notice(raw.into()),
            Cell::Event(raw.into()),
            Cell::Plan(raw.into()),
            Cell::AgentMsg {
                from: raw.into(),
                text: raw.into(),
            },
            Cell::Subagent {
                agent_id: raw.into(),
                task: raw.into(),
                tools: 0,
                done: false,
                failed: false,
            },
            Cell::Workflow {
                id: "wf".into(),
                title: raw.into(),
                phases: vec![WfPhase {
                    title: raw.into(),
                    index: 0,
                    total: 1,
                    agents: vec![],
                }],
                done: false,
            },
            Cell::Tool {
                id: "tool".into(),
                name: raw.into(),
                input: json!({}),
                status: ToolStatus::Ok,
                output: Some(raw.into()),
                expanded: true,
            },
            Cell::Tool {
                id: "bash".into(),
                name: "bash".into(),
                input: json!({"command": raw}),
                status: ToolStatus::Error,
                output: Some(format!("{raw}\nmore")),
                expanded: true,
            },
            Cell::Tool {
                id: "diff".into(),
                name: "edit_file".into(),
                input: json!({"path": raw}),
                status: ToolStatus::Ok,
                output: Some(format!("edited {raw}\n```diff file.rs\n+{raw}\n```")),
                expanded: true,
            },
        ];
        for cell in &cells {
            let fingerprint = cell.fingerprint();
            let mut lines = vec![Line::from("existing\u{1b}[31m")];
            render_cell(cell, 24, &mut lines);
            assert_eq!(lines[0].to_string(), "existing\u{1b}[31m");
            let text = lines[1..]
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !text
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\t')),
                "{text:?}"
            );
            assert!(!text.contains("payload"), "{text:?}");
            assert!(text.contains("前後"), "{text:?}");
            assert_eq!(cell.fingerprint(), fingerprint);
        }
        if let Cell::Assistant { text, .. } = &cells[1] {
            assert_eq!(text, raw);
        }
        if let Cell::Tool { input, output, .. } = &cells[9] {
            assert_eq!(input["command"], raw);
            assert_eq!(output.as_deref(), Some(format!("{raw}\nmore").as_str()));
        }
    }

    #[test]
    fn paragraph_backend_emission_requires_display_sanitization() {
        use ratatui::backend::{Backend, CrosstermBackend};
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::{Paragraph, Widget};

        let emitted = |lines: Vec<Line<'static>>| {
            let area = Rect::new(0, 0, 120, 4);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(lines).render(area, &mut buffer);
            let mut bytes = Vec::new();
            let mut backend = CrosstermBackend::new(&mut bytes);
            backend
                .draw(
                    buffer
                        .content
                        .iter()
                        .enumerate()
                        .map(|(i, cell)| ((i % 120) as u16, (i / 120) as u16, cell)),
                )
                .unwrap();
            String::from_utf8(bytes).unwrap()
        };
        let sequence = "\u{1b}]52;c;payload\u{7}";
        let raw = format!("before{sequence}after");
        assert!(emitted(vec![Line::from(raw.clone())]).contains(sequence));
        let mut lines = Vec::new();
        render_cell(&Cell::Notice(raw), 120, &mut lines);
        let bytes = emitted(lines);
        assert!(!bytes.contains(sequence));
        assert!(!bytes.contains("payload"));
        assert!(bytes.contains("beforeafter"));
    }

    #[test]
    fn sanitization_precedes_wrapping_and_truncation() {
        assert_eq!(wrap_plain("a\u{1b}[31mb\u{1b}[0mc", 2), ["ab", "c"]);
        assert_eq!(truncate("a\u{1b}]title\u{7}bc", 2), "ab...");
    }

    #[test]
    fn job_polling_tools_render_no_lines() {
        for name in ["job_status", "job_output"] {
            for status in [ToolStatus::Running, ToolStatus::Ok, ToolStatus::Error] {
                for expanded in [false, true] {
                    for output in [None, Some("job_1: finished\nresult text".to_string())] {
                        let cell = Cell::Tool {
                            id: "poll".into(),
                            name: name.into(),
                            input: json!({"id": "job_1"}),
                            status,
                            output,
                            expanded,
                        };
                        let mut lines = Vec::new();
                        render_cell(&cell, 80, &mut lines);
                        assert!(
                            lines.is_empty(),
                            "{name} must not render, even when expanded"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn agent_launch_notice_is_static_and_descriptive() {
        let mut tool_header = Vec::new();
        render_cell(
            &Cell::Tool {
                id: "read".into(),
                name: "read_file".into(),
                input: json!({"path": "a.rs"}),
                status: ToolStatus::Ok,
                output: None,
                expanded: false,
            },
            80,
            &mut tool_header,
        );
        for id in ["task_7", "job_8", "reviewer"] {
            let mut initial = Vec::new();
            for (tools, done, failed) in [
                (0, false, false),
                (3, false, false),
                (3, true, false),
                (3, true, true),
            ] {
                let cell = Cell::Subagent {
                    agent_id: id.into(),
                    task: "Review the parser".into(),
                    tools,
                    done,
                    failed,
                };
                let mut lines = Vec::new();
                render_cell(&cell, 80, &mut lines);
                assert_eq!(lines.len(), 2);
                assert_eq!(lines[0].to_string(), "• Started agent: Review the parser");
                assert_eq!(lines[0].spans[0].style, tool_header[0].spans[0].style);
                assert_eq!(lines[0].spans[1].style, tool_header[0].spans[1].style);
                assert_eq!(lines[0].spans[2].style.fg, Some(Palette::TEXT()));
                if initial.is_empty() {
                    initial = lines;
                } else {
                    assert_eq!(
                        lines, initial,
                        "lifecycle updates must not change launch notices"
                    );
                }
            }
        }
    }

    #[test]
    fn agent_creation_tools_hide_headers_but_preserve_errors() {
        for name in ["task", "explore", "spawn_agent"] {
            for status in [ToolStatus::Running, ToolStatus::Ok, ToolStatus::Error] {
                for expanded in [false, true] {
                    let cell = Cell::Tool {
                        id: "create".into(),
                        name: name.into(),
                        input: json!({"description": "Review the parser"}),
                        status,
                        output: Some("invalid task parameters".into()),
                        expanded,
                    };
                    let mut lines = Vec::new();
                    render_cell(&cell, 80, &mut lines);
                    if status == ToolStatus::Error {
                        assert!(lines
                            .iter()
                            .any(|l| l.to_string().contains("invalid task parameters")));
                        assert_eq!(lines[0].spans[0].style.fg, Some(Palette::ERROR()));
                    } else {
                        assert!(lines.is_empty(), "{name} must not duplicate launch notices");
                    }
                }
            }
        }
    }

    #[test]
    fn successful_creation_renders_one_notice_per_spawn() {
        use bob_core::core::events::AgentEvent;
        let mut view = super::super::view::ViewModel::new();
        view.apply(&AgentEvent::ToolCall {
            agent_id: "root".into(),
            tool_use_id: "create".into(),
            name: "task".into(),
            input: json!({"tasks": [{"description": "Review the parser"}]}),
        });
        view.apply(&AgentEvent::SubagentSpawn {
            agent_id: "job_1".into(),
            parent_id: "root".into(),
            task: "Review the parser".into(),
            prompt: "Review parser correctness".into(),
        });
        view.apply(&AgentEvent::ToolResult {
            agent_id: "root".into(),
            tool_use_id: "create".into(),
            output: "job_1 started".into(),
            is_error: false,
        });
        let mut lines = Vec::new();
        for cell in &view.cells {
            render_cell(cell, 80, &mut lines);
        }
        let text: Vec<_> = lines
            .iter()
            .map(|l| l.to_string())
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(text, ["• Started agent: Review the parser"]);
    }

    #[test]
    fn ordinary_tools_still_render() {
        let cell = Cell::Tool {
            id: "read".into(),
            name: "read_file".into(),
            input: json!({"path": "src/main.rs"}),
            status: ToolStatus::Ok,
            output: Some("fn main() {}".into()),
            expanded: false,
        };
        let mut lines = Vec::new();
        render_cell(&cell, 80, &mut lines);
        assert!(lines
            .iter()
            .any(|line| line.to_string().contains("src/main.rs")));
    }

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
    fn with_left_lead_repeats_lead_on_every_wrapped_row() {
        use ratatui::text::{Line, Span};
        // A single line wider than the content width must wrap into multiple rows,
        // each carrying the lead as its first span (the bug: continuation rows used to
        // lose it after the outer wrapper re-split them).
        let lead = Span::raw("| ");
        let lines = vec![Line::from(Span::raw("aaaa bbbb cccc dddd"))];
        let out = with_left_lead(lines, lead, 8); // content width 6 after the 2-col lead
        assert!(out.len() > 1, "long line should wrap into multiple rows");
        for row in &out {
            assert_eq!(
                row.spans.first().map(|s| s.content.as_ref()),
                Some("| "),
                "every wrapped row must start with the lead"
            );
            // Every emitted row fits the reserved width, so the outer wrapper is a
            // no-op and can't strand the lead.
            assert!(row.width() <= 8, "row exceeds width: {}", row.width());
        }
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
