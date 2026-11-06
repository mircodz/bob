use super::*;

#[tokio::test]
async fn subtree_stop_routes_by_thread_even_without_a_visible_parent_run() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let view = until(&handle, &thread, |view| {
        view.threads
            .iter()
            .any(|item| item.id == thread && item.state == ThreadState::Active)
    })
    .await;
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    app.draft_mut().insert("keep this draft");
    let parent = app
        .threads
        .iter_mut()
        .find(|item| item.id == thread)
        .unwrap();
    let run = parent.run.clone();
    parent.run = None;
    parent.state = ThreadState::Completed;
    parent.activity = None;
    let mut child = parent.clone();
    child.id = ThreadId("visible-descendant".into());
    child.parent_thread = Some(thread.clone());
    child.parent_run = run;
    child.state = ThreadState::Active;
    child.activity = Some(bob_kernel::ThreadActivity::Model);
    app.threads.push(child);
    let mut jobs = JoinSet::new();
    assert!(!app.key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut jobs
    ));
    assert_eq!(jobs.len(), 1);
    collect(&mut app, &mut jobs).await;
    until(&handle, &thread, |view| {
        view.threads
            .iter()
            .any(|item| item.id == thread && item.state == ThreadState::Cancelled)
    })
    .await;
    assert_eq!(app.draft_mut().text, "keep this draft");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_escape_does_not_stop_an_unrelated_thread_or_hold_the_composer() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let view = until(&handle, &thread, |view| {
        view.threads
            .iter()
            .any(|item| item.id == thread && item.state == ThreadState::Active)
    })
    .await;
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    let selected = app
        .threads
        .iter_mut()
        .find(|item| item.id == thread)
        .unwrap();
    selected.state = ThreadState::Completed;
    selected.run = None;
    selected.activity = None;
    let mut unrelated = selected.clone();
    unrelated.id = ThreadId("unrelated".into());
    unrelated.state = ThreadState::Active;
    app.threads.push(unrelated);
    app.draft_mut().insert("unsent text");
    let mut jobs = JoinSet::new();
    assert!(!app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut jobs));
    assert!(jobs.is_empty());
    assert_eq!(app.focus, Focus::Composer);
    assert_eq!(app.draft_mut().text, "unsent text");
    handle.shutdown().await.unwrap();
}

#[test]
fn explicit_stop_targets_the_selected_thread_and_joining_is_not_terminal() {
    let summary = ThreadSummary {
        id: ThreadId("parent".into()),
        session: bob_kernel::SessionId("session".into()),
        parent_run: None,
        parent_thread: None,
        needs_approval: false,
        activity: Some(bob_kernel::ThreadActivity::Agent),
        name: "Parent".into(),
        model: "fixture".into(),
        state: ThreadState::Active,
        run: None,
    };
    assert!(
        matches!(route("/stop", &summary, None).unwrap(), Action::Command(Command::StopThread { thread, .. }) if thread == summary.id)
    );
    let line =
        super::super::activity::line(&summary, None, None, Duration::ZERO, "esc to interrupt")
            .unwrap();
    assert!(line.to_string().contains("Working (0s · esc to interrupt)"));
    assert!(!super::super::style::terminal_thread(&summary.state));
    assert_eq!(
        super::super::style::thread_color(&summary.state),
        Palette::RUNNING()
    );
    let mut terminal = summary;
    terminal.state = ThreadState::Completed;
    assert!(
        matches!(route("/stop", &terminal, None).unwrap(), Action::Command(Command::StopThread { thread, .. }) if thread == terminal.id)
    );
}
