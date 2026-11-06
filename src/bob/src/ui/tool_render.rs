use super::render::{append, Row};
use super::state::{Anchor, AnchorId, HitAction};
use super::style::{bold, safe, text, wrap, BAND_INSET};
use bob_kernel::{Entry, EntryKind, OperationState};
use bob_ui::{diffview, highlight::highlight_line, theme::Palette};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::Value;

const PREVIEW: usize = 6;
const EXPANDED_MAX: usize = 500;

#[path = "archive_recall.rs"]
mod archive_recall;

#[path = "read_presentation.rs"]
mod read_presentation;
pub(super) use read_presentation::summary as read_summary;

fn arg<'a>(input: &'a Value, key: &str) -> &'a str {
    input.get(key).and_then(Value::as_str).unwrap_or("")
}

fn aliased_arg<'a>(input: &'a Value, key: &str, alias: &str) -> &'a str {
    input[key].as_str().or(input[alias].as_str()).unwrap_or("")
}

fn preview(value: &str, limit: usize) -> String {
    let clean = safe(value).split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = clean.chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push_str("...");
    }
    result
}

pub(super) fn display(name: &str, input: &Value) -> (String, String) {
    let (name, detail) = match name {
        "read_file" => ("Read", arg(input, "path")),
        "search_conversation" => ("Search conversations", arg(input, "query")),
        "read_conversation" => {
            return ("Read conversation".into(), archive_recall::detail(input));
        }
        "read_history" => (
            "Read history",
            input["thread_id"].as_str().unwrap_or("self"),
        ),
        "write_file" => ("Write", arg(input, "path")),
        "edit_file" | "multi_edit" => ("Edit", arg(input, "path")),
        "list_dir" => ("List", arg(input, "path")),
        "glob" => ("Glob", arg(input, "pattern")),
        "grep" => ("Grep", arg(input, "pattern")),
        "bash" => ("Bash", arg(input, "command")),
        "web_fetch" => ("Fetch", arg(input, "url")),
        "web_search" => ("Search", arg(input, "query")),
        "todo_write" | "exit_plan" => ("Plan", ""),
        "enter_plan" => ("Plan mode", ""),
        "ask_user" => ("Question", ""),
        "task" => ("Task", ""),
        "workflow" => ("Workflow", arg(input, "title")),
        "explore" => ("Explore", arg(input, "description")),
        "spawn_agent" | "launch_agent" => ("Agent", arg(input, "name")),
        "send_message" => ("Message to", aliased_arg(input, "to", "thread_id")),
        "stop_agent" => ("Stop agent", arg(input, "name")),
        "list_agents" => ("Agents", ""),
        "wait_agent" => ("Wait for agent", arg(input, "thread_id")),
        "wait_message" => ("Wait for reply", arg(input, "from")),
        "memory" => {
            let name = if arg(input, "scope") == "global" {
                "Memory (global)"
            } else {
                "Memory"
            };
            let detail = match arg(input, "action") {
                "list" => "list".into(),
                "forget" => format!(
                    "forget #{}",
                    input
                        .get("id")
                        .and_then(Value::as_i64)
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                ),
                _ => preview(arg(input, "content"), 120),
            };
            return (name.into(), detail);
        }
        other => (other, ""),
    };
    (name.into(), detail.into())
}

fn compact_result(source: &str) -> String {
    fn fields(value: &Value) -> Vec<String> {
        match value {
            Value::String(text) => vec![text.clone()],
            Value::Array(entries) => entries.iter().flat_map(fields).collect(),
            Value::Object(object) => {
                let mut result = Vec::new();
                for key in [
                    "answer", "message", "content", "summary", "error", "text", "entries",
                    "memories",
                ] {
                    if let Some(value) = object.get(key) {
                        result.extend(fields(value));
                    }
                }
                if result.is_empty() {
                    if let Some(id) = object.get("id").and_then(Value::as_i64) {
                        result.push(format!("Memory #{id}"));
                    }
                }
                result
            }
            _ => Vec::new(),
        }
    }
    match serde_json::from_str::<Value>(source) {
        Ok(value) => {
            let fields = fields(&value);
            if fields.is_empty() {
                "Result available in inspector".into()
            } else {
                fields.join("\n")
            }
        }
        Err(_) if source.trim_start().starts_with(['{', '[']) => {
            "Result available in inspector".into()
        }
        Err(_) => safe(source),
    }
}

fn work_status(value: &Value) -> &str {
    value["status"]
        .as_str()
        .or(value["state"].as_str())
        .unwrap_or("unknown")
}

fn work_style(status: &str) -> Style {
    Style::default().fg(match status {
        "success" | "completed" | "done" => Palette::OK(),
        "failed" => Palette::ERROR(),
        "cancelled" => Palette::DIM(),
        "running" | "queued" | "pending" | "active" => Palette::RUNNING(),
        _ => Palette::WARN(),
    })
}

fn work_failed(value: &Value) -> bool {
    matches!(
        work_status(value),
        "failed" | "cancelled" | "unknown" | "interrupted"
    ) || value.get("error").is_some_and(|error| !error.is_null())
}

struct WorkResult {
    lines: Vec<Line<'static>>,
    failed: bool,
    structured: bool,
}

impl WorkResult {
    fn line(&mut self, label: &str, value: &str, style: Style) {
        for (index, line) in safe(value).lines().enumerate() {
            if self.lines.len() > EXPANDED_MAX {
                break;
            }
            self.lines.push(Line::from(vec![
                Span::styled(
                    if index == 0 { safe(label) } else { "  ".into() },
                    Style::default().fg(Palette::DIM()),
                ),
                Span::styled(line.to_owned(), style),
            ]));
        }
    }

    fn value(&mut self, label: &str, value: &Value, depth: usize) {
        if self.lines.len() > EXPANDED_MAX || value.is_null() {
            return;
        }
        if depth == 12 {
            self.line(
                label,
                "Nested output retained in full output",
                work_style("unknown"),
            );
            return;
        }
        match value {
            Value::Object(object)
                if object.contains_key("path") && object.contains_key("state") =>
            {
                self.line(
                    "",
                    &format!("{} · {}", arg(value, "path"), work_status(value)),
                    work_style(work_status(value)),
                );
                if let Some(error) = value["error"].as_str() {
                    self.line("  Error: ", error, work_style("failed"));
                }
                if let Some(artifact) = value["result_artifact"].as_str() {
                    self.line("  Retained output: ", artifact, text());
                }
            }
            Value::Object(object) => {
                if !label.is_empty() {
                    self.line("", label.trim_end_matches(": "), text());
                }
                for key in ["status", "error"] {
                    if let Some(value) = object.get(key).filter(|v| !v.is_null()) {
                        let style = if key == "error" {
                            work_style("failed")
                        } else {
                            work_style(value.as_str().unwrap_or("unknown"))
                        };
                        self.line(
                            &format!("{}{key}: ", "  ".repeat(depth + 1)),
                            value.as_str().unwrap_or("See full output"),
                            style,
                        );
                    }
                }
                for (key, child) in object {
                    if matches!(key.as_str(), "status" | "error")
                        || (key == "results" && object.contains_key("outcomes"))
                    {
                        continue;
                    }
                    self.value(
                        &format!(
                            "{}{}: ",
                            "  ".repeat(depth + 1),
                            safe(key).replace('_', " ")
                        ),
                        child,
                        depth + 1,
                    );
                    if self.lines.len() > EXPANDED_MAX {
                        break;
                    }
                }
            }
            Value::Array(values) => {
                for (index, child) in values.iter().enumerate() {
                    self.value(&format!("{label}{}: ", index + 1), child, depth + 1);
                    if self.lines.len() > EXPANDED_MAX {
                        break;
                    }
                }
                if values.is_empty() {
                    self.line(label, "None", text());
                }
            }
            Value::String(value) => {
                if let Ok(nested) = serde_json::from_str::<Value>(value) {
                    if nested.is_object() || nested.is_array() {
                        self.value(label, &nested, depth + 1);
                        return;
                    }
                }
                self.line(label, value, text());
            }
            value => self.line(label, &value.to_string(), text()),
        }
    }

    fn agent(&mut self, agent: &Value, output: bool) {
        if self.lines.len() > EXPANDED_MAX {
            return;
        }
        let name = agent["name"]
            .as_str()
            .or(agent["thread_id"].as_str())
            .unwrap_or("Agent");
        self.lines.push(Line::from(vec![
            Span::styled("• ", work_style(work_status(agent))),
            Span::styled(safe(name), text()),
            Span::styled(
                format!(" · {}", safe(work_status(agent))),
                work_style(work_status(agent)),
            ),
        ]));
        if output {
            self.value("  ", &agent["output"], 0);
        }
        if agent["output_omitted"] == true {
            self.line(
                "  ",
                "Output omitted from aggregate; inspect child thread",
                work_style("unknown"),
            );
            if let Some(artifact) = agent["result_artifact"].as_str() {
                self.line("  Retained output: ", artifact, text());
            }
        }
    }
}

fn work_result(name: &str, source: &str) -> Option<WorkResult> {
    if !matches!(
        name,
        "task"
            | "explore"
            | "workflow"
            | "spawn_agent"
            | "launch_agent"
            | "list_agents"
            | "stop_agent"
            | "wait_agent"
    ) {
        return None;
    }
    let mut result = WorkResult {
        lines: Vec::new(),
        failed: false,
        structured: false,
    };
    let value = match serde_json::from_str::<Value>(source) {
        Ok(value) => value,
        Err(_) => {
            if name == "wait_agent" {
                if let Some(status) = source
                    .strip_prefix("Agent ")
                    .and_then(|source| source.split_once(' '))
                    .and_then(|(_, report)| {
                        report.strip_suffix(". Its report is queued in your context.")
                    })
                {
                    result.failed = matches!(status, "failed" | "cancelled");
                    result.line("", source, work_style(status));
                    return Some(result);
                }
            }
            result.structured = source.trim_start().starts_with(['{', '[']);
            result.line(
                "",
                if result.structured {
                    "Result preview incomplete; inspect full output"
                } else {
                    source
                },
                text(),
            );
            return Some(result);
        }
    };
    result.structured = value.is_object() || value.is_array();
    let agents = value["agents"].as_array();
    if let Some(agents) = agents {
        let mut counts = Vec::new();
        for status in [
            "completed",
            "failed",
            "cancelled",
            "unknown",
            "interrupted",
            "running",
            "queued",
            "active",
            "idle",
            "held",
            "stopping",
        ] {
            let count = agents
                .iter()
                .filter(|agent| work_status(agent) == status)
                .count();
            if count != 0 {
                counts.push(format!("{count} {status}"));
            }
        }
        result.failed =
            name != "list_agents" && (work_failed(&value) || agents.iter().any(work_failed));
        let status = if name == "list_agents" {
            format!("{} agents", agents.len())
        } else {
            work_status(&value).into()
        };
        result.line(
            "",
            &format!(
                "{status}{}",
                if counts.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", counts.join(" · "))
                }
            ),
            if result.failed {
                work_style("failed")
            } else if name == "list_agents" {
                text()
            } else {
                work_style(&status)
            },
        );
        if name == "list_agents" && !value["next_cursor"].is_null() {
            result.line("", "More agents available on the next page", text());
        }
        if let Some(error) = value["error"].as_str() {
            result.line("Error: ", error, work_style("failed"));
        }
        // Failed reports precede potentially long successful outputs in the preview.
        for agent in agents.iter().filter(|agent| work_failed(agent)) {
            result.agent(agent, name != "list_agents");
            if result.lines.len() > EXPANDED_MAX {
                break;
            }
        }
        if name == "workflow" {
            result.value("Output: ", &value["output"], 0);
        }
        for agent in agents.iter().filter(|agent| !work_failed(agent)) {
            if result.lines.len() > EXPANDED_MAX {
                break;
            }
            result.agent(agent, matches!(name, "task" | "explore"));
        }
    } else if value.get("status").is_some() {
        result.failed = work_failed(&value);
        result.value("", &value, 0);
    } else if let Some(message) = value["message"].as_str() {
        result.line("", message, text());
    } else {
        result.value("", &value, 0);
    }
    Some(result)
}

fn push_wrapped(
    rows: &mut Vec<Row>,
    entry: &Entry,
    lines: impl IntoIterator<Item = Line<'static>>,
    prefix: &str,
    width: usize,
    limit: usize,
) -> bool {
    push_wrapped_counted(rows, entry, lines, prefix, width, limit).is_some()
}

/// Where a row limit cut the output: whole lines left out, and whether a line was cut mid-wrap.
#[derive(Clone, Copy)]
struct Cut {
    lines: usize,
    partial: bool,
}

fn push_wrapped_counted(
    rows: &mut Vec<Row>,
    entry: &Entry,
    lines: impl IntoIterator<Item = Line<'static>>,
    prefix: &str,
    width: usize,
    limit: usize,
) -> Option<Cut> {
    let identity = AnchorId::Entry(entry.sequence);
    let base = rows
        .last()
        .filter(|row| row.anchor.identity == identity)
        .map_or(0, |row| row.anchor.line + 1);
    let hit = entry
        .tool
        .as_ref()
        .map_or(HitAction::Entry(entry.sequence), |tool| {
            HitAction::Tool(tool.id.clone())
        });
    let mut used = 0;
    let mut lines = lines.into_iter().enumerate();
    while let Some((line, content)) = lines.next() {
        for (index, (offset, mut content)) in wrap(&content, width).into_iter().enumerate() {
            if used == limit {
                return Some(Cut {
                    lines: lines.count() + usize::from(index == 0),
                    partial: index > 0,
                });
            }
            content.spans.insert(
                0,
                Span::styled(prefix.to_owned(), Style::default().fg(Palette::FAINT())),
            );
            rows.push(Row {
                anchor: Anchor {
                    identity: identity.clone(),
                    line: base + line,
                    offset,
                },
                line: content,
                band: false,
                hit: hit.clone(),
            });
            used += 1;
        }
    }
    None
}

fn diff(source: &str, file_path: Option<&str>) -> Option<Vec<Line<'static>>> {
    if let Some(path) = file_path {
        let source_lines: Vec<_> = source.trim_end_matches('\n').split('\n').collect();
        let start = source_lines.windows(2).position(|pair| {
            pair[0].split_once(" -").is_some_and(|(added, removed)| {
                added
                    .strip_prefix('+')
                    .is_some_and(|count| count.parse::<usize>().is_ok())
                    && removed.parse::<usize>().is_ok()
            }) && pair[1].starts_with("@@ -")
        });
        if let Some(start) = start {
            let language = std::path::Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("");
            let mut lines: Vec<_> = source_lines[..=start]
                .iter()
                .map(|line| diffview::diff_header(line))
                .collect();
            for raw in &source_lines[start + 1..] {
                if raw.starts_with(['+', '-', ' ']) {
                    // Adapt bare unified rows to the shared numbered format without
                    // letting indentation or source pipes become a line-number gutter.
                    let row = format!("{} | {}", &raw[..1], &raw[1..]);
                    lines.extend(diffview::render_diff(&row, language));
                } else {
                    lines.push(Line::styled(
                        (*raw).to_owned(),
                        Style::default().fg(Palette::DIM()),
                    ));
                }
            }
            return Some(lines);
        }
    }
    let (header, rest) = source.split_once("```diff")?;
    let (language, rest) = rest.split_once('\n')?;
    let (body, trailing) = rest.split_once("```").unwrap_or((rest, ""));
    let mut lines = Vec::new();
    if !header.trim().is_empty() {
        lines.extend(header.trim().lines().map(diffview::diff_header));
    }
    lines.extend(diffview::render_diff(
        body.trim_end_matches('\n'),
        language.trim(),
    ));
    lines.extend(
        trailing
            .trim_matches('\n')
            .lines()
            .map(|line| Line::styled(line.to_owned(), Style::default().fg(Palette::DIM()))),
    );
    Some(lines)
}

pub(super) enum BashInputState<'a> {
    Unavailable,
    Loading,
    Failed(&'a str),
}

pub(super) fn tool(
    rows: &mut Vec<Row>,
    entry: &Entry,
    result: Option<&Entry>,
    expanded: bool,
    width: u16,
) {
    tool_with_input_state(
        rows,
        entry,
        result,
        expanded,
        width,
        BashInputState::Unavailable,
    );
}

pub(super) fn tool_with_input_state(
    rows: &mut Vec<Row>,
    entry: &Entry,
    result: Option<&Entry>,
    expanded: bool,
    width: u16,
    input_state: BashInputState<'_>,
) {
    let Some(tool) = &entry.tool else { return };
    let input = &tool.input_preview;
    let presentation = tool.presentation.as_ref();
    let exit_code = presentation
        .and_then(|p| p.exit_code)
        .filter(|code| *code != 0);
    let recall = archive_recall::handles(&tool.name);
    let work = result.and_then(|result| {
        archive_recall::result(
            &tool.name,
            &result.text,
            result.kind == EntryKind::ToolError
                || entry.kind == EntryKind::ToolError
                || tool.state == OperationState::Failed,
        )
        .or_else(|| work_result(&tool.name, &result.text))
    });
    let failed = work.as_ref().is_some_and(|work| work.failed)
        || result.is_some_and(|result| result.kind == EntryKind::ToolError)
        || entry.kind == EntryKind::ToolError
        || tool.state == OperationState::Failed
        || exit_code.is_some();
    let launched = matches!(tool.name.as_str(), "launch_agent" | "spawn_agent")
        && tool.state == OperationState::Done
        && !failed;
    let hit = if launched {
        HitAction::Entry(entry.sequence)
    } else {
        HitAction::Tool(tool.id.clone())
    };
    let content_width = width.saturating_sub(BAND_INSET * 2).max(1) as usize;
    let body_width = content_width.saturating_sub(4).max(1);
    let color = if failed {
        Palette::ERROR()
    } else {
        match tool.state {
            OperationState::Done | OperationState::Resolved => Palette::OK(),
            OperationState::Cancelled => Palette::DIM(),
            _ => Palette::RUNNING(),
        }
    };
    let (name, detail) = display(&tool.name, input);
    let mut header = vec![
        Span::styled("• ", Style::default().fg(color)),
        Span::styled(
            if launched {
                "Started agent:".into()
            } else {
                name
            },
            bold(),
        ),
    ];
    let shell = tool.name == "bash";
    let mut command_clipped = false;
    if launched {
        let label = ["description", "task", "name"]
            .into_iter()
            .map(|key| arg(input, key))
            .find(|value| !value.trim().is_empty())
            .unwrap_or("");
        header.push(Span::styled(
            format!(
                " {}",
                preview(label, content_width.saturating_sub(17).min(72))
            ),
            text(),
        ));
        append(
            rows,
            entry.sequence,
            vec![Line::from(header), Line::default()],
            false,
            hit,
            width,
        );
        return;
    } else if shell && !detail.is_empty() {
        header.push(Span::raw(" "));
        let command_width = content_width.saturating_sub(7).max(1);
        let command = safe(&detail);
        let limit = if expanded { usize::MAX } else { PREVIEW };
        let base = rows
            .last()
            .filter(|row| row.anchor.identity == AnchorId::Entry(entry.sequence))
            .map_or(0, |row| row.anchor.line + 1);
        let mut used = 0;
        'command: for (line, source) in command.split('\n').enumerate() {
            for (offset, content) in wrap(&Line::from(highlight_line(source, "sh")), command_width)
            {
                if used == limit {
                    command_clipped = true;
                    break 'command;
                }
                let mut spans = if used == 0 {
                    header.clone()
                } else {
                    vec![Span::raw("       ")]
                };
                spans.extend(content.spans);
                rows.push(Row {
                    anchor: Anchor {
                        identity: AnchorId::Entry(entry.sequence),
                        line: base + line,
                        offset,
                    },
                    line: Line::from(spans),
                    band: false,
                    hit: hit.clone(),
                });
                used += 1;
            }
        }
    } else {
        if !detail.is_empty() {
            header.push(Span::styled(
                format!(" {}", preview(&detail, 72)),
                Style::default().fg(Palette::DIM()),
            ));
        }
        if !failed {
            if let Some(summary) = read_summary(tool) {
                header.push(Span::styled(
                    format!(" · {summary}"),
                    Style::default().fg(Palette::DIM()),
                ));
            }
        }
        append(
            rows,
            entry.sequence,
            vec![Line::from(header)],
            false,
            hit.clone(),
            width,
        );
    }
    let mut status = Vec::new();
    if tool.name == "workflow" {
        status.push(Line::styled(
            "    click / F2 inspect workflow · F3 full output · Shift+F3 input",
            Style::default().fg(Palette::FAINT()),
        ));
    }
    if shell && input["preview_truncated"] == true {
        let (message, color) = if !expanded {
            (
                "… command preview truncated (click to expand)".into(),
                Palette::FAINT(),
            )
        } else {
            match input_state {
                BashInputState::Loading => (
                    "… command preview truncated · loading full command…".into(),
                    Palette::FAINT(),
                ),
                BashInputState::Unavailable => (
                    "… command preview truncated · full command not available".into(),
                    Palette::FAINT(),
                ),
                BashInputState::Failed(error) => (
                    format!(
                        "… command preview truncated · full command unavailable: {}",
                        preview(error, 160)
                    ),
                    Palette::ERROR(),
                ),
            }
        };
        status.push(Line::styled(
            format!("    {message}"),
            Style::default().fg(color),
        ));
    } else if command_clipped {
        status.push(Line::styled(
            "    … more command (click to expand)",
            Style::default().fg(Palette::FAINT()),
        ));
    }
    if let Some(code) = exit_code {
        status.push(Line::styled(
            format!("    exit {code}"),
            Style::default().fg(Palette::ERROR()),
        ));
    } else if failed {
        status.push(Line::styled(
            "    failed",
            Style::default().fg(Palette::ERROR()),
        ));
    }
    let state = match tool.state {
        OperationState::Unknown => Some("unknown outcome"),
        OperationState::Interrupted => Some("interrupted"),
        OperationState::Cancelled => Some("cancelled"),
        OperationState::Approval => Some("approval needed"),
        _ => None,
    };
    if let Some(state) = state {
        status.push(Line::styled(
            format!("    {state}"),
            Style::default().fg(if tool.state == OperationState::Approval {
                Palette::WARN()
            } else {
                Palette::ERROR()
            }),
        ));
    }
    append(rows, entry.sequence, status, false, hit.clone(), width);
    let output = result.map(|result| result.text.as_str());
    let communication = matches!(tool.name.as_str(), "ask_user" | "send_message" | "memory");
    let mut truncated = false;
    let mut hidden_lines = None;
    let history = tool.name == "read_history" && !failed;
    let full_work_output =
        work.as_ref().is_some_and(|work| work.structured) || (history && output.is_some());
    if history {
        if let Some(output) = output {
            let value = serde_json::from_str::<Value>(output).ok();
            let summary = value
                .as_ref()
                .and_then(|value| value["entries"].as_array())
                .map_or_else(
                    || "Retained history page".to_owned(),
                    |entries| format!("{} history entries", entries.len()),
                );
            let more = value.as_ref().is_some_and(|value| {
                !value["next_cursor"].is_null() || !value["next_offset"].is_null()
            });
            push_wrapped(
                rows,
                entry,
                [Line::styled(
                    format!(
                        "{summary}{}",
                        if more { " · more pages available" } else { "" }
                    ),
                    Style::default().fg(Palette::FAINT()),
                )],
                "    ",
                body_width,
                PREVIEW,
            );
        }
    } else if let Some(work) = work {
        truncated = push_wrapped(
            rows,
            entry,
            work.lines,
            "    ",
            body_width,
            if expanded && recall {
                archive_recall::EXPANDED_ROWS
            } else if expanded {
                EXPANDED_MAX
            } else {
                PREVIEW
            },
        ) || (!recall && result.is_some_and(|result| result.is_preview));
    } else if communication {
        let mut fields: Vec<(&str, String)> = Vec::new();
        match tool.name.as_str() {
            "ask_user" => {
                fields.push(("Q", arg(input, "question").into()));
                if let Some(output) = output {
                    let answer = compact_result(output);
                    fields.push((
                        if failed { "Error" } else { "A" },
                        if answer == "user dismissed the question without answering" {
                            "Dismissed".into()
                        } else {
                            answer
                                .strip_prefix("user answered: ")
                                .unwrap_or(&answer)
                                .into()
                        },
                    ));
                }
                if expanded && !arg(input, "detail").is_empty() {
                    fields.push(("Context", arg(input, "detail").into()));
                }
            }
            "send_message" => {
                fields.push(("", aliased_arg(input, "message", "text").into()));
                if expanded || failed {
                    if let Some(output) = output {
                        fields.push((
                            if failed { "Error" } else { "Result" },
                            compact_result(output),
                        ));
                    }
                }
            }
            _ => {
                if expanded || failed || arg(input, "action") == "list" {
                    if let Some(output) = output {
                        fields.push((if failed { "Error" } else { "" }, compact_result(output)));
                    }
                }
            }
        }
        let mut remaining = if expanded { EXPANDED_MAX } else { PREVIEW };
        for (label, value) in fields {
            let prefix = if label.is_empty() {
                "    ".into()
            } else {
                format!("    {label}  ")
            };
            let value = if expanded {
                safe(&value)
            } else {
                bob_ui::widgets::fit_text(
                    &preview(&value, 120),
                    content_width.saturating_sub(prefix.len()).max(1),
                )
            };
            let start = rows.len();
            let clipped = push_wrapped(
                rows,
                entry,
                value
                    .split('\n')
                    .map(|line| Line::styled(line.to_owned(), text())),
                &" ".repeat(prefix.len()),
                content_width.saturating_sub(prefix.len()).max(1),
                if expanded {
                    remaining
                } else {
                    remaining.min(1)
                },
            );
            if let Some(row) = rows.get_mut(start) {
                row.line.spans[0] = Span::styled(prefix, Style::default().fg(Palette::DIM()));
            }
            remaining = remaining.saturating_sub(rows.len() - start);
            truncated |= clipped && expanded;
        }
    } else if !expanded
        && !failed
        && matches!(tool.state, OperationState::Done | OperationState::Resolved)
        && matches!(
            tool.name.as_str(),
            "read_file" | "grep" | "glob" | "list_dir"
        )
        && (output.is_some() || (tool.name == "read_file" && presentation.is_some()))
    {
    } else {
        if let Some(summary) = presentation
            .and_then(|p| p.summary.as_deref())
            .filter(|summary| !summary.is_empty() && (tool.name != "read_file" || failed))
        {
            push_wrapped(
                rows,
                entry,
                safe(summary).split('\n').map(|line| {
                    Line::styled(
                        line.to_owned(),
                        Style::default().fg(if failed && !shell {
                            Palette::ERROR()
                        } else {
                            Palette::FAINT()
                        }),
                    )
                }),
                "    ",
                body_width,
                PREVIEW,
            );
        }
        if let Some(output) = output {
            let body_preview = presentation.and_then(|p| p.body_preview.as_deref());
            let structured = serde_json::from_str::<Value>(output)
                .is_ok_and(|value| value.is_object() || value.is_array());
            let source = if !expanded || structured {
                body_preview.unwrap_or(output)
            } else {
                output
            };
            let source = safe(source);
            let limit = if expanded { EXPANDED_MAX } else { PREVIEW };
            let lead = if shell { "  │ " } else { "    " };
            if !source.trim_end().is_empty() {
                let lines = if !shell {
                    diff(
                        &source,
                        matches!(
                            tool.name.as_str(),
                            "edit_file" | "multi_edit" | "write_file"
                        )
                        .then(|| arg(input, "path")),
                    )
                } else {
                    None
                };
                let cut = if let Some(lines) = lines {
                    push_wrapped_counted(rows, entry, lines, lead, body_width, limit)
                } else {
                    push_wrapped_counted(
                        rows,
                        entry,
                        source.trim_end().split('\n').map(|line| {
                            Line::styled(
                                line.to_owned(),
                                Style::default().fg(if failed && !shell {
                                    Palette::ERROR()
                                } else {
                                    Palette::FAINT()
                                }),
                            )
                        }),
                        lead,
                        body_width,
                        limit,
                    )
                };
                truncated = cut.is_some();
                hidden_lines = cut.filter(|cut| cut.lines > 0 && !cut.partial);
            }
            if result.is_some_and(|result| result.is_preview)
                || (expanded && structured)
                || (!expanded
                    && !structured
                    && body_preview.is_some_and(|body| !body.is_empty())
                    && source != safe(output))
            {
                truncated = true;
                hidden_lines = None;
            }
        } else if tool.state == OperationState::Done && tool.result.is_some() {
            append(
                rows,
                entry.sequence,
                vec![Line::styled(
                    "    Output outside this page · click to expand",
                    Style::default().fg(Palette::FAINT()),
                )],
                false,
                HitAction::Output(tool.id.clone()),
                width,
            );
        }
    }
    let mut footer = Vec::new();
    if truncated {
        footer.push(Line::styled(
            format!(
                "{}{}",
                if shell { "  │ " } else { "    " },
                if expanded {
                    "… display truncated · click to read full output".to_owned()
                } else if let Some(cut) = hidden_lines {
                    format!(
                        "... {} more line{} (click to expand)",
                        cut.lines,
                        if cut.lines == 1 { "" } else { "s" }
                    )
                } else {
                    "... more output (click to expand)".to_owned()
                }
            ),
            Style::default().fg(Palette::FAINT()),
        ));
    }
    if full_work_output && !truncated {
        footer.push(Line::styled(
            "    Full output · click to inspect",
            Style::default().fg(Palette::FAINT()),
        ));
    }
    footer.push(Line::default());
    let footer_hit = if (truncated && expanded) || (full_work_output && !truncated) {
        HitAction::Output(tool.id.clone())
    } else {
        hit
    };
    let base = rows
        .last()
        .filter(|row| row.anchor.identity == AnchorId::Entry(entry.sequence))
        .map_or(0, |row| row.anchor.line + 1);
    for (line, content) in footer.into_iter().enumerate() {
        rows.push(Row {
            anchor: Anchor {
                identity: AnchorId::Entry(entry.sequence),
                line: base + line,
                offset: 0,
            },
            line: content,
            band: false,
            hit: footer_hit.clone(),
        });
    }
}

#[cfg(test)]
#[path = "work_result_tests.rs"]
mod work_result_tests;
