use crate::tools::PreparedWrites;
use crate::workspace::{read_snapshot, workspace_file};
use anyhow::{bail, ensure, Context, Result};
use bob_core::core::config::LspServerConfig;
use bob_core::lsp::workspace_edit::{
    apply_edits, collect_workspace_edit, local_file_path, DocumentEdits,
};
use bob_core::lsp::{
    path_to_uri, DiagnosticPublication, DocumentSnapshot, Health, LspClient, LspManager,
    PositionEncoding,
};
use bob_kernel::{EffectClass, ToolDescriptor, ToolOutcome};
use bob_types::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

pub struct LspService {
    manager: Arc<LspManager>,
    workspace: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Prepared {
    Query {
        name: String,
        input: Value,
    },
    Writes {
        name: String,
        input: Value,
        plan: PreparedWrites,
        paths: Vec<PathBuf>,
    },
}

impl LspService {
    pub fn start(configs: &[LspServerConfig], workspace: &Path) -> Self {
        Self {
            manager: LspManager::start(configs, workspace),
            workspace: workspace.to_path_buf(),
        }
    }

    pub fn statuses(&self) -> Vec<(String, Health)> {
        self.manager.statuses()
    }

    pub fn notify_changed(&self, path: &Path) {
        if path.starts_with(&self.workspace) {
            if let Some(client) = self.manager.client_for(path) {
                client.defer_sync_file(path);
            }
        }
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        let position =
            json!({"type":"integer", "minimum":1, "description":"1-based editor position."});
        let file = json!({"type":"string", "description":"Target file (relative to workspace or absolute)."});
        [
            ("lsp", EffectClass::Read,
             "Query configured language servers: diagnostics, definition, references, hover, implementation, document_symbols, workspace_symbols. Diagnostics explicitly wait for a fresh publication, never silently report cached results as clean. Positions use 1-based Unicode scalar columns. Prefer this over grep for symbol definitions and references.",
             json!({"operation":{"type":"string","enum":["diagnostics","definition","references","hover","implementation","document_symbols","workspace_symbols"]},"filePath":file,"line":position,"character":position,"query":{"type":"string"},"severity":{"type":"string","enum":["error","warning","all"]}}), vec!["operation"]),
            ("rename_symbol", EffectClass::Write,
             "Rename a symbol across the workspace via a configured language server. Give any occurrence using 1-based line and column, plus newName. Prepares exact validated file changes for approval; rejects stale snapshots, unsupported resource operations, and external targets. Returns changed-file diffs.",
             json!({"filePath":file,"line":position,"character":position,"newName":{"type":"string","minLength":1}}), vec!["filePath","line","character","newName"]),
            ("code_action", EffectClass::Write,
             "List or apply configured language-server code actions at a 1-based position or range. Omit apply to list titles and kinds; set apply to an exact unique title to prepare validated file edits. End positions default to the start. Server commands and disabled actions are rejected; only direct workspace edits can be applied.",
             json!({"filePath":file,"line":position,"character":position,"endLine":position,"endCharacter":position,"apply":{"type":"string"}}), vec!["filePath","line","character"]),
        ].into_iter().map(|(name,effect,description,properties,required)| ToolDescriptor {
            spec: ToolSpec { name:name.into(), description:description.into(), input_schema:json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}) }, effect,
        }).collect()
    }

    #[cfg(test)]
    pub async fn prepare(&self, name: &str, input: &Value) -> Result<Value> {
        let (_sender, cancel) = watch::channel(false);
        self.prepare_with_cancel(name, input, cancel).await
    }

    pub async fn prepare_with_cancel(
        &self,
        name: &str,
        input: &Value,
        cancel: watch::Receiver<bool>,
    ) -> Result<Value> {
        let (sender, signal) = watch::channel(false);
        let work = self.prepare_inner(name, input, &signal);
        tokio::pin!(work);
        tokio::select! {
            biased;
            _ = super::cancelled(cancel) => {
                let _ = sender.send(true);
                let _ = work.await;
                bail!("LSP preparation cancelled")
            }
            _ = tokio::time::sleep(super::REQUEST_TIMEOUT) => {
                let _ = sender.send(true);
                let _ = work.await;
                bail!("LSP preparation timed out")
            }
            result = &mut work => result,
        }
    }

    async fn prepare_inner(
        &self,
        name: &str,
        input: &Value,
        cancel: &watch::Receiver<bool>,
    ) -> Result<Value> {
        validate_input(name, input)?;
        let prepared =
            if name == "rename_symbol" || (name == "code_action" && input.get("apply").is_some()) {
                self.prepare_writes(name, input, cancel).await?
            } else {
                Prepared::Query {
                    name: name.into(),
                    input: input.clone(),
                }
            };
        Ok(serde_json::to_value(prepared)?)
    }

    pub fn validate_prepared(name: &str, input: &Value, spec: &Value) -> Result<()> {
        validate_input(name, input)?;
        let (prepared_name, prepared_input, writes) =
            match serde_json::from_value::<Prepared>(spec.clone())? {
                Prepared::Query { name, input } => (name, input, false),
                Prepared::Writes {
                    name,
                    input,
                    plan,
                    paths,
                } => {
                    plan.validate_structure()?;
                    ensure!(
                        paths == plan.targets(),
                        "prepared LSP notification targets changed"
                    );
                    (name, input, true)
                }
            };
        ensure!(
            prepared_name == name && prepared_input == *input,
            "prepared LSP invocation changed"
        );
        ensure!(
            writes
                == (name == "rename_symbol"
                    || (name == "code_action" && input.get("apply").is_some())),
            "prepared LSP effect changed"
        );
        Ok(())
    }

    pub fn prepared_targets(
        name: &str,
        input: &Value,
        spec: &Value,
        workspace: &Path,
    ) -> Result<Vec<PathBuf>> {
        Self::validate_prepared(name, input, spec)?;
        match serde_json::from_value::<Prepared>(spec.clone())? {
            Prepared::Query { .. } => Ok(Vec::new()),
            Prepared::Writes { plan, .. } => {
                plan.validate_workspace_binding(workspace)?;
                Ok(plan.targets())
            }
        }
    }

    pub fn prepared_preview(spec: &Value) -> Result<String> {
        match serde_json::from_value::<Prepared>(spec.clone())? {
            Prepared::Query { name, input } => {
                Ok(format!("{name}\n{}", serde_json::to_string_pretty(&input)?))
            }
            Prepared::Writes { name, plan, .. } => Ok(format!("{name}\n{}", plan.preview())),
        }
    }

    pub async fn execute(
        &self,
        name: &str,
        spec: &Value,
        cancel: &watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        ensure!(
            !*cancel.borrow() && cancel.has_changed().is_ok(),
            "LSP operation cancelled"
        );
        match serde_json::from_value::<Prepared>(spec.clone())? {
            Prepared::Query {
                name: prepared_name,
                input,
            } => {
                ensure!(name == prepared_name, "prepared LSP tool name changed");
                validate_input(name, &input)?;
                ensure!(
                    name == "lsp" || (name == "code_action" && input.get("apply").is_none()),
                    "mutation requires an exact validated write plan"
                );
                let result = if name == "lsp" {
                    self.query(&input, cancel).await
                } else {
                    self.list_actions(&input, cancel).await
                };
                settled_query_outcome(result, cancel)
            }
            Prepared::Writes {
                name: prepared_name,
                input,
                plan,
                paths,
            } => {
                Self::validate_prepared(name, &input, spec)?;
                ensure!(
                    name == prepared_name && matches!(name, "rename_symbol" | "code_action"),
                    "prepared LSP tool name changed"
                );
                plan.validate_workspace_binding(&self.workspace)?;
                let result = blocking(cancel, move |cancel| {
                    let mut result = plan.execute(&cancel);
                    if let Ok(outcome) = &mut result {
                        if !outcome.is_error {
                            outcome.text.push_str(&format!("\n{}", plan.preview()));
                        }
                    }
                    result
                })
                .await;
                for path in paths {
                    self.notify_changed(&path);
                }
                result
            }
        }
    }

    async fn synchronized(
        &self,
        input: &Value,
        cancel: &watch::Receiver<bool>,
    ) -> Result<(LspClient, PathBuf, String, bob_core::lsp::SyncStamp)> {
        let workspace = self.workspace.clone();
        let requested = required_string(input, "filePath")?.to_owned();
        let (path, text) = blocking(cancel, move |_| {
            let path = workspace_file(&workspace, &requested)?;
            let text = read_snapshot(&path)?.0;
            Ok((path, text))
        })
        .await?;
        let client = self.manager.client_for(&path).with_context(|| {
            let statuses = self.manager.statuses();
            let detail = if statuses.is_empty() {
                "no language servers are configured".to_owned()
            } else {
                statuses
                    .iter()
                    .map(|(name, health)| format!("{name}: {health:?}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            format!(
                "no initialized language server configured for {}; server status: {detail}",
                path.display()
            )
        })?;
        let stamp = cancellable(cancel, client.sync_file(&path, &text)).await?;
        Ok((client, path, text, stamp))
    }

    async fn query(&self, input: &Value, cancel: &watch::Receiver<bool>) -> Result<String> {
        let operation = required_string(input, "operation")?;
        if operation == "workspace_symbols" {
            let statuses = self.manager.statuses();
            ensure!(!statuses.is_empty(), "no language servers configured");
            let results =
                futures::future::join_all(statuses.into_iter().map(|(name, health)| async move {
                    let result = async {
                        let client = self.manager.client_by_name(&name).with_context(|| {
                            format!("language server is unavailable: {health:?}")
                        })?;
                        cancellable(
                            cancel,
                            client.request(
                                "workspace/symbol",
                                json!({"query":input["query"].as_str().unwrap_or("")}),
                            ),
                        )
                        .await
                    }
                    .await;
                    (name, result)
                }))
                .await;
            return self.render_workspace_symbols(results);
        }
        let (client, path, text, stamp) = self.synchronized(input, cancel).await?;
        match operation {
            "diagnostics" => {
                let publication = cancellable(
                    cancel,
                    client.fresh_diagnostics(&path, stamp, Duration::from_secs(5)),
                )
                .await?;
                render_diagnostics(
                    &self.display_path(&path),
                    &text,
                    client.position_encoding(),
                    &publication,
                    input["severity"].as_str().unwrap_or("error"),
                )
            }
            "document_symbols" => {
                let result = cancellable(
                    cancel,
                    client.request(
                        "textDocument/documentSymbol",
                        json!({"textDocument":{"uri":path_to_uri(&path)}}),
                    ),
                )
                .await?;
                let mut out = String::new();
                for symbol in array_or_null(&result)? {
                    render_symbol(symbol, 0, &mut out);
                }
                Ok(if out.is_empty() {
                    "No symbols.".into()
                } else {
                    out
                })
            }
            "hover" | "definition" | "references" | "implementation" => {
                let mut params = position_params(&path, &text, client.position_encoding(), input)?;
                if operation == "references" {
                    params["context"] = json!({"includeDeclaration":true});
                }
                let result = cancellable(
                    cancel,
                    client.request(&format!("textDocument/{operation}"), params),
                )
                .await?;
                if operation == "hover" {
                    return Ok(render_hover(&result["contents"]));
                }
                let locations = collect_locations(&result)?;
                if locations.is_empty() {
                    return Ok(if operation == "references" {
                        "No references found."
                    } else {
                        "No results."
                    }
                    .into());
                }
                let mut out = if operation == "references" {
                    format!("{} reference(s):\n", locations.len())
                } else {
                    String::new()
                };
                for (uri, line, units) in locations {
                    let column = local_file_path(&uri)
                        .ok()
                        .and_then(|path| client.document_snapshot(&path))
                        .and_then(|snapshot| {
                            client
                                .position_encoding()
                                .to_column(&snapshot.text, line, units)
                        });
                    let location = self.display_uri(&uri);
                    match column {
                        Some(column) => {
                            out.push_str(&format!("{location}:{}:{}\n", line + 1, column + 1))
                        }
                        None => out.push_str(&format!(
                            "{location}:{} (server {:?} offset {units})\n",
                            line + 1,
                            client.position_encoding()
                        )),
                    }
                }
                Ok(out)
            }
            _ => bail!("unknown LSP operation {operation}"),
        }
    }

    async fn list_actions(&self, input: &Value, cancel: &watch::Receiver<bool>) -> Result<String> {
        let (client, path, text, _) = self.synchronized(input, cancel).await?;
        let params = action_params(
            &path,
            &text,
            client.position_encoding(),
            input,
            client.diagnostics_for(&path),
        )?;
        let result = cancellable(cancel, client.request("textDocument/codeAction", params)).await?;
        let actions = array_or_null(&result)?;
        if actions.is_empty() {
            let kinds = client.code_action_kinds();
            return Ok(if kinds.is_empty() {
                "No code actions available here.".into()
            } else {
                format!(
                    "No actions at this position. This server supports: {}",
                    kinds.join(", ")
                )
            });
        }
        let mut out = String::from("Available code actions (apply one by title):\n");
        for action in actions {
            out.push_str(&format!(
                "  - {}  [{}]{}\n",
                action["title"].as_str().unwrap_or("(untitled)"),
                action["kind"].as_str().unwrap_or(""),
                if action.get("disabled").is_some() {
                    " (disabled)"
                } else {
                    ""
                }
            ));
        }
        Ok(out)
    }

    async fn prepare_writes(
        &self,
        name: &str,
        input: &Value,
        cancel: &watch::Receiver<bool>,
    ) -> Result<Prepared> {
        let (client, path, text, stamp) = self.synchronized(input, cancel).await?;
        let params = if name == "rename_symbol" {
            let mut params = position_params(&path, &text, client.position_encoding(), input)?;
            params["newName"] = input["newName"].clone();
            params
        } else {
            action_params(
                &path,
                &text,
                client.position_encoding(),
                input,
                client.diagnostics_for(&path),
            )?
        };
        let snapshot_client = client.clone();
        let mut snapshots = blocking(cancel, move |_| {
            snapshot_client
                .document_snapshots_bounded(PreparedWrites::MAX_FILES, PreparedWrites::MAX_BYTES)
        })
        .await?;
        let mut edits = cancellable(cancel, request_edits(&client, name, input, &params)).await?;
        PreparedWrites::check_budget(edits.len(), 0)?;
        let workspace = self.workspace.clone();
        let known = snapshots
            .keys()
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        let (returned_edits, discovered) = blocking(cancel, move |cancel| {
            let mut discovered = Vec::new();
            let mut bytes = 0usize;
            for (requested, edit) in &edits {
                check_cancel(&cancel)?;
                let path = workspace_file(
                    &workspace,
                    requested.to_str().context("non-Unicode edit path")?,
                )?;
                if !known.contains(&path_to_uri(&path)) {
                    ensure!(
                        edit.version.is_none(),
                        "unknown document version for {}; no files changed",
                        path.display()
                    );
                    let text = read_snapshot(&path)?.0;
                    bytes = bytes.saturating_add(text.len());
                    PreparedWrites::check_budget(edits.len(), bytes)?;
                    discovered.push((path, text));
                }
            }
            Ok((edits, discovered))
        })
        .await?;
        edits = returned_edits;
        if !discovered.is_empty() {
            for (path, text) in discovered {
                let stamp = cancellable(cancel, client.sync_file(&path, &text)).await?;
                snapshots.insert(
                    path_to_uri(&path),
                    DocumentSnapshot {
                        version: stamp.version,
                        text,
                    },
                );
            }
            edits = cancellable(cancel, request_edits(&client, name, input, &params)).await?;
        }
        ensure!(
            client
                .document_snapshot(&path)
                .is_some_and(|doc| doc.version == stamp.version && doc.text == text),
            "request document changed during LSP preparation; no files changed"
        );
        let snapshot_client = client.clone();
        let current = blocking(cancel, move |_| {
            snapshot_client
                .document_snapshots_bounded(PreparedWrites::MAX_FILES, PreparedWrites::MAX_BYTES)
        })
        .await?;
        let encoding = client.position_encoding();
        let workspace = self.workspace.clone();
        let plan = blocking(cancel, move |cancel| {
            let changes = snapshot_changes_cancellable(
                &edits,
                &snapshots,
                &current,
                encoding,
                Some(&cancel),
            )?;
            ensure!(!changes.is_empty(), "language server produced no changes");
            PreparedWrites::prepare_cancellable(&workspace, changes, Some(&cancel))
        })
        .await?;
        let paths = plan.targets();
        Ok(Prepared::Writes {
            name: name.into(),
            input: input.clone(),
            plan,
            paths,
        })
    }

    fn render_workspace_symbols(&self, results: Vec<(String, Result<Value>)>) -> Result<String> {
        let mut out = String::new();
        let mut failures = Vec::new();
        let mut healthy = 0;
        for (name, result) in results {
            let result = result.and_then(|value| {
                array_or_null(&value)?;
                Ok(value)
            });
            match result {
                Err(error) => failures.push(format!("{name}: {error:#}")),
                Ok(result) => {
                    healthy += 1;
                    for symbol in array_or_null(&result)? {
                        let location = &symbol["location"];
                        out.push_str(&format!(
                            "{}:{}  {} {}\n",
                            self.display_uri(location["uri"].as_str().unwrap_or("")),
                            location["range"]["start"]["line"]
                                .as_u64()
                                .unwrap_or(0)
                                .saturating_add(1),
                            symbol_kind(symbol["kind"].as_u64().unwrap_or(0)),
                            symbol["name"].as_str().unwrap_or("")
                        ));
                    }
                }
            }
        }
        ensure!(
            healthy > 0,
            "workspace symbols unavailable from all language servers:\n{}",
            failures.join("\n")
        );
        if out.is_empty() {
            out.push_str("No matching symbols.\n");
        }
        if !failures.is_empty() {
            out.push_str(&format!(
                "Language server failures (partial results):\n{}",
                failures.join("\n")
            ));
        }
        Ok(out)
    }

    fn display_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    fn display_uri(&self, uri: &str) -> String {
        local_file_path(uri)
            .map(|path| self.display_path(&path))
            .unwrap_or_else(|_| uri.into())
    }
}

fn settled_query_outcome(
    result: Result<String>,
    cancel: &watch::Receiver<bool>,
) -> Result<ToolOutcome> {
    check_cancel(cancel)?;
    let (text, is_error) = match result {
        Ok(text) => (text, false),
        Err(error) if error.is::<tokio::task::JoinError>() => return Err(error),
        Err(error) => (format!("{error:#}"), true),
    };
    Ok(ToolOutcome {
        text,
        is_error,
        interrupted: false,
        presentation: None,
    })
}

fn check_cancel(cancel: &watch::Receiver<bool>) -> Result<()> {
    ensure!(
        !*cancel.borrow() && cancel.has_changed().is_ok(),
        "LSP operation cancelled"
    );
    Ok(())
}

async fn cancellable<T>(
    cancel: &watch::Receiver<bool>,
    work: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        biased;
        _ = super::cancelled(cancel.clone()) => bail!("LSP operation cancelled"),
        result = tokio::time::timeout(super::REQUEST_TIMEOUT, work) => result.context("LSP request timed out")?,
    }
}

async fn blocking<T: Send + 'static>(
    cancel: &watch::Receiver<bool>,
    work: impl FnOnce(watch::Receiver<bool>) -> Result<T> + Send + 'static,
) -> Result<T> {
    check_cancel(cancel)?;
    let (sender, signal) = watch::channel(false);
    let mut task = tokio::task::spawn_blocking(move || {
        check_cancel(&signal)?;
        work(signal)
    });
    tokio::select! {
        biased;
        _ = super::cancelled(cancel.clone()) => {
            let _ = sender.send(true);
            task.await.context("LSP filesystem worker failed")?
        }
        result = &mut task => result.context("LSP filesystem worker failed")?,
    }
}

fn required_string<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{key} is required and must be a nonempty string"))
}

fn coordinate(input: &Value, key: &str) -> Result<usize> {
    input[key]
        .as_u64()
        .and_then(|value| value.checked_sub(1))
        .and_then(|value| usize::try_from(value).ok())
        .with_context(|| format!("{key} must be a positive 1-based integer"))
}

fn validate_input(name: &str, input: &Value) -> Result<()> {
    let object = input.as_object().context("LSP input must be an object")?;
    let allowed: &[&str] = match name {
        "lsp" => &[
            "operation",
            "filePath",
            "line",
            "character",
            "query",
            "severity",
        ],
        "rename_symbol" => &["filePath", "line", "character", "newName"],
        "code_action" => &[
            "filePath",
            "line",
            "character",
            "endLine",
            "endCharacter",
            "apply",
        ],
        _ => bail!("unknown LSP tool {name}"),
    };
    for key in object.keys() {
        ensure!(allowed.contains(&key.as_str()), "unknown LSP field {key}");
    }
    for key in ["filePath", "query", "severity", "apply", "newName"] {
        if let Some(value) = input.get(key) {
            ensure!(value.is_string(), "{key} must be a string");
        }
    }
    for key in ["line", "character", "endLine", "endCharacter"] {
        if input.get(key).is_some() {
            coordinate(input, key)?;
        }
    }
    if let Some(severity) = input.get("severity") {
        ensure!(
            matches!(severity.as_str(), Some("error" | "warning" | "all")),
            "severity must be error, warning, or all"
        );
    }
    let operation = if name == "lsp" {
        required_string(input, "operation")?
    } else {
        name
    };
    ensure!(
        matches!(
            operation,
            "diagnostics"
                | "definition"
                | "references"
                | "hover"
                | "implementation"
                | "document_symbols"
                | "workspace_symbols"
                | "rename_symbol"
                | "code_action"
        ) && (name != "lsp" || !matches!(operation, "rename_symbol" | "code_action")),
        "unknown LSP operation {operation}"
    );
    if operation != "workspace_symbols" {
        required_string(input, "filePath")?;
    }
    if matches!(
        operation,
        "definition" | "references" | "hover" | "implementation" | "rename_symbol" | "code_action"
    ) {
        coordinate(input, "line")?;
        coordinate(input, "character")?;
    }
    if name == "rename_symbol" {
        required_string(input, "newName")?;
    }
    if name == "code_action" {
        let start = (coordinate(input, "line")?, coordinate(input, "character")?);
        let end = (
            if input.get("endLine").is_some() {
                coordinate(input, "endLine")?
            } else {
                start.0
            },
            if input.get("endCharacter").is_some() {
                coordinate(input, "endCharacter")?
            } else {
                start.1
            },
        );
        ensure!(end >= start, "invalid or reversed action range");
        if input.get("apply").is_some() {
            required_string(input, "apply")?;
        }
    }
    Ok(())
}

fn position_params(
    path: &Path,
    text: &str,
    encoding: PositionEncoding,
    input: &Value,
) -> Result<Value> {
    let line = coordinate(input, "line")?;
    let character = encoding
        .from_column(text, line, coordinate(input, "character")?)
        .context("position is out of range")?;
    Ok(
        json!({"textDocument":{"uri":path_to_uri(path)},"position":{"line":line,"character":character}}),
    )
}

fn action_params(
    path: &Path,
    text: &str,
    encoding: PositionEncoding,
    input: &Value,
    diagnostics: Vec<Value>,
) -> Result<Value> {
    let start = position_params(path, text, encoding, input)?;
    let end_line = if input.get("endLine").is_some() {
        coordinate(input, "endLine")?
    } else {
        coordinate(input, "line")?
    };
    let end_column = if input.get("endCharacter").is_some() {
        coordinate(input, "endCharacter")?
    } else {
        coordinate(input, "character")?
    };
    let end_character = encoding
        .from_column(text, end_line, end_column)
        .context("end position is out of range")?;
    Ok(
        json!({"textDocument":start["textDocument"],"range":{"start":start["position"],"end":{"line":end_line,"character":end_character}},"context":{"diagnostics":diagnostics}}),
    )
}

async fn request_edits(
    client: &LspClient,
    name: &str,
    input: &Value,
    params: &Value,
) -> Result<BTreeMap<PathBuf, DocumentEdits>> {
    let edit = if name == "rename_symbol" {
        client
            .request("textDocument/rename", params.clone())
            .await?
    } else {
        let result = client
            .request("textDocument/codeAction", params.clone())
            .await?;
        let action = select_action(array_or_null(&result)?, required_string(input, "apply")?)?;
        let resolved = if action.get("edit").is_some() {
            action.clone()
        } else {
            client.request("codeAction/resolve", action.clone()).await?
        };
        direct_action_edit(&resolved)?.clone()
    };
    PreparedWrites::check_budget(0, serde_json::to_vec(&edit)?.len())?;
    let edits = collect_workspace_edit(&edit).map_err(anyhow::Error::msg)?;
    PreparedWrites::check_budget(edits.len(), 0)?;
    Ok(edits)
}

fn select_action<'a>(actions: &'a [Value], title: &str) -> Result<&'a Value> {
    let mut matching = actions
        .iter()
        .filter(|action| action["title"].as_str() == Some(title));
    let action = matching
        .next()
        .with_context(|| format!("no action titled '{title}'"))?;
    ensure!(matching.next().is_none(), "ambiguous action title");
    ensure!(
        action.get("disabled").is_none() && action.get("command").is_none(),
        "disabled actions and server commands are unsupported; no files changed"
    );
    Ok(action)
}

fn direct_action_edit(action: &Value) -> Result<&Value> {
    ensure!(
        action.get("disabled").is_none() && action.get("command").is_none(),
        "disabled actions and server commands are unsupported; no files changed"
    );
    action
        .get("edit")
        .filter(|edit| !edit.is_null())
        .context("action has no direct edit; no files changed")
}

#[cfg(test)]
fn snapshot_changes(
    edits: &BTreeMap<PathBuf, DocumentEdits>,
    snapshots: &HashMap<String, DocumentSnapshot>,
    current: &HashMap<String, DocumentSnapshot>,
    encoding: PositionEncoding,
) -> Result<Vec<(PathBuf, String, String)>> {
    snapshot_changes_cancellable(edits, snapshots, current, encoding, None)
}

fn snapshot_changes_cancellable(
    edits: &BTreeMap<PathBuf, DocumentEdits>,
    snapshots: &HashMap<String, DocumentSnapshot>,
    current: &HashMap<String, DocumentSnapshot>,
    encoding: PositionEncoding,
    cancel: Option<&watch::Receiver<bool>>,
) -> Result<Vec<(PathBuf, String, String)>> {
    PreparedWrites::check_budget(edits.len(), 0)?;
    let mut changes = Vec::new();
    let mut bytes = 0usize;
    for (path, edit) in edits {
        if let Some(cancel) = cancel {
            check_cancel(cancel)?;
        }
        let uri = path_to_uri(path);
        let snapshot = snapshots.get(&uri).with_context(|| {
            format!(
                "{} was not snapshotted before the request; no files changed",
                path.display()
            )
        })?;
        ensure!(edit.version.is_none_or(|version| version == snapshot.version) && current.get(&uri).is_some_and(|doc| doc.version == snapshot.version && doc.text == snapshot.text),"stale document version for {}; no files changed",path.display());
        bytes = bytes.saturating_add(snapshot.text.len());
        PreparedWrites::check_budget(edits.len(), bytes)?;
        let after =
            apply_edits(&snapshot.text, &edit.edits, encoding).map_err(anyhow::Error::msg)?;
        if after != snapshot.text {
            bytes = bytes.saturating_add(after.len());
            PreparedWrites::check_budget(edits.len(), bytes)?;
            changes.push((path.clone(), snapshot.text.clone(), after));
        }
    }
    Ok(changes)
}

fn array_or_null(value: &Value) -> Result<&[Value]> {
    if value.is_null() {
        return Ok(&[]);
    }
    value
        .as_array()
        .map(Vec::as_slice)
        .context("invalid language-server response: expected an array or null")
}

fn collect_locations(result: &Value) -> Result<Vec<(String, usize, usize)>> {
    let values = if result.is_null() {
        &[][..]
    } else if let Some(values) = result.as_array() {
        values.as_slice()
    } else {
        std::slice::from_ref(result)
    };
    values
        .iter()
        .map(|value| {
            let uri = value["uri"]
                .as_str()
                .or_else(|| value["targetUri"].as_str())
                .context("location URI missing")?;
            let range = value
                .get("range")
                .or_else(|| value.get("targetSelectionRange"))
                .or_else(|| value.get("targetRange"))
                .context("location range missing")?;
            let number = |key: &str| {
                range["start"][key]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n < usize::MAX)
                    .context("invalid location position")
            };
            Ok((uri.into(), number("line")?, number("character")?))
        })
        .collect()
}

fn render_hover(contents: &Value) -> String {
    if contents.is_null() {
        return "No hover information.".into();
    }
    if let Some(text) = contents.as_str().or_else(|| contents["value"].as_str()) {
        return text.into();
    }
    if let Some(parts) = contents.as_array() {
        return parts
            .iter()
            .map(render_hover)
            .collect::<Vec<_>>()
            .join("\n\n");
    }
    contents.to_string()
}

fn render_symbol(symbol: &Value, depth: usize, out: &mut String) {
    if out.len() >= 256 * 1024 {
        if !out.ends_with("… symbol output truncated\n") {
            out.push_str("… symbol output truncated\n");
        }
        return;
    }
    if depth >= 32 {
        out.push_str("… symbol nesting truncated at depth 32\n");
        return;
    }
    let line = symbol["range"]["start"]["line"]
        .as_u64()
        .or_else(|| symbol["location"]["range"]["start"]["line"].as_u64())
        .unwrap_or(0)
        .saturating_add(1);
    out.push_str(&format!(
        "{}{} {}  :{}\n",
        "  ".repeat(depth),
        symbol_kind(symbol["kind"].as_u64().unwrap_or(0)),
        symbol["name"].as_str().unwrap_or(""),
        line
    ));
    if let Some(children) = symbol["children"].as_array() {
        for child in children {
            render_symbol(child, depth + 1, out);
        }
    }
}

fn symbol_kind(kind: u64) -> &'static str {
    match kind {
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

fn render_diagnostics(
    path: &str,
    text: &str,
    encoding: PositionEncoding,
    publication: &DiagnosticPublication,
    severity: &str,
) -> Result<String> {
    let min_severity = match severity {
        "all" => 4,
        "warning" => 2,
        _ => 1,
    };
    let mut out = if publication.version_confirmed {
        String::new()
    } else {
        "Document version unavailable; freshness unconfirmed.\n".into()
    };
    let lines: Vec<_> = text.lines().collect();
    let mut shown = 0;
    for diagnostic in &publication.diagnostics {
        let severity = diagnostic["severity"].as_u64().unwrap_or(1);
        if severity > min_severity {
            continue;
        }
        let line = diagnostic["range"]["start"]["line"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .context("invalid diagnostic line")?;
        let units = diagnostic["range"]["start"]["character"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .context("invalid diagnostic character")?;
        let column = encoding
            .to_column(text, line, units)
            .context("server diagnostic has an invalid position")?;
        let label = match severity {
            1 => "ERROR",
            2 => "WARN",
            3 => "INFO",
            _ => "HINT",
        };
        out.push_str(&format!(
            "{label} {path}:{}:{} {}\n",
            line + 1,
            column + 1,
            diagnostic["message"]
                .as_str()
                .unwrap_or("")
                .replace('\n', " ")
        ));
        if let Some(source) = lines.get(line) {
            out.push_str(&format!("  {source}\n{}^\n", " ".repeat(column + 2)));
        }
        shown += 1;
    }
    if shown == 0 {
        out.push_str(&if publication.version_confirmed {
            format!("No diagnostics for {path} (at or above {severity} severity).")
        } else {
            "Server published no matching diagnostics; this is not a verified clean result.".into()
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Workspace(PathBuf);

    impl Workspace {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("bob-lsp-service-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
        fn file(&self, name: &str, text: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn edit(start: usize, end: usize, replacement: &str) -> Value {
        json!({"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":end}},"newText":replacement})
    }

    #[test]
    fn workspace_symbols_preserve_healthy_results_and_name_every_failure() {
        let workspace = Workspace::new();
        let service = LspService::start(&[], &workspace.0);
        let symbols = json!([{"name":"kept_symbol","kind":12,"location":{"uri":path_to_uri(&workspace.0.join("a.rs")),"range":{"start":{"line":2,"character":0}}}}]);
        let output = service
            .render_workspace_symbols(vec![
                ("offline".into(), Err(anyhow::anyhow!("unavailable"))),
                ("healthy".into(), Ok(symbols)),
                ("broken".into(), Ok(json!({"invalid":"response"}))),
            ])
            .unwrap();
        assert!(output.contains("a.rs:3") && output.contains("kept_symbol"));
        assert!(
            output.contains("offline: unavailable")
                && output.contains("broken: invalid language-server response")
        );
        assert!(output.contains("partial results"));
        let empty = service
            .render_workspace_symbols(vec![
                ("empty".into(), Ok(Value::Null)),
                ("offline".into(), Err(anyhow::anyhow!("unavailable"))),
            ])
            .unwrap();
        assert!(empty.contains("No matching symbols") && empty.contains("offline"));
        let error = service
            .render_workspace_symbols(vec![
                ("first".into(), Err(anyhow::anyhow!("unavailable"))),
                ("second".into(), Err(anyhow::anyhow!("request failed"))),
            ])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("all language servers")
                && error.contains("first")
                && error.contains("second")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_work_yields_and_cancellation_awaits_settlement() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let owner = std::thread::current().id();
        let (cancel_sender, cancel) = watch::channel(false);
        let (started_sender, started) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let settled = Arc::new(AtomicBool::new(false));
        let worker_settled = settled.clone();
        let mut work = Box::pin(blocking(&cancel, move |cancel| {
            assert_ne!(std::thread::current().id(), owner);
            started_sender.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            worker_settled.store(true, Ordering::SeqCst);
            check_cancel(&cancel)
        }));
        assert!(futures::poll!(&mut work).is_pending());
        tokio::time::timeout(Duration::from_secs(5), started)
            .await
            .unwrap()
            .unwrap();
        cancel_sender.send(true).unwrap();
        assert!(futures::poll!(&mut work).is_pending());
        assert!(!settled.load(Ordering::SeqCst));
        tokio::task::yield_now().await;
        release.send(()).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(5), work)
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        assert!(settled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancelled_or_abandoned_preparation_does_not_start_filesystem_work() {
        let service = LspService::start(&[], Path::new("."));
        let (sender, cancel) = watch::channel(true);
        let input = json!({"filePath":"missing.rs","line":1,"character":1,"newName":"new"});
        assert!(service
            .prepare_with_cancel("rename_symbol", &input, cancel.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        drop(sender);
        assert!(
            blocking(&cancel, |_| -> Result<()> { panic!("must not start") })
                .await
                .is_err()
        );
    }

    #[test]
    fn schemas_and_validation_cover_seven_queries_and_normal_action_inputs() {
        let service = LspService::start(&[], Path::new("."));
        let descriptors = service.descriptors();
        assert_eq!(descriptors.len(), 3);
        assert_eq!(descriptors[0].effect, EffectClass::Read);
        for operation in descriptors[0].spec.input_schema["properties"]["operation"]["enum"]
            .as_array()
            .unwrap()
        {
            validate_input(
                "lsp",
                &json!({"operation":operation,"filePath":"a.rs","line":1,"character":1}),
            )
            .unwrap();
        }
        assert_eq!(
            descriptors[0].spec.input_schema["properties"]["operation"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            7
        );
        for input in [
            json!({}),
            json!({"operation":"rename_symbol"}),
            json!({"operation":"hover","filePath":"a.rs","line":0,"character":1}),
            json!({"operation":"diagnostics","filePath":"a.rs","severity":"fatal"}),
            json!({"operation":"workspace_symbols","query":false}),
            json!({"operation":"workspace_symbols","extra":true}),
        ] {
            assert!(validate_input("lsp", &input).is_err(), "{input}");
        }
        for input in [
            json!({"filePath":"a.rs","line":2,"character":1,"endLine":1}),
            json!({"filePath":"a.rs","line":1,"character":1,"apply":null}),
            json!({"filePath":"a.rs","line":1,"character":1,"endCharacter":0}),
        ] {
            assert!(validate_input("code_action", &input).is_err());
        }
        validate_input(
            "code_action",
            &json!({"filePath":"a.rs","line":1,"character":1}),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn empty_configuration_starts_nothing_and_queries_fail_explicitly() {
        let workspace = Workspace::new();
        workspace.file("a.rs", "fn a() {}\n");
        let service = LspService::start(&[], &workspace.0);
        assert!(service.statuses().is_empty());
        service.notify_changed(&workspace.0.join("a.rs"));
        let (_cancel_sender, cancel) = watch::channel(false);
        for (input, diagnostic) in [
            (
                json!({"operation":"workspace_symbols","query":"a"}),
                "no language servers configured",
            ),
            (
                json!({"operation":"hover","filePath":"a.rs","line":1,"character":4}),
                "no initialized language server configured for",
            ),
            (
                json!({"operation":"diagnostics","filePath":"a.rs"}),
                "no initialized language server configured for",
            ),
        ] {
            let spec = service.prepare("lsp", &input).await.unwrap();
            let outcome = service.execute("lsp", &spec, &cancel).await.unwrap();
            assert!(outcome.is_error);
            assert!(!outcome.interrupted);
            assert!(outcome.text.contains(diagnostic), "{}", outcome.text);
            if input.get("filePath").is_some() {
                assert!(outcome.text.contains("a.rs"));
                assert!(outcome
                    .text
                    .contains("server status: no language servers are configured"));
            }
        }
        let spec = service
            .prepare(
                "code_action",
                &json!({"filePath":"a.rs","line":1,"character":1}),
            )
            .await
            .unwrap();
        assert_eq!(spec["kind"], "query");
        let outcome = service
            .execute("code_action", &spec, &cancel)
            .await
            .unwrap();
        assert!(outcome.is_error);
        assert!(!outcome.interrupted);
        assert!(outcome
            .text
            .contains("no initialized language server configured for"));
        assert!(outcome.text.contains("a.rs"));
        assert!(outcome
            .text
            .contains("server status: no language servers are configured"));
        assert!(service.statuses().is_empty());
        LspService::validate_prepared(
            "code_action",
            &json!({"filePath":"a.rs","line":1,"character":1}),
            &spec,
        )
        .unwrap();
        assert!(LspService::validate_prepared(
            "code_action",
            &json!({"filePath":"b.rs","line":1,"character":1}),
            &spec
        )
        .is_err());
        assert!(service
            .execute("lsp", &spec, &cancel)
            .await
            .unwrap_err()
            .to_string()
            .contains("name changed"));
        let (_true_cancel_sender, cancelled) = watch::channel(true);
        assert!(service
            .execute("code_action", &spec, &cancelled)
            .await
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
        for (name, input) in [
            (
                "rename_symbol",
                json!({"filePath":"a.rs","line":1,"character":1,"newName":"b"}),
            ),
            (
                "code_action",
                json!({"filePath":"a.rs","line":1,"character":1,"apply":"Fix"}),
            ),
        ] {
            let mutation = json!({"kind":"query","name":name,"input":input});
            assert!(service
                .execute(name, &mutation, &cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("write plan"));
        }
        let invalid = json!({"kind":"query","name":"lsp","input":{"operation":"invalid"}});
        assert!(service
            .execute("lsp", &invalid, &cancel)
            .await
            .unwrap_err()
            .to_string()
            .contains("unknown LSP operation"));
    }

    #[test]
    fn query_settlement_preserves_context_and_rechecks_cancellation() {
        let (sender, cancel) = watch::channel(false);
        let error = || Err(anyhow::anyhow!("server unavailable").context("query failed"));
        let outcome = settled_query_outcome(error(), &cancel).unwrap();
        assert!(outcome.is_error);
        assert!(!outcome.interrupted);
        assert_eq!(outcome.text, "query failed: server unavailable");
        let outcome = settled_query_outcome(Ok("symbols".into()), &cancel).unwrap();
        assert!(!outcome.is_error && !outcome.interrupted);
        assert_eq!(outcome.text, "symbols");
        sender.send(true).unwrap();
        for result in [error(), Ok("already finished".into())] {
            assert!(settled_query_outcome(result, &cancel)
                .unwrap_err()
                .to_string()
                .contains("cancelled"));
        }
        let (sender, cancel) = watch::channel(false);
        drop(sender);
        assert!(settled_query_outcome(error(), &cancel)
            .unwrap_err()
            .to_string()
            .contains("cancelled"));
    }

    #[tokio::test]
    async fn query_filesystem_worker_panics_are_not_recoverable_tool_errors() {
        let (_sender, cancel) = watch::channel(false);
        let result = blocking(&cancel, |_| -> Result<String> {
            panic!("fixture filesystem worker panic")
        })
        .await;
        let error = settled_query_outcome(result, &cancel).unwrap_err();
        assert!(error.is::<tokio::task::JoinError>());
        assert!(format!("{error:#}").contains("fixture filesystem worker panic"));
    }

    #[test]
    fn positions_convert_unicode_scalar_columns_to_negotiated_units() {
        let path = std::env::current_dir().unwrap().join("a.rs");
        let text = "a😀z\r\nβ\n";
        let input = json!({"line":1,"character":3,"endLine":2,"endCharacter":2});
        for (encoding, units) in [
            (PositionEncoding::Utf8, 5),
            (PositionEncoding::Utf16, 3),
            (PositionEncoding::Utf32, 2),
        ] {
            let params = position_params(&path, text, encoding, &input).unwrap();
            assert_eq!(params["position"], json!({"line":0,"character":units}));
            let params = action_params(&path, text, encoding, &input, vec![]).unwrap();
            assert_eq!(params["range"]["end"]["line"], 1);
            assert_eq!(params["context"]["diagnostics"], json!([]));
        }
        assert!(position_params(
            &path,
            text,
            PositionEncoding::Utf16,
            &json!({"line":1,"character":5})
        )
        .is_err());
    }

    #[test]
    fn diagnostic_freshness_and_severity_are_explicit() {
        let mut publication = DiagnosticPublication {
            diagnostics: vec![],
            version_confirmed: false,
        };
        let output = render_diagnostics(
            "a.rs",
            "a😀z\n",
            PositionEncoding::Utf16,
            &publication,
            "error",
        )
        .unwrap();
        assert!(
            output.contains("freshness unconfirmed") && output.contains("not a verified clean")
        );
        publication.version_confirmed = true;
        assert!(
            render_diagnostics("a.rs", "", PositionEncoding::Utf16, &publication, "error")
                .unwrap()
                .starts_with("No diagnostics")
        );
        let diagnostic = json!({"range":{"start":{"line":0,"character":3}},"severity":2,"message":"problem\ncontinued"});
        publication.diagnostics = vec![diagnostic.clone(); 21];
        publication.diagnostics[20]["message"] = json!("unique twenty-first diagnostic");
        let output = render_diagnostics(
            "a.rs",
            "a😀z\n",
            PositionEncoding::Utf16,
            &publication,
            "warning",
        )
        .unwrap();
        assert_eq!(
            output.matches("WARN a.rs:1:3 problem continued").count(),
            20
        );
        assert!(output.contains("unique twenty-first diagnostic"));
        assert_eq!(output.matches("WARN a.rs:1:3").count(), 21);
        assert!(!output.contains("showing first 20"));
        assert!(render_diagnostics(
            "a.rs",
            "a😀z\n",
            PositionEncoding::Utf16,
            &publication,
            "error"
        )
        .unwrap()
        .starts_with("No diagnostics"));
        publication.diagnostics[0]["range"]["start"]["character"] = json!(2);
        assert!(render_diagnostics(
            "a.rs",
            "a😀z\n",
            PositionEncoding::Utf16,
            &publication,
            "all"
        )
        .is_err());
    }

    #[test]
    fn location_hover_and_symbol_protocol_shapes_render_without_raw_wrappers() {
        let location = json!({"uri":"file:///a.rs","range":{"start":{"line":2,"character":3}}});
        let link = json!({"targetUri":"file:///b.rs","targetRange":{"start":{"line":0,"character":0}},"targetSelectionRange":{"start":{"line":4,"character":5}}});
        assert_eq!(
            collect_locations(&location).unwrap(),
            vec![("file:///a.rs".into(), 2, 3)]
        );
        assert_eq!(
            collect_locations(&json!([location, link])).unwrap()[1],
            ("file:///b.rs".into(), 4, 5)
        );
        assert!(collect_locations(&Value::Null).unwrap().is_empty());
        assert!(collect_locations(&json!({"uri":"file:///a.rs"})).is_err());
        assert_eq!(
            render_hover(&json!([{"language":"rust","value":"fn run()"},"docs"])),
            "fn run()\n\ndocs"
        );
        let mut output = String::new();
        render_symbol(
            &json!({"name":"Thing","kind":23,"range":{"start":{"line":0}},"children":[{"name":"run","kind":6,"location":{"range":{"start":{"line":2}}}}]}),
            0,
            &mut output,
        );
        assert_eq!(output, "struct Thing  :1\n  method run  :3\n");
        let mut nested = json!({"name":"end"});
        for _ in 0..40 {
            nested = json!({"name":"node","children":[nested]});
        }
        let mut output = String::new();
        render_symbol(&nested, 0, &mut output);
        assert!(output.contains("nesting truncated"));
    }

    #[test]
    fn actions_reject_commands_disabled_ambiguous_and_missing_edits() {
        let valid = json!({"title":"Fix","edit":{"changes":{}}});
        assert!(select_action(std::slice::from_ref(&valid), "Fix").is_ok());
        assert!(direct_action_edit(&valid).is_ok());
        assert!(select_action(&[valid.clone(), valid], "Fix")
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        for action in [
            json!({"title":"Fix","command":"unsafe"}),
            json!({"title":"Fix","disabled":{"reason":"no"}}),
        ] {
            assert!(select_action(std::slice::from_ref(&action), "Fix").is_err());
            assert!(direct_action_edit(&action).is_err());
        }
        assert!(direct_action_edit(&json!({"title":"Fix"})).is_err());
        assert!(select_action(&[], "Fix").is_err());
    }

    #[test]
    fn workspace_planning_validates_snapshot_versions_and_strict_edits() {
        let path = std::env::current_dir().unwrap().join("a.rs");
        let uri = path_to_uri(&path);
        let snapshots = HashMap::from([(
            uri.clone(),
            DocumentSnapshot {
                version: 7,
                text: "a😀z\r\n".into(),
            },
        )]);
        let current = snapshots.clone();
        let result = json!({"documentChanges":[{"textDocument":{"uri":uri,"version":7},"edits":[edit(1,3,"new")]}]});
        let edits = collect_workspace_edit(&result).unwrap();
        assert_eq!(
            snapshot_changes(&edits, &snapshots, &current, PositionEncoding::Utf16).unwrap(),
            vec![(path.clone(), "a😀z\r\n".into(), "anewz\r\n".into())]
        );
        assert!(
            snapshot_changes(&edits, &HashMap::new(), &current, PositionEncoding::Utf16).is_err()
        );
        let mut changed = current.clone();
        changed.get_mut(&uri).unwrap().version += 1;
        assert!(snapshot_changes(&edits, &snapshots, &changed, PositionEncoding::Utf16).is_err());
        changed = current.clone();
        changed.get_mut(&uri).unwrap().text.push('x');
        assert!(snapshot_changes(&edits, &snapshots, &changed, PositionEncoding::Utf16).is_err());
        for (version, edits) in [
            (8, vec![edit(1, 3, "x")]),
            (7, vec![edit(2, 3, "x")]),
            (7, vec![edit(0, 3, "x"), edit(1, 3, "y")]),
        ] {
            let result = json!({"documentChanges":[{"textDocument":{"uri":uri,"version":version},"edits":edits}]});
            assert!(snapshot_changes(
                &collect_workspace_edit(&result).unwrap(),
                &snapshots,
                &current,
                PositionEncoding::Utf16
            )
            .is_err());
        }
        assert!(collect_workspace_edit(
            &json!({"documentChanges":[{"kind":"rename","oldUri":uri,"newUri":"file:///b.rs"}]})
        )
        .is_err());
    }

    #[tokio::test]
    async fn valid_plan_from_another_workspace_is_rejected_before_staging() {
        let workspace = Workspace::new();
        let other = Workspace::new();
        workspace.file("a.rs", "local\n");
        let path = other.file("a.rs", "old\n");
        let service = LspService::start(&[], &workspace.0);
        let plan = PreparedWrites::prepare(
            &other.0,
            vec![(path.clone(), "old\n".into(), "new\n".into())],
        )
        .unwrap();
        let spec = serde_json::to_value(Prepared::Writes {
            name: "rename_symbol".into(),
            input: json!({"filePath":"a.rs","line":1,"character":1,"newName":"new"}),
            plan,
            paths: vec![path.clone()],
        })
        .unwrap();
        let (_sender, cancel) = watch::channel(false);
        assert!(service
            .execute("rename_symbol", &spec, &cancel)
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "old\n");
        assert_eq!(std::fs::read_dir(&other.0).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&workspace.0).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn exact_plan_preparation_does_not_mutate_and_execution_never_needs_server() {
        let workspace = Workspace::new();
        let path = workspace.file("a.rs", "old\n");
        let service = LspService::start(&[], &workspace.0);
        let plan = PreparedWrites::prepare(
            &workspace.0,
            vec![(path.clone(), "old\n".into(), "new\n".into())],
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
        let spec = serde_json::to_value(Prepared::Writes {
            name: "rename_symbol".into(),
            input: json!({"filePath":"a.rs","line":1,"character":1,"newName":"new"}),
            plan,
            paths: vec![path.clone()],
        })
        .unwrap();
        let preview = LspService::prepared_preview(&spec).unwrap();
        assert!(preview.contains("a.rs") && preview.contains("old") && preview.contains("new"));
        assert!(LspService::validate_prepared(
            "rename_symbol",
            &json!({"filePath":"a.rs","line":1,"character":1,"newName":"different"}),
            &spec
        )
        .is_err());
        let (_cancel_sender, cancel) = watch::channel(false);
        let outcome = service
            .execute("rename_symbol", &spec, &cancel)
            .await
            .unwrap();
        assert!(!outcome.is_error);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");
        assert!(service
            .execute("rename_symbol", &spec, &cancel)
            .await
            .is_err());
        let plan = PreparedWrites::prepare(
            &workspace.0,
            vec![(path.clone(), "new\n".into(), "third\n".into())],
        )
        .unwrap();
        let spec = serde_json::to_value(Prepared::Writes {
            name: "code_action".into(),
            input: json!({"filePath":"a.rs","line":1,"character":1,"apply":"Fix"}),
            plan,
            paths: vec![path.clone()],
        })
        .unwrap();
        std::fs::write(&path, "user edit\n").unwrap();
        assert!(service
            .execute("code_action", &spec, &cancel)
            .await
            .is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user edit\n");
    }
}
