//! LSP over stdio uses Content-Length framing, unlike MCP's newline-delimited stdio.

mod command;
mod positions;
pub(crate) mod transport;
pub(crate) mod workspace_edit;
pub use positions::PositionEncoding;

use crate::core::config::LspServerConfig;
use crate::core::process::ManagedProcess;
use command::resolve_command;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use transport::RpcConnection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Health {
    /// Process spawned, initialize handshake in flight.
    Starting,
    /// Initialized; indexing progress from `$/progress`.
    Indexing(Option<u8>),
    Ready,
    Failed(String),
}

#[derive(Clone, Default)]
struct FileDiagnostics {
    diagnostics: Vec<Value>,
    generation: u64,
    version: Option<i64>,
}

#[derive(Clone)]
pub struct DocumentSnapshot {
    pub version: i64,
    pub text: String,
}

#[derive(Debug)]
pub struct DiagnosticPublication {
    pub diagnostics: Vec<Value>,
    pub version_confirmed: bool,
}

#[derive(Clone, Copy)]
pub struct SyncStamp {
    pub version: i64,
    pub generation: u64,
}

#[derive(Clone)]
pub struct LspClient {
    inner: Arc<LspInner>,
}

struct LspInner {
    name: String,
    connection: Arc<RpcConnection>,
    diagnostics: Mutex<HashMap<String, FileDiagnostics>>,
    generation: AtomicU64,
    documents: Mutex<HashMap<String, DocumentSnapshot>>,
    sync_lock: tokio::sync::Mutex<()>,
    pending_sync: Mutex<PendingSync>,
    encoding: Mutex<PositionEncoding>,
    health: Mutex<Health>,
    code_action_kinds: Mutex<Vec<String>>,
}

#[derive(Default)]
struct PendingSync {
    queue: VecDeque<SyncWork>,
    running: bool,
}

enum SyncWork {
    Deferred(PathBuf),
    Explicit {
        path: PathBuf,
        text: String,
        completion: tokio::sync::oneshot::Sender<anyhow::Result<SyncStamp>>,
    },
    Barrier(tokio::sync::oneshot::Sender<()>),
}

impl LspClient {
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn health(&self) -> Health {
        if self.inner.connection.is_closed() {
            return Health::Failed("connection closed".into());
        }
        self.inner.health.lock().unwrap().clone()
    }

    /// Returns after initialization; indexing continues in the background.
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

        let mut child = ManagedProcess::spawn(&mut command).map_err(|e| {
            anyhow::anyhow!(
                "failed to start LSP '{}' (command '{}'): {}. \
                 Ensure it's installed and its executable directory is on PATH.",
                cfg.name,
                cfg.command,
                e
            )
        })?;
        let stdin = child
            .take_stdin()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        let stdout = child
            .take_stdout()
            .ok_or_else(|| anyhow::anyhow!("no stdout"))?;

        let inner = Arc::new(LspInner {
            name: cfg.name.clone(),
            connection: RpcConnection::new(
                stdin,
                Some(child),
                true,
                std::time::Duration::from_secs(15),
            ),
            diagnostics: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            documents: Mutex::new(HashMap::new()),
            sync_lock: tokio::sync::Mutex::new(()),
            pending_sync: Mutex::new(PendingSync::default()),
            encoding: Mutex::new(PositionEncoding::Utf16),
            health: Mutex::new(Health::Starting),
            code_action_kinds: Mutex::new(Vec::new()),
        });

        spawn_reader(&inner, stdout);

        let client = LspClient { inner };

        #[allow(clippy::disallowed_names)]
        let init = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "general": { "positionEncodings": ["utf-8", "utf-16", "utf-32"] },
                "textDocument": {
                    "publishDiagnostics": { "relatedInformation": true, "versionSupport": true },
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
        if let Some(kinds) =
            init_result["capabilities"]["codeActionProvider"]["codeActionKinds"].as_array()
        {
            let kinds: Vec<String> = kinds
                .iter()
                .filter_map(|k| k.as_str().map(|s| s.to_string()))
                .collect();
            *client.inner.code_action_kinds.lock().unwrap() = kinds;
        }
        *client.inner.encoding.lock().unwrap() =
            PositionEncoding::negotiate(init_result["capabilities"]["positionEncoding"].as_str())?;
        client
            .inner
            .connection
            .notify("initialized", json!({}))
            .await?;
        *client.inner.health.lock().unwrap() = Health::Indexing(None);
        Ok(client)
    }

    /// Advertised kinds; an empty list does not rule out dynamic actions.
    pub fn code_action_kinds(&self) -> Vec<String> {
        self.inner.code_action_kinds.lock().unwrap().clone()
    }

    /// Call before document requests so the server sees the latest text and version.
    pub async fn sync_file(&self, path: &Path, text: &str) -> anyhow::Result<SyncStamp> {
        let (completion, result) = tokio::sync::oneshot::channel();
        self.enqueue_sync(SyncWork::Explicit {
            path: path.to_path_buf(),
            text: text.to_string(),
            completion,
        });
        tokio::time::timeout(std::time::Duration::from_secs(15), result)
            .await
            .map_err(|_| anyhow::anyhow!("document synchronization timed out"))?
            .map_err(|_| anyhow::anyhow!("document synchronization stopped"))?
    }

    pub(crate) fn defer_sync_file(&self, path: &Path) {
        self.enqueue_sync(SyncWork::Deferred(path.to_path_buf()));
    }

    async fn sync_barrier(&self) -> anyhow::Result<()> {
        let (completion, result) = tokio::sync::oneshot::channel();
        self.enqueue_sync(SyncWork::Barrier(completion));
        tokio::time::timeout(std::time::Duration::from_secs(15), result)
            .await
            .map_err(|_| anyhow::anyhow!("document synchronization timed out"))?
            .map_err(|_| anyhow::anyhow!("document synchronization stopped"))
    }

    fn enqueue_sync(&self, sync: SyncWork) {
        let mut pending = self.inner.pending_sync.lock().unwrap();
        if let SyncWork::Deferred(path) | SyncWork::Explicit { path, .. } = &sync {
            let start = pending
                .queue
                .iter()
                .rposition(|queued| !matches!(queued, SyncWork::Deferred(_)))
                .map_or(0, |index| index + 1);
            let mut index = 0;
            pending.queue.retain(|queued| {
                let keep = index < start
                    || !matches!(queued, SyncWork::Deferred(queued_path) if queued_path == path);
                index += 1;
                keep
            });
        }
        pending.queue.push_back(sync);
        if pending.running {
            return;
        }
        pending.running = true;
        let weak = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            while let Some(inner) = weak.upgrade() {
                let client = LspClient { inner };
                let sync = {
                    let mut pending = client.inner.pending_sync.lock().unwrap();
                    let Some(sync) = pending.queue.pop_front() else {
                        pending.running = false;
                        return;
                    };
                    sync
                };
                match sync {
                    SyncWork::Deferred(path) => {
                        let _ = client.send_document_sync(&path, None).await;
                    }
                    SyncWork::Explicit {
                        path,
                        text,
                        completion,
                    } => {
                        let result = client.send_document_sync(&path, Some(&text)).await;
                        let _ = completion.send(result);
                    }
                    SyncWork::Barrier(completion) => {
                        let _ = completion.send(());
                    }
                }
            }
        });
    }

    async fn send_document_sync(
        &self,
        path: &Path,
        text: Option<&str>,
    ) -> anyhow::Result<SyncStamp> {
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            let _sync = self.inner.sync_lock.lock().await;
            let disk_text;
            let text = match text {
                Some(text) => text,
                None => {
                    disk_text = crate::tools::filesystem::read_regular(path)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
                    &disk_text
                }
            };
            let uri = path_to_uri(path);
            let version = self.document_snapshot(path).map_or(1, |doc| doc.version + 1);
            let generation = self.inner.generation.load(Ordering::Acquire);
            let (method, params) = if version == 1 {
                ("textDocument/didOpen", json!({"textDocument": {
                    "uri": uri, "languageId": language_id_for(path), "version": version, "text": text
                }}))
            } else {
                ("textDocument/didChange", json!({
                    "textDocument": {"uri":uri,"version":version},
                    "contentChanges":[{"text":text}]
                }))
            };
            self.inner.connection.notify(method, params).await?;
            self.inner.documents.lock().unwrap().insert(uri, DocumentSnapshot {
                version, text: text.to_string(),
            });
            Ok(SyncStamp { version, generation })
        }).await.map_err(|_| anyhow::anyhow!("document synchronization timed out"))?
    }

    pub(crate) async fn lock_documents(&self) -> anyhow::Result<tokio::sync::MutexGuard<'_, ()>> {
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.inner.sync_lock.lock(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("document lock timed out"))
    }

    pub fn document_snapshot(&self, path: &Path) -> Option<DocumentSnapshot> {
        self.inner
            .documents
            .lock()
            .unwrap()
            .get(&path_to_uri(path))
            .cloned()
    }

    pub fn document_snapshots(&self) -> HashMap<String, DocumentSnapshot> {
        self.inner.documents.lock().unwrap().clone()
    }

    pub fn position_encoding(&self) -> PositionEncoding {
        *self.inner.encoding.lock().unwrap()
    }

    pub async fn fresh_diagnostics(
        &self,
        path: &Path,
        stamp: SyncStamp,
        timeout: std::time::Duration,
    ) -> anyhow::Result<DiagnosticPublication> {
        let uri = path_to_uri(path);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            anyhow::ensure!(
                !self.inner.connection.is_closed(),
                "language server disconnected before fresh diagnostics"
            );
            anyhow::ensure!(
                self.document_snapshot(path)
                    .is_some_and(|doc| doc.version == stamp.version),
                "document changed while waiting for diagnostics"
            );
            if let Some(publication) = self.inner.diagnostics.lock().unwrap().get(&uri) {
                if publication.generation > stamp.generation
                    && publication
                        .version
                        .is_none_or(|version| version == stamp.version)
                {
                    return Ok(DiagnosticPublication {
                        diagnostics: publication.diagnostics.clone(),
                        version_confirmed: publication.version.is_some(),
                    });
                }
            }
            if tokio::time::Instant::now() >= deadline {
                anyhow::bail!("timed out waiting for fresh diagnostics; cached diagnostics may be stale (document version {})", stamp.version);
            }
            tokio::time::sleep_until(
                (tokio::time::Instant::now() + std::time::Duration::from_millis(20)).min(deadline),
            )
            .await;
        }
    }

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

    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.sync_barrier().await?;
        self.inner.connection.request(method, params).await
    }
}

/// Correlate replies and update diagnostic/progress state without owning the client.
fn spawn_reader(inner: &Arc<LspInner>, stdout: impl AsyncRead + Send + Unpin + 'static) {
    let weak = Arc::downgrade(inner);
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        loop {
            let msg = match read_frame(&mut reader).await {
                Ok(m) => m,
                Err(_) => {
                    if let Some(inner) = weak.upgrade() {
                        inner.connection.disconnect();
                        *inner.health.lock().unwrap() = Health::Failed("server exited".into());
                    }
                    break;
                }
            };

            let Some(inner) = weak.upgrade() else { break };
            if msg.get("method").is_none() {
                inner.connection.receive(msg);
                continue;
            }

            match msg.get("method").and_then(|v| v.as_str()) {
                Some("textDocument/publishDiagnostics") => {
                    let params = &msg["params"];
                    if let Some(uri) = params["uri"].as_str() {
                        let Some(diags) = params["diagnostics"].as_array().cloned() else {
                            continue;
                        };
                        if params
                            .get("version")
                            .is_some_and(|version| !version.is_null() && version.as_i64().is_none())
                        {
                            continue;
                        }
                        let generation = inner.generation.fetch_add(1, Ordering::AcqRel) + 1;
                        inner.diagnostics.lock().unwrap().insert(
                            uri.to_string(),
                            FileDiagnostics {
                                diagnostics: diags,
                                generation,
                                version: params["version"].as_i64(),
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

fn update_progress(inner: &Arc<LspInner>, params: &Value) {
    let value = &params["value"];
    match value["kind"].as_str() {
        Some("begin") | Some("report") => {
            let pct = value["percentage"].as_u64().map(|p| p as u8);
            let mut h = inner.health.lock().unwrap();
            *h = Health::Indexing(pct);
        }
        Some("end") => {
            *inner.health.lock().unwrap() = Health::Ready;
        }
        _ => {}
    }
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> anyhow::Result<Value> {
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
            anyhow::ensure!(line.len() <= 8192, "LSP header too large");
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
    anyhow::ensure!(len <= 16 * 1024 * 1024, "LSP frame too large");
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Requires an absolute path; URI escaping is handled by `Url`.
pub fn path_to_uri(path: &Path) -> String {
    reqwest::Url::from_file_path(path)
        .expect("LSP paths must be absolute")
        .to_string()
}

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

pub struct LspManager {
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
    /// Returns immediately; servers initialize concurrently.
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

    /// Route by extension and deepest matching root among started clients.
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

    /// Name-based routing for requests without a file, such as `workspace/symbol`.
    pub fn client_by_name(&self, name: &str) -> Option<LspClient> {
        self.servers
            .iter()
            .find(|s| s.cfg.name == name)
            .and_then(|s| s.client.lock().unwrap().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use crate::tools::builtin::{ReadFileTool, WriteFileTool};
    use crate::tools::edit::{EditFileTool, MultiEditTool};
    use crate::tools::lsp_actions::RenameSymbolTool;
    use crate::tools::registry::{Tool, ToolContext};
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    fn fixture_client() -> (LspClient, tokio::io::DuplexStream) {
        let (pipe, server) = tokio::io::duplex(64 * 1024);
        let (reader, writer) = tokio::io::split(pipe);
        let inner = Arc::new(LspInner {
            name: "fixture".into(),
            connection: RpcConnection::new(writer, None, true, Duration::from_secs(1)),
            diagnostics: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            documents: Mutex::new(HashMap::new()),
            sync_lock: tokio::sync::Mutex::new(()),
            pending_sync: Mutex::new(PendingSync::default()),
            encoding: Mutex::new(PositionEncoding::Utf16),
            health: Mutex::new(Health::Ready),
            code_action_kinds: Mutex::new(Vec::new()),
        });
        spawn_reader(&inner, reader);
        (LspClient { inner }, server)
    }

    async fn send_frame(writer: &mut (impl tokio::io::AsyncWrite + Unpin), message: Value) {
        let body = message.to_string();
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n{}", body.len(), body).as_bytes())
            .await
            .unwrap();
    }

    async fn receive_frame(reader: &mut BufReader<impl AsyncRead + Unpin>) -> Value {
        tokio::time::timeout(Duration::from_secs(2), read_frame(reader))
            .await
            .expect("expected a document notification")
            .unwrap()
    }

    #[tokio::test]
    async fn reader_does_not_keep_client_alive() {
        let (client, _server) = fixture_client();
        let weak = Arc::downgrade(&client.inner);
        drop(client);
        assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn eof_releases_lsp_request_and_marks_failed() {
        let (client, server) = fixture_client();
        let mut request = Box::pin(client.request("fixture", Value::Null));
        assert!(futures::poll!(&mut request).is_pending());
        drop(server);
        let result = tokio::time::timeout(Duration::from_millis(200), request)
            .await
            .unwrap();
        assert!(result.unwrap_err().to_string().contains("closed"));
        assert!(matches!(client.health(), Health::Failed(_)));
    }

    #[tokio::test]
    async fn diagnostic_freshness_uses_publication_and_document_version() {
        let (client, _server) = fixture_client();
        let directory = TestDir::new();
        let file = directory.join("fixture.rs");
        let uri = path_to_uri(&file);
        let stamp = client.sync_file(&file, "x").await.unwrap();
        for (generation, version) in [
            (stamp.generation, Some(stamp.version)),
            (stamp.generation + 1, Some(stamp.version - 1)),
            (stamp.generation + 2, Some(stamp.version + 1)),
        ] {
            client.inner.diagnostics.lock().unwrap().insert(
                uri.clone(),
                FileDiagnostics {
                    diagnostics: vec![json!({"message":"stale"})],
                    generation,
                    version,
                },
            );
            assert!(client
                .fresh_diagnostics(&file, stamp, Duration::ZERO)
                .await
                .unwrap_err()
                .to_string()
                .contains("stale"));
        }
        client.inner.diagnostics.lock().unwrap().insert(
            uri.clone(),
            FileDiagnostics {
                diagnostics: vec![json!({"message":"new same count"})],
                generation: stamp.generation + 3,
                version: Some(stamp.version),
            },
        );
        assert_eq!(
            client
                .fresh_diagnostics(&file, stamp, Duration::ZERO)
                .await
                .unwrap()
                .diagnostics[0]["message"],
            "new same count"
        );
        client.inner.diagnostics.lock().unwrap().insert(
            uri,
            FileDiagnostics {
                diagnostics: vec![],
                generation: stamp.generation + 4,
                version: Some(stamp.version),
            },
        );
        assert!(client
            .fresh_diagnostics(&file, stamp, Duration::ZERO)
            .await
            .unwrap()
            .diagnostics
            .is_empty());
        client.inner.connection.disconnect();
        assert!(client
            .fresh_diagnostics(&file, stamp, Duration::ZERO)
            .await
            .unwrap_err()
            .to_string()
            .contains("disconnected"));
    }

    #[tokio::test]
    async fn fresh_empty_publication_is_not_a_timeout() {
        let (client, mut server) = fixture_client();
        let directory = TestDir::new();
        let file = directory.join("fixture.rs");
        let stamp = client.sync_file(&file, "x").await.unwrap();
        send_frame(
            &mut server,
            json!({"method":"textDocument/publishDiagnostics","params":{
                "uri":path_to_uri(&file),"version":stamp.version,"diagnostics":[]
            }}),
        )
        .await;
        assert!(client
            .fresh_diagnostics(&file, stamp, Duration::from_secs(1))
            .await
            .unwrap()
            .diagnostics
            .is_empty());
    }

    #[tokio::test]
    async fn edits_sync_documents_without_returning_automatic_diagnostics() {
        let (directory, client, server, ctx) = fixture_context();
        let file = directory.join("sample.rs");
        let mut reader = BufReader::new(server);
        client.inner.diagnostics.lock().unwrap().insert(path_to_uri(&file), FileDiagnostics {
            diagnostics: vec![json!({"severity":1,"message":"cached diagnostic","range":{"start":{"line":0,"character":0}}})],
            generation: 0,
            version: Some(0),
        });
        let written = WriteFileTool
            .execute(
                json!({"path":"sample.rs","content":"fn sample() { let n = 1; }"}),
                &ctx,
            )
            .await
            .unwrap();
        let opened = receive_frame(&mut reader).await;
        ReadFileTool
            .execute(json!({"path":"sample.rs"}), &ctx)
            .await
            .unwrap();
        let edited = EditFileTool
            .execute(
                json!({"path":"sample.rs","old_string":"n = 1","new_string":"n = 2"}),
                &ctx,
            )
            .await
            .unwrap();
        let changed = receive_frame(&mut reader).await;
        let multi = MultiEditTool
            .execute(
                json!({"path":"sample.rs","edits":[{"old_string":"n = 2","new_string":"n = 3"}]}),
                &ctx,
            )
            .await
            .unwrap();
        for result in [&written, &edited, &multi] {
            assert!(!result.as_text().unwrap().contains("<diagnostics>"));
            assert!(!result.as_text().unwrap().contains("cached diagnostic"));
        }
        let changed_again = receive_frame(&mut reader).await;
        for (message, (version, number)) in
            [opened, changed, changed_again]
                .into_iter()
                .zip([(1, 1), (2, 2), (3, 3)])
        {
            assert_eq!(message["params"]["textDocument"]["version"], version);
            let text = if version == 1 {
                assert_eq!(message["method"], "textDocument/didOpen");
                &message["params"]["textDocument"]["text"]
            } else {
                assert_eq!(message["method"], "textDocument/didChange");
                &message["params"]["contentChanges"][0]["text"]
            };
            assert!(text.as_str().unwrap().contains(&format!("n = {number}")));
        }
        assert_eq!(client.diagnostics_for(&file).len(), 1);
        client.inner.connection.disconnect();
    }

    #[tokio::test]
    async fn committed_writes_and_edits_return_success_while_sync_is_stalled() {
        let (directory, client, _server, ctx) = fixture_context();
        let file = directory.join("sample.rs");
        let lock = client.lock_documents().await.unwrap();
        let results = tokio::time::timeout(Duration::from_secs(1), async {
            let written = WriteFileTool
                .execute(json!({"path":"sample.rs","content":"one"}), &ctx)
                .await
                .unwrap();
            let edited = EditFileTool
                .execute(
                    json!({"path":"sample.rs","old_string":"one","new_string":"two"}),
                    &ctx,
                )
                .await
                .unwrap();
            let multi = MultiEditTool
                .execute(
                    json!({"path":"sample.rs","edits":[{"old_string":"two","new_string":"three"}]}),
                    &ctx,
                )
                .await
                .unwrap();
            [written, edited, multi]
        })
        .await
        .expect("committed mutations must not wait for document synchronization");
        assert!(results[0].as_text().unwrap().starts_with("wrote "));
        assert!(results[1]
            .as_text()
            .unwrap()
            .starts_with("edited sample.rs"));
        assert!(results[2]
            .as_text()
            .unwrap()
            .starts_with("edited sample.rs"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "three");
        assert!(ctx.files.fully_read(&file.to_string_lossy(), "three"));
        assert!(client.document_snapshot(&file).is_none());
        drop(lock);
        client.sync_file(&file, "three").await.unwrap();
        assert_eq!(client.document_snapshot(&file).unwrap().text, "three");
    }

    #[tokio::test]
    async fn deferred_edits_coalesce_without_crossing_explicit_sync_order() {
        let (client, server) = fixture_client();
        let directory = TestDir::new();
        let file = directory.join("sample.rs");
        let lock = client.lock_documents().await.unwrap();
        directory.write("sample.rs", "in flight");
        client.defer_sync_file(&file);
        tokio::task::yield_now().await;
        assert!(client.inner.pending_sync.lock().unwrap().queue.is_empty());
        directory.write("sample.rs", "superseded edit");
        client.defer_sync_file(&file);
        let mut explicit = Box::pin(client.sync_file(&file, "explicit"));
        assert!(futures::poll!(&mut explicit).is_pending());
        directory.write("sample.rs", "later edit");
        client.defer_sync_file(&file);
        directory.write("sample.rs", "latest edit");
        client.defer_sync_file(&file);
        assert_eq!(client.inner.pending_sync.lock().unwrap().queue.len(), 2);
        drop(lock);
        let stamp = explicit.await.unwrap();
        client.sync_barrier().await.unwrap();
        let snapshot = client.document_snapshot(&file).unwrap();
        assert_eq!(snapshot.text, "latest edit");
        assert_eq!(snapshot.version, stamp.version + 1);
        let mut reader = BufReader::new(server);
        for (version, text) in [(1, "latest edit"), (2, "explicit"), (3, "latest edit")] {
            let message = receive_frame(&mut reader).await;
            assert_eq!(message["params"]["textDocument"]["version"], version);
            let actual = if version == 1 {
                &message["params"]["textDocument"]["text"]
            } else {
                &message["params"]["contentChanges"][0]["text"]
            };
            assert_eq!(actual, text);
        }
    }

    #[tokio::test]
    async fn reversed_commit_enqueue_order_syncs_current_disk() {
        use crate::tools::filesystem::{prepare_batch, WriteRequest};

        for drain_newer in [false, true] {
            let (directory, client, server, ctx) = fixture_context();
            let file = directory.join("a.rs");
            let mut reader = BufReader::new(server);
            client.sync_file(&file, "old").await.unwrap();
            assert_eq!(
                receive_frame(&mut reader).await["method"],
                "textDocument/didOpen"
            );

            let earlier = prepare_batch(
                &ctx,
                vec![WriteRequest {
                    path: PathBuf::from("a.rs"),
                    resolved: file.clone(),
                    expected: Some("old".into()),
                    content: "first commit".into(),
                }],
            )
            .await
            .unwrap()
            .commit()
            .unwrap();
            ctx.files.record_read(&file.to_string_lossy());
            EditFileTool
                .execute(
                    json!({"path":"a.rs","old_string":"first commit","new_string":"second commit"}),
                    &ctx,
                )
                .await
                .unwrap();
            if drain_newer {
                client.sync_barrier().await.unwrap();
                let changed = receive_frame(&mut reader).await;
                assert_eq!(
                    changed["params"]["contentChanges"][0]["text"],
                    "second commit"
                );
            }

            crate::tools::lsp::sync_after_edit(&ctx.lsp, &earlier[0]);
            client.sync_barrier().await.unwrap();
            let changed = receive_frame(&mut reader).await;
            assert_eq!(changed["method"], "textDocument/didChange");
            assert_eq!(
                changed["params"]["contentChanges"][0]["text"],
                "second commit"
            );
            assert_eq!(
                changed["params"]["textDocument"]["version"],
                if drain_newer { 3 } else { 2 }
            );
            let disk = std::fs::read_to_string(&file).unwrap();
            assert_eq!(disk, "second commit");
            assert_eq!(client.document_snapshot(&file).unwrap().text, disk);
        }
    }

    #[tokio::test]
    async fn workspace_symbols_waits_for_pending_changes_even_with_later_enqueues() {
        let (directory, client, server, ctx) = fixture_context();
        let file = directory.join("a.rs");
        let mut reader = BufReader::new(server);
        client.sync_file(&file, "old").await.unwrap();
        assert_eq!(
            receive_frame(&mut reader).await["method"],
            "textDocument/didOpen"
        );
        let lock = client.lock_documents().await.unwrap();
        client.defer_sync_file(&directory.join("b.rs"));
        tokio::task::yield_now().await;
        assert!(client.inner.pending_sync.lock().unwrap().queue.is_empty());
        ctx.files.record_read(&file.to_string_lossy());
        EditFileTool
            .execute(
                json!({"path":"a.rs","old_string":"old","new_string":"new"}),
                &ctx,
            )
            .await
            .unwrap();
        let tool = crate::tools::lsp::LspTool::new(ctx.lsp.clone().unwrap());
        let mut query =
            Box::pin(tool.execute(json!({"operation":"workspace_symbols","query":"new"}), &ctx));
        assert!(futures::poll!(&mut query).is_pending());
        client.defer_sync_file(&file);
        {
            let pending = client.inner.pending_sync.lock().unwrap();
            assert_eq!(pending.queue.len(), 3);
            assert!(matches!(&pending.queue[0], SyncWork::Deferred(path) if path == &file));
            assert!(matches!(&pending.queue[1], SyncWork::Barrier(_)));
            assert!(matches!(&pending.queue[2], SyncWork::Deferred(path) if path == &file));
        }
        drop(lock);
        let server = async {
            assert_eq!(
                receive_frame(&mut reader).await["method"],
                "textDocument/didOpen"
            );
            let changed = receive_frame(&mut reader).await;
            assert_eq!(changed["method"], "textDocument/didChange");
            assert_eq!(changed["params"]["textDocument"]["uri"], path_to_uri(&file));
            assert_eq!(changed["params"]["contentChanges"][0]["text"], "new");
            let mut request = receive_frame(&mut reader).await;
            if request["method"] == "textDocument/didChange" {
                assert_eq!(request["params"]["contentChanges"][0]["text"], "new");
                request = receive_frame(&mut reader).await;
            }
            assert_eq!(request["method"], "workspace/symbol");
            assert_eq!(request["params"]["query"], "new");
            send_frame(reader.get_mut(), json!({"id":request["id"],"result":[]})).await;
        };
        let (result, ()) = tokio::join!(query, server);
        assert_eq!(result.unwrap().as_text().unwrap(), "No matching symbols.");
    }

    #[tokio::test]
    async fn cancelling_explicit_sync_does_not_interrupt_an_in_flight_notification() {
        let (client, server) = fixture_client();
        let directory = TestDir::new();
        let file = directory.join("sample.rs");
        let text = "x".repeat(128 * 1024);
        let mut explicit = Box::pin(client.sync_file(&file, &text));
        assert!(futures::poll!(&mut explicit).is_pending());
        tokio::task::yield_now().await;
        assert!(client.document_snapshot(&file).is_none());
        drop(explicit);
        let mut reader = BufReader::new(server);
        let message = receive_frame(&mut reader).await;
        assert_eq!(message["params"]["textDocument"]["text"], text);
        client
            .sync_file(&directory.join("barrier.rs"), "barrier")
            .await
            .unwrap();
        assert_eq!(client.document_snapshot(&file).unwrap().text, text);
        assert!(!client.inner.connection.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_sync_keeps_its_fifteen_second_deadline() {
        let (client, _server) = fixture_client();
        let directory = TestDir::new();
        let _lock = client.lock_documents().await.unwrap();
        let started = tokio::time::Instant::now();
        let error = client
            .sync_file(&directory.join("sample.rs"), "text")
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("timed out"));
        assert_eq!(started.elapsed(), Duration::from_secs(15));
        assert!(!client.inner.connection.is_closed());
    }

    #[tokio::test]
    async fn unstarted_sync_worker_does_not_keep_a_dropped_client_alive() {
        let (client, _server) = fixture_client();
        let directory = TestDir::new();
        let weak = Arc::downgrade(&client.inner);
        client.defer_sync_file(&directory.join("sample.rs"));
        drop(client);
        assert!(weak.upgrade().is_none());
        tokio::task::yield_now().await;
    }

    fn fixture_context() -> (TestDir, LspClient, tokio::io::DuplexStream, ToolContext) {
        let dir = TestDir::new();
        for file in ["a.rs", "b.rs", "c.rs"] {
            dir.write(file, "old");
        }
        let (client, server) = fixture_client();
        let manager = Arc::new(LspManager {
            servers: vec![ServerSlot {
                cfg: LspServerConfig {
                    name: "fixture".into(),
                    command: "unused-fixture".into(),
                    args: vec![],
                    extensions: vec!["rs".into()],
                    root: ".".into(),
                },
                client: Mutex::new(Some(client.clone())),
                health: Mutex::new(Health::Ready),
            }],
            repo_root: dir.path().to_path_buf(),
        });
        let ctx = ToolContext {
            cwd: dir.to_string_lossy().into_owned(),
            files: Arc::new(crate::tools::file_tracker::FileTracker::new()),
            todos: Arc::new(crate::tools::todo::TodoStore::new()),
            jobs: crate::tools::jobs::JobRegistry::new(),
            user_asker: None,
            lsp: Some(manager),
            coord: None,
            permissions: None,
        };
        (dir, client, server, ctx)
    }

    async fn rename_scenario(scenario: &'static str) {
        let (directory, client, server, ctx) = fixture_context();
        let root = directory.0.clone();
        let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            while let Ok(message) = read_frame(&mut server).await {
                if message["method"] != "textDocument/rename" {
                    continue;
                }
                let count = seen.fetch_add(1, Ordering::SeqCst) + 1;
                let second = if scenario == "new_target" && count == 2 {
                    "c.rs"
                } else {
                    "b.rs"
                };
                if scenario == "stale_disk" && count == 2 {
                    std::fs::write(root.join("b.rs"), "concurrent user edit").unwrap();
                }
                let replacement = |end| json!({"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":end}},"newText":"new"});
                let result = if scenario == "resource" {
                    json!({"documentChanges":[{"kind":"delete","uri":path_to_uri(&root.join("b.rs"))}]})
                } else {
                    json!({"documentChanges":[
                        {"textDocument":{"uri":path_to_uri(&root.join("a.rs")),"version":if scenario=="stale_version" {0} else {1}},"edits":[replacement(3)]},
                        {"textDocument":{"uri":path_to_uri(&root.join(second)),"version":if count==1 || scenario=="new_target" {Value::Null} else {json!(1)}},"edits":[replacement(if scenario=="bad_range" {30} else {3})]}
                    ]})
                };
                send_frame(
                    server.get_mut(),
                    json!({"id":message["id"],"result":result}),
                )
                .await;
                if scenario == "closed_after_reply" && count == 2 {
                    break;
                }
            }
        });
        let tool = RenameSymbolTool::new(ctx.lsp.clone().unwrap());
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            tool.execute(
                json!({
                    "filePath":"a.rs","line":1,"character":1,"newName":"new"
                }),
                &ctx,
            ),
        )
        .await
        .unwrap();
        if matches!(scenario, "success" | "closed_after_reply") {
            assert!(outcome
                .unwrap()
                .as_text()
                .unwrap()
                .contains("changed 2 file(s)"));
            if scenario == "success" {
                client
                    .sync_file(&directory.join("barrier.rs"), "barrier")
                    .await
                    .unwrap();
            }
            for file in ["a.rs", "b.rs"] {
                let path = directory.0.join(file);
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
                if scenario == "success" {
                    assert_eq!(client.document_snapshot(&path).unwrap().text, "new");
                }
            }
        } else {
            assert!(outcome.is_err(), "{scenario} must not report success");
            assert_eq!(
                std::fs::read_to_string(directory.0.join("a.rs")).unwrap(),
                "old"
            );
            assert_eq!(
                std::fs::read_to_string(directory.0.join("b.rs")).unwrap(),
                if scenario == "stale_disk" {
                    "concurrent user edit"
                } else {
                    "old"
                }
            );
        }
        assert!(requests.load(Ordering::SeqCst) <= 2);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn unversioned_empty_diagnostics_never_claim_clean() {
        let (_directory, _client, server, ctx) = fixture_context();
        let task = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            while let Ok(message) = read_frame(&mut server).await {
                if message["method"] == "textDocument/didOpen" {
                    send_frame(server.get_mut(), json!({
                        "method":"textDocument/publishDiagnostics",
                        "params":{"uri":message["params"]["textDocument"]["uri"],"diagnostics":[]}
                    })).await;
                }
            }
        });
        let tool = crate::tools::lsp::LspTool::new(ctx.lsp.clone().unwrap());
        let output = tokio::time::timeout(
            Duration::from_secs(1),
            tool.execute(json!({"operation":"diagnostics","filePath":"a.rs"}), &ctx),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(output.as_text().unwrap().contains("freshness unconfirmed"));
        assert!(!output.as_text().unwrap().contains("No diagnostics"));
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn unopened_targets_use_one_bounded_preparation_pass() {
        rename_scenario("success").await;
    }

    #[tokio::test]
    async fn committed_rename_reports_success_when_post_edit_sync_disconnects() {
        rename_scenario("closed_after_reply").await;
    }

    #[tokio::test]
    async fn stale_files_versions_ranges_and_new_targets_fail_before_writes() {
        for scenario in [
            "stale_disk",
            "stale_version",
            "bad_range",
            "new_target",
            "resource",
        ] {
            rename_scenario(scenario).await;
        }
    }

    #[tokio::test]
    async fn framed_parser_reads_unicode_and_back_to_back_messages_without_overread() {
        let first = json!({"id":1,"result":"é😀\ntext"});
        let second = json!({"id":2,"error":{"code":-32601,"message":"unknown"}});
        let body = first.to_string();
        let next = second.to_string();
        let bytes = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}Content-Length: {}\r\n\r\n{}",
            body.len(), body, next.len(), next
        );
        let mut reader = BufReader::with_capacity(1, bytes.as_bytes());
        assert_eq!(read_frame(&mut reader).await.unwrap(), first);
        assert_eq!(read_frame(&mut reader).await.unwrap(), second);
        assert_eq!(
            read_frame(&mut reader).await.unwrap_err().to_string(),
            "server closed stdout"
        );
    }

    #[tokio::test]
    async fn framed_parser_distinguishes_header_body_and_json_failures() {
        for (bytes, expected) in [
            (
                "Content-Type: application/json\r\n\r\n{}",
                "no Content-Length",
            ),
            ("Content-Length: 16777217\r\n\r\n", "LSP frame too large"),
            ("Content-Length: 2\r", "server closed stdout"),
        ] {
            let error = read_frame(&mut BufReader::new(bytes.as_bytes()))
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), expected);
        }
        let header = format!("{}\r\n", "x".repeat(8193));
        assert_eq!(
            read_frame(&mut BufReader::new(header.as_bytes()))
                .await
                .unwrap_err()
                .to_string(),
            "LSP header too large"
        );
        for length in ["-1", "two"] {
            let bytes = format!("Content-Length: {length}\r\n\r\n{{}}");
            let error = read_frame(&mut BufReader::new(bytes.as_bytes()))
                .await
                .unwrap_err();
            assert!(error.downcast_ref::<std::num::ParseIntError>().is_some());
        }
        let error = read_frame(&mut BufReader::new(
            b"Content-Length: 5\r\n\r\n{}".as_slice(),
        ))
        .await
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        let error = read_frame(&mut BufReader::new(
            b"Content-Length: 1\r\n\r\n{".as_slice(),
        ))
        .await
        .unwrap_err();
        assert!(error.downcast_ref::<serde_json::Error>().is_some());
    }

    #[tokio::test]
    async fn failed_sync_does_not_advance_the_document_snapshot() {
        let (client, _server) = fixture_client();
        let directory = TestDir::new();
        let path = directory.join("sample.rs");
        let stamp = client.sync_file(&path, "original").await.unwrap();
        client.inner.connection.disconnect();
        assert!(client.sync_file(&path, "replacement").await.is_err());
        let snapshot = client.document_snapshot(&path).unwrap();
        assert_eq!(snapshot.version, stamp.version);
        assert_eq!(snapshot.text, "original");
        assert!(client
            .sync_file(&directory.join("new.rs"), "new")
            .await
            .is_err());
        assert!(client
            .document_snapshot(&directory.join("new.rs"))
            .is_none());
    }

    #[tokio::test]
    async fn changed_document_invalidates_diagnostic_stamp_even_with_matching_publication() {
        let (client, _server) = fixture_client();
        let directory = TestDir::new();
        let path = directory.join("sample.rs");
        let stamp = client.sync_file(&path, "original").await.unwrap();
        client.inner.diagnostics.lock().unwrap().insert(
            path_to_uri(&path),
            FileDiagnostics {
                diagnostics: vec![],
                generation: stamp.generation + 1,
                version: Some(stamp.version),
            },
        );
        client.sync_file(&path, "changed").await.unwrap();
        assert_eq!(
            client
                .fresh_diagnostics(&path, stamp, Duration::ZERO)
                .await
                .unwrap_err()
                .to_string(),
            "document changed while waiting for diagnostics"
        );
    }
}
