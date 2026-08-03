//! LSP-powered code actions that MUTATE files. Kept separate from the read-only
//! `lsp` tool because these must go through the permission prompt (they rewrite
//! source across the whole project), whereas `lsp` is auto-approved.
//!
//! Two tools, sharing the WorkspaceEdit-application machinery below:
//!   - `rename_symbol`   — drives `textDocument/rename` (the most common action,
//!     with its own `newName` param).
//!   - `code_action`     — the general surface: lists and applies any
//!     `textDocument/codeAction` (quick-fixes, refactors, "organize imports", …).
//!
//! Doing these through the server is correct where a text search-replace would
//! clobber unrelated matches, miss shadowed scopes, or can't compute the fix.

use crate::core::types::ToolSpec;
use crate::tools::builtin::resolve_path;
use crate::tools::diff::{compact_diff, diff_lines, diff_stat, format_unified};
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// One-shot rename: the single most common refactor, with a dedicated tool so
/// the model does it in one call (position + newName) instead of the list/apply
/// dance of the general `code_action` tool.
pub struct RenameSymbolTool {
    manager: Arc<crate::lsp::LspManager>,
}

impl RenameSymbolTool {
    pub fn new(manager: Arc<crate::lsp::LspManager>) -> Self {
        RenameSymbolTool { manager }
    }
}

#[async_trait]
impl Tool for RenameSymbolTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "rename_symbol".to_string(),
            description: "Rename a symbol (function, variable, type, field, …) across the whole \
                project via the language server. Give the position of ANY occurrence — the server \
                finds every reference (respecting scope, unlike text search-replace) and rewrites \
                them all consistently. Positions are 1-based line & column as shown in editors. \
                Prefer this over edit_file for renames: it won't clobber unrelated text matches or \
                miss shadowed uses. Returns a diff of every file it changed."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "File containing an occurrence of the symbol." },
                    "line": { "type": "integer", "description": "1-based line of an occurrence." },
                    "character": { "type": "integer", "description": "1-based column of an occurrence." },
                    "newName": { "type": "string", "description": "The new name for the symbol." }
                },
                "required": ["filePath", "line", "character", "newName"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let file = match input["filePath"].as_str() {
            Some(f) => resolve_path(&ctx.cwd, f),
            None => return Err(ToolError::invalid_input("filePath is required")),
        };
        let new_name = match input["newName"].as_str() {
            Some(n) if !n.is_empty() => n,
            _ => return Err(ToolError::invalid_input("newName is required")),
        };
        let line = match input["line"].as_i64() {
            Some(l) if l >= 1 => l - 1,
            _ => return Err(ToolError::invalid_input("line is required (1-based)")),
        };
        let character = match input["character"].as_i64() {
            Some(c) if c >= 1 => c - 1,
            _ => return Err(ToolError::invalid_input("character is required (1-based)")),
        };

        let client = match self.manager.client_for(&file) {
            Some(c) => c,
            None => {
                return Err(ToolError::unavailable(format!(
                    "no language server configured for {} (check `bob lsp list`)",
                    file.display()
                )))
            }
        };

        // Sync the anchor file so the server resolves the position correctly.
        if let Ok(text) = std::fs::read_to_string(&file) {
            client.sync_file(&file, &text).await;
        }

        let params = json!({
            "textDocument": { "uri": format!("file://{}", file.to_string_lossy()) },
            "position": { "line": line, "character": character },
            "newName": new_name
        });
        let result = match client.request("textDocument/rename", params).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("rename failed: {}", e))),
        };
        if result.is_null() {
            return Err(ToolError::failed(
                "the language server returned no edit (symbol not renamable here?)",
            ));
        }

        let edits = match collect_workspace_edit(&result) {
            Ok(e) if !e.is_empty() => e,
            Ok(_) => return Err(ToolError::failed("rename produced no changes")),
            Err(e) => return Err(ToolError::failed(e)),
        };

        Ok(apply_and_report(edits, ctx))
    }
}

/// A single text replacement in a file, in LSP 0-based (line, char) coordinates.
struct TextEdit {
    start_line: usize,
    start_char: usize,
    end_line: usize,
    end_char: usize,
    new_text: String,
}

/// Parse a WorkspaceEdit (either `changes: {uri: [TextEdit]}` or
/// `documentChanges: [{textDocument, edits}]`) into edits keyed by file path.
fn collect_workspace_edit(result: &Value) -> Result<BTreeMap<PathBuf, Vec<TextEdit>>, String> {
    let mut out: BTreeMap<PathBuf, Vec<TextEdit>> = BTreeMap::new();

    let mut add = |uri: &str, edits: &Value| {
        let path = uri_to_path(uri);
        if let Some(arr) = edits.as_array() {
            let list = out.entry(path).or_default();
            for e in arr {
                if let Some(te) = parse_text_edit(e) {
                    list.push(te);
                }
            }
        }
    };

    if let Some(changes) = result.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits) in changes {
            add(uri, edits);
        }
    } else if let Some(dc) = result.get("documentChanges").and_then(|d| d.as_array()) {
        for entry in dc {
            // TextDocumentEdit { textDocument: {uri}, edits: [...] }. Ignore
            // create/rename/delete file ops — rename shouldn't produce them.
            if let Some(uri) = entry["textDocument"]["uri"].as_str() {
                add(uri, &entry["edits"]);
            }
        }
    } else {
        return Err("workspace edit had neither `changes` nor `documentChanges`".to_string());
    }
    Ok(out)
}

fn parse_text_edit(e: &Value) -> Option<TextEdit> {
    let range = &e["range"];
    Some(TextEdit {
        start_line: range["start"]["line"].as_u64()? as usize,
        start_char: range["start"]["character"].as_u64()? as usize,
        end_line: range["end"]["line"].as_u64()? as usize,
        end_char: range["end"]["character"].as_u64()? as usize,
        new_text: e["newText"].as_str().unwrap_or("").to_string(),
    })
}

/// Apply the edits to disk and return a multi-file diff report.
fn apply_and_report(edits: BTreeMap<PathBuf, Vec<TextEdit>>, ctx: &ToolContext) -> String {
    let mut report = String::new();
    let mut files_changed = 0;
    let mut total_added = 0;
    let mut total_removed = 0;

    for (path, mut file_edits) in edits {
        let before = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                report.push_str(&format!("skipped {} (read error: {})\n", path.display(), e));
                continue;
            }
        };
        // Apply edits from the END of the file backwards so earlier offsets stay
        // valid as we splice. Sort by (start_line, start_char) descending.
        file_edits.sort_by(|a, b| (b.start_line, b.start_char).cmp(&(a.start_line, a.start_char)));
        let after = match apply_edits(&before, &file_edits) {
            Ok(s) => s,
            Err(e) => {
                report.push_str(&format!("skipped {} ({})\n", path.display(), e));
                continue;
            }
        };
        if after == before {
            continue;
        }
        if let Err(e) = std::fs::write(&path, &after) {
            report.push_str(&format!("failed to write {} ({})\n", path.display(), e));
            continue;
        }
        // Keep the file tracker honest so a later edit_file doesn't flag a
        // stale-read conflict on a file we just rewrote.
        ctx.files.record_write(&path.to_string_lossy());

        let full = diff_lines(&before, &after);
        let (added, removed) = diff_stat(&full);
        total_added += added;
        total_removed += removed;
        files_changed += 1;
        let compact = compact_diff(&full, 2);
        let label = path.to_string_lossy();
        report.push_str(&format!(
            "```diff {}\n{}\n```\n",
            label,
            format_unified(&compact)
        ));
    }

    if files_changed == 0 {
        return format!("no files changed.\n{}", report);
    }
    format!(
        "renamed across {} file(s) (+{} -{})\n{}",
        files_changed, total_added, total_removed, report
    )
}

/// Splice a set of edits into `content`. Edits MUST be pre-sorted last-first so
/// applying one doesn't shift the offsets of the next.
fn apply_edits(content: &str, edits: &[TextEdit]) -> Result<String, String> {
    // Work on a Vec<char>-per-line model via byte offsets computed from lines.
    let line_starts = line_start_offsets(content);
    let mut bytes = content.to_string();
    for e in edits {
        let start = offset_at(&line_starts, content, e.start_line, e.start_char)
            .ok_or("edit position out of range")?;
        let end = offset_at(&line_starts, content, e.end_line, e.end_char)
            .ok_or("edit position out of range")?;
        if start > end || end > bytes.len() {
            return Err("edit range out of range".to_string());
        }
        bytes.replace_range(start..end, &e.new_text);
    }
    Ok(bytes)
}

/// Byte offset of the start of each line.
fn line_start_offsets(s: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    for (i, b) in s.bytes().enumerate() {
        if b == b'\n' {
            offsets.push(i + 1);
        }
    }
    offsets
}

/// Convert an LSP (line, character) — where character is a UTF-16 code-unit
/// offset — to a byte offset in `s`. We approximate UTF-16 with char counting;
/// correct for the BMP (covers code, the practical case) and simpler than full
/// UTF-16 accounting.
fn offset_at(line_starts: &[usize], s: &str, line: usize, character: usize) -> Option<usize> {
    let line_start = *line_starts.get(line)?;
    let line_str = s[line_start..].split('\n').next().unwrap_or("");
    let mut byte = line_start;
    let mut chars = 0;
    for ch in line_str.chars() {
        if chars >= character {
            break;
        }
        byte += ch.len_utf8();
        chars += 1;
    }
    Some(byte)
}

fn uri_to_path(uri: &str) -> PathBuf {
    PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
}

/// The general code-action surface. Two modes:
///   - list (no `apply`): return the actions available at a position/range,
///     plus the kinds the server advertised — so the model sees "what can I do
///     here" without guessing.
///   - apply (`apply` = an action title from the list): run that action and
///     apply its WorkspaceEdit.
pub struct CodeActionTool {
    manager: Arc<crate::lsp::LspManager>,
}

impl CodeActionTool {
    pub fn new(manager: Arc<crate::lsp::LspManager>) -> Self {
        CodeActionTool { manager }
    }
}

#[async_trait]
impl Tool for CodeActionTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "code_action".to_string(),
            description: "List or apply language-server code actions at a position: quick-fixes \
                (fix this error), refactors (extract/inline), and source actions (organize imports, \
                fix all). Call WITHOUT `apply` to list the actions available at the given location \
                (titles + kinds) — this is how you discover what the server can do. Call WITH \
                `apply` set to one of those titles to run it and rewrite the affected files. \
                Positions are 1-based; give the line/column of the code you want to act on (e.g. an \
                error's location). For renames prefer the dedicated rename_symbol tool."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filePath": { "type": "string", "description": "File to act in." },
                    "line": { "type": "integer", "description": "1-based line of the target code." },
                    "character": { "type": "integer", "description": "1-based column of the target code." },
                    "endLine": { "type": "integer", "description": "1-based end line for a range selection (defaults to line)." },
                    "endCharacter": { "type": "integer", "description": "1-based end column for a range selection (defaults to character)." },
                    "apply": { "type": "string", "description": "Title of the action to apply (omit to just list available actions)." }
                },
                "required": ["filePath", "line", "character"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let file = match input["filePath"].as_str() {
            Some(f) => resolve_path(&ctx.cwd, f),
            None => return Err(ToolError::invalid_input("filePath is required")),
        };
        let line = match input["line"].as_i64() {
            Some(l) if l >= 1 => l - 1,
            _ => return Err(ToolError::invalid_input("line is required (1-based)")),
        };
        let character = match input["character"].as_i64() {
            Some(c) if c >= 1 => c - 1,
            _ => return Err(ToolError::invalid_input("character is required (1-based)")),
        };
        let end_line = input["endLine"].as_i64().map(|l| l - 1).unwrap_or(line);
        let end_char = input["endCharacter"]
            .as_i64()
            .map(|c| c - 1)
            .unwrap_or(character);

        let client = match self.manager.client_for(&file) {
            Some(c) => c,
            None => {
                return Err(ToolError::unavailable(format!(
                    "no language server configured for {} (check `bob lsp list`)",
                    file.display()
                )))
            }
        };
        if let Ok(text) = std::fs::read_to_string(&file) {
            client.sync_file(&file, &text).await;
        }

        // Include any diagnostics overlapping the range as context, so the server
        // can offer the matching quick-fixes.
        let diags = client.diagnostics_for(&file);
        let uri = format!("file://{}", file.to_string_lossy());
        let params = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": line, "character": character },
                "end": { "line": end_line, "character": end_char }
            },
            "context": { "diagnostics": diags }
        });
        let result = match client.request("textDocument/codeAction", params).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("codeAction failed: {}", e))),
        };
        let actions = result.as_array().cloned().unwrap_or_default();

        // Apply mode: find the requested action by title and run it.
        if let Some(want) = input["apply"].as_str() {
            let chosen = actions.iter().find(|a| a["title"].as_str() == Some(want));
            let action = match chosen {
                Some(a) => a,
                None => {
                    let titles: Vec<&str> =
                        actions.iter().filter_map(|a| a["title"].as_str()).collect();
                    return Err(ToolError::failed(format!(
                        "no action titled '{}'. Available: {}",
                        want,
                        if titles.is_empty() {
                            "(none)".to_string()
                        } else {
                            titles.join(" | ")
                        }
                    )));
                }
            };
            // A CodeAction may carry its edit inline (`edit`) or need resolving
            // via codeAction/resolve. Try inline first, then resolve.
            let edit = if action.get("edit").is_some() {
                action["edit"].clone()
            } else {
                match client.request("codeAction/resolve", action.clone()).await {
                    Ok(resolved) => resolved["edit"].clone(),
                    Err(e) => {
                        return Err(ToolError::failed(format!(
                            "could not resolve action: {}",
                            e
                        )))
                    }
                }
            };
            if edit.is_null() {
                // Some actions run a command instead of returning an edit; we
                // don't execute arbitrary server commands (side effects, no diff).
                return Ok(format!(
                    "action '{}' has no direct edit (it runs a server command, which bob does not \
                     execute automatically).",
                    want
                ));
            }
            let edits = match collect_workspace_edit(&edit) {
                Ok(e) if !e.is_empty() => e,
                Ok(_) => return Ok(format!("action '{}' produced no changes", want)),
                Err(e) => return Err(ToolError::failed(e)),
            };
            return Ok(apply_and_report(edits, ctx));
        }

        // List mode: show available actions (title + kind) and advertised kinds.
        if actions.is_empty() {
            let kinds = client.code_action_kinds();
            if kinds.is_empty() {
                return Ok("No code actions available here.".to_string());
            }
            return Ok(format!(
                "No actions at this position. This server supports: {}",
                kinds.join(", ")
            ));
        }
        let mut out = String::from("Available code actions (apply one by title):\n");
        for a in &actions {
            let title = a["title"].as_str().unwrap_or("(untitled)");
            let kind = a["kind"].as_str().unwrap_or("");
            if kind.is_empty() {
                out.push_str(&format!("  - {}\n", title));
            } else {
                out.push_str(&format!("  - {}  [{}]\n", title, kind));
            }
        }
        Ok(out)
    }
}
