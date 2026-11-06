use super::*;

#[path = "model_default_tests.rs"]
mod default_tests;

async fn summary(handle: &Handle, thread: &ThreadId) -> ThreadSummary {
    handle
        .transcript(Some(thread.clone()), None, 1)
        .await
        .unwrap()
        .threads
        .into_iter()
        .find(|item| &item.id == thread)
        .unwrap()
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.into(),
        context_window: 200_000,
    }
}

#[tokio::test]
async fn explicit_model_routing_does_not_require_catalog_membership() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let summary = summary(&handle, &thread).await;
    for text in ["/model anthropic:new-model", "/models anthropic:new-model"] {
        let Action::Application(commands::Request::Model(Some(requested))) =
            commands::route(text, &summary, None, &[]).unwrap()
        else {
            panic!("explicit model request expected")
        };
        assert_eq!(requested, "anthropic:new-model");
    }
    assert!(matches!(
        commands::route("/models", &summary, None, &[]).unwrap(),
        Action::Application(commands::Request::Models)
    ));
    assert!(matches!(
        commands::route("/model", &summary, None, &[]).unwrap(),
        Action::Application(commands::Request::Model(None))
    ));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn model_discovery_refreshes_kernel_catalog_before_profile_update() {
    let workspace = Workspace::new();
    let (handle, thread, executor) = start(&workspace, false, true).await;
    let before = handle.context(thread.clone()).await.unwrap();
    let target = summary(&handle, &thread).await;
    let defaults = commands::DefaultsWriter::at(workspace.0.join("global.toml"));
    commands::update_model(
        &handle,
        &target,
        "anthropic:fixture",
        &defaults,
        |_| async {
            executor
                .extra_models
                .write()
                .unwrap()
                .push(model("anthropic:fixture"));
            assert!(!handle
                .catalog()
                .await?
                .iter()
                .any(|item| item.model.id == "anthropic:fixture"));
            Ok("anthropic:fixture".into())
        },
    )
    .await
    .unwrap();
    let after = handle.context(thread.clone()).await.unwrap();
    assert_eq!(after.profile.model, "anthropic:fixture");
    assert_eq!(after.profile.instructions, before.profile.instructions);
    assert_eq!(after.profile.reasoning, before.profile.reasoning);
    assert!(after.profile_revision > before.profile_revision);

    executor
        .extra_models
        .write()
        .unwrap()
        .push(model("openai:fixture"));
    let summary = summary(&handle, &thread).await;
    let Outcome::Picker(picker) = commands::execute(
        commands::Request::Models,
        &handle,
        &summary,
        None,
        None,
        None,
    )
    .await
    .unwrap() else {
        panic!("model picker expected")
    };
    assert_eq!(
        picker
            .items
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>(),
        ["fixture", "anthropic:fixture", "openai:fixture"]
    );
    assert_eq!(picker.selected, 1);
    assert_eq!(picker.title, "Select a model (current: anthropic:fixture)");
    assert_eq!(
        picker.command().as_deref(),
        Some("/model anthropic:fixture")
    );
    assert_eq!(
        handle.context(thread).await.unwrap().profile.model,
        "anthropic:fixture"
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn delayed_model_discovery_cannot_overwrite_newer_profile() {
    let workspace = Workspace::new();
    let (handle, thread, executor) = start(&workspace, false, true).await;
    let path = workspace.0.join("global.toml");
    std::fs::write(&path, "# unchanged defaults\nreasoning = 'low'\n").unwrap();
    let defaults = commands::DefaultsWriter::at(path.clone());
    let target = summary(&handle, &thread).await;
    let error = commands::update_model(&handle, &target, "openai:late", &defaults, |_| async {
        let context = handle.context(thread.clone()).await?;
        let mut profile = context.profile;
        profile.reasoning = ReasoningEffort::High;
        handle
            .command(Command::UpdateProfile {
                id: CommandId::new(),
                thread: thread.clone(),
                profile,
                expected_revision: context.profile_revision,
            })
            .await?;
        executor
            .extra_models
            .write()
            .unwrap()
            .push(model("openai:late"));
        Ok("openai:late".into())
    })
    .await
    .err()
    .expect("stale revision must be rejected");
    assert!(error.to_string().contains("profile changed"), "{error:#}");
    let context = handle.context(thread).await.unwrap();
    assert_eq!(context.profile.model, "fixture");
    assert_eq!(context.profile.reasoning, ReasoningEffort::High);
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "# unchanged defaults\nreasoning = 'low'\n"
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn discovery_and_catalog_failures_leave_profile_unchanged() {
    let workspace = Workspace::new();
    let (handle, thread, executor) = start(&workspace, false, true).await;
    let before = handle.context(thread.clone()).await.unwrap();
    let path = workspace.0.join("global.toml");
    let defaults = commands::DefaultsWriter::at(path.clone());
    let target = summary(&handle, &thread).await;
    assert!(
        commands::update_model(&handle, &target, "openai:missing", &defaults, |_| async {
            bail!("requested provider unavailable")
        })
        .await
        .is_err()
    );
    assert!(
        commands::update_model(&handle, &target, "openai:absent", &defaults, |_| async {
            Ok("openai:absent".into())
        })
        .await
        .is_err()
    );
    assert!(
        commands::update_model(&handle, &target, "fixture", &defaults, |_| async {
            executor
                .extra_models
                .write()
                .unwrap()
                .push(model("fixture"));
            Ok("fixture".into())
        })
        .await
        .is_err()
    );
    let after = handle.context(thread).await.unwrap();
    assert_eq!(after.profile.model, before.profile.model);
    assert_eq!(after.profile_revision, before.profile_revision);
    assert_eq!(handle.catalog().await.unwrap().len(), 1);
    assert!(!path.exists());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_model_command_preserves_edited_draft_and_clears_sending_indicator() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    app.draft_mut().insert("/model anthropic:unavailable");
    let mut jobs = JoinSet::new();
    app.submit(&mut jobs);
    app.draft_mut().insert(" edited");
    collect(&mut app, &mut jobs).await;
    assert_eq!(app.draft_mut().text, "/model anthropic:unavailable edited");
    let pane = app.pane().unwrap();
    assert!(pane.notice.contains("Provider service has not connected"));
    assert!(pane.receipt.is_none());
    assert!(pane.pending.is_none());
    assert_eq!(
        handle.context(thread).await.unwrap().profile.model,
        "fixture"
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn stale_model_picker_cannot_replace_new_runtime_or_thread_draft() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    let mut jobs = JoinSet::new();
    app.draft_mut().insert("/models");
    app.submit(&mut jobs);
    app.select(ThreadId::new());
    app.draft_mut().insert("other thread draft");
    collect(&mut app, &mut jobs).await;
    assert!(app.picker.is_none());
    assert_eq!(app.draft_mut().text, "other thread draft");

    app.select(thread.clone());
    app.draft_mut().insert("/models");
    app.submit(&mut jobs);
    app.runtime_epoch += 1;
    app.pane_mut().unwrap().notice = "new runtime notice".into();
    collect(&mut app, &mut jobs).await;
    assert!(app.picker.is_none());
    assert_eq!(app.draft_mut().text, "/models");
    assert_eq!(app.pane().unwrap().notice, "new runtime notice");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn new_session_rejects_busy_root_preserving_draft_and_scope() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let original = summary(&handle, &thread).await;
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    app.draft_mut().insert("/new");
    let mut jobs = JoinSet::new();
    app.submit(&mut jobs);
    collect(&mut app, &mut jobs).await;
    assert_eq!(app.selected.as_ref(), Some(&thread));
    assert_eq!(app.draft_mut().text, "/new");
    assert!(app
        .pane()
        .unwrap()
        .notice
        .contains("finish or stop active agents and jobs first"));
    assert_eq!(
        handle
            .sessions(bob_kernel::SessionQuery::default())
            .await
            .unwrap()
            .sessions
            .len(),
        1
    );

    stop_and_settle(&handle, &original).await;
    app.submit(&mut jobs);
    collect(&mut app, &mut jobs).await;
    assert_ne!(app.selected.as_ref(), Some(&thread));
    assert!(handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread,
            text: "old session work".into()
        })
        .await
        .is_err());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn new_session_preflight_blocks_unsettled_agents_and_jobs() {
    use bob_kernel::{JobId, JobKind, JobState, JobSummary};
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let mut summary = summary(&handle, &thread).await;
    for state in [
        ThreadState::Active,
        ThreadState::Stopping,
        ThreadState::Interrupted,
        ThreadState::Held,
    ] {
        summary.state = state;
        assert!(commands::ensure_session_idle(&[&summary], &[]).is_err());
    }
    summary.state = ThreadState::Completed;
    for state in [
        JobState::Queued,
        JobState::Running,
        JobState::Stopping,
        JobState::Unknown,
        JobState::Completed,
        JobState::Failed,
        JobState::Cancelled,
    ] {
        let job = JobSummary {
            id: JobId::new(),
            session: summary.session.clone(),
            thread: thread.clone(),
            operation: None,
            run: None,
            kind: JobKind::Task,
            state,
            title: "fixture child".into(),
            result: None,
            started_at: 0,
            background: true,
        };
        assert_eq!(
            commands::ensure_session_idle(&[&summary], &[job]).is_ok(),
            matches!(
                state,
                JobState::Completed | JobState::Failed | JobState::Cancelled
            )
        );
    }
    handle.shutdown().await.unwrap();
}
