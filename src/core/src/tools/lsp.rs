//! The `lsp` tool: a single tool exposing language-server intelligence to the
//! model via an `operation` enum (diagnostics, definition, references, hover,
//! symbols). It routes the target file to the right server through the
//! LspManager, syncs the file's current on-disk text first, and returns compact,
//! token-cheap output — for diagnostics, rustc-style with the source line and a
//! caret under the column, which is far more actionable than a bare message.

use crate::core::types::ToolSpec;
use crate::lsp::LspManager;
use crate::tools::builtin::resolve_path;
use crate::tools::registry::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

pub struct LspTool {
    manager: Arc<LspManager>,
}

impl LspTool {
    pub fn new(manager: Arc<LspManager>) -> Self {
        LspTool { manager }
    }
}

#[async_trait]
impl Tool for LspTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp".to_string(),
            description: "Query language servers for code intelligence. Operations:\n\
                - diagnostics: compiler/linter errors & warnings for a file (with source context)\n\
                - definition: where a symbol is defined\n\
                - references: all uses of a symbol\n\
                - hover: type/signature/docs for a symbol\n\
                - implementation: implementations of a trait/interface method\n\
                - document_symbols: outline of a file (functions, types, etc.)\n\
                - workspace_symbols: search symbols across the project by name\n\
                Positions use 1-based line & column as shown in editors. definition/references/\
                hover/implementation need line+character; document_symbols needs only filePath; \
                workspace_symbols needs only query. Prefer this over grep for 'where is X defined/\
                used' and over guessing types."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operation": {
                        "type": "string",
                        "enum": ["diagnostics", "definition", "references", "hover",
                                 "implementation", "document_symbols", "workspace_symbols"],
                        "description": "Which LSP query to run."
                    },
                    "filePath": { "type": "string", "description": "Target file (relative to cwd or absolute)." },
                    "line": { "type": "integer", "description": "1-based line number." },
                    "character": { "type": "integer", "description": "1-based column number." },
                    "query": { "type": "string", "description": "Symbol name for workspace_symbols." },
                    "severity": {
                        "type": "string",
                        "enum": ["error", "warning", "all"],
                        "description": "diagnostics only: minimum severity to include (default error)."
                    }
                },
                "required": ["operation"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let op = input["operation"].as_str().unwrap_or("");

        // workspace_symbols is the only op not tied to a specific file.
        if op == "workspace_symbols" {
            let query = input["query"].as_str().unwrap_or("");
            return self.workspace_symbols(query).await;
        }

        let file = match input["filePath"].as_str() {
            Some(f) => resolve_path(&ctx.cwd, f),
            None => return Err(ToolError::invalid_input("filePath is required")),
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

        // Read current on-disk text and sync it so the server sees the latest.
        let text = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) => {
                return Err(ToolError::failed(format!(
                    "cannot read {}: {}",
                    file.display(),
                    e
                )))
            }
        };
        client.sync_file(&file, &text).await;

        match op {
            "diagnostics" => self.diagnostics(&client, &file, &text, &input).await,
            "definition" => {
                self.locations(&client, &file, &input, "textDocument/definition")
                    .await
            }
            "references" => self.references(&client, &file, &input).await,
            "implementation" => {
                self.locations(&client, &file, &input, "textDocument/implementation")
                    .await
            }
            "hover" => self.hover(&client, &file, &input).await,
            "document_symbols" => self.document_symbols(&client, &file).await,
            other => Err(ToolError::invalid_input(format!(
                "unknown operation '{}'",
                other
            ))),
        }
    }
}

impl LspTool {
    /// Wait briefly for the server to (re)publish diagnostics for the file, then
    /// render them rustc-style: `SEVERITY file:line:col message` + the source
    /// line + a caret under the column. rust-analyzer pushes an empty set first
    /// then the real one, so we poll with a short settle window.
    async fn diagnostics(
        &self,
        client: &crate::lsp::LspClient,
        file: &Path,
        text: &str,
        input: &Value,
    ) -> ToolResult {
        let min_sev = match input["severity"].as_str() {
            Some("all") => 4,
            Some("warning") => 2,
            _ => 1, // error
        };

        // Poll up to ~5s, settling 200ms after the last change, for diagnostics.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut last: Vec<Value> = client.diagnostics_for(file);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let now = client.diagnostics_for(file);
            if !now.is_empty() && now.len() == last.len() {
                break; // settled
            }
            last = now;
            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }

        let diags = client.diagnostics_for(file);
        let lines: Vec<&str> = text.lines().collect();
        let mut out = String::new();
        let mut shown = 0;
        for d in &diags {
            let sev = d["severity"].as_i64().unwrap_or(1);
            if sev > min_sev {
                continue;
            }
            if shown >= 20 {
                out.push_str("… and more (showing first 20)\n");
                break;
            }
            let sev_label = match sev {
                1 => "ERROR",
                2 => "WARN",
                3 => "INFO",
                _ => "HINT",
            };
            let line = d["range"]["start"]["line"].as_i64().unwrap_or(0);
            let col = d["range"]["start"]["character"].as_i64().unwrap_or(0);
            let msg = d["message"].as_str().unwrap_or("");
            out.push_str(&format!(
                "{} {}:{}:{} {}\n",
                sev_label,
                rel(file),
                line + 1,
                col + 1,
                msg.replace('\n', " ")
            ));
            // Source line + caret.
            if let Some(src) = lines.get(line as usize) {
                out.push_str(&format!("  {}\n", src));
                let pad: String = std::iter::repeat_n(' ', col as usize + 2).collect();
                out.push_str(&format!("{}^\n", pad));
            }
            shown += 1;
        }
        if shown == 0 {
            return Ok(format!(
                "No diagnostics for {} (at or above {} severity).",
                rel(file),
                match min_sev {
                    1 => "error",
                    2 => "warning",
                    _ => "hint",
                }
            ));
        }
        Ok(out)
    }

    async fn locations(
        &self,
        client: &crate::lsp::LspClient,
        file: &Path,
        input: &Value,
        method: &str,
    ) -> ToolResult {
        let params = match position_params(file, input) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        let result = match client.request(method, params).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("{}", e))),
        };
        let locs = collect_locations(&result);
        if locs.is_empty() {
            return Ok("No results.".to_string());
        }
        Ok(locs
            .into_iter()
            .map(|(uri, line, col)| format!("{}:{}:{}", uri_to_rel(&uri), line + 1, col + 1))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn references(
        &self,
        client: &crate::lsp::LspClient,
        file: &Path,
        input: &Value,
    ) -> ToolResult {
        let mut params = match position_params(file, input) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        params["context"] = json!({ "includeDeclaration": true });
        let result = match client.request("textDocument/references", params).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("{}", e))),
        };
        let locs = collect_locations(&result);
        if locs.is_empty() {
            return Ok("No references found.".to_string());
        }
        let mut out = format!("{} reference(s):\n", locs.len());
        for (uri, line, col) in locs {
            out.push_str(&format!("{}:{}:{}\n", uri_to_rel(&uri), line + 1, col + 1));
        }
        Ok(out)
    }

    async fn hover(
        &self,
        client: &crate::lsp::LspClient,
        file: &Path,
        input: &Value,
    ) -> ToolResult {
        let params = match position_params(file, input) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        let result = match client.request("textDocument/hover", params).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("{}", e))),
        };
        // hover.contents is MarkupContent { kind, value } or a string / array.
        let contents = &result["contents"];
        if let Some(s) = contents.as_str() {
            return Ok(s.to_string());
        }
        if let Some(v) = contents["value"].as_str() {
            return Ok(v.to_string());
        }
        if contents.is_null() {
            return Ok("No hover information.".to_string());
        }
        Ok(contents.to_string())
    }

    async fn document_symbols(&self, client: &crate::lsp::LspClient, file: &Path) -> ToolResult {
        let params = json!({ "textDocument": { "uri": path_uri(file) } });
        let result = match client.request("textDocument/documentSymbol", params).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("{}", e))),
        };
        let arr = match result.as_array() {
            Some(a) if !a.is_empty() => a,
            _ => return Ok("No symbols.".to_string()),
        };
        let mut out = String::new();
        for s in arr {
            render_symbol(s, 0, &mut out);
        }
        Ok(out)
    }

    async fn workspace_symbols(&self, query: &str) -> ToolResult {
        // Query every ready server; merge results. workspace/symbol isn't file-
        // scoped, so we can't route — ask all servers that have a client.
        let mut out = String::new();
        let mut any = false;
        for (name, _health) in self.manager.statuses() {
            if let Some(client) = self.manager.client_by_name(&name) {
                let params = json!({ "query": query });
                if let Ok(result) = client.request("workspace/symbol", params).await {
                    if let Some(arr) = result.as_array() {
                        for s in arr {
                            any = true;
                            let name = s["name"].as_str().unwrap_or("");
                            let kind = symbol_kind(s["kind"].as_i64().unwrap_or(0));
                            let loc = &s["location"];
                            let uri = loc["uri"].as_str().unwrap_or("");
                            let line = loc["range"]["start"]["line"].as_i64().unwrap_or(0) + 1;
                            out.push_str(&format!(
                                "{}:{}  {} {}\n",
                                uri_to_rel(uri),
                                line,
                                kind,
                                name
                            ));
                        }
                    }
                }
            }
        }
        if !any {
            return Ok("No matching symbols.".to_string());
        }
        Ok(out)
    }
}

// --- helpers ---------------------------------------------------------------

fn position_params(file: &Path, input: &Value) -> Result<Value, ToolError> {
    let line = match input["line"].as_i64() {
        Some(l) if l >= 1 => l - 1,
        _ => return Err(ToolError::invalid_input("line is required (1-based)")),
    };
    let character = match input["character"].as_i64() {
        Some(c) if c >= 1 => c - 1,
        _ => return Err(ToolError::invalid_input("character is required (1-based)")),
    };
    Ok(json!({
        "textDocument": { "uri": path_uri(file) },
        "position": { "line": line, "character": character }
    }))
}

fn path_uri(file: &Path) -> String {
    format!("file://{}", file.to_string_lossy())
}

/// Collect (uri, line, character) from a Location, Location[], or
/// LocationLink[] result.
fn collect_locations(result: &Value) -> Vec<(String, i64, i64)> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<(String, i64, i64)>, v: &Value| {
        // LocationLink uses targetUri/targetRange; Location uses uri/range.
        let uri = v["uri"].as_str().or_else(|| v["targetUri"].as_str());
        let range = if v.get("range").is_some() {
            &v["range"]
        } else {
            &v["targetRange"]
        };
        if let Some(uri) = uri {
            let line = range["start"]["line"].as_i64().unwrap_or(0);
            let col = range["start"]["character"].as_i64().unwrap_or(0);
            out.push((uri.to_string(), line, col));
        }
    };
    match result {
        Value::Array(arr) => {
            for v in arr {
                push(&mut out, v);
            }
        }
        Value::Null => {}
        v => push(&mut out, v),
    }
    out
}

fn render_symbol(s: &Value, depth: usize, out: &mut String) {
    let name = s["name"].as_str().unwrap_or("");
    let kind = symbol_kind(s["kind"].as_i64().unwrap_or(0));
    // DocumentSymbol has `range`; SymbolInformation has `location.range`.
    let line = s["range"]["start"]["line"]
        .as_i64()
        .or_else(|| s["location"]["range"]["start"]["line"].as_i64())
        .unwrap_or(0)
        + 1;
    let indent: String = std::iter::repeat_n("  ", depth).collect();
    out.push_str(&format!("{}{} {}  :{}\n", indent, kind, name, line));
    if let Some(children) = s["children"].as_array() {
        for c in children {
            render_symbol(c, depth + 1, out);
        }
    }
}

/// LSP SymbolKind enum → short label.
fn symbol_kind(k: i64) -> &'static str {
    match k {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "fn",
        13 => "var",
        14 => "const",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "typeparam",
        _ => "symbol",
    }
}

/// Best-effort path relative to cwd for display.
fn rel(file: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| {
            file.strip_prefix(&cwd)
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| file.to_string_lossy().to_string())
}

fn uri_to_rel(uri: &str) -> String {
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    rel(Path::new(path))
}
