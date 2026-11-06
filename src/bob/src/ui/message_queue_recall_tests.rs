use super::*;

async fn start_queue(workspace: &Workspace) -> (Handle, ThreadId, Arc<QueueExecutor>) {
    let executor = Arc::new(QueueExecutor {
        requests: Mutex::new(Vec::new()),
        release: Semaphore::new(0),
    });
    let handle = Runtime::open(&workspace.0.join("recall.sqlite"), executor.clone())
        .await
        .unwrap();
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
    submit(&handle, &thread, "starter".into()).await;
    until(&handle, &thread, |_| {
        executor.requests.lock().unwrap().len() == 1
    })
    .await;
    (handle, thread, executor)
}

async fn submit(handle: &Handle, thread: &ThreadId, text: String) -> CommandId {
    let id = CommandId::new();
    handle
        .command(Command::Submit {
            id: id.clone(),
            thread: thread.clone(),
            text,
        })
        .await
        .unwrap();
    id
}

async fn completed(handle: &Handle, thread: &ThreadId, executor: &QueueExecutor, requests: usize) {
    until(handle, thread, |view| {
        executor.requests.lock().unwrap().len() == requests
            && view
                .threads
                .iter()
                .any(|item| item.id == *thread && item.state == ThreadState::Completed)
    })
    .await;
}

async fn keyboard_recall(pending_count: usize, queue_metadata: bool) {
    let workspace = Workspace::new();
    let (handle, thread, executor) = start_queue(&workspace).await;
    let mut newest_text = format!(
        "newest queued input {} complete tail",
        "界e\u{301}".repeat(3500)
    );
    if !queue_metadata {
        newest_text.push_str(&"x".repeat(composer::INPUT_LIMIT - newest_text.len()));
        assert_eq!(newest_text.len(), composer::INPUT_LIMIT);
    }
    assert!(newest_text.len() > 16384);
    let mut ids = Vec::new();
    let mut texts = Vec::new();
    for index in 0..pending_count {
        let text = if index + 1 == pending_count {
            newest_text.clone()
        } else {
            format!("queued input {index:03}")
        };
        ids.push(submit(&handle, &thread, text.clone()).await);
        texts.push(text);
    }
    let newest = ids.last().unwrap().clone();
    let view = handle
        .transcript(Some(thread.clone()), None, 100)
        .await
        .unwrap();
    let entry = view
        .entries
        .iter()
        .find(|entry| {
            entry
                .submission
                .as_ref()
                .is_some_and(|submission| submission.id == newest)
        })
        .unwrap();
    assert!(entry.is_preview);
    assert_ne!(entry.text, newest_text);
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    if queue_metadata {
        let queue = handle
            .queue_snapshot(thread.clone(), ids.clone())
            .await
            .unwrap();
        assert_eq!(queue.pending.len(), 128);
        assert_eq!(queue.pending.last().unwrap().id, ids[127]);
        app.pane_mut().unwrap().accept_queue(queue);
    } else {
        assert!(app.pane().unwrap().queue.is_empty());
    }
    assert_eq!(app.pane().unwrap().visible_queue().len(), pending_count);
    assert_eq!(
        app.pane().unwrap().visible_queue().last().unwrap().id,
        newest
    );
    let mut jobs = JoinSet::new();
    app.key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut jobs,
    );
    assert_eq!(jobs.len(), 1);
    if queue_metadata {
        app.paste("draft typed while recall is pending");
    }
    collect(&mut app, &mut jobs).await;
    if queue_metadata {
        assert_eq!(app.draft_mut().text, "draft typed while recall is pending");
        assert_eq!(
            app.pane().unwrap().recalled.as_deref(),
            Some(newest_text.as_str())
        );
        app.draft_mut().clear();
        app.key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut jobs,
        );
        assert!(jobs.is_empty());
    }
    assert_eq!(app.draft_mut().text, newest_text);
    let snapshot = handle
        .queue_snapshot(thread.clone(), ids.clone())
        .await
        .unwrap();
    for id in &ids {
        assert_eq!(
            snapshot
                .submissions
                .iter()
                .find(|item| &item.id == id)
                .unwrap()
                .state,
            if *id == newest {
                SubmissionState::Recalled
            } else {
                SubmissionState::Pending
            }
        );
    }
    assert_eq!(app.pane().unwrap().visible_queue().len(), pending_count - 1);
    assert_eq!(executor.requests.lock().unwrap().len(), 1);

    executor.release.add_permits(pending_count);
    completed(&handle, &thread, &executor, pending_count).await;
    {
        let requests = executor.requests.lock().unwrap();
        assert_eq!(requests.len(), pending_count);
        let messages = &requests.last().unwrap().messages;
        for text in &texts {
            let occurrences = messages
                .iter()
                .flat_map(|message| &message.content)
                .filter(
                    |block| matches!(block, ContentBlock::Text { text: value } if value == text),
                )
                .count();
            assert_eq!(occurrences, usize::from(text != &newest_text));
        }
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut jobs);
    collect(&mut app, &mut jobs).await;
    until(&handle, &thread, |_| {
        executor.requests.lock().unwrap().len() == pending_count + 1
    })
    .await;
    {
        let requests = executor.requests.lock().unwrap();
        let messages = &requests.last().unwrap().messages;
        for text in &texts {
            let occurrences = messages
                .iter()
                .flat_map(|message| &message.content)
                .filter(
                    |block| matches!(block, ContentBlock::Text { text: value } if value == text),
                )
                .count();
            assert_eq!(
                occurrences, 1,
                "each input must reach the model exactly once"
            );
        }
    }
    assert!(app.draft_mut().text.is_empty());
    executor.release.add_permits(1);
    completed(&handle, &thread, &executor, pending_count + 1).await;
    assert_eq!(executor.requests.lock().unwrap().len(), pending_count + 1);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn backspace_recalls_transcript_first_full_unicode_input_once() {
    keyboard_recall(2, false).await;
}

#[tokio::test]
async fn backspace_recalls_newest_of_132_past_queue_cap_with_late_draft() {
    keyboard_recall(132, true).await;
}

#[tokio::test]
async fn transcript_recall_read_failure_keeps_both_queue_and_draft_unchanged() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start_queue(&workspace).await;
    let older = submit(&handle, &thread, "older queued".into()).await;
    let newest = submit(&handle, &thread, "界".repeat(3000)).await;
    let mut view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    view.entries
        .iter_mut()
        .find(|entry| {
            entry
                .submission
                .as_ref()
                .is_some_and(|submission| submission.id == newest)
        })
        .unwrap()
        .sequence = i64::MAX;
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    let mut jobs = JoinSet::new();
    app.key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut jobs,
    );
    collect(&mut app, &mut jobs).await;
    assert!(app.draft_mut().text.is_empty());
    assert!(app.pane().unwrap().recalled.is_none());
    assert!(app.pane().unwrap().notice.contains("Shortcut failed"));
    let queue = handle.queue(thread).await.unwrap();
    assert_eq!(
        queue.iter().map(|input| &input.id).collect::<Vec<_>>(),
        [&older, &newest]
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn oversized_queue_record_is_rejected_before_recall() {
    let workspace = Workspace::new();
    let (handle, thread, executor) = start_queue(&workspace).await;
    let id = submit(&handle, &thread, "retained queued input".into()).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let mut queue = handle
        .queue_snapshot(thread.clone(), vec![id.clone()])
        .await
        .unwrap();
    queue.pending[0].text = "x".repeat(composer::INPUT_LIMIT + 1);
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    app.pane_mut().unwrap().accept_queue(queue);
    let mut jobs = JoinSet::new();
    app.key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut jobs,
    );
    collect(&mut app, &mut jobs).await;
    assert!(app.draft_mut().text.is_empty());
    assert!(app.pane().unwrap().recalled.is_none());
    assert!(app.pane().unwrap().notice.contains("exceeds 64 KiB"));
    let queue = handle.queue(thread).await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].id, id);
    assert_eq!(queue[0].text, "retained queued input");
    assert_eq!(executor.requests.lock().unwrap().len(), 1);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn consumed_transcript_recall_does_not_fall_back_to_another_pending_input() {
    let workspace = Workspace::new();
    let (handle, thread, executor) = start_queue(&workspace).await;
    let newest = submit(&handle, &thread, "界".repeat(3000)).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    executor.release.add_permits(1);
    until(&handle, &thread, |_| {
        executor.requests.lock().unwrap().len() == 2
    })
    .await;
    let remaining = submit(&handle, &thread, "another queued input".into()).await;
    let mut jobs = JoinSet::new();
    app.key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut jobs,
    );
    collect(&mut app, &mut jobs).await;
    assert!(app.draft_mut().text.is_empty());
    assert!(app.pane().unwrap().recalled.is_none());
    assert!(app.pane().unwrap().notice.contains("Shortcut failed"));
    let snapshot = handle
        .queue_snapshot(thread, vec![newest.clone(), remaining.clone()])
        .await
        .unwrap();
    assert_eq!(snapshot.pending.len(), 1);
    assert_eq!(snapshot.pending[0].id, remaining);
    assert_eq!(
        snapshot
            .submissions
            .iter()
            .find(|item| item.id == newest)
            .unwrap()
            .state,
        SubmissionState::Consumed
    );
    assert_eq!(executor.requests.lock().unwrap().len(), 2);
    handle.shutdown().await.unwrap();
}
