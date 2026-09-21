use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Relative completion paths from Git, falling back to a bounded walk.
pub async fn gather_files(cwd: &Path) -> Vec<String> {
    if let Some(files) = git_ls_files(cwd).await {
        return files;
    }
    let cwd = cwd.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        walk(&cwd, &cwd, &mut out, 0);
        out.sort();
        out
    })
    .await
    .unwrap_or_default()
}

async fn git_ls_files(cwd: &Path) -> Option<Vec<String>> {
    let output = git_output(
        cwd,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        2 * 1024 * 1024,
    )
    .await?;
    let files: Vec<String> = output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .take(MAX_FILES)
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();
    Some(files)
}

pub(super) async fn git_output(cwd: &Path, args: &[&str], limit: usize) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = bob_core::core::process::ManagedProcess::spawn(&mut command).ok()?;
    let output =
        match tokio::time::timeout(Duration::from_secs(2), child.collect_output(limit)).await {
            Ok(Ok(output)) => output,
            _ => {
                let _ = child.terminate_and_wait().await;
                return None;
            }
        };
    (output.status.success() && !output.stdout.truncated()).then_some(output.stdout.bytes)
}

const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", "dist", "build"];
const MAX_FILES: usize = 20_000;

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 12 || out.len() >= MAX_FILES {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name != "." && file_type.is_dir() {
            continue;
        }
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk(root, &path, out, depth + 1);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_string_lossy().to_string());
        }
        if out.len() >= MAX_FILES {
            return;
        }
    }
}

pub struct Match<'a> {
    pub path: &'a str,
    pub score: i64,
}

/// Rank `files` against `query` with a simple fzf-style subsequence scorer.
/// An empty query returns the first `limit` files unranked.
pub fn fuzzy_rank<'a>(files: &'a [String], query: &str, limit: usize) -> Vec<&'a str> {
    if query.is_empty() {
        return files.iter().take(limit).map(|s| s.as_str()).collect();
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let mut matches: Vec<Match> = files
        .iter()
        .filter_map(|f| score(f, &q).map(|score| Match { path: f, score }))
        .collect();
    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.len().cmp(&b.path.len()))
            .then_with(|| a.path.cmp(b.path))
    });
    matches.into_iter().take(limit).map(|m| m.path).collect()
}

/// Reward consecutive, segment-boundary, and basename matches.
fn score(path: &str, query: &[char]) -> Option<i64> {
    let hay: Vec<char> = path.to_lowercase().chars().collect();
    let basename_start = path.rfind('/').map(|i| i + 1).unwrap_or(0);

    let mut qi = 0;
    let mut total: i64 = 0;
    let mut prev_matched = false;
    let mut prev_sep = true; // start-of-string counts as a boundary
    for (hi, &hc) in hay.iter().enumerate() {
        if qi < query.len() && hc == query[qi] {
            let mut s = 1;
            if prev_matched {
                s += 5;
            }
            if prev_sep {
                s += 3;
            }
            if hi >= basename_start {
                s += 2;
            }
            total += s;
            qi += 1;
            prev_matched = true;
        } else {
            prev_matched = false;
        }
        prev_sep = hc == '/' || hc == '_' || hc == '-' || hc == '.';
    }
    if qi == query.len() {
        // Prefer shorter paths overall.
        Some(total - (path.len() as i64) / 20)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        [
            "src/core/src/core/session.rs",
            "src/tui/src/tui/mod.rs",
            "src/tui/src/tui/files.rs",
            "src/core/src/agent/agent.rs",
            "README.md",
            "Cargo.toml",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn query_matching_preserves_prefix_limits_and_gapped_subsequences() {
        let candidates = files();
        for (label, query, limit, expected) in [
            (
                "empty query preserves input order",
                "",
                3,
                vec![
                    "src/core/src/core/session.rs",
                    "src/tui/src/tui/mod.rs",
                    "src/tui/src/tui/files.rs",
                ],
            ),
            (
                "full-path subsequence",
                "sesn",
                5,
                vec![
                    "src/core/src/core/session.rs",
                    "src/core/src/agent/agent.rs",
                ],
            ),
            (
                "case insensitive",
                "SESN",
                5,
                vec![
                    "src/core/src/core/session.rs",
                    "src/core/src/agent/agent.rs",
                ],
            ),
            (
                "ranked prefix limit",
                "sesn",
                1,
                vec!["src/core/src/core/session.rs"],
            ),
            ("non-subsequence rejected", "zzzz", 5, vec![]),
            ("empty query zero limit", "", 0, vec![]),
            ("matching query zero limit", "sesn", 0, vec![]),
        ] {
            assert_eq!(fuzzy_rank(&candidates, query, limit), expected, "{label}");
        }
        assert!(!candidates[0].contains("sesn"));
    }

    #[test]
    fn gapped_basename_matching_requires_every_character_in_order() {
        let candidates = ["session.rs", "agent.rs", "sense.rs"].map(String::from);
        for query in ["sesn", "SESN"] {
            assert_eq!(fuzzy_rank(&candidates, query, 10), vec!["session.rs"]);
        }
        assert!(fuzzy_rank(&candidates, "sessionn", 10).is_empty());
        assert!(fuzzy_rank(&candidates, "noses", 10).is_empty());
    }

    #[test]
    fn tied_scores_sort_by_length_then_lexically_not_input_order() {
        let candidates = ["za.rs", "ba.rs", "a.rs", "ca.rs"].map(String::from);
        assert_eq!(
            fuzzy_rank(&candidates, "rs", 10),
            vec!["a.rs", "ba.rs", "ca.rs", "za.rs"]
        );
        assert_eq!(fuzzy_rank(&candidates, "rs", 2), vec!["a.rs", "ba.rs"]);
    }

    #[test]
    fn basename_match_outranks_a_competing_directory_match() {
        let candidates = ["files/mod.rs", "src/files.rs"].map(String::from);
        for candidate in &candidates {
            assert_eq!(
                fuzzy_rank(std::slice::from_ref(candidate), "files", 1),
                vec![candidate.as_str()]
            );
        }
        assert_eq!(
            fuzzy_rank(&candidates, "files", 2),
            vec!["src/files.rs", "files/mod.rs"]
        );
        assert_eq!(fuzzy_rank(&candidates, "files", 1), vec!["src/files.rs"]);
    }
}
