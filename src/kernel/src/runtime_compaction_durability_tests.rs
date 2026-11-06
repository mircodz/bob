use super::*;

#[path = "runtime_compaction_budget_tests.rs"]
mod budgets;

type Dispatch = (RequestSpec, tokio::sync::oneshot::Sender<Completion>);
struct BarrierExecutor {
    requests: mpsc::UnboundedSender<Dispatch>,
}
#[async_trait]
impl Executor for BarrierExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        CompactExecutor.models()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    fn measured_input_tokens(&self, _: &str, usage: &Usage) -> Option<u64> {
        Some(
            usage
                .input_tokens
                .saturating_add(usage.cache_read_input_tokens)
                .saturating_add(usage.cache_creation_input_tokens),
        )
        .filter(|n| *n > 0)
    }
    async fn generate(
        &self,
        spec: RequestSpec,
        _: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.requests.send((spec, tx)).unwrap();
        Ok(rx.await?)
    }
    async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
        unreachable!()
    }
    async fn execute(
        &self,
        _: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        panic!("completed tools must not replay")
    }
}
async fn dispatch(rx: &mut mpsc::UnboundedReceiver<Dispatch>) -> Dispatch {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn finish(actor: &mut Actor, rx: &mut mpsc::Receiver<Finished>) {
    let result = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    actor
        .workers
        .remove(&result.id)
        .unwrap()
        .task
        .await
        .unwrap();
    actor.finish(result).unwrap();
}
async fn advance(actor: &mut Actor, run: &RunId, thread: &ThreadId) {
    let (tx, mut rx) = mpsc::channel(16);
    actor.request(run, thread, &tx).unwrap();
    if !actor.workers.is_empty() {
        finish(actor, &mut rx).await;
    }
}
fn state(actor: &Actor, thread: &ThreadId) -> CompactionState {
    decode(
        actor
            .store
            .db
            .query_row(
                "SELECT state FROM compactions WHERE thread=?1",
                [thread],
                |r| r.get(0),
            )
            .unwrap(),
    )
    .unwrap()
}
fn history(actor: &Actor, thread: &ThreadId) -> Vec<(i64, String)> {
    let mut stmt = actor
        .store
        .db
        .prepare("SELECT seq,model FROM entries WHERE thread=?1 AND model IS NOT NULL ORDER BY seq")
        .unwrap();
    stmt.query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn final_reduction_is_atomic_reopens_and_keeps_transcript() {
    let (dir, store, receipt, _) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    let (run, mut pending) = seeded(&mut actor, &receipt.thread, true);
    pending.summary = "large summary ".repeat(12_000);
    pending.save(&actor.store.db, &receipt.thread).unwrap();
    let original = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    let transcript = history(&actor, &receipt.thread);
    let mut published = false;
    for _ in 0..20 {
        advance(&mut actor, &run, &receipt.thread).await;
        let snapshot = actor.context_snapshot(&receipt.thread).unwrap();
        assert_eq!(history(&actor, &receipt.thread), transcript);
        if snapshot.checkpoint.is_some() {
            published = true;
            break;
        }
        assert_eq!(
            crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap(),
            original
        );
    }
    assert!(published);
    let ids = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    assert!(load_message(&actor.store.db, &ids[0])
        .unwrap()
        .text()
        .contains("preserve exact task"));
    drop(actor);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), receipt.session.clone())
            .unwrap();
    store.adopt_claim(claim, true).unwrap();
    assert_eq!(
        crate::context::working_ids(&store.db, &receipt.thread).unwrap(),
        ids
    );
}

#[tokio::test]
async fn actual_inflight_clear_cancel_profile_and_revision_fence_late_summary() {
    for invalidation in ["clear", "cancel", "profile", "revision"] {
        let (_dir, store, receipt, models) = fixture();
        let (requests, mut dispatched) = mpsc::unbounded_channel();
        let mut actor = Actor::new_owned(store, Arc::new(BarrierExecutor { requests })).unwrap();
        let (run, pending) = seeded(&mut actor, &receipt.thread, false);
        let original = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
        let (done, mut completed) = mpsc::channel(16);
        actor
            .compact_step(&run, &receipt.thread, pending, &done)
            .unwrap();
        let (spec, response) = dispatch(&mut dispatched).await;
        assert_eq!(spec.system, SUMMARY_SYSTEM);
        let before = state(&actor, &receipt.thread);
        assert_eq!(before.attempts, 1);
        match invalidation {
            "clear" => {
                actor
                    .store
                    .command(
                        &Command::ClearContext {
                            id: CommandId::new(),
                            thread: receipt.thread.clone(),
                            expected_epoch: 0,
                        },
                        &models,
                    )
                    .unwrap();
            }
            "cancel" => {
                actor
                    .store
                    .command(
                        &Command::StopThread {
                            id: CommandId::new(),
                            thread: receipt.thread.clone(),
                        },
                        &models,
                    )
                    .unwrap();
            }
            "profile" => {
                let mut profile = actor.context_snapshot(&receipt.thread).unwrap().profile;
                profile.instructions.push_str(" changed");
                actor
                    .store
                    .command(
                        &Command::UpdateProfile {
                            id: CommandId::new(),
                            thread: receipt.thread.clone(),
                            profile,
                            expected_revision: 0,
                        },
                        &models,
                    )
                    .unwrap();
            }
            _ => {
                entry(
                    &actor.store.db,
                    &receipt.thread,
                    "submission",
                    "new",
                    EntryData {
                        message: Some(&Message::user_text("new human steering")),
                        ..Default::default()
                    },
                )
                .unwrap();
            }
        }
        response.send(answer("must not publish")).unwrap();
        finish(&mut actor, &mut completed).await;
        assert!(actor
            .context_snapshot(&receipt.thread)
            .unwrap()
            .checkpoint
            .is_none());
        if invalidation == "clear" {
            assert!(
                crate::context::working_ids(&actor.store.db, &receipt.thread)
                    .unwrap()
                    .is_empty()
            );
        } else {
            let after = state(&actor, &receipt.thread);
            assert_eq!(
                (after.cursor, after.offset, after.summary),
                (before.cursor, before.offset, before.summary)
            );
            assert!(
                crate::context::working_ids(&actor.store.db, &receipt.thread)
                    .unwrap()
                    .starts_with(&original)
            );
        }
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
}

#[tokio::test]
async fn inflight_crash_persists_attempt_and_retries_only_unaccepted_fragment() {
    let (dir, store, receipt, models) = fixture();
    let (requests, mut dispatched) = mpsc::unbounded_channel();
    let mut actor = Actor::new_owned(store, Arc::new(BarrierExecutor { requests })).unwrap();
    let (run, pending) = seeded(&mut actor, &receipt.thread, false);
    let (done, _completed) = mpsc::channel(16);
    actor
        .compact_step(&run, &receipt.thread, pending, &done)
        .unwrap();
    let (first, _response) = dispatch(&mut dispatched).await;
    assert_eq!(state(&actor, &receipt.thread).attempts, 1);
    for (_, worker) in actor.workers.drain() {
        worker.task.abort();
        let _ = worker.task.await;
    }
    drop(actor);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), receipt.session.clone())
            .unwrap();
    store.adopt_claim(claim, true).unwrap();
    let (requests, mut dispatched) = mpsc::unbounded_channel();
    let mut actor = Actor::new_owned(store, Arc::new(BarrierExecutor { requests })).unwrap();
    actor
        .store
        .command(
            &Command::Resume {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
            },
            &models,
        )
        .unwrap();
    assert_eq!(state(&actor, &receipt.thread).attempts, 1);
    let (done, mut completed) = mpsc::channel(16);
    actor.request(&run, &receipt.thread, &done).unwrap();
    let (second, response) = dispatch(&mut dispatched).await;
    assert_eq!(first.messages[0].text(), second.messages[0].text());
    assert_eq!(state(&actor, &receipt.thread).attempts, 2);
    response.send(answer("accepted after reopen")).unwrap();
    finish(&mut actor, &mut completed).await;
    assert!(state(&actor, &receipt.thread).offset > 0);
    assert!(actor
        .context_snapshot(&receipt.thread)
        .unwrap()
        .checkpoint
        .is_none());
}

#[tokio::test]
async fn invalid_summaries_exhaust_durably_and_explicit_compact_resets() {
    let (dir, store, receipt, models) = fixture();
    let (requests, mut dispatched) = mpsc::unbounded_channel();
    let mut actor = Actor::new_owned(store, Arc::new(BarrierExecutor { requests })).unwrap();
    let (run, mut pending) = seeded(&mut actor, &receipt.thread, false);
    pending.summary.clear();
    pending.save(&actor.store.db, &receipt.thread).unwrap();
    let original = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    let (done, mut completed) = mpsc::channel(16);
    for index in 0..5 {
        actor.request(&run, &receipt.thread, &done).unwrap();
        let (_, response) = dispatch(&mut dispatched).await;
        let mut invalid = answer("");
        match index % 3 {
            0 => {}
            1 => {
                invalid.message.content = vec![ContentBlock::Thinking {
                    thinking: "reasoning only".into(),
                    signature: "opaque".into(),
                }]
            }
            _ => {
                invalid = answer("text plus forbidden tool");
                invalid.message.content.push(ContentBlock::ToolUse {
                    id: "bad".into(),
                    name: "write_file".into(),
                    input: json!({}),
                });
            }
        }
        response.send(invalid).unwrap();
        finish(&mut actor, &mut completed).await;
        let pending = state(&actor, &receipt.thread);
        assert_eq!((pending.cursor, pending.offset), (0, 0));
        assert_eq!(
            crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap(),
            original
        );
    }
    assert!(state(&actor, &receipt.thread).phase == Phase::Blocked);
    assert_eq!(state(&actor, &receipt.thread).attempts, 5);
    drop(actor);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), receipt.session.clone())
            .unwrap();
    store.adopt_claim(claim, true).unwrap();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    for _ in 0..3 {
        assert!(actor
            .store
            .command(
                &Command::Resume {
                    id: CommandId::new(),
                    thread: receipt.thread.clone()
                },
                &models
            )
            .unwrap_err()
            .to_string()
            .contains("Compaction blocked"));
    }
    let revision = actor.context_snapshot(&receipt.thread).unwrap().revision;
    actor
        .store
        .command(
            &Command::Compact {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                expected_revision: revision,
            },
            &models,
        )
        .unwrap();
    let count: i64 = actor
        .store
        .db
        .query_row("SELECT COUNT(*) FROM compactions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap(),
        original
    );
}

#[tokio::test]
async fn output_truncation_replans_input_and_output_without_accepting_progress() {
    let (_dir, store, receipt, _) = fixture();
    let (requests, mut dispatched) = mpsc::unbounded_channel();
    let mut actor = Actor::new_owned(store, Arc::new(BarrierExecutor { requests })).unwrap();
    let (run, pending) = seeded(&mut actor, &receipt.thread, false);
    let (done, mut completed) = mpsc::channel(16);
    actor
        .compact_step(&run, &receipt.thread, pending, &done)
        .unwrap();
    let (first, response) = dispatch(&mut dispatched).await;
    let mut truncated = answer("unusable partial summary");
    truncated.stop_reason = StopReason::MaxTokens;
    response.send(truncated).unwrap();
    finish(&mut actor, &mut completed).await;
    assert_eq!(
        (
            state(&actor, &receipt.thread).cursor,
            state(&actor, &receipt.thread).offset
        ),
        (0, 0)
    );
    actor.request(&run, &receipt.thread, &done).unwrap();
    let (second, response) = dispatch(&mut dispatched).await;
    assert!(second.max_tokens > first.max_tokens);
    assert!(estimate_input_tokens(&second).unwrap() < estimate_input_tokens(&first).unwrap());
    assert!(
        estimate_input_tokens(&second).unwrap()
            <= crate::context::input_limit(
                &second.model,
                &ModelCapabilities::default(),
                second.max_tokens
            )
    );
    response.send(answer("valid smaller summary")).unwrap();
    finish(&mut actor, &mut completed).await;
}

#[test]
fn projection_omits_opaque_reasoning_and_resumes_at_utf8_boundaries() {
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text {
                text: "Visible 🦀 text".into(),
            },
            ContentBlock::Thinking {
                thinking: "hidden reasoning".into(),
                signature: "SECRET_SIGNATURE".into(),
            },
            ContentBlock::RedactedThinking {
                data: "SECRET_REDACTED".into(),
            },
            ContentBlock::ReasoningItem {
                item: json!({"opaque":"SECRET_ITEM"}),
            },
            ContentBlock::ToolUse {
                id: "call".into(),
                name: "read_file".into(),
                input: json!({"path":"visible"}),
            },
        ],
    };
    let projected = projection(&message);
    assert!(projected.contains("Visible 🦀 text"));
    assert!(projected.contains("read_file"));
    assert!(!projected.contains("SECRET"));
    assert!(!projected.contains("hidden reasoning"));
    let mut offset = 0;
    let mut rebuilt = String::new();
    while offset < projected.len() {
        let end = end_boundary(&projected, offset, 5);
        assert!(end > offset);
        rebuilt.push_str(&projected[offset..end]);
        offset = end;
    }
    assert_eq!(rebuilt, projected);
}
