use super::{commands::COMMANDS, state::Draft};
use bob_core::core::commands::CustomCommand;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Slash,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Item {
    pub label: String,
    pub description: String,
}

#[derive(Default)]
pub(super) struct Completion {
    pub items: Vec<Item>,
    pub selected: usize,
    pub kind: Option<Kind>,
    at: Option<usize>,
}

impl Completion {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn refresh(&mut self, draft: &Draft, custom: &[CustomCommand], files: &[String]) {
        if let Some((at, query)) = at_token(draft) {
            self.kind = Some(Kind::File);
            self.at = Some(at);
            self.items = fuzzy_rank(files, query, 8)
                .into_iter()
                .map(|label| Item {
                    label: label.to_owned(),
                    description: String::new(),
                })
                .collect();
        } else if draft.text.starts_with('/') && !draft.text.contains(char::is_whitespace) {
            self.kind = Some(Kind::Slash);
            self.at = None;
            self.items = COMMANDS
                .iter()
                .map(|(label, description)| Item {
                    label: (*label).into(),
                    description: (*description).into(),
                })
                .chain(custom.iter().map(|command| Item {
                    label: format!("/{}", command.name),
                    description: command.description.clone(),
                }))
                .filter(|item| item.label.starts_with(&draft.text))
                .collect();
        } else {
            self.clear();
        }
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
    }

    pub fn accept(&mut self, draft: &mut Draft) -> Option<Kind> {
        let item = self.items.get(self.selected)?;
        let kind = self.kind?;
        let accepted = match kind {
            Kind::Slash => draft.replace(0..draft.text.len(), &item.label),
            Kind::File => draft.replace(self.at?..draft.cursor, &format!("@{}", item.label)),
        };
        if !accepted {
            return None;
        }
        self.clear();
        Some(kind)
    }
}

pub(super) fn at_token(draft: &Draft) -> Option<(usize, &str)> {
    for (offset, ch) in draft.text[..draft.cursor].char_indices().rev() {
        if ch == '@' {
            return Some((offset, &draft.text[offset + 1..draft.cursor]));
        }
        if ch.is_whitespace() {
            break;
        }
    }
    None
}

pub(super) async fn branch(cwd: &Path) -> Option<String> {
    use std::{process::Stdio, time::Duration};
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(["branch", "--show-current"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = bob_core::core::process::ManagedProcess::spawn(&mut command).ok()?;
    match tokio::time::timeout(Duration::from_secs(2), child.collect_output(4096)).await {
        Ok(Ok(output)) if output.status.success() && !output.stdout.truncated() => {
            let branch = String::from_utf8_lossy(&output.stdout.bytes)
                .trim()
                .to_owned();
            (!branch.is_empty()).then_some(branch)
        }
        _ => {
            let _ = child.terminate_and_wait().await;
            None
        }
    }
}

pub(super) async fn gather_files(cwd: &Path) -> Vec<String> {
    use std::{process::Stdio, time::Duration};
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Ok(mut child) = bob_core::core::process::ManagedProcess::spawn(&mut command) {
        match tokio::time::timeout(
            Duration::from_secs(2),
            child.collect_output(2 * 1024 * 1024),
        )
        .await
        {
            Ok(Ok(output)) if output.status.success() && !output.stdout.truncated() => {
                return output
                    .stdout
                    .bytes
                    .split(|byte| *byte == 0)
                    .filter(|path| !path.is_empty())
                    .take(20_000)
                    .map(|path| String::from_utf8_lossy(path).into_owned())
                    .collect();
            }
            _ => {
                let _ = child.terminate_and_wait().await;
            }
        }
    }
    let cwd = cwd.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        walk(&cwd, &cwd, &mut files, 0);
        files.sort();
        files
    })
    .await
    .unwrap_or_default()
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<String>, depth: usize) {
    if depth > 12 || files.len() >= 20_000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if kind.is_dir() {
            if name.starts_with('.')
                || ["target", "node_modules", "dist", "build"].contains(&name.as_ref())
            {
                continue;
            }
            walk(root, &entry.path(), files, depth + 1);
        } else if let Ok(path) = entry.path().strip_prefix(root) {
            files.push(path.to_string_lossy().into_owned());
        }
        if files.len() >= 20_000 {
            break;
        }
    }
}

fn fuzzy_rank<'a>(files: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    if query.is_empty() {
        return files.iter().take(limit).map(String::as_str).collect();
    }
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let mut matches: Vec<_> = files
        .iter()
        .filter_map(|path| score(path, &query).map(|score| (path.as_str(), score)))
        .collect();
    matches.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.0.len().cmp(&b.0.len()))
            .then_with(|| a.0.cmp(b.0))
    });
    matches
        .into_iter()
        .take(limit)
        .map(|(path, _)| path)
        .collect()
}

fn score(path: &str, query: &[char]) -> Option<i64> {
    let hay: Vec<char> = path.to_lowercase().chars().collect();
    let basename = hay.iter().rposition(|ch| *ch == '/').map_or(0, |i| i + 1);
    let (mut qi, mut total, mut consecutive, mut boundary) = (0, 0, false, true);
    for (hi, ch) in hay.into_iter().enumerate() {
        if qi < query.len() && ch == query[qi] {
            total += 1
                + if consecutive { 5 } else { 0 }
                + if boundary { 3 } else { 0 }
                + if hi >= basename { 2 } else { 0 };
            qi += 1;
            consecutive = true;
        } else {
            consecutive = false;
        }
        boundary = matches!(ch, '/' | '_' | '-' | '.');
    }
    (qi == query.len()).then_some(total - path.len() as i64 / 20)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_registry_and_prefix_descriptions_are_preserved() {
        let mut completion = Completion::default();
        let mut draft = Draft::default();
        draft.insert("/");
        completion.refresh(&draft, &[], &[]);
        assert_eq!(completion.items.len(), 17);
        assert_eq!(completion.items[0].label, "/new");
        assert_eq!(completion.items[16].label, "/exit");
        draft.insert("co");
        completion.refresh(&draft, &[], &[]);
        assert_eq!(
            completion
                .items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            ["/copy", "/context", "/compact"]
        );
        assert_eq!(completion.items[1].description, "show context-window usage");
        completion.selected = 2;
        assert_eq!(completion.accept(&mut draft), Some(Kind::Slash));
        assert_eq!(draft.text, "/compact");
    }

    #[test]
    fn custom_command_menu_contains_description_and_clamps_selection() {
        let custom = [CustomCommand {
            name: "review".into(),
            description: "Review staged changes".into(),
            template: "$ARGUMENTS".into(),
        }];
        let mut completion = Completion::default();
        let mut draft = Draft::default();
        draft.insert("/");
        completion.refresh(&draft, &custom, &[]);
        completion.selected = 17;
        draft.insert("rev");
        completion.refresh(&draft, &custom, &[]);
        assert_eq!(completion.selected, 0);
        assert_eq!(completion.items[0].description, "Review staged changes");
        completion.accept(&mut draft);
        assert_eq!(draft.text, "/review");
        draft.insert(" args");
        completion.refresh(&draft, &custom, &[]);
        assert!(completion.items.is_empty());
    }

    #[test]
    fn unicode_file_completion_replaces_only_token_before_cursor() {
        let files = ["src/界.rs".to_owned(), "other.rs".to_owned()];
        let mut completion = Completion::default();
        let mut draft = Draft::default();
        draft.insert("λ @界 suffix");
        draft.cursor = "λ @界".len();
        completion.refresh(&draft, &[], &files);
        assert_eq!(completion.kind, Some(Kind::File));
        assert_eq!(completion.accept(&mut draft), Some(Kind::File));
        assert_eq!(draft.text, "λ @src/界.rs suffix");
        assert_eq!(&draft.text[draft.cursor..], " suffix");
        assert!(completion.items.is_empty());
    }

    #[test]
    fn file_fuzzy_ranking_prefers_basename_and_accepts_gaps() {
        let files = ["files/mod.rs", "src/files.rs", "src/session.rs"].map(String::from);
        assert_eq!(
            fuzzy_rank(&files, "files", 2),
            ["src/files.rs", "files/mod.rs"]
        );
        assert_eq!(fuzzy_rank(&files, "SESN", 8), ["src/session.rs"]);
        assert_eq!(fuzzy_rank(&files, "", 1), ["files/mod.rs"]);
    }
}
