use super::*;
use crate::store::tests::fixture;
use async_trait::async_trait;
use bob_types::Usage;
use std::sync::atomic::{AtomicUsize, Ordering};

struct SummaryExecutor {
    calls: AtomicUsize,
}
#[async_trait]
impl Executor for SummaryExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "test".into(),
            context_window: 20_000,
        }]
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        spec: RequestSpec,
        _: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        assert!(spec.tools.is_empty());
        assert!(
            estimate_input_tokens(&spec)? + spec.max_tokens as usize <= spec.model.context_window
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Completion{message:Message{role:Role::Assistant,content:vec![ContentBlock::Text{text:"The prior work was inspected; preserve safety constraints and unfinished verification.".into()}]},stop_reason:StopReason::EndTurn,usage:Usage{input_tokens:100,output_tokens:20,..Default::default()}})
    }
    async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
        unreachable!()
    }
    async fn execute(
        &self,
        _: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        unreachable!()
    }
}

async fn step(actor: &mut Actor, run: &RunId, thread: &ThreadId) {
    let (tx, mut rx) = mpsc::channel(64);
    actor.request(run, thread, &tx).unwrap();
    if !actor.workers.is_empty() {
        let finished = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let worker = actor.workers.remove(&finished.id).unwrap();
        worker.task.await.unwrap();
        actor.finish(finished).unwrap();
    }
}

#[tokio::test]
async fn chunk_compaction_resumes_after_crash_without_exposing_partial_checkpoint() {
    let (dir, store, receipt, _) = fixture();
    let executor = Arc::new(SummaryExecutor {
        calls: AtomicUsize::new(0),
    });
    let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
    let run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state,compact_only) VALUES(?1,?2,'active',1)",
            params![run, receipt.thread],
        )
        .unwrap();
    let human = CommandId::new();
    submit(
        &actor.store.db,
        &human,
        &receipt.thread,
        "human",
        None,
        "VERBATIM human request: keep exact behavior",
    )
    .unwrap();
    consume(&actor.store.db, &human, &run).unwrap();
    for index in 0..350 {
        let text = format!("Historical answer {index}: {}", "data ".repeat(300));
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.clone() }],
        };
        entry(
            &actor.store.db,
            &receipt.thread,
            "assistant",
            &text,
            EntryData {
                message: Some(&message),
                ..Default::default()
            },
        )
        .unwrap();
    }
    actor
        .store
        .db
        .execute(
            "UPDATE contexts SET compact_pending=1 WHERE thread=?1",
            [&receipt.thread],
        )
        .unwrap();
    step(&mut actor, &run, &receipt.thread).await;
    step(&mut actor, &run, &receipt.thread).await;
    assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    let snapshot = actor.context_snapshot(&receipt.thread).unwrap();
    assert_eq!(snapshot.message_count, 351);
    assert!(snapshot.checkpoint.is_none());
    let raw: String = actor
        .store
        .db
        .query_row(
            "SELECT state FROM compactions WHERE thread=?1",
            [&receipt.thread],
            |r| r.get(0),
        )
        .unwrap();
    let state: CompactionState = decode(raw).unwrap();
    assert!(state.cursor > 1 || state.offset > 0);
    drop(actor);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), receipt.session.clone())
            .unwrap();
    store.adopt_claim(claim, true).unwrap();
    let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
    actor
        .store
        .command(
            &Command::Resume {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
            },
            &executor.models(),
        )
        .unwrap();
    for _ in 0..100 {
        step(&mut actor, &run, &receipt.thread).await;
        if actor
            .context_snapshot(&receipt.thread)
            .unwrap()
            .checkpoint
            .is_some()
        {
            break;
        }
    }
    let snapshot = actor.context_snapshot(&receipt.thread).unwrap();
    assert!(snapshot.checkpoint.is_some());
    assert!(snapshot.message_count <= 9);
    let ids = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    let summary = load_message(&actor.store.db, &ids[0]).unwrap().text();
    assert!(summary.contains("VERBATIM human request: keep exact behavior"));
    let transcript: i64 = actor
        .store
        .db
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE kind IN ('submission','assistant')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(transcript, 351);
    let usage = crate::sessions::usage(&actor.store.db, Some(&receipt.session)).unwrap();
    assert_eq!(
        usage.compaction_requests,
        executor.calls.load(Ordering::SeqCst) as u64
    );
    drop(actor);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), receipt.session.clone())
            .unwrap();
    store.adopt_claim(claim, true).unwrap();
    assert_eq!(
        crate::context::working_ids(&store.db, &receipt.thread)
            .unwrap()
            .len(),
        snapshot.message_count
    );
}

#[tokio::test]
async fn clear_while_summary_is_running_fences_checkpoint_and_tool_admission() {
    let (_dir, store, receipt, _) = fixture();
    let executor = Arc::new(SummaryExecutor {
        calls: AtomicUsize::new(0),
    });
    let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
    let run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, receipt.thread],
        )
        .unwrap();
    let model = OperationId::new();
    actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,'model','model','{}','started')",params![model,run,receipt.thread]).unwrap();
    actor
        .store
        .command(
            &Command::ClearContext {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                expected_epoch: 0,
            },
            &executor.models(),
        )
        .unwrap();
    actor
        .finish(Finished {
            id: model,
            result: Ok(Output::Model(Completion {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "late".into(),
                        name: "write_file".into(),
                        input: json!({"path":"never"}),
                    }],
                },
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            })),
        })
        .unwrap();
    assert_eq!(
        actor
            .context_snapshot(&receipt.thread)
            .unwrap()
            .message_count,
        0
    );
    let tools: i64 = actor
        .store
        .db
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='tool'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tools, 0);
}

#[tokio::test]
async fn oversized_tool_outcome_is_local_and_retention_is_bounded() {
    let (_dir, store, receipt, models) = fixture();
    let executor = Arc::new(SummaryExecutor {
        calls: AtomicUsize::new(0),
    });
    let mut actor = Actor::new_owned(store, executor).unwrap();
    let run = RunId::new();
    let operation = OperationId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, receipt.thread],
        )
        .unwrap();
    actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'read','tool','call','{}','started')",params![operation,run,receipt.thread]).unwrap();
    actor
        .finish(Finished {
            id: operation.clone(),
            result: Ok(Output::Tool(
                ToolOutcome {
                    text: "x".repeat(MAX_ARTIFACT + 100),
                    is_error: false,
                    interrupted: false,
                    presentation: None,
                },
                None,
            )),
        })
        .unwrap();
    let output: String = actor
        .store
        .db
        .query_row(
            "SELECT result FROM operations WHERE id=?1",
            [&operation],
            |r| r.get(0),
        )
        .unwrap();
    let retained = full_artifact(&actor.store.db, &ArtifactId(output)).unwrap();
    assert!(retained.len() <= MAX_ARTIFACT);
    assert!(retained.contains("Output exceeded"));
    actor
        .store
        .command(
            &Command::Submit {
                id: CommandId::new(),
                thread: receipt.thread,
                text: "actor remains responsive".into(),
            },
            &models,
        )
        .unwrap();
    assert!(actor.error.is_none());
}
