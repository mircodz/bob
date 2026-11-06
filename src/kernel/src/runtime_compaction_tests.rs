use super::*;
use crate::store::tests::fixture;
use async_trait::async_trait;
use bob_types::Usage;

#[path = "runtime_compaction_context_tests.rs"]
mod context_tests;
#[path = "runtime_compaction_durability_tests.rs"]
mod durability;

struct CompactExecutor;
#[async_trait]
impl Executor for CompactExecutor {
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
        _: RequestSpec,
        _: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        Ok(answer("Small valid summary"))
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
fn seeded(actor: &mut Actor, thread: &ThreadId, complete: bool) -> (RunId, CompactionState) {
    let run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state,compact_only) VALUES(?1,?2,'active',1)",
            params![run, thread],
        )
        .unwrap();
    let message = Message::user_text("visible 🦀 fragment ".repeat(12_000));
    entry(
        &actor.store.db,
        thread,
        "submission",
        "original",
        EntryData {
            message: Some(&message),
            ..Default::default()
        },
    )
    .unwrap();
    let (epoch, revision, profile_revision): (i64,i64,i64) = actor.store.db.query_row("SELECT c.epoch,c.revision,t.profile_revision FROM contexts c JOIN threads t ON t.id=c.thread WHERE c.thread=?1",[thread],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    let state: CompactionState = serde_json::from_value(json!({
        "epoch":epoch,"revision":revision,"profile_revision":profile_revision,
        "tail_start":i64::MAX,"cursor":0,"offset":0,"summary":"s".repeat(40_000),
        "newest_request":"preserve exact task","complete":complete,"chunk_limit":24_000,"output_tokens":null,"projection_version":1
    })).unwrap();
    actor
        .store
        .db
        .execute(
            "INSERT INTO compactions(thread,state) VALUES(?1,?2)",
            params![thread, encode(&state).unwrap()],
        )
        .unwrap();
    (run, state)
}
fn pending_request(actor: &Actor, thread: &ThreadId) -> (OperationId, RequestSpec) {
    let (id,raw): (String,String) = actor.store.db.query_row("SELECT id,input FROM operations WHERE thread=?1 AND name='compact' ORDER BY rowid DESC LIMIT 1",[thread],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    let value: Value = decode(raw).unwrap();
    let spec = decode(
        full_artifact(
            &actor.store.db,
            &ArtifactId(value["manifest"].as_str().unwrap().into()),
        )
        .unwrap(),
    )
    .unwrap();
    (OperationId(id), spec)
}

#[tokio::test]
async fn oversized_final_summary_enters_reduction_instead_of_resume_loop() {
    let (_dir, store, receipt, _) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    let (run, mut state) = seeded(&mut actor, &receipt.thread, true);
    state.summary = "large ".repeat(15_000);
    actor
        .store
        .db
        .execute(
            "UPDATE compactions SET state=?1 WHERE thread=?2",
            params![encode(&state).unwrap(), receipt.thread],
        )
        .unwrap();
    let (tx, _rx) = mpsc::channel(16);
    actor.request(&run, &receipt.thread, &tx).unwrap();
    assert!(
        !actor.workers.is_empty(),
        "oversized final candidate must request reduction, not pause unchanged"
    );
    assert!(actor
        .context_snapshot(&receipt.thread)
        .unwrap()
        .checkpoint
        .is_none());
}

#[tokio::test]
async fn overflow_retry_shrinks_actual_full_request_not_ceiling() {
    let (_dir, store, receipt, _) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    let (run, state) = seeded(&mut actor, &receipt.thread, false);
    let (done, _rx) = mpsc::channel(16);
    actor
        .compact_step(&run, &receipt.thread, state, &done)
        .unwrap();
    let (op, first) = pending_request(&actor, &receipt.thread);
    let tx = actor.store.db.transaction().unwrap();
    assert!(retry_model(&tx, &op, ModelFailureKind::ContextOverflow, 16_384, false).unwrap());
    tx.commit().unwrap();
    actor.request(&run, &receipt.thread, &done).unwrap();
    let (_, second) = pending_request(&actor, &receipt.thread);
    assert!(
        estimate_input_tokens(&second).unwrap() < estimate_input_tokens(&first).unwrap(),
        "overflow must shrink the actual request"
    );
}
