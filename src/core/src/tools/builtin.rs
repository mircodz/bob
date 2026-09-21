use crate::core::types::ToolSpec;
use crate::tools::filesystem::{self, WriteRequest, MAX_OUTPUT_BYTES};
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub fn resolve_path(cwd: &str, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".to_string(),
            description: "Read a file from disk. Returns the contents with 1-based line numbers \
                (the numbers are display only — never include them in edits). Reads the whole \
                file by default; pass offset/limit only for very large files. Regular UTF-8 \
                files up to 16 MiB are supported. Output is capped at \
                2000 lines, 256 KiB of file text, and 2000 characters per line — when a file is longer the tail is \
                elided with a note. Page through lines with offset/limit; for clipped lines use \
                the reported offset/column_offset cursor to read the remaining characters. \
                You MUST read a file before editing it. Prefer this over `cat`/`head`/`tail` via \
                bash. When you need several files, issue multiple read calls in one step."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, relative to cwd or absolute." },
                    "offset": { "type": "integer", "minimum": 1, "description": "1-based line to start from." },
                    "limit": { "type": "integer", "minimum": 1, "description": "Max lines to return." },
                    "column_offset": { "type": "integer", "minimum": 1, "description": "1-based Unicode scalar position in the first returned line (default 1). Later lines start at column 1; each line shows at most 2000 characters." }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let path = filesystem::required_path(&input)?;
        let offset = filesystem::optional_usize(&input, "offset", 1)?;
        let limit = filesystem::optional_usize(&input, "limit", 2000)?.min(2000);
        let column_offset = filesystem::optional_usize(&input, "column_offset", 1)?;
        if offset == 0 || limit == 0 || column_offset == 0 {
            return Err(ToolError::invalid_input(
                "offset, limit, and column_offset must be positive integers",
            ));
        }
        let full = filesystem::workspace_path(&ctx.cwd, Path::new(path))?;
        let text = filesystem::read_regular(&full)?;
        let instructions =
            crate::core::project_context::scoped_read_instructions(Path::new(&ctx.cwd), &full)?;
        let start = offset - 1;
        let mut out = String::new();
        let mut visible = Vec::new();
        let mut byte_offset = 0;
        let mut shown_lines = 0;
        let mut truncated = false;
        let lines: Vec<&str> = if text.is_empty() {
            vec![""]
        } else {
            text.split_inclusive('\n').collect()
        };
        for (index, line) in lines.iter().enumerate() {
            let end = byte_offset + line.len();
            if index < start {
                byte_offset = end;
                continue;
            }
            if shown_lines == limit {
                truncated = true;
                break;
            }
            let display = line.strip_suffix('\n').unwrap_or(line);
            let column = if index == start { column_offset } else { 1 };
            let shown_start = display
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(display.len()))
                .nth(column - 1)
                .ok_or_else(|| {
                    ToolError::invalid_input("column_offset is beyond the end of the line")
                })?;
            let shown_end = shown_start
                + display[shown_start..]
                    .char_indices()
                    .nth(2000)
                    .map_or(display.len() - shown_start, |(i, _)| i);
            let line_truncated = shown_end < display.len();
            let continuation = if line_truncated {
                format!(
                    "... [line truncated; use offset={} and column_offset={} to continue]",
                    index + 1,
                    column + 2000
                )
            } else {
                String::new()
            };
            let rendered = format!(
                "{:>6}\t{}{}",
                index + 1,
                &display[shown_start..shown_end],
                continuation
            );
            if out.len() + rendered.len() + 1 > MAX_OUTPUT_BYTES {
                truncated = true;
                break;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&rendered);
            visible.push(
                byte_offset + shown_start..if line_truncated {
                    byte_offset + shown_end
                } else {
                    end
                },
            );
            byte_offset = end;
            shown_lines += 1;
        }
        if truncated {
            out.push_str(&format!(
                "\n\n... [output truncated; use offset={} and limit to read further]",
                start.saturating_add(shown_lines).saturating_add(1)
            ));
        }
        if shown_lines == 0 {
            out.push_str("(offset is beyond end of file)");
        } else {
            ctx.files
                .record_visible(&full.to_string_lossy(), &text, visible);
        }
        if !instructions.is_empty() {
            out.push_str("\n\n");
            out.push_str(&instructions);
        }
        Ok(out.into())
    }
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".to_string(),
            description: "Create a file or replace its entire contents. Prefer edit_file or \
                multi_edit for existing code. Read an existing file's current contents before \
                overwriting it (including every page; truncated lines do not count as fully read), \
                preserve unrelated work, and use full replacement only when \
                deliberate. Do not create documentation/README files unless requested. The \
                supplied content is written without automatic formatting or headers."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let path = filesystem::required_path(&input)?;
        let content = filesystem::required_string(&input, "content")?;
        let full = filesystem::workspace_path(&ctx.cwd, Path::new(path))?;
        let expected = match std::fs::symlink_metadata(&full) {
            Ok(_) => {
                let before = filesystem::read_regular(&full)?;
                if let Some(error) =
                    ctx.files
                        .check_snapshot(&full.to_string_lossy(), &before, true)
                {
                    return Err(ToolError::failed(error));
                }
                Some(before)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let batch = filesystem::prepare_batch(
            ctx,
            vec![WriteRequest {
                path: PathBuf::from(path),
                resolved: full.clone(),
                content: content.to_string(),
                expected,
            }],
        )
        .await?;
        let written = batch.commit()?;
        ctx.files
            .record_content(&written[0].to_string_lossy(), content, true);
        crate::tools::lsp::sync_after_edit(&ctx.lsp, &written[0], content).await;
        Ok(format!("wrote {} bytes to {}", content.len(), path).into())
    }

    fn preview(&self, input: &Value, ctx: &ToolContext) -> Option<String> {
        let path = filesystem::required_path(input).ok()?;
        let content = filesystem::required_string(input, "content").ok()?;
        let full = filesystem::workspace_path(&ctx.cwd, Path::new(path)).ok()?;
        match filesystem::read_regular(&full) {
            Ok(before) => {
                use crate::tools::diff::{compact_diff, diff_lines, diff_stat, format_unified};
                let full_diff = diff_lines(&before, content);
                let (added, removed) = diff_stat(&full_diff);
                let compact = compact_diff(&full_diff, 3);
                Some(filesystem::bounded_output(format!(
                    "overwrite {} (+{} -{})\n```diff {}\n{}\n```",
                    path,
                    added,
                    removed,
                    path,
                    format_unified(&compact)
                )))
            }
            Err(error) if error.kind == crate::tools::registry::ToolErrorKind::NotFound => {
                let preview: String = content.lines().take(40).collect::<Vec<_>>().join("\n");
                Some(filesystem::bounded_output(format!(
                    "create {}\n```{}\n{}\n```",
                    path, path, preview
                )))
            }
            Err(_) => None,
        }
    }
}

pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "list_dir".to_string(),
            description: "List the immediate entries of a directory (defaults to the working \
                directory). Use for a quick look at what's in a folder; use `glob` to find files \
                by pattern across a tree, and `grep` to search contents. Prefer this over `ls` \
                via bash."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let path = match input.get("path") {
            None => ".",
            Some(_) => filesystem::required_path(&input)?,
        };
        let full = filesystem::workspace_path(&ctx.cwd, Path::new(path))?;
        let mut entries = Vec::new();
        let mut bytes = 0;
        let mut truncated = false;
        for entry in std::fs::read_dir(&full)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if entries.len() == 2000 || bytes + name.len() + 1 > MAX_OUTPUT_BYTES {
                truncated = true;
                break;
            }
            bytes += name.len() + 1;
            entries.push(name);
        }
        entries.sort();
        let mut output = if entries.is_empty() {
            "(empty)".to_string()
        } else {
            entries.join("\n")
        };
        if truncated {
            output.push_str("\n... [directory listing truncated; use glob to narrow the search]");
        }
        Ok(output.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files::FileFixture as Fixture;
    use crate::tools::edit::EditFileTool;
    use crate::tools::registry::ToolErrorKind;
    use crate::tools::schema::CompiledSchema;

    #[test]
    fn read_only_classification() {
        assert!(ReadFileTool.is_read_only());
        assert!(ListDirTool.is_read_only());
        assert!(!WriteFileTool.is_read_only());
    }

    #[tokio::test]
    async fn reading_error_text_is_success_and_missing_file_is_not_found() {
        let fixture = Fixture::new();
        fixture.write("log", "error: mismatched types\nsecond line");
        let result = ReadFileTool
            .execute(json!({"path":"log"}), &fixture.ctx)
            .await
            .unwrap();
        assert!(result
            .as_text()
            .unwrap()
            .contains("error: mismatched types"));
        let error = ReadFileTool
            .execute(json!({"path":"missing"}), &fixture.ctx)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::NotFound);
    }

    #[tokio::test]
    async fn malformed_writes_leave_bytes_unchanged() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "original");
        ReadFileTool
            .execute(json!({"path":"file"}), &fixture.ctx)
            .await
            .unwrap();
        for input in [
            json!({"path":"file"}),
            json!({"path":"file","content":null}),
            json!({"content":"new"}),
            json!({"path":"","content":"new"}),
        ] {
            assert_eq!(
                WriteFileTool
                    .execute(input, &fixture.ctx)
                    .await
                    .unwrap_err()
                    .kind,
                ToolErrorKind::InvalidInput
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
        assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn overwrite_requires_complete_current_visible_read() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "one\ntwo\nthree\n");
        let write = json!({"path":"file","content":"replacement"});
        assert!(WriteFileTool
            .execute(write.clone(), &fixture.ctx)
            .await
            .is_err());
        ReadFileTool
            .execute(json!({"path":"file","limit":1}), &fixture.ctx)
            .await
            .unwrap();
        assert!(WriteFileTool
            .execute(write.clone(), &fixture.ctx)
            .await
            .is_err());
        ReadFileTool
            .execute(json!({"path":"file","offset":2,"limit":2}), &fixture.ctx)
            .await
            .unwrap();
        WriteFileTool.execute(write, &fixture.ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
    }

    #[tokio::test]
    async fn truncated_lines_and_capped_pages_cannot_authorize_overwrite() {
        let fixture = Fixture::new();
        fixture.write("long", &"x".repeat(3000));
        let output = ReadFileTool
            .execute(json!({"path":"long"}), &fixture.ctx)
            .await
            .unwrap();
        assert!(output.as_text().unwrap().contains("line truncated"));
        assert!(WriteFileTool
            .execute(json!({"path":"long","content":"new"}), &fixture.ctx)
            .await
            .is_err());
        fixture.write("many", &"line\n".repeat(2001));
        let output = ReadFileTool
            .execute(json!({"path":"many","limit":999999}), &fixture.ctx)
            .await
            .unwrap();
        assert!(output.as_text().unwrap().contains("output truncated"));
        assert!(WriteFileTool
            .execute(json!({"path":"many","content":"new"}), &fixture.ctx)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn invalid_read_pagination_and_beyond_eof_do_not_record_a_read() {
        let fixture = Fixture::new();
        fixture.write("file", "text");
        for input in [
            json!({"path":"file","offset":0}),
            json!({"path":"file","limit":-1}),
            json!({"path":"file","limit":"all"}),
        ] {
            assert_eq!(
                ReadFileTool
                    .execute(input, &fixture.ctx)
                    .await
                    .unwrap_err()
                    .kind,
                ToolErrorKind::InvalidInput
            );
        }
        ReadFileTool
            .execute(json!({"path":"file","offset":999}), &fixture.ctx)
            .await
            .unwrap();
        assert!(EditFileTool
            .execute(
                json!({"path":"file","old_string":"text","new_string":"new"}),
                &fixture.ctx
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn column_continuation_allows_complete_read_of_2001_character_line() {
        for newline in ["", "\n"] {
            let fixture = Fixture::new();
            fixture.write("long", &format!("{}x{newline}", "a".repeat(2000)));
            let write = json!({"path":"long","content":"replacement"});
            let first = ReadFileTool
                .execute(json!({"path":"long"}), &fixture.ctx)
                .await
                .unwrap();
            assert!(first
                .as_text()
                .unwrap()
                .contains("offset=1 and column_offset=2001"));
            assert!(!first.as_text().unwrap().contains("ax"));
            assert!(WriteFileTool
                .execute(write.clone(), &fixture.ctx)
                .await
                .is_err());
            let last = ReadFileTool
                .execute(
                    json!({"path":"long","offset":1,"column_offset":2001}),
                    &fixture.ctx,
                )
                .await
                .unwrap();
            assert_eq!(last.as_text().unwrap().lines().next().unwrap(), "     1\tx");
            assert!(!last.as_text().unwrap().contains("line truncated"));
            WriteFileTool.execute(write, &fixture.ctx).await.unwrap();
        }
    }

    #[tokio::test]
    async fn unicode_column_pages_merge_coverage_without_filling_unseen_gaps() {
        let fixture = Fixture::new();
        fixture.write("long", &format!("{}終\nnext\n", "é🙂".repeat(2000)));
        let write = json!({"path":"long","content":"replacement"});
        let tail = ReadFileTool
            .execute(json!({"path":"long","column_offset":4001}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(
            tail.as_text().unwrap().lines().next().unwrap(),
            "     1\t終"
        );
        assert!(tail.as_text().unwrap().contains("     2\tnext"));
        assert!(WriteFileTool
            .execute(write.clone(), &fixture.ctx)
            .await
            .is_err());
        let first = ReadFileTool
            .execute(json!({"path":"long","limit":1}), &fixture.ctx)
            .await
            .unwrap();
        assert!(first
            .as_text()
            .unwrap()
            .contains(&format!("{}... [line truncated", "é🙂".repeat(1000))));
        assert!(first.as_text().unwrap().contains("column_offset=2001"));
        assert!(WriteFileTool
            .execute(write.clone(), &fixture.ctx)
            .await
            .is_err());
        let middle = ReadFileTool
            .execute(
                json!({"path":"long","offset":1,"column_offset":2001,"limit":1}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert!(middle
            .as_text()
            .unwrap()
            .contains(&format!("{}... [line truncated", "é🙂".repeat(1000))));
        assert!(middle.as_text().unwrap().contains("column_offset=4001"));
        WriteFileTool.execute(write, &fixture.ctx).await.unwrap();
    }

    #[tokio::test]
    async fn column_pages_from_different_snapshots_do_not_authorize_overwrite() {
        let fixture = Fixture::new();
        let path = fixture.write("long", &"a".repeat(2001));
        ReadFileTool
            .execute(json!({"path":"long"}), &fixture.ctx)
            .await
            .unwrap();
        let changed = format!("b{}", "a".repeat(2000));
        std::fs::write(&path, &changed).unwrap();
        ReadFileTool
            .execute(json!({"path":"long","column_offset":2001}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(
            fixture
                .ctx
                .files
                .check_snapshot(&path.to_string_lossy(), &changed, false),
            None
        );
        let error = WriteFileTool
            .execute(json!({"path":"long","content":"replacement"}), &fixture.ctx)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::Failed);
        assert!(error.message.contains("not been fully shown"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), changed);
    }

    #[tokio::test]
    async fn unicode_columns_count_scalars_and_reset_for_later_lines() {
        let fixture = Fixture::new();
        let content = "e\u{301}🙂終\nnext\n";
        let path = fixture.write("file", content);
        let output = ReadFileTool
            .execute(json!({"path":"file","column_offset":2}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(
            output
                .as_text()
                .unwrap()
                .lines()
                .take(2)
                .collect::<Vec<_>>(),
            ["     1\t\u{301}🙂終", "     2\tnext"]
        );
        assert!(!fixture
            .ctx
            .files
            .fully_read(&path.to_string_lossy(), content));
        let output = ReadFileTool
            .execute(
                json!({"path":"file","column_offset":5,"limit":1}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert_eq!(output.as_text().unwrap().lines().next(), Some("     1\t"));
        assert!(!fixture
            .ctx
            .files
            .fully_read(&path.to_string_lossy(), content));
        ReadFileTool
            .execute(json!({"path":"file","limit":1}), &fixture.ctx)
            .await
            .unwrap();
        assert!(fixture
            .ctx
            .files
            .fully_read(&path.to_string_lossy(), content));
    }

    #[tokio::test]
    async fn byte_capped_output_records_only_rendered_lines_and_can_be_completed() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.dir.join(".git")).unwrap();
        let content = format!("{}\n", "x".repeat(2000)).repeat(132);
        let path = fixture.write("file", &content);
        let output = ReadFileTool
            .execute(json!({"path":"file"}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(
            output
                .as_text()
                .unwrap()
                .lines()
                .filter(|line| line.contains('\t'))
                .count(),
            130
        );
        assert!(output
            .as_text()
            .unwrap()
            .contains("use offset=131 and limit to read further"));
        assert!(!output.as_text().unwrap().contains("line truncated"));
        assert!(!fixture
            .ctx
            .files
            .fully_read(&path.to_string_lossy(), &content));
        let tail = ReadFileTool
            .execute(json!({"path":"file","offset":131}), &fixture.ctx)
            .await
            .unwrap();
        assert_eq!(
            tail.as_text()
                .unwrap()
                .lines()
                .filter(|line| line.contains('\t'))
                .count(),
            2
        );
        assert!(!tail.as_text().unwrap().contains("truncated"));
        assert!(fixture
            .ctx
            .files
            .fully_read(&path.to_string_lossy(), &content));
    }

    #[tokio::test]
    async fn overwrite_rejects_stale_full_read_until_current_contents_are_read() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "before");
        ReadFileTool
            .execute(json!({"path":"file"}), &fixture.ctx)
            .await
            .unwrap();
        std::fs::write(&path, "after!").unwrap();
        let input = json!({"path":"file","content":"replacement"});
        let error = WriteFileTool
            .execute(input.clone(), &fixture.ctx)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::Failed);
        assert!(error.message.contains("modified since"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "after!");
        assert!(fixture
            .ctx
            .files
            .fully_read(&path.to_string_lossy(), "before"));
        assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 1);
        ReadFileTool
            .execute(json!({"path":"file"}), &fixture.ctx)
            .await
            .unwrap();
        WriteFileTool.execute(input, &fixture.ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
        assert!(fixture
            .ctx
            .files
            .fully_read(&path.to_string_lossy(), "replacement"));
    }

    #[tokio::test]
    async fn read_numeric_schema_and_execution_boundaries() {
        let schema = CompiledSchema::compile(&ReadFileTool.spec().input_schema).unwrap();
        for name in ["offset", "limit", "column_offset"] {
            for (case, value, schema_accepts, execution_accepts) in [
                ("minimum", json!(1), true, true),
                ("integral float", json!(1.0), true, true),
                ("zero", json!(0), false, false),
                ("negative", json!(-1.0), false, false),
                ("fractional", json!(1.5), false, false),
                ("overflow", json!(1e100), true, false),
                ("string", json!("1"), false, false),
                ("null", json!(null), false, false),
            ] {
                let fixture = Fixture::new();
                fixture.write("file", "text\nnext\n");
                let mut input = json!({"path":"file"});
                input[name] = value;
                assert_eq!(
                    schema.validate(&input).is_ok(),
                    schema_accepts,
                    "{name}: {case}"
                );
                let result = ReadFileTool.execute(input, &fixture.ctx).await;
                assert_eq!(
                    result.is_ok(),
                    execution_accepts,
                    "{name}: {case}: {result:?}"
                );
                if let Err(error) = result {
                    assert_eq!(error.kind, ToolErrorKind::InvalidInput, "{name}: {case}");
                    assert!(
                        WriteFileTool
                            .execute(json!({"path":"file","content":"replacement"}), &fixture.ctx)
                            .await
                            .is_err(),
                        "{name}: {case} must not authorize a write"
                    );
                }
            }
        }
        let fixture = Fixture::new();
        fixture.write("file", "text\nnext\n");
        let beyond_line = json!({"path":"file","column_offset":6});
        assert!(schema.validate(&beyond_line).is_ok());
        assert_eq!(
            ReadFileTool
                .execute(beyond_line, &fixture.ctx)
                .await
                .unwrap_err()
                .kind,
            ToolErrorKind::InvalidInput
        );
        assert!(WriteFileTool
            .execute(json!({"path":"file","content":"replacement"}), &fixture.ctx)
            .await
            .is_err());
        let output = ReadFileTool
            .execute(
                json!({"path":"file","offset":1.0,"limit":2.0,"column_offset":2.0}),
                &fixture.ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            output
                .as_text()
                .unwrap()
                .lines()
                .take(2)
                .collect::<Vec<_>>(),
            ["     1\text", "     2\tnext"]
        );
        assert!(WriteFileTool
            .execute(json!({"path":"file","content":"replacement"}), &fixture.ctx)
            .await
            .is_err());
    }
}
