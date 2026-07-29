//! Project file listing + fuzzy matching for `@file` autocomplete.

use std::path::Path;
use std::process::Command;

/// Gather candidate file paths (relative to `cwd`) for `@file` completion.
/// Prefers `git ls-files` (honours .gitignore); falls back to a bounded walk
/// that skips the usual noise directories.
pub fn gather_files(cwd: &Path) -> Vec<String> {
    if let Some(files) = git_ls_files(cwd) {
        return files;
    }
    let mut out = Vec::new();
    walk(cwd, cwd, &mut out, 0);
    out.sort();
    out
}

fn git_ls_files(cwd: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    if files.is_empty() {
        None
    } else {
        Some(files)
    }
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
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name != "." {
            // Skip dotfiles/dotdirs except keep it simple (git path handles the rest).
            if path.is_dir() {
                continue;
            }
        }
        if path.is_dir() {
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

/// A fuzzy match result: the path and its score (higher is better).
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
    // Higher score first; break ties by shorter path, then lexical.
    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.len().cmp(&b.path.len()))
            .then_with(|| a.path.cmp(b.path))
    });
    matches.into_iter().take(limit).map(|m| m.path).collect()
}

/// Subsequence score: every query char must appear in order. Rewards
/// consecutive matches, matches right after a path separator, and matches in
/// the basename. Returns None if not a subsequence.
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
                s += 5; // consecutive
            }
            if prev_sep {
                s += 3; // start of a path segment
            }
            if hi >= basename_start {
                s += 2; // in the file name, not a parent dir
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
            "src/remote/src/host.rs",
            "README.md",
            "Cargo.toml",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn empty_query_returns_prefix() {
        let f = files();
        let got = fuzzy_rank(&f, "", 3);
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn subsequence_matches_across_path() {
        let f = files();
        // "sesn" is a subsequence of ".../session.rs" (se-ss-io-n) and matches
        // no other candidate's basename as strongly.
        let got = fuzzy_rank(&f, "session", 5);
        assert_eq!(got.first(), Some(&"src/core/src/core/session.rs"));
    }

    #[test]
    fn basename_beats_dir_match() {
        let f = files();
        // "files" should rank files.rs top (basename match), not other paths.
        let got = fuzzy_rank(&f, "files", 5);
        assert_eq!(got.first(), Some(&"src/tui/src/tui/files.rs"));
    }

    #[test]
    fn non_subsequence_is_filtered_out() {
        let f = files();
        let got = fuzzy_rank(&f, "zzzz", 5);
        assert!(got.is_empty());
    }
}
