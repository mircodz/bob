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
    fn is_read_only(&self) -> bool {
        true
    }
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lsp".to_string(),
            description: "Query language servers for code intelligence. Diagnostics are \
                requested explicitly, not appended automatically to edits. Operations:\n\
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

        let file = crate::tools::filesystem::workspace_path(&ctx.cwd, &file)?;
        let text = crate::tools::filesystem::read_regular(&file)?;
        let stamp = client
            .sync_file(&file, &text)
            .await
            .map_err(|error| ToolError::failed(error.to_string()))?;

        match op {
            "diagnostics" => self.diagnostics(&client, &file, &text, &input, stamp).await,
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
    /// Only a new publication for the synchronized document can report clean.
    async fn diagnostics(
        &self,
        client: &crate::lsp::LspClient,
        file: &Path,
        text: &str,
        input: &Value,
        stamp: crate::lsp::SyncStamp,
    ) -> ToolResult {
        let min_sev = match input["severity"].as_str() {
            Some("all") => 4,
            Some("warning") => 2,
            _ => 1, // error
        };

        let publication = client
            .fresh_diagnostics(file, stamp, std::time::Duration::from_secs(5))
            .await
            .map_err(|error| ToolError::failed(error.to_string()))?;
        let lines: Vec<&str> = text.lines().collect();
        let mut out = if publication.version_confirmed {
            String::new()
        } else {
            "Document version unavailable; freshness unconfirmed.\n".to_string()
        };
        let mut shown = 0;
        for d in &publication.diagnostics {
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
            let units = d["range"]["start"]["character"].as_u64().unwrap_or(0) as usize;
            let col = client
                .position_encoding()
                .to_column(text, line as usize, units)
                .ok_or_else(|| ToolError::failed("server diagnostic has an invalid position"))?;
            let msg = d["message"].as_str().unwrap_or("");
            out.push_str(&format!(
                "{} {}:{}:{} {}\n",
                sev_label,
                rel(file),
                line + 1,
                col + 1,
                msg.replace('\n', " ")
            ));
            if let Some(src) = lines.get(line as usize) {
                out.push_str(&format!("  {}\n", src));
                let pad: String = std::iter::repeat_n(' ', col + 2).collect();
                out.push_str(&format!("{}^\n", pad));
            }
            shown += 1;
        }
        if shown == 0 {
            if !publication.version_confirmed {
                out.push_str("Server published no matching diagnostics; this is not a verified clean result.");
                return Ok(out.into());
            }
            return Ok(format!(
                "No diagnostics for {} (at or above {} severity).",
                rel(file),
                match min_sev {
                    1 => "error",
                    2 => "warning",
                    _ => "hint",
                }
            )
            .into());
        }
        Ok(out.into())
    }

    async fn locations(
        &self,
        client: &crate::lsp::LspClient,
        file: &Path,
        input: &Value,
        method: &str,
    ) -> ToolResult {
        let params = match position_params(client, file, input) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        let result = match client.request(method, params).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("{}", e))),
        };
        let locs = collect_locations(&result);
        if locs.is_empty() {
            return Ok("No results.".into());
        }
        Ok(locs
            .into_iter()
            .map(|(uri, line, col)| render_location(client, &uri, line, col))
            .collect::<Vec<_>>()
            .join("\n")
            .into())
    }

    async fn references(
        &self,
        client: &crate::lsp::LspClient,
        file: &Path,
        input: &Value,
    ) -> ToolResult {
        let mut params = match position_params(client, file, input) {
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
            return Ok("No references found.".into());
        }
        let mut out = format!("{} reference(s):\n", locs.len());
        for (uri, line, col) in locs {
            out.push_str(&render_location(client, &uri, line, col));
            out.push('\n');
        }
        Ok(out.into())
    }

    async fn hover(
        &self,
        client: &crate::lsp::LspClient,
        file: &Path,
        input: &Value,
    ) -> ToolResult {
        let params = match position_params(client, file, input) {
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
            return Ok(s.into());
        }
        if let Some(v) = contents["value"].as_str() {
            return Ok(v.into());
        }
        if contents.is_null() {
            return Ok("No hover information.".into());
        }
        Ok(contents.to_string().into())
    }

    async fn document_symbols(&self, client: &crate::lsp::LspClient, file: &Path) -> ToolResult {
        let params = json!({ "textDocument": { "uri": path_uri(file) } });
        let result = match client.request("textDocument/documentSymbol", params).await {
            Ok(r) => r,
            Err(e) => return Err(ToolError::failed(format!("{}", e))),
        };
        let arr = match result.as_array() {
            Some(a) if !a.is_empty() => a,
            _ => return Ok("No symbols.".into()),
        };
        let mut out = String::new();
        for s in arr {
            render_symbol(s, 0, &mut out);
        }
        Ok(out.into())
    }

    async fn workspace_symbols(&self, query: &str) -> ToolResult {
        // No file routing for workspace/symbol; query all ready servers.
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
            return Ok("No matching symbols.".into());
        }
        Ok(out.into())
    }
}

fn position_params(
    client: &crate::lsp::LspClient,
    file: &Path,
    input: &Value,
) -> Result<Value, ToolError> {
    let line = match input["line"].as_i64() {
        Some(l) if l >= 1 => l - 1,
        _ => return Err(ToolError::invalid_input("line is required (1-based)")),
    };
    let character = match input["character"].as_i64() {
        Some(c) if c >= 1 => c - 1,
        _ => return Err(ToolError::invalid_input("character is required (1-based)")),
    };
    let snapshot = client
        .document_snapshot(file)
        .ok_or_else(|| ToolError::failed("document is not synchronized"))?;
    let character = client
        .position_encoding()
        .from_column(&snapshot.text, line as usize, character as usize)
        .ok_or_else(|| ToolError::invalid_input("position is out of range"))?;
    Ok(json!({
        "textDocument": { "uri": path_uri(file) },
        "position": { "line": line, "character": character }
    }))
}

fn path_uri(file: &Path) -> String {
    crate::lsp::path_to_uri(file)
}

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

fn render_location(client: &crate::lsp::LspClient, uri: &str, line: i64, units: i64) -> String {
    let column = reqwest::Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .and_then(|path| client.document_snapshot(&path))
        .and_then(|doc| {
            client
                .position_encoding()
                .to_column(&doc.text, line as usize, units as usize)
        });
    match column {
        Some(column) => format!("{}:{}:{}", uri_to_rel(uri), line + 1, column + 1),
        None => format!(
            "{}:{} (server {:?} offset {})",
            uri_to_rel(uri),
            line + 1,
            client.position_encoding(),
            units
        ),
    }
}

fn uri_to_rel(uri: &str) -> String {
    reqwest::Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .map(|path| rel(&path))
        .unwrap_or_else(|| uri.to_string())
}

/// Enqueues best-effort synchronization without delaying committed edit results.
pub fn sync_after_edit(lsp: &Option<Arc<LspManager>>, file: &Path) {
    if let Some(client) = lsp.as_ref().and_then(|manager| manager.client_for(file)) {
        client.defer_sync_file(file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files::FileFixture;
    use crate::tools::registry::{ToolErrorKind, ToolRegistry};

    #[test]
    fn location_results_accept_single_array_and_link_forms_in_order() {
        let location = json!({
            "uri":"file:///project/a.rs",
            "range":{"start":{"line":3, "character":7}, "end":{"line":3, "character":9}}
        });
        let link = json!({
            "targetUri":"file:///project/b.rs",
            "targetRange":{"start":{"line":5, "character":2}, "end":{"line":5, "character":8}},
            "targetSelectionRange":{"start":{"line":5, "character":2}, "end":{"line":5, "character":8}}
        });
        let first = ("file:///project/a.rs".to_string(), 3, 7);
        let second = ("file:///project/b.rs".to_string(), 5, 2);
        assert_eq!(collect_locations(&location), vec![first.clone()]);
        assert_eq!(
            collect_locations(&json!([location, link])),
            vec![first, second]
        );
        for empty in [
            Value::Null,
            json!([]),
            json!({"range":{"start":{"line":1}}}),
        ] {
            assert!(collect_locations(&empty).is_empty());
        }
    }

    #[test]
    fn symbol_rendering_preserves_hierarchy_and_converts_zero_based_lines() {
        let symbol = json!({
            "name":"Container", "kind":23, "range":{"start":{"line":4}},
            "children":[
                {"name":"run", "kind":6, "range":{"start":{"line":6}}, "children":[
                    {"name":"local", "kind":13, "range":{"start":{"line":7}}}
                ]},
                {"name":"FLAG", "kind":14, "location":{"range":{"start":{"line":10}}}}
            ]
        });
        let mut output = String::new();
        render_symbol(&symbol, 0, &mut output);
        assert_eq!(
            output,
            "struct Container  :5\n  method run  :7\n    var local  :8\n  const FLAG  :11\n"
        );
        let mut output = String::new();
        render_symbol(
            &json!({"name":"future", "kind":999, "range":{"start":{"line":0}}}),
            0,
            &mut output,
        );
        assert_eq!(output, "symbol future  :1\n");
    }

    #[test]
    fn file_uri_display_decodes_spaces_unicode_and_reserved_characters() {
        let cwd = std::env::current_dir().unwrap();
        let relative = Path::new("src").join("space é#%.rs");
        let path = cwd.join(&relative);
        let uri = path_uri(&path);
        assert!(uri.contains("%20"));
        assert!(uri.contains("%23"));
        assert!(uri.contains("%25"));
        assert_eq!(uri_to_rel(&uri), relative.to_string_lossy());
        assert_eq!(rel(&relative), relative.to_string_lossy());
        for non_file in ["untitled:buffer", "https://example.invalid/a", "not a URI"] {
            assert_eq!(uri_to_rel(non_file), non_file);
        }
    }

    #[tokio::test]
    async fn invalid_inputs_are_distinct_from_missing_server_configuration() {
        let fixture = FileFixture::new();
        let manager = LspManager::start(&[], fixture.dir.path());
        let tool = Arc::new(LspTool::new(manager));
        let mut registry = ToolRegistry::new(None);
        registry.add(tool.clone());
        for input in [
            json!({}),
            json!({"operation":"unknown"}),
            json!({"operation":"diagnostics", "severity":"fatal"}),
            json!({"operation":"hover", "filePath":12}),
            json!({"operation":"hover", "line":1.5}),
        ] {
            assert_eq!(
                registry
                    .execute("lsp", input, &fixture.ctx)
                    .await
                    .unwrap_err()
                    .kind,
                ToolErrorKind::InvalidInput
            );
        }
        let error = tool
            .execute(json!({"operation":"hover"}), &fixture.ctx)
            .await
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
        assert_eq!(error.message, "filePath is required");
        let error = tool
            .execute(
                json!({"operation":"hover", "filePath":"missing.rs", "line":1, "character":1}),
                &fixture.ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::Unavailable);
        assert!(error.message.contains("no language server configured"));
    }
}
