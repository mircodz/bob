//! Built-in tools: read, write, list_dir, bash. Each is a small struct
//! implementing the `Tool` trait.

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext};
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
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".to_string(),
            description: "Read a file from disk. Returns the contents with 1-based line numbers \
                (the numbers are display only — never include them in edits). Reads the whole \
                file by default; pass offset/limit only for very large files. You MUST read a \
                file before editing it. Prefer this over `cat`/`head`/`tail` via bash. When you \
                need several files, issue multiple read calls in one step."
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let path = input["path"].as_str().unwrap_or("");
        let full = resolve_path(&ctx.cwd, path);
        let text = match std::fs::read_to_string(&full) {
            Ok(t) => t,
            Err(e) => return format!("error: {}", e),
        };
        ctx.files.record_read(&full.to_string_lossy());

        let lines: Vec<&str> = text.split('\n').collect();
        let offset = input["offset"].as_u64().unwrap_or(0) as usize;
        let start = if offset > 0 { offset - 1 } else { 0 };
        let end = match input["limit"].as_u64() {
            Some(l) => (start + l as usize).min(lines.len()),
            None => lines.len(),
        };
        if start >= lines.len() {
            return String::new();
        }
        lines[start..end]
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{:>6}\t{}", start + i + 1, l))
            .collect::<Vec<_>>()
            .join("\n")
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let path = input["path"].as_str().unwrap_or("");
        let content = input["content"].as_str().unwrap_or("");
        let full = resolve_path(&ctx.cwd, path);
        if let Some(parent) = full.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&full, content) {
            return format!("error: {}", e);
        }
        ctx.files.record_write(&full.to_string_lossy());
        format!("wrote {} bytes to {}", content.len(), path)
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let path = input["path"].as_str().unwrap_or(".");
        let full = resolve_path(&ctx.cwd, path);
        let mut entries: Vec<String> = match std::fs::read_dir(&full) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect(),
            Err(e) => return format!("error: {}", e),
        };
        entries.sort();
        if entries.is_empty() {
            "(empty)".to_string()
        } else {
            entries.join("\n")
        }
    }
}

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".to_string(),
            description:
                "Run a shell command via `bash -c` and return its combined stdout/stderr. \
                Use this to actually DO things: run builds, tests, linters, git, package managers, \
                and scripts. Do NOT use it to read, search, or list files — use read_file, grep, \
                glob, and list_dir instead (they're faster and cleaner). Guidance: commands run \
                from the working directory, so don't `cd` unless asked; quote paths that contain \
                spaces; chain related steps with `&&`; avoid destructive commands (`rm -rf`, \
                `git push`, `git reset --hard`) unless explicitly requested; never commit or push \
                unless the user asks. Set `timeout` (seconds) to bound a command that might hang; \
                it's killed and reported if it exceeds that. Set `run_in_background: true` for \
                long-running work (a dev server, a watch build) you don't want to block on — it \
                returns a job id immediately; poll it with job_status / job_output."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout": { "type": "number", "description": "Max seconds to wait before killing the command (foreground only)." },
                    "run_in_background": { "type": "boolean", "description": "Run detached and return a job id instead of blocking." }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let command = input["command"].as_str().unwrap_or("").to_string();
        let cwd = ctx.cwd.clone();

        // Background: spawn detached, register as a job, return its id at once. The
        // model collects the result later via job_status / job_output.
        if input["run_in_background"].as_bool().unwrap_or(false) {
            let id = ctx.jobs.next_id();
            let jobs = ctx.jobs.clone();
            let job_id = id.clone();
            let bg_cmd = command.clone();
            let handle = tokio::spawn(async move {
                let out = tokio::task::spawn_blocking(move || {
                    std::process::Command::new("bash")
                        .arg("-c")
                        .arg(&bg_cmd)
                        .current_dir(&cwd)
                        .output()
                })
                .await;
                let (status, text) = match out {
                    Ok(Ok(o)) => (
                        if o.status.success() {
                            crate::tools::jobs::JobStatus::Done
                        } else {
                            crate::tools::jobs::JobStatus::Failed
                        },
                        combine_output(&o.stdout, &o.stderr, o.status.code()),
                    ),
                    Ok(Err(e)) => (
                        crate::tools::jobs::JobStatus::Failed,
                        format!("error: {}", e),
                    ),
                    Err(e) => (
                        crate::tools::jobs::JobStatus::Failed,
                        format!("error: {}", e),
                    ),
                };
                jobs.finish(&job_id, status, text);
            });
            ctx.jobs
                .register(id.clone(), "bash", truncate_desc(&command), handle);
            return format!(
                "started background job {id}: {}\nPoll with job_status / job_output.",
                truncate_desc(&command)
            );
        }

        // Foreground: run to completion, optionally bounded by a timeout.
        let run = tokio::task::spawn_blocking(move || {
            std::process::Command::new("bash")
                .arg("-c")
                .arg(&command)
                .current_dir(&cwd)
                .output()
        });

        let result = match input["timeout"].as_f64() {
            Some(secs) if secs > 0.0 => {
                let dur = std::time::Duration::from_secs_f64(secs);
                match tokio::time::timeout(dur, run).await {
                    Ok(r) => r,
                    Err(_) => {
                        return format!(
                            "error: command timed out after {}s (still running detached; \
                             it was not killed cleanly — prefer run_in_background for long work)",
                            secs
                        );
                    }
                }
            }
            _ => run.await,
        };

        match result {
            Ok(Ok(out)) => combine_output(&out.stdout, &out.stderr, out.status.code()),
            Ok(Err(e)) => format!("error: {}", e),
            Err(e) => format!("error: {}", e),
        }
    }
}

/// Merge stdout+stderr into the single text block the model sees, falling back to
/// an exit-code note when a command produced no output.
fn combine_output(stdout: &[u8], stderr: &[u8], code: Option<i32>) -> String {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    let combined = [stdout.trim_end(), stderr.trim_end()]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let combined = combined.trim().to_string();
    if combined.is_empty() {
        format!("(exit {})", code.unwrap_or(-1))
    } else {
        combined
    }
}

/// A short one-line description of a command, for the jobs panel.
fn truncate_desc(cmd: &str) -> String {
    let line = cmd.replace('\n', " ");
    if line.chars().count() > 60 {
        format!("{}...", line.chars().take(60).collect::<String>())
    } else {
        line
    }
}
