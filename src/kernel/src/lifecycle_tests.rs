use super::*;
use crate::presentation::ThreadActivity;
use std::collections::VecDeque;

#[path = "reply_wait_tests.rs"]
mod reply_wait_tests;

#[derive(Debug)]
struct Request {
    spec: RequestSpec,
    reply: oneshot::Sender<anyhow::Result<Completion>>,
}
impl Request {
    fn finish(self, completion: Completion) {
        self.reply.send(Ok(completion)).unwrap();
    }
    fn contains(&self, text: &str) -> bool {
        self.spec
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| match block {
                ContentBlock::Text { text: value } => value.contains(text),
                ContentBlock::ToolResult { content, .. } => content.model_text().contains(text),
                _ => false,
            })
    }
}
struct Gated {
    models: Vec<ModelInfo>,
    requests: mpsc::UnboundedSender<Request>,
}
#[async_trait]
impl Executor for Gated {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![]
    }
    async fn generate(
        &self,
        spec: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        let (reply, receive) = oneshot::channel();
        self.requests.send(Request { spec, reply }).unwrap();
        tokio::select! {
            result = receive => result?,
            _ = crate::model::cancelled(&mut cancel) => anyhow::bail!("cancelled fixture request"),
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
struct Requests {
    receive: mpsc::UnboundedReceiver<Request>,
    pending: VecDeque<Request>,
}
impl Requests {
    async fn next(&mut self, task: &str) -> Request {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(index) = self.pending.iter().position(|request| {
                    request
                        .spec
                        .messages
                        .first()
                        .is_some_and(|message| message.text().contains(task))
                }) {
                    return self.pending.remove(index).unwrap();
                }
                self.pending.push_back(
                    self.receive
                        .recv()
                        .await
                        .expect("model request channel closed"),
                );
            }
        })
        .await
        .expect("expected model request")
    }
}
async fn gated(models: Vec<ModelInfo>, path: &Path) -> (Handle, Requests) {
    let (send, receive) = mpsc::unbounded_channel();
    let handle = Runtime::open_paused(
        path,
        Arc::new(Gated {
            models,
            requests: send,
        }),
    )
    .await
    .unwrap();
    (
        handle,
        Requests {
            receive,
            pending: VecDeque::new(),
        },
    )
}
async fn begin(handle: &Handle, root: &Receipt) {
    handle
        .activate_session(
            root.session.clone(),
            vec![Command::Submit {
                id: CommandId::new(),
                thread: root.thread.clone(),
                text: "root-owned-fixture".into(),
            }],
        )
        .await
        .unwrap();
}
async fn joining(handle: &Handle, root: &ThreadId, name: &str) -> View {
    wait_view(handle, root, |view| {
        view.threads
            .iter()
            .any(|thread| thread.name == name && thread.activity == Some(ThreadActivity::Agent))
    })
    .await
}
fn launch(name: &str, task: &str) -> Completion {
    tool_call("spawn_agent", json!({"name":name,"task":task}))
}
fn readonly_db(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

#[tokio::test]
async fn root_parent_grandchild_join_collects_once_with_same_runs() {
    let (dir, store, root, models) = fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let (handle, mut requests) = gated(models, &path).await;
    begin(&handle, &root).await;
    let root_request = requests.next("root-owned-fixture").await;
    assert!(root_request
        .spec
        .system
        .contains(&format!("Current thread: {}", root.thread)));
    assert!(root_request
        .spec
        .system
        .contains("Parent thread: none (you are root)"));
    root_request.finish(launch("parent", "parent-owned-fixture"));
    let parent = requests.next("parent-owned-fixture").await;
    requests
        .next("root-owned-fixture")
        .await
        .finish(answer("early root candidate"));
    let first = wait_view(&handle, &root.thread, |view| {
        view.threads.iter().any(|thread| {
            thread.id == root.thread && thread.activity == Some(ThreadActivity::Agent)
        })
    })
    .await;
    let root_run = first
        .threads
        .iter()
        .find(|thread| thread.id == root.thread)
        .unwrap()
        .run
        .clone();
    let parent_id = first
        .threads
        .iter()
        .find(|thread| thread.name == "parent")
        .unwrap()
        .id
        .clone();
    assert!(parent
        .spec
        .system
        .contains(&format!("Current thread: {parent_id}")));
    assert!(parent
        .spec
        .system
        .contains(&format!("Parent thread: {}", root.thread)));
    parent.finish(launch("grandchild", "grandchild-owned-fixture"));
    let grandchild = requests.next("grandchild-owned-fixture").await;
    assert!(grandchild
        .spec
        .system
        .contains(&format!("Parent thread: {parent_id}")));
    assert!(grandchild
        .spec
        .system
        .contains(&format!("Root thread: {}", root.thread)));
    requests
        .next("parent-owned-fixture")
        .await
        .finish(answer("early parent candidate"));
    let view = joining(&handle, &root.thread, "parent").await;
    assert!(view
        .threads
        .iter()
        .filter(|thread| thread.name == "parent" || thread.id == root.thread)
        .all(|thread| thread.state == ThreadState::Active));
    grandchild.finish(answer("grandchild retained result"));
    let parent = requests.next("parent-owned-fixture").await;
    assert!(parent.contains("grandchild retained result"));
    parent.finish(answer("parent synthesized grandchild retained result"));
    let root_request = requests.next("root-owned-fixture").await;
    assert!(root_request.contains("parent synthesized grandchild retained result"));
    root_request.finish(answer("root synthesized both reports"));
    let settled = wait_view(&handle, &root.thread, |view| {
        view.threads
            .iter()
            .all(|thread| thread.state == ThreadState::Completed)
    })
    .await;
    assert_eq!(
        settled
            .threads
            .iter()
            .find(|thread| thread.id == root.thread)
            .unwrap()
            .run,
        root_run
    );
    handle.shutdown().await.unwrap();
    let db = readonly_db(&path);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get::<_, usize>(0))
            .unwrap(),
        3
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM coordination WHERE kind='completion'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM submissions WHERE source='coordination' AND state='queued'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn guidance_wakes_join_and_returns_to_child_without_new_budget() {
    let (dir, store, root, models) = fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let (handle, mut requests) = gated(models, &path).await;
    begin(&handle, &root).await;
    requests
        .next("root-owned-fixture")
        .await
        .finish(launch("child", "guidance-child-fixture"));
    let child = requests.next("guidance-child-fixture").await;
    requests
        .next("root-owned-fixture")
        .await
        .finish(answer("root early final"));
    wait_view(&handle, &root.thread, |view| {
        view.threads.iter().any(|thread| {
            thread.id == root.thread && thread.activity == Some(ThreadActivity::Agent)
        })
    })
    .await;
    child.finish(tool_call(
        "send_message",
        json!({"to":root.thread,"message":"need-guidance-fixture"}),
    ));
    let root_request = requests.next("root-owned-fixture").await;
    assert!(root_request.contains("need-guidance-fixture"));
    root_request.finish(tool_call(
        "send_message",
        json!({"to":"child","message":"use-safe-option-fixture"}),
    ));
    let root_request = requests.next("root-owned-fixture").await;
    root_request.finish(answer("root still joining"));
    let child = requests.next("guidance-child-fixture").await;
    child.finish(tool_call("list_agents", json!({})));
    let child = requests.next("guidance-child-fixture").await;
    assert!(child.contains("use-safe-option-fixture"));
    child.finish(answer("guided child result"));
    let root_request = requests.next("root-owned-fixture").await;
    assert!(root_request.contains("guided child result"));
    root_request.finish(answer("collected guided result"));
    wait_view(&handle, &root.thread, |view| {
        view.threads
            .iter()
            .all(|thread| thread.state == ThreadState::Completed)
    })
    .await;
    handle.shutdown().await.unwrap();
    let db = readonly_db(&path);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get::<_, usize>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM coordination WHERE kind='message'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        2
    );
}

#[ignore = "request budget removed; turns are no longer stopped by max_requests"]
#[tokio::test]
async fn failure_and_budget_exhaustion_cancel_live_descendants_without_retry() {
    for budget in [false, true] {
        let (dir, store, root, models) = fixture();
        if budget {
            store
                .db
                .execute(
                    "UPDATE threads SET profile=json_set(profile,'$.max_requests',1) WHERE id=?1",
                    [&root.thread],
                )
                .unwrap();
        }
        drop(store);
        let path = dir.0.join("lab.db");
        let (handle, mut requests) = gated(models, &path).await;
        begin(&handle, &root).await;
        requests
            .next("root-owned-fixture")
            .await
            .finish(launch("child", "cleanup-child-fixture"));
        let child = if !budget {
            let child = requests.next("cleanup-child-fixture").await;
            requests
                .next("root-owned-fixture")
                .await
                .reply
                .send(Err(anyhow::anyhow!("fatal parent failure")))
                .unwrap();
            Some(child)
        } else {
            None
        };
        wait_view(&handle, &root.thread, |view| {
            view.threads
                .iter()
                .any(|thread| thread.id == root.thread && thread.state == ThreadState::Failed)
                && view.threads.iter().all(|thread| {
                    !matches!(thread.state, ThreadState::Active | ThreadState::Stopping)
                })
        })
        .await;
        drop(child);
        handle.shutdown().await.unwrap();
        let db = readonly_db(&path);
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM runs WHERE state IN ('active','interrupted','stopping')",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM agents WHERE state IN ('queued','running')",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM runs WHERE thread=?1",
                [&root.thread],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            1
        );
    }
}

#[tokio::test]
async fn near_limit_base_instructions_remain_delegatable_without_profile_inflation() {
    let (dir, store, root, models) = fixture();
    let base = "b".repeat(65536);
    store
        .db
        .execute(
            "UPDATE threads SET profile=json_set(profile,'$.instructions',?2) WHERE id=?1",
            params![root.thread, base],
        )
        .unwrap();
    drop(store);
    let path = dir.0.join("lab.db");
    let (handle, mut requests) = gated(models, &path).await;
    begin(&handle, &root).await;
    requests.next("root-owned-fixture").await.finish(tool_call(
        "spawn_agent",
        json!({"name":"reviewer","task":"near-limit-child-fixture","subagent_type":"reviewer"}),
    ));
    let child = requests.next("near-limit-child-fixture").await;
    assert!(child.spec.system.contains(&base));
    assert!(child.spec.system.contains("# Role: collaborator"));
    requests
        .next("root-owned-fixture")
        .await
        .finish(answer("joining reviewer"));
    child.finish(answer("review retained"));
    let root_request = requests.next("root-owned-fixture").await;
    assert!(root_request.contains("review retained"));
    root_request.finish(answer("review collected"));
    wait_view(&handle, &root.thread, |view| {
        view.threads
            .iter()
            .all(|thread| thread.state == ThreadState::Completed)
    })
    .await;
    handle.shutdown().await.unwrap();
    let db = readonly_db(&path);
    let (instructions, read_only, subtype): (String,bool,String) = db.query_row("SELECT json_extract(t.profile,'$.instructions'),json_extract(t.profile,'$.read_only'),a.subagent_type FROM agents a JOIN threads t ON t.id=a.thread", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(instructions, base);
    assert!(read_only);
    assert_eq!(subtype, "reviewer");
}

#[tokio::test]
async fn task_and_typed_workflow_yield_for_guidance_then_collect_one_aggregate() {
    for workflow in [false, true] {
        let (dir, store, root, models) = fixture();
        drop(store);
        let path = dir.0.join("lab.db");
        let (handle, mut requests) = gated(models, &path).await;
        begin(&handle, &root).await;
        let name = if workflow { "workflow" } else { "task" };
        let input = if workflow {
            json!({"steps":[{"id":"first","agent":{"prompt":"group-first-fixture","schema":{"type":"object","properties":{"ready":{"type":"boolean"}},"required":["ready"]}}},{"id":"second","agent":{"prompt":"group-second-fixture {$first}"}}]})
        } else {
            json!({"tasks":[{"description":"group-worker","prompt":"group-first-fixture"}]})
        };
        requests
            .next("root-owned-fixture")
            .await
            .finish(tool_call(name, input));
        let child_request = requests.next("group-first-fixture").await;
        let view = handle
            .view(Some(root.thread.clone()), None, 100)
            .await
            .unwrap();
        let child = view
            .threads
            .iter()
            .find(|thread| thread.id != root.thread)
            .unwrap()
            .id
            .clone();
        assert!(
            view.threads
                .iter()
                .find(|thread| thread.id == root.thread)
                .unwrap()
                .state
                == ThreadState::Active
        );
        child_request.finish(tool_call(
            "send_message",
            json!({"to":root.thread,"message":"group-guidance-needed"}),
        ));
        let root_request = requests.next("root-owned-fixture").await;
        assert!(root_request.contains("group-guidance-needed"));
        assert!(root_request.contains("Collection yielded"));
        root_request.finish(tool_call(
            "send_message",
            json!({"to":child,"message":"group-guidance-answer"}),
        ));
        requests
            .next("root-owned-fixture")
            .await
            .finish(answer("waiting for aggregate"));
        requests
            .next("group-first-fixture")
            .await
            .finish(tool_call("list_agents", json!({})));
        let child_request = requests.next("group-first-fixture").await;
        assert!(child_request.contains("group-guidance-answer"));
        child_request.finish(answer(r#"{"ready":true}"#));
        if workflow {
            let second = requests.next("group-second-fixture").await;
            second.finish(answer("dependent node finished"));
        }
        let root_request = requests.next("root-owned-fixture").await;
        assert!(root_request.contains(if workflow {
            "dependent node finished"
        } else {
            "ready"
        }));
        root_request.finish(answer("aggregate collected"));
        wait_view(&handle, &root.thread, |view| {
            view.threads
                .iter()
                .all(|thread| thread.state == ThreadState::Completed)
        })
        .await;
        handle.shutdown().await.unwrap();
        let db = readonly_db(&path);
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM coordination WHERE kind='completion'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(db.query_row("SELECT COUNT(*) FROM entries e JOIN operations o ON o.id=e.operation WHERE o.name=?1 AND e.kind IN ('tool_result','tool_error')", [name], |r| r.get::<_,usize>(0)).unwrap(), 1);
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM jobs WHERE kind=?1 AND state='completed'",
                [name],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            1
        );
    }
}

fn seed_child(store: &mut Store, root: &Receipt, models: &[ModelInfo]) -> (RunId, ThreadId) {
    let run = insert_run(store, &root.thread);
    let operation = insert_tool(store, &root.thread, &run, "spawn_agent", json!({}));
    let profile = crate::sessions::session(&store.db, &root.session)
        .unwrap()
        .profile;
    let tx = store.db.transaction().unwrap();
    let child = crate::delegation::launch(
        &tx,
        &operation,
        &run,
        &root.thread,
        &profile,
        models,
        crate::delegation::Child {
            name: "child",
            prompt: "must-not-start",
            role: "collaborator",
            subagent_type: None,
            model: None,
            read_only: false,
            blocking: false,
            schema: None,
        },
    )
    .unwrap();
    tx.execute(
        "UPDATE operations SET state='done' WHERE id=?1",
        [&operation],
    )
    .unwrap();
    tx.commit().unwrap();
    (run, child)
}

#[tokio::test]
async fn stop_before_start_wait_returns_cancelled_result() {
    let (dir, mut store, root, models) = fixture();
    let (run, child) = seed_child(&mut store, &root, &models);
    store
        .command(
            &Command::StopThread {
                id: CommandId::new(),
                thread: child.clone(),
            },
            &models,
        )
        .unwrap();
    insert_tool(
        &store,
        &root.thread,
        &run,
        "wait_agent",
        json!({"thread_id":child}),
    );
    drop(store);
    let path = dir.0.join("lab.db");
    let (handle, mut requests) = gated(models, &path).await;
    handle
        .activate_session(root.session.clone(), vec![resume(&root.thread)])
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(5), requests.receive.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(request.contains("cancelled"));
    request.finish(answer("collected cancellation"));
    wait_view(&handle, &root.thread, |view| {
        view.threads
            .iter()
            .any(|thread| thread.id == root.thread && thread.state == ThreadState::Completed)
    })
    .await;
    handle.shutdown().await.unwrap();
    let db = readonly_db(&path);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM runs WHERE thread=?1", [&child], |r| r
            .get::<_, usize>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn reopened_join_holds_unknown_child_without_relaunch_or_false_success() {
    let (dir, mut store, root, models) = fixture();
    let (run, child) = seed_child(&mut store, &root, &models);
    store.db.execute("INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,'model','model','{}','waiting')", params![OperationId::new(),run,root.thread]).unwrap();
    drop(store);
    let path = dir.0.join("lab.db");
    let (handle, requests) = gated(models, &path).await;
    handle
        .activate_session(root.session.clone(), vec![resume(&root.thread)])
        .await
        .unwrap();
    wait_view(&handle, &root.thread, |view| {
        view.threads
            .iter()
            .any(|thread| thread.id == root.thread && thread.state == ThreadState::Interrupted)
    })
    .await;
    assert!(requests.receive.is_empty());
    handle
        .command(Command::StopThread {
            id: CommandId::new(),
            thread: root.thread.clone(),
        })
        .await
        .unwrap();
    wait_view(&handle, &root.thread, |view| {
        view.threads
            .iter()
            .any(|thread| thread.id == root.thread && thread.state == ThreadState::Cancelled)
    })
    .await;
    handle.shutdown().await.unwrap();
    let db = readonly_db(&path);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM runs WHERE thread=?1", [&child], |r| r
            .get::<_, usize>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM coordination WHERE kind='completion'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn final_boundary_checks_accepted_messages_and_late_reports_do_not_reopen() {
    let (_dir, mut store, root, models) = fixture();
    let (run, child) = seed_child(&mut store, &root, &models);
    let tx = store.db.transaction().unwrap();
    assert!(settle_run(&tx, &run, "completed", "too early").is_err());
    crate::delegation::settle(&tx, &child, "completed", "child retained output").unwrap();
    crate::lifecycle::publish(&tx, &child).unwrap();
    assert!(settle_run(&tx, &run, "completed", "uncollected").is_err());
    let message = CommandId(
        tx.query_row(
            "SELECT id FROM submissions WHERE owner_run=?1 AND state='queued'",
            [&run],
            |r| r.get(0),
        )
        .unwrap(),
    );
    consume(&tx, &message, &run).unwrap();
    settle_run(&tx, &run, "completed", "collected").unwrap();
    crate::lifecycle::publish(&tx, &child).unwrap();
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM coordination WHERE kind='completion'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM submissions WHERE owner_run=?1 AND state='queued'",
            [&run],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        0
    );
    tx.commit().unwrap();
}
