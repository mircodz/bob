//! Surgical string-replacement edit tools with staleness checks and a compact
//! unified diff appended to the result.

use crate::core::types::ToolSpec;
use crate::tools::builtin::resolve_path;
use crate::tools::diff::{compact_diff, diff_lines, diff_stat, format_unified};
use crate::tools::registry::{Tool, ToolContext};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Build the standard edit result string with a compact unified diff appended.
/// The diff fence is tagged with the file path so a UI can pick the right
/// syntax for highlighting (e.g. ```diff src/main.rs).
fn edit_result(path: &str, before: &str, after: &str) -> String {
    let full = diff_lines(before, after);
    let (added, removed) = diff_stat(&full);
    let compact = compact_diff(&full, 3);
    format!(
        "edited {} (+{} -{})\n```diff {}\n{}\n```",
        path,
        added,
        removed,
        path,
        format_unified(&compact)
    )
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

/// Apply one string replacement to `content`, enforcing the uniqueness rules.
/// Returns the updated content, or a human-readable error (used verbatim in the
/// tool result, so `label` prefixes it, e.g. "edit 2: "). `old_string` must match
/// exactly once unless `replace_all` is set.
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

/// Compute the would-be new content of a single edit, without touching disk.
/// Returns None if the file can't be read or the edit wouldn't apply cleanly.
fn compute_edit(
    cwd: &str,
    path: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Option<(String, String)> {
    let full = resolve_path(cwd, path);
    let content = std::fs::read_to_string(&full).ok()?;
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let path = input["path"].as_str().unwrap_or("");
        let old_string = input["old_string"].as_str().unwrap_or("");
        let new_string = input["new_string"].as_str().unwrap_or("");
        let replace_all = input["replace_all"].as_bool().unwrap_or(false);
        let full = resolve_path(&ctx.cwd, path);
        let full_str = full.to_string_lossy().to_string();

        if let Some(stale) = ctx.files.check_editable(&full_str) {
            return format!("error: {}", stale);
        }

        let content = match std::fs::read_to_string(&full) {
            Ok(c) => c,
            Err(e) => return format!("error: {}", e),
        };
        let updated = match apply_edit(&content, old_string, new_string, replace_all, "") {
            Ok(u) => u,
            Err(e) => return format!("error: {}", e),
        };

        if let Err(e) = std::fs::write(&full, &updated) {
            return format!("error: {}", e);
        }
        ctx.files.record_write(&full_str);
        edit_result(path, &content, &updated)
    }

    fn preview(&self, input: &Value, ctx: &ToolContext) -> Option<String> {
        let path = input["path"].as_str()?;
        let (before, after) = compute_edit(
            &ctx.cwd,
            path,
            input["old_string"].as_str().unwrap_or(""),
            input["new_string"].as_str().unwrap_or(""),
            input["replace_all"].as_bool().unwrap_or(false),
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let path = input["path"].as_str().unwrap_or("");
        let full = resolve_path(&ctx.cwd, path);
        let full_str = full.to_string_lossy().to_string();

        if let Some(stale) = ctx.files.check_editable(&full_str) {
            return format!("error: {}", stale);
        }

        let empty = vec![];
        let edits = input["edits"].as_array().unwrap_or(&empty);
        let mut content = match std::fs::read_to_string(&full) {
            Ok(c) => c,
            Err(e) => return format!("error: {}", e),
        };
        let original = content.clone();

        for (i, e) in edits.iter().enumerate() {
            let old_string = e["old_string"].as_str().unwrap_or("");
            let new_string = e["new_string"].as_str().unwrap_or("");
            let replace_all = e["replace_all"].as_bool().unwrap_or(false);
            content = match apply_edit(
                &content,
                old_string,
                new_string,
                replace_all,
                &format!("edit {i}: "),
            ) {
                Ok(u) => u,
                Err(err) => return format!("error: {}", err),
            };
        }

        if let Err(e) = std::fs::write(&full, &content) {
            return format!("error: {}", e);
        }
        ctx.files.record_write(&full_str);
        edit_result(path, &original, &content)
    }

    fn preview(&self, input: &Value, ctx: &ToolContext) -> Option<String> {
        let path = input["path"].as_str()?;
        let full = resolve_path(&ctx.cwd, path);
        let original = std::fs::read_to_string(&full).ok()?;
        let mut content = original.clone();
        for e in input["edits"].as_array()? {
            let old_string = e["old_string"].as_str().unwrap_or("");
            let new_string = e["new_string"].as_str().unwrap_or("");
            let replace_all = e["replace_all"].as_bool().unwrap_or(false);
            let occ = count_occurrences(&content, old_string);
            if occ == 0 || (occ > 1 && !replace_all) {
                return None; // wouldn't apply cleanly; skip preview
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
