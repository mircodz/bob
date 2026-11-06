use super::*;
use async_trait::async_trait;
use bob_types::{Completion, ContentBlock, Message, Role, StopReason, Usage};
use rusqlite::Connection;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, Notify};

#[path = "workflow_yield_tests.rs"]
mod workflow_yield_tests;

fn artifact_count(db: &Connection) -> usize {
    db.query_row("SELECT COUNT(*) FROM artifacts", [], |r| r.get(0))
        .unwrap()
}

fn agent_outcome(db: &Connection, child: &ThreadId) -> (String, ArtifactId) {
    db.query_row(
        "SELECT state,outcome FROM agents WHERE thread=?1",
        [child],
        |r| Ok((r.get(0)?, ArtifactId(r.get(1)?))),
    )
    .unwrap()
}

fn launch_child(
    store: &mut Store,
    parent: &Receipt,
    run: &RunId,
    op: &OperationId,
    models: &[ModelInfo],
    name: &str,
) -> Receipt {
    let profile = profile(store, parent);
    let tx = store.db.transaction().unwrap();
    let thread = delegation::launch(
        &tx,
        op,
        run,
        &parent.thread,
        &profile,
        models,
        Child {
            name,
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
    tx.commit().unwrap();
    Receipt {
        thread,
        ..parent.clone()
    }
}

#[test]
fn finder_repeated_polls_reuse_pass_zero_artifact() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"shape":"loop","find_prompt":"Find new items","max_rounds":3});
    let (run, op, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let first = children(&store, &op)[0].clone();
    child_done(&mut store, &first, "completed", r#"{"findings":["kept"]}"#);
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let ids = children(&store, &op);
    assert_eq!(ids.len(), 2);
    let pass_zero: String = store
        .db
        .query_row(
            "SELECT result FROM workflow_nodes WHERE operation=?1 AND path='find/pass0'",
            [&op],
            |r| r.get(0),
        )
        .unwrap();
    let count = artifact_count(&store.db);
    for _ in 0..16 {
        assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
        assert_eq!(artifact_count(&store.db), count);
        assert_eq!(children(&store, &op), ids);
        let iterations: Vec<(usize, String)> = store.db.prepare(
            "SELECT iteration,result FROM workflow_iterations WHERE operation=?1 AND node='find' ORDER BY iteration"
        ).unwrap().query_map([&op], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
            .collect::<Result<_, _>>().unwrap();
        assert_eq!(iterations, vec![(0, pass_zero.clone())]);
    }
    child_done(&mut store, &ids[1], "completed", r#"{"findings":[]}"#);
    let output = workflow_tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "success");
    assert_eq!(output["output"]["findings"], json!(["kept"]));
    assert_eq!(output["output"]["termination_reason"], "dry");
    let count = artifact_count(&store.db);
    assert_eq!(
        workflow_tick(&mut store, &root, &run, &op, &models, &input),
        Some(output)
    );
    assert_eq!(artifact_count(&store.db), count);
}

#[test]
fn aggregate_overflow_fails_workflow_and_job_but_retains_child_and_node_artifacts() {
    let (dir, mut store, root, models) = store::tests::fixture();
    let input = json!({"shape":"fan_out","items":["one","two"],"map_prompt":"Read {item}"});
    let (run, op, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let ids = children(&store, &op);
    assert_eq!(ids.len(), 2);
    let texts = ["a".repeat(MAX_ARTIFACT / 3), "b".repeat(MAX_ARTIFACT / 3)];
    assert!(
        encode(&json!({"results":texts,"outcomes":texts}))
            .unwrap()
            .len()
            > MAX_ARTIFACT
    );
    for (child, text) in ids.iter().zip(&texts) {
        child_done(&mut store, child, "completed", text);
    }
    let output = workflow_tick(&mut store, &root, &run, &op, &models, &input).unwrap();
    assert_eq!(output["status"], "failed");
    assert!(output["error"]
        .as_str()
        .unwrap()
        .contains("Aggregate exceeds retention budget"));
    assert_eq!(output["agents"].as_array().unwrap().len(), 2);
    let retained: Vec<_> = ids
        .iter()
        .map(|child| agent_outcome(&store.db, child))
        .collect();
    for (index, ((state, artifact), text)) in retained.iter().zip(&texts).enumerate() {
        assert_eq!(state, "completed");
        assert_eq!(full_artifact(&store.db, artifact).unwrap(), *text);
        let node = ArtifactId(store.db.query_row(
            "SELECT result FROM workflow_nodes WHERE operation=?1 AND path=?2 AND state='completed'",
            params![op, format!("map/{index}")], |r| r.get(0),
        ).unwrap());
        let value: Value = decode(full_artifact(&store.db, &node).unwrap()).unwrap();
        assert_eq!(value["output"], *text);
        assert!(value["error"].is_null());
    }
    let (workflow_state, workflow_result): (String, String) = store
        .db
        .query_row(
            "SELECT state,result FROM workflows WHERE operation=?1",
            [&op],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(workflow_state, "failed");
    let job = jobs::list(&store.db, &root.session).unwrap().remove(0);
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.result.as_ref().unwrap().0, workflow_result);
    assert_eq!(
        serde_json::from_str::<Value>(
            &full_artifact(&store.db, job.result.as_ref().unwrap()).unwrap()
        )
        .unwrap(),
        output
    );
    drop(store);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    assert_eq!(
        jobs::list(&store.db, &root.session).unwrap()[0].state,
        JobState::Failed
    );
    let count = artifact_count(&store.db);
    assert_eq!(
        workflow_tick(&mut store, &root, &run, &op, &models, &input),
        Some(output)
    );
    assert_eq!(artifact_count(&store.db), count);
    for ((_, artifact), text) in retained.iter().zip(&texts) {
        assert_eq!(full_artifact(&store.db, artifact).unwrap(), *text);
    }
}

#[test]
fn stop_finished_parent_stops_descendants_and_jobs_without_touching_sibling() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let (run, op, _) = operation(&store, &root, "launch_agent", json!({}));
    let parent = launch_child(&mut store, &root, &run, &op, &models, "parent");
    let sibling = launch_child(&mut store, &root, &run, &op, &models, "unrelated");
    let (parent_run, parent_op, parent_tool) = operation(
        &store,
        &parent,
        "bash",
        json!({"command":"parent job","run_in_background":true}),
    );
    let active = launch_child(
        &mut store,
        &parent,
        &parent_run,
        &parent_op,
        &models,
        "active descendant",
    );
    let (active_run, active_op, active_tool) = operation(
        &store,
        &active,
        "bash",
        json!({"command":"descendant job","run_in_background":true}),
    );
    let queued = launch_child(
        &mut store,
        &active,
        &active_run,
        &active_op,
        &models,
        "queued descendant",
    );
    let (sibling_run, _, sibling_tool) = operation(
        &store,
        &sibling,
        "bash",
        json!({"command":"unrelated job","run_in_background":true}),
    );
    let tx = store.db.transaction().unwrap();
    let parent_job = jobs::start_shell(&tx, &parent_tool).unwrap();
    let active_job = jobs::start_shell(&tx, &active_tool).unwrap();
    let sibling_job = jobs::start_shell(&tx, &sibling_tool).unwrap();
    for child in [&parent.thread, &active.thread, &sibling.thread] {
        tx.execute("UPDATE agents SET state='running' WHERE thread=?1", [child])
            .unwrap();
        tx.execute(
            "UPDATE submissions SET state='settled' WHERE thread=?1",
            [child],
        )
        .unwrap();
    }
    tx.execute(
        "UPDATE runs SET state='completed' WHERE id=?1",
        [&parent_run],
    )
    .unwrap();
    delegation::settle(&tx, &parent.thread, "completed", "parent already finished").unwrap();
    tx.commit().unwrap();
    let retained = agent_outcome(&store.db, &parent.thread);
    let tx = store.db.transaction().unwrap();
    delegation::stop_child(&tx, &root.thread, &parent.thread).unwrap();
    tx.commit().unwrap();
    assert_eq!(agent_outcome(&store.db, &parent.thread), retained);
    for (run, expected) in [
        (&parent_run, "completed"),
        (&active_run, "stopping"),
        (&sibling_run, "active"),
    ] {
        assert_eq!(
            store
                .db
                .query_row("SELECT state FROM runs WHERE id=?1", [run], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            expected
        );
    }
    for (thread, state, held) in [
        (&parent.thread, "completed", true),
        (&active.thread, "running", true),
        (&queued.thread, "cancelled", true),
        (&sibling.thread, "running", false),
    ] {
        assert_eq!(store.db.query_row("SELECT a.state,t.held FROM agents a JOIN threads t ON t.id=a.thread WHERE a.thread=?1", [thread], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?))).unwrap(), (state.into(), held));
    }
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT state FROM submissions WHERE thread=?1",
                [&queued.thread],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "cancelled"
    );
    for (job, expected) in [
        (&parent_job, "stopping"),
        (&active_job, "stopping"),
        (&sibling_job, "running"),
    ] {
        assert_eq!(
            store
                .db
                .query_row("SELECT state FROM jobs WHERE id=?1", [job], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            expected
        );
    }
    assert!(!store
        .db
        .query_row(
            "SELECT held FROM threads WHERE id=?1",
            [&root.thread],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
}

#[test]
fn delegation_terminal_settlement_is_idempotent_for_duplicate_and_conflicting_reports() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let (run, op, _) = operation(&store, &root, "launch_agent", json!({}));
    for initial in ["completed", "failed", "cancelled", "unknown"] {
        let child = launch_child(&mut store, &root, &run, &op, &models, initial);
        child_done(
            &mut store,
            &child.thread,
            initial,
            "first authoritative report",
        );
        let retained = agent_outcome(&store.db, &child.thread);
        assert_eq!(retained.0, initial);
        let count = artifact_count(&store.db);
        for next in [initial, "completed", "failed", "cancelled", "unknown"] {
            let tx = store.db.transaction().unwrap();
            delegation::settle(&tx, &child.thread, next, "late conflicting report").unwrap();
            tx.commit().unwrap();
            assert_eq!(agent_outcome(&store.db, &child.thread), retained);
            assert_eq!(artifact_count(&store.db), count);
            assert_eq!(
                full_artifact(&store.db, &retained.1).unwrap(),
                "first authoritative report"
            );
        }
    }
}

#[test]
fn delegation_settlement_preserves_unknown_without_an_existing_outcome() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let (run, op, _) = operation(&store, &root, "launch_agent", json!({}));
    for unknown_operation in [false, true] {
        let child = launch_child(&mut store, &root, &run, &op, &models, "uncertain child");
        if unknown_operation {
            let (_, effect, _) = operation(
                &store,
                &child,
                "bash",
                json!({"command":"uncertain effect"}),
            );
            store
                .db
                .execute(
                    "UPDATE operations SET state='unknown' WHERE id=?1",
                    [&effect],
                )
                .unwrap();
        } else {
            store
                .db
                .execute(
                    "UPDATE agents SET state='unknown' WHERE thread=?1",
                    [&child.thread],
                )
                .unwrap();
        }
        child_done(
            &mut store,
            &child.thread,
            "completed",
            "late successful worker",
        );
        let retained = agent_outcome(&store.db, &child.thread);
        assert_eq!(retained.0, "unknown");
        assert_eq!(
            full_artifact(&store.db, &retained.1).unwrap(),
            "late successful worker"
        );
        let count = artifact_count(&store.db);
        child_done(
            &mut store,
            &child.thread,
            "failed",
            "later conflicting worker",
        );
        assert_eq!(agent_outcome(&store.db, &child.thread), retained);
        assert_eq!(artifact_count(&store.db), count);
    }
}

fn failure_workflow() -> Value {
    json!({"steps":[
        {"id":"prior","agent":{"prompt":"prior success"}},
        {"id":"parallel","parallel":{"branches":{
            "a_hold":{"agent":{"prompt":"hold until cancelled"}},
            "z_fail":{"loop":{"max":1,"steps":[
                {"id":"seed","agent":{"prompt":"empty prompt seed","schema":{"type":"object","required":["prompt"],"properties":{"prompt":{"type":"string"}}}}},
                {"id":"expand","agent":{"prompt":"{$seed.prompt}"}}
            ]}}
        }}}
    ]})
}

struct FailureExecutor {
    models: Vec<ModelInfo>,
    started: Notify,
    cancelled: Notify,
    release: Notify,
}

fn completion(text: &str) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        },
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

#[async_trait]
impl Executor for FailureExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        request: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        let prompt = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User)
            .and_then(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            })
            .unwrap();
        let prompt = prompt.rsplit('\n').next().unwrap();
        match prompt {
            "run failure workflow"
                if !request.messages.iter().any(|message| {
                    message
                        .content
                        .iter()
                        .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
                }) =>
            {
                Ok(Completion {
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: "workflow-call".into(),
                            name: "workflow".into(),
                            input: failure_workflow(),
                        }],
                    },
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                })
            }
            "prior success" => Ok(completion("retained prior success")),
            "hold until cancelled" => {
                self.started.notify_one();
                crate::cancelled(&mut cancel).await;
                self.cancelled.notify_one();
                self.release.notified().await;
                anyhow::bail!("sibling cancellation acknowledged")
            }
            "empty prompt seed" => {
                self.started.notified().await;
                Ok(completion(r#"{"prompt":""}"#))
            }
            "run failure workflow" => Ok(completion("workflow failed as expected")),
            _ => anyhow::bail!("unexpected prompt: {prompt}"),
        }
    }
    async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
        anyhow::bail!("unexpected external preparation")
    }
    async fn execute(
        &self,
        _: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        anyhow::bail!("unexpected external execution")
    }
}

#[tokio::test]
async fn runtime_empty_expansion_retains_prior_success_and_waits_for_sibling_cancellation() {
    let (dir, store, root, models) = store::tests::fixture();
    drop(store);
    let executor = Arc::new(FailureExecutor {
        models,
        started: Notify::new(),
        cancelled: Notify::new(),
        release: Notify::new(),
    });
    let path = dir.0.join("lab.db");
    let handle = crate::Runtime::open_paused(&path, executor.clone())
        .await
        .unwrap();
    let db =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.busy_timeout(Duration::from_secs(2)).unwrap();
    handle
        .activate_session(
            root.session.clone(),
            vec![Command::Submit {
                id: CommandId::new(),
                thread: root.thread.clone(),
                text: "run failure workflow".into(),
            }],
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), executor.cancelled.notified())
        .await
        .expect("workflow never cancelled its running sibling");
    let op = OperationId(
        db.query_row("SELECT operation FROM workflows", [], |r| r.get(0))
            .unwrap(),
    );
    let (workflow_state, workflow_result, operation_state): (String, Option<String>, String) = db.query_row(
        "SELECT w.state,w.result,o.state FROM workflows w JOIN operations o ON o.id=w.operation WHERE w.operation=?1",
        [&op], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).unwrap();
    assert_eq!(workflow_state, "failing");
    assert!(workflow_result.is_none());
    assert_eq!(operation_state, "waiting");
    let jobs = handle.jobs(root.session.clone()).await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, JobState::Running);
    assert!(jobs[0].result.is_none());
    assert_eq!(db.query_row("SELECT COUNT(*) FROM entries WHERE operation=?1 AND kind IN ('tool_result','tool_error')", [&op], |r| r.get::<_, usize>(0)).unwrap(), 0);
    let prior = ArtifactId(db.query_row("SELECT result FROM workflow_nodes WHERE operation=?1 AND path='root/prior' AND state='completed'", [&op], |r| r.get(0)).unwrap());
    let prior_value: Value = decode(full_artifact(&db, &prior).unwrap()).unwrap();
    assert_eq!(prior_value["output"], "retained prior success");
    let held_child = ThreadId(
        db.query_row(
            "SELECT child FROM workflow_nodes WHERE operation=?1 AND path='root/parallel/a_hold'",
            [&op],
            |r| r.get(0),
        )
        .unwrap(),
    );
    assert_eq!(
        db.query_row(
            "SELECT state FROM runs WHERE thread=?1 ORDER BY ordinal DESC LIMIT 1",
            [&held_child],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "stopping"
    );
    executor.release.notify_one();
    let mut live = handle.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let done: bool = db
                .query_row(
                    "SELECT state='failed' FROM operations WHERE id=?1",
                    [&op],
                    |r| r.get(0),
                )
                .unwrap();
            if done {
                break;
            }
            live.changed().await.unwrap();
        }
    })
    .await
    .expect("workflow never finalized after sibling cancellation");
    let output: Value =
        serde_json::from_str(&handle.tool_output(op.clone(), 0, 24000).await.unwrap().text)
            .unwrap();
    assert_eq!(output["status"], "failed");
    assert!(output["error"]
        .as_str()
        .unwrap()
        .contains("child prompt must contain"));
    assert!(output["output"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["path"] == "root/prior" && node["result_artifact"] == prior.0));
    assert!(
        output["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["status"] == "completed"
                && agent["output"] == "retained prior success")
    );
    assert_eq!(agent_outcome(&db, &held_child).0, "cancelled");
    let job = handle.jobs(root.session.clone()).await.unwrap().remove(0);
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(
        serde_json::from_str::<Value>(&full_artifact(&db, job.result.as_ref().unwrap()).unwrap())
            .unwrap(),
        output
    );
    assert_eq!(db.query_row("SELECT COUNT(*) FROM entries WHERE operation=?1 AND kind IN ('tool_result','tool_error')", [&op], |r| r.get::<_, usize>(0)).unwrap(), 1);
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM agents WHERE operation=?1",
            [&op],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        3
    );
    handle.shutdown().await.unwrap();
}
