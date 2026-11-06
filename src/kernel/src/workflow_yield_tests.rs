use super::*;
use crate::contracts::{PermissionPolicy, PermissionRule, RuleDecision};
use std::collections::VecDeque;

fn pipeline(parallel: bool) -> Value {
    let second = if parallel {
        json!({"id":"second","parallel":{"branches":{
            "a_admit":{"agent":{"prompt":"must be rolled back"}},
            "z_empty":{"agent":{"prompt":"{$first.prompt}"}}
        }}})
    } else {
        json!({"id":"second","agent":{"prompt":"{$first.prompt}"}})
    };
    json!({"steps":[
        {"id":"first","agent":{"prompt":"yield-first-fixture","schema":{
            "type":"object","required":["prompt"],"properties":{"prompt":{"type":"string"}}
        }}},
        second
    ]})
}

fn call(name: &str, input: Value) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: format!("call-{name}"),
                name: name.into(),
                input,
            }],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

#[derive(Debug)]
struct YieldRequest {
    spec: RequestSpec,
    reply: oneshot::Sender<Completion>,
}
impl YieldRequest {
    fn finish(self, answer: Completion) {
        self.reply.send(answer).unwrap();
    }
    fn contains(&self, needle: &str) -> bool {
        self.spec
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|block| match block {
                ContentBlock::Text { text } => text.contains(needle),
                ContentBlock::ToolResult { content, .. } => content.model_text().contains(needle),
                _ => false,
            })
    }
}
struct YieldExecutor {
    models: Vec<ModelInfo>,
    send: mpsc::UnboundedSender<YieldRequest>,
}
#[async_trait]
impl Executor for YieldExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        spec: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        let (reply, receive) = oneshot::channel();
        self.send.send(YieldRequest { spec, reply }).unwrap();
        tokio::select! {
            result = receive => Ok(result?),
            _ = crate::cancelled(&mut cancel) => anyhow::bail!("cancelled fixture request"),
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
struct YieldRequests {
    receive: mpsc::UnboundedReceiver<YieldRequest>,
    pending: VecDeque<YieldRequest>,
}
impl YieldRequests {
    async fn next(&mut self, prompt: &str) -> YieldRequest {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(index) = self.pending.iter().position(|request| {
                    request
                        .spec
                        .messages
                        .first()
                        .is_some_and(|m| m.text().contains(prompt))
                }) {
                    return self.pending.remove(index).unwrap();
                }
                self.pending
                    .push_back(self.receive.recv().await.expect("request channel closed"));
            }
        })
        .await
        .expect("expected gated model request")
    }
}

fn committed_call(db: &Connection, op: &OperationId) -> (String, String, String, usize) {
    db.query_row(
        "SELECT state,call_id,result,(SELECT COUNT(*) FROM entries WHERE operation=o.id AND kind IN ('tool_result','tool_error')) FROM operations o WHERE id=?1",
        [op], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    ).unwrap()
}
fn workflow_output(db: &Connection, op: &OperationId) -> Value {
    let result = ArtifactId(
        db.query_row(
            "SELECT result FROM workflows WHERE operation=?1",
            [op],
            |r| r.get(0),
        )
        .unwrap(),
    );
    decode(full_artifact(db, &result).unwrap()).unwrap()
}
fn node_count(db: &Connection, op: &OperationId) -> usize {
    db.query_row(
        "SELECT COUNT(*) FROM workflow_nodes WHERE operation=?1",
        [op],
        |r| r.get(0),
    )
    .unwrap()
}
fn cleanup(store: &mut Store, root: &Receipt, models: &[ModelInfo]) {
    let tx = store.db.transaction().unwrap();
    crate::workflow::cleanup(&tx, &root.session, models).unwrap();
    tx.commit().unwrap();
}

#[tokio::test]
async fn yielded_empty_expansion_fails_only_workflow_and_actor_keeps_running() {
    let (dir, store, root, models) = store::tests::fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let (send, receive) = mpsc::unbounded_channel();
    let handle = crate::Runtime::open_paused(&path, Arc::new(YieldExecutor { models, send }))
        .await
        .unwrap();
    let mut requests = YieldRequests {
        receive,
        pending: VecDeque::new(),
    };
    let db =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.busy_timeout(Duration::from_secs(2)).unwrap();
    handle
        .activate_session(
            root.session.clone(),
            vec![Command::Submit {
                id: CommandId::new(),
                thread: root.thread.clone(),
                text: "yield-root-fixture".into(),
            }],
        )
        .await
        .unwrap();
    requests
        .next("yield-root-fixture")
        .await
        .finish(call("workflow", pipeline(false)));
    requests.next("yield-first-fixture").await.finish(call(
        "send_message",
        json!({
            "to":root.thread,"message":"need yield guidance"
        }),
    ));
    let root_request = requests.next("yield-root-fixture").await;
    assert!(root_request.contains("need yield guidance"));
    assert!(root_request.contains("Collection yielded"));
    let op = OperationId(
        db.query_row("SELECT operation FROM workflows", [], |r| r.get(0))
            .unwrap(),
    );
    let frozen = committed_call(&db, &op);
    assert_eq!(frozen.0, "done");
    assert_eq!(frozen.3, 1);
    root_request.finish(call(
        "spawn_agent",
        json!({"name":"healthy sibling","task":"yield-healthy-fixture"}),
    ));
    let healthy = requests.next("yield-healthy-fixture").await;
    let parent = requests.next("yield-root-fixture").await;
    requests
        .next("yield-first-fixture")
        .await
        .finish(completion(r#"{"prompt":""}"#));
    let mut live = handle.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let failed: bool = db
                .query_row(
                    "SELECT state='failed' FROM workflows WHERE operation=?1",
                    [&op],
                    |r| r.get(0),
                )
                .unwrap();
            if failed {
                break;
            }
            live.changed().await.unwrap();
        }
    })
    .await
    .expect("yielded workflow did not settle");
    let output = workflow_output(&db, &op);
    assert_eq!(output["status"], "failed");
    assert!(output["error"]
        .as_str()
        .unwrap()
        .contains("child prompt must contain"));
    assert_eq!(output["agents"][0]["output"], r#"{"prompt":""}"#);
    assert_eq!(node_count(&db, &op), 2);
    assert_eq!(committed_call(&db, &op), frozen);
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM agents WHERE operation=?1",
            [&op],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
    assert!(handle
        .view(Some(root.thread.clone()), None, 100)
        .await
        .unwrap()
        .error
        .is_none());
    healthy.finish(completion("healthy sibling retained"));
    let mut live = handle.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let completed: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM agents a JOIN threads t ON t.id=a.thread WHERE t.name='healthy sibling' AND a.state='completed')", [], |r| r.get(0)).unwrap();
            if completed { break; }
            live.changed().await.unwrap();
        }
    }).await.expect("unrelated sibling did not complete");
    parent.finish(completion("waiting for retained results"));
    let parent = requests.next("yield-root-fixture").await;
    assert!(parent.contains("healthy sibling retained"));
    assert!(parent.contains("child prompt must contain"));
    parent.finish(completion("collected failed workflow and healthy sibling"));
    let mut live = handle.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let completed: bool = db.query_row("SELECT state='completed' FROM runs WHERE thread=?1 ORDER BY ordinal DESC LIMIT 1", [&root.thread], |r| r.get(0)).unwrap();
            if completed { break; }
            live.changed().await.unwrap();
        }
    }).await.expect("owner did not complete after collecting both results");
    assert_eq!(committed_call(&db, &op), frozen);
    assert!(handle
        .view(Some(root.thread.clone()), None, 100)
        .await
        .unwrap()
        .error
        .is_none());
    handle.shutdown().await.unwrap();
}

fn yielded(store: &mut Store, root: &Receipt, run: &RunId, op: &OperationId) {
    let child = children(store, op).last().unwrap().clone();
    let child_receipt = Receipt {
        thread: child.clone(),
        ..root.clone()
    };
    let (_, message, _) = operation(store, &child_receipt, "send_message", json!({}));
    let tx = store.db.transaction().unwrap();
    tx.execute(
        "UPDATE agents SET state='running' WHERE thread=?1",
        [&child],
    )
    .unwrap();
    tx.execute(
        "UPDATE submissions SET state='settled' WHERE thread=?1",
        [&child],
    )
    .unwrap();
    crate::coordination::execute(
        &tx,
        &message,
        &child,
        "send_message",
        &json!({"to":root.thread,"message":"guidance before next node"}),
    )
    .unwrap();
    assert!(crate::lifecycle::guidance(&tx, run).unwrap());
    tx.execute("UPDATE jobs SET background=1 WHERE operation=?1", [op])
        .unwrap();
    let job: String = tx
        .query_row("SELECT job FROM workflows WHERE operation=?1", [op], |r| {
            r.get(0)
        })
        .unwrap();
    finish_tool_entry(
        &tx,
        op,
        &json!({"status":"running","job_id":job}).to_string(),
        false,
    )
    .unwrap();
    tx.execute("UPDATE operations SET state='done' WHERE id=?1", [op])
        .unwrap();
    tx.commit().unwrap();
}

fn sibling_operation(
    store: &Store,
    root: &Receipt,
    run: &RunId,
    name: &str,
    input: &Value,
) -> OperationId {
    let op = OperationId::new();
    store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,?4,'tool',?1,?5,'ready')", params![op,run,root.thread,name,encode(input).unwrap()]).unwrap();
    op
}

#[test]
fn yielded_admission_error_rolls_back_partial_children_and_keeps_other_workflow_results() {
    let (_dir, mut store, root, models) = store::tests::fixture();
    let mut input = pipeline(true);
    input["steps"].as_array_mut().unwrap().insert(
        0,
        json!({"id":"prior","agent":{"prompt":"prior retained work"}}),
    );
    let (run, op, _) = operation(&store, &root, "workflow", input.clone());
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let prior = children(&store, &op)[0].clone();
    child_done(&mut store, &prior, "completed", "prior retained result");
    assert!(workflow_tick(&mut store, &root, &run, &op, &models, &input).is_none());
    let prior_result = ArtifactId(
        store
            .db
            .query_row(
                "SELECT result FROM workflow_nodes WHERE operation=?1 AND path='root/prior'",
                [&op],
                |r| r.get(0),
            )
            .unwrap(),
    );
    yielded(&mut store, &root, &run, &op);
    let frozen = committed_call(&store.db, &op);
    let first = children(&store, &op)[1].clone();
    child_done(&mut store, &first, "completed", r#"{"prompt":""}"#);
    let retained = agent_outcome(&store.db, &first);
    let healthy_input = json!({"steps":[{"id":"healthy","agent":{"prompt":"unrelated"}}]});
    let healthy_op = sibling_operation(&store, &root, &run, "workflow", &healthy_input);
    assert!(workflow_tick(
        &mut store,
        &root,
        &run,
        &healthy_op,
        &models,
        &healthy_input
    )
    .is_none());
    yielded(&mut store, &root, &run, &healthy_op);
    let healthy_child = children(&store, &healthy_op)[0].clone();
    child_done(
        &mut store,
        &healthy_child,
        "completed",
        "independent result",
    );
    cleanup(&mut store, &root, &models);
    let failed = workflow_output(&store.db, &op);
    assert_eq!(failed["status"], "failed");
    assert!(failed["error"]
        .as_str()
        .unwrap()
        .contains("child prompt must contain"));
    assert_eq!(children(&store, &op), vec![prior, first.clone()]);
    assert_eq!(node_count(&store.db, &op), 3);
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT executions FROM workflows WHERE operation=?1",
                [&op],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT COUNT(*) FROM workflow_dependencies WHERE operation=?1",
                [&op],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .db
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get::<_, usize>(0))
            .unwrap(),
        4
    );
    let prior_value: Value = decode(full_artifact(&store.db, &prior_result).unwrap()).unwrap();
    assert_eq!(prior_value["output"], "prior retained result");
    assert!(failed["output"]
        .as_array()
        .unwrap()
        .iter()
        .any(|node| node["path"] == "root/prior" && node["result_artifact"] == prior_result.0));
    assert_eq!(agent_outcome(&store.db, &first), retained);
    assert_eq!(
        full_artifact(&store.db, &retained.1).unwrap(),
        r#"{"prompt":""}"#
    );
    assert_eq!(committed_call(&store.db, &op), frozen);
    assert_eq!(
        workflow_output(&store.db, &healthy_op)["output"]["healthy"],
        "independent result"
    );
    let count = artifact_count(&store.db);
    cleanup(&mut store, &root, &models);
    assert_eq!(artifact_count(&store.db), count);
}

async fn wait_for_state(handle: &crate::Handle, db: &Connection, sql: &str) {
    let mut live = handle.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if db.query_row(sql, [], |r| r.get::<_, bool>(0)).unwrap() {
                break;
            }
            live.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state did not settle: {sql}"));
}

async fn reject_inline_workflow(case: &str) {
    let (dir, store, root, models) = store::tests::fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let (send, receive) = mpsc::unbounded_channel();
    let handle = crate::Runtime::open_paused(&path, Arc::new(YieldExecutor { models, send }))
        .await
        .unwrap();
    let mut requests = YieldRequests {
        receive,
        pending: VecDeque::new(),
    };
    let db =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    db.busy_timeout(Duration::from_secs(2)).unwrap();
    handle
        .activate_session(
            root.session.clone(),
            vec![Command::Submit {
                id: CommandId::new(),
                thread: root.thread.clone(),
                text: "inline-root-fixture".into(),
            }],
        )
        .await
        .unwrap();
    requests.next("inline-root-fixture").await.finish(call(
        "spawn_agent",
        json!({"name":"healthy sibling","task":"inline-healthy-fixture"}),
    ));
    let healthy = requests.next("inline-healthy-fixture").await;
    requests.next("inline-root-fixture").await.finish(call(
        "workflow",
        json!({"steps":[
            {"id":"prior","agent":{"prompt":"inline-prior-fixture"}},
            {"id":"blocked","agent":{"prompt":"inline-blocked-fixture"}},
            {"id":"later","agent":{"prompt":"must never be admitted"}}
        ]}),
    ));
    requests
        .next("inline-prior-fixture")
        .await
        .finish(completion("retained prior result"));
    let mut blocked = requests.next("inline-blocked-fixture").await;
    let op = OperationId(
        db.query_row("SELECT operation FROM workflows", [], |r| r.get(0))
            .unwrap(),
    );
    assert!(db
        .query_row(
            "SELECT o.state='waiting' AND j.background=0 FROM operations o JOIN jobs j ON j.operation=o.id WHERE o.id=?1",
            [&op],
            |r| r.get::<_, bool>(0),
        )
        .unwrap());
    if case == "policy" {
        let mut policy = Policy::default();
        policy.denied_tools.insert("workflow".into());
        handle
            .command(Command::SetPolicy {
                id: CommandId::new(),
                session: root.session.clone(),
                expected_revision: 0,
                policy,
            })
            .await
            .unwrap();
    } else {
        handle
            .command(Command::SetPermissions {
                id: CommandId::new(),
                session: root.session.clone(),
                expected_revision: 0,
                permissions: PermissionPolicy {
                    rules: vec![PermissionRule {
                        tool: "workflow".into(),
                        decision: if case == "approval" {
                            RuleDecision::RequireApproval
                        } else {
                            RuleDecision::Deny
                        },
                    }],
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        if case == "approval" {
            wait_for_state(
                &handle,
                &db,
                "SELECT EXISTS(SELECT 1 FROM decisions WHERE state='pending' AND revision=1)",
            )
            .await;
            handle
                .command(Command::Decide {
                    id: CommandId::new(),
                    operation: op.clone(),
                    revision: 1,
                    approve: false,
                })
                .await
                .unwrap();
        }
    }
    let parent = requests.next("inline-root-fixture").await;
    assert!(parent.contains(if case == "approval" {
        "Permission denied by user."
    } else {
        "Explicit denial or capability restriction"
    }));
    let state: String = db
        .query_row(
            "SELECT state FROM workflows WHERE operation=?1",
            [&op],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        matches!(state.as_str(), "failing" | "failed"),
        "{case}: {state}"
    );
    tokio::time::timeout(Duration::from_secs(10), blocked.reply.closed())
        .await
        .expect("denied workflow child was not cancelled");
    assert!(
        !healthy.reply.is_closed(),
        "unrelated sibling was cancelled"
    );
    wait_for_state(
        &handle,
        &db,
        "SELECT EXISTS(SELECT 1 FROM workflows w JOIN jobs j ON j.id=w.job WHERE w.state='failed' AND j.state='failed')",
    )
    .await;
    let frozen = committed_call(&db, &op);
    assert_eq!(frozen.0, "failed");
    assert_eq!(frozen.3, 1);
    let output = workflow_output(&db, &op);
    assert_eq!(output["agents"].as_array().unwrap().len(), 2);
    assert_eq!(output["agents"][0]["output"], "retained prior result");
    assert_eq!(output["agents"][1]["status"], "cancelled");
    healthy.finish(completion("unrelated sibling completed"));
    wait_for_state(
        &handle,
        &db,
        "SELECT EXISTS(SELECT 1 FROM agents a JOIN threads t ON t.id=a.thread WHERE t.name='healthy sibling' AND a.state='completed')",
    )
    .await;
    parent.finish(completion("waiting for sibling report"));
    let parent = requests.next("inline-root-fixture").await;
    assert!(parent.contains("unrelated sibling completed"));
    parent.finish(completion("collected rejection and healthy sibling"));
    wait_for_state(
        &handle,
        &db,
        "SELECT EXISTS(SELECT 1 FROM runs r JOIN threads t ON t.id=r.thread WHERE t.parent_run IS NULL AND r.state='completed')",
    )
    .await;
    assert_eq!(committed_call(&db, &op), frozen);
    assert!(handle
        .view(Some(root.thread), None, 100)
        .await
        .unwrap()
        .error
        .is_none());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn inline_workflow_policy_revocation_drains_without_stranding_parent() {
    reject_inline_workflow("policy").await;
}

#[tokio::test]
async fn inline_workflow_permission_revocation_drains_without_stranding_parent() {
    reject_inline_workflow("permission").await;
}

#[tokio::test]
async fn inline_workflow_renewed_approval_denial_drains_without_stranding_parent() {
    reject_inline_workflow("approval").await;
}

fn approved_yield(
    store: &mut Store,
    root: &Receipt,
    models: &[ModelInfo],
    decision: RuleDecision,
) -> (OperationId, ThreadId) {
    store
        .command(
            &Command::SetPermissions {
                id: CommandId::new(),
                session: root.session.clone(),
                expected_revision: 0,
                permissions: PermissionPolicy {
                    rules: vec![PermissionRule {
                        tool: "workflow".into(),
                        decision,
                    }],
                    ..Default::default()
                },
            },
            models,
        )
        .unwrap();
    let input = pipeline(false);
    let (run, op, _) = operation(store, root, "workflow", input.clone());
    store
        .db
        .execute("UPDATE operations SET policy_revision=1 WHERE id=?1", [&op])
        .unwrap();
    store.db.execute("INSERT INTO decisions(operation,revision,preview,state) VALUES(?1,1,'workflow','approved')", [&op]).unwrap();
    assert!(workflow_tick(store, root, &run, &op, models, &input).is_none());
    yielded(store, root, &run, &op);
    let child = children(store, &op)[0].clone();
    (op, child)
}

#[test]
fn yielded_workflow_rechecks_denials_and_source_approval_revision_before_admission() {
    for case in [
        "policy_deny",
        "permission_deny",
        "stale_session",
        "stale_source",
        "missing",
        "revoked",
    ] {
        let (_dir, mut store, root, models) = store::tests::fixture();
        let (op, first) = approved_yield(&mut store, &root, &models, RuleDecision::RequireApproval);
        let frozen = committed_call(&store.db, &op);
        match case {
            "policy_deny" => {
                let mut policy = Policy {
                    yolo: true,
                    ..Default::default()
                };
                policy.denied_tools.insert("workflow".into());
                store
                    .db
                    .execute(
                        "UPDATE sessions SET policy=?1 WHERE id=?2",
                        params![encode(&policy).unwrap(), root.session],
                    )
                    .unwrap();
            }
            "permission_deny" => {
                let permissions = PermissionPolicy {
                    rules: vec![PermissionRule {
                        tool: "workflow".into(),
                        decision: RuleDecision::Deny,
                    }],
                    ..Default::default()
                };
                store
                    .command(
                        &Command::SetPermissions {
                            id: CommandId::new(),
                            session: root.session.clone(),
                            expected_revision: 1,
                            permissions,
                        },
                        &models,
                    )
                    .unwrap();
            }
            "stale_session" => {
                store
                    .db
                    .execute(
                        "UPDATE sessions SET revision=revision+1 WHERE id=?1",
                        [&root.session],
                    )
                    .unwrap();
            }
            "stale_source" => {
                store
                    .db
                    .execute("UPDATE operations SET policy_revision=0 WHERE id=?1", [&op])
                    .unwrap();
            }
            "missing" => {
                store
                    .db
                    .execute("DELETE FROM decisions WHERE operation=?1", [&op])
                    .unwrap();
            }
            "revoked" => {
                store
                    .db
                    .execute(
                        "UPDATE decisions SET state='denied' WHERE operation=?1",
                        [&op],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let other_run = RunId(
            store
                .db
                .query_row("SELECT run FROM operations WHERE id=?1", [&op], |r| {
                    r.get(0)
                })
                .unwrap(),
        );
        let other_op = sibling_operation(&store, &root, &other_run, "spawn_agent", &json!({}));
        let unrelated = launch_child(
            &mut store,
            &root,
            &other_run,
            &other_op,
            &models,
            "unrelated",
        );
        child_done(
            &mut store,
            &unrelated.thread,
            "completed",
            "unrelated retained result",
        );
        let unrelated_outcome = agent_outcome(&store.db, &unrelated.thread);
        child_done(
            &mut store,
            &first,
            "completed",
            r#"{"prompt":"must not admit"}"#,
        );
        let retained = agent_outcome(&store.db, &first);
        cleanup(&mut store, &root, &models);
        let output = workflow_output(&store.db, &op);
        assert_eq!(output["status"], "failed", "{case}");
        assert!(
            output["error"]
                .as_str()
                .unwrap()
                .contains(if case.ends_with("deny") {
                    "denied by current policy"
                } else {
                    "previous approval is missing or stale"
                }),
            "{case}: {output}"
        );
        assert_eq!(children(&store, &op), vec![first.clone()], "{case}");
        assert_eq!(node_count(&store.db, &op), 2, "{case}");
        assert_eq!(agent_outcome(&store.db, &first), retained);
        assert_eq!(
            full_artifact(&store.db, &retained.1).unwrap(),
            r#"{"prompt":"must not admit"}"#
        );
        assert_eq!(
            agent_outcome(&store.db, &unrelated.thread),
            unrelated_outcome
        );
        assert_eq!(committed_call(&store.db, &op), frozen);
        assert_eq!(
            store
                .db
                .query_row("SELECT state FROM jobs WHERE operation=?1", [&op], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap(),
            "failed"
        );
        let count = artifact_count(&store.db);
        cleanup(&mut store, &root, &models);
        assert_eq!(artifact_count(&store.db), count);
    }
}

#[test]
fn yielded_workflow_keeps_current_approval_and_inline_typed_dependencies() {
    for decision in [RuleDecision::Ask, RuleDecision::RequireApproval] {
        let (_dir, mut store, root, models) = store::tests::fixture();
        let (op, first) = approved_yield(&mut store, &root, &models, decision);
        let frozen = committed_call(&store.db, &op);
        cleanup(&mut store, &root, &models);
        assert_eq!(children(&store, &op), vec![first.clone()]);
        child_done(
            &mut store,
            &first,
            "completed",
            r#"{"prompt":"dependent work"}"#,
        );
        cleanup(&mut store, &root, &models);
        let ids = children(&store, &op);
        assert_eq!(ids.len(), 2);
        assert_eq!(node_count(&store.db, &op), 2);
        let first_result = ArtifactId(
            store
                .db
                .query_row(
                    "SELECT result FROM workflow_nodes WHERE operation=?1 AND path='root/first'",
                    [&op],
                    |r| r.get(0),
                )
                .unwrap(),
        );
        let typed: Value = decode(full_artifact(&store.db, &first_result).unwrap()).unwrap();
        assert_eq!(typed["output"], json!({"prompt":"dependent work"}));
        child_done(&mut store, &ids[1], "completed", "dependent finished");
        cleanup(&mut store, &root, &models);
        let output = workflow_output(&store.db, &op);
        assert_eq!(output["status"], "success");
        assert_eq!(output["output"]["second"], "dependent finished");
        assert_eq!(committed_call(&store.db, &op), frozen);
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT COUNT(*) FROM coordination WHERE kind='completion'",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            1
        );
    }
}
