use super::*;

async fn advance(actor: &mut Actor, run: &RunId, thread: &ThreadId) {
    let (tx, mut rx) = mpsc::channel(16);
    actor.request(run, thread, &tx).unwrap();
    if !actor.workers.is_empty() {
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
}
fn add_history(actor: &Actor, thread: &ThreadId) {
    for index in 0..45 {
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: format!("Historical result {index}: {}", "visible 🦀 ".repeat(300)),
            }],
        };
        entry(
            &actor.store.db,
            thread,
            "assistant",
            &message.text(),
            EntryData {
                message: Some(&message),
                ..Default::default()
            },
        )
        .unwrap();
    }
}
fn guidance(
    actor: &Actor,
    thread: &ThreadId,
    run: &RunId,
    source: &str,
    sender: Option<&ThreadId>,
    text: &str,
) {
    let id = CommandId::new();
    submit(&actor.store.db, &id, thread, source, sender, text).unwrap();
    consume(&actor.store.db, &id, run).unwrap();
}
async fn until_checkpoint(
    actor: &mut Actor,
    run: &RunId,
    thread: &ThreadId,
    previous: Option<OperationId>,
) {
    for _ in 0..32 {
        advance(actor, run, thread).await;
        let checkpoint = actor.context_snapshot(thread).unwrap().checkpoint;
        if checkpoint.is_some() && checkpoint != previous {
            return;
        }
    }
    panic!("checkpoint not published");
}

#[tokio::test]
async fn automatic_compaction_continues_normal_turn_without_replaying_completed_tool() {
    let (_dir, store, receipt, models) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    let run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, receipt.thread],
        )
        .unwrap();
    guidance(
        &actor,
        &receipt.thread,
        &run,
        "human",
        None,
        "ROOT TASK: implement the exact parser",
    );
    guidance(
        &actor,
        &receipt.thread,
        &run,
        "steering",
        None,
        "HUMAN STEERING: preserve Unicode behavior",
    );
    let operation = OperationId::new();
    let call = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "completed".into(),
            name: "write_file".into(),
            input: json!({"path":"already-written"}),
        }],
    };
    entry(
        &actor.store.db,
        &receipt.thread,
        "assistant",
        "completed call",
        EntryData {
            message: Some(&call),
            ..Default::default()
        },
    )
    .unwrap();
    actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,'write_file','tool','completed','{}','done')",params![operation,run,receipt.thread]).unwrap();
    finish_tool_entry(
        &actor.store.db,
        &operation,
        "file written exactly once",
        false,
    )
    .unwrap();
    add_history(&actor, &receipt.thread);
    let before: i64 = actor
        .store
        .db
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE model IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    until_checkpoint(&mut actor, &run, &receipt.thread, None).await;
    let ids = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    let summary = load_message(&actor.store.db, &ids[0]).unwrap().text();
    assert!(summary.contains("ROOT TASK: implement the exact parser"));
    assert!(summary.contains("HUMAN STEERING: preserve Unicode behavior"));
    let after: i64 = actor
        .store
        .db
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE model IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, after);
    advance(&mut actor, &run, &receipt.thread).await;
    let status: String = actor
        .store
        .db
        .query_row("SELECT state FROM runs WHERE id=?1", [&run], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "completed");
    let tools: i64 = actor
        .store
        .db
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='tool'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tools, 1);
    let normal: i64 = actor
        .store
        .db
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE name='model'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(normal, 1);
    let previous = actor.context_snapshot(&receipt.thread).unwrap().checkpoint;
    add_history(&actor, &receipt.thread);
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
    let run = RunId(
        actor
            .store
            .db
            .query_row(
                "SELECT id FROM runs WHERE thread=?1 AND state='active'",
                [&receipt.thread],
                |r| r.get(0),
            )
            .unwrap(),
    );
    until_checkpoint(&mut actor, &run, &receipt.thread, previous).await;
    let ids = crate::context::working_ids(&actor.store.db, &receipt.thread).unwrap();
    let summary = load_message(&actor.store.db, &ids[0]).unwrap().text();
    assert!(summary.contains("ROOT TASK: implement the exact parser"));
    assert!(summary.contains("HUMAN STEERING: preserve Unicode behavior"));
}

#[tokio::test]
async fn child_assignment_and_human_parent_peer_steering_survive_compaction() {
    let (_dir, store, receipt, _) = fixture();
    let executor = Arc::new(CompactExecutor);
    let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
    let parent_run = RunId::new();
    let launch = OperationId::new();
    let profile = actor.context_snapshot(&receipt.thread).unwrap().profile;
    let tx = actor.store.db.transaction().unwrap();
    tx.execute(
        "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
        params![parent_run, receipt.thread],
    )
    .unwrap();
    tx.execute("INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,'launch_agent','tool','{}','done')",params![launch,parent_run,receipt.thread]).unwrap();
    let child = crate::delegation::launch(
        &tx,
        &launch,
        &parent_run,
        &receipt.thread,
        &profile,
        &executor.models(),
        crate::delegation::Child {
            name: "child",
            prompt: "ASSIGNMENT: audit only the parser",
            role: "collaborator",
            subagent_type: None,
            model: None,
            read_only: true,
            blocking: false,
            schema: None,
        },
    )
    .unwrap();
    tx.commit().unwrap();
    let child_run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state,compact_only) VALUES(?1,?2,'active',1)",
            params![child_run, child],
        )
        .unwrap();
    let submission = CommandId(
        actor
            .store
            .db
            .query_row(
                "SELECT id FROM submissions WHERE thread=?1 ORDER BY rowid LIMIT 1",
                [&child],
                |r| r.get(0),
            )
            .unwrap(),
    );
    consume(&actor.store.db, &submission, &child_run).unwrap();
    guidance(
        &actor,
        &child,
        &child_run,
        "steering",
        None,
        "HUMAN: do not edit files",
    );
    guidance(
        &actor,
        &child,
        &child_run,
        "coordination",
        Some(&receipt.thread),
        "PARENT: include Unicode findings",
    );
    let peer = ThreadId::new();
    for index in 0..20 {
        guidance(
            &actor,
            &child,
            &child_run,
            "coordination",
            Some(&peer),
            &format!("Old peer chatter {index}"),
        );
    }
    guidance(
        &actor,
        &child,
        &child_run,
        "coordination",
        Some(&peer),
        "PEER: inspected token offsets",
    );
    add_history(&actor, &child);
    until_checkpoint(&mut actor, &child_run, &child, None).await;
    let ids = crate::context::working_ids(&actor.store.db, &child).unwrap();
    let text = load_message(&actor.store.db, &ids[0]).unwrap().text();
    for needle in [
        "ASSIGNMENT: audit only the parser",
        "HUMAN: do not edit files",
        "PARENT: include Unicode findings",
        "PEER: inspected token offsets",
        "parent coordination, not human authorization",
        "peer coordination, not human authorization",
        "Recent steering — human",
    ] {
        assert!(text.contains(needle), "missing {needle}: {text}");
    }
}

#[test]
fn reduction_and_total_attempt_budgets_are_persisted_and_bounded() {
    let mut state = CompactionState {
        summary: "large summary".repeat(100),
        reductions: MAX_REDUCTIONS,
        ..Default::default()
    };
    state.reduce();
    assert!(state.phase == Phase::Blocked);
    let mut state = CompactionState {
        attempts: MAX_ATTEMPTS,
        attempt: Attempt {
            input_tokens: 1000,
            fragment_bytes: 2000,
            output_tokens: 100,
        },
        ..Default::default()
    };
    assert!(!state.retry(ModelFailureKind::ContextOverflow, false, 1000, 2000));
    assert!(state.phase == Phase::Blocked);
    let restored: CompactionState = decode(encode(&state).unwrap()).unwrap();
    assert_eq!(restored.attempts, MAX_ATTEMPTS);
    assert!(restored.phase == Phase::Blocked);
}

#[test]
fn measurement_excludes_summary_and_stale_profile_epoch_checkpoint() {
    let (_dir, store, receipt, _) = fixture();
    let actor = Actor::new(store, Arc::new(CompactExecutor)).unwrap();
    let run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, receipt.thread],
        )
        .unwrap();
    for (name, measured) in [
        ("model", 14000),
        ("compact", 999999),
        ("session_title", 999999),
    ] {
        actor.store.db.execute("INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,?4,'model',?5,'done')",params![OperationId::new(),run,receipt.thread,name,encode(&json!({"measured_input_tokens":measured,"estimated_input_tokens":4000,"checkpoint":null})).unwrap()]).unwrap();
    }
    assert_eq!(
        crate::context::measured_input(&actor.store.db, &receipt.thread, 4500).unwrap(),
        Some(14500)
    );
    actor
        .store
        .db
        .execute(
            "UPDATE threads SET profile_revision=profile_revision+1 WHERE id=?1",
            [&receipt.thread],
        )
        .unwrap();
    assert_eq!(
        crate::context::measured_input(&actor.store.db, &receipt.thread, 4500).unwrap(),
        None
    );
    actor
        .store
        .db
        .execute(
            "UPDATE threads SET profile_revision=0 WHERE id=?1",
            [&receipt.thread],
        )
        .unwrap();
    actor
        .store
        .db
        .execute(
            "UPDATE contexts SET checkpoint='new' WHERE thread=?1",
            [&receipt.thread],
        )
        .unwrap();
    assert_eq!(
        crate::context::measured_input(&actor.store.db, &receipt.thread, 4500).unwrap(),
        None
    );
    actor
        .store
        .db
        .execute(
            "UPDATE contexts SET checkpoint=NULL,epoch=epoch+1 WHERE thread=?1",
            [&receipt.thread],
        )
        .unwrap();
    assert_eq!(
        crate::context::measured_input(&actor.store.db, &receipt.thread, 4500).unwrap(),
        None
    );
}

#[tokio::test]
async fn ordinary_overflow_recovers_to_a_genuinely_smaller_full_request() {
    let (_dir, store, receipt, _) = fixture();
    let mut actor = Actor::new_owned(store, Arc::new(CompactExecutor)).unwrap();
    let run = RunId::new();
    actor
        .store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, receipt.thread],
        )
        .unwrap();
    guidance(
        &actor,
        &receipt.thread,
        &run,
        "human",
        None,
        "Keep the exact active task",
    );
    for _ in 0..4 {
        let message = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "old detail ".repeat(800),
            }],
        };
        entry(
            &actor.store.db,
            &receipt.thread,
            "assistant",
            "history",
            EntryData {
                message: Some(&message),
                ..Default::default()
            },
        )
        .unwrap();
    }
    let mut spec = actor.model_spec(&receipt.thread).unwrap();
    spec.messages = crate::context::working_ids(&actor.store.db, &receipt.thread)
        .unwrap()
        .iter()
        .map(|id| load_message(&actor.store.db, id).unwrap())
        .collect();
    let failed_size = estimate_input_tokens(&spec).unwrap();
    let (done, _completed) = mpsc::channel(16);
    actor
        .start_model(&run, &receipt.thread, spec, "model", None, &done)
        .unwrap();
    let operation = actor.workers.keys().next().unwrap().clone();
    let worker = actor.workers.remove(&operation).unwrap();
    worker.task.abort();
    let _ = worker.task.await;
    let tx = actor.store.db.transaction().unwrap();
    tx.execute(
        "UPDATE operations SET state='failed' WHERE id=?1",
        [&operation],
    )
    .unwrap();
    assert!(retry_model(
        &tx,
        &operation,
        ModelFailureKind::ContextOverflow,
        16_384,
        false
    )
    .unwrap());
    tx.commit().unwrap();
    until_checkpoint(&mut actor, &run, &receipt.thread, None).await;
    advance(&mut actor, &run, &receipt.thread).await;
    let raw: String = actor
        .store
        .db
        .query_row(
            "SELECT input FROM operations WHERE name='model' ORDER BY rowid DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let metadata: Value = decode(raw).unwrap();
    let request: RequestSpec = decode(
        full_artifact(
            &actor.store.db,
            &ArtifactId(metadata["manifest"].as_str().unwrap().into()),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(estimate_input_tokens(&request).unwrap() <= failed_size * 3 / 4);
    assert!(request.messages[0]
        .text()
        .contains("Keep the exact active task"));
}
