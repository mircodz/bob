//! Minimal line-level diff (LCS-based) producing a unified-diff-style hunk list.
//! Shared by the edit tools (to report what changed) and the TUI (to render a
//! pretty colored diff).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffOp {
    Context,
    Add,
    Remove,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
}

/// Compute a line diff between two strings.
pub fn diff_lines(before: &str, after: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = before.split('\n').collect();
    let b: Vec<&str> = after.split('\n').collect();
    let n = a.len();
    let m = b.len();

    // LCS table.
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    let (mut oldn, mut newn) = (1usize, 1usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine {
                op: DiffOp::Context,
                text: a[i].to_string(),
                old_line: Some(oldn),
                new_line: Some(newn),
            });
            oldn += 1;
            newn += 1;
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(DiffLine {
                op: DiffOp::Remove,
                text: a[i].to_string(),
                old_line: Some(oldn),
                new_line: None,
            });
            oldn += 1;
            i += 1;
        } else {
            out.push(DiffLine {
                op: DiffOp::Add,
                text: b[j].to_string(),
                old_line: None,
                new_line: Some(newn),
            });
            newn += 1;
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine {
            op: DiffOp::Remove,
            text: a[i].to_string(),
            old_line: Some(oldn),
            new_line: None,
        });
        oldn += 1;
        i += 1;
    }
    while j < m {
        out.push(DiffLine {
            op: DiffOp::Add,
            text: b[j].to_string(),
            old_line: None,
            new_line: Some(newn),
        });
        newn += 1;
        j += 1;
    }
    out
}

/// Trim a full-file diff to only changed regions plus `context` lines around.
pub fn compact_diff(lines: &[DiffLine], context: usize) -> Vec<DiffLine> {
    let mut keep = vec![false; lines.len()];
    for (idx, l) in lines.iter().enumerate() {
        if l.op != DiffOp::Context {
            let lo = idx.saturating_sub(context);
            let hi = (idx + context).min(lines.len().saturating_sub(1));
            for k in lo..=hi {
                keep[k] = true;
            }
        }
    }
    let mut out: Vec<DiffLine> = Vec::new();
    let mut gap = false;
    for (idx, l) in lines.iter().enumerate() {
        if keep[idx] {
            out.push(l.clone());
            gap = false;
        } else if !gap {
            out.push(DiffLine {
                op: DiffOp::Context,
                text: "...".to_string(),
                old_line: None,
                new_line: None,
            });
            gap = true;
        }
    }
    if out.first().map(|l| l.text == "...").unwrap_or(false) {
        out.remove(0);
    }
    if out.last().map(|l| l.text == "...").unwrap_or(false) {
        out.pop();
    }
    out
}

pub fn diff_stat(lines: &[DiffLine]) -> (usize, usize) {
    let mut added = 0;
    let mut removed = 0;
    for l in lines {
        match l.op {
            DiffOp::Add => added += 1,
            DiffOp::Remove => removed += 1,
            _ => {}
        }
    }
    (added, removed)
}

/// Render a diff as a plain unified-style string (for tool output / the model).
/// Each line is prefixed with its new-file line number (blank for removals) so
/// UIs can show line numbers without recomputing the diff:
///   "  12| context", "+ 13| added", "-   | removed".
pub fn format_unified(lines: &[DiffLine]) -> String {
    lines
        .iter()
        .map(|l| {
            let sign = match l.op {
                DiffOp::Add => "+",
                DiffOp::Remove => "-",
                DiffOp::Context => " ",
            };
            let num = match l.op {
                DiffOp::Remove => l.old_line,
                _ => l.new_line,
            };
            let num_str = match num {
                Some(n) => format!("{:>4}", n),
                None => "    ".to_string(),
            };
            format!("{} {}| {}", sign, num_str, l.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
