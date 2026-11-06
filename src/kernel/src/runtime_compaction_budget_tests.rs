use super::*;

#[path = "runtime_compaction_publication_tests.rs"]
mod publication;

#[tokio::test]
async fn normalized_measurement_triggers_compaction_and_raw_usage_is_unchanged() {
    let (_dir, store, receipt, _) = fixture();
    let (requests, mut dispatched) = mpsc::unbounded_channel();
    let mut actor = Actor::new_owned(store, Arc::new(BarrierExecutor { requests })).unwrap();
    let run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, receipt.thread],
        )
        .unwrap();
    let message = Message::user_text("small initial request");
    entry(
        &actor.store.db,
        &receipt.thread,
        "submission",
        "small",
        EntryData {
            message: Some(&message),
            ..Default::default()
        },
    )
    .unwrap();
    let mut spec = actor.model_spec(&receipt.thread).unwrap();
    spec.messages = vec![message];
    assert!(estimate_input_tokens(&spec).unwrap() < 10_000);
    let (done, mut completed) = mpsc::channel(16);
    actor
        .start_model(&run, &receipt.thread, spec, "model", None, &done)
        .unwrap();
    let (_, response) = dispatch(&mut dispatched).await;
    let mut completion = answer("visible ordinary response");
    completion.usage = Usage {
        input_tokens: 12_000,
        cache_read_input_tokens: 6000,
        cache_creation_input_tokens: 2000,
        output_tokens: 10,
    };
    response.send(completion).unwrap();
    finish(&mut actor, &mut completed).await;
    assert_eq!(
        actor
            .context_snapshot(&receipt.thread)
            .unwrap()
            .budget
            .measured_input_tokens,
        Some(20_000)
    );
    let usage = crate::sessions::usage(&actor.store.db, Some(&receipt.session)).unwrap();
    assert_eq!(usage.usage.input_tokens, 12_000);
    assert_eq!(usage.usage.cache_read_input_tokens, 6000);
    assert_eq!(usage.usage.cache_creation_input_tokens, 2000);
    let run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, receipt.thread],
        )
        .unwrap();
    actor.request(&run, &receipt.thread, &done).unwrap();
    finish(&mut actor, &mut completed).await;
    actor.request(&run, &receipt.thread, &done).unwrap();
    let (summary, response) = dispatch(&mut dispatched).await;
    assert_eq!(summary.system, SUMMARY_SYSTEM);
    let mut completion = answer("small summary");
    completion.usage.input_tokens = 900_000;
    response.send(completion).unwrap();
    finish(&mut actor, &mut completed).await;
    assert_eq!(
        actor
            .context_snapshot(&receipt.thread)
            .unwrap()
            .budget
            .measured_input_tokens,
        Some(20_000)
    );
    actor.request(&run, &receipt.thread, &done).unwrap();
    assert!(actor
        .context_snapshot(&receipt.thread)
        .unwrap()
        .checkpoint
        .is_some());
    assert_eq!(
        actor
            .context_snapshot(&receipt.thread)
            .unwrap()
            .budget
            .measured_input_tokens,
        None
    );
}

#[tokio::test]
async fn publication_failure_rolls_back_working_context_and_can_resume_validation() {
    let (_dir, store, receipt, _) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    let (run, mut pending) = seeded(&mut actor, &receipt.thread, true);
    pending.summary = "valid summary".into();
    pending.attempts = MAX_ATTEMPTS;
    pending.save(&actor.store.db, &receipt.thread).unwrap();
    let original = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    let before = actor.context_snapshot(&receipt.thread).unwrap();
    actor.store.db.execute_batch("CREATE TEMP TRIGGER reject_checkpoint BEFORE INSERT ON checkpoints BEGIN SELECT RAISE(ABORT,'injected publication failure'); END;").unwrap();
    let (done, _completed) = mpsc::channel(16);
    assert!(actor
        .request(&run, &receipt.thread, &done)
        .unwrap_err()
        .to_string()
        .contains("injected publication failure"));
    assert_eq!(
        crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap(),
        original
    );
    let after = actor.context_snapshot(&receipt.thread).unwrap();
    assert_eq!(
        (after.revision, after.checkpoint),
        (before.revision, before.checkpoint)
    );
    assert!(state(&actor, &receipt.thread).phase == Phase::Validate);
    actor
        .store
        .db
        .execute_batch("DROP TRIGGER reject_checkpoint;")
        .unwrap();
    actor.request(&run, &receipt.thread, &done).unwrap();
    assert!(actor
        .context_snapshot(&receipt.thread)
        .unwrap()
        .checkpoint
        .is_some());
    assert!(actor.workers.is_empty());
}

#[tokio::test]
async fn output_cap_retries_shrink_fragments_and_exhaust_without_checkpoint() {
    let (_dir, store, receipt, models) = fixture();
    let (requests, mut dispatched) = mpsc::unbounded_channel();
    let mut actor = Actor::new_owned(store, Arc::new(BarrierExecutor { requests })).unwrap();
    let (run, mut pending) = seeded(&mut actor, &receipt.thread, false);
    pending.summary.clear();
    pending.output_tokens = Some(10_000);
    pending.save(&actor.store.db, &receipt.thread).unwrap();
    let (done, mut completed) = mpsc::channel(16);
    let mut last_input = usize::MAX;
    for _ in 0..5 {
        actor.request(&run, &receipt.thread, &done).unwrap();
        let (spec, response) = dispatch(&mut dispatched).await;
        assert_eq!(spec.max_tokens, 10_000);
        let input = estimate_input_tokens(&spec).unwrap();
        assert!(input < last_input);
        last_input = input;
        let mut truncated = answer("never accept this partial summary");
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
    }
    assert!(state(&actor, &receipt.thread).phase == Phase::Blocked);
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
    assert!(actor
        .context_snapshot(&receipt.thread)
        .unwrap()
        .checkpoint
        .is_none());
}

#[tokio::test]
async fn unchanged_reduction_blocks_and_changed_profile_allows_recovery() {
    let (_dir, store, receipt, models) = fixture();
    let (requests, mut dispatched) = mpsc::unbounded_channel();
    let mut actor = Actor::new_owned(store, Arc::new(BarrierExecutor { requests })).unwrap();
    let (run, mut pending) = seeded(&mut actor, &receipt.thread, true);
    pending.summary = "large summary ".repeat(9000);
    pending.reduce();
    pending.chunk_limit = 100_000;
    pending.save(&actor.store.db, &receipt.thread).unwrap();
    let (done, mut completed) = mpsc::channel(16);
    loop {
        actor.request(&run, &receipt.thread, &done).unwrap();
        let (_, response) = dispatch(&mut dispatched).await;
        let current = state(&actor, &receipt.thread);
        let reply = if current.reduction_offset + current.attempt.fragment_bytes
            >= current.reduction_source.len()
        {
            current.reduction_source.clone()
        } else {
            "interim summary".into()
        };
        response.send(answer(&reply)).unwrap();
        finish(&mut actor, &mut completed).await;
        if state(&actor, &receipt.thread).phase == Phase::Blocked {
            break;
        }
    }
    actor.request(&run, &receipt.thread, &done).unwrap();
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
    let mut profile = actor.context_snapshot(&receipt.thread).unwrap().profile;
    profile.instructions.push_str(" changed profile");
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
    actor.request(&run, &receipt.thread, &done).unwrap();
    finish(&mut actor, &mut completed).await;
    let remaining: i64 = actor
        .store
        .db
        .query_row(
            "SELECT COUNT(*) FROM compactions WHERE thread=?1",
            [&receipt.thread],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0);
    assert!(actor
        .context_snapshot(&receipt.thread)
        .unwrap()
        .checkpoint
        .is_none());
}

#[tokio::test]
async fn rolling_reduction_preserves_source_cursor_and_finishes_checkpoint() {
    let (_dir, store, receipt, _) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    let (run, mut pending) = seeded(&mut actor, &receipt.thread, false);
    pending.summary = "rolling summary ".repeat(9000);
    pending.save(&actor.store.db, &receipt.thread).unwrap();
    let original = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    let mut reduced = false;
    for _ in 0..24 {
        advance(&mut actor, &run, &receipt.thread).await;
        if actor
            .context_snapshot(&receipt.thread)
            .unwrap()
            .checkpoint
            .is_some()
        {
            break;
        }
        let pending = state(&actor, &receipt.thread);
        assert_eq!(
            crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap(),
            original
        );
        if pending.phase == Phase::Reduce {
            assert_eq!((pending.cursor, pending.offset), (0, 0));
        }
        if pending.reductions > 0 && pending.phase == Phase::Summarize {
            reduced = true;
        }
    }
    assert!(reduced);
    assert!(actor
        .context_snapshot(&receipt.thread)
        .unwrap()
        .checkpoint
        .is_some());
}

#[tokio::test]
async fn legacy_serialized_cursor_restarts_safe_projection_without_changing_history() {
    let (_dir, store, receipt, _) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    let (run, mut pending) = seeded(&mut actor, &receipt.thread, false);
    pending.projection_version = 0;
    pending.cursor = 1;
    pending.offset = 3;
    pending.summary = "old serialization summary".into();
    pending.save(&actor.store.db, &receipt.thread).unwrap();
    let original = history(&actor, &receipt.thread);
    advance(&mut actor, &run, &receipt.thread).await;
    let migrated = state(&actor, &receipt.thread);
    assert_eq!(migrated.projection_version, 1);
    assert_eq!(migrated.attempts, 1);
    let (_, request) = pending_request(&actor, &receipt.thread);
    assert!(request.messages[0]
        .text()
        .contains("User:\nvisible 🦀 fragment"));
    assert!(!request.messages[0]
        .text()
        .contains("old serialization summary"));
    assert_eq!(history(&actor, &receipt.thread), original);
}

#[tokio::test]
async fn fixed_protected_overhead_blocks_without_dispatch_and_uncertain_effects_prevent_retry() {
    let (_dir, store, receipt, models) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    let (run, mut pending) = seeded(&mut actor, &receipt.thread, true);
    pending.newest_request = "exact protected task ".repeat(8000);
    pending.save(&actor.store.db, &receipt.thread).unwrap();
    let (done, _completed) = mpsc::channel(16);
    actor.request(&run, &receipt.thread, &done).unwrap();
    assert!(state(&actor, &receipt.thread).phase == Phase::Blocked);
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
    actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,'write_file','tool','{}','unknown')",params![OperationId::new(),run,receipt.thread]).unwrap();
    let revision = actor.context_snapshot(&receipt.thread).unwrap().revision;
    assert!(actor
        .store
        .command(
            &Command::Compact {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                expected_revision: revision
            },
            &models
        )
        .unwrap_err()
        .to_string()
        .contains("uncertain"));
    assert!(state(&actor, &receipt.thread).phase == Phase::Blocked);
}
