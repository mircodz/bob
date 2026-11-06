use anyhow::Result;
use async_trait::async_trait;
use bob_kernel::*;
use bob_types::{
    Completion, ContentBlock, Message, ReasoningEffort, ReasoningKind, Role, StopReason, ToolSpec,
    Usage,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::json;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};

const WAIT: Duration = Duration::from_secs(10);

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-streaming-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn db(&self) -> PathBuf {
        self.0.join("live.sqlite")
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("streaming test timed out")
}

struct Attempt {
    request: RequestSpec,
    progress: ModelProgress,
    cancel: watch::Receiver<bool>,
    finish: oneshot::Sender<Result<Completion>>,
}
struct ExecutorFixture {
    attempts: mpsc::Sender<Attempt>,
    calls: AtomicUsize,
    effects: AtomicUsize,
}
impl ExecutorFixture {
    fn new() -> (Arc<Self>, mpsc::Receiver<Attempt>) {
        let (attempts, rx) = mpsc::channel(32);
        (
            Arc::new(Self {
                attempts,
                calls: AtomicUsize::new(0),
                effects: AtomicUsize::new(0),
            }),
            rx,
        )
    }
}
#[async_trait]
impl Executor for ExecutorFixture {
    fn models(&self) -> Vec<ModelInfo> {
        ["root", "child"]
            .into_iter()
            .map(|id| ModelInfo {
                id: id.into(),
                context_window: 1_000_000,
            })
            .collect()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            spec: ToolSpec {
                name: "probe".into(),
                description: "Detect accidental tool admission".into(),
                input_schema: json!({"type":"object"}),
            },
            effect: EffectClass::Read,
        }]
    }
    async fn generate(
        &self,
        request: RequestSpec,
        cancel: watch::Receiver<bool>,
        progress: ModelProgress,
    ) -> Result<Completion> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (finish, rx) = oneshot::channel();
        self.attempts
            .send(Attempt {
                request,
                progress,
                cancel,
                finish,
            })
            .await
            .map_err(|_| anyhow::anyhow!("test attempt receiver closed"))?;
        rx.await?
    }
    async fn prepare(&self, _: Invocation) -> Result<PreparedTool> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("unexpected tool preparation")
    }
    async fn execute(&self, _: PreparedTool, _: watch::Receiver<bool>) -> Result<ToolOutcome> {
        self.effects.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("unexpected tool execution")
    }
}

fn answer(text: &str) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        },
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}
fn call(name: &str, input: serde_json::Value) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call".into(),
                name: name.into(),
                input,
            }],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}
async fn create(handle: &Handle, workspace: &Path) -> Receipt {
    bounded(handle.command(Command::CreateSession {
        id: CommandId::new(),
        workspace: workspace.to_string_lossy().into_owned(),
        profile: Profile {
            model: "root".into(),
            instructions: String::new(),
            read_only: false,
            max_requests: 64,
            reasoning: ReasoningEffort::Off,
        },
        policy: Policy::default(),
    }))
    .await
    .unwrap()
}
async fn submit(handle: &Handle, thread: &ThreadId) {
    bounded(handle.command(Command::Submit {
        id: CommandId::new(),
        thread: thread.clone(),
        text: "stream now".into(),
    }))
    .await
    .unwrap();
}
async fn next(rx: &mut mpsc::Receiver<Attempt>) -> Attempt {
    bounded(rx.recv()).await.unwrap()
}
async fn live_until(
    rx: &mut watch::Receiver<Arc<LiveSnapshot>>,
    predicate: impl Fn(&LiveSnapshot) -> bool,
) -> Arc<LiveSnapshot> {
    bounded(async {
        loop {
            let snapshot = rx.borrow_and_update().clone();
            if predicate(&snapshot) {
                return snapshot;
            }
            rx.changed().await.unwrap();
        }
    })
    .await
}
async fn view(handle: &Handle, thread: &ThreadId) -> View {
    bounded(handle.view(Some(thread.clone()), None, 100))
        .await
        .unwrap()
}
fn scalar(db: &Path, sql: &str) -> i64 {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}
async fn settled(
    rx: &mut watch::Receiver<Arc<LiveSnapshot>>,
    operation: &OperationId,
) -> Arc<LiveSnapshot> {
    live_until(rx, |s| {
        s.attempts
            .iter()
            .any(|a| &a.operation == operation && a.settled_sequence.is_some())
    })
    .await
}

#[tokio::test]
async fn preview_is_transient_and_final_completion_is_authoritative() {
    let dir = TempDir::new();
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = bounded(Runtime::open(&dir.db(), executor.clone()))
        .await
        .unwrap();
    let mut live = handle.subscribe();
    let root = create(&handle, &dir.0).await;
    submit(&handle, &root.thread).await;
    let attempt = next(&mut requests).await;
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM operations WHERE kind='model' AND state='started'"
        ),
        1
    );
    let start = live_until(&mut live, |s| !s.attempts.is_empty()).await;
    let operation = start.attempts[0].operation.clone();
    let sequence = start.sequence;
    attempt
        .progress
        .emit(ModelUpdate::Reasoning {
            block_id: "summary".into(),
            kind: ReasoningKind::Summary,
            text: "Public summary".into(),
        })
        .await
        .unwrap();
    attempt
        .progress
        .emit(ModelUpdate::Text(
            "draft, not the answer {partial tool:".into(),
        ))
        .await
        .unwrap();
    attempt
        .progress
        .emit(ModelUpdate::Activity(ModelActivity::ToolCall))
        .await
        .unwrap();
    let preview = live_until(&mut live, |s| {
        s.attempts[0].activity == ModelActivity::ToolCall
    })
    .await;
    assert_eq!(preview.sequence, sequence);
    assert!(preview.revision > start.revision);
    assert_eq!(preview.attempts[0].reasoning[0].text, "Public summary");
    assert_eq!(preview.attempts[0].thread, root.thread);
    let before = view(&handle, &root.thread).await;
    assert_eq!(before.sequence, sequence);
    assert!(before
        .entries
        .iter()
        .all(|e| !matches!(e.kind, EntryKind::Assistant | EntryKind::ToolCall)));
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM operations WHERE kind='tool'"
        ),
        0
    );
    assert_eq!(executor.effects.load(Ordering::SeqCst), 0);
    let late = attempt.progress.clone();
    attempt
        .finish
        .send(Ok(answer("authoritative answer")))
        .unwrap();
    let final_live = settled(&mut live, &operation).await;
    let retired = final_live
        .attempts
        .iter()
        .find(|a| a.operation == operation)
        .unwrap();
    let durable = view(&handle, &root.thread).await;
    assert!(durable.sequence >= retired.settled_sequence.unwrap());
    let assistant: Vec<_> = durable
        .entries
        .iter()
        .filter(|e| e.kind == EntryKind::Assistant)
        .collect();
    assert_eq!(assistant.len(), 1);
    assert_eq!(assistant[0].text, "authoritative answer");
    assert_eq!(assistant[0].operation.as_ref(), Some(&operation));
    assert!(retired.text.starts_with("draft"));
    assert!(retired.error.is_none());
    late.emit(ModelUpdate::Text("late poison".into()))
        .await
        .unwrap();
    let reconnect = handle.subscribe();
    assert_eq!(reconnect.borrow().instance, final_live.instance);
    assert_eq!(reconnect.borrow().attempts[0].revision, retired.revision);
    assert!(!reconnect.borrow().attempts[0].text.contains("poison"));
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn stop_rejects_late_progress_and_resume_gets_a_new_operation() {
    let dir = TempDir::new();
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = Runtime::open(&dir.db(), executor.clone()).await.unwrap();
    let mut live = handle.subscribe();
    let root = create(&handle, &dir.0).await;
    submit(&handle, &root.thread).await;
    let mut first = next(&mut requests).await;
    first
        .progress
        .emit(ModelUpdate::Text("before stop".into()))
        .await
        .unwrap();
    let preview = live_until(&mut live, |s| {
        s.attempts.iter().any(|a| a.text == "before stop")
    })
    .await;
    let identity = preview.attempts[0].clone();
    bounded(handle.command(Command::Stop {
        id: CommandId::new(),
        run: identity.run.clone(),
    }))
    .await
    .unwrap();
    bounded(cancelled(&mut first.cancel)).await;
    first
        .progress
        .emit(ModelUpdate::Text("late after stop".into()))
        .await
        .unwrap();
    let stopping = live_until(&mut live, |s| s.attempts[0].stopping).await;
    assert_eq!(stopping.attempts[0].text, "before stop");
    let late = first.progress.clone();
    first.finish.send(Ok(call("probe", json!({})))).unwrap();
    settled(&mut live, &identity.operation).await;
    live_until(&mut live, |s| s.sequence > stopping.sequence).await;
    bounded(async {
        loop {
            if view(&handle, &root.thread).await.threads[0].state == ThreadState::Cancelled {
                break;
            }
            live.changed().await.unwrap();
        }
    })
    .await;
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM operations WHERE kind='tool'"
        ),
        0
    );
    submit(&handle, &root.thread).await;
    bounded(handle.command(Command::Resume {
        id: CommandId::new(),
        thread: root.thread.clone(),
    }))
    .await
    .unwrap();
    let second = next(&mut requests).await;
    second
        .progress
        .emit(ModelUpdate::Text("new preview".into()))
        .await
        .unwrap();
    late.emit(ModelUpdate::Text("old attempt poison".into()))
        .await
        .unwrap();
    let resumed = live_until(&mut live, |s| {
        s.attempts.iter().any(|a| a.text == "new preview")
    })
    .await;
    let new = resumed
        .attempts
        .iter()
        .find(|a| a.text == "new preview")
        .unwrap();
    assert_ne!(new.operation, identity.operation);
    assert_eq!(new.thread, identity.thread);
    assert!(resumed.attempts.iter().all(|a| !a.text.contains("poison")));
    let operation = new.operation.clone();
    second.finish.send(Ok(answer("resumed answer"))).unwrap();
    settled(&mut live, &operation).await;
    assert_eq!(executor.effects.load(Ordering::SeqCst), 0);
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn root_and_child_progress_have_independent_durable_identities() {
    let dir = TempDir::new();
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = Runtime::open(&dir.db(), executor).await.unwrap();
    let mut live = handle.subscribe();
    let root = create(&handle, &dir.0).await;
    submit(&handle, &root.thread).await;
    next(&mut requests)
        .await
        .finish
        .send(Ok(call(
            "launch_agent",
            json!({"name":"worker", "prompt":"child task", "model":"child"}),
        )))
        .unwrap();
    let a = next(&mut requests).await;
    let b = next(&mut requests).await;
    let (root_attempt, child_attempt) = if a.request.model.id == "root" {
        (a, b)
    } else {
        (b, a)
    };
    assert_eq!(child_attempt.request.model.id, "child");
    root_attempt
        .progress
        .emit(ModelUpdate::Text("root stream".into()))
        .await
        .unwrap();
    child_attempt
        .progress
        .emit(ModelUpdate::Text("child stream".into()))
        .await
        .unwrap();
    let snapshot = live_until(&mut live, |s| {
        s.attempts
            .iter()
            .filter(|a| a.settled_sequence.is_none() && !a.text.is_empty())
            .count()
            == 2
    })
    .await;
    let root_live = snapshot
        .attempts
        .iter()
        .find(|a| a.text == "root stream")
        .unwrap();
    let child_live = snapshot
        .attempts
        .iter()
        .find(|a| a.text == "child stream")
        .unwrap();
    assert_eq!(root_live.thread, root.thread);
    assert_ne!(root_live.thread, child_live.thread);
    assert_ne!(root_live.run, child_live.run);
    assert_ne!(root_live.operation, child_live.operation);
    let child_view = view(&handle, &child_live.thread).await;
    assert_eq!(
        child_view
            .threads
            .iter()
            .find(|t| t.id == child_live.thread)
            .unwrap()
            .parent_run
            .as_ref(),
        Some(&root_live.run)
    );
    for attempt in [root_live, child_live] {
        let db = Connection::open_with_flags(dir.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let pair: (String, String) = db
            .query_row(
                "SELECT thread,run FROM operations WHERE id=?1",
                [&attempt.operation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(pair, (attempt.thread.0.clone(), attempt.run.0.clone()));
    }
    bounded(handle.command(Command::Stop {
        id: CommandId::new(),
        run: root_live.run.clone(),
    }))
    .await
    .unwrap();
    root_attempt
        .finish
        .send(Err(anyhow::anyhow!("root stopped")))
        .unwrap();
    child_attempt
        .finish
        .send(Err(anyhow::anyhow!("child stopped")))
        .unwrap();
    settled(&mut live, &root_live.operation).await;
    settled(&mut live, &child_live.operation).await;
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn absent_and_slow_subscribers_do_not_block_bounded_progress_or_commands() {
    let dir = TempDir::new();
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = Runtime::open(&dir.db(), executor.clone()).await.unwrap();
    let peer = Runtime::open(&dir.db(), executor).await.unwrap();
    let root = create(&handle, &dir.0).await;
    submit(&handle, &root.thread).await;
    let attempt = next(&mut requests).await;
    let progress = attempt.progress.clone();
    let burst = tokio::spawn(async move {
        for _ in 0..192 {
            progress
                .emit(ModelUpdate::Reasoning {
                    block_id: "thinking".into(),
                    kind: ReasoningKind::Thinking,
                    text: "界".repeat(1365),
                })
                .await
                .unwrap();
            progress
                .emit(ModelUpdate::Text("x".repeat(4096)))
                .await
                .unwrap();
        }
    });
    let during = create(&peer, &dir.0).await;
    assert_ne!(during.thread, root.thread);
    let policy = view(&handle, &root.thread)
        .await
        .policies
        .into_iter()
        .find(|policy| policy.session == root.session)
        .unwrap();
    bounded(handle.command(Command::SetPolicy {
        id: CommandId::new(),
        session: root.session.clone(),
        expected_revision: policy.revision,
        policy: policy.policy,
    }))
    .await
    .unwrap();
    bounded(burst).await.unwrap();
    let mut reconnect = handle.subscribe();
    let snapshot = live_until(&mut reconnect, |s| s.attempts.iter().any(|a| a.truncated)).await;
    let draft = &snapshot.attempts[0];
    let bytes = draft.text.len()
        + draft
            .reasoning
            .iter()
            .map(|r| r.block_id.len() + r.text.len())
            .sum::<usize>();
    assert!(bytes <= 256 * 1024);
    assert!(bytes > 200 * 1024);
    let operation = draft.operation.clone();
    drop(reconnect);
    attempt
        .finish
        .send(Ok(answer("small authoritative final")))
        .unwrap();
    let mut reconnect = handle.subscribe();
    let final_live = settled(&mut reconnect, &operation).await;
    assert!(final_live.attempts[0].truncated);
    assert!(view(&handle, &root.thread)
        .await
        .entries
        .iter()
        .any(|e| e.text == "small authoritative final"));
    bounded(handle.shutdown()).await.unwrap();
    bounded(peer.shutdown()).await.unwrap();
}

#[tokio::test]
async fn large_updates_preserve_unicode_and_retired_attempts_are_bounded() {
    let dir = TempDir::new();
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = Runtime::open(&dir.db(), executor).await.unwrap();
    let mut live = handle.subscribe();
    let root = create(&handle, &dir.0).await;
    for index in 0..20 {
        submit(&handle, &root.thread).await;
        let attempt = next(&mut requests).await;
        if index == 0 {
            let text = "é界".repeat(10_000);
            attempt
                .progress
                .emit(ModelUpdate::Text(text.clone()))
                .await
                .unwrap();
            let received =
                live_until(&mut live, |s| s.attempts.iter().any(|a| a.text == text)).await;
            assert_eq!(received.attempts[0].text, text);
            assert!(!received.attempts[0].truncated);
        }
        let start = live_until(&mut live, |s| {
            s.attempts.iter().any(|a| a.settled_sequence.is_none())
        })
        .await;
        let operation = start
            .attempts
            .iter()
            .find(|a| a.settled_sequence.is_none())
            .unwrap()
            .operation
            .clone();
        attempt.finish.send(Ok(answer("done"))).unwrap();
        let final_live = settled(&mut live, &operation).await;
        assert!(final_live.attempts.len() <= 16);
    }
    assert_eq!(live.borrow().attempts.len(), 16);
    bounded(handle.shutdown()).await.unwrap();
}

#[tokio::test]
async fn cancellation_unblocks_a_saturated_progress_producer() {
    let dir = TempDir::new();
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = Runtime::open(&dir.db(), executor).await.unwrap();
    let mut live = handle.subscribe();
    let root = create(&handle, &dir.0).await;
    submit(&handle, &root.thread).await;
    let mut attempt = next(&mut requests).await;
    let start = live_until(&mut live, |s| !s.attempts.is_empty()).await;
    let operation = start.attempts[0].operation.clone();
    let progress = attempt.progress.clone();
    let (saturated, ready) = oneshot::channel();
    let burst = tokio::spawn(async move {
        let mut saturated = Some(saturated);
        for index in 0..10_000 {
            progress
                .emit(ModelUpdate::Text("bounded burst".into()))
                .await
                .unwrap();
            if index == 15 {
                saturated.take().unwrap().send(()).unwrap();
            }
        }
    });
    bounded(ready).await.unwrap();
    bounded(handle.command(Command::Stop {
        id: CommandId::new(),
        run: start.attempts[0].run.clone(),
    }))
    .await
    .unwrap();
    bounded(cancelled(&mut attempt.cancel)).await;
    bounded(burst).await.unwrap();
    let stopping = live_until(&mut live, |s| s.attempts[0].stopping).await;
    attempt
        .progress
        .emit(ModelUpdate::Text("after-stop poison".into()))
        .await
        .unwrap();
    attempt
        .finish
        .send(Err(anyhow::anyhow!("stream cancelled")))
        .unwrap();
    let final_live = settled(&mut live, &operation).await;
    assert_eq!(final_live.attempts[0].text, stopping.attempts[0].text);
    assert_eq!(
        final_live.attempts[0].error.as_deref(),
        Some("stream cancelled")
    );
    assert!(view(&handle, &root.thread)
        .await
        .entries
        .iter()
        .all(|e| e.kind != EntryKind::Assistant));
    bounded(handle.shutdown()).await.unwrap();
}

async fn malformed_completion_is_isolated(ids: Vec<String>, expected_error: &str) {
    let dir = TempDir::new();
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = bounded(Runtime::open(&dir.db(), executor.clone()))
        .await
        .unwrap();
    let mut live = handle.subscribe();
    let bad = create(&handle, &dir.0).await;
    let peer = bounded(Runtime::open(&dir.db(), executor.clone()))
        .await
        .unwrap();
    let mut healthy_live = peer.subscribe();
    let healthy = create(&peer, &dir.0).await;
    submit(&handle, &bad.thread).await;
    let bad_attempt = next(&mut requests).await;
    submit(&peer, &healthy.thread).await;
    let healthy_attempt = next(&mut requests).await;
    let started = live_until(&mut live, |s| s.attempts.len() == 1).await;
    let bad_operation = started
        .attempts
        .iter()
        .find(|a| a.thread == bad.thread)
        .unwrap()
        .operation
        .clone();
    let healthy_started = live_until(&mut healthy_live, |s| s.attempts.len() == 1).await;
    let healthy_operation = healthy_started
        .attempts
        .iter()
        .find(|a| a.thread == healthy.thread)
        .unwrap()
        .operation
        .clone();
    let mut completion = call("probe", json!({}));
    completion.message.content = ids
        .into_iter()
        .map(|id| ContentBlock::ToolUse {
            id,
            name: "probe".into(),
            input: json!({}),
        })
        .collect();
    bad_attempt.finish.send(Ok(completion)).unwrap();
    let failed = settled(&mut live, &bad_operation).await;
    let retired = failed
        .attempts
        .iter()
        .find(|a| a.operation == bad_operation)
        .unwrap();
    assert_eq!(retired.error.as_deref(), Some(expected_error));
    let durable = bounded(async {
        loop {
            let durable = view(&handle, &bad.thread).await;
            let state = durable
                .threads
                .iter()
                .find(|thread| thread.id == bad.thread)
                .unwrap()
                .state;
            if state == ThreadState::Failed {
                break durable;
            }
            assert_eq!(state, ThreadState::Stopping);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(durable.error.is_none());
    assert!(durable.sequence >= retired.settled_sequence.unwrap());
    assert_eq!(
        durable
            .threads
            .iter()
            .find(|t| t.id == bad.thread)
            .unwrap()
            .state,
        ThreadState::Failed
    );
    let operation = durable
        .operations
        .iter()
        .find(|o| o.id == bad_operation)
        .unwrap();
    assert_eq!(operation.state, OperationState::Failed);
    assert!(operation.result.is_none());
    assert!(durable
        .entries
        .iter()
        .any(|e| { e.kind == EntryKind::Recovery && e.text.contains(expected_error) }));
    assert!(durable
        .entries
        .iter()
        .all(|e| !matches!(e.kind, EntryKind::Assistant | EntryKind::ToolCall)));
    let healthy_snapshot = healthy_live.borrow().clone();
    let running = healthy_snapshot
        .attempts
        .iter()
        .find(|a| a.operation == healthy_operation)
        .unwrap();
    assert!(running.error.is_none());
    assert!(running.settled_sequence.is_none());
    assert!(!running.stopping);
    assert!(!*healthy_attempt.cancel.borrow());
    healthy_attempt
        .progress
        .emit(ModelUpdate::Text("still healthy".into()))
        .await
        .unwrap();
    live_until(&mut healthy_live, |s| {
        s.attempts
            .iter()
            .any(|a| a.operation == healthy_operation && a.text == "still healthy")
    })
    .await;
    let durable = view(&peer, &healthy.thread).await;
    assert!(durable.error.is_none());
    assert_eq!(
        durable
            .threads
            .iter()
            .find(|t| t.id == healthy.thread)
            .unwrap()
            .state,
        ThreadState::Active
    );
    submit(&handle, &bad.thread).await;
    let recovered = next(&mut requests).await;
    assert!(recovered
        .request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .all(|block| !matches!(block, ContentBlock::ToolUse { .. })));
    let recovering = live_until(&mut live, |snapshot| {
        snapshot.attempts.iter().any(|attempt| {
            attempt.thread == bad.thread
                && attempt.operation != bad_operation
                && attempt.settled_sequence.is_none()
        })
    })
    .await;
    let recovered_operation = recovering
        .attempts
        .iter()
        .find(|attempt| {
            attempt.thread == bad.thread
                && attempt.operation != bad_operation
                && attempt.settled_sequence.is_none()
        })
        .unwrap()
        .operation
        .clone();
    recovered
        .finish
        .send(Ok(answer("explicit follow-up after failed response")))
        .unwrap();
    settled(&mut live, &recovered_operation).await;
    healthy_attempt
        .finish
        .send(Ok(answer("healthy answer")))
        .unwrap();
    settled(&mut healthy_live, &healthy_operation).await;
    assert!(view(&peer, &healthy.thread)
        .await
        .entries
        .iter()
        .any(|e| e.kind == EntryKind::Assistant && e.text == "healthy answer"));
    submit(&peer, &healthy.thread).await;
    let follow_up = next(&mut requests).await;
    let started = live_until(&mut healthy_live, |s| {
        s.attempts
            .iter()
            .any(|a| a.thread == healthy.thread && a.settled_sequence.is_none())
    })
    .await;
    let operation = started
        .attempts
        .iter()
        .find(|a| a.thread == healthy.thread && a.settled_sequence.is_none())
        .unwrap()
        .operation
        .clone();
    follow_up
        .finish
        .send(Ok(answer("follow-up answer")))
        .unwrap();
    settled(&mut healthy_live, &operation).await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 4);
    assert_eq!(executor.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM operations WHERE kind='tool'"
        ),
        0
    );
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM entries WHERE kind='tool_call'"
        ),
        0
    );
    assert_eq!(
        view(&handle, &bad.thread)
            .await
            .threads
            .iter()
            .find(|t| t.id == bad.thread)
            .unwrap()
            .state,
        ThreadState::Completed
    );
    bounded(handle.shutdown()).await.unwrap();
    bounded(peer.shutdown()).await.unwrap();
}

#[tokio::test]
async fn duplicate_final_tool_ids_fail_only_their_attempt() {
    malformed_completion_is_isolated(
        vec!["duplicate".into(), "duplicate".into()],
        "provider returned duplicate/empty tool IDs",
    )
    .await;
}

#[tokio::test]
async fn empty_final_tool_id_fails_only_its_attempt() {
    malformed_completion_is_isolated(
        vec!["valid".into(), String::new()],
        "provider returned duplicate/empty tool IDs",
    )
    .await;
}

#[tokio::test]
async fn excess_final_tool_calls_fail_only_their_attempt() {
    malformed_completion_is_isolated(
        (0..33).map(|index| format!("call-{index}")).collect(),
        "provider returned too many tool calls",
    )
    .await;
}

#[tokio::test]
async fn oversized_final_response_fails_only_its_attempt() {
    malformed_completion_is_isolated(
        vec!["x".repeat(4 * 1024 * 1024)],
        "provider response exceeds the retention limit",
    )
    .await;
}

#[tokio::test]
async fn failed_completion_commit_does_not_publish_success_or_admit_tools() {
    let dir = TempDir::new();
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = Runtime::open(&dir.db(), executor.clone()).await.unwrap();
    let mut live = handle.subscribe();
    let root = create(&handle, &dir.0).await;
    submit(&handle, &root.thread).await;
    let attempt = next(&mut requests).await;
    attempt
        .progress
        .emit(ModelUpdate::Text("uncommitted preview".into()))
        .await
        .unwrap();
    let before = live_until(&mut live, |s| s.attempts.iter().any(|a| !a.text.is_empty())).await;
    Connection::open(dir.db()).unwrap().execute_batch("CREATE TRIGGER fail_settlement BEFORE INSERT ON changes WHEN NEW.kind='operation_updated' BEGIN SELECT RAISE(ABORT,'injected settlement failure'); END;").unwrap();
    let mut completion = call("probe", json!({}));
    completion.message.content.insert(
        0,
        ContentBlock::Text {
            text: "Retained after storage repair.".into(),
        },
    );
    attempt.finish.send(Ok(completion)).unwrap();
    let failed = live_until(&mut live, |s| s.attempts.iter().any(|a| a.error.is_some())).await;
    assert_eq!(failed.sequence, before.sequence);
    assert!(failed.attempts[0].settled_sequence.is_none());
    assert!(failed.attempts[0]
        .error
        .as_ref()
        .unwrap()
        .contains("injected settlement failure"));
    let durable = view(&handle, &root.thread).await;
    assert!(durable.error.is_some());
    assert!(durable
        .entries
        .iter()
        .all(|e| !matches!(e.kind, EntryKind::Assistant | EntryKind::ToolCall)));
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM operations WHERE kind='tool'"
        ),
        0
    );
    assert_eq!(executor.effects.load(Ordering::SeqCst), 0);
    assert!(bounded(handle.command(Command::Submit {
        id: CommandId::new(),
        thread: root.thread,
        text: "must not run".into()
    }))
    .await
    .is_err());
    let error = bounded(handle.shutdown()).await.unwrap_err();
    assert!(error.to_string().contains("still settling"), "{error:#}");
    assert!(SessionClaim::acquire(&dir.db(), root.session.clone()).is_err());
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM operations WHERE kind='model' AND state='started'"
        ),
        1
    );
    Connection::open(dir.db())
        .unwrap()
        .execute_batch("DROP TRIGGER fail_settlement")
        .unwrap();
    bounded(async {
        loop {
            match handle.shutdown().await {
                Ok(()) => break,
                Err(error) => {
                    assert!(error.to_string().contains("still settling"), "{error:#}");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        }
    })
    .await;
    assert!(SessionClaim::acquire(&dir.db(), root.session).is_ok());
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM operations WHERE kind='tool'"
        ),
        0
    );
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM entries WHERE kind='tool_call'"
        ),
        0
    );
    assert_eq!(
        scalar(
            &dir.db(),
            "SELECT COUNT(*) FROM operations WHERE kind='model' AND state='interrupted'"
        ),
        1
    );
    assert_eq!(scalar(&dir.db(), "SELECT COUNT(*) FROM entries WHERE kind='assistant' AND text='Retained after storage repair.' AND model IS NULL"), 1);
    assert_eq!(executor.effects.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[ignore = "subprocess crash fixture"]
async fn crash_during_stream_fixture() {
    let dir = PathBuf::from(std::env::var("BOB_STREAM_CRASH_DIR").unwrap());
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = Runtime::open(&dir.join("live.sqlite"), executor)
        .await
        .unwrap();
    let root = create(&handle, &dir).await;
    submit(&handle, &root.thread).await;
    let attempt = next(&mut requests).await;
    attempt
        .progress
        .emit(ModelUpdate::Text("vanishing transient draft".into()))
        .await
        .unwrap();
    let mut live = handle.subscribe();
    let snapshot = live_until(&mut live, |s| s.attempts.iter().any(|a| !a.text.is_empty())).await;
    std::fs::write(dir.join("instance"), &snapshot.instance).unwrap();
    std::process::exit(73);
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::test]
async fn crash_discards_live_drafts_and_reopen_does_not_replay() {
    let dir = TempDir::new();
    let mut child = ChildGuard(
        ProcessCommand::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "crash_during_stream_fixture",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("BOB_STREAM_CRASH_DIR", &dir.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let status = bounded(async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert_eq!(status.code(), Some(73));
    let old_instance = std::fs::read_to_string(dir.0.join("instance")).unwrap();
    let db = Connection::open_with_flags(dir.db(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let root = ThreadId(
        db.query_row("SELECT id FROM threads", [], |r| r.get(0))
            .unwrap(),
    );
    drop(db);
    let (executor, mut requests) = ExecutorFixture::new();
    let handle = Runtime::open(&dir.db(), executor.clone()).await.unwrap();
    let session = view(&handle, &root).await.threads[0].session.clone();
    bounded(handle.activate_session(session, vec![]))
        .await
        .unwrap();
    let mut live = handle.subscribe();
    assert_ne!(live.borrow().instance, old_instance);
    assert!(live.borrow().attempts.is_empty());
    let recovered = view(&handle, &root).await;
    assert_eq!(recovered.threads[0].state, ThreadState::Interrupted);
    assert!(recovered
        .operations
        .iter()
        .any(|o| o.state == OperationState::Interrupted));
    assert_eq!(
        recovered
            .operations
            .iter()
            .map(|o| (o.name.as_str(), o.state))
            .collect::<Vec<_>>(),
        vec![
            ("model", OperationState::Interrupted),
            ("context", OperationState::Done)
        ],
        "only the interrupted model request and its completed context preparation may remain"
    );
    assert!(recovered
        .entries
        .iter()
        .all(|e| !e.text.contains("vanishing")));
    let peer = Runtime::open(&dir.db(), executor.clone()).await.unwrap();
    let mut barrier_live = peer.subscribe();
    let barrier = create(&peer, &dir.0).await;
    submit(&peer, &barrier.thread).await;
    let attempt = next(&mut requests).await;
    attempt
        .progress
        .emit(ModelUpdate::Text("barrier".into()))
        .await
        .unwrap();
    let started = live_until(&mut barrier_live, |s| {
        s.attempts.iter().any(|a| a.thread == barrier.thread)
    })
    .await;
    let operation = started.attempts[0].operation.clone();
    attempt.finish.send(Ok(answer("barrier done"))).unwrap();
    settled(&mut barrier_live, &operation).await;
    bounded(peer.shutdown()).await.unwrap();
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        view(&handle, &root)
            .await
            .threads
            .iter()
            .find(|t| t.id == root)
            .unwrap()
            .state,
        ThreadState::Interrupted
    );
    bounded(handle.command(Command::Resume {
        id: CommandId::new(),
        thread: root.clone(),
    }))
    .await
    .unwrap();
    let attempt = next(&mut requests).await;
    let started = live_until(&mut live, |s| s.attempts.iter().any(|a| a.thread == root)).await;
    let operation = started
        .attempts
        .iter()
        .find(|a| a.thread == root)
        .unwrap()
        .operation
        .clone();
    attempt
        .finish
        .send(Ok(answer("explicitly resumed")))
        .unwrap();
    settled(&mut live, &operation).await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    bounded(handle.shutdown()).await.unwrap();
}
