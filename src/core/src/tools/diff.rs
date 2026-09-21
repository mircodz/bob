//! Minimal line-level diff (LCS-based) producing a unified-diff-style hunk list.

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

pub fn diff_lines(before: &str, after: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = before.split('\n').collect();
    let b: Vec<&str> = after.split('\n').collect();
    let n = a.len();
    let m = b.len();

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
            for k in keep.iter_mut().take(hi + 1).skip(lo) {
                *k = true;
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

/// Prefix each line with its sign and line number (old for removals, new otherwise).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[DiffLine]) -> Vec<(DiffOp, &str, Option<usize>, Option<usize>)> {
        lines
            .iter()
            .map(|line| (line.op, line.text.as_str(), line.old_line, line.new_line))
            .collect()
    }

    #[test]
    fn diff_replacement_keeps_line_numbers_and_removes_before_adding_on_ties() {
        use DiffOp::*;
        let lines = diff_lines("same\nold\nend", "same\nnew\nend");
        assert_eq!(
            rows(&lines),
            vec![
                (Context, "same", Some(1), Some(1)),
                (Remove, "old", Some(2), None),
                (Add, "new", None, Some(2)),
                (Context, "end", Some(3), Some(3)),
            ]
        );
        assert_eq!(diff_stat(&lines), (1, 1));
        assert_eq!(
            format_unified(&lines),
            "     1| same\n-    2| old\n+    2| new\n     3| end"
        );
        assert_eq!(
            rows(&diff_lines("a\nb\na", "b\na\nb")),
            vec![
                (Remove, "a", Some(1), None),
                (Context, "b", Some(2), Some(1)),
                (Context, "a", Some(3), Some(2)),
                (Add, "b", None, Some(3)),
            ]
        );
    }

    #[test]
    fn diff_empty_and_trailing_lines_are_preserved() {
        use DiffOp::*;
        for (before, after, expected) in [
            ("", "", vec![(Context, "", Some(1), Some(1))]),
            (
                "",
                "x",
                vec![(Remove, "", Some(1), None), (Add, "x", None, Some(1))],
            ),
            (
                "x",
                "",
                vec![(Remove, "x", Some(1), None), (Add, "", None, Some(1))],
            ),
            (
                "x",
                "x\n",
                vec![(Context, "x", Some(1), Some(1)), (Add, "", None, Some(2))],
            ),
            (
                "x\n",
                "x",
                vec![
                    (Context, "x", Some(1), Some(1)),
                    (Remove, "", Some(2), None),
                ],
            ),
        ] {
            assert_eq!(
                rows(&diff_lines(before, after)),
                expected,
                "{before:?} -> {after:?}"
            );
        }
    }

    #[test]
    fn compact_diff_keeps_context_once_and_marks_only_internal_gaps() {
        use DiffOp::*;
        let lines = diff_lines("a\nb\nc\nd\ne\nf\ng", "a\nB\nc\nd\ne\nF\ng");
        let compact = compact_diff(&lines, 0);
        assert_eq!(
            rows(&compact),
            vec![
                (Remove, "b", Some(2), None),
                (Add, "B", None, Some(2)),
                (Context, "...", None, None),
                (Remove, "f", Some(6), None),
                (Add, "F", None, Some(6)),
            ]
        );
        assert_eq!(rows(&compact_diff(&lines, 2)), rows(&lines));
        assert!(compact_diff(&diff_lines("unchanged", "unchanged"), 3).is_empty());
        assert!(compact_diff(&[], 3).is_empty());
    }

    #[test]
    fn diff_reconstructs_both_inputs_with_contiguous_line_numbers() {
        let samples = [
            "",
            "a",
            "a\n",
            "\na",
            "a\na\nb",
            "b\na\nb",
            "é\n終\n",
            "a\n\nb",
        ];
        for before in samples {
            for after in samples {
                let lines = diff_lines(before, after);
                let old: Vec<_> = lines.iter().filter(|line| line.op != DiffOp::Add).collect();
                let new: Vec<_> = lines
                    .iter()
                    .filter(|line| line.op != DiffOp::Remove)
                    .collect();
                assert_eq!(
                    old.iter()
                        .map(|line| line.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    before
                );
                assert_eq!(
                    new.iter()
                        .map(|line| line.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                    after
                );
                for (index, line) in old.iter().enumerate() {
                    assert_eq!(line.old_line, Some(index + 1));
                }
                for (index, line) in new.iter().enumerate() {
                    assert_eq!(line.new_line, Some(index + 1));
                }
                for line in &lines {
                    if line.op == DiffOp::Add {
                        assert_eq!(line.old_line, None);
                    } else if line.op == DiffOp::Remove {
                        assert_eq!(line.new_line, None);
                    }
                }
                let (added, removed) = diff_stat(&lines);
                assert_eq!(old.len() + added, new.len() + removed);
                for context in [0, 1, 2, 20] {
                    let compact = compact_diff(&lines, context);
                    assert_eq!(diff_stat(&compact), (added, removed));
                    let changes = |rows: &[DiffLine]| {
                        rows.iter()
                            .filter(|line| line.op != DiffOp::Context)
                            .map(|line| (line.op, line.text.clone(), line.old_line, line.new_line))
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(changes(&compact), changes(&lines));
                }
            }
        }
    }
}
