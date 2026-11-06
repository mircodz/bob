use super::{
    render::{self, Row},
    state::{HitAction, Pane},
    style::{bold, state_color, text, Palette},
    tool_render,
};
use bob_kernel::{Entry, EntryKind, OperationId, OperationState};
use ratatui::text::{Line, Span};
use std::collections::HashSet;

pub(super) struct Group<'a> {
    pub entries: Vec<&'a Entry>,
}

pub(super) fn groups(entries: &[Entry]) -> Vec<Group<'_>> {
    let mut groups = Vec::new();
    let mut run = Vec::new();
    let mut seen = HashSet::new();
    let finish = |run: &mut Vec<_>, groups: &mut Vec<_>| {
        if run.len() >= 3 {
            groups.push(Group {
                entries: std::mem::take(run),
            });
        } else {
            run.clear();
        }
    };
    for entry in entries {
        if super::visibility::hidden_tool(entry) {
            continue;
        }
        if let Some(tool) = &entry.tool {
            if !seen.insert(tool.id.clone()) {
                continue;
            }
            match tool.name.as_str() {
                "read_file" | "grep" | "glob" | "list_dir" => run.push(entry),
                _ => finish(&mut run, &mut groups),
            }
        } else if !(entry.kind == EntryKind::Assistant && entry.text.trim().is_empty()) {
            finish(&mut run, &mut groups);
        }
    }
    finish(&mut run, &mut groups);
    groups
}

impl Group<'_> {
    pub fn first(&self) -> &OperationId {
        &self.entries[0].tool.as_ref().unwrap().id
    }
    pub fn render(&self, pane: &Pane, rows: &mut Vec<Row>, width: u16) {
        let mut files = HashSet::new();
        let mut directories = HashSet::new();
        let mut searches = 0;
        let mut errors = 0;
        let mut running = None;
        for (index, entry) in self.entries.iter().enumerate() {
            let tool = entry.tool.as_ref().unwrap();
            match tool.name.as_str() {
                "read_file" => {
                    if let Some(path) = tool.input_preview["path"].as_str() {
                        files.insert(path);
                    }
                }
                "list_dir" => {
                    directories.insert(tool.input_preview["path"].as_str().unwrap_or("."));
                }
                _ => searches += 1,
            }
            if matches!(
                tool.state,
                OperationState::Failed
                    | OperationState::Unknown
                    | OperationState::Interrupted
                    | OperationState::Cancelled
            ) {
                errors += 1;
            } else if tool.state != OperationState::Done && running.is_none() {
                running = Some(index);
            }
        }
        let mut counts = Vec::new();
        for (count, one, many) in [
            (files.len(), "file", "files"),
            (searches, "search", "searches"),
            (directories.len(), "directory", "directories"),
        ] {
            if count > 0 {
                counts.push(format!("{count} {}", if count == 1 { one } else { many }));
            }
        }
        let mut summary = format!(
            "{} {}",
            if running.is_some() {
                "Exploring"
            } else {
                "Explored"
            },
            counts.join(" · ")
        );
        if errors > 0 {
            summary.push_str(&format!(" · {errors} failed"));
        }
        let color = if errors > 0 {
            Palette::ERROR()
        } else if running.is_some() {
            Palette::RUNNING()
        } else {
            Palette::OK()
        };
        let first = self.entries[0];
        let hit = HitAction::Tool(self.first().clone());
        render::append(
            rows,
            first.sequence,
            vec![Line::from(vec![
                Span::styled("• ", text().fg(color)),
                Span::styled(summary, bold()),
            ])],
            false,
            hit.clone(),
            width,
        );
        let expanded = pane.expanded.contains(self.first());
        for (index, entry) in self.entries.iter().enumerate() {
            let tool = entry.tool.as_ref().unwrap();
            let failed = matches!(
                tool.state,
                OperationState::Failed
                    | OperationState::Unknown
                    | OperationState::Interrupted
                    | OperationState::Cancelled
            );
            let result = pane.view.as_ref().and_then(|view| {
                view.entries.iter().find(|candidate| {
                    candidate.operation.as_ref() == Some(&tool.id)
                        && matches!(candidate.kind, EntryKind::ToolResult | EntryKind::ToolError)
                })
            });
            if !expanded && !failed && running != Some(index) {
                continue;
            }
            if failed {
                let start = rows.len();
                tool_render::tool(rows, entry, result, false, width.saturating_sub(2));
                while rows.len() > start && rows.last().is_some_and(|row| row.line.width() == 0) {
                    rows.pop();
                }
                for row in &mut rows[start..] {
                    row.line.spans.insert(0, Span::raw("  "));
                    if !matches!(row.hit, HitAction::Output(_)) {
                        row.hit = hit.clone();
                    }
                }
            } else {
                let (title, mut detail) = tool_render::display(&tool.name, &tool.input_preview);
                if let Some(summary) = tool_render::read_summary(tool) {
                    detail.push_str(&format!(" · {summary}"));
                }
                let line = Line::from(vec![
                    Span::raw("  "),
                    Span::styled("• ", text().fg(state_color(&tool.state))),
                    Span::styled(title, bold()),
                    Span::styled(format!(" {detail}"), text().fg(Palette::DIM())),
                ]);
                render::append(rows, entry.sequence, vec![line], false, hit.clone(), width);
            }
        }
        render::append(
            rows,
            first.sequence,
            vec![Line::default()],
            false,
            hit,
            width,
        );
    }
}
