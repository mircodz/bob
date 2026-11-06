use super::*;

type ModelDispatch = (
    RequestSpec,
    tokio::sync::oneshot::Sender<anyhow::Result<Completion>>,
);

struct SchedulerExecutor {
    window: usize,
    requests: mpsc::UnboundedSender<ModelDispatch>,
}

#[async_trait]
impl Executor for SchedulerExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "test".into(),
                context_window: self.window,
            },
            ModelInfo {
                id: "larger".into(),
                context_window: self.window * 2,
            },
        ]
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    fn model_capabilities(&self, _: &str) -> ModelCapabilities {
        if self.window >= 2_000_000 {
            ModelCapabilities {
                max_input_tokens: None,
                max_output_tokens: 65_536,
                preferred_output_tokens: 40_000,
            }
        } else {
            ModelCapabilities::default()
        }
    }
    fn classify_error(&self, _: &anyhow::Error) -> ModelFailureKind {
        ModelFailureKind::ContextOverflow
    }
    async fn generate(
        &self,
        spec: RequestSpec,
        _: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.requests.send((spec, tx)).unwrap();
        rx.await?
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

async fn scheduled_model(
    actor: &mut Actor,
    done: &mpsc::Sender<Finished>,
    completed: &mut mpsc::Receiver<Finished>,
) -> Option<String> {
    for _ in 0..4 {
        actor.schedule(done).unwrap();
        if let Some(operation) = actor.workers.keys().next() {
            let name: String = actor
                .store
                .db
                .query_row(
                    "SELECT name FROM operations WHERE id=?1",
                    [operation],
                    |r| r.get(0),
                )
                .unwrap();
            if name == "context" {
                finish(actor, completed).await;
            } else {
                return Some(name);
            }
        }
    }
    None
}

fn candidate(actor: &Actor, thread: &ThreadId, pending: &CompactionState) -> RequestSpec {
    let mut spec = actor.model_spec(thread).unwrap();
    spec.messages.push(Message::user_text(format!(
        "[conversation summary]\n{}\n\n[Protected task and steering, verbatim]\n{}",
        pending.summary, pending.newest_request,
    )));
    let mut stmt = actor
        .store
        .db
        .prepare("SELECT model FROM working WHERE thread=?1 AND ordinal>=?2 ORDER BY ordinal")
        .unwrap();
    let ids = stmt
        .query_map(params![thread, pending.tail_start], |r| {
            r.get::<_, String>(0)
        })
        .unwrap();
    for id in ids {
        spec.messages
            .push(load_message(&actor.store.db, &ArtifactId(id.unwrap())).unwrap());
    }
    spec
}

async fn serialization_case(mode: &str) {
    let (dir, store, receipt, _) = fixture();
    let (requests, mut dispatched) = mpsc::unbounded_channel();
    let executor = Arc::new(SchedulerExecutor {
        window: 2_000_000,
        requests,
    });
    let models = executor.models();
    let mut actor = Actor::new_owned(store, executor).unwrap();
    let run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state,compact_only) VALUES(?1,?2,'active',1)",
            params![run, receipt.thread],
        )
        .unwrap();
    let escaped = "\u{1}".repeat(64 * 1024);
    let peer = ThreadId::new();
    for index in 0..9 {
        let source = if index == 0 {
            "human"
        } else if index <= 4 {
            "steering"
        } else {
            "coordination"
        };
        let id = CommandId::new();
        submit(
            &actor.store.db,
            &id,
            &receipt.thread,
            source,
            (index > 4).then_some(&peer),
            &escaped,
        )
        .unwrap();
        consume(&actor.store.db, &id, &run).unwrap();
    }
    if mode != "manifest" {
        for _ in 0..8 {
            let message = answer("small recent result").message;
            entry(
                &actor.store.db,
                &receipt.thread,
                "assistant",
                "small recent result",
                EntryData {
                    message: Some(&message),
                    ..Default::default()
                },
            )
            .unwrap();
        }
    }
    let original = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    let transcript = history(&actor, &receipt.thread);
    let before = actor.context_snapshot(&receipt.thread).unwrap();
    actor
        .store
        .command(
            &Command::Compact {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                expected_revision: before.revision,
            },
            &models,
        )
        .unwrap();
    let (done, mut completed) = mpsc::channel(16);
    let oversized_summary = "\u{1}".repeat(128 * 1024);
    let mut saw_oversized_candidate = false;
    let mut saw_reduction = false;
    let mut settled = false;
    for _ in 0..80 {
        if let Some(name) = scheduled_model(&mut actor, &done, &mut completed).await {
            assert_eq!(name, "compact");
            let (spec, response) = tokio::time::timeout(Duration::from_secs(5), dispatched.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(spec.system, SUMMARY_SYSTEM);
            assert!(encode(&spec).unwrap().len() <= MAX_ARTIFACT);
            let pending = state(&actor, &receipt.thread);
            let reducing = pending.phase == Phase::Reduce;
            saw_reduction |= reducing;
            let completion = answer(if mode == "manifest" || (reducing && mode == "reduce") {
                "small summary"
            } else {
                &oversized_summary
            });
            assert!(estimate_message(&completion.message) <= spec.max_tokens as usize);
            response.send(Ok(completion)).unwrap();
            finish(&mut actor, &mut completed).await;
            let pending = state(&actor, &receipt.thread);
            if pending.phase == Phase::Validate {
                let spec = candidate(&actor, &receipt.thread, &pending);
                let caps = actor.model_capabilities(&spec.model.id);
                assert!(
                    estimate_input_tokens(&spec).unwrap()
                        < crate::context::input_limit(&spec.model, &caps, spec.max_tokens) * 4 / 5
                );
                if !saw_reduction {
                    if mode == "manifest" {
                        assert!(spec
                            .messages
                            .iter()
                            .all(|message| encode(&ModelRecord::Message(message.clone()))
                                .unwrap()
                                .len()
                                <= MAX_ARTIFACT));
                        assert!(encode(&spec).unwrap().len() > MAX_ARTIFACT);
                    } else {
                        assert!(
                            encode(&ModelRecord::Message(spec.messages[0].clone()))
                                .unwrap()
                                .len()
                                > MAX_ARTIFACT
                        );
                    }
                    saw_oversized_candidate = true;
                }
            }
        }
        assert_eq!(history(&actor, &receipt.thread), transcript);
        let snapshot = actor.context_snapshot(&receipt.thread).unwrap();
        if snapshot.checkpoint.is_some() {
            assert_eq!(mode, "reduce");
            assert!(saw_reduction);
            assert_eq!(snapshot.revision, before.revision + 1);
            let mut spec = actor.model_spec(&receipt.thread).unwrap();
            spec.messages = crate::context::working_ids(&actor.store.db, &receipt.thread)
                .unwrap()
                .iter()
                .map(|id| load_message(&actor.store.db, id).unwrap())
                .collect();
            assert!(checkpoint_retention_error(&spec).unwrap().is_none());
            assert!(spec.messages[0]
                .text()
                .contains(&protected_context(&actor.store.db, &receipt.thread).unwrap()));
            settled = true;
            break;
        }
        assert_eq!(
            crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap(),
            original
        );
        assert_eq!(snapshot.revision, before.revision);
        let blocked: bool = actor.store.db.query_row("SELECT EXISTS(SELECT 1 FROM compactions WHERE thread=?1 AND json_extract(state,'$.phase')='blocked')", [&receipt.thread], |r| r.get(0)).unwrap();
        if blocked {
            assert_ne!(mode, "reduce");
            assert!(scheduled_model(&mut actor, &done, &mut completed)
                .await
                .is_none());
            let pending = state(&actor, &receipt.thread);
            assert!(pending
                .blocked
                .contains("Original working context retained"));
            if mode == "manifest" {
                assert!(pending.blocked.contains("serialized request manifest"));
                assert_eq!(pending.reductions, 0);
            } else {
                assert!(saw_reduction);
            }
            let status: (String, bool) = actor.store.db.query_row("SELECT r.state,t.held FROM runs r JOIN threads t ON t.id=r.thread WHERE r.id=?1", [&run], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
            assert_eq!(status, ("interrupted".into(), true));
            assert!(actor.workers.is_empty());
            assert!(actor
                .store
                .command(
                    &Command::Resume {
                        id: CommandId::new(),
                        thread: receipt.thread.clone()
                    },
                    &models
                )
                .is_err());
            drop(actor);
            let store = Store::open(&dir.0.join("lab.db")).unwrap();
            assert_eq!(
                crate::context::working_ids(&store.db, &receipt.thread).unwrap(),
                original
            );
            let restored: CompactionState = decode(
                store
                    .db
                    .query_row(
                        "SELECT state FROM compactions WHERE thread=?1",
                        [&receipt.thread],
                        |r| r.get(0),
                    )
                    .unwrap(),
            )
            .unwrap();
            assert!(restored.phase == Phase::Blocked);
            assert_eq!(restored.attempts, pending.attempts);
            assert!(saw_oversized_candidate);
            return;
        }
    }
    assert!(settled, "scheduler did not settle compaction");
    assert!(saw_oversized_candidate);
}

#[tokio::test]
async fn scheduler_reduces_token_fitting_serialization_oversized_checkpoint() {
    serialization_case("reduce").await;
}

#[tokio::test]
async fn scheduler_blocks_unreducible_serialization_oversized_checkpoint_durably() {
    serialization_case("block").await;
}

#[tokio::test]
async fn scheduler_blocks_full_manifest_overflow_before_checkpoint_publication() {
    serialization_case("manifest").await;
}

#[tokio::test]
async fn ordinary_provider_overflow_then_larger_profile_recovers_without_stale_ceiling() {
    let (_dir, store, receipt, _) = fixture();
    let (requests, mut dispatched) = mpsc::unbounded_channel();
    let executor = Arc::new(SchedulerExecutor {
        window: 20_000,
        requests,
    });
    let models = executor.models();
    let mut actor = Actor::new_owned(store, executor).unwrap();
    for _ in 0..8 {
        let message = answer("earlier result").message;
        entry(
            &actor.store.db,
            &receipt.thread,
            "assistant",
            "earlier result",
            EntryData {
                message: Some(&message),
                ..Default::default()
            },
        )
        .unwrap();
    }
    let task = "p".repeat(8000);
    actor
        .store
        .command(
            &Command::Submit {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                text: task.clone(),
            },
            &models,
        )
        .unwrap();
    let (done, mut completed) = mpsc::channel(16);
    assert_eq!(
        scheduled_model(&mut actor, &done, &mut completed)
            .await
            .as_deref(),
        Some("model")
    );
    let (ordinary, response) = tokio::time::timeout(Duration::from_secs(5), dispatched.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ordinary.model.id, "test");
    let old_ceiling = estimate_input_tokens(&ordinary).unwrap() * 3 / 4;
    response
        .send(Err(anyhow::anyhow!("ordinary provider context overflow")))
        .unwrap();
    finish(&mut actor, &mut completed).await;
    let recorded: usize = actor.store.db.query_row("SELECT json_extract(input,'$.overflow_input_limit') FROM operations WHERE thread=?1 AND name='model' ORDER BY rowid DESC LIMIT 1", [&receipt.thread], |r| r.get(0)).unwrap();
    assert_eq!(recorded, old_ceiling);
    let original = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    let transcript = history(&actor, &receipt.thread);
    assert!(scheduled_model(&mut actor, &done, &mut completed)
        .await
        .is_none());
    assert!(state(&actor, &receipt.thread).phase == Phase::Blocked);
    assert_eq!(
        crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap(),
        original
    );
    assert!(actor
        .context_snapshot(&receipt.thread)
        .unwrap()
        .checkpoint
        .is_none());
    assert!(actor
        .store
        .command(
            &Command::Resume {
                id: CommandId::new(),
                thread: receipt.thread.clone()
            },
            &models
        )
        .is_err());
    let before = actor.context_snapshot(&receipt.thread).unwrap();
    let mut profile = before.profile;
    profile.model = "larger".into();
    actor
        .store
        .command(
            &Command::UpdateProfile {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                profile,
                expected_revision: before.profile_revision,
            },
            &models,
        )
        .unwrap();
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
    let mut recovered = false;
    let mut summarized = false;
    for _ in 0..16 {
        if let Some(name) = scheduled_model(&mut actor, &done, &mut completed).await {
            let (spec, response) = tokio::time::timeout(Duration::from_secs(5), dispatched.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(spec.model.id, "larger");
            if name == "compact" {
                let pending = state(&actor, &receipt.thread);
                assert_eq!(pending.profile_revision, before.profile_revision + 1);
                assert_eq!(pending.final_input_ceiling, None);
                summarized = true;
            } else {
                assert_eq!(name, "model");
                assert!(summarized);
                assert!(actor
                    .context_snapshot(&receipt.thread)
                    .unwrap()
                    .checkpoint
                    .is_some());
                assert!(estimate_input_tokens(&spec).unwrap() > old_ceiling);
                assert!(spec.messages[0].text().contains(&task));
                assert_eq!(history(&actor, &receipt.thread), transcript);
                recovered = true;
            }
            response
                .send(Ok(answer("small summary or recovered answer")))
                .unwrap();
            finish(&mut actor, &mut completed).await;
            if recovered {
                break;
            }
        }
    }
    assert!(
        recovered,
        "larger model remained blocked by the old ordinary overflow"
    );
    let status: String = actor
        .store
        .db
        .query_row(
            "SELECT state FROM runs WHERE thread=?1 ORDER BY ordinal DESC LIMIT 1",
            [&receipt.thread],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "completed");
}
