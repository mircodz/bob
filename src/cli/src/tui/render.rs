use super::diffview::diff_header;
#[cfg(not(test))]
use super::diffview::render_diff;
use super::exploration::is_exploration;
#[cfg(not(test))]
use super::highlight::highlight_line;
use super::indent_line;
#[cfg(not(test))]
use super::markdown::render_markdown;
pub(super) use super::markdown::render_markdown as render_markdown_snippet;
use super::theme::Palette;
use super::view::{Cell, ToolStatus, WfPhase, WfStatus};
use bob_core::core::types::{ShellOutcome, ShellOutput, ToolOutput};
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
        "memory" => {
            let display = if arg("scope") == "global" {
                "Memory (global)"
            } else {
                "Memory"
            };
            let detail = match input
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("save")
            {
                "list" => "list".into(),
                "forget" => format!(
                    "forget #{}",
                    input.get("id").map(Value::to_string).unwrap_or_default()
                ),
                _ => message_preview(&arg("content")),
            };
            (display.into(), detail)
        }
        "ask_user" => ("Question".into(), String::new()),
        "task" => ("Task".into(), String::new()),
        "workflow" => ("Workflow".into(), arg("title")),
        "enter_plan" => ("Plan mode".into(), String::new()),
        "exit_plan" => ("Plan".into(), String::new()),
        "explore" => ("Explore".into(), arg("description")),
        "spawn_agent" => ("Agent".into(), arg("name")),
        "send_message" => ("Message to".into(), arg("to")),
        "stop_agent" => ("Stop agent".into(), arg("name")),
        "list_agents" => ("Agents".into(), String::new()),
        other => (other.to_string(), String::new()),
    }
}

pub(super) fn message_preview(text: &str) -> String {
    let clean = safe_display_text(text);
    let mut chars = clean
        .split_whitespace()
        .flat_map(|word| std::iter::once(' ').chain(word.chars()))
        .skip(1);
    let mut preview: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        preview.push_str("...");
    }
    preview
}

fn communication_fields(
    name: &str,
    input: &Value,
    output: Option<&str>,
    status: ToolStatus,
    expanded: bool,
) -> Option<Vec<(&'static str, String)>> {
    let arg = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("");
    let mut fields = match name {
        "ask_user" => vec![("Q", arg("question").to_owned())],
        "send_message" => vec![("", arg("message").to_owned())],
        _ => return None,
    };
    if status == ToolStatus::Error {
        fields.push(("Error", output.unwrap_or("Tool failed").to_owned()));
    } else if name == "ask_user" {
        if let Some(output) = output {
            let answer = if output == "user dismissed the question without answering" {
                "Dismissed"
            } else {
                output.strip_prefix("user answered: ").unwrap_or(output)
            };
            fields.push(("A", answer.to_owned()));
        }
    } else if expanded {
        if let Some(output) = output {
            fields.push(("Result", output.to_owned()));
        }
    }
    if expanded && name == "ask_user" && !arg("detail").is_empty() {
        fields.push(("Context", arg("detail").to_owned()));
    }
    Some(fields)
}

fn truncate(s: &str, n: usize) -> String {
    let clean = safe_display_text(s).replace('\n', " ");
    if clean.chars().count() > n {
        format!("{}...", clean.chars().take(n).collect::<String>())
    } else {
        clean
    }
}

/// Wrap to display columns, returning at least one row.
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
    // Newlines have zero display width but must split rows before padding.
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

/// Reserve the lead's display columns before wrapping so every visual row keeps it.
pub(super) fn with_left_lead(
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
    compact: bool,
    header: Vec<Span<'static>>,
    command: Option<Vec<Line<'static>>>,
    exit_status: Option<String>,
    output_refs: Vec<String>,
    loader_context: Option<String>,
    shell: bool,
    output: PreparedOutput,
}

enum PreparedOutput {
    None,
    Diff(Vec<Line<'static>>),
    Fields(Vec<(&'static str, String)>),
    Plain { text: String, total: usize },
}

impl PreparedCell {
    pub(super) fn new(cell: &Cell) -> Self {
        Self::prepare(cell, false)
    }

    pub(super) fn compact_tool(cell: &Cell) -> Self {
        Self::prepare(cell, true)
    }

    fn prepare(cell: &Cell, compact: bool) -> Self {
        if !cell.is_visible() {
            return Self::default();
        }
        match cell {
            Cell::Assistant { text, .. } | Cell::Plan(text) => Self {
                content: render_markdown(text),
                tool: None,
            },
            Cell::AgentMsg { from, text } => {
                let mut content = vec![Line::from(vec![
                    Span::styled("• ", Style::default().fg(Palette::ACCENT())),
                    Span::styled(
                        format!("Message from {}", message_preview(from)),
                        Style::default()
                            .fg(Palette::TEXT())
                            .add_modifier(Modifier::BOLD),
                    ),
                ])];
                content.extend(render_markdown(text).into_iter().map(indent_line));
                Self {
                    content,
                    tool: None,
                }
            }
            Cell::Tool {
                name,
                input,
                output,
                expanded,
                status,
                ..
            } => {
                let expanded = if compact {
                    *status == ToolStatus::Error
                } else {
                    *expanded
                };
                let (display, arg) = tool_display(name, input);
                let mut header = vec![Span::styled(
                    display,
                    Style::default()
                        .fg(Palette::TEXT())
                        .add_modifier(Modifier::BOLD),
                )];
                let command = if name == "bash" && !arg.is_empty() {
                    Some(
                        safe_display_text(&arg)
                            .replace('\t', "    ")
                            .split('\n')
                            .map(|line| Line::from(highlight_line(line, "sh")))
                            .collect(),
                    )
                } else {
                    if !arg.is_empty() {
                        header.push(Span::styled(
                            format!(" {}", truncate(&arg, 72)),
                            Style::default().fg(Palette::DIM()),
                        ));
                    }
                    None
                };
                let text_output = output.as_ref().and_then(ToolOutput::as_text);
                let loader_context = match output {
                    Some(ToolOutput::Read { loader_context, .. }) if expanded => loader_context
                        .as_deref()
                        .map(|text| safe_display_text(text).into_owned()),
                    _ => None,
                };
                let shell = name == "bash" || matches!(output, Some(ToolOutput::Shell(_)));
                let (exit_status, body) = match output {
                    Some(ToolOutput::Shell(shell)) => (
                        shell_status(shell),
                        Some(Cow::Owned(shell_display_text(shell))),
                    ),
                    Some(ToolOutput::Read { body, .. }) => {
                        (None, Some(Cow::Borrowed(body.as_str())))
                    }
                    _ => (None, text_output.map(Cow::Borrowed)),
                };
                let output_refs = match output {
                    Some(ToolOutput::Shell(shell)) => {
                        [("stdout", &shell.stdout), ("stderr", &shell.stderr)]
                            .into_iter()
                            .filter_map(|(name, stream)| {
                                stream
                                    .output_id
                                    .as_ref()
                                    .map(|id| format!("{name} truncated; retained output: {id}"))
                            })
                            .collect()
                    }
                    _ => Vec::new(),
                };
                let output = if let Some(fields) =
                    communication_fields(name, input, text_output, *status, expanded)
                {
                    PreparedOutput::Fields(fields)
                } else if *status != ToolStatus::Error
                    && (compact || (!expanded && (is_exploration(name) || name == "memory")))
                {
                    PreparedOutput::None
                } else if let Some(output) = body.as_deref() {
                    let output = safe_display_text(output);
                    if let Some((head, lang, body)) = (!shell && !compact)
                        .then(|| parse_diff_output(&output))
                        .flatten()
                    {
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
                        let limit = if expanded { EXPANDED_MAX } else { PREVIEW };
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
                    tool: Some(PreparedTool {
                        compact,
                        header,
                        command,
                        exit_status,
                        output_refs,
                        loader_context,
                        shell,
                        output,
                    }),
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
        if self.tool.as_ref().is_some_and(|tool| tool.compact) {
            while out.len() > start && out.last().is_some_and(|line| line.width() == 0) {
                out.pop();
            }
        }
    }
}

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
            use unicode_width::UnicodeWidthStr;
            const MARGIN: usize = super::widgets::BAND_MARGIN as usize;
            let bg = Style::default().bg(Palette::INPUT_BG());
            let gutter = || Span::raw(" ".repeat(MARGIN)); // base-bg on both sides
            let band_w = width.saturating_sub(MARGIN * 2).max(1);
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
            status, expanded, ..
        } => {
            render_tool(
                prepared.tool.as_ref().expect("visible tool was prepared"),
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
        Cell::Retry {
            attempt,
            max_attempts,
            ..
        } => {
            out.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(Palette::RUNNING())),
                Span::styled(
                    format!("connection hiccup — retry {attempt} of {max_attempts}"),
                    Style::default().fg(Palette::TEXT()),
                ),
            ]));
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
            let bar = || Span::styled(" ▏ ", Style::default().fg(Palette::ACCENT()));
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
            out.extend(prepared.content.iter().cloned().map(|mut line| {
                line.spans.insert(0, bar());
                line
            }));
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

pub(super) fn wf_dot(status: WfStatus) -> Span<'static> {
    let color = match status {
        WfStatus::Running => Palette::RUNNING(),
        WfStatus::Done => Palette::OK(),
        WfStatus::Failed => Palette::ERROR(),
    };
    Span::styled("•".to_string(), Style::default().fg(color))
}

/// Row order must match `Cell::workflow_agent_at`; completed runs collapse to a summary.
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
    status: ToolStatus,
    expanded: bool,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let expanded = if prepared.compact {
        status == ToolStatus::Error
    } else {
        expanded
    };
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
    if let Some(command) = &prepared.command {
        header.push(Span::raw(" "));
        let indent = header.iter().map(Span::width).sum::<usize>();
        let command_width = width.saturating_sub(4 + indent).max(1);
        let limit = if expanded { usize::MAX } else { PREVIEW };
        let mut rows = command
            .iter()
            .cloned()
            .flat_map(|line| super::wrap_line(line, command_width));
        for (index, row) in rows.by_ref().take(limit).enumerate() {
            let mut spans = if index == 0 {
                header.clone()
            } else {
                vec![Span::raw(" ".repeat(indent))]
            };
            spans.extend(row.spans);
            out.push(Line::from(spans));
        }
        if rows.next().is_some() {
            out.push(Line::from(Span::styled(
                "    … more command (click to expand)",
                Style::default().fg(Palette::FAINT()),
            )));
        }
    } else {
        out.push(Line::from(header));
    }
    if let Some(exit_status) = &prepared.exit_status {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(exit_status.clone(), Style::default().fg(Palette::ERROR())),
        ]));
    }

    for reference in &prepared.output_refs {
        for row in wrap_plain(reference, width.saturating_sub(8).max(1)) {
            out.push(Line::from(Span::styled(
                format!("    {row}"),
                Style::default().fg(Palette::FAINT()),
            )));
        }
    }

    if let Some(context) = &prepared.loader_context {
        let rows = wrap_plain_limited(context, width.saturating_sub(8).max(1), EXPANDED_MAX + 1);
        let clipped = rows.len() > EXPANDED_MAX;
        for row in rows.into_iter().take(EXPANDED_MAX) {
            out.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(row, Style::default().fg(Palette::FAINT())),
            ]));
        }
        if clipped {
            out.push(Line::from(Span::styled(
                "    … loader context display truncated",
                Style::default().fg(Palette::FAINT()),
            )));
        }
    }

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
        PreparedOutput::Fields(fields) => {
            let mut remaining = if expanded { EXPANDED_MAX } else { fields.len() };
            for (label, value) in fields {
                let prefix = if label.is_empty() {
                    "    ".into()
                } else {
                    format!("    {label}  ")
                };
                let content_width = width.saturating_sub(2 + prefix.len()).max(1);
                let rows = if expanded {
                    wrap_plain_limited(
                        &safe_display_text(value),
                        content_width,
                        remaining.saturating_add(1),
                    )
                } else {
                    vec![super::widgets::fit_text(
                        &message_preview(value),
                        content_width,
                    )]
                };
                let clipped = rows.len() > remaining;
                for (index, row) in rows.into_iter().take(remaining).enumerate() {
                    out.push(Line::from(vec![
                        Span::styled(
                            if index == 0 {
                                prefix.clone()
                            } else {
                                " ".repeat(prefix.len())
                            },
                            Style::default().fg(Palette::DIM()),
                        ),
                        Span::styled(
                            row,
                            Style::default().fg(if *label == "Error" {
                                Palette::ERROR()
                            } else {
                                Palette::TEXT()
                            }),
                        ),
                    ]));
                    remaining = remaining.saturating_sub(1);
                }
                if clipped {
                    out.push(Line::from(Span::styled(
                        "    … more text",
                        Style::default().fg(Palette::FAINT()),
                    )));
                    break;
                }
            }
            out.push(Line::from(""));
            return;
        }
        PreparedOutput::Plain { text, total } => (text, *total),
    };

    let is_error = status == ToolStatus::Error && !prepared.shell;
    let color = if is_error {
        Palette::ERROR()
    } else {
        Palette::FAINT()
    };
    let limit = if expanded { EXPANDED_MAX } else { PREVIEW };
    let gutter = prepared.shell;
    // Reserve the scrollback indent and lead so wrapping repeats the gutter.
    let lead: &str = if gutter { "  │ " } else { "    " };
    let content_w = if gutter {
        width.saturating_sub(4 + lead.chars().count()).max(1)
    } else {
        width.saturating_sub(2 + lead.chars().count()).max(8)
    };
    // Bound visual rows, not just logical lines; one long line can fill the viewport.
    let mut rows_used = 0usize;
    let mut shown_lines = 0usize;
    let mut clipped_line = false;
    for line in body.split('\n').take(total) {
        if rows_used >= limit {
            break;
        }
        let remaining = limit - rows_used;
        let rows = wrap_plain_limited(line, content_w, remaining + 1);
        clipped_line = rows.len() > remaining;
        for row in rows.into_iter().take(remaining) {
            out.push(Line::from(vec![
                Span::styled(lead, Style::default().fg(Palette::FAINT())),
                Span::styled(row, Style::default().fg(color)),
            ]));
            rows_used += 1;
        }
        shown_lines += 1;
    }
    let extra = total.saturating_sub(shown_lines);
    if clipped_line || extra > 0 {
        let hint = if clipped_line {
            "... more output".to_owned()
        } else {
            format!("... {extra} more lines")
        };
        out.push(Line::from(Span::styled(
            format!(
                "{lead}{hint}{}",
                if prepared.compact {
                    ""
                } else {
                    " (click to expand)"
                }
            ),
            Style::default().fg(Palette::FAINT()),
        )));
    }
    out.push(Line::from(""));
}

fn shell_status(output: &ShellOutput) -> Option<String> {
    let mut status = Vec::new();
    match output.outcome {
        ShellOutcome::Exited => {}
        ShellOutcome::TimedOut => status.push("timed out".to_owned()),
        ShellOutcome::Cancelled => status.push("cancelled".to_owned()),
    }
    if let Some(code) = output.exit_code.filter(|code| *code != 0) {
        status.push(format!("exit {code}"));
    }
    if let Some(signal) = output.signal {
        status.push(format!("signal {signal}"));
    }
    if output.core_dumped {
        status.push("core dumped".to_owned());
    }
    (!status.is_empty()).then(|| status.join("; "))
}

fn shell_display_text(output: &ShellOutput) -> String {
    let mut text = output.stdout.text.clone();
    if !output.stderr.text.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&output.stderr.text);
    }
    text
}

fn parse_diff_output(output: &str) -> Option<(String, String, String)> {
    let fence = output.find("```diff")?;
    let header = output[..fence].trim().to_string();
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

    #[test]
    fn shell_results_separate_failure_status_from_unchanged_output() {
        let body = "same output\n```diff\n- ordinary command output\n```";
        for (code, signal, core_dumped, outcome, exit) in [
            (Some(0), None, false, ShellOutcome::Exited, None),
            (Some(7), None, false, ShellOutcome::Exited, Some("exit 7")),
            (
                None,
                Some(15),
                false,
                ShellOutcome::Exited,
                Some("signal 15"),
            ),
            (
                None,
                Some(11),
                true,
                ShellOutcome::Exited,
                Some("signal 11; core dumped"),
            ),
            (None, None, false, ShellOutcome::TimedOut, Some("timed out")),
            (
                None,
                None,
                false,
                ShellOutcome::Cancelled,
                Some("cancelled"),
            ),
        ] {
            for expanded in [false, true] {
                let output: ToolOutput = ShellOutput {
                    exit_code: code,
                    signal,
                    core_dumped,
                    outcome,
                    stdout: bob_core::core::types::ShellStream {
                        text: body.into(),
                        total_bytes: body.len() as u64,
                        ..Default::default()
                    },
                    ..Default::default()
                }
                .into();
                let status = if exit.is_some() {
                    ToolStatus::Error
                } else {
                    ToolStatus::Ok
                };
                let cell = Cell::Tool {
                    id: "shell".into(),
                    name: "bash".into(),
                    input: json!({"command": "make check"}),
                    status,
                    output: Some(output.clone()),
                    expanded,
                };
                let before = cell.fingerprint();
                let mut lines = Vec::new();
                render_cell(&cell, 80, &mut lines);
                let mut expected = vec!["• Bash make check".to_owned()];
                if let Some(exit) = exit {
                    expected.push(format!("    {exit}"));
                    assert_eq!(lines[1].spans[1].style.fg, Some(Palette::ERROR()));
                }
                expected.extend(body.lines().map(|line| format!("  │ {line}")));
                expected.push(String::new());
                assert_eq!(
                    lines.iter().map(Line::to_string).collect::<Vec<_>>(),
                    expected
                );
                for row in lines
                    .iter()
                    .filter(|row| row.to_string().starts_with("  │ "))
                {
                    assert_eq!(row.spans[1].style.fg, Some(Palette::FAINT()));
                }
                assert_eq!(
                    lines[0].spans[0].style.fg,
                    Some(if status == ToolStatus::Error {
                        Palette::ERROR()
                    } else {
                        Palette::OK()
                    })
                );
                assert_eq!(cell.fingerprint(), before);
                assert!(matches!(&cell, Cell::Tool { output: Some(raw), .. } if raw == &output));
            }
        }
    }

    #[test]
    fn shell_streams_stay_neutral_and_retention_refs_survive_preview_limits() {
        use bob_core::core::types::ShellStream;
        let mut cell = tool("bash", "", false);
        let shell = ShellOutput {
            exit_code: Some(2),
            stdout: ShellStream {
                text: "out\n".repeat(600),
                total_bytes: 9000,
                truncated: true,
                output_id: Some("output_42".into()),
            },
            stderr: ShellStream {
                text: "diagnostic".into(),
                total_bytes: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        if let Cell::Tool { output, status, .. } = &mut cell {
            *output = Some(shell.clone().into());
            *status = ToolStatus::Error;
        }
        for expanded in [false, true] {
            if let Cell::Tool {
                expanded: value, ..
            } = &mut cell
            {
                *value = expanded;
            }
            let mut lines = Vec::new();
            render_cell(&cell, 100, &mut lines);
            let reference = lines
                .iter()
                .find(|line| line.to_string().contains("output_42"))
                .unwrap();
            assert_eq!(
                reference.to_string(),
                "    stdout truncated; retained output: output_42"
            );
            assert_eq!(reference.spans[0].style.fg, Some(Palette::FAINT()));
            assert!(lines
                .iter()
                .any(|line| line.to_string().contains("more lines")));
        }
        let mut shell = shell;
        shell.stdout.text = "out".into();
        shell.stdout.output_id = None;
        if let Cell::Tool { output, .. } = &mut cell {
            *output = Some(shell.into());
        }
        let mut lines = Vec::new();
        render_cell(&cell, 100, &mut lines);
        assert_eq!(
            lines.iter().map(Line::to_string).collect::<Vec<_>>(),
            [
                "• Bash printf '%s' hello",
                "    exit 2",
                "  │ out",
                "  │ diagnostic",
                ""
            ]
        );
        for line in &lines[2..4] {
            assert_eq!(line.spans[1].style.fg, Some(Palette::FAINT()));
        }
    }

    #[test]
    fn shell_command_expansion_preserves_multiline_unicode_and_long_tokens() {
        let command = format!(
            "python3 - <<'PY'\n\tprint('{}')\n\n{}PY\nprintf final_marker",
            "日本語e\u{301}".repeat(80),
            "print('line')\n".repeat(510),
        );
        for width in [32, 80] {
            let mut cell = Cell::Tool {
                id: "shell".into(),
                name: "bash".into(),
                input: json!({"command": command}),
                status: ToolStatus::Running,
                output: None,
                expanded: false,
            };
            let mut collapsed = Vec::new();
            render_cell(&cell, width, &mut collapsed);
            assert_eq!(collapsed.len(), PREVIEW + 2);
            assert!(collapsed[PREVIEW].to_string().contains("click to expand"));
            if let Cell::Tool { expanded, .. } = &mut cell {
                *expanded = true;
            }
            let prepared = PreparedCell::new(&cell);
            let mut expanded = Vec::new();
            prepared.render(&cell, width, &mut expanded);
            assert_eq!(collapsed[..PREVIEW], expanded[..PREVIEW]);
            let rows = &expanded[..expanded.len() - 1];
            assert!(rows.len() > 500);
            assert!(rows.iter().all(|row| row.width() <= width - 4));
            let recovered: String = rows
                .iter()
                .enumerate()
                .map(|(index, line)| {
                    let text = line.to_string();
                    text.strip_prefix(if index == 0 { "• Bash " } else { "       " })
                        .unwrap()
                        .to_owned()
                })
                .collect();
            assert_eq!(recovered, command.replace('\t', "    ").replace('\n', ""));
            assert_eq!(
                rows.last().unwrap().to_string(),
                "       printf final_marker"
            );
            assert!(matches!(&cell, Cell::Tool { input, .. } if input["command"] == command));
        }
    }

    #[test]
    fn compact_exploration_preserves_wrapped_failure_details() {
        let output = format!(
            "{}final diagnostic",
            "missing file with a long path\n".repeat(10)
        );
        let cell = Cell::Tool {
            id: "read".into(),
            name: "read_file".into(),
            input: json!({"path": "src/file.rs"}),
            status: ToolStatus::Error,
            output: Some(output.clone().into()),
            expanded: false,
        };
        for width in [24, 100] {
            let mut lines = Vec::new();
            PreparedCell::compact_tool(&cell).render(&cell, width, &mut lines);
            let body: String = lines
                .iter()
                .skip(1)
                .map(|line| line.to_string().strip_prefix("    ").unwrap().to_owned())
                .collect();
            assert_eq!(body, output.replace('\n', ""));
            assert!(lines
                .last()
                .unwrap()
                .to_string()
                .contains("final diagnostic"));
        }
    }

    #[test]
    fn expanded_reads_keep_context_visible_independently_of_body_clipping_and_cache() {
        let mut cell = Cell::Tool {
            id: "read".into(),
            name: "read_file".into(),
            input: json!({"path":"file.rs"}),
            status: ToolStatus::Ok,
            output: Some(ToolOutput::Read {
                body: "     1\tfile text\n".repeat(600),
                loader_context: Some(
                    "[!] Instructions omitted; recover with job_output output_7".into(),
                ),
            }),
            expanded: true,
        };
        let rendered = |cell: &Cell| {
            let mut lines = Vec::new();
            render_cell(cell, 100, &mut lines);
            lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        };
        let text = rendered(&cell);
        assert!(text.contains("file text"));
        assert!(text.contains("[!] Instructions omitted; recover with job_output output_7"));
        assert!(text.contains("more lines"));
        let before = cell.fingerprint();
        if let Cell::Tool {
            output: Some(ToolOutput::Read { loader_context, .. }),
            ..
        } = &mut cell
        {
            *loader_context =
                Some("<project-instruction>\nscoped rules\n</project-instruction>".into());
        }
        assert_ne!(cell.fingerprint(), before);
        let text = rendered(&cell);
        assert!(text.contains("scoped rules"));
        assert!(text.contains("</project-instruction>"));
        let before = cell.fingerprint();
        if let Cell::Tool {
            output: Some(ToolOutput::Read { body, .. }),
            ..
        } = &mut cell
        {
            *body = "     1\tupdated body".into();
        }
        assert_ne!(cell.fingerprint(), before);
        assert!(rendered(&cell).contains("updated body"));
        let mut compact = Vec::new();
        PreparedCell::compact_tool(&cell).render(&cell, 100, &mut compact);
        assert_eq!(compact.len(), 1);
    }

    #[test]
    fn prepared_tools_cover_named_rendering_branches() {
        let cell = |name: &str, output: Option<&str>, expanded, status| Cell::Tool {
            id: "tool".into(),
            name: name.into(),
            input: json!({"path": "src/main.rs", "command": "printf '%s' hello"}),
            status,
            output: output.map(ToolOutput::from),
            expanded,
        };
        let long_output = "line\n".repeat(510);
        let visual_output = format!("{}\nlast", "abcdefgh".repeat(7));
        let mut expanded_rows = vec!["• unknown"];
        expanded_rows.extend(vec!["    line"; 500]);
        expanded_rows.extend(["    ... 10 more lines (click to expand)", ""]);
        let cases = [
            (
                "collapsed read retains ordinary tool path",
                cell("read_file", Some("fn main() {}"), false, ToolStatus::Ok),
                80,
                vec!["• Read src/main.rs", ""],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "collapsed directory hides payload",
                cell("list_dir", Some("secret listing"), false, ToolStatus::Ok),
                80,
                vec!["• List src/main.rs", ""],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "collapsed search hides matches",
                cell("grep", Some("src/main.rs:1:match"), false, ToolStatus::Ok),
                80,
                vec!["• Grep", ""],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "expanded search reveals matches",
                cell("grep", Some("src/main.rs:1:match"), true, ToolStatus::Ok),
                80,
                vec!["• Grep", "    src/main.rs:1:match", ""],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "collapsed read keeps failure visible",
                cell(
                    "read_file",
                    Some("file not found"),
                    false,
                    ToolStatus::Error,
                ),
                80,
                vec!["• Read src/main.rs", "    file not found", ""],
                Palette::ERROR(),
                Some(Palette::ERROR()),
            ),
            (
                "expanded read reveals payload",
                cell("read_file", Some("fn main() {}"), true, ToolStatus::Ok),
                80,
                vec!["• Read src/main.rs", "    fn main() {}", ""],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "expanded directory error uses error text",
                cell("list_dir", Some("denied"), true, ToolStatus::Error),
                80,
                vec!["• List src/main.rs", "    denied", ""],
                Palette::ERROR(),
                Some(Palette::ERROR()),
            ),
            (
                "running shell without output",
                cell("bash", None, false, ToolStatus::Running),
                80,
                vec!["• Bash printf '%s' hello", ""],
                Palette::RUNNING(),
                Some(Palette::FAINT()),
            ),
            (
                "unknown tool empty output",
                cell("unknown", Some(""), false, ToolStatus::Ok),
                80,
                vec!["• unknown", ""],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "whitespace-only output has no body",
                cell("unknown", Some(" \n\t\n\u{2003}"), false, ToolStatus::Ok),
                80,
                vec!["• unknown", ""],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "interior whitespace survives sanitized output",
                cell(
                    "unknown",
                    Some("one  \n\n\ttwo\u{1b}[31m 日本語\n\u{1b}]title\u{7}three \n\t\n"),
                    false,
                    ToolStatus::Ok,
                ),
                80,
                vec![
                    "• unknown",
                    "    one  ",
                    "    ",
                    "    \ttwo 日本語",
                    "    three ",
                    "",
                ],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "narrow shell repeats gutter at Unicode display boundary",
                cell(
                    "bash",
                    Some("界e\u{301}界e\u{301}界tail"),
                    false,
                    ToolStatus::Ok,
                ),
                16,
                vec![
                    "• Bash print",
                    "       f '%s",
                    "       ' hel",
                    "       lo",
                    "  │ 界e\u{301}界e\u{301}界",
                    "  │ tail",
                    "",
                ],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "narrow error repeats plain indent",
                cell(
                    "unknown",
                    Some("abcdefghijklmnop"),
                    false,
                    ToolStatus::Error,
                ),
                14,
                vec!["• unknown", "    abcdefgh", "    ijklmnop", ""],
                Palette::ERROR(),
                Some(Palette::ERROR()),
            ),
            (
                "collapsed preview caps logical lines",
                cell(
                    "unknown",
                    Some("1\n2\n3\n4\n5\n6\n7\n"),
                    false,
                    ToolStatus::Ok,
                ),
                80,
                vec![
                    "• unknown",
                    "    1",
                    "    2",
                    "    3",
                    "    4",
                    "    5",
                    "    6",
                    "    ... 1 more lines (click to expand)",
                    "",
                ],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "collapsed shell caps visual rows before next logical line",
                cell("bash", Some(&visual_output), false, ToolStatus::Ok),
                14,
                vec![
                    "• Bash pri",
                    "       ntf",
                    "        '%",
                    "       s' ",
                    "       hel",
                    "       lo",
                    "  │ abcdef",
                    "  │ ghabcd",
                    "  │ efghab",
                    "  │ cdefgh",
                    "  │ abcdef",
                    "  │ ghabcd",
                    "  │ ... more output (click to expand)",
                    "",
                ],
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "expanded preview caps at five hundred rows",
                cell("unknown", Some(&long_output), true, ToolStatus::Ok),
                80,
                expanded_rows,
                Palette::OK(),
                Some(Palette::FAINT()),
            ),
            (
                "diff uses added and removed row styling",
                cell(
                    "edit_file",
                    Some(
                        "edited src/main.rs (+1 -1)\n```diff src/main.rs\n- 1| old\n+ 1| new\n```",
                    ),
                    false,
                    ToolStatus::Ok,
                ),
                80,
                vec![
                    "• Edit src/main.rs",
                    "   edited src/main.rs (+1 -1)",
                    "   -    1 old",
                    "   +    1 new",
                    "",
                ],
                Palette::OK(),
                None,
            ),
        ];
        for (name, cell, width, expected, bullet_color, body_color) in cases {
            let mut lines = Vec::new();
            PreparedCell::new(&cell).render(&cell, width, &mut lines);
            assert_eq!(
                lines.iter().map(Line::to_string).collect::<Vec<_>>(),
                expected,
                "{name}"
            );
            assert_eq!(lines[0].spans[0].style.fg, Some(bullet_color), "{name}");
            assert_eq!(
                lines[0].spans[1].style,
                Style::default()
                    .fg(Palette::TEXT())
                    .add_modifier(Modifier::BOLD),
                "{name}"
            );
            if expected[0].ends_with(" src/main.rs") {
                assert_eq!(lines[0].spans[2].content, " src/main.rs", "{name}");
                assert_eq!(lines[0].spans[2].style.fg, Some(Palette::DIM()), "{name}");
            }
            if let Some(color) = body_color {
                let body_start = if matches!(&cell, Cell::Tool { name, .. } if name == "bash") {
                    lines
                        .iter()
                        .position(|line| line.to_string().starts_with("  │ "))
                        .unwrap_or(lines.len() - 1)
                } else {
                    1
                };
                for line in &lines[body_start..lines.len() - 1] {
                    assert_eq!(line.spans[0].style.fg, Some(Palette::FAINT()), "{name}");
                    if let Some(body) = line.spans.get(1) {
                        assert_eq!(body.style.fg, Some(color), "{name}");
                    }
                }
            } else {
                assert_eq!(lines[1].spans[1].style.fg, Some(Palette::DIM()));
                assert!(lines[1].spans[1]
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD));
                for (row, color, bg) in [
                    (2, Palette::DIFF_REMOVE(), Palette::DIFF_REMOVE_BG()),
                    (3, Palette::DIFF_ADD(), Palette::DIFF_ADD_BG()),
                ] {
                    assert_eq!(lines[row].spans[1].style.fg, Some(color));
                    assert!(lines[row].spans[1]
                        .style
                        .add_modifier
                        .contains(Modifier::BOLD));
                    assert!(lines[row].spans[3..]
                        .iter()
                        .all(|span| span.style.bg == Some(bg)));
                }
            }
        }
    }

    #[test]
    fn prepared_markdown_preserves_styles_and_plan_decoration() {
        let text = "# Heading\n\n**Bold** and *italic* 日本語\n\n> quoted text\n\n```rust\nlet value = 42;\n```\n\n| key | value |\n| --- | --- |\n| a | b |";
        for (cell, prefix) in [
            (
                Cell::Assistant {
                    text: text.into(),
                    open: false,
                },
                "",
            ),
            (Cell::Plan(text.into()), " ▏ "),
        ] {
            let mut lines = Vec::new();
            PreparedCell::new(&cell).render(&cell, 80, &mut lines);
            for expected in [
                "# Heading",
                "Bold and italic 日本語",
                "▏ quoted text",
                "let value = 42;",
                "┌─────┬───────┐",
                "│ key │ value │",
                "│ a   │ b     │",
                "└─────┴───────┘",
            ] {
                assert!(
                    lines
                        .iter()
                        .any(|line| line.to_string() == format!("{prefix}{expected}")),
                    "{expected}"
                );
            }
            for (text, color, modifier) in [
                ("Heading", Palette::HEADING(), Modifier::BOLD),
                ("Bold", Palette::TEXT(), Modifier::BOLD),
                ("italic", Palette::TEXT(), Modifier::ITALIC),
                ("▏ ", Palette::BLOCKQUOTE_BAR(), Modifier::empty()),
            ] {
                let span = lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .find(|span| span.content == text)
                    .unwrap();
                assert_eq!(span.style.fg, Some(color));
                assert!(span.style.add_modifier.contains(modifier));
            }
            assert_eq!(lines.last().unwrap().to_string(), "");
            if !prefix.is_empty() {
                assert_eq!(lines[0].to_string(), " ▏ plan proposed");
                assert_eq!(lines[1].to_string(), " ▏ ");
                assert_eq!(lines[lines.len() - 2].to_string(), " ▏ ");
                for line in &lines[..lines.len() - 1] {
                    assert_eq!(
                        line.spans[0],
                        Span::styled(" ▏ ", Style::default().fg(Palette::ACCENT()))
                    );
                }
                assert!(lines[0].spans[1]
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD));
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
                "edit_file",
                "edited file.rs\n```diff file.rs\n+ 1| let n = 2;\n```",
                false,
            ),
            tool("bash", "hello", false),
            Cell::AgentMsg {
                from: "root".into(),
                text: "Keep **readable** message styling".into(),
            },
        ];
        PREPARE_COUNTS.with(|counts| counts.set((0, 0, 0)));
        let prepared: Vec<_> = cells.iter().map(PreparedCell::new).collect();
        assert_eq!(PREPARE_COUNTS.with(|counts| counts.get()), (3, 1, 1));
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
        assert_eq!(PREPARE_COUNTS.with(|counts| counts.get()), (3, 1, 1));
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
        for (name, text, width, limit, expected) in [
            ("zero limit", "text", 8, 0, vec![]),
            ("empty input", "", 8, 2, vec![""]),
            ("trailing newline", "a\n", 8, 2, vec!["a", ""]),
            ("clipped trailing newline", "a\n", 8, 1, vec!["a"]),
            (
                "wide character in narrow column",
                "日本語",
                1,
                2,
                vec!["日", "本"],
            ),
            (
                "combining mark and tab",
                "e\u{301}\tx",
                1,
                2,
                vec!["e\u{301}\t", "x"],
            ),
            (
                "blank logical row consumes budget",
                "a\n\nbbbb\n ",
                2,
                3,
                vec!["a", "", "bb"],
            ),
        ] {
            assert_eq!(wrap_plain_limited(text, width, limit), expected, "{name}");
        }
    }

    #[test]
    fn rendered_workflow_rows_map_labels_to_agent_ids_and_terminal_states() {
        use super::super::view::WfAgent;
        let agent = |id: &str, label: &str, status, tools| WfAgent {
            agent_id: id.into(),
            label: label.into(),
            status,
            tools,
            model: None,
            tokens: 0,
            duration_secs: None,
            started_unix: 0,
        };
        let mut cell = Cell::Workflow {
            id: "wf-demo".into(),
            title: "Demo".into(),
            phases: vec![
                WfPhase {
                    title: "Gather".into(),
                    index: 0,
                    total: 3,
                    agents: vec![
                        agent("wf-demo.1-a", "gather-1", WfStatus::Running, 1),
                        agent("wf-demo.1-b", "gather-2", WfStatus::Done, 7),
                    ],
                },
                WfPhase {
                    title: "Check".into(),
                    index: 1,
                    total: 2,
                    agents: vec![
                        agent("wf-demo.2-a", "check-1", WfStatus::Failed, 0),
                        agent("wf-demo.2-b", "check-2", WfStatus::Running, 2),
                    ],
                },
            ],
            done: false,
        };
        let mut lines = Vec::new();
        PreparedCell::new(&cell).render(&cell, 80, &mut lines);
        assert_eq!(
            lines.iter().map(Line::to_string).collect::<Vec<_>>(),
            [
                "• Workflow Demo · running",
                "  Gather  1/3",
                "    • gather-1  (1 tool)",
                "    • gather-2  done",
                "  Check  1/2",
                "    • check-1  failed",
                "    • check-2  (2 tools)",
                "",
            ]
        );
        for (row, expected) in [
            None,
            None,
            Some("wf-demo.1-a"),
            Some("wf-demo.1-b"),
            None,
            Some("wf-demo.2-a"),
            Some("wf-demo.2-b"),
            None,
            None,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(cell.workflow_agent_at(row), expected, "row {row}");
        }
        assert_eq!(cell.workflow_agent_at(usize::MAX), None);
        assert_eq!(lines[0].spans[0].style.fg, Some(Palette::RUNNING()));
        for (row, color) in [
            (2, Palette::RUNNING()),
            (3, Palette::OK()),
            (5, Palette::ERROR()),
            (6, Palette::RUNNING()),
        ] {
            assert_eq!(lines[row].spans[1].style.fg, Some(color));
        }
        if let Cell::Workflow { done, .. } = &mut cell {
            *done = true;
        }
        lines.clear();
        PreparedCell::new(&cell).render(&cell, 80, &mut lines);
        assert_eq!(
            lines.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["• Workflow Demo · 2 phases · 4 agents", "",]
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(Palette::OK()));
        for row in 0..9 {
            assert_eq!(cell.workflow_agent_at(row), None, "collapsed row {row}");
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
                output: Some(format!("{raw}\nmore").into()),
                expanded: true,
            },
            Cell::Tool {
                id: "diff".into(),
                name: "edit_file".into(),
                input: json!({"path": raw}),
                status: ToolStatus::Ok,
                output: Some(format!("edited {raw}\n```diff file.rs\n+{raw}\n```").into()),
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
            assert_eq!(
                output.as_ref().and_then(ToolOutput::as_text),
                Some(format!("{raw}\nmore").as_str())
            );
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
                    for output in [None, Some("job_1: finished\nresult text".into())] {
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
                assert_eq!(lines[0].spans[0].style, Style::default().fg(Palette::OK()));
                assert_eq!(
                    lines[0].spans[1].style,
                    Style::default()
                        .fg(Palette::TEXT())
                        .add_modifier(Modifier::BOLD)
                );
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
    fn wrap_plain_breaks_on_embedded_newlines() {
        let rows = wrap_plain("line one\nline two", 40);
        assert_eq!(rows, vec!["line one".to_string(), "line two".to_string()]);

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

        assert_eq!(wrap_plain("x\n", 10), vec!["x".to_string(), "".to_string()]);
    }

    #[test]
    fn with_left_lead_repeats_lead_on_every_wrapped_row() {
        use ratatui::text::{Line, Span};
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
            assert!(row.width() <= 8, "row exceeds width: {}", row.width());
        }
    }

    #[test]
    fn message_preview_flattens_sanitizes_and_bounds_unicode() {
        assert_eq!(
            message_preview("  Check\n\n the\tparser.  "),
            "Check the parser."
        );
        assert_eq!(
            message_preview("\x1b[31mCheck\x1b[0m \x1b]0;hidden title\x07parser"),
            "Check parser"
        );
        assert_eq!(
            message_preview(&"界".repeat(121)),
            format!("{}...", "界".repeat(120))
        );
        assert_eq!(message_preview(&"界".repeat(120)), "界".repeat(120));
        assert_eq!(message_preview(" \n\t"), "");
    }

    #[test]
    fn message_summary_preserves_routing_errors_and_expanded_content() {
        for (status, output) in [
            (ToolStatus::Running, None),
            (ToolStatus::Ok, Some("delivered to 'reviewer'")),
            (ToolStatus::Error, Some("no agent named 'reviewer'")),
        ] {
            for expanded in [false, true] {
                let input = json!({"to":"reviewer", "message":"Check\n the parser and add a regression test."});
                let cell = Cell::Tool {
                    id: "message-1".into(),
                    name: "send_message".into(),
                    input: input.clone(),
                    status,
                    output: output.map(ToolOutput::from),
                    expanded,
                };
                let mut lines = Vec::new();
                render_cell(&cell, 100, &mut lines);
                assert_eq!(lines[0].to_string(), "• Message to reviewer");
                let body = lines[1..]
                    .iter()
                    .map(Line::to_string)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(body.contains("the parser and add a regression test."));
                if status == ToolStatus::Error || expanded {
                    if let Some(output) = output {
                        assert!(body.contains(output));
                    }
                } else {
                    assert_eq!(lines.len(), 3);
                    assert_eq!(
                        lines[1].to_string(),
                        "    Check the parser and add a regression test."
                    );
                }
                if status == ToolStatus::Error {
                    assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
                        span.content.contains("no agent named")
                            && span.style.fg == Some(Palette::ERROR())
                    }));
                }
                assert!(matches!(&cell, Cell::Tool { input: stored, .. } if stored == &input));
            }
        }
    }

    #[test]
    fn compact_questions_fit_unicode_width_and_expand_without_losing_answers() {
        let question = format!("Where should {} question-tail", "界e\u{301}".repeat(60));
        let answer = format!("Keep {} answer-tail", "選択 ".repeat(60));
        let output = format!("user answered: {answer}");
        let input =
            json!({"question":question, "detail":"A full explanation.", "options":["one","two"]});
        for width in [24, 60] {
            let mut cell = Cell::Tool {
                id: "question".into(),
                name: "ask_user".into(),
                input: input.clone(),
                status: ToolStatus::Ok,
                output: Some(output.clone().into()),
                expanded: false,
            };
            let mut lines = Vec::new();
            render_cell(&cell, width, &mut lines);
            assert_eq!(lines.len(), 4);
            assert!(lines[1].to_string().starts_with("    Q  Where should"));
            assert!(lines[2].to_string().starts_with("    A  Keep"));
            assert!(lines.iter().all(|line| line.width() <= width));
            if let Cell::Tool { expanded, .. } = &mut cell {
                *expanded = true;
            }
            lines.clear();
            render_cell(&cell, width, &mut lines);
            let values: String = lines
                .iter()
                .skip(1)
                .flat_map(|line| line.spans.iter().skip(1))
                .map(|span| span.content.as_ref())
                .collect();
            assert!(values.contains(&question));
            assert!(values.contains(&answer));
            assert!(values.contains("A full explanation."));
            assert!(
                matches!(&cell, Cell::Tool { input: stored, output: Some(stored_output), .. }
                if stored == &input && stored_output.as_text() == Some(output.as_str()))
            );
        }
    }

    #[test]
    fn memory_summary_expands_the_result_and_keeps_failures_visible() {
        let input = json!({"content":"Prefer focused tests."});
        let output =
            json!({"remembered":{"id":42,"content":"Prefer focused tests.","scope":"project"}})
                .to_string();
        let mut cell = Cell::Tool {
            id: "memory".into(),
            name: "memory".into(),
            input: input.clone(),
            status: ToolStatus::Ok,
            output: Some(output.clone().into()),
            expanded: false,
        };
        let mut lines = Vec::new();
        render_cell(&cell, 100, &mut lines);
        assert_eq!(
            lines.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["• Memory Prefer focused tests.", ""]
        );
        if let Cell::Tool { expanded, .. } = &mut cell {
            *expanded = true;
        }
        lines.clear();
        render_cell(&cell, 100, &mut lines);
        let body: String = lines
            .iter()
            .skip(1)
            .flat_map(|line| line.spans.iter().skip(1))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(body, output);
        if let Cell::Tool {
            status,
            output,
            expanded,
            ..
        } = &mut cell
        {
            *status = ToolStatus::Error;
            *output = Some("memory storage unavailable".into());
            *expanded = false;
        }
        lines.clear();
        render_cell(&cell, 100, &mut lines);
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "memory storage unavailable" && span.style.fg == Some(Palette::ERROR())
        }));
        assert!(matches!(&cell, Cell::Tool { input: stored, .. } if stored == &input));
    }

    #[test]
    fn user_band_has_exact_unicode_wrapping_padding_and_background() {
        let raw = "界e\u{301}\x1b[31mAB\x1b[0m\nZ\n";
        let cell = Cell::User(raw.into());
        let mut lines = Vec::new();
        render_cell(&cell, 12, &mut lines);
        assert_eq!(
            lines.iter().map(Line::to_string).collect::<Vec<_>>(),
            [
                "            ",
                "   › 界e\u{301}A   ",
                "     B      ",
                "     Z      ",
                "            ",
                "            ",
                "",
            ]
        );
        for line in &lines[..6] {
            assert_eq!(line.width(), 12);
            assert_eq!(line.spans.first().unwrap(), &Span::raw("  "));
            assert_eq!(line.spans.last().unwrap(), &Span::raw("  "));
            for span in &line.spans[1..line.spans.len() - 1] {
                assert_eq!(span.style.bg, Some(Palette::INPUT_BG()));
            }
        }
        for (row, content) in [(1, "界e\u{301}A"), (2, "B"), (3, "Z"), (4, "")] {
            assert_eq!(lines[row].spans[1].style.fg, Some(Palette::DIM()));
            assert_eq!(lines[row].spans[2].content, content);
            assert_eq!(lines[row].spans[2].style.fg, Some(Palette::TEXT()));
        }
        assert!(
            lines[6].spans.is_empty() || lines[6].spans.iter().all(|s| s.style == Style::default())
        );
        assert!(matches!(&cell, Cell::User(text) if text == raw));
    }

    #[test]
    fn sanitizer_keeps_control_state_at_every_unicode_span_boundary() {
        let style = Style::default()
            .fg(Palette::ERROR())
            .add_modifier(Modifier::ITALIC);
        for sequence in [
            "\x1b[31m",
            "\x1b(B",
            "\x1b]52;c;秘密\x1b\\",
            "\u{90}hidden\x07still hidden\u{9c}",
            "\x1b]hidden\x1b[2Jstill hidden\x07",
            "\x1b[31\u{18}",
            "\x1bPdiscard\u{1a}",
        ] {
            let raw = format!("前{sequence}e\u{301}後");
            for split in raw
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(raw.len()))
            {
                let mut lines = vec![Line::from(vec![
                    Span::styled(raw[..split].to_owned(), style),
                    Span::styled(raw[split..].to_owned(), style),
                ])];
                sanitize_display_lines(&mut lines);
                assert_eq!(
                    lines[0].to_string(),
                    "前e\u{301}後",
                    "{sequence:?} split at {split}"
                );
                assert_eq!(lines[0].spans.len(), 2);
                assert!(lines[0].spans.iter().all(|span| span.style == style));
            }
        }
    }
}
