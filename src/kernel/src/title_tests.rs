use super::*;
use crate::store::tests::{fixture, TempDir};
use crate::{ModelProgress, ModelUpdate, SessionCatalog, ThreadState};
use async_trait::async_trait;
use tokio::sync::Semaphore;

struct TitleExecutor {
    models: Vec<ModelInfo>,
    calls: Mutex<Vec<RequestSpec>>,
    permits: Semaphore,
    completion: Completion,
}

fn answer(text: &str) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        },
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: 17,
            output_tokens: 5,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 3,
        },
    }
}

#[async_trait]
impl Executor for TitleExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        request: RequestSpec,
        _: watch::Receiver<bool>,
        progress: ModelProgress,
    ) -> anyhow::Result<Completion> {
        let title = request.system == titles::INSTRUCTION;
        self.calls.lock().unwrap().push(request);
        if title {
            self.permits.acquire().await.unwrap().forget();
            progress
                .emit(ModelUpdate::Text("Improve session title handling".into()))
                .await?;
            Ok(self.completion.clone())
        } else {
            Ok(answer("Ordinary assistant reply"))
        }
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

fn setup(permits: usize) -> (TempDir, Actor, Receipt, Arc<TitleExecutor>) {
    let (dir, store, old, models) = fixture();
    let executor = Arc::new(TitleExecutor {
        models,
        calls: Mutex::new(Vec::new()),
        permits: Semaphore::new(permits),
        completion: answer("Improve session title handling"),
    });
    let mut actor = Actor::new(store, executor.clone()).unwrap();
    actor.options.auto_titles = true;
    let snapshot = crate::sessions::session(&actor.store.db, &old.session).unwrap();
    let root = actor
        .accept_command(&Command::CreateSession {
            id: CommandId::new(),
            workspace: snapshot.workspace,
            profile: Profile {
                instructions: "Never send this project instruction to the title model".into(),
                ..snapshot.profile
            },
            policy: Policy::default(),
        })
        .unwrap();
    assert_eq!(actor.selected_session.as_ref(), Some(&root.session));
    (dir, actor, root, executor)
}

fn submit(actor: &mut Actor, root: &Receipt, text: &str) -> Command {
    let command = Command::Submit {
        id: CommandId::new(),
        thread: root.thread.clone(),
        text: text.into(),
    };
    actor.accept_command(&command).unwrap();
    command
}

fn state(actor: &Actor, root: &Receipt) -> String {
    actor
        .store
        .db
        .query_row(
            "SELECT state FROM session_titles WHERE session=?1",
            [&root.session],
            |r| r.get(0),
        )
        .unwrap()
}

async fn finish_title(actor: &mut Actor) {
    let result = tokio::time::timeout(Duration::from_secs(2), actor.title_rx.recv())
        .await
        .unwrap()
        .unwrap();
    actor.finish_title(result).unwrap();
    actor.publish_live().unwrap();
}

fn metadata_counts(actor: &Actor) -> (i64, i64, i64, i64, i64, i64) {
    actor.store.db.query_row(
        "SELECT (SELECT COUNT(*) FROM entries),(SELECT COUNT(*) FROM working),(SELECT COUNT(*) FROM operations),(SELECT COUNT(*) FROM runs),(SELECT COUNT(*) FROM jobs),(SELECT COUNT(*) FROM model_context)",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)),
    ).unwrap()
}

#[tokio::test]
async fn title_is_once_only_bounded_metadata_with_usage_and_no_recency_change() {
    let (_dir, mut actor, root, executor) = setup(1);
    let command = submit(&mut actor, &root, &"界".repeat(3000));
    let before = metadata_counts(&actor);
    let recency: (i64, i64) = actor
        .store
        .db
        .query_row(
            "SELECT updated,updated_at FROM sessions WHERE id=?1",
            [&root.session],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let sequence = durable_sequence(&actor.store.db).unwrap();
    let mut watch = actor.live.subscribe();
    watch.borrow_and_update();
    actor.accept_command(&command).unwrap();
    actor.schedule_title().unwrap();
    actor.schedule_title().unwrap();
    assert!(actor.workers.is_empty());
    assert!(actor.live.subscribe().borrow().attempts.is_empty());
    assert_eq!(state(&actor, &root), "running");
    finish_title(&mut actor).await;
    assert!(watch.has_changed().unwrap());
    assert!(watch.borrow().sequence > sequence);
    assert_eq!(state(&actor, &root), "generated");
    assert_eq!(metadata_counts(&actor), before);
    assert_eq!(
        recency,
        actor
            .store
            .db
            .query_row(
                "SELECT updated,updated_at FROM sessions WHERE id=?1",
                [&root.session],
                |r| Ok((r.get(0)?, r.get(1)?))
            )
            .unwrap()
    );
    let snapshot = crate::sessions::session(&actor.store.db, &root.session).unwrap();
    assert_eq!(snapshot.title, "Improve session title handling");
    let page = crate::sessions::sessions(
        &actor.store.db,
        SessionQuery {
            search: Some("session title".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].title, snapshot.title);
    let usage = crate::sessions::usage(&actor.store.db, Some(&root.session)).unwrap();
    assert_eq!(usage.requests, 1);
    assert_eq!(usage.compaction_requests, 0);
    assert_eq!(usage.usage.input_tokens, 17);
    assert_eq!(usage.usage.output_tokens, 5);
    assert_eq!(usage.usage.cache_creation_input_tokens, 2);
    assert_eq!(usage.usage.cache_read_input_tokens, 3);
    assert_eq!(
        crate::sessions::usage(&actor.store.db, None)
            .unwrap()
            .requests,
        1
    );
    let calls = executor.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let request = &calls[0];
    assert_eq!(request.model.id, "test");
    assert_eq!(request.reasoning, ReasoningEffort::Off);
    assert_eq!(request.max_tokens, 128);
    assert!(request.tools.is_empty());
    assert_eq!(request.system, titles::INSTRUCTION);
    let [message] = request.messages.as_slice() else {
        panic!("one bounded message expected")
    };
    let [ContentBlock::Text { text }] = message.content.as_slice() else {
        panic!("text only")
    };
    assert!(text.len() <= titles::INPUT_BYTES);
    assert!(text.chars().all(|c| c == '界'));
    drop(calls);
    submit(&mut actor, &root, "An entirely different second task");
    actor.schedule_title().unwrap();
    assert!(actor.title_worker.is_none());
    assert_eq!(executor.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn title_never_blocks_ordinary_completion_but_must_settle_before_session_switch() {
    let (_dir, mut actor, root, _) = setup(0);
    submit(&mut actor, &root, "First ordinary request");
    let (done, mut completed) = mpsc::channel(64);
    actor.schedule(&done).unwrap();
    actor.schedule_title().unwrap();
    for _ in 0..8 {
        let finished = tokio::time::timeout(Duration::from_secs(2), completed.recv())
            .await
            .unwrap()
            .unwrap();
        let worker = actor.workers.remove(&finished.id).unwrap();
        worker.task.await.unwrap();
        actor.finish(finished).unwrap();
        actor.schedule(&done).unwrap();
        actor.schedule(&done).unwrap();
        if actor.workers.is_empty() {
            break;
        }
    }
    let view = actor
        .store
        .view(Some(root.thread.clone()), None, 100, None)
        .unwrap();
    assert_eq!(
        view.threads
            .iter()
            .find(|thread| thread.id == root.thread)
            .unwrap()
            .state,
        ThreadState::Completed
    );
    assert!(view
        .entries
        .iter()
        .any(|entry| entry.text == "Ordinary assistant reply"));
    assert!(!view
        .entries
        .iter()
        .any(|entry| entry.text == "Improve session title handling"));
    assert!(actor.title_worker.is_some());
    assert!(actor.workers.is_empty());
    let snapshot = crate::sessions::session(&actor.store.db, &root.session).unwrap();
    let create = Command::CreateSession {
        id: CommandId::new(),
        workspace: snapshot.workspace,
        profile: snapshot.profile,
        policy: Policy::default(),
    };
    assert!(actor.accept_command(&create).is_err());
    actor.cancel_titles(true).unwrap();
    assert!(actor.title_worker.is_some());
    assert!(actor.accept_command(&create).is_err());
    finish_title(&mut actor).await;
    let next = actor.accept_command(&create).unwrap();
    actor.activate_session(next.session, Vec::new()).unwrap();
    assert!(actor.title_worker.is_none());
    assert_eq!(state(&actor, &root), "fallback");
    assert_eq!(
        crate::sessions::usage(&actor.store.db, Some(&root.session))
            .unwrap()
            .requests,
        2
    );
}

#[tokio::test]
async fn stop_clear_and_profile_change_fence_results_and_never_retry() {
    for action in ["stop", "clear", "profile"] {
        let (_dir, mut actor, root, _) = setup(0);
        submit(&mut actor, &root, "Keep the fallback title");
        actor.schedule_title().unwrap();
        let snapshot = crate::sessions::session(&actor.store.db, &root.session).unwrap();
        actor.models.push(ModelInfo {
            id: "other".into(),
            context_window: 100_000,
        });
        let command = match action {
            "stop" => Command::StopThread {
                id: CommandId::new(),
                thread: root.thread.clone(),
            },
            "clear" => Command::ClearContext {
                id: CommandId::new(),
                thread: root.thread.clone(),
                expected_epoch: snapshot.context_epoch,
            },
            _ => Command::UpdateProfile {
                id: CommandId::new(),
                thread: root.thread.clone(),
                expected_revision: snapshot.profile_revision,
                profile: Profile {
                    model: "other".into(),
                    instructions: "new profile".into(),
                    ..snapshot.profile
                },
            },
        };
        actor.accept_command(&command).unwrap();
        actor
            .finish_title(TitleFinished {
                session: root.session.clone(),
                title: Some("Discard stale generated title".into()),
                usage: Usage::default(),
            })
            .unwrap();
        assert_eq!(state(&actor, &root), "fallback");
        assert_eq!(
            crate::sessions::session(&actor.store.db, &root.session)
                .unwrap()
                .title,
            "Keep the fallback title"
        );
        actor.cancel_titles(false).unwrap();
        assert!(actor.title_worker.is_none());
        submit(&mut actor, &root, "Next request must not retry");
        actor.schedule_title().unwrap();
        assert!(actor.title_worker.is_none());
        assert_eq!(
            crate::sessions::usage(&actor.store.db, Some(&root.session))
                .unwrap()
                .requests,
            1
        );
    }
}

#[tokio::test]
async fn queued_completion_after_cancellation_accounts_usage_once_without_applying_title() {
    let (_dir, mut actor, root, executor) = setup(1);
    submit(&mut actor, &root, "Keep the fallback title");
    actor.schedule_title().unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        &mut actor.title_worker.as_mut().unwrap().task,
    )
    .await
    .unwrap()
    .unwrap();
    actor.cancel_titles(true).unwrap();
    assert!(actor.title_worker.is_some());
    let finished = actor.title_rx.try_recv().unwrap();
    let duplicate = TitleFinished {
        session: finished.session.clone(),
        title: finished.title.clone(),
        usage: finished.usage,
    };
    actor.finish_title(finished).unwrap();
    assert!(actor.title_worker.is_none());
    actor
        .accept_command(&Command::StopThread {
            id: CommandId::new(),
            thread: root.thread.clone(),
        })
        .unwrap();
    let snapshot = crate::sessions::session(&actor.store.db, &root.session).unwrap();
    let next = actor
        .accept_command(&Command::CreateSession {
            id: CommandId::new(),
            workspace: snapshot.workspace,
            profile: snapshot.profile,
            policy: Policy::default(),
        })
        .unwrap();
    assert_eq!(actor.selected_session.as_ref(), Some(&next.session));
    submit(&mut actor, &next, "Newer session title stays untouched");
    actor.schedule_title().unwrap();
    let sequence = durable_sequence(&actor.store.db).unwrap();
    actor.finish_title(duplicate).unwrap();
    assert_eq!(durable_sequence(&actor.store.db).unwrap(), sequence);
    assert_eq!(state(&actor, &root), "fallback");
    assert_eq!(
        crate::sessions::session(&actor.store.db, &root.session)
            .unwrap()
            .title,
        "Keep the fallback title"
    );
    assert_eq!(actor.title_worker.as_ref().unwrap().session, next.session);
    assert_eq!(state(&actor, &next), "running");
    assert_eq!(
        crate::sessions::session(&actor.store.db, &next.session)
            .unwrap()
            .title,
        "Newer session title stays untouched"
    );
    let usage = crate::sessions::usage(&actor.store.db, Some(&root.session)).unwrap();
    assert_eq!(usage.requests, 1);
    assert_eq!(usage.compaction_requests, 0);
    assert_eq!(usage.usage.input_tokens, 17);
    assert_eq!(usage.usage.output_tokens, 5);
    assert_eq!(usage.usage.cache_creation_input_tokens, 2);
    assert_eq!(usage.usage.cache_read_input_tokens, 3);
    let total = crate::sessions::usage(&actor.store.db, None).unwrap();
    assert_eq!(total.requests, 2);
    assert_eq!(total.usage.input_tokens, 17);
    assert_eq!(executor.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unknown_and_undispatched_title_completions_are_ignored() {
    let (_dir, mut actor, root, executor) = setup(0);
    submit(
        &mut actor,
        &root,
        "Do not accept an undispatched completion",
    );
    let sequence = durable_sequence(&actor.store.db).unwrap();
    for session in [SessionId::new(), root.session.clone()] {
        actor
            .finish_title(TitleFinished {
                session,
                title: Some("Reject this unexpected title".into()),
                usage: answer("Reject this unexpected title").usage,
            })
            .unwrap();
    }
    assert_eq!(durable_sequence(&actor.store.db).unwrap(), sequence);
    assert_eq!(state(&actor, &root), "queued");
    let usage = crate::sessions::usage(&actor.store.db, None).unwrap();
    assert_eq!(usage.requests, 0);
    assert_eq!(usage.usage.input_tokens, 0);
    assert_eq!(usage.usage.output_tokens, 0);
    assert_eq!(usage.usage.cache_creation_input_tokens, 0);
    assert_eq!(usage.usage.cache_read_input_tokens, 0);
    assert!(executor.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn snapshot_title_revision_tracks_the_sqlite_read_snapshot() {
    let (dir, mut actor, root, _) = setup(1);
    submit(&mut actor, &root, "Keep this title until completion");
    actor.schedule_title().unwrap();
    let reader = rusqlite::Connection::open_with_flags(
        dir.0.join("lab.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let before = crate::sessions::session(&reader, &root.session).unwrap();
    assert_eq!(before.title, "Keep this title until completion");
    assert_eq!(before.title_revision, durable_sequence(&reader).unwrap());
    finish_title(&mut actor).await;
    let pinned = crate::sessions::session(&reader, &root.session).unwrap();
    assert_eq!(pinned.title, before.title);
    assert_eq!(pinned.title_revision, before.title_revision);
    reader.execute_batch("COMMIT").unwrap();
    let after = crate::sessions::session(&reader, &root.session).unwrap();
    assert_eq!(after.title, "Improve session title handling");
    assert!(after.title_revision > before.title_revision);
    assert_eq!(after.title_revision, durable_sequence(&reader).unwrap());
    actor
        .finish_title(TitleFinished {
            session: root.session.clone(),
            title: Some("Never replace a completed title".into()),
            usage: answer("Never replace a completed title").usage,
        })
        .unwrap();
    let duplicate = crate::sessions::session(&reader, &root.session).unwrap();
    assert_eq!(duplicate.title, after.title);
    assert_eq!(duplicate.title_revision, after.title_revision);
}

#[tokio::test]
async fn shutdown_and_crash_prevent_replay_but_preserve_unused_eligibility() {
    for stage in ["queued", "running", "shutdown", "eligible"] {
        let (dir, mut actor, root, executor) = setup(0);
        if stage != "eligible" {
            submit(&mut actor, &root, "Retain first request fallback");
        }
        if matches!(stage, "running" | "shutdown") {
            actor.schedule_title().unwrap();
        }
        if stage == "shutdown" {
            actor.cancel_titles(true).unwrap();
            assert!(actor.title_worker.is_some());
            finish_title(&mut actor).await;
            assert!(actor.title_worker.is_none());
        }
        drop(actor);
        let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
        let claim =
            crate::SessionClaim::acquire(store.path.as_deref().unwrap(), root.session.clone())
                .unwrap();
        store.adopt_claim(claim, true).unwrap();
        let mut actor = Actor::new(store, executor).unwrap();
        actor.options.auto_titles = true;
        assert_eq!(actor.selected_session.as_ref(), Some(&root.session));
        if stage == "eligible" {
            assert_eq!(state(&actor, &root), "eligible");
            submit(&mut actor, &root, "First request after reopening");
            assert_eq!(state(&actor, &root), "queued");
            actor.schedule_title().unwrap();
            assert!(actor.title_worker.is_some());
        } else {
            assert_eq!(state(&actor, &root), "fallback");
            actor.schedule_title().unwrap();
            assert!(actor.title_worker.is_none());
        }
        assert_eq!(
            crate::sessions::usage(&actor.store.db, Some(&root.session))
                .unwrap()
                .requests,
            u64::from(matches!(stage, "running" | "shutdown" | "eligible"))
        );
    }
}

#[tokio::test]
async fn completed_titles_survive_clear_and_reopen_unchanged() {
    let (dir, mut actor, root, executor) = setup(1);
    submit(&mut actor, &root, "Make automatic session titles");
    actor.schedule_title().unwrap();
    finish_title(&mut actor).await;
    actor
        .accept_command(&Command::ClearContext {
            id: CommandId::new(),
            thread: root.thread.clone(),
            expected_epoch: 0,
        })
        .unwrap();
    drop(actor);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), root.session.clone()).unwrap();
    store.adopt_claim(claim, true).unwrap();
    let mut actor = Actor::new(store, executor.clone()).unwrap();
    actor.options.auto_titles = true;
    submit(&mut actor, &root, "Completely different new task");
    actor.schedule_title().unwrap();
    assert!(actor.title_worker.is_none());
    assert_eq!(executor.calls.lock().unwrap().len(), 1);
    assert_eq!(
        crate::sessions::session(&actor.store.db, &root.session)
            .unwrap()
            .title,
        "Improve session title handling"
    );
}

#[tokio::test]
async fn defaults_and_old_sessions_do_not_generate_titles() {
    assert!(!RuntimeOptions::default().auto_titles);
    let (_dir, store, root, models) = fixture();
    let executor = Arc::new(TitleExecutor {
        models,
        calls: Mutex::new(Vec::new()),
        permits: Semaphore::new(1),
        completion: answer("Never generate this title"),
    });
    let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
    submit(&mut actor, &root, "Default headless first request");
    actor.schedule_title().unwrap();
    actor.options.auto_titles = true;
    submit(&mut actor, &root, "Still not eligible later");
    actor.schedule_title().unwrap();
    assert!(actor.title_worker.is_none());
    assert_eq!(
        actor
            .store
            .db
            .query_row("SELECT COUNT(*) FROM session_titles", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        crate::sessions::session(&actor.store.db, &root.session)
            .unwrap()
            .title,
        "Default headless first request"
    );
    assert!(executor.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reservation_and_dispatch_are_transactional_and_deduplicated() {
    let (_dir, mut actor, root, executor) = setup(1);
    actor.store.db.execute_batch("CREATE TRIGGER fail_title_receipt BEFORE INSERT ON commands BEGIN SELECT RAISE(ABORT,'receipt failed'); END;").unwrap();
    let command = Command::Submit {
        id: CommandId::new(),
        thread: root.thread.clone(),
        text: "Atomic title reservation please".into(),
    };
    assert!(actor.accept_command(&command).is_err());
    assert_eq!(state(&actor, &root), "eligible");
    actor
        .store
        .db
        .execute_batch("DROP TRIGGER fail_title_receipt")
        .unwrap();
    actor.accept_command(&command).unwrap();
    actor.accept_command(&command).unwrap();
    actor.store.db.execute_batch("CREATE TRIGGER fail_title_dispatch BEFORE INSERT ON changes WHEN NEW.kind='session_metadata' BEGIN SELECT RAISE(ABORT,'metadata failed'); END;").unwrap();
    assert!(actor.schedule_title().is_err());
    assert!(actor.title_worker.is_none());
    assert_eq!(state(&actor, &root), "queued");
    assert!(executor.calls.lock().unwrap().is_empty());
    assert_eq!(
        crate::sessions::usage(&actor.store.db, Some(&root.session))
            .unwrap()
            .requests,
        0
    );
    actor
        .store
        .db
        .execute_batch("DROP TRIGGER fail_title_dispatch")
        .unwrap();
    actor.schedule_title().unwrap();
    finish_title(&mut actor).await;
    assert_eq!(executor.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn timeout_and_cancel_drop_a_noncooperative_async_provider() {
    let (_dir, actor, _, executor) = setup(0);
    let request = RequestSpec {
        model: actor.models[0].clone(),
        workspace: String::new(),
        system: titles::INSTRUCTION.into(),
        messages: vec![Message::user_text("Bound this title request")],
        tools: Vec::new(),
        reasoning: ReasoningEffort::Off,
        max_tokens: 128,
    };
    let (cancel, signal) = watch::channel(false);
    let (title, usage) = tokio::time::timeout(
        Duration::from_secs(1),
        generate_title(executor.clone(), request.clone(), signal, Duration::ZERO),
    )
    .await
    .unwrap();
    assert!(title.is_none());
    assert_eq!(usage.input_tokens, 0);
    cancel.send_replace(true);
    let (title, _) = generate_title(
        executor,
        request,
        cancel.subscribe(),
        Duration::from_secs(18),
    )
    .await;
    assert!(title.is_none());
}

#[tokio::test]
async fn stream_sink_is_bounded_invisible_and_rejects_tools() {
    let (sink, invalid) = ModelProgress::bounded_sink(8);
    sink.emit(ModelUpdate::Text("界界".into())).await.unwrap();
    assert!(sink.emit(ModelUpdate::Text("界".into())).await.is_err());
    assert!(invalid.load(Ordering::Acquire));
    let (sink, invalid) = ModelProgress::bounded_sink(256);
    assert!(sink
        .emit(ModelUpdate::Activity(crate::ModelActivity::ToolCall))
        .await
        .is_err());
    assert!(invalid.load(Ordering::Acquire));
}

#[test]
fn title_uses_only_public_text_when_provider_returns_reasoning_metadata() {
    for metadata in [
        ContentBlock::ReasoningItem {
            item: json!({"type":"reasoning", "encrypted_content":"opaque"}),
        },
        ContentBlock::RedactedThinking {
            data: "opaque".into(),
        },
        ContentBlock::Thinking {
            thinking: "Not a title source".into(),
            signature: "opaque".into(),
        },
    ] {
        let mut completion = answer("Plan Read-Only Kusto MCP Integration");
        completion.message.content.insert(0, metadata);
        assert_eq!(
            titles::validated(&completion).as_deref(),
            Some("Plan Read-Only Kusto MCP Integration")
        );
        completion.message.content.pop();
        assert!(titles::validated(&completion).is_none());
    }
}

#[test]
fn title_validation_rejects_controls_markup_lengths_tools_and_incomplete_output() {
    for text in [
        "two words",
        "one two three four five six seven eight",
        "\"Quoted title here\"",
        "# Markdown title here",
        "One title\nsecond line",
        "One title \u{1b}[31mred",
        "One title \u{9b}red",
        "One title \u{202e}reversed",
        "Trailing sentence punctuation.",
        "A title `code`",
        "   ",
    ] {
        assert!(titles::validated(&answer(text)).is_none(), "{text:?}");
    }
    assert!(titles::validated(&answer(&format!("{} two three", "x".repeat(256)))).is_none());
    let mut truncated = answer("Three useful title words");
    truncated.stop_reason = StopReason::MaxTokens;
    assert!(titles::validated(&truncated).is_none());
    truncated.stop_reason = StopReason::ToolUse;
    assert!(titles::validated(&truncated).is_none());
    truncated.stop_reason = StopReason::EndTurn;
    truncated.message.content.push(ContentBlock::ToolUse {
        id: "bad".into(),
        name: "bash".into(),
        input: json!({}),
    });
    assert!(titles::validated(&truncated).is_none());
    assert_eq!(
        titles::validated(&answer("  Improve Unicode title handling  ")).as_deref(),
        Some("Improve Unicode title handling")
    );
    assert_eq!(
        titles::validated(&answer("Améliorer les titres Unicode")).as_deref(),
        Some("Améliorer les titres Unicode")
    );
    let fallback = titles::fallback("\u{1b}bad\r\nnew\u{7} title\u{202e}");
    assert!(!fallback.chars().any(char::is_control));
    assert!(!fallback.contains('\u{202e}'));
    assert_eq!(titles::fallback(&"界".repeat(100)).chars().count(), 49);
}

#[tokio::test]
async fn invalid_completion_preserves_fallback_but_accounts_usage() {
    let (_dir, mut actor, root, _) = setup(0);
    submit(&mut actor, &root, "Fallback remains on invalid result");
    actor.schedule_title().unwrap();
    actor
        .finish_title(TitleFinished {
            session: root.session.clone(),
            title: None,
            usage: answer("Too short").usage,
        })
        .unwrap();
    assert_eq!(state(&actor, &root), "fallback");
    assert_eq!(
        crate::sessions::session(&actor.store.db, &root.session)
            .unwrap()
            .title,
        "Fallback remains on invalid result"
    );
    assert_eq!(
        crate::sessions::usage(&actor.store.db, Some(&root.session))
            .unwrap()
            .usage
            .input_tokens,
        17
    );
}

#[test]
fn v8_catalog_is_read_only_and_upgrade_preserves_transcript_without_backfill() {
    let (dir, mut store, root, models) = fixture();
    store
        .command(
            &Command::Submit {
                id: CommandId::new(),
                thread: root.thread.clone(),
                text: "An existing version eight conversation".into(),
            },
            &models,
        )
        .unwrap();
    store
        .db
        .execute_batch(
            "DROP TABLE session_titles; PRAGMA user_version=8; PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
    let before = store
        .transcript(Some(root.thread.clone()), None, 100, None)
        .unwrap();
    let title_revision = durable_sequence(&store.db).unwrap();
    drop(store);
    let path = dir.0.join("lab.db");
    let bytes = std::fs::read(&path).unwrap();
    let catalog = SessionCatalog::open(&path).unwrap();
    let session = catalog.session(root.session.clone()).unwrap();
    assert_eq!(session.title, "An existing version eight conversation");
    assert_eq!(session.title_revision, title_revision);
    assert_eq!(
        catalog
            .sessions(SessionQuery {
                search: Some("version eight".into()),
                ..Default::default()
            })
            .unwrap()
            .sessions[0]
            .title,
        session.title
    );
    assert_eq!(
        encode(
            &catalog
                .transcript(Some(root.thread.clone()), None, 100)
                .unwrap()
        )
        .unwrap(),
        encode(&before).unwrap()
    );
    drop(catalog);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store
            .db
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        crate::store::SCHEMA
    );
    assert_eq!(
        store
            .db
            .query_row("SELECT COUNT(*) FROM session_titles", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        encode(
            &store
                .transcript(Some(root.thread.clone()), None, 100, None)
                .unwrap()
        )
        .unwrap(),
        encode(&before).unwrap()
    );
    assert_eq!(
        crate::sessions::session(&store.db, &root.session)
            .unwrap()
            .title,
        session.title
    );
    let catalog = SessionCatalog::open(&path).unwrap();
    let upgraded = catalog.session(root.session).unwrap();
    assert_eq!(upgraded.title, session.title);
    assert_eq!(
        upgraded.title_revision,
        durable_sequence(&store.db).unwrap()
    );
}

#[tokio::test]
async fn only_ordinary_root_submissions_reserve_a_title() {
    let (_dir, mut actor, root, _) = setup(1);
    let run = RunId::new();
    let child = ThreadId::new();
    let profile = crate::sessions::session(&actor.store.db, &root.session)
        .unwrap()
        .profile;
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'completed')",
            params![run, root.thread],
        )
        .unwrap();
    actor
        .store
        .db
        .execute(
            "INSERT INTO threads(id,session,parent_run,name,profile) VALUES(?1,?2,?3,'child',?4)",
            params![child, root.session, run, encode(&profile).unwrap()],
        )
        .unwrap();
    actor
        .accept_command(&Command::Submit {
            id: CommandId::new(),
            thread: child,
            text: "Child ordinary input is not the title".into(),
        })
        .unwrap();
    crate::store::submit(
        &actor.store.db,
        &CommandId::new(),
        &root.thread,
        "coordination",
        None,
        "Internal coordination must not trigger titles",
    )
    .unwrap();
    actor.schedule_title().unwrap();
    assert_eq!(state(&actor, &root), "eligible");
    assert!(actor.title_worker.is_none());
    submit(&mut actor, &root, "The first ordinary root request");
    assert_eq!(state(&actor, &root), "queued");
    assert_eq!(
        crate::sessions::session(&actor.store.db, &root.session)
            .unwrap()
            .title,
        "The first ordinary root request"
    );
}

#[tokio::test]
async fn title_concurrency_is_one_and_waits_for_normal_worker_capacity() {
    let (_dir, mut actor, root, _) = setup(0);
    submit(&mut actor, &root, "One title request only");
    for _ in 0..16 {
        let (cancel, _) = watch::channel(false);
        actor.workers.insert(
            OperationId::new(),
            Worker {
                cancel,
                task: tokio::spawn(std::future::pending()),
                cancelled_at: None,
            },
        );
    }
    actor.schedule_title().unwrap();
    assert!(actor.title_worker.is_none());
    assert_eq!(state(&actor, &root), "queued");
    for (_, worker) in actor.workers.drain() {
        worker.task.abort();
    }
    actor.schedule_title().unwrap();
    actor.schedule_title().unwrap();
    assert!(actor.title_worker.is_some());
    assert_eq!(
        crate::sessions::usage(&actor.store.db, Some(&root.session))
            .unwrap()
            .requests,
        1
    );
}

#[test]
fn failed_v8_upgrade_rolls_back_schema_and_version() {
    let (dir, store, _, _) = fixture();
    store.db.execute_batch("DROP TABLE session_titles; CREATE INDEX pending_titles ON sessions(workspace); PRAGMA user_version=8; PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    drop(store);
    let path = dir.0.join("lab.db");
    assert!(Store::open(&path).is_err());
    let db =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        8
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='session_titles'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert!(SessionCatalog::open(&path).is_ok());
}

#[test]
fn future_and_foreign_databases_are_rejected_without_changes() {
    for (application, version) in [
        (APPLICATION, crate::store::SCHEMA + 1),
        (APPLICATION, 7),
        (12345, crate::store::SCHEMA),
    ] {
        let (dir, store, _, _) = fixture();
        store
            .db
            .pragma_update(None, "application_id", application)
            .unwrap();
        store
            .db
            .pragma_update(None, "user_version", version)
            .unwrap();
        store
            .db
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(store);
        let path = dir.0.join("lab.db");
        let bytes = std::fs::read(&path).unwrap();
        assert!(SessionCatalog::open(&path).is_err());
        assert!(Store::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
