//! The `memory` tool: lets the agent persist durable facts/preferences/conventions
//! across sessions by appending them to a memory file (`AGENTS.md`). Two scopes:
//! `project` writes to `<cwd>/AGENTS.md` (this repo's conventions), `global` writes
//! to `~/.bob/AGENTS.md` (the user's standing preferences everywhere). Entries live
//! under a `## Memories` heading so they're grouped and human-editable.

use crate::core::types::ToolSpec;
use crate::tools::builtin::resolve_path;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;

pub struct MemoryTool;

/// The heading memory entries are collected under in the file.
const MEMORY_HEADING: &str = "## Memories";

#[async_trait]
impl Tool for MemoryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "memory".to_string(),
            description:
                "Save a durable fact, preference, or convention so it's remembered in future \
                sessions. Use it when the user states how they want things done, corrects your \
                approach, or shares a project fact that will matter next time — record WHY, not \
                just what. `scope: \"project\"` (default) appends to this repo's AGENTS.md; \
                `scope: \"global\"` appends to your user-wide ~/.bob/AGENTS.md for preferences \
                that apply everywhere. Keep each memory one concise line. Don't save one-off \
                details, secrets, or anything already written down."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The memory as one concise line (e.g. \"Always run `cargo fmt` before committing — the CI blocks on it\")." },
                    "scope": { "type": "string", "enum": ["project", "global"], "description": "project = this repo's AGENTS.md (default); global = ~/.bob/AGENTS.md (applies everywhere)." }
                },
                "required": ["content"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let content = input["content"].as_str().unwrap_or("").trim();
        if content.is_empty() {
            return Err(ToolError::invalid_input("content is required"));
        }
        let scope = input["scope"].as_str().unwrap_or("project");
        let path = match scope {
            "global" => global_memory_path()?,
            _ => resolve_path(&ctx.cwd, "AGENTS.md"),
        };

        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        // Skip if this exact line is already recorded (idempotent).
        let line = format!("- {}", content);
        if existing.lines().any(|l| l.trim() == line) {
            return Ok(format!("already remembered: {}", content));
        }
        let updated = append_memory(&existing, &line);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&path, updated)?;
        ctx.files.record_write(&path.to_string_lossy());
        Ok(format!(
            "saved {} memory to {}: {}",
            scope,
            path.display(),
            content
        ))
    }
}

/// Path to the user-global memory file (`~/.bob/AGENTS.md`).
fn global_memory_path() -> Result<PathBuf, ToolError> {
    let home = dirs::home_dir()
        .ok_or_else(|| ToolError::unavailable("no home directory for global memory"))?;
    Ok(home.join(".bob").join("AGENTS.md"))
}

/// Insert `line` under the `## Memories` heading, creating the heading (and the
/// file's structure) if absent. Preserves everything else in the file.
fn append_memory(existing: &str, line: &str) -> String {
    if let Some(idx) = existing.find(MEMORY_HEADING) {
        // Find the end of the memories section (next heading, or EOF) and append
        // the new line at its tail.
        let after_heading = idx + MEMORY_HEADING.len();
        let rest = &existing[after_heading..];
        // The section runs until the next top-of-line "## " heading.
        let section_end = rest
            .match_indices("\n## ")
            .next()
            .map(|(i, _)| after_heading + i)
            .unwrap_or(existing.len());
        let (head, tail) = existing.split_at(section_end);
        let head = head.trim_end();
        format!("{head}\n{line}\n{tail}")
            .trim_end_matches('\n')
            .to_string()
            + "\n"
    } else {
        // No memories section yet — add one at the end.
        let base = existing.trim_end();
        if base.is_empty() {
            format!("{MEMORY_HEADING}\n{line}\n")
        } else {
            format!("{base}\n\n{MEMORY_HEADING}\n{line}\n")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_section_in_empty_file() {
        let out = append_memory("", "- use tabs");
        assert!(out.contains("## Memories"));
        assert!(out.contains("- use tabs"));
    }

    #[test]
    fn appends_to_existing_section() {
        let existing = "# Project\n\n## Memories\n- first\n";
        let out = append_memory(existing, "- second");
        assert!(out.contains("- first"));
        assert!(out.contains("- second"));
        // Only one Memories heading.
        assert_eq!(out.matches("## Memories").count(), 1);
    }

    #[test]
    fn preserves_content_after_the_section() {
        let existing = "## Memories\n- a\n\n## Conventions\nkeep it simple\n";
        let out = append_memory(existing, "- b");
        assert!(out.contains("- a"));
        assert!(out.contains("- b"));
        assert!(out.contains("## Conventions"));
        assert!(out.contains("keep it simple"));
    }

    #[test]
    fn appends_below_existing_non_memory_content() {
        let existing = "# Project rules\nbe kind\n";
        let out = append_memory(existing, "- remember this");
        assert!(out.starts_with("# Project rules"));
        assert!(out.contains("be kind"));
        assert!(out.contains("## Memories"));
        assert!(out.contains("- remember this"));
    }
}
