//! Built-in filesystem tools: read_file, write_file, list_dir. Each is a small
//! struct implementing the `Tool` trait. (The `bash` tool lives in `bash.rs`.)

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Resolve a possibly-relative path against the tool context's cwd.
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
                file by default; pass offset/limit only for very large files. Output is capped at \
                2000 lines and each line at 2000 characters — when a file is longer the tail is \
                elided with a note, so page through it with offset/limit if you need the rest. \
                You MUST read a file before editing it. Prefer this over `cat`/`head`/`tail` via \
                bash. When you need several files, issue multiple read calls in one step."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path, relative to cwd or absolute." },
                    "offset": { "type": "number", "description": "1-based line to start from." },
                    "limit": { "type": "number", "description": "Max lines to return." }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let path = input["path"].as_str().unwrap_or("");
        let full = resolve_path(&ctx.cwd, path);
        let text = std::fs::read_to_string(&full)?;
        ctx.files.record_read(&full.to_string_lossy());

        // Default caps mirror Claude Code: at most ~2000 lines, and each line
        // truncated to ~2000 chars, so a large or minified file can't flood the
        // context window. An explicit offset/limit overrides the line cap.
        const DEFAULT_LINE_LIMIT: usize = 2000;
        const MAX_LINE_CHARS: usize = 2000;

        let lines: Vec<&str> = text.split('\n').collect();
        let total = lines.len();
        let offset = input["offset"].as_u64().unwrap_or(0) as usize;
        let start = if offset > 0 { offset - 1 } else { 0 };
        let end = match input["limit"].as_u64() {
            Some(l) => (start + l as usize).min(total),
            None => (start + DEFAULT_LINE_LIMIT).min(total),
        };
        if start >= total {
            return Ok(String::new());
        }
        let mut out = lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, l)| {
                let shown = if l.chars().count() > MAX_LINE_CHARS {
                    let head: String = l.chars().take(MAX_LINE_CHARS).collect();
                    format!("{head}... [line truncated]")
                } else {
                    (*l).to_string()
                };
                format!("{:>6}\t{}", start + i + 1, shown)
            })
            .collect::<Vec<_>>()
            .join("\n");
        if end < total {
            out.push_str(&format!(
                "\n\n... [{} more lines; use offset/limit to read further]",
                total - end
            ));
        }
        Ok(out)
    }
}

pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".to_string(),
            description: "Create a new file, or completely overwrite an existing one, with the \
                given content. PREFER `edit_file`/`multi_edit` for changing existing files — only \
                use write_file for genuinely new files or a deliberate full rewrite. If the file \
                exists you must read it first (overwriting unseen content is rejected). Do NOT \
                create documentation/README files unless the user explicitly asks. Writes exactly \
                what you provide — no automatic formatting or headers."
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
        let path = input["path"].as_str().unwrap_or("");
        let content = input["content"].as_str().unwrap_or("");
        let full = resolve_path(&ctx.cwd, path);
        if let Some(parent) = full.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&full, content)?;
        ctx.files.record_write(&full.to_string_lossy());
        Ok(format!("wrote {} bytes to {}", content.len(), path))
    }

    fn preview(&self, input: &Value, ctx: &ToolContext) -> Option<String> {
        let path = input["path"].as_str()?;
        let content = input["content"].as_str().unwrap_or("");
        let full = resolve_path(&ctx.cwd, path);
        match std::fs::read_to_string(&full) {
            // Overwriting an existing file → show the diff.
            Ok(before) => {
                use crate::tools::diff::{compact_diff, diff_lines, diff_stat, format_unified};
                let full_diff = diff_lines(&before, content);
                let (added, removed) = diff_stat(&full_diff);
                let compact = compact_diff(&full_diff, 3);
                Some(format!(
                    "overwrite {} (+{} -{})\n```diff {}\n{}\n```",
                    path,
                    added,
                    removed,
                    path,
                    format_unified(&compact)
                ))
            }
            // New file → show the content as a highlighted code block.
            Err(_) => {
                let preview: String = content.lines().take(40).collect::<Vec<_>>().join("\n");
                Some(format!("create {}\n```{}\n{}\n```", path, path, preview))
            }
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
        let path = input["path"].as_str().unwrap_or(".");
        let full = resolve_path(&ctx.cwd, path);
        let mut entries: Vec<String> = std::fs::read_dir(&full)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        entries.sort();
        Ok(if entries.is_empty() {
            "(empty)".to_string()
        } else {
            entries.join("\n")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::{Tool, ToolErrorKind};

    #[test]
    fn read_only_classification() {
        // Reads/searches are read-only; writes/edits/bash are not. This is the
        // single source of truth the concurrency split + explore-subset consume.
        assert!(ReadFileTool.is_read_only());
        assert!(ListDirTool.is_read_only());
        assert!(!WriteFileTool.is_read_only());
    }

    fn ctx(cwd: &str) -> ToolContext {
        ToolContext {
            cwd: cwd.to_string(),
            files: std::sync::Arc::new(crate::tools::file_tracker::FileTracker::new()),
            todos: std::sync::Arc::new(crate::tools::todo::TodoStore::new()),
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: None,
            coord: None,
            permissions: None,
        }
    }

    /// The whole point of the typed-error refactor: a file whose CONTENT starts
    /// with "error:" must read successfully, not be misclassified as a failure
    /// (the old `output.starts_with("error:")` bug).
    #[tokio::test]
    async fn reading_a_file_containing_error_text_is_ok() {
        let dir = std::env::temp_dir().join(format!("bobtest_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.txt");
        std::fs::write(&path, "error: mismatched types\nsecond line").unwrap();

        let result = ReadFileTool
            .execute(json!({ "path": path.to_string_lossy() }), &ctx("."))
            .await;
        assert!(result.is_ok(), "reading a file must not be an error");
        assert!(result.unwrap().contains("error: mismatched types"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A missing file yields a typed NotFound (from the io::Error conversion),
    /// so the model can react to the category rather than parse prose.
    #[tokio::test]
    async fn reading_a_missing_file_is_not_found() {
        let result = ReadFileTool
            .execute(json!({ "path": "/no/such/bob/file.xyz" }), &ctx("."))
            .await;
        let err = result.expect_err("missing file must be an error");
        assert_eq!(err.kind, ToolErrorKind::NotFound);
    }
}
