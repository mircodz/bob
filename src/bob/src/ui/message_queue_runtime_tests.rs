use super::*;
use bob_kernel::SubmissionState;
use std::sync::Mutex;
use tokio::sync::Semaphore;

#[path = "message_queue_recall_tests.rs"]
mod message_queue_recall_tests;

struct QueueExecutor {
    requests: Mutex<Vec<RequestSpec>>,
    release: Semaphore,
}

#[async_trait]
impl Executor for QueueExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "fixture".into(),
            context_window: 100_000,
        }]
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        request: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _progress: bob_kernel::ModelProgress,
    ) -> Result<Completion> {
        self.requests.lock().unwrap().push(request);
        tokio::select! {
            permit = self.release.acquire() => permit.unwrap().forget(),
            _ = cancelled(&mut cancel) => bail!("cancelled fixture"),
        }
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "finished".into(),
                }],
            },
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }
    async fn prepare(&self, _invocation: Invocation) -> Result<PreparedTool> {
        unreachable!()
    }
    async fn execute(
        &self,
        _tool: PreparedTool,
        _cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        unreachable!()
    }
}

#[tokio::test]
async fn persisted_queue_recall_consumption_and_reopen_render_once_in_either_reply_order() {
    let workspace = Workspace::new();
    let path = workspace.0.join("queue.sqlite");
    let executor = Arc::new(QueueExecutor {
        requests: Mutex::new(Vec::new()),
        release: Semaphore::new(0),
    });
    let handle = Runtime::open(&path, executor.clone()).await.unwrap();
    let receipt = handle
        .command(Command::CreateSession {
            id: CommandId::new(),
            workspace: workspace.0.to_string_lossy().into_owned(),
            profile: Profile {
                model: "fixture".into(),
                instructions: String::new(),
                read_only: false,
                max_requests: 8,
                reasoning: ReasoningEffort::Off,
            },
            policy: Policy::default(),
        })
        .await
        .unwrap();
    let thread = receipt.thread;
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: thread.clone(),
            text: "starter".into(),
        })
        .await
        .unwrap();
    until(&handle, &thread, |_| {
        executor.requests.lock().unwrap().len() == 1
    })
    .await;
    let delivered = CommandId::new();
    let recalled = CommandId::new();
    for id in [&delivered, &recalled] {
        handle
            .command(Command::Submit {
                id: id.clone(),
                thread: thread.clone(),
                text: "identical queued input".into(),
            })
            .await
            .unwrap();
    }
    let pending_view = handle
        .transcript(Some(thread.clone()), None, 100)
        .await
        .unwrap();
    let pending_queue = handle
        .queue_snapshot(thread.clone(), vec![delivered.clone(), recalled.clone()])
        .await
        .unwrap();
    assert_eq!(pending_queue.pending.len(), 2);
    assert!(pending_view
        .entries
        .iter()
        .filter_map(|entry| entry.submission.as_ref())
        .filter(|submission| submission.id == delivered || submission.id == recalled)
        .all(|submission| submission.state == SubmissionState::Pending));
    handle
        .command(Command::RecallSubmission {
            id: CommandId::new(),
            submission: recalled.clone(),
        })
        .await
        .unwrap();
    let recall_queue = handle
        .queue_snapshot(thread.clone(), vec![delivered.clone(), recalled.clone()])
        .await
        .unwrap();
    assert_eq!(recall_queue.pending.len(), 1);
    assert_eq!(
        recall_queue
            .submissions
            .iter()
            .find(|submission| submission.id == recalled)
            .unwrap()
            .state,
        SubmissionState::Recalled
    );
    executor.release.add_permits(1);
    until(&handle, &thread, |_| {
        executor.requests.lock().unwrap().len() == 2
    })
    .await;
    let consumed_view = handle
        .transcript(Some(thread.clone()), None, 100)
        .await
        .unwrap();
    let consumed_queue = handle
        .queue_snapshot(thread.clone(), vec![delivered.clone(), recalled.clone()])
        .await
        .unwrap();
    assert!(consumed_queue.pending.is_empty());
    assert_eq!(
        consumed_queue
            .submissions
            .iter()
            .find(|submission| submission.id == delivered)
            .unwrap()
            .state,
        SubmissionState::Consumed
    );
    {
        let requests = executor.requests.lock().unwrap();
        let messages = &requests[1].messages;
        let occurrences = messages.iter().flat_map(|message| &message.content).filter(|block| matches!(block, ContentBlock::Text { text } if text == "identical queued input")).count();
        assert_eq!(
            occurrences, 1,
            "queued input must be delivered once, not resubmitted by projection"
        );
    }
    for metadata_first in [true, false] {
        let mut app = App::new(80);
        app.reply(Reply::Ready(Ok((
            handle.clone(),
            pending_view.clone(),
            thread.clone(),
        ))));
        app.pane_mut().unwrap().accept_queue(pending_queue.clone());
        assert_eq!(
            render(&mut app).1.matches("identical queued input").count(),
            2
        );
        app.pane_mut().unwrap().accept_queue(recall_queue.clone());
        assert_eq!(
            render(&mut app).1.matches("identical queued input").count(),
            1
        );
        if metadata_first {
            app.pane_mut().unwrap().accept_queue(consumed_queue.clone());
        }
        app.reply(Reply::View {
            thread: thread.clone(),
            generation: app.pane().unwrap().generation,
            result: Ok(consumed_view.clone()),
        });
        if !metadata_first {
            app.pane_mut().unwrap().accept_queue(consumed_queue.clone());
        }
        assert_eq!(
            render(&mut app).1.matches("identical queued input").count(),
            1
        );
        let builds = app.pane().unwrap().render_builds;
        app.pane_mut().unwrap().accept_queue(pending_queue.clone());
        assert_eq!(
            render(&mut app).1.matches("identical queued input").count(),
            1
        );
        assert_eq!(app.pane().unwrap().render_builds, builds);
        assert!(app.pane().unwrap().visible_queue().is_empty());
    }
    let mut partial = consumed_view.clone();
    partial.entries.retain(|entry| {
        entry
            .submission
            .as_ref()
            .is_none_or(|submission| submission.id != delivered && submission.id != recalled)
    });
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), partial, thread.clone()))));
    let mut jobs = JoinSet::new();
    app.refresh(&mut jobs);
    while !jobs.is_empty() {
        collect(&mut app, &mut jobs).await;
    }
    let old_entry = pending_view
        .entries
        .iter()
        .find(|entry| {
            entry
                .submission
                .as_ref()
                .is_some_and(|submission| submission.id == delivered)
        })
        .unwrap()
        .clone();
    let pane = app.pane_mut().unwrap();
    pane.view.as_mut().unwrap().entries.push(old_entry);
    pane.view
        .as_mut()
        .unwrap()
        .entries
        .sort_by_key(|entry| entry.sequence);
    pane.reconcile_submissions();
    assert!(pane.visible_queue().is_empty());
    app.refresh(&mut jobs);
    assert!(
        !jobs.is_empty(),
        "newly tracked IDs must refresh metadata even at the same changes revision"
    );
    while !jobs.is_empty() {
        collect(&mut app, &mut jobs).await;
    }
    assert_eq!(
        app.pane().unwrap().submissions[&delivered].state,
        SubmissionState::Consumed
    );

    executor.release.add_permits(1);
    until(&handle, &thread, |view| {
        view.threads
            .iter()
            .any(|item| item.id == thread && item.state == ThreadState::Completed)
    })
    .await;
    handle.shutdown().await.unwrap();
    let reopened = Runtime::open(&path, executor.clone()).await.unwrap();
    let view = reopened
        .transcript(Some(thread.clone()), None, 100)
        .await
        .unwrap();
    let queue = reopened
        .queue_snapshot(thread.clone(), vec![delivered.clone(), recalled.clone()])
        .await
        .unwrap();
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((reopened.clone(), view, thread))));
    app.pane_mut().unwrap().accept_queue(queue);
    assert_eq!(
        render(&mut app).1.matches("identical queued input").count(),
        1
    );
    assert_eq!(executor.requests.lock().unwrap().len(), 2);
    reopened.shutdown().await.unwrap();
}
