use super::*;
use std::collections::VecDeque;

#[derive(Debug)]
struct ModelGate {
    spec: RequestSpec,
    reply: oneshot::Sender<Completion>,
    cancelled: oneshot::Receiver<()>,
}

impl ModelGate {
    fn finish(self, completion: Completion) {
        self.reply.send(completion).unwrap();
    }

    fn occurrences(&self, needle: &str) -> usize {
        self.spec
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .map(|block| match block {
                ContentBlock::Text { text } => text.matches(needle).count(),
                ContentBlock::ToolResult { content, .. } => {
                    content.model_text().matches(needle).count()
                }
                _ => 0,
            })
            .sum()
    }
}

#[derive(Debug)]
struct ToolGate {
    invocation: Invocation,
    reply: oneshot::Sender<ToolOutcome>,
}

struct RegressionExecutor {
    models: Vec<ModelInfo>,
    requests: mpsc::UnboundedSender<ModelGate>,
    tools: mpsc::UnboundedSender<ToolGate>,
    generated: AtomicUsize,
    prepared: Mutex<Vec<Invocation>>,
    executed: Mutex<Vec<Invocation>>,
}

#[async_trait]
impl Executor for RegressionExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            spec: ToolSpec {
                name: "edit_file".into(),
                description: "Gated fixture mutation".into(),
                input_schema: json!({"type":"object"}),
            },
            effect: EffectClass::Write,
        }]
    }

    async fn generate(
        &self,
        spec: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        self.generated.fetch_add(1, Ordering::SeqCst);
        let (reply, receive) = oneshot::channel();
        let (cancelled, cancellation) = oneshot::channel();
        self.requests
            .send(ModelGate {
                spec,
                reply,
                cancelled: cancellation,
            })
            .unwrap();
        tokio::select! {
            result = receive => Ok(result?),
            _ = crate::model::cancelled(&mut cancel) => {
                let _ = cancelled.send(());
                anyhow::bail!("fixture observed model cancellation")
            }
        }
    }

    async fn prepare(&self, invocation: Invocation) -> anyhow::Result<PreparedTool> {
        self.prepared.lock().unwrap().push(invocation.clone());
        Ok(PreparedTool {
            invocation,
            specification: json!({}),
            preview: "gated mutation".into(),
            presentation: None,
        })
    }

    async fn execute(
        &self,
        prepared: PreparedTool,
        mut cancel: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        self.executed
            .lock()
            .unwrap()
            .push(prepared.invocation.clone());
        let (reply, receive) = oneshot::channel();
        self.tools
            .send(ToolGate {
                invocation: prepared.invocation,
                reply,
            })
            .unwrap();
        tokio::select! {
            result = receive => Ok(result?),
            _ = crate::model::cancelled(&mut cancel) => {
                anyhow::bail!("fixture observed tool cancellation")
            }
        }
    }
}

struct ModelGates {
    receive: mpsc::UnboundedReceiver<ModelGate>,
    pending: VecDeque<ModelGate>,
}

impl ModelGates {
    async fn next(&mut self, task: &str) -> ModelGate {
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
                self.pending
                    .push_back(self.receive.recv().await.expect("expected model request"));
            }
        })
        .await
        .expect("model request checkpoint timed out")
    }

    fn assert_empty(&self) {
        assert!(self.pending.is_empty(), "unexpected buffered model request");
        assert!(self.receive.is_empty(), "unexpected new model request");
    }
}

fn executor(
    models: Vec<ModelInfo>,
) -> (
    Arc<RegressionExecutor>,
    ModelGates,
    mpsc::UnboundedReceiver<ToolGate>,
) {
    let (requests, receive) = mpsc::unbounded_channel();
    let (tools, tool_receive) = mpsc::unbounded_channel();
    (
        Arc::new(RegressionExecutor {
            models,
            requests,
            tools,
            generated: AtomicUsize::new(0),
            prepared: Mutex::new(Vec::new()),
            executed: Mutex::new(Vec::new()),
        }),
        ModelGates {
            receive,
            pending: VecDeque::new(),
        },
        tool_receive,
    )
}

fn allow_writes(store: &mut Store, root: &Receipt, models: &[ModelInfo]) {
    store
        .command(
            &Command::SetPolicy {
                id: CommandId::new(),
                session: root.session.clone(),
                expected_revision: 0,
                policy: Policy {
                    yolo: true,
                    ..Default::default()
                },
            },
            models,
        )
        .unwrap();
}

fn human(thread: &ThreadId, text: &str) -> Command {
    Command::Submit {
        id: CommandId::new(),
        thread: thread.clone(),
        text: text.into(),
    }
}

fn readonly_db(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn thread_state(view: &View, thread: &ThreadId) -> ThreadState {
    view.threads.iter().find(|t| &t.id == thread).unwrap().state
}

#[tokio::test]
async fn same_schedule_batch_stop_prevents_child_prepare_execute_and_model_start() {
    for child_work in ["ready", "pending", "model"] {
        let (_dir, mut store, root, models) = fixture();
        allow_writes(&mut store, &root, &models);
        let parent_run = insert_run(&store, &root.thread);
        let launch = insert_tool(
            &store,
            &root.thread,
            &parent_run,
            "spawn_agent",
            json!({"name":"batch-child","task":"batch-child-task"}),
        );
        let snapshot = crate::sessions::session(&store.db, &root.session).unwrap();
        let tx = store.db.transaction().unwrap();
        let child = crate::delegation::launch(
            &tx,
            &launch,
            &parent_run,
            &root.thread,
            &snapshot.profile,
            &models,
            crate::delegation::Child {
                name: "batch-child",
                prompt: "batch-child-task",
                role: "collaborator",
                subagent_type: None,
                model: None,
                read_only: false,
                blocking: false,
                schema: None,
            },
        )
        .unwrap();
        tx.execute("UPDATE operations SET state='done' WHERE id=?1", [&launch])
            .unwrap();
        tx.commit().unwrap();
        let child_run = insert_run(&store, &child);
        let submission = CommandId(
            store
                .db
                .query_row(
                    "SELECT id FROM submissions WHERE thread=?1 AND state='queued'",
                    [&child],
                    |r| r.get(0),
                )
                .unwrap(),
        );
        consume(&store.db, &submission, &child_run).unwrap();
        store
            .db
            .execute(
                "UPDATE agents SET state='running' WHERE thread=?1",
                [&child],
            )
            .unwrap();
        let stop = insert_tool(
            &store,
            &root.thread,
            &parent_run,
            "stop_agent",
            json!({"name":child}),
        );
        let child_operation = (child_work != "model").then(|| {
            let operation = insert_tool(&store, &child, &child_run, "edit_file", json!({}));
            if child_work == "ready" {
                let prepared = PreparedTool {
                    invocation: Invocation {
                        operation_id: operation.clone(),
                        run_id: child_run.clone(),
                        thread_id: child.clone(),
                        workspace: snapshot.workspace.clone(),
                        name: "edit_file".into(),
                        input: json!({}),
                    },
                    specification: json!({}),
                    preview: "prepared child write must not execute".into(),
                    presentation: None,
                };
                store.db.execute(
                    "UPDATE operations SET state='ready',prepared=?1,effect_write=1,policy_revision=(SELECT revision FROM sessions WHERE id=?2) WHERE id=?3",
                    params![encode(&prepared).unwrap(), root.session, operation],
                ).unwrap();
            }
            operation
        });
        let ordinals: (i64, i64) = store
            .db
            .query_row(
                "SELECT p.ordinal,c.ordinal FROM runs p JOIN runs c ON c.id=?2 WHERE p.id=?1",
                params![parent_run, child_run],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(ordinals.0 < ordinals.1);
        let (executor, requests, tools) = executor(models);
        let mut actor = Actor::new_owned(store, executor.clone()).unwrap();
        let (done, mut finished) = mpsc::channel(16);

        actor.schedule(&done).unwrap();

        assert!(
            actor.workers.is_empty(),
            "{child_work}: stopped child dispatched a worker"
        );
        let view = actor
            .store
            .view(Some(child.clone()), None, 100, None)
            .unwrap();
        assert_eq!(thread_state(&view, &child), ThreadState::Cancelled);
        if let Some(operation) = child_operation {
            assert_eq!(
                operation_state(&view, &operation),
                Some(OperationState::Cancelled)
            );
        }
        let parent = actor
            .store
            .view(Some(root.thread.clone()), None, 100, None)
            .unwrap();
        assert_eq!(operation_state(&parent, &stop), Some(OperationState::Done));
        assert_eq!(
            actor
                .store
                .db
                .query_row(
                    "SELECT COUNT(*) FROM operations WHERE run=?1 AND kind='model'",
                    [&child_run],
                    |r| r.get::<_, usize>(0),
                )
                .unwrap(),
            0
        );
        assert_eq!(executor.generated.load(Ordering::SeqCst), 0);
        assert!(executor.prepared.lock().unwrap().is_empty());
        assert!(executor.executed.lock().unwrap().is_empty());
        requests.assert_empty();
        assert!(tools.is_empty());
        assert!(finished.try_recv().is_err());
    }
}

#[tokio::test]
async fn interrupted_foreground_outcome_drains_owned_child_and_next_message_continues() {
    let (dir, mut store, root, models) = fixture();
    allow_writes(&mut store, &root, &models);
    let snapshot = crate::sessions::session(&store.db, &root.session).unwrap();
    let healthy = store
        .command(
            &Command::CreateSession {
                id: CommandId::new(),
                workspace: snapshot.workspace,
                profile: snapshot.profile,
                policy: Policy::default(),
            },
            &models,
        )
        .unwrap();
    drop(store);
    let path = dir.0.join("lab.db");
    let (executor, mut requests, mut tools) = executor(models);
    let handle = Runtime::open(&path, executor.clone()).await.unwrap();
    handle
        .activate_session(root.session.clone(), Vec::new())
        .await
        .unwrap();
    let healthy_handle = Runtime::open(&path, executor.clone()).await.unwrap();
    healthy_handle
        .activate_session(healthy.session.clone(), Vec::new())
        .await
        .unwrap();
    healthy_handle
        .command(human(&healthy.thread, "healthy-unrelated-root"))
        .await
        .unwrap();
    let healthy_request = requests.next("healthy-unrelated-root").await;
    handle
        .command(human(&root.thread, "interrupted-root-task"))
        .await
        .unwrap();
    requests
        .next("interrupted-root-task")
        .await
        .finish(tool_call(
            "spawn_agent",
            json!({"name":"owned-child","task":"owned-child-await-cancellation"}),
        ));
    let child_request = requests.next("owned-child-await-cancellation").await;
    requests
        .next("interrupted-root-task")
        .await
        .finish(tool_call("edit_file", json!({})));
    let tool = tokio::time::timeout(Duration::from_secs(5), tools.recv())
        .await
        .unwrap()
        .expect("root foreground tool must execute");
    assert_eq!(tool.invocation.thread_id, root.thread);
    let operation = tool.invocation.operation_id;
    let root_run = tool.invocation.run_id;
    tool.reply
        .send(ToolOutcome {
            text: "Fixture cannot establish whether the write completed.".into(),
            is_error: false,
            interrupted: true,
            presentation: None,
        })
        .unwrap();

    tokio::time::timeout(Duration::from_secs(5), child_request.cancelled)
        .await
        .expect("owned child was not cancelled")
        .unwrap();
    let drained = wait_view(&handle, &root.thread, |view| {
        operation_state(view, &operation) == Some(OperationState::Unknown)
            && thread_state(view, &root.thread) == ThreadState::Interrupted
            && view
                .threads
                .iter()
                .any(|t| t.name == "owned-child" && t.state == ThreadState::Cancelled)
    })
    .await;
    assert!(child_request.reply.is_closed());
    let child = drained
        .threads
        .iter()
        .find(|t| t.name == "owned-child")
        .unwrap()
        .id
        .clone();
    healthy_request.finish(answer("unrelated work completed normally"));
    wait_view(&healthy_handle, &healthy.thread, |view| {
        thread_state(view, &healthy.thread) == ThreadState::Completed
    })
    .await;
    // `/resume` alone still refuses an uncertain outcome; a new human message continues.
    assert!(handle.command(resume(&root.thread)).await.is_err());
    assert_eq!(executor.generated.load(Ordering::SeqCst), 4);
    let queued = human(&root.thread, "human-work-after-unknown");
    handle.command(queued.clone()).await.unwrap();
    let next = requests.next("interrupted-root-task").await;
    assert_eq!(next.occurrences("human-work-after-unknown"), 1);
    assert_eq!(
        next.occurrences("Fixture cannot establish whether the write completed."),
        1,
        "the model still sees the uncertain outcome"
    );
    next.finish(answer("new human work completed"));
    let settled = wait_view(&handle, &root.thread, |view| {
        thread_state(view, &root.thread) == ThreadState::Completed
            && view
                .threads
                .iter()
                .find(|t| t.id == root.thread)
                .unwrap()
                .run
                .as_ref()
                != Some(&root_run)
    })
    .await;
    assert_eq!(
        operation_state(&settled, &operation),
        Some(OperationState::Resolved)
    );
    {
        let db = readonly_db(&path);
        assert_eq!(
            db.query_row("SELECT state FROM runs WHERE id=?1", [&root_run], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "cancelled"
        );
        assert!(!db
            .query_row(
                "SELECT held FROM threads WHERE id=?1",
                [&root.thread],
                |r| r.get::<_, bool>(0)
            )
            .unwrap());
        assert_eq!(db.query_row("SELECT COUNT(*) FROM operations WHERE thread=?1 AND state IN ('started','preparing')", [&child], |r| r.get::<_, usize>(0)).unwrap(), 0);
    }
    assert_eq!(executor.prepared.lock().unwrap().len(), 1);
    assert_eq!(executor.executed.lock().unwrap().len(), 1);
    assert_eq!(executor.generated.load(Ordering::SeqCst), 5);
    requests.assert_empty();
    assert!(tools.is_empty());
    healthy_handle.shutdown().await.unwrap();
    handle.shutdown().await.unwrap();
    let db = readonly_db(&path);
    assert_eq!(
        db.query_row(
            "SELECT state FROM operations WHERE id=?1",
            [&operation],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "resolved"
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM runs WHERE thread=?1", [&child], |r| r
            .get::<_, usize>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM operations WHERE thread=?1 AND kind='tool'",
            [&child],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn retained_background_notice_enters_next_human_request_once_without_replaying_tools() {
    let (dir, mut store, root, models) = fixture();
    let old_run = insert_run(&store, &root.thread);
    let original = CommandId::new();
    submit(
        &store.db,
        &original,
        &root.thread,
        "human",
        None,
        "closed-root-seed",
    )
    .unwrap();
    consume(&store.db, &original, &old_run).unwrap();
    let old_tool = insert_tool(&store, &root.thread, &old_run, "edit_file", json!({}));
    let message = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: old_tool.0.clone(),
            name: "edit_file".into(),
            input: json!({}),
        }],
    };
    entry(
        &store.db,
        &root.thread,
        "assistant",
        "previous write",
        EntryData {
            message: Some(&message),
            ..Default::default()
        },
    )
    .unwrap();
    finish_tool_entry(&store.db, &old_tool, "previous write completed", false).unwrap();
    store
        .db
        .execute(
            "UPDATE operations SET state='done' WHERE id=?1",
            [&old_tool],
        )
        .unwrap();
    let tx = store.db.transaction().unwrap();
    settle_run(
        &tx,
        &old_run,
        "completed",
        "root closed before background completion",
    )
    .unwrap();
    tx.commit().unwrap();
    let notice = CommandId::new();
    let notice_text =
        "Shell job fixture-late-job completed. Use job_output to inspect its retained result.";
    // Characterize durable admission after a late notice, not native process timing.
    submit(
        &store.db,
        &notice,
        &root.thread,
        "coordination",
        None,
        notice_text,
    )
    .unwrap();
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT state,owner_run FROM submissions WHERE id=?1",
                [&notice],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .unwrap(),
        ("retained".into(), None)
    );
    drop(store);
    let path = dir.0.join("lab.db");
    let (executor, mut requests, tools) = executor(models);
    let handle = Runtime::open_paused(&path, executor.clone()).await.unwrap();
    handle
        .activate_session(root.session.clone(), vec![])
        .await
        .unwrap();
    for _ in 0..3 {
        let view = handle
            .view(Some(root.thread.clone()), None, 100)
            .await
            .unwrap();
        assert!(view.error.is_none());
        assert_eq!(thread_state(&view, &root.thread), ThreadState::Completed);
        assert_eq!(
            view.threads
                .iter()
                .find(|t| t.id == root.thread)
                .unwrap()
                .run
                .as_ref(),
            Some(&old_run)
        );
    }
    assert_eq!(executor.generated.load(Ordering::SeqCst), 0);
    requests.assert_empty();
    {
        let db = readonly_db(&path);
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM runs WHERE thread=?1",
                [&root.thread],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT state FROM submissions WHERE id=?1",
                [&notice],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "retained"
        );
    }

    let first_human = human(&root.thread, "inspect-the-late-notice");
    handle.command(first_human.clone()).await.unwrap();
    let request = requests.next("closed-root-seed").await;
    assert_eq!(request.occurrences(notice_text), 1);
    assert_eq!(request.occurrences("inspect-the-late-notice"), 1);
    assert!(request.spec.messages.iter().any(|m| {
        m.role == Role::User
            && m.text().contains("not a human instruction")
            && m.text().contains(notice_text)
    }));
    let new_run = {
        let db = readonly_db(&path);
        let (state, owner, run): (String, String, String) = db
            .query_row(
                "SELECT state,owner_run,run FROM submissions WHERE id=?1",
                [&notice],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "in_use");
        assert_eq!(owner, run);
        assert_ne!(run, old_run.0);
        assert_eq!(
            db.query_row(
                "SELECT run FROM submissions WHERE id=?1",
                [first_human.id()],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            run
        );
        RunId(run)
    };
    request.finish(answer("late notice inspected"));
    wait_view(&handle, &root.thread, |view| {
        thread_state(view, &root.thread) == ThreadState::Completed
    })
    .await;
    handle
        .command(human(&root.thread, "subsequent-human-run"))
        .await
        .unwrap();
    let next = requests.next("closed-root-seed").await;
    assert_eq!(
        next.occurrences(notice_text),
        1,
        "retained history must not be delivered twice"
    );
    assert_eq!(next.occurrences("subsequent-human-run"), 1);
    next.finish(answer("subsequent run completed"));
    wait_view(&handle, &root.thread, |view| {
        thread_state(view, &root.thread) == ThreadState::Completed
    })
    .await;
    assert_eq!(executor.generated.load(Ordering::SeqCst), 2);
    assert!(executor.prepared.lock().unwrap().is_empty());
    assert!(executor.executed.lock().unwrap().is_empty());
    requests.assert_empty();
    assert!(tools.is_empty());
    handle.shutdown().await.unwrap();
    let db = readonly_db(&path);
    assert_eq!(
        db.query_row(
            "SELECT state,owner_run,run FROM submissions WHERE id=?1",
            [&notice],
            |r| Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?
            )),
        )
        .unwrap(),
        ("settled".into(), new_run.0.clone(), new_run.0)
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM entries WHERE submission=?1 AND model IS NOT NULL",
            [&notice],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM operations WHERE thread=?1 AND name='edit_file'",
            [&root.thread],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT state FROM operations WHERE id=?1",
            [&old_tool],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "done"
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM runs WHERE thread=?1",
            [&root.thread],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        3
    );
}

fn completed(view: &View, thread: &ThreadId) -> bool {
    thread_state(view, thread) == ThreadState::Completed
}

fn edit_then_answer(request: RequestSpec) -> anyhow::Result<Completion> {
    let has_result = request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
    });
    Ok(if has_result {
        answer("edited")
    } else {
        tool_call("edit_file", json!({}))
    })
}

/// Session be319b3e: an interrupted shell left an `unknown` claim on the workspace, and every
/// later session in that workspace stalled on its first tool call.
#[tokio::test]
async fn uncertain_or_orphaned_effects_of_another_session_do_not_freeze_the_workspace() {
    for stale in ["unknown", "started"] {
        let (dir, mut store, dead, models) = fixture();
        let snapshot = crate::sessions::session(&store.db, &dead.session).unwrap();
        let live = store
            .command(
                &Command::CreateSession {
                    id: CommandId::new(),
                    workspace: snapshot.workspace.clone(),
                    profile: snapshot.profile,
                    policy: Policy {
                        yolo: true,
                        ..Default::default()
                    },
                },
                &models,
            )
            .unwrap();
        let owner =
            crate::SessionClaim::acquire(&dir.0.join("lab.db"), dead.session.clone()).unwrap();
        let old_run = insert_run(&store, &dead.thread);
        let old = insert_tool(&store, &dead.thread, &old_run, "edit_file", json!({}));
        store
            .db
            .execute(
                "UPDATE operations SET state=?2,effect_write=1 WHERE id=?1",
                params![old, stale],
            )
            .unwrap();
        store.db.execute(
            "INSERT INTO effect_claims(operation,resource,session,generation,writer) VALUES(?1,?2,?3,'gone',1)",
            params![old, snapshot.workspace, dead.session],
        ).unwrap();
        drop(owner);
        drop(store);
        let executor = Arc::new(LifecycleExecutor::new(models, edit_then_answer));
        let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
            .await
            .unwrap();
        handle
            .activate_session(live.session.clone(), Vec::new())
            .await
            .unwrap();
        handle
            .command(human(&live.thread, "edit the workspace"))
            .await
            .unwrap();
        wait_view(&handle, &live.thread, |view| completed(view, &live.thread)).await;
        assert_eq!(executor.executed.lock().unwrap().len(), 1, "{stale}");
        handle.shutdown().await.unwrap();
    }
}

/// Session 6baf8401: after Esc the thread stayed held and every new message sat in the queue.
#[tokio::test]
async fn a_new_message_after_stop_starts_a_new_turn() {
    let (dir, store, root, models) = fixture();
    insert_run(&store, &root.thread);
    drop(store);
    let executor = Arc::new(LifecycleExecutor::new(models, |_| Ok(answer("answered"))));
    let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    handle
        .activate_session(
            root.session.clone(),
            vec![Command::StopThread {
                id: CommandId::new(),
                thread: root.thread.clone(),
            }],
        )
        .await
        .unwrap();
    wait_view(&handle, &root.thread, |view| {
        thread_state(view, &root.thread) == ThreadState::Cancelled
    })
    .await;
    handle
        .command(human(&root.thread, "after the stop"))
        .await
        .unwrap();
    wait_view(&handle, &root.thread, |view| completed(view, &root.thread)).await;
    assert_eq!(executor.generated.load(Ordering::SeqCst), 1);
    handle.shutdown().await.unwrap();
}

/// A turn that could not be planned used to halt the kernel for the whole process.
#[tokio::test]
async fn a_turn_that_cannot_be_planned_fails_alone_and_the_kernel_keeps_serving() {
    let (dir, store, root, models) = fixture();
    store
        .db
        .execute(
            "INSERT INTO compactions(thread,state) VALUES(?1,'not a compaction plan')",
            [&root.thread],
        )
        .unwrap();
    drop(store);
    let executor = Arc::new(LifecycleExecutor::new(models, |_| Ok(answer("answered"))));
    let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    handle
        .activate_session(root.session.clone(), Vec::new())
        .await
        .unwrap();
    handle.command(human(&root.thread, "first")).await.unwrap();
    let failed = wait_view(&handle, &root.thread, |view| {
        thread_state(view, &root.thread) == ThreadState::Failed
    })
    .await;
    assert!(failed
        .entries
        .iter()
        .any(|entry| entry.text.starts_with("Turn stopped:")));
    let db = rusqlite::Connection::open(dir.0.join("lab.db")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute("DELETE FROM compactions", []).unwrap();
    drop(db);
    handle.command(human(&root.thread, "second")).await.unwrap();
    wait_view(&handle, &root.thread, |view| completed(view, &root.thread)).await;
    assert_eq!(executor.generated.load(Ordering::SeqCst), 1);
    handle.shutdown().await.unwrap();
}

/// Session 8d37aa65: bob restarted mid-turn, then "continue" was sent eight times and never ran.
#[tokio::test]
async fn continuing_after_a_restart_cancels_unstarted_calls_and_keeps_queued_words() {
    let (dir, store, root, models) = fixture();
    let run = insert_run(&store, &root.thread);
    let pending = insert_tool(&store, &root.thread, &run, "edit_file", json!({}));
    let call = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: pending.0.clone(),
            name: "edit_file".into(),
            input: json!({}),
        }],
    };
    entry(
        &store.db,
        &root.thread,
        "assistant",
        "",
        EntryData {
            message: Some(&call),
            ..Default::default()
        },
    )
    .unwrap();
    submit(
        &store.db,
        &CommandId::new(),
        &root.thread,
        "steering",
        None,
        "typed while it was stuck",
    )
    .unwrap();
    drop(store);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    let executor = Arc::new(LifecycleExecutor::new(models, move |request| {
        requests.lock().unwrap().push(
            request
                .messages
                .last()
                .map(Message::text)
                .unwrap_or_default(),
        );
        Ok(answer("answered"))
    }));
    let handle = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    handle
        .activate_session(root.session.clone(), Vec::new())
        .await
        .unwrap();
    let view = handle
        .view(Some(root.thread.clone()), None, 100)
        .await
        .unwrap();
    assert_eq!(thread_state(&view, &root.thread), ThreadState::Interrupted);
    handle
        .command(human(&root.thread, "continue"))
        .await
        .unwrap();
    let view = wait_view(&handle, &root.thread, |view| {
        completed(view, &root.thread) && seen.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(
        operation_state(&view, &pending),
        Some(OperationState::Cancelled)
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["typed while it was stuck".to_owned(), "continue".to_owned()]
    );
    assert!(executor.executed.lock().unwrap().is_empty());
    assert!(handle.queue(root.thread.clone()).await.unwrap().is_empty());
    handle.shutdown().await.unwrap();
}
