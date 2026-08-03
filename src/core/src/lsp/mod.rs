//! LSP client + manager. bob acts as an LSP *client*, driving one or more
//! language servers over stdio (JSON-RPC 2.0 with Content-Length framing — the
//! LSP wire format, distinct from MCP's newline-delimited variant). We don't use
//! an off-the-shelf framework (async-lsp's tower stack fights a multi-server
//! manager); the transport is ~150 lines and gives us full control over
//! background spawning, per-server health, and notification handling
//! (publishDiagnostics + $/progress).
//!
//! Servers are configured per-project (see LspServerConfig). The manager routes
//! a file to the right server by the longest-matching `root` whose `extensions`
//! contains the file's extension — this is how monorepos work.

use crate::core::config::LspServerConfig;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::oneshot;

/// Lifecycle/health of a single language server, surfaced in the status bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// Process spawned, initialize handshake in flight.
    Starting,
    /// Initialized; still indexing the workspace. Optional percent from
    /// `$/progress` (rust-analyzer reports "Indexing" with a percentage).
    Indexing(Option<u8>),
    /// Indexed and ready to answer requests.
    Ready,
    /// Failed to start or crashed. Carries a short reason.
    Failed(String),
}

/// Diagnostics for one file, plus the doc version they correspond to.
#[derive(Clone, Default)]
struct FileDiagnostics {
    /// Doc version the server reported these against (0 if unversioned). Kept so
    /// the diagnostics tool can tell stale pushes from fresh ones after an edit.
    #[allow(dead_code)]
    version: i64,
    diagnostics: Vec<Value>,
}

/// A live connection to one language server.
#[derive(Clone)]
pub struct LspClient {
    inner: Arc<LspInner>,
}

struct LspInner {
    name: String,
    stdin: tokio::sync::Mutex<ChildStdin>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    next_id: AtomicI64,
    /// Latest publishDiagnostics per file URI.
    diagnostics: Mutex<HashMap<String, FileDiagnostics>>,
    /// Doc versions we've sent per file URI (for didOpen/didChange).
    versions: Mutex<HashMap<String, i64>>,
    health: Mutex<Health>,
    /// codeActionKinds the server advertised at initialize (e.g. "quickfix",
    /// "refactor", "source.organizeImports"). Empty if it didn't specify.
    code_action_kinds: Mutex<Vec<String>>,
    _child: tokio::sync::Mutex<Child>,
}

impl LspClient {
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn health(&self) -> Health {
        self.inner.health.lock().unwrap().clone()
    }

    /// Spawn the server and run the initialize handshake. Returns as soon as the
    /// server acknowledges `initialize`; workspace indexing continues in the
    /// background and is reflected via `health()`.
    pub async fn start(cfg: &LspServerConfig, repo_root: &Path) -> anyhow::Result<LspClient> {
        let server_root = repo_root.join(&cfg.root);
        let root_uri = path_to_uri(&server_root);

        let mut command = tokio::process::Command::new(resolve_command(&cfg.command));
        command
            .args(&cfg.args)
            .current_dir(&server_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = command.spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to start LSP '{}' (command '{}'): {}. \
                 Ensure it's installed and on PATH — if it lives in ~/.cargo/bin or \
                 another dir, launch bob with that dir on PATH.",
                cfg.name,
                cfg.command,
                e
            )
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;

        let inner = Arc::new(LspInner {
            name: cfg.name.clone(),
            stdin: tokio::sync::Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            diagnostics: Mutex::new(HashMap::new()),
            versions: Mutex::new(HashMap::new()),
            health: Mutex::new(Health::Starting),
            code_action_kinds: Mutex::new(Vec::new()),
            _child: tokio::sync::Mutex::new(child),
        });

        spawn_reader(inner.clone(), stdout);

        let client = LspClient { inner };

        // initialize handshake.
        #[allow(clippy::disallowed_names)]
        let init = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": true },
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": {},
                    "references": {},
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": { "valueSet": [
                                "quickfix", "refactor", "refactor.extract",
                                "refactor.inline", "refactor.rewrite", "source",
                                "source.organizeImports", "source.fixAll"
                            ]}
                        }
                    },
                    "rename": { "prepareSupport": true }
                },
                "window": { "workDoneProgress": true }
            }
        });
        let init_result = client.request("initialize", init).await?;
        // Record the code-action kinds the server advertises, so `code_action`
        // can tell the model what this server can do without a round-trip.
        if let Some(kinds) =
            init_result["capabilities"]["codeActionProvider"]["codeActionKinds"].as_array()
        {
            let kinds: Vec<String> = kinds
                .iter()
                .filter_map(|k| k.as_str().map(|s| s.to_string()))
                .collect();
            *client.inner.code_action_kinds.lock().unwrap() = kinds;
        }
        client.notify("initialized", json!({})).await;
        *client.inner.health.lock().unwrap() = Health::Indexing(None);
        Ok(client)
    }

    /// The code-action kinds this server advertised at initialize (may be empty
    /// if it didn't specify — many servers still support actions dynamically).
    pub fn code_action_kinds(&self) -> Vec<String> {
        self.inner.code_action_kinds.lock().unwrap().clone()
    }

    /// Notify the server a file is open (or its content changed). Bumps the doc
    /// version. Call before requesting diagnostics/nav so the server sees the
    /// latest on-disk (or in-flight) text.
    pub async fn sync_file(&self, path: &Path, text: &str) {
        let uri = path_to_uri(path);
        let version = {
            let mut versions = self.inner.versions.lock().unwrap();
            let v = versions.entry(uri.clone()).or_insert(0);
            *v += 1;
            *v
        };
        let language_id = language_id_for(path);
        if version == 1 {
            self.notify(
                "textDocument/didOpen",
                json!({ "textDocument": {
                    "uri": uri, "languageId": language_id,
                    "version": version, "text": text
                }}),
            )
            .await;
        } else {
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [ { "text": text } ]
                }),
            )
            .await;
        }
    }

    /// The latest diagnostics for a file (empty if none / not opened).
    pub fn diagnostics_for(&self, path: &Path) -> Vec<Value> {
        let uri = path_to_uri(path);
        self.inner
            .diagnostics
            .lock()
            .unwrap()
            .get(&uri)
            .map(|d| d.diagnostics.clone())
            .unwrap_or_default()
    }

    /// Send a textDocument request (definition/references/hover/etc.) at a
    /// position, returning the raw `result`.
    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&msg).await?;
        let resp = tokio::time::timeout(std::time::Duration::from_secs(15), rx)
            .await
            .map_err(|_| anyhow::anyhow!("LSP request '{}' timed out", method))?
            .map_err(|_| anyhow::anyhow!("LSP connection closed"))?;
        if let Some(err) = resp.get("error") {
            anyhow::bail!("LSP error: {}", err);
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.send(&msg).await;
    }

    /// Write one framed JSON-RPC message (Content-Length header + body).
    async fn send(&self, msg: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_string(msg)?;
        let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut stdin = self.inner.stdin.lock().await;
        stdin.write_all(frame.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }
}

/// Resolve an LSP server command to an executable path. If `command` already
/// contains a path separator, or bob's inherited `PATH` can find it, it's used
/// verbatim (the OS resolves it). Otherwise we probe common install dirs that a
/// GUI/non-login launch often misses — most importantly `~/.cargo/bin`, where
/// rustup places the `rust-analyzer` shim. Returns the original name if nothing
/// matches, so the spawn still surfaces a clear "not found" error.
fn resolve_command(command: &str) -> std::path::PathBuf {
    let as_path = Path::new(command);
    if as_path.is_absolute() || command.contains('/') {
        return as_path.to_path_buf();
    }
    // Already resolvable via the inherited PATH? Trust it.
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if is_executable(&dir.join(command)) {
                return as_path.to_path_buf(); // let the OS resolve it normally
            }
        }
    }
    // Fall back to common install locations the launch env may have dropped.
    let home = dirs::home_dir();
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(h) = &home {
        candidates.push(h.join(".cargo/bin").join(command));
        candidates.push(h.join(".local/bin").join(command));
        candidates.push(h.join("go/bin").join(command));
    }
    candidates.push(std::path::PathBuf::from("/opt/homebrew/bin").join(command));
    candidates.push(std::path::PathBuf::from("/usr/local/bin").join(command));
    candidates.push(std::path::PathBuf::from("/usr/bin").join(command));
    for c in candidates {
        if is_executable(&c) {
            return c;
        }
    }
    as_path.to_path_buf()
}

/// Whether `path` is an existing file we can execute.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// On non-unix, treat any existing file as runnable (the OS handles the rest).
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Background task: read framed messages, correlate responses to pending
/// requests, and fold notifications (publishDiagnostics, $/progress) into state.
fn spawn_reader(inner: Arc<LspInner>, stdout: ChildStdout) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let msg = match read_frame(&mut reader).await {
                Ok(m) => m,
                Err(_) => {
                    // stdout closed → server died. Mark failed unless it was a
                    // clean shutdown (Ready stays Ready is fine; a crash mid-run
                    // is surfaced).
                    let mut h = inner.health.lock().unwrap();
                    if !matches!(*h, Health::Failed(_)) {
                        *h = Health::Failed("server exited".into());
                    }
                    break;
                }
            };

            // Response to one of our requests?
            if let Some(id) = msg.get("id").and_then(|v| v.as_i64()) {
                if let Some(tx) = inner.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(msg);
                    continue;
                }
                // Server-to-client request (e.g. workspace/configuration). We
                // don't implement these; ignore. (A robust client would reply.)
                continue;
            }

            // Notification.
            match msg.get("method").and_then(|v| v.as_str()) {
                Some("textDocument/publishDiagnostics") => {
                    let params = &msg["params"];
                    if let Some(uri) = params["uri"].as_str() {
                        let version = params["version"].as_i64().unwrap_or(0);
                        let diags = params["diagnostics"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default();
                        inner.diagnostics.lock().unwrap().insert(
                            uri.to_string(),
                            FileDiagnostics {
                                version,
                                diagnostics: diags,
                            },
                        );
                    }
                    // First diagnostics push implies the server is answering.
                    let mut h = inner.health.lock().unwrap();
                    if matches!(*h, Health::Indexing(_)) {
                        *h = Health::Ready;
                    }
                }
                Some("$/progress") => {
                    update_progress(&inner, &msg["params"]);
                }
                _ => {}
            }
        }
    });
}

/// Fold a `$/progress` notification into health. rust-analyzer emits a WorkDone
/// progress with a percentage during indexing and an `end` when done.
fn update_progress(inner: &Arc<LspInner>, params: &Value) {
    let value = &params["value"];
    match value["kind"].as_str() {
        Some("begin") | Some("report") => {
            let pct = value["percentage"].as_u64().map(|p| p as u8);
            let mut h = inner.health.lock().unwrap();
            // Only downgrade Ready->Indexing if a real indexing pass restarts.
            *h = Health::Indexing(pct);
        }
        Some("end") => {
            *inner.health.lock().unwrap() = Health::Ready;
        }
        _ => {}
    }
}

/// Read one Content-Length-framed JSON-RPC message from the stream.
async fn read_frame(reader: &mut BufReader<ChildStdout>) -> anyhow::Result<Value> {
    let mut content_length: Option<usize> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        loop {
            let mut b = [0u8; 1];
            let n = reader.read(&mut b).await?;
            if n == 0 {
                anyhow::bail!("server closed stdout");
            }
            line.push(b[0]);
            if line.ends_with(b"\r\n") {
                break;
            }
        }
        let text = std::str::from_utf8(&line)?.trim_end();
        if text.is_empty() {
            break;
        }
        if let Some(v) = text.strip_prefix("Content-Length:") {
            content_length = Some(v.trim().parse()?);
        }
    }
    let len = content_length.ok_or_else(|| anyhow::anyhow!("no Content-Length"))?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Map a filesystem path to a `file://` URI. Best-effort; assumes UTF-8 paths.
fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.starts_with('/') {
        format!("file://{}", s)
    } else {
        format!("file://{}", s) // relative shouldn't happen; servers get cwd too
    }
}

/// Guess an LSP languageId from a file extension.
fn language_id_for(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust",
        Some("ts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("js") => "javascript",
        Some("jsx") => "javascriptreact",
        Some("py") => "python",
        Some("go") => "go",
        Some("c") => "c",
        Some("cpp") | Some("cc") | Some("cxx") => "cpp",
        Some("h") | Some("hpp") => "cpp",
        Some(other) => return other.to_string(),
        None => "plaintext",
    }
    .to_string()
}

/// Manages every configured language server for a session: starts them in the
/// background, routes files to the right one, and exposes health for the UI.
pub struct LspManager {
    /// Config paired with a lazily-populated client (None until started/failed).
    servers: Vec<ServerSlot>,
    repo_root: std::path::PathBuf,
}

struct ServerSlot {
    cfg: LspServerConfig,
    client: Mutex<Option<LspClient>>,
    /// Health mirror for servers that failed before a client existed.
    health: Mutex<Health>,
}

impl LspManager {
    /// Build a manager and kick off background start for every server. Returns
    /// immediately — servers initialize concurrently without blocking startup.
    pub fn start(configs: &[LspServerConfig], repo_root: &Path) -> Arc<LspManager> {
        let servers = configs
            .iter()
            .map(|cfg| ServerSlot {
                cfg: cfg.clone(),
                client: Mutex::new(None),
                health: Mutex::new(Health::Starting),
            })
            .collect();
        let mgr = Arc::new(LspManager {
            servers,
            repo_root: repo_root.to_path_buf(),
        });

        for i in 0..mgr.servers.len() {
            let mgr2 = mgr.clone();
            tokio::spawn(async move {
                let slot = &mgr2.servers[i];
                match LspClient::start(&slot.cfg, &mgr2.repo_root).await {
                    Ok(client) => {
                        *slot.client.lock().unwrap() = Some(client);
                    }
                    Err(e) => {
                        *slot.health.lock().unwrap() = Health::Failed(e.to_string());
                    }
                }
            });
        }
        mgr
    }

    /// Find the client responsible for a file: the server whose `extensions`
    /// contains the file's extension and whose resolved root is the longest
    /// prefix of the file path (monorepo disambiguation).
    pub fn client_for(&self, path: &Path) -> Option<LspClient> {
        let ext = path.extension().and_then(|e| e.to_str())?;
        let mut best: Option<(usize, LspClient)> = None;
        for slot in &self.servers {
            if !slot.cfg.extensions.iter().any(|e| e == ext) {
                continue;
            }
            let root = self.repo_root.join(&slot.cfg.root);
            if !path.starts_with(&root) {
                continue;
            }
            let depth = root.components().count();
            let client = slot.client.lock().unwrap().clone();
            if let Some(client) = client {
                if best.as_ref().map(|(d, _)| depth > *d).unwrap_or(true) {
                    best = Some((depth, client));
                }
            }
        }
        best.map(|(_, c)| c)
    }

    /// A (name, health) snapshot for every configured server, for the status bar.
    pub fn statuses(&self) -> Vec<(String, Health)> {
        self.servers
            .iter()
            .map(|slot| {
                let health = slot
                    .client
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|c| c.health())
                    .unwrap_or_else(|| slot.health.lock().unwrap().clone());
                (slot.cfg.name.clone(), health)
            })
            .collect()
    }

    /// Look up a started client by its configured name (for workspace/symbol,
    /// which isn't file-scoped so can't be routed by path).
    pub fn client_by_name(&self, name: &str) -> Option<LspClient> {
        self.servers
            .iter()
            .find(|s| s.cfg.name == name)
            .and_then(|s| s.client.lock().unwrap().clone())
    }
}
