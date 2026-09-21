//! Surgical string-replacement edit tools with staleness checks and a compact
//! unified diff appended to the result.

use crate::core::types::ToolSpec;
use crate::tools::diff::{compact_diff, diff_lines, diff_stat, format_unified};
use crate::tools::filesystem::{self, WriteRequest};
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The diff fence includes the file path for syntax highlighting.
fn edit_result(path: &str, before: &str, after: &str) -> String {
    let full = diff_lines(before, after);
    let (added, removed) = diff_stat(&full);
    let compact = compact_diff(&full, 3);
    filesystem::bounded_output(format!(
        "edited {} (+{} -{})\n```diff {}\n{}\n```",
        path,
        added,
        removed,
        path,
        format_unified(&compact)
    ))
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count()
}

fn replace_first(haystack: &str, needle: &str, replacement: &str) -> String {
    match haystack.find(needle) {
        Some(idx) => {
            let mut s = String::with_capacity(haystack.len());
            s.push_str(&haystack[..idx]);
            s.push_str(replacement);
            s.push_str(&haystack[idx + needle.len()..]);
            s
        }
        None => haystack.to_string(),
    }
}

fn apply_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
    label: &str,
) -> Result<String, String> {
    if old_string == new_string {
        return Err(format!("{label}old_string and new_string are identical"));
    }
    let occ = count_occurrences(content, old_string);
    if occ == 0 {
        return Err(format!("{label}old_string not found"));
    }
    if occ > 1 && !replace_all {
        return Err(format!(
            "{label}old_string matches {occ} times; add context to make it unique or set replace_all"
        ));
    }
    Ok(if replace_all {
        content.replace(old_string, new_string)
    } else {
        replace_first(content, old_string, new_string)
    })
}

fn compute_edit(
    cwd: &str,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Option<(String, String)> {
    let full = filesystem::workspace_path(cwd, Path::new(path)).ok()?;
    let content = filesystem::read_regular(&full).ok()?;
    let occ = count_occurrences(&content, old_string);
    if occ == 0 || (occ > 1 && !replace_all) {
        return None;
    }
    let updated = if replace_all {
        content.replace(old_string, new_string)
    } else {
        replace_first(&content, old_string, new_string)
    };
    Some((content, updated))
}

pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".to_string(),
            description: "Make a surgical edit by replacing an exact string in a file — the \
                preferred way to change existing code. `old_string` must appear EXACTLY once; \
                include enough surrounding context (whole lines, correct indentation) to make it \
                unique, or set replace_all to change every occurrence. Preserve the file's exact \
                whitespace — never include the display line numbers from read_file. You must read \
                the file first; the edit is rejected if the file changed on disk since your last \
                read. Prefer multi_edit when making several edits to the same file."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string", "description": "Exact text to replace." },
                    "new_string": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence (default false)." }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let path = filesystem::required_path(&input)?;
        let old_string = filesystem::required_string(&input, "old_string")?;
        let new_string = filesystem::required_string(&input, "new_string")?;
        let replace_all = filesystem::optional_bool(&input, "replace_all")?;
        let full = filesystem::workspace_path(&ctx.cwd, Path::new(path))?;
        let full_str = full.to_string_lossy();
        let content = filesystem::read_regular(&full)?;
        if let Some(stale) = ctx.files.check_snapshot(&full_str, &content, false) {
            return Err(ToolError::failed(stale));
        }
        let fully_known = ctx.files.fully_read(&full_str, &content);
        let updated = apply_edit(&content, old_string, new_string, replace_all, "")
            .map_err(ToolError::invalid_input)?;
        let batch = filesystem::prepare_batch(
            ctx,
            vec![WriteRequest {
                path: PathBuf::from(path),
                resolved: full.clone(),
                content: updated.clone(),
                expected: Some(content.clone()),
            }],
        )
        .await?;
        let written = batch.commit()?;
        ctx.files
            .record_content(&written[0].to_string_lossy(), &updated, fully_known);
        crate::tools::lsp::sync_after_edit(&ctx.lsp, &written[0], &updated).await;
        Ok(edit_result(path, &content, &updated).into())
    }

    fn preview(&self, input: &Value, ctx: &ToolContext) -> Option<String> {
        let path = input["path"].as_str()?;
        let (before, after) = compute_edit(
            &ctx.cwd,
            path,
            filesystem::required_string(input, "old_string").ok()?,
            filesystem::required_string(input, "new_string").ok()?,
            filesystem::optional_bool(input, "replace_all").ok()?,
        )?;
        Some(edit_result(path, &before, &after))
    }
}

pub struct MultiEditTool;

#[async_trait]
impl Tool for MultiEditTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "multi_edit".to_string(),
            description: "Apply several edits to ONE file in a single atomic operation. Edits run \
                in order, each against the result of the previous, and if any one fails to match \
                nothing is written — so the file is never left half-edited. Use this instead of \
                multiple edit_file calls on the same file. Same rules as edit_file: read first, \
                each old_string must match uniquely (or set replace_all), preserve exact \
                whitespace."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string" },
                                "new_string": { "type": "string" },
                                "replace_all": { "type": "boolean" }
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let path = filesystem::required_path(&input)?;
        let edits = input
            .get("edits")
            .and_then(Value::as_array)
            .filter(|edits| !edits.is_empty())
            .ok_or_else(|| ToolError::invalid_input("edits must be a nonempty array"))?;
        let validated: Vec<_> = edits
            .iter()
            .map(|edit| {
                Ok((
                    filesystem::required_string(edit, "old_string")?,
                    filesystem::required_string(edit, "new_string")?,
                    filesystem::optional_bool(edit, "replace_all")?,
                ))
            })
            .collect::<Result<_, ToolError>>()?;
        let full = filesystem::workspace_path(&ctx.cwd, Path::new(path))?;
        let full_str = full.to_string_lossy();
        let original = filesystem::read_regular(&full)?;
        if let Some(stale) = ctx.files.check_snapshot(&full_str, &original, false) {
            return Err(ToolError::failed(stale));
        }
        let fully_known = ctx.files.fully_read(&full_str, &original);
        let mut content = original.clone();
        for (i, (old_string, new_string, replace_all)) in validated.into_iter().enumerate() {
            content = apply_edit(
                &content,
                old_string,
                new_string,
                replace_all,
                &format!("edit {}: ", i + 1),
            )
            .map_err(ToolError::invalid_input)?;
        }
        let batch = filesystem::prepare_batch(
            ctx,
            vec![WriteRequest {
                path: PathBuf::from(path),
                resolved: full.clone(),
                content: content.clone(),
                expected: Some(original.clone()),
            }],
        )
        .await?;
        let written = batch.commit()?;
        ctx.files
            .record_content(&written[0].to_string_lossy(), &content, fully_known);
        crate::tools::lsp::sync_after_edit(&ctx.lsp, &written[0], &content).await;
        Ok(edit_result(path, &original, &content).into())
    }

    fn preview(&self, input: &Value, ctx: &ToolContext) -> Option<String> {
        let path = input["path"].as_str()?;
        let full = filesystem::workspace_path(&ctx.cwd, Path::new(path)).ok()?;
        let original = filesystem::read_regular(&full).ok()?;
        let mut content = original.clone();
        for e in input["edits"].as_array()? {
            let old_string = filesystem::required_string(e, "old_string").ok()?;
            let new_string = filesystem::required_string(e, "new_string").ok()?;
            let replace_all = filesystem::optional_bool(e, "replace_all").ok()?;
            let occ = count_occurrences(&content, old_string);
            if occ == 0 || (occ > 1 && !replace_all) {
                return None;
            }
            content = if replace_all {
                content.replace(old_string, new_string)
            } else {
                replace_first(&content, old_string, new_string)
            };
        }
        Some(edit_result(path, &original, &content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files::FileFixture as Fixture;
    use crate::tools::builtin::{ReadFileTool, WriteFileTool};
    use crate::tools::registry::ToolErrorKind;

    #[test]
    fn exact_replacement_preserves_unicode_whitespace_and_line_endings() {
        for (content, old, new, expected) in [
            ("é🙂終", "🙂", "λ", "éλ終"),
            (
                "\tlet x = 1;\r\nnext\r\n",
                "x = 1",
                "x = 2",
                "\tlet x = 2;\r\nnext\r\n",
            ),
            ("prefix middle suffix", "middle", "", "prefix  suffix"),
            ("a[i] + a.*", "a[i]", "$1\\n", "$1\\n + a.*"),
            ("whole file", "whole file", "", ""),
        ] {
            assert_eq!(apply_edit(content, old, new, false, "").unwrap(), expected);
        }
        for needle in ["é", "  value", "value\n"] {
            let error =
                apply_edit("e\u{301}\tvalue\r\n", needle, "new", false, "edit 2: ").unwrap_err();
            assert_eq!(error, "edit 2: old_string not found");
        }
    }

    #[test]
    fn replacement_rejects_missing_empty_identical_and_ambiguous_matches() {
        for (old, new, replace_all, expected) in [
            ("", "x", false, "old_string not found"),
            ("", "x", true, "old_string not found"),
            ("absent", "x", true, "old_string not found"),
            (
                "one",
                "one",
                false,
                "old_string and new_string are identical",
            ),
            (
                "one",
                "one",
                true,
                "old_string and new_string are identical",
            ),
            (
                "one",
                "x",
                false,
                "old_string matches 2 times; add context to make it unique or set replace_all",
            ),
        ] {
            assert_eq!(
                apply_edit("one two one", old, new, replace_all, "edit 3: ").unwrap_err(),
                format!("edit 3: {expected}")
            );
        }
        assert_eq!(
            apply_edit("one two one", "one", "🙂", true, "").unwrap(),
            "🙂 two 🙂"
        );
        assert_eq!(
            apply_edit("one two one", "two one", "end", false, "").unwrap(),
            "one end"
        );
    }

    #[tokio::test]
    async fn malformed_edits_leave_bytes_and_snapshot_unchanged() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "original");
        fixture.ctx.files.record_read(&path.to_string_lossy());
        for input in [
            json!({"path":"file","old_string":"original"}),
            json!({"path":"file","old_string":"original","new_string":false}),
            json!({"path":"file","old_string":"original","new_string":"new","replace_all":1}),
        ] {
            assert_eq!(
                EditFileTool
                    .execute(input, &fixture.ctx)
                    .await
                    .unwrap_err()
                    .kind,
                ToolErrorKind::InvalidInput
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
            assert!(fixture
                .ctx
                .files
                .fully_read(&path.to_string_lossy(), "original"));
        }
        for input in [
            json!({"path":"file"}),
            json!({"path":"file","edits":[]}),
            json!({"path":"file","edits":[{"old_string":"original"}]}),
            json!({"path":"file","edits":[
                {"old_string":"original","new_string":"changed"},
                {"old_string":"changed","new_string":"final","replace_all":"true"}
            ]}),
        ] {
            assert_eq!(
                MultiEditTool
                    .execute(input, &fixture.ctx)
                    .await
                    .unwrap_err()
                    .kind,
                ToolErrorKind::InvalidInput
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
            assert!(fixture
                .ctx
                .files
                .fully_read(&path.to_string_lossy(), "original"));
        }
        assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn later_edit_failures_leave_original_bytes_and_read_authorization_intact() {
        for (second, expected) in [
            (json!({"old_string":"absent","new_string":"x"}), "edit 2: old_string not found"),
            (json!({"old_string":"two","new_string":"x"}), "edit 2: old_string matches 2 times; add context to make it unique or set replace_all"),
            (json!({"old_string":"two","new_string":"two"}), "edit 2: old_string and new_string are identical"),
        ] {
            let fixture = Fixture::new();
            let path = fixture.write("file", "one two");
            fixture.ctx.files.record_read(&path.to_string_lossy());
            let error = MultiEditTool.execute(json!({"path":"file","edits":[
                {"old_string":"one","new_string":"two"}, second
            ]}), &fixture.ctx).await.unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::InvalidInput);
            assert_eq!(error.message, expected);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two");
            assert!(fixture.ctx.files.fully_read(&path.to_string_lossy(), "one two"));
            assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn ordered_edits_consume_previous_results_and_record_only_final_contents() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "one one\r\n🙂\n");
        fixture.ctx.files.record_read(&path.to_string_lossy());
        MultiEditTool
            .execute(
                json!({"path":"file","edits":[
                    {"old_string":"one","new_string":"two","replace_all":true},
                    {"old_string":"two two","new_string":"three"},
                    {"old_string":"🙂","new_string":"終"}
                ]}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "three\r\n終\n");
        assert!(fixture
            .ctx
            .files
            .fully_read(&path.to_string_lossy(), "three\r\n終\n"));
        assert!(fixture
            .ctx
            .files
            .check_snapshot(&path.to_string_lossy(), "two two\r\n🙂\n", false)
            .is_some());
        assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn stale_read_rejects_single_and_multi_edits_even_when_target_still_matches() {
        for multi in [false, true] {
            let fixture = Fixture::new();
            let path = fixture.write("file", "target\nold unrelated\n");
            fixture.ctx.files.record_read(&path.to_string_lossy());
            let current = "target\nnew unrelated\n";
            std::fs::write(&path, current).unwrap();
            let edit = json!({"old_string":"target","new_string":"updated"});
            let result = if multi {
                MultiEditTool
                    .execute(json!({"path":"file","edits":[edit]}), &fixture.ctx)
                    .await
            } else {
                EditFileTool
                    .execute(
                        json!({"path":"file","old_string":"target","new_string":"updated"}),
                        &fixture.ctx,
                    )
                    .await
            };
            let error = result.unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::Failed);
            assert!(error.message.contains("modified since"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), current);
            assert!(fixture
                .ctx
                .files
                .fully_read(&path.to_string_lossy(), "target\nold unrelated\n"));
            assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 1);
        }
    }

    #[tokio::test]
    async fn partial_edit_does_not_promote_unseen_content_to_a_full_read() {
        for multi in [false, true] {
            let fixture = Fixture::new();
            let path = fixture.write("file", "one\ntwo\n");
            ReadFileTool
                .execute(json!({"path":"file","limit":1}), &fixture.ctx)
                .await
                .unwrap();
            if multi {
                MultiEditTool
                    .execute(
                        json!({"path":"file","edits":[
                            {"old_string":"one","new_string":"changed"}
                        ]}),
                        &fixture.ctx,
                    )
                    .await
                    .unwrap();
            } else {
                EditFileTool
                    .execute(
                        json!({"path":"file","old_string":"one","new_string":"changed"}),
                        &fixture.ctx,
                    )
                    .await
                    .unwrap();
            }
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed\ntwo\n");
            assert_eq!(
                fixture
                    .ctx
                    .files
                    .check_snapshot(&path.to_string_lossy(), "changed\ntwo\n", false),
                None
            );
            let error = WriteFileTool
                .execute(json!({"path":"file","content":"new"}), &fixture.ctx)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::Failed);
            assert!(error.message.contains("not been fully shown"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed\ntwo\n");
        }
    }

    #[tokio::test]
    async fn multi_edit_failure_is_atomic_and_empty_content_is_valid() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "one two");
        ReadFileTool
            .execute(json!({"path":"file"}), &fixture.ctx)
            .await
            .unwrap();
        assert!(MultiEditTool.execute(json!({"path":"file","edits":[
            {"old_string":"one","new_string":"first"}, {"old_string":"absent","new_string":"bad"}
        ]}), &fixture.ctx).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one two");
        WriteFileTool
            .execute(json!({"path":"file","content":""}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }
}
