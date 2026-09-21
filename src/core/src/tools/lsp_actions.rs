//! Mutating LSP tools authorize each target and validate snapshots before commit.

use crate::core::types::ToolSpec;
use crate::lsp::workspace_edit::{apply_edits, collect_workspace_edit};
use crate::tools::builtin::resolve_path;
use crate::tools::diff::{compact_diff, diff_lines, diff_stat, format_unified};
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

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

        let file = crate::tools::filesystem::workspace_path(&ctx.cwd, &file)?;
        let text = crate::tools::filesystem::read_regular(&file)?;
        client
            .sync_file(&file, &text)
            .await
            .map_err(|e| ToolError::failed(e.to_string()))?;
        let character = client
            .position_encoding()
            .from_column(&text, line as usize, character as usize)
            .ok_or_else(|| ToolError::invalid_input("position out of range"))?;
        let mut snapshots = client.document_snapshots();

        let params = json!({
            "textDocument": { "uri": crate::lsp::path_to_uri(&file) },
            "position": { "line": line, "character": character },
            "newName": new_name
        });
        let result = match client.request("textDocument/rename", params.clone()).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("rename failed: {}", e))),
        };
        if result.is_null() {
            return Err(ToolError::failed(
                "the language server returned no edit (symbol not renamable here?)",
            ));
        }

        let mut edits = match collect_workspace_edit(&result) {
            Ok(e) if !e.is_empty() => e,
            Ok(_) => return Err(ToolError::failed("rename produced no changes")),
            Err(e) => return Err(ToolError::failed(e)),
        };

        if prepare_discovered(&edits, ctx, &client, &mut snapshots).await? {
            let result = client
                .request("textDocument/rename", params)
                .await
                .map_err(|error| ToolError::failed(error.to_string()))?;
            edits = collect_workspace_edit(&result).map_err(ToolError::failed)?;
        }
        apply_and_report(edits, ctx, &client, &snapshots, &self.manager).await
    }
}

async fn action_edit(
    client: &crate::lsp::LspClient,
    actions: &[Value],
    title: &str,
) -> Result<Value, ToolError> {
    let mut matching = actions
        .iter()
        .filter(|action| action["title"].as_str() == Some(title));
    let action = matching
        .next()
        .ok_or_else(|| ToolError::failed(format!("no action titled '{}'", title)))?;
    if matching.next().is_some() {
        return Err(ToolError::failed("ambiguous action title"));
    }
    let action = if action.get("edit").is_some() {
        action.clone()
    } else {
        client
            .request("codeAction/resolve", action.clone())
            .await
            .map_err(|error| ToolError::failed(error.to_string()))?
    };
    if action.get("disabled").is_some() || action.get("command").is_some() {
        return Err(ToolError::failed(
            "disabled actions and server commands are unsupported; no files changed",
        ));
    }
    if action["edit"].is_null() {
        return Err(ToolError::failed(
            "action has no direct edit; no files changed",
        ));
    }
    Ok(action["edit"].clone())
}

async fn authorize_target(ctx: &ToolContext, path: &std::path::Path) -> Result<(), ToolError> {
    if let Some(permissions) = &ctx.permissions {
        let request = crate::core::permissions::PermissionRequest {
            tool: "edit_file".into(),
            input: json!({"path":path}),
            cwd: ctx.cwd.clone(),
            bash: None,
            preview: None,
            agent: ctx.coord.as_ref().map(|coord| coord.name.clone()),
            read_only: false,
        };
        if !permissions.check(&request).await {
            return Err(ToolError::permission_denied(format!(
                "workspace edit denied for {}; no files changed",
                path.display()
            )));
        }
    }
    Ok(())
}

async fn prepare_discovered(
    edits: &BTreeMap<PathBuf, crate::lsp::workspace_edit::DocumentEdits>,
    ctx: &ToolContext,
    client: &crate::lsp::LspClient,
    snapshots: &mut std::collections::HashMap<String, crate::lsp::DocumentSnapshot>,
) -> Result<bool, ToolError> {
    let mut discovered = Vec::new();
    for (requested, edits) in edits {
        let path = crate::tools::filesystem::workspace_path(&ctx.cwd, requested)?;
        authorize_target(ctx, &path).await?;
        let text = crate::tools::filesystem::read_regular(&path)?;
        let uri = crate::lsp::path_to_uri(&path);
        if let Some(snapshot) = snapshots.get(&uri) {
            if snapshot.text != text
                || edits
                    .version
                    .is_some_and(|version| version != snapshot.version)
                || client
                    .document_snapshot(&path)
                    .is_none_or(|current| current.version != snapshot.version)
            {
                return Err(ToolError::failed(format!(
                    "stale server snapshot for {}; no files changed",
                    path.display()
                )));
            }
        } else {
            if edits.version.is_some() {
                return Err(ToolError::failed(format!(
                    "unknown document version for {}; no files changed",
                    path.display()
                )));
            }
            discovered.push((path, text));
        }
    }
    if discovered.is_empty() {
        return Ok(false);
    }
    for (path, text) in discovered {
        let stamp = client
            .sync_file(&path, &text)
            .await
            .map_err(|error| ToolError::failed(error.to_string()))?;
        snapshots.insert(
            crate::lsp::path_to_uri(&path),
            crate::lsp::DocumentSnapshot {
                version: stamp.version,
                text,
            },
        );
    }
    Ok(true)
}

async fn apply_and_report(
    edits: BTreeMap<PathBuf, crate::lsp::workspace_edit::DocumentEdits>,
    ctx: &ToolContext,
    client: &crate::lsp::LspClient,
    snapshots: &std::collections::HashMap<String, crate::lsp::DocumentSnapshot>,
    manager: &crate::lsp::LspManager,
) -> ToolResult {
    use crate::tools::filesystem::{prepare_batch, read_regular, workspace_path, WriteRequest};
    let mut prepared = Vec::new();
    for (requested, edits) in edits {
        let path = workspace_path(&ctx.cwd, &requested)?;
        authorize_target(ctx, &path).await?;
        let uri = crate::lsp::path_to_uri(&path);
        let snapshot = snapshots.get(&uri).ok_or_else(|| {
            ToolError::failed(format!(
                "{} was not snapshotted before the request; no files changed",
                path.display()
            ))
        })?;
        if edits
            .version
            .is_some_and(|version| version != snapshot.version)
            || !client.document_snapshot(&path).is_some_and(|current| {
                current.version == snapshot.version && current.text == snapshot.text
            })
        {
            return Err(ToolError::failed(format!(
                "stale document version for {}; no files changed",
                path.display()
            )));
        }
        let before = read_regular(&path)?;
        if before != snapshot.text {
            return Err(ToolError::failed(format!(
                "{} changed since the server snapshot; synchronize and retry; no files changed",
                path.display()
            )));
        }
        let after = apply_edits(&before, &edits.edits, client.position_encoding())
            .map_err(ToolError::failed)?;
        prepared.push((path, before, after, snapshot.version, requested));
    }
    let requests = prepared
        .iter()
        .filter(|(_, before, after, _, _)| before != after)
        .map(|(path, before, after, _, requested)| WriteRequest {
            path: requested.clone(),
            resolved: path.clone(),
            content: after.clone(),
            expected: Some(before.clone()),
        })
        .collect();
    let batch = prepare_batch(ctx, requests).await?;
    let committed = {
        let _sync = client
            .lock_documents()
            .await
            .map_err(|error| ToolError::failed(error.to_string()))?;
        for (path, before, _, version, _) in &prepared {
            if !client
                .document_snapshot(path)
                .is_some_and(|current| current.version == *version && current.text == *before)
            {
                return Err(ToolError::failed(
                    "server document changed during preflight; no files changed",
                ));
            }
        }
        batch.commit()
    };
    let mut sync_errors = Vec::new();
    for (path, _, _, _, _) in &prepared {
        match read_regular(path) {
            Ok(actual) => {
                ctx.files.record_write(&path.to_string_lossy());
                if let Err(error) = client.sync_file(path, &actual).await {
                    sync_errors.push(format!("{}: {}", path.display(), error));
                }
                if let Some(other) = manager.client_for(path) {
                    if other.name() != client.name() {
                        if let Err(error) = other.sync_file(path, &actual).await {
                            sync_errors.push(format!("{}: {}", path.display(), error));
                        }
                    }
                }
            }
            Err(error) => sync_errors.push(format!("{}: {}", path.display(), error)),
        }
    }
    if let Err(mut error) = committed {
        if !sync_errors.is_empty() {
            error.message.push_str(&format!(
                "; document sync failed: {}",
                sync_errors.join("; ")
            ));
        }
        return Err(error);
    }
    if !sync_errors.is_empty() {
        return Err(ToolError::failed(format!(
            "disk edits committed; rollback not attempted; document sync failed: {}",
            sync_errors.join("; ")
        )));
    }
    let mut report = String::new();
    let mut files_changed = 0;
    let mut total_added = 0;
    let mut total_removed = 0;
    for (path, before, after, _, _) in prepared {
        if before == after {
            continue;
        }
        let full = diff_lines(&before, &after);
        let (added, removed) = diff_stat(&full);
        total_added += added;
        total_removed += removed;
        files_changed += 1;
        report.push_str(&format!(
            "```diff {}\n{}\n```\n",
            path.display(),
            format_unified(&compact_diff(&full, 2))
        ));
    }
    Ok(format!(
        "changed {} file(s) (+{} -{})\n{}",
        files_changed, total_added, total_removed, report
    )
    .into())
}

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
        let end_line = match input.get("endLine") {
            None => line,
            Some(value) => value
                .as_i64()
                .filter(|value| *value >= 1)
                .map(|value| value - 1)
                .ok_or_else(|| ToolError::invalid_input("endLine must be 1-based"))?,
        };
        let end_char = match input.get("endCharacter") {
            None => character,
            Some(value) => value
                .as_i64()
                .filter(|value| *value >= 1)
                .map(|value| value - 1)
                .ok_or_else(|| ToolError::invalid_input("endCharacter must be 1-based"))?,
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
        let file = crate::tools::filesystem::workspace_path(&ctx.cwd, &file)?;
        let text = crate::tools::filesystem::read_regular(&file)?;
        client
            .sync_file(&file, &text)
            .await
            .map_err(|e| ToolError::failed(e.to_string()))?;
        if end_line < 0 || end_char < 0 || (end_line, end_char) < (line, character) {
            return Err(ToolError::invalid_input("invalid or reversed action range"));
        }
        let encoding = client.position_encoding();
        let character = encoding
            .from_column(&text, line as usize, character as usize)
            .ok_or_else(|| ToolError::invalid_input("start position out of range"))?;
        let end_char = encoding
            .from_column(&text, end_line as usize, end_char as usize)
            .ok_or_else(|| ToolError::invalid_input("end position out of range"))?;
        let mut snapshots = client.document_snapshots();

        let diags = client.diagnostics_for(&file);
        let uri = crate::lsp::path_to_uri(&file);
        let params = json!({
            "textDocument": { "uri": uri },
            "range": {
                "start": { "line": line, "character": character },
                "end": { "line": end_line, "character": end_char }
            },
            "context": { "diagnostics": diags }
        });
        let result = match client
            .request("textDocument/codeAction", params.clone())
            .await
        {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("codeAction failed: {}", e))),
        };
        let actions = result.as_array().cloned().unwrap_or_default();

        if let Some(want) = input["apply"].as_str() {
            let edit = action_edit(&client, &actions, want).await?;
            let mut edits = collect_workspace_edit(&edit).map_err(ToolError::failed)?;
            if prepare_discovered(&edits, ctx, &client, &mut snapshots).await? {
                let result = client
                    .request("textDocument/codeAction", params)
                    .await
                    .map_err(|error| ToolError::failed(error.to_string()))?;
                let actions = result
                    .as_array()
                    .ok_or_else(|| ToolError::failed("invalid code action response"))?;
                let edit = action_edit(&client, actions, want).await?;
                edits = collect_workspace_edit(&edit).map_err(ToolError::failed)?;
            }
            return apply_and_report(edits, ctx, &client, &snapshots, &self.manager).await;
        }

        if actions.is_empty() {
            let kinds = client.code_action_kinds();
            if kinds.is_empty() {
                return Ok("No code actions available here.".into());
            }
            return Ok(format!(
                "No actions at this position. This server supports: {}",
                kinds.join(", ")
            )
            .into());
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
        Ok(out.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files::FileFixture;
    use crate::tools::registry::ToolErrorKind;

    #[tokio::test]
    async fn rename_validates_required_fields_before_server_lookup() {
        let fixture = FileFixture::new();
        let manager = crate::lsp::LspManager::start(&[], fixture.dir.path());
        let tool = RenameSymbolTool::new(manager);
        let valid = json!({"filePath":"file.rs", "newName":"renamed", "line":1, "character":1});
        for (field, invalid, message) in [
            ("filePath", Value::Null, "filePath is required"),
            ("filePath", json!(12), "filePath is required"),
            ("newName", json!(""), "newName is required"),
            ("newName", Value::Null, "newName is required"),
            ("line", json!(0), "line is required (1-based)"),
            ("line", json!(-1), "line is required (1-based)"),
            ("line", json!(1.5), "line is required (1-based)"),
            ("character", json!(0), "character is required (1-based)"),
            ("character", json!("1"), "character is required (1-based)"),
            ("character", Value::Null, "character is required (1-based)"),
        ] {
            let mut input = valid.clone();
            input[field] = invalid;
            let error = tool.execute(input, &fixture.ctx).await.unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::InvalidInput, "{field}");
            assert_eq!(error.message, message);
        }
        let error = tool.execute(valid, &fixture.ctx).await.unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::Unavailable);
        assert!(error.message.contains("no language server configured"));
    }

    #[tokio::test]
    async fn code_action_validates_both_range_endpoints_before_server_lookup() {
        let fixture = FileFixture::new();
        let manager = crate::lsp::LspManager::start(&[], fixture.dir.path());
        let tool = CodeActionTool::new(manager);
        let valid = json!({"filePath":"file.rs", "line":1, "character":1});
        let mut missing_file = valid.clone();
        missing_file.as_object_mut().unwrap().remove("filePath");
        assert_eq!(
            tool.execute(missing_file, &fixture.ctx)
                .await
                .unwrap_err()
                .message,
            "filePath is required"
        );
        for (field, message) in [
            ("line", "line is required (1-based)"),
            ("character", "character is required (1-based)"),
            ("endLine", "endLine must be 1-based"),
            ("endCharacter", "endCharacter must be 1-based"),
        ] {
            for invalid in [Value::Null, json!(0), json!(-1), json!(1.5), json!("1")] {
                let mut input = valid.clone();
                input[field] = invalid;
                let error = tool.execute(input, &fixture.ctx).await.unwrap_err();
                assert_eq!(error.kind, ToolErrorKind::InvalidInput, "{field}");
                assert_eq!(error.message, message);
            }
        }
        for input in [
            valid,
            json!({"filePath":"file.rs", "line":1, "character":1, "endLine":2, "endCharacter":3}),
        ] {
            let error = tool.execute(input, &fixture.ctx).await.unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::Unavailable);
            assert!(error.message.contains("no language server configured"));
        }
    }
}
