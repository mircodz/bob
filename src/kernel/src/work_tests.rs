use crate::delegation::{self, Child};
use crate::jobs;
use crate::model::*;
use crate::store::{self, *};
use crate::{JobKind, JobState};
use rusqlite::params;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

#[path = "workflow_handle_tests.rs"]
mod workflow_handle_tests;
#[path = "workflow_regression_tests.rs"]
mod workflow_regression_tests;

fn operation(
    store: &Store,
    root: &Receipt,
    name: &str,
    input: Value,
) -> (RunId, OperationId, PreparedTool) {
    let run = RunId::new();
    let id = OperationId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![run, root.thread],
        )
        .unwrap();
    store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,?4,'tool','call',?5,'ready')",params![id,run,root.thread,name,encode(&input).unwrap()]).unwrap();
    let tool = PreparedTool {
        invocation: Invocation {
            operation_id: id.clone(),
            run_id: run.clone(),
            thread_id: root.thread.clone(),
            workspace: store
                .db
                .query_row(
                    "SELECT workspace FROM sessions WHERE id=?1",
                    [&root.session],
                    |r| r.get(0),
                )
                .unwrap(),
            name: name.into(),
            input,
        },
        specification: json!({"private":"prepared binding"}),
        preview: "preview".into(),
        presentation: None,
    };
    store
        .db
        .execute(
            "UPDATE operations SET prepared=?2 WHERE id=?1",
            params![id, encode(&tool).unwrap()],
        )
        .unwrap();
    (run, id, tool)
}
fn profile(store: &Store, root: &Receipt) -> Profile {
    decode(
        store
            .db
            .query_row(
                "SELECT profile FROM threads WHERE id=?1",
                [&root.thread],
                |r| r.get(0),
            )
            .unwrap(),
    )
    .unwrap()
}
fn children(store: &Store, op: &OperationId) -> Vec<ThreadId> {
    let mut stmt = store
        .db
        .prepare("SELECT thread FROM agents WHERE operation=?1 ORDER BY rowid")
        .unwrap();
    let rows = stmt.query_map([op], |r| Ok(ThreadId(r.get(0)?))).unwrap();
    rows.collect::<Result<_, _>>().unwrap()
}
fn result(text: &str) -> ToolOutcome {
    ToolOutcome {
        text: text.into(),
        is_error: false,
        interrupted: false,
        presentation: None,
    }
}
fn workflow_tick(
    store: &mut Store,
    root: &Receipt,
    run: &RunId,
    op: &OperationId,
    models: &[ModelInfo],
    input: &Value,
) -> Option<Value> {
    let profile = profile(store, root);
    let tx = store.db.transaction().unwrap();
    let output =
        crate::workflow::execute(&tx, op, run, &root.thread, &profile, models, input).unwrap();
    tx.commit().unwrap();
    output.map(|text| serde_json::from_str(&text).unwrap())
}
fn child_done(store: &mut Store, child: &ThreadId, state: &str, text: &str) {
    let tx = store.db.transaction().unwrap();
    delegation::settle(&tx, child, state, text).unwrap();
    tx.execute(
        "UPDATE submissions SET state='settled' WHERE thread=?1",
        [child],
    )
    .unwrap();
    tx.commit().unwrap();
}

#[tokio::test]
async fn stopping_shell_retains_queued_progress_but_unknown_shell_does_not() {
    let (_dir, mut store, root, _) = store::tests::fixture();
    let (_, op, tool) = operation(&store, &root, "bash", json!({"run_in_background": true}));
    let tx = store.db.transaction().unwrap();
    let job = jobs::start_shell(&tx, &tool).unwrap();
    jobs::stop(&tx, &job).unwrap();
    tx.commit().unwrap();
    let (reply, ack) = oneshot::channel();
    jobs::append(
        &mut store.db,
        jobs::ProgressUpdate {
            operation: op.clone(),
            text: "queued before stop λ".into(),
            reply,
        },
    )
    .unwrap();
    ack.await.unwrap().unwrap();
    assert_eq!(
        jobs::page(&store.db, &job, 0, 24000).unwrap().text,
        "queued before stop λ"
    );
    store
        .db
        .execute("UPDATE jobs SET state='unknown' WHERE id=?1", [&job])
        .unwrap();
    let (reply, ack) = oneshot::channel();
    jobs::append(
        &mut store.db,
        jobs::ProgressUpdate {
            operation: op,
            text: "late unknown".into(),
            reply,
        },
    )
    .unwrap();
    ack.await.unwrap().unwrap();
    assert_eq!(
        jobs::page(&store.db, &job, 0, 24000).unwrap().text,
        "queued before stop λ"
    );
}

#[tokio::test]
async fn background_shell_publication_progress_final_and_late_fences() {
    let (_dir, mut store, root, _models) = store::tests::fixture();
    let (_run, op, tool) = operation(
        &store,
        &root,
        "bash",
        json!({"command":"printf λ","run_in_background":true}),
    );
    let frozen = encode(&tool).unwrap();
    let tx = store.db.transaction().unwrap();
    let resources = crate::effect_claims::resources(&tool, &[]).unwrap();
    assert!(
        crate::effect_claims::claim(&tx, &op, &root.session, "fixture", &resources, true).unwrap()
    );
    let job = jobs::start_shell(&tx, &tool).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT COUNT(*) FROM effect_claims WHERE operation=?1",
                [&op],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        1
    );
    let listed = jobs::list(&store.db, &root.session).unwrap();
    assert_eq!(listed[0].kind, JobKind::Shell);
    assert_eq!(listed[0].state, JobState::Running);
    assert_eq!(
        jobs::tool_input(&store.db, &op).unwrap(),
        tool.invocation.input
    );
    assert_eq!(
        store
            .db
            .query_row("SELECT prepared FROM operations WHERE id=?1", [&op], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
        frozen
    );
    let (tx, mut rx) = mpsc::channel(2);
    let progress = crate::ToolProgress {
        operation: op.clone(),
        tx,
    };
    let append = progress.append("first λ\n".into());
    let drain = async {
        jobs::append(&mut store.db, rx.recv().await.unwrap()).unwrap();
    };
    let (sent, ()) = tokio::join!(append, drain);
    sent.unwrap();
    assert_eq!(
        jobs::page(&store.db, &job, 0, 24000).unwrap().text,
        "first λ\n"
    );
    assert!(progress.append("x".repeat(16385)).await.is_err());
    let tx = store.db.transaction().unwrap();
    jobs::finish_shell(&tx, &op, Ok((result("authoritative\n"), None))).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT COUNT(*) FROM effect_claims WHERE operation=?1",
                [&op],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(
        jobs::page(&store.db, &job, 0, 24000).unwrap().text,
        "authoritative\n"
    );
    let (reply, ack) = oneshot::channel();
    jobs::append(
        &mut store.db,
        jobs::ProgressUpdate {
            operation: op.clone(),
            text: "late".into(),
            reply,
        },
    )
    .unwrap();
    ack.await.unwrap().unwrap();
    assert_eq!(
        jobs::page(&store.db, &job, 0, 24000).unwrap().text,
        "authoritative\n"
    );
    let tx = store.db.transaction().unwrap();
    jobs::finish_shell(&tx, &op, Ok((result("duplicate"), None))).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE operation=?1 AND kind='tool_result'",
                [&op],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT COUNT(*) FROM submissions WHERE text LIKE 'Shell job %'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn turn_tracking_keeps_queue_and_does_not_promote_shell() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let (run, _op, _tool) = operation(&store, &root, "bash", json!({"command":"work"}));
    let submission = CommandId::new();
    store
        .command(
            &Command::Submit {
                id: submission.clone(),
                thread: root.thread.clone(),
                text: "next input".into(),
            },
            &models,
        )
        .unwrap();
    let cmd = Command::Background {
        id: CommandId::new(),
        thread: root.thread.clone(),
    };
    store.command(&cmd, &models).unwrap();
    store.command(&cmd, &models).unwrap();
    let list = jobs::list(&store.db, &root.session).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].kind, JobKind::Turn);
    assert_eq!(list[0].run, Some(run));
    assert_eq!(
        jobs::queue(&store.db, &root.thread).unwrap()[0].id,
        submission
    );
    store
        .command(
            &Command::RecallSubmission {
                id: CommandId::new(),
                submission,
            },
            &models,
        )
        .unwrap();
    assert!(jobs::queue(&store.db, &root.thread).unwrap().is_empty());
    assert_eq!(
        store
            .db
            .query_row("SELECT state FROM runs", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "active"
    );
}

#[test]
fn stopping_root_preserves_detached_shell_but_clear_cancels_it() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let (run, _, tool) = operation(
        &store,
        &root,
        "bash",
        json!({"command":"work","run_in_background":true}),
    );
    let tx = store.db.transaction().unwrap();
    jobs::start_shell(&tx, &tool).unwrap();
    tx.commit().unwrap();
    store
        .command(
            &Command::Stop {
                id: CommandId::new(),
                run,
            },
            &models,
        )
        .unwrap();
    assert_eq!(
        jobs::list(&store.db, &root.session).unwrap()[0].state,
        JobState::Running
    );
    store
        .command(
            &Command::ClearContext {
                id: CommandId::new(),
                thread: root.thread.clone(),
                expected_epoch: 0,
            },
            &models,
        )
        .unwrap();
    assert_eq!(
        jobs::list(&store.db, &root.session).unwrap()[0].state,
        JobState::Stopping
    );
}

#[test]
fn shell_recovery_is_unknown_with_initial_result_not_replayed() {
    let (dir, mut store, root, _models) = store::tests::fixture();
    let (_, op, tool) = operation(
        &store,
        &root,
        "bash",
        json!({"command":"write effect","run_in_background":true}),
    );
    let tx = store.db.transaction().unwrap();
    jobs::start_shell(&tx, &tool).unwrap();
    tx.commit().unwrap();
    drop(store);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), root.session.clone()).unwrap();
    store.adopt_claim(claim, true).unwrap();
    assert_eq!(
        jobs::list(&store.db, &root.session).unwrap()[0].state,
        JobState::Unknown
    );
    let tx = store.db.transaction().unwrap();
    jobs::finish_shell(&tx, &op, Ok((result("stale worker"), None))).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        jobs::list(&store.db, &root.session).unwrap()[0].state,
        JobState::Unknown
    );
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE operation=?1 AND kind='tool_result'",
                [&op],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        1
    );
}

#[test]
fn job_queries_bound_input_and_preserve_exact_long_command() {
    let (_dir, store, root, _models) = store::tests::fixture();
    let (_, op, _) = operation(&store, &root, "bash", json!({"command":"λ".repeat(8192)}));
    assert_eq!(
        jobs::tool_input(&store.db, &op).unwrap()["command"],
        "λ".repeat(8192)
    );
    store
        .db
        .execute(
            "UPDATE operations SET input=?2 WHERE id=?1",
            params![op, encode(&json!({"content":"x".repeat(65536)})).unwrap()],
        )
        .unwrap();
    assert!(jobs::tool_input(&store.db, &op).is_err());
    let exact: String = store
        .db
        .query_row("SELECT input FROM operations WHERE id=?1", [&op], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(exact.len() > 65536);
}

#[test]
fn task_batch_rolls_back_every_launch_on_invalid_model() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let (run, op, _) = operation(&store, &root, "task", json!({}));
    let profile = profile(&store, &root);
    let tx = store.db.transaction().unwrap();
    assert!(delegation::launch_group(
        &tx,
        delegation::GroupRequest {
            operation: &op,
            run: &run,
            thread: &root.thread,
            name: "task",
            input: &json!({"tasks":[{"description":"valid","prompt":"work"},{"description":"bad","prompt":"work","model":"unavailable"}]}),
        },
        &profile,
        &models,
    )
    .is_err());
    drop(tx);
    assert!(children(&store, &op).is_empty());
}

#[test]
fn task_aggregation_retains_successful_siblings_and_restricts_roles() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let (run, op, _) = operation(&store, &root, "task", json!({}));
    let profile = profile(&store, &root);
    let input = json!({"tasks":[{"description":"one","prompt":"first"},{"description":"two","prompt":"second"}]});
    let tx = store.db.transaction().unwrap();
    assert!(delegation::launch_group(
        &tx,
        delegation::GroupRequest {
            operation: &op,
            run: &run,
            thread: &root.thread,
            name: "task",
            input: &input,
        },
        &profile,
        &models,
    )
    .unwrap()
    .is_none());
    tx.commit().unwrap();
    let ids = children(&store, &op);
    child_done(&mut store, &ids[0], "completed", "kept success");
    child_done(&mut store, &ids[1], "failed", "kept failure");
    let (pending, reports) = delegation::reports(&store.db, &op).unwrap();
    assert!(!pending);
    assert_eq!(reports["status"], "failed");
    assert_eq!(reports["agents"][0]["output"], "kept success");
    assert_eq!(reports["agents"][1]["output"], "kept failure");
    for name in ["task", "explore", "spawn_agent", "launch_agent", "workflow"] {
        assert!(!delegation::allowed("task", name));
        assert!(!delegation::allowed("explore", name));
    }
    assert!(delegation::allowed("task", "read_file"));
}

#[test]
fn stop_authority_is_descendant_only() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let (run, op, _) = operation(&store, &root, "launch_agent", json!({}));
    let profile = profile(&store, &root);
    let tx = store.db.transaction().unwrap();
    let a = delegation::launch(
        &tx,
        &op,
        &run,
        &root.thread,
        &profile,
        &models,
        Child {
            name: "a",
            prompt: "work",
            role: "collaborator",
            subagent_type: None,
            model: None,
            read_only: false,
            blocking: false,
            schema: None,
        },
    )
    .unwrap();
    let b = delegation::launch(
        &tx,
        &op,
        &run,
        &root.thread,
        &profile,
        &models,
        Child {
            name: "b",
            prompt: "work",
            role: "collaborator",
            subagent_type: None,
            model: None,
            read_only: false,
            blocking: false,
            schema: None,
        },
    )
    .unwrap();
    assert!(delegation::stop_child(&tx, &a, &b).is_err());
    assert!(delegation::stop_child(&tx, &a, &root.thread).is_err());
    delegation::stop_child(&tx, &root.thread, &a).unwrap();
    assert_eq!(
        tx.query_row("SELECT state FROM agents WHERE thread=?1", [&a], |r| r
            .get::<_, String>(
            0
        ))
        .unwrap(),
        "cancelled"
    );
    tx.commit().unwrap();
}

#[test]
fn workflow_fanout_preserves_partial_failures_and_skips_dependents() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"steps":[{"id":"map","fan_out":{"over":["first","second"],"prompt":"Inspect {item}"}},{"id":"reduce","agent":{"prompt":"Summarize {$map}"}}]});
    let (run, op, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let ids = children(&store, &op);
    assert_eq!(ids.len(), 2);
    child_done(&mut store, &ids[0], "completed", "first success");
    child_done(&mut store, &ids[1], "failed", "second failed");
    let output = workflow_tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "failed");
    assert_eq!(output["output"]["map"][0], "first success");
    assert_eq!(output["output"]["map"][1], Value::Null);
    assert_eq!(children(&store, &op).len(), 2);
    assert_eq!(output["agents"][1]["output"], "second failed");
}

#[test]
fn workflow_schema_checks_actual_output_and_never_invents_boolean() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"steps":[{"id":"check","agent":{"prompt":"Return actual result","schema":{"type":"object","required":["pass"],"properties":{"pass":{"type":"boolean"}}}}}]});
    let (run, op, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let child = children(&store, &op).remove(0);
    child_done(&mut store, &child, "completed", r#"{"pass":"probably"}"#);
    let output = workflow_tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "failed");
    assert!(output["agents"][0]["output"]
        .as_str()
        .unwrap()
        .contains("Actual output"));
}

#[test]
fn workflow_loop_persists_each_iteration_and_condition_result() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"steps":[{"id":"repair","loop":{"max":3,"until":"$check.pass","steps":[{"id":"check","agent":{"prompt":"Check","schema":{"type":"object","required":["pass"],"properties":{"pass":{"type":"boolean"}}}}}]}}]});
    let (run, op, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let child = children(&store, &op)[0].clone();
    child_done(&mut store, &child, "completed", r#"{"pass":false}"#);
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let child = children(&store, &op)[1].clone();
    child_done(&mut store, &child, "completed", r#"{"pass":true}"#);
    let output = workflow_tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "success");
    assert_eq!(output["output"]["repair"]["_iterations"], 2);
    assert_eq!(output["output"]["repair"]["_stop_reason"], "condition_met");
    assert_eq!(
        store
            .db
            .query_row("SELECT COUNT(*) FROM workflow_iterations", [], |r| r
                .get::<_, usize>(0))
            .unwrap(),
        2
    );
}

#[test]
fn workflow_restart_retains_children_without_relaunch() {
    let (dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"shape":"fan_out","items":["a","b"],"map_prompt":"read {item}"});
    let (run, op, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let ids = children(&store, &op);
    drop(store);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), root.session.clone()).unwrap();
    store.adopt_claim(claim, true).unwrap();
    let output = workflow_tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "failed");
    assert!(output["error"].as_str().unwrap().contains("unknown"));
    assert_eq!(children(&store, &op), ids);
    assert_eq!(
        jobs::list(&store.db, &root.session).unwrap()[0].state,
        JobState::Unknown
    );
}

#[test]
fn workflow_references_are_single_pass_and_external_schemas_rejected() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"steps":[{"id":"map","fan_out":{"over":["{$missing} {item.nope} λ"],"prompt":"Read {item}"}}]});
    let (run, op, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let child = children(&store, &op).remove(0);
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT text FROM submissions WHERE thread=?1",
                [child],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "Read {$missing} {item.nope} λ"
    );
    assert!(crate::workflow::schema::CompiledSchema::compile(
        &json!({"$ref":"https://example.invalid/schema"})
    )
    .is_err());
}

#[test]
fn tiny_progress_chunks_coalesce_into_bounded_spool_rows() {
    let (_dir, mut store, root, _) = store::tests::fixture();
    let (_, op, tool) = operation(
        &store,
        &root,
        "bash",
        json!({"command":"work","run_in_background":true}),
    );
    let tx = store.db.transaction().unwrap();
    let job = jobs::start_shell(&tx, &tool).unwrap();
    tx.commit().unwrap();
    for _ in 0..1024 {
        let (reply, _ack) = oneshot::channel();
        jobs::append(
            &mut store.db,
            jobs::ProgressUpdate {
                operation: op.clone(),
                text: "x".into(),
                reply,
            },
        )
        .unwrap();
    }
    assert_eq!(
        store
            .db
            .query_row("SELECT COUNT(*) FROM job_spool WHERE job=?1", [&job], |r| r
                .get::<_, usize>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        jobs::page(&store.db, &job, 0, 24_000).unwrap().text,
        "x".repeat(1024)
    );
}

#[test]
fn workflow_cleanup_waits_for_child_and_settles_after_parent_cancellation() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"steps":[{"id":"work","agent":{"prompt":"work"}}]});
    let (run, op, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let child = children(&store, &op).remove(0);
    let child_run = RunId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
            params![child_run, child],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE agents SET state='running' WHERE thread=?1",
            [&child],
        )
        .unwrap();
    store
        .command(
            &Command::Stop {
                id: CommandId::new(),
                run,
            },
            &models,
        )
        .unwrap();
    store
        .db
        .execute("UPDATE operations SET state='cancelled' WHERE id=?1", [&op])
        .unwrap();
    let tx = store.db.transaction().unwrap();
    crate::workflow::cleanup(&tx, &root.session, &models).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        jobs::list(&store.db, &root.session).unwrap()[0].state,
        JobState::Stopping
    );
    let tx = store.db.transaction().unwrap();
    settle_run(&tx, &child_run, "cancelled", "Stopped child").unwrap();
    crate::workflow::cleanup(&tx, &root.session, &models).unwrap();
    tx.commit().unwrap();
    let listed = jobs::list(&store.db, &root.session).unwrap();
    assert_eq!(listed[0].state, JobState::Cancelled);
    assert!(listed[0].result.is_some());
    let listed = crate::coordination::list(&store.db, &root.thread, &json!({})).unwrap();
    let agents: Vec<_> = listed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|agent| agent["thread_id"] == child.0)
        .collect();
    assert_eq!(agents[0]["thread_id"], child.0);
    assert_eq!(agents[0]["state"], "cancelled");
    assert!(agents[0]["name"].is_string());
}

#[test]
fn relative_permission_grant_is_workspace_anchored_and_rejects_traversal() {
    let (_dir, store, root, _models) = store::tests::fixture();
    let (_, _, tool) = operation(&store, &root, "write_file", json!({"path":"src/main.rs"}));
    let choices = crate::approvals::choices(&tool, true);
    let grant = choices.iter().find_map(|c| c.grant.clone()).unwrap();
    let expected = std::path::Path::new(&tool.invocation.workspace).join("src/main.rs");
    assert_eq!(grant.path_prefix.as_deref(), expected.to_str());
    let permissions = crate::PermissionPolicy {
        grants: vec![grant],
        ..Default::default()
    };
    let caps = crate::ToolCapabilities {
        mutates_workspace: true,
        plan_safe: false,
        ..Default::default()
    };
    let profile = profile(&store, &root);
    assert_eq!(
        crate::policy::evaluate_in_workspace(
            &Policy::default(),
            &permissions,
            &profile,
            "write_file",
            &tool.invocation.input,
            &caps,
            &tool.invocation.workspace
        ),
        crate::RuleDecision::Allow
    );
    assert!(crate::policy::grant_path(&tool.invocation.workspace, "../secret").is_none());
}
