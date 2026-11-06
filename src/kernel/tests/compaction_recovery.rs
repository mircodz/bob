#![cfg(unix)]

use anyhow::{bail, ensure, Result};
use async_trait::async_trait;
use bob_kernel::*;
use bob_types::{Completion, ContentBlock, Message, ReasoningEffort, Role, StopReason, Usage};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command as ProcessCommand;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(15);
const FIXTURE: &str = "compaction_summary_sigkill_fixture";
const FIXTURE_DIR: &str = "BOB_COMPACTION_CRASH_DIR";
const FIXTURE_NONCE: &str = "BOB_COMPACTION_CRASH_NONCE";
const ROOT_MODEL: &str = "compaction-test-model";
const BARRIER_MODEL: &str = "scheduling-barrier-model";
const TASK: &str = "Record the approved storage decision; do not perform any external effects.";
const DECISION: &str =
    "DECISION: retain the original transcript and atomically replace working context.";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("bob-compaction-crash-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::create_dir(path.join("workspace")).unwrap();
        Self(path)
    }

    fn db(&self) -> PathBuf {
        self.0.join("runtime.sqlite")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    timeout(WAIT, future)
        .await
        .expect("compaction recovery watchdog expired")
}

fn original_body() -> String {
    format!(
        "{DECISION}\n{}\nEND OF ORIGINAL ASSISTANT ENTRY",
        "Synthetic retained detail.\n".repeat(1900)
    )
}

fn answer(text: String) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text }],
        },
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

fn assert_summary(request: &RequestSpec) {
    assert_eq!(request.model.id, ROOT_MODEL);
    assert!(request.system.starts_with("Summarize transcript data"));
    assert!(request.tools.is_empty());
    assert_eq!(request.messages.len(), 1);
    assert!(request.messages[0]
        .text()
        .contains("Next transcript fragment:"));
}

struct Dispatch {
    request: RequestSpec,
    finish: oneshot::Sender<Completion>,
}

struct SyntheticExecutor {
    crash: Option<(PathBuf, String)>,
    dispatch: mpsc::Sender<Dispatch>,
    requests: Mutex<Vec<RequestSpec>>,
}

impl SyntheticExecutor {
    fn new(crash: Option<(PathBuf, String)>) -> (Arc<Self>, mpsc::Receiver<Dispatch>) {
        let (dispatch, receiver) = mpsc::channel(4);
        (
            Arc::new(Self {
                crash,
                dispatch,
                requests: Mutex::new(Vec::new()),
            }),
            receiver,
        )
    }

    fn root_requests(&self) -> Vec<RequestSpec> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.model.id == ROOT_MODEL)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl Executor for SyntheticExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        [ROOT_MODEL, BARRIER_MODEL]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                context_window: 32_768,
            })
            .collect()
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }

    async fn generate(
        &self,
        request: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _progress: ModelProgress,
    ) -> Result<Completion> {
        self.requests.lock().unwrap().push(request.clone());
        if request.model.id == BARRIER_MODEL {
            return Ok(answer("scheduling barrier completed".into()));
        }
        if let Some((dir, nonce)) = &self.crash {
            if self.root_requests().len() == 1 {
                ensure!(
                    !request.system.starts_with("Summarize transcript data"),
                    "seed was not a normal completion"
                );
                return Ok(answer(original_body()));
            }
            assert_eq!(self.root_requests().len(), 2);
            assert_summary(&request);
            ensure!(
                request.messages[0].text().contains(&original_body()),
                "fixture must reach a real summary of the original body"
            );
            std::fs::write(
                dir.join("summary-request.json"),
                serde_json::to_vec(&request)?,
            )?;
            {
                let mut stdout = std::io::stdout().lock();
                writeln!(stdout, "\nCOMPACTION_READY {nonce} {}", std::process::id())?;
                stdout.flush()?;
            }
            cancelled(&mut cancel).await;
            bail!("fixture summary cancelled instead of killed")
        }
        let (finish, response) = oneshot::channel();
        self.dispatch
            .send(Dispatch { request, finish })
            .await
            .map_err(|_| anyhow::anyhow!("parent dispatch receiver closed"))?;
        tokio::select! {
            result = response => Ok(result?),
            _ = cancelled(&mut cancel) => bail!("synthetic summary cancelled"),
        }
    }

    async fn prepare(&self, _: Invocation) -> Result<PreparedTool> {
        bail!("this acceptance test must not prepare external tools")
    }

    async fn execute(&self, _: PreparedTool, _: watch::Receiver<bool>) -> Result<ToolOutcome> {
        bail!("this acceptance test must not execute external tools")
    }
}

async fn command(handle: &Handle, command: Command) -> Receipt {
    bounded(handle.command(command)).await.unwrap()
}

async fn create(handle: &Handle, workspace: &Path, model: &str) -> Receipt {
    command(
        handle,
        Command::CreateSession {
            id: CommandId::new(),
            workspace: workspace.to_string_lossy().into_owned(),
            profile: Profile {
                model: model.into(),
                instructions: "Deterministic process-crash acceptance; no external effects.".into(),
                read_only: true,
                max_requests: 8,
                reasoning: ReasoningEffort::Off,
            },
            policy: Policy::default(),
        },
    )
    .await
}

async fn view(handle: &Handle, thread: &ThreadId) -> View {
    let result = bounded(handle.view(Some(thread.clone()), None, 100))
        .await
        .unwrap();
    assert!(result.error.is_none(), "kernel halted: {:?}", result.error);
    result
}

fn thread_state(view: &View, thread: &ThreadId) -> ThreadState {
    view.threads
        .iter()
        .find(|item| &item.id == thread)
        .unwrap()
        .state
}

async fn completed(handle: &Handle, thread: &ThreadId) -> View {
    let mut live = handle.subscribe();
    bounded(async {
        loop {
            drop(live.borrow_and_update());
            let snapshot = view(handle, thread).await;
            if thread_state(&snapshot, thread) == ThreadState::Completed {
                return snapshot;
            }
            live.changed()
                .await
                .expect("runtime closed before completion");
        }
    })
    .await
}

async fn barrier(dir: &TempDir, executor: &Arc<SyntheticExecutor>) {
    let handle = bounded(Runtime::open(&dir.db(), executor.clone()))
        .await
        .unwrap();
    let receipt = create(&handle, &dir.0.join("workspace"), BARRIER_MODEL).await;
    command(
        &handle,
        Command::Submit {
            id: CommandId::new(),
            thread: receipt.thread.clone(),
            text: "scheduler barrier".into(),
        },
    )
    .await;
    completed(&handle, &receipt.thread).await;
    bounded(handle.shutdown()).await.unwrap();
}

#[derive(Serialize, Deserialize)]
struct Seed {
    thread: ThreadId,
    context: ContextSnapshot,
    entries: Vec<Entry>,
    working: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Checkpoint {
    operation: String,
    epoch: i64,
    revision: i64,
    manifest: String,
}

#[derive(Debug, PartialEq)]
struct Durable {
    held: bool,
    run: String,
    run_state: String,
    compact_only: bool,
    normal_requests: i64,
    epoch: i64,
    revision: i64,
    checkpoint: Option<String>,
    compact_pending: bool,
    pending: Option<Value>,
    working: Vec<String>,
    checkpoints: Vec<Checkpoint>,
}

// The public context snapshot omits the cursor, held bit and checkpoint manifest.
// One read-only transaction prevents inspection itself from mixing commit versions.
fn durable(path: &Path, thread: &ThreadId) -> Durable {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.busy_timeout(Duration::from_secs(2)).unwrap();
    let tx = db.transaction().unwrap();
    let (held, epoch, revision, checkpoint, compact_pending) = tx.query_row(
        "SELECT t.held,c.epoch,c.revision,c.checkpoint,c.compact_pending FROM threads t JOIN contexts c ON c.thread=t.id WHERE t.id=?1",
        [thread], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    ).unwrap();
    let (run, run_state, compact_only, normal_requests) = tx.query_row(
        "SELECT id,state,compact_only,requests FROM runs WHERE thread=?1 ORDER BY ordinal DESC LIMIT 1",
        [thread], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).unwrap();
    let pending: Option<String> = tx
        .query_row(
            "SELECT state FROM compactions WHERE thread=?1",
            [thread],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    let working = tx
        .prepare("SELECT model FROM working WHERE thread=?1 ORDER BY ordinal")
        .unwrap()
        .query_map([thread], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    let checkpoints = tx.prepare(
        "SELECT operation,epoch,revision,manifest FROM checkpoints WHERE thread=?1 ORDER BY revision",
    ).unwrap().query_map([thread], |r| Ok(Checkpoint {
        operation: r.get(0)?, epoch: r.get(1)?, revision: r.get(2)?, manifest: r.get(3)?,
    })).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    Durable {
        held,
        run,
        run_state,
        compact_only,
        normal_requests,
        epoch,
        revision,
        checkpoint,
        compact_pending,
        pending: pending.map(|text| serde_json::from_str(&text).unwrap()),
        working,
        checkpoints,
    }
}

fn assert_unpublished(snapshot: &Durable, seed: &Seed) {
    assert_eq!(snapshot.working, seed.working);
    assert_eq!(snapshot.epoch, seed.context.epoch);
    assert_eq!(snapshot.revision, seed.context.revision);
    assert!(snapshot.checkpoint.is_none());
    assert!(snapshot.checkpoints.is_empty());
    assert!(snapshot.compact_pending);
    assert!(snapshot.compact_only);
    assert_eq!(snapshot.normal_requests, 0);
}

fn assert_published(snapshot: &Durable, seed: &Seed) {
    assert!(snapshot.pending.is_none());
    assert!(!snapshot.compact_pending);
    assert!(!snapshot.held);
    assert!(snapshot.compact_only);
    assert_eq!(snapshot.normal_requests, 0);
    assert_eq!(snapshot.run_state, "completed");
    assert_eq!(snapshot.epoch, seed.context.epoch);
    assert_eq!(snapshot.revision, seed.context.revision + 1);
    assert_eq!(snapshot.checkpoints.len(), 1);
    let checkpoint = &snapshot.checkpoints[0];
    assert_eq!(
        snapshot.checkpoint.as_deref(),
        Some(checkpoint.operation.as_str())
    );
    assert_eq!(checkpoint.epoch, snapshot.epoch);
    assert_eq!(checkpoint.revision, snapshot.revision);
    assert_eq!(snapshot.working.len(), 1);
    assert_ne!(snapshot.working, seed.working);
}

async fn assert_transcript(handle: &Handle, seed: &Seed) {
    let current = bounded(handle.transcript(Some(seed.thread.clone()), None, 100))
        .await
        .unwrap();
    assert!(current.error.is_none());
    assert_eq!(
        current
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Assistant)
            .count(),
        1
    );
    for original in &seed.entries {
        let retained = current
            .entries
            .iter()
            .find(|entry| entry.sequence == original.sequence)
            .expect("original transcript entry disappeared");
        assert_eq!(retained.kind, original.kind);
        assert_eq!(retained.text, original.text);
        assert_eq!(retained.artifact, original.artifact);
        assert_eq!(retained.operation, original.operation);
    }
    let assistant = seed
        .entries
        .iter()
        .find(|entry| entry.kind == EntryKind::Assistant)
        .unwrap();
    let page = bounded(handle.entry_text(seed.thread.clone(), assistant.sequence, 0, 65_536))
        .await
        .unwrap();
    assert_eq!(page.total_bytes, original_body().len());
    assert!(page.next_offset.is_none());
    assert_eq!(page.text, original_body());
    assert!(page.text.contains(DECISION));
}

async fn assert_checkpoint(handle: &Handle, seed: &Seed, snapshot: &Durable) {
    assert_published(snapshot, seed);
    let manifest = bounded(handle.artifact(
        ArtifactId(snapshot.checkpoints[0].manifest.clone()),
        0,
        65_536,
    ))
    .await
    .unwrap();
    let ids: Vec<String> = serde_json::from_str(&manifest).unwrap();
    assert_eq!(
        ids, snapshot.working,
        "checkpoint and working context must be one atomic replacement"
    );
    let stored = bounded(handle.artifact(ArtifactId(ids[0].clone()), 0, 65_536))
        .await
        .unwrap();
    let summary: Message = serde_json::from_str(&stored).unwrap();
    assert_eq!(summary.role, Role::User);
    assert!(summary.text().starts_with("[conversation summary]"));
    assert!(summary.text().contains(DECISION));
    assert!(summary.text().contains(TASK));
    assert!(summary.text().len() < original_body().len() / 10);
    let context = bounded(handle.context(seed.thread.clone())).await.unwrap();
    assert_eq!(
        context.checkpoint,
        snapshot.checkpoint.clone().map(OperationId)
    );
    assert_eq!(context.revision, snapshot.revision);
    assert_eq!(context.message_count, ids.len());
    assert!(!context.compact_pending);
}

#[tokio::test]
#[ignore = "env-guarded subprocess fixture; exclude from broad --ignored sweeps"]
async fn compaction_summary_sigkill_fixture() {
    let dir = PathBuf::from(
        std::env::var_os(FIXTURE_DIR).expect("parent-only fixture: missing directory guard"),
    );
    let nonce = std::env::var(FIXTURE_NONCE).expect("parent-only fixture: missing nonce guard");
    uuid::Uuid::parse_str(&nonce).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("guard")).unwrap(), nonce);
    assert!(std::env::args().any(|arg| arg == "--exact"));
    assert!(std::env::args().any(|arg| arg == FIXTURE));
    assert!(
        !dir.join("runtime.sqlite").exists(),
        "fixture requires a fresh private database"
    );
    let (_watchdog, expired) = std::sync::mpsc::channel::<()>();
    let _watchdog_thread = std::thread::spawn(move || {
        if expired.recv_timeout(Duration::from_secs(60))
            == Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        {
            std::process::exit(78);
        }
    });
    let (executor, _requests) = SyntheticExecutor::new(Some((dir.clone(), nonce)));
    let handle = bounded(Runtime::open(&dir.join("runtime.sqlite"), executor.clone()))
        .await
        .unwrap();
    let root = create(&handle, &dir.join("workspace"), ROOT_MODEL).await;
    command(
        &handle,
        Command::Submit {
            id: CommandId::new(),
            thread: root.thread.clone(),
            text: TASK.into(),
        },
    )
    .await;
    completed(&handle, &root.thread).await;
    assert_eq!(executor.root_requests().len(), 1);
    let context = bounded(handle.context(root.thread.clone())).await.unwrap();
    assert!(context.checkpoint.is_none());
    assert!(!context.compact_pending);
    assert_eq!(context.message_count, 2);
    let entries = bounded(handle.transcript(Some(root.thread.clone()), None, 100))
        .await
        .unwrap()
        .entries;
    let seed = Seed {
        working: durable(&dir.join("runtime.sqlite"), &root.thread).working,
        thread: root.thread.clone(),
        context,
        entries,
    };
    assert_eq!(seed.working.len(), 2);
    assert_transcript(&handle, &seed).await;
    assert!((30_000..60_000).contains(&original_body().len()));
    std::fs::write(dir.join("seed.json"), serde_json::to_vec(&seed).unwrap()).unwrap();
    command(
        &handle,
        Command::Compact {
            id: CommandId::new(),
            thread: root.thread,
            expected_revision: seed.context.revision,
        },
    )
    .await;
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn sigkill_during_compaction_requires_resume_and_preserves_transcript() {
    let dir = TempDir::new();
    let nonce = uuid::Uuid::new_v4().to_string();
    std::fs::write(dir.0.join("guard"), &nonce).unwrap();
    let mut child = ProcessCommand::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            FIXTURE,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FIXTURE_DIR, &dir.0)
        .env(FIXTURE_NONCE, &nonce)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let stdout = child.stdout.take().unwrap();
    let ready = timeout(WAIT, async {
        let mut lines = BufReader::new(stdout.take(4096)).lines();
        let expected = format!("COMPACTION_READY {nonce} {pid}");
        while let Some(line) = lines.next_line().await? {
            if line == expected {
                return Ok::<_, anyhow::Error>(());
            }
        }
        bail!("fixture did not signal summary dispatch (including zero-filter success)")
    })
    .await;
    // Reap even when readiness fails, before surfacing its assertion; drop is the panic fallback.
    let kill = child.start_kill();
    let status = bounded(child.wait()).await.unwrap();
    ready.expect("fixture readiness timed out").unwrap();
    kill.unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "fixture must die from SIGKILL, not finish or exit itself"
    );

    let seed: Seed =
        serde_json::from_slice(&std::fs::read(dir.0.join("seed.json")).unwrap()).unwrap();
    let first_request: RequestSpec =
        serde_json::from_slice(&std::fs::read(dir.0.join("summary-request.json")).unwrap())
            .unwrap();
    assert_summary(&first_request);
    let crashed = durable(&dir.db(), &seed.thread);
    assert_unpublished(&crashed, &seed);
    assert_eq!(crashed.run_state, "active");
    let pending = crashed.pending.as_ref().unwrap();
    assert_eq!(pending["phase"], "summarize");
    assert_eq!(pending["attempts"], 1);
    assert_eq!(pending["cursor"], 0);
    assert_eq!(pending["offset"], 0);
    assert_eq!(pending["summary"], "");
    assert_eq!(pending["complete"], false);

    let (executor, mut requests) = SyntheticExecutor::new(None);
    let handle = bounded(Runtime::open(&dir.db(), executor.clone()))
        .await
        .unwrap();
    let session = view(&handle, &seed.thread).await.threads[0].session.clone();
    bounded(handle.activate_session(session, vec![]))
        .await
        .unwrap();
    let recovered = view(&handle, &seed.thread).await;
    assert_eq!(
        thread_state(&recovered, &seed.thread),
        ThreadState::Interrupted
    );
    let interrupted = recovered
        .operations
        .iter()
        .filter(|op| op.name == "compact")
        .collect::<Vec<_>>();
    assert_eq!(interrupted.len(), 1);
    assert_eq!(interrupted[0].state, OperationState::Interrupted);
    assert!(interrupted[0].result.is_none());
    let held = durable(&dir.db(), &seed.thread);
    assert_unpublished(&held, &seed);
    assert!(held.held);
    assert_eq!(held.run, crashed.run);
    assert_eq!(held.run_state, "interrupted");
    assert_eq!(held.pending, crashed.pending);
    assert_transcript(&handle, &seed).await;
    barrier(&dir, &executor).await;
    assert!(
        executor.root_requests().is_empty(),
        "reopen must not dispatch provider work for held compaction"
    );
    assert!(requests.try_recv().is_err());
    assert_eq!(durable(&dir.db(), &seed.thread), held);

    command(
        &handle,
        Command::Resume {
            id: CommandId::new(),
            thread: seed.thread.clone(),
        },
    )
    .await;
    let retry = bounded(requests.recv())
        .await
        .expect("Resume did not dispatch the summary");
    assert_summary(&retry.request);
    assert_eq!(
        serde_json::to_value(&retry.request).unwrap(),
        serde_json::to_value(&first_request).unwrap()
    );
    let inflight = durable(&dir.db(), &seed.thread);
    assert_unpublished(&inflight, &seed);
    assert_eq!(inflight.run, crashed.run);
    assert_eq!(inflight.run_state, "active");
    assert!(!inflight.held);
    assert_eq!(inflight.pending.as_ref().unwrap()["attempts"], 2);
    assert_eq!(inflight.pending.as_ref().unwrap()["cursor"], 0);
    assert_eq!(inflight.pending.as_ref().unwrap()["offset"], 0);
    assert_transcript(&handle, &seed).await;
    let mut live = handle.subscribe();
    retry.finish.send(answer(DECISION.into())).unwrap();
    bounded(async {
        loop {
            drop(live.borrow_and_update());
            let snapshot = durable(&dir.db(), &seed.thread);
            if snapshot.checkpoint.is_some() {
                assert_published(&snapshot, &seed);
                break;
            }
            assert_unpublished(&snapshot, &seed);
            assert_eq!(snapshot.run_state, "active");
            live.changed()
                .await
                .expect("runtime closed before checkpoint publication");
        }
    })
    .await;
    let finished = completed(&handle, &seed.thread).await;
    assert_eq!(
        finished
            .operations
            .iter()
            .filter(|op| op.name == "compact" && op.state == OperationState::Done)
            .count(),
        1
    );
    let published = durable(&dir.db(), &seed.thread);
    assert_eq!(published.run, crashed.run);
    assert_checkpoint(&handle, &seed, &published).await;
    assert_transcript(&handle, &seed).await;
    barrier(&dir, &executor).await;
    assert_eq!(
        executor.root_requests().len(),
        1,
        "compact-only recovery must not continue the normal conversation"
    );
    assert!(requests.try_recv().is_err());
    assert_eq!(durable(&dir.db(), &seed.thread), published);
    bounded(handle.shutdown()).await.unwrap();
    drop(handle);

    let (executor, mut requests) = SyntheticExecutor::new(None);
    let handle = bounded(Runtime::open(&dir.db(), executor.clone()))
        .await
        .unwrap();
    let session = view(&handle, &seed.thread)
        .await
        .threads
        .into_iter()
        .find(|thread| thread.id == seed.thread)
        .unwrap()
        .session;
    bounded(handle.activate_session(session, vec![]))
        .await
        .unwrap();
    assert_eq!(
        thread_state(&view(&handle, &seed.thread).await, &seed.thread),
        ThreadState::Completed
    );
    assert_eq!(durable(&dir.db(), &seed.thread), published);
    assert_checkpoint(&handle, &seed, &published).await;
    assert_transcript(&handle, &seed).await;
    barrier(&dir, &executor).await;
    assert!(
        executor.root_requests().is_empty(),
        "finished compaction replayed after reopen"
    );
    assert!(requests.try_recv().is_err());
    assert_eq!(durable(&dir.db(), &seed.thread), published);
    bounded(handle.shutdown()).await.unwrap();
}
