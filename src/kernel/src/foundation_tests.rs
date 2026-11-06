use crate::context::*;
use crate::contracts::*;
use crate::model::*;
use crate::store::tests::fixture;
use crate::store::*;
use bob_types::{ContentBlock, Message, Role, Usage};
use rusqlite::params;
use serde_json::json;

fn run(store: &Store, thread: &ThreadId) -> RunId {
    let run = RunId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, thread],
        )
        .unwrap();
    run
}
fn human(store: &Store, thread: &ThreadId, run: &RunId, text: &str) {
    let id = CommandId::new();
    submit(&store.db, &id, thread, "human", None, text).unwrap();
    consume(&store.db, &id, run).unwrap();
}
fn context(store: &Store, thread: &ThreadId, models: &[ModelInfo]) -> ContextSnapshot {
    snapshot(&store.db, thread, &models[0], &ModelCapabilities::default()).unwrap()
}
fn operation(store: &Store, thread: &ThreadId, run: &RunId, name: &str) -> OperationId {
    let id = OperationId::new();
    store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,?4,'tool','call','{}','waiting')",params![id,run,thread,name]).unwrap();
    id
}

#[test]
fn checkpoint_is_atomic_independent_of_transcript_and_survives_restart() {
    let (dir, mut store, receipt, models) = fixture();
    let run = run(&store, &receipt.thread);
    human(
        &store,
        &receipt.thread,
        &run,
        "preserve the latest instruction verbatim",
    );
    for _ in 0..300 {
        entry(
            &store.db,
            &receipt.thread,
            "assistant",
            "old answer",
            EntryData {
                message: Some(&Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "old answer".into(),
                    }],
                }),
                ..Default::default()
            },
        )
        .unwrap();
    }
    let before = context(&store, &receipt.thread, &models);
    assert_eq!(before.message_count, 301);
    let transcript: i64 = store
        .db
        .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get(0))
        .unwrap();
    store.db.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE INSERT ON checkpoints BEGIN SELECT RAISE(ABORT,'injected checkpoint failure'); END;").unwrap();
    {
        let tx = store.db.transaction().unwrap();
        assert!(checkpoint(
            &tx,
            &receipt.thread,
            &OperationId::new(),
            before.epoch,
            before.revision,
            &[Message::user_text("summary")]
        )
        .is_err());
    }
    assert_eq!(
        context(&store, &receipt.thread, &models).revision,
        before.revision
    );
    assert_eq!(context(&store, &receipt.thread, &models).message_count, 301);
    store
        .db
        .execute_batch("DROP TRIGGER fail_checkpoint")
        .unwrap();
    let checkpoint_id = OperationId::new();
    {
        let tx = store.db.transaction().unwrap();
        checkpoint(
            &tx,
            &receipt.thread,
            &checkpoint_id,
            before.epoch,
            before.revision,
            &[Message::user_text(
                "summary\npreserve the latest instruction verbatim",
            )],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        store
            .db
            .query_row("SELECT COUNT(*) FROM entries", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        transcript
    );
    drop(store);
    let store = Store::open(&dir.0.join("lab.db")).unwrap();
    let after = context(&store, &receipt.thread, &models);
    assert_eq!(after.checkpoint, Some(checkpoint_id));
    assert_eq!(after.message_count, 1);
    assert!(load_message(
        &store.db,
        &working_ids(&store.db, &receipt.thread).unwrap()[0]
    )
    .unwrap()
    .text()
    .contains("verbatim"));
}

#[test]
fn clear_fences_old_outputs_and_old_settings_without_deleting_history() {
    let (_dir, mut store, receipt, models) = fixture();
    let run = run(&store, &receipt.thread);
    human(&store, &receipt.thread, &run, "old epoch");
    let op = operation(&store, &receipt.thread, &run, "edit");
    let before = context(&store, &receipt.thread, &models);
    store
        .command(
            &Command::ClearContext {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                expected_epoch: before.epoch,
            },
            &models,
        )
        .unwrap();
    finish_tool_entry(&store.db, &op, "late successful old effect", false).unwrap();
    assert_eq!(context(&store, &receipt.thread, &models).message_count, 0);
    assert!(store
        .command(
            &Command::ClearContext {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                expected_epoch: before.epoch
            },
            &models
        )
        .is_err());
    assert!(store
        .command(
            &Command::Compact {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                expected_revision: before.revision
            },
            &models
        )
        .is_err());
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE thread=?1",
                [&receipt.thread],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
    let mut profile = before.profile;
    profile.max_requests = 2000;
    store
        .command(
            &Command::UpdateProfile {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                profile: profile.clone(),
                expected_revision: 0,
            },
            &models,
        )
        .unwrap();
    assert!(store
        .command(
            &Command::UpdateProfile {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                profile,
                expected_revision: 0
            },
            &models
        )
        .is_err());
}

#[test]
fn huge_history_context_query_has_no_message_ceiling() {
    let (_dir, mut store, receipt, models) = fixture();
    let tx = store.db.transaction().unwrap();
    let model = artifact(
        &tx,
        &encode(&ModelRecord::Message(Message::user_text("retained"))).unwrap(),
    )
    .unwrap();
    tx.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO entries(thread,kind,text,model,model_order) SELECT ?1,'submission','retained',?2,x FROM n",params![receipt.thread,model]).unwrap();
    tx.commit().unwrap();
    let snapshot = context(&store, &receipt.thread, &models);
    assert_eq!(snapshot.message_count, 10000);
    assert!(snapshot.budget.estimated_input_tokens > 0);
    let tx = store.db.transaction().unwrap();
    checkpoint(
        &tx,
        &receipt.thread,
        &OperationId::new(),
        snapshot.epoch,
        snapshot.revision,
        &[Message::user_text("summary")],
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(context(&store, &receipt.thread, &models).message_count, 1);
}

#[test]
fn budgeting_caps_input_and_output_and_preserves_tool_exchange_boundaries() {
    let model = ModelInfo {
        id: "small".into(),
        context_window: 4096,
    };
    let caps = ModelCapabilities {
        max_input_tokens: Some(2000),
        max_output_tokens: 256,
        preferred_output_tokens: 16000,
    };
    let budget = request_budget(&model, &caps);
    assert_eq!(budget.output_reserve, 256);
    assert_eq!(budget.input_limit, 2000);
    let use_tool = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "a".into(),
            name: "read".into(),
            input: json!({}),
        }],
    };
    let result = Message {
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "a".into(),
            content: "result".into(),
            is_error: Some(false),
        }],
    };
    assert_eq!(
        safe_boundaries(&[Message::user_text("request"), use_tool, result.clone()]).unwrap(),
        vec![true, true, false, true]
    );
    assert!(safe_boundaries(&[result]).is_err());
}

#[test]
fn questions_are_not_permission_decisions_and_yolo_never_answers_them() {
    let (_dir, mut store, receipt, models) = fixture();
    let run = run(&store, &receipt.thread);
    store
        .command(
            &Command::SetPolicy {
                id: CommandId::new(),
                session: receipt.session.clone(),
                policy: Policy {
                    yolo: true,
                    ..Default::default()
                },
                expected_revision: 0,
            },
            &models,
        )
        .unwrap();
    let op = operation(&store, &receipt.thread, &run, "ask_user");
    let tx = store.db.transaction().unwrap();
    assert!(crate::controls::execute(
        &tx,
        &op,
        &receipt.thread,
        "ask_user",
        &json!({"question":"Which?","options":["A","B"]})
    )
    .unwrap()
    .is_none());
    tx.commit().unwrap();
    let question = crate::controls::snapshot(&store.db, &receipt.thread)
        .unwrap()
        .questions
        .remove(0);
    assert!(store
        .command(
            &Command::Decide {
                id: CommandId::new(),
                operation: op.clone(),
                revision: question.revision,
                approve: true
            },
            &models
        )
        .is_err());
    store
        .command(
            &Command::SetPermissions {
                id: CommandId::new(),
                session: receipt.session.clone(),
                permissions: PermissionPolicy::default(),
                expected_revision: question.revision,
            },
            &models,
        )
        .unwrap();
    assert!(store
        .command(
            &Command::AnswerQuestion {
                id: CommandId::new(),
                operation: op.clone(),
                revision: question.revision,
                answer: "A".into()
            },
            &models
        )
        .is_err());
    let refreshed = crate::controls::snapshot(&store.db, &receipt.thread)
        .unwrap()
        .questions
        .remove(0);
    store
        .command(
            &Command::AnswerQuestion {
                id: CommandId::new(),
                operation: op,
                revision: refreshed.revision,
                answer: "Other actual answer".into(),
            },
            &models,
        )
        .unwrap();
    assert!(crate::controls::snapshot(&store.db, &receipt.thread)
        .unwrap()
        .questions
        .is_empty());
}

#[test]
fn plan_refinement_remains_plan_until_explicit_approval() {
    let (_dir, mut store, receipt, models) = fixture();
    let run = run(&store, &receipt.thread);
    let op = operation(&store, &receipt.thread, &run, "exit_plan");
    let tx = store.db.transaction().unwrap();
    crate::controls::execute(
        &tx,
        &op,
        &receipt.thread,
        "exit_plan",
        &json!({"plan":"change only the requested file"}),
    )
    .unwrap();
    tx.commit().unwrap();
    let proposal = crate::controls::snapshot(&store.db, &receipt.thread)
        .unwrap()
        .plans
        .remove(0);
    assert!(
        crate::sessions::session(&store.db, &receipt.session)
            .unwrap()
            .policy
            .plan
    );
    store
        .command(
            &Command::DecidePlan {
                id: CommandId::new(),
                operation: op,
                revision: proposal.revision,
                approve: false,
                feedback: Some("add verification".into()),
            },
            &models,
        )
        .unwrap();
    assert!(
        crate::sessions::session(&store.db, &receipt.session)
            .unwrap()
            .policy
            .plan
    );
    let next = operation(&store, &receipt.thread, &run, "exit_plan");
    let tx = store.db.transaction().unwrap();
    crate::controls::execute(
        &tx,
        &next,
        &receipt.thread,
        "exit_plan",
        &json!({"plan":"change file and verify behavior"}),
    )
    .unwrap();
    tx.commit().unwrap();
    let proposal = crate::controls::snapshot(&store.db, &receipt.thread)
        .unwrap()
        .plans
        .remove(0);
    store
        .command(
            &Command::DecidePlan {
                id: CommandId::new(),
                operation: next,
                revision: proposal.revision,
                approve: true,
                feedback: None,
            },
            &models,
        )
        .unwrap();
    assert!(
        !crate::sessions::session(&store.db, &receipt.session)
            .unwrap()
            .policy
            .plan
    );
}

#[test]
fn offline_catalog_and_usage_do_not_need_an_executor_or_execution_lock() {
    let (dir, store, receipt, _) = fixture();
    let run = run(&store, &receipt.thread);
    human(&store, &receipt.thread, &run, "inspect useful session");
    let operation = OperationId::new();
    store.db.execute("INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,'model','model','{}','done')",params![operation,run,receipt.thread]).unwrap();
    crate::sessions::record_usage(
        &store.db,
        &operation,
        Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 5,
        },
    )
    .unwrap();
    let catalog = crate::SessionCatalog::open(&dir.0.join("lab.db")).unwrap();
    let page = catalog
        .sessions(SessionQuery {
            search: Some("useful".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.sessions.len(), 1);
    assert_eq!(page.sessions[0].title, "inspect useful session");
    assert_eq!(
        catalog.session(receipt.session.clone()).unwrap().thread,
        receipt.thread
    );
    drop(catalog);
    drop(store);
    let store = Store::open(&dir.0.join("lab.db")).unwrap();
    let usage = crate::sessions::usage(&store.db, Some(&receipt.session)).unwrap();
    assert_eq!(usage.requests, 1);
    assert_eq!(usage.usage.total_input(), 18);
}

#[test]
fn incompatible_schema_is_rejected_without_modifying_database() {
    let (dir, store, _, _) = fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=3; PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    assert!(Store::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
