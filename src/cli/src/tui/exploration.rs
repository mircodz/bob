use super::theme::Palette;
use super::view::{Cell, ToolStatus};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

pub(super) fn is_exploration(name: &str) -> bool {
    matches!(name, "read_file" | "grep" | "glob" | "list_dir")
}

pub(super) struct Group {
    pub first: usize,
    pub last_visible: usize,
    id: String,
    summary: String,
    status: ToolStatus,
    expanded: bool,
    shown: Vec<usize>,
}

impl Group {
    pub fn shows_tool(&self, index: usize) -> bool {
        self.shown.binary_search(&index).is_ok()
    }

    pub fn header(&self) -> Line<'static> {
        let color = match self.status {
            ToolStatus::Running => Palette::RUNNING(),
            ToolStatus::Ok => Palette::OK(),
            ToolStatus::Error => Palette::ERROR(),
        };
        Line::from(vec![
            Span::styled("• ", Style::default().fg(color)),
            Span::styled(
                self.summary.clone(),
                Style::default()
                    .fg(Palette::TEXT())
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    }

    pub fn fingerprint(&self, cell: &Cell, index: usize) -> u64 {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        self.first.hash(&mut hash);
        (index == self.last_visible).hash(&mut hash);
        self.shows_tool(index).hash(&mut hash);
        if index == self.first {
            self.summary.hash(&mut hash);
            (self.status as u8).hash(&mut hash);
            self.expanded.hash(&mut hash);
        }
        if self.shows_tool(index) {
            if let Cell::Tool {
                name,
                input,
                status,
                ..
            } = cell
            {
                name.hash(&mut hash);
                input.to_string().hash(&mut hash);
                (*status as u8).hash(&mut hash);
                if *status == ToolStatus::Error {
                    cell.fingerprint().hash(&mut hash);
                }
            }
        }
        hash.finish()
    }
}

#[derive(Default)]
pub(super) struct Exploration {
    revision: Option<u64>,
    len: usize,
    groups: HashMap<usize, Group>,
    members: HashMap<usize, usize>,
    expanded: HashSet<String>,
    pub visible: Vec<usize>,
}

impl Exploration {
    pub fn group(&self, index: usize) -> Option<&Group> {
        self.members
            .get(&index)
            .and_then(|first| self.groups.get(first))
    }

    pub fn toggle(&mut self, first: usize) {
        let group = &self.groups[&first];
        if !self.expanded.remove(&group.id) {
            self.expanded.insert(group.id.clone());
        }
        self.revision = None;
    }

    pub fn refresh(&mut self, cells: &[Cell], revision: u64) {
        if self.revision == Some(revision) && self.len == cells.len() {
            return;
        }
        self.groups.clear();
        self.members.clear();
        let mut run = Vec::new();
        for (index, cell) in cells.iter().enumerate() {
            match cell {
                Cell::Tool { name, .. } if is_exploration(name) => run.push(index),
                Cell::Tool { name, .. }
                    if matches!(name.as_str(), "job_status" | "job_output" | "todo_write") => {}
                Cell::Assistant { text, .. } if text.is_empty() => {}
                _ => self.finish_run(cells, &mut run),
            }
        }
        self.finish_run(cells, &mut run);
        self.expanded
            .retain(|id| self.groups.values().any(|group| &group.id == id));
        self.visible = cells
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| {
                let visible = self.group(index).map_or_else(
                    || cell.is_visible(),
                    |group| index == group.first || group.shows_tool(index),
                );
                visible.then_some(index)
            })
            .collect();
        self.revision = Some(revision);
        self.len = cells.len();
    }

    fn finish_run(&mut self, cells: &[Cell], run: &mut Vec<usize>) {
        if run.len() < 3 {
            run.clear();
            return;
        }
        let first = run[0];
        let Cell::Tool { id, .. } = &cells[first] else {
            unreachable!()
        };
        let expanded = self.expanded.contains(id);
        let mut files = HashSet::new();
        let mut directories = HashSet::new();
        let mut searches = 0;
        let mut running = None;
        let mut errors = 0;
        let mut shown = Vec::new();
        for &index in run.iter() {
            let Cell::Tool {
                name,
                input,
                status,
                ..
            } = &cells[index]
            else {
                unreachable!()
            };
            match name.as_str() {
                "read_file" => {
                    files.insert(input["path"].as_str().unwrap_or(""));
                }
                "list_dir" => {
                    directories.insert(input["path"].as_str().unwrap_or("."));
                }
                _ => searches += 1,
            }
            if *status == ToolStatus::Running && running.is_none() {
                running = Some(index);
            }
            if *status == ToolStatus::Error {
                errors += 1;
            }
            if expanded || *status == ToolStatus::Error {
                shown.push(index);
            }
            self.members.insert(index, first);
        }
        if let Some(index) = running.filter(|index| !shown.contains(index)) {
            shown.push(index);
            shown.sort_unstable();
        }
        let mut counts = Vec::new();
        for (count, singular, plural) in [
            (files.len(), "file", "files"),
            (searches, "search", "searches"),
            (directories.len(), "directory", "directories"),
        ] {
            if count > 0 {
                counts.push(format!(
                    "{count} {}",
                    if count == 1 { singular } else { plural }
                ));
            }
        }
        let verb = if running.is_some() {
            "Exploring"
        } else {
            "Explored"
        };
        let mut summary = format!("{verb} {}", counts.join(" · "));
        if errors > 0 {
            summary.push_str(&format!(" · {errors} failed"));
        }
        self.groups.insert(
            first,
            Group {
                first,
                last_visible: shown.last().copied().unwrap_or(first),
                id: id.clone(),
                summary,
                status: if errors > 0 {
                    ToolStatus::Error
                } else if running.is_some() {
                    ToolStatus::Running
                } else {
                    ToolStatus::Ok
                },
                expanded,
                shown,
            },
        );
        run.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(id: &str, name: &str) -> Cell {
        Cell::Tool {
            id: id.into(),
            name: name.into(),
            input: json!({"path": "src", "pattern": "needle"}),
            status: ToolStatus::Ok,
            output: Some("result".into()),
            expanded: false,
        }
    }

    #[test]
    fn mixed_runs_count_paths_once_and_stop_at_conversation_boundaries() {
        for boundary in [
            Cell::User("next question".into()),
            Cell::Assistant {
                text: "Found the cause".into(),
                open: false,
            },
            tool("edit", "edit_file"),
            tool("shell", "bash"),
            tool("ask", "ask_user"),
            tool("agent", "explore"),
        ] {
            let cells = vec![
                tool("r1", "read_file"),
                tool("r2", "read_file"),
                boundary,
                tool("g", "grep"),
                tool("poll", "job_output"),
                Cell::Assistant {
                    text: String::new(),
                    open: false,
                },
                tool("l", "list_dir"),
                tool("r3", "read_file"),
                tool("r4", "read_file"),
                tool("glob", "glob"),
            ];
            let mut groups = Exploration::default();
            groups.refresh(&cells, 1);
            assert!(groups.group(0).is_none());
            assert!(groups.group(1).is_none());
            let group = groups.group(9).unwrap();
            assert_eq!(group.first, 3);
            assert_eq!(group.summary, "Explored 1 file · 2 searches · 1 directory");
            assert!(!groups.visible.contains(&4));
            assert!(!groups.visible.contains(&5));
            groups.toggle(3);
            groups.refresh(&cells, 1);
            let expected: Vec<_> = cells
                .iter()
                .enumerate()
                .filter_map(|(i, cell)| cell.is_visible().then_some(i))
                .collect();
            assert_eq!(groups.visible, expected);
        }
    }

    #[test]
    fn out_of_order_results_keep_failures_and_active_work_visible() {
        let mut cells: Vec<_> = (0..4).map(|i| tool(&i.to_string(), "read_file")).collect();
        for cell in &mut cells {
            let Cell::Tool { status, .. } = cell else {
                unreachable!()
            };
            *status = ToolStatus::Running;
        }
        let mut groups = Exploration::default();
        groups.refresh(&cells, 1);
        assert_eq!(groups.visible, [0]);
        assert!(groups.group(0).unwrap().summary.starts_with("Exploring"));
        for (revision, index, result, expected) in [
            (2, 2, ToolStatus::Error, vec![0, 2]),
            (3, 0, ToolStatus::Ok, vec![0, 1, 2]),
            (4, 3, ToolStatus::Ok, vec![0, 1, 2]),
            (5, 1, ToolStatus::Ok, vec![0, 2]),
        ] {
            let Cell::Tool { status, .. } = &mut cells[index] else {
                unreachable!()
            };
            *status = result;
            groups.refresh(&cells, revision);
            assert_eq!(groups.visible, expected);
            assert_eq!(groups.group(0).unwrap().status, ToolStatus::Error);
        }
        assert_eq!(
            groups.group(0).unwrap().summary,
            "Explored 1 file · 1 failed"
        );
        groups.toggle(0);
        groups.refresh(&cells, 5);
        cells.push(tool("new", "list_dir"));
        groups.refresh(&cells, 6);
        assert_eq!(groups.visible, [0, 1, 2, 3, 4]);
        assert!(groups.group(4).unwrap().expanded);

        let replacement: Vec<_> = (0..3)
            .map(|i| tool(&format!("replacement-{i}"), "grep"))
            .collect();
        groups.refresh(&replacement, 7);
        assert_eq!(groups.visible, [0]);
        assert!(!groups.group(0).unwrap().expanded);
    }
}
