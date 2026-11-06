use super::*;

async fn reasoning(
    handle: &Handle,
    target: &ThreadSummary,
    defaults: &commands::DefaultsWriter,
    effort: ReasoningEffort,
) -> Outcome {
    commands::execute_with_defaults(
        commands::Request::Reasoning(Some(effort)),
        handle,
        target,
        None,
        None,
        None,
        defaults,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn inspecting_models_and_reasoning_never_creates_defaults() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let target = summary(&handle, &thread).await;
    let path = workspace.0.join("global.toml");
    let defaults = commands::DefaultsWriter::at(path.clone());
    for request in [
        commands::Request::Model(None),
        commands::Request::Models,
        commands::Request::Reasoning(None),
    ] {
        commands::execute_with_defaults(request, &handle, &target, None, None, None, &defaults)
            .await
            .unwrap();
        assert!(!path.exists());
    }
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn root_changes_save_only_resolved_defaults_and_preserve_config_sentinels() {
    let workspace = Workspace::new();
    let path = workspace.0.join("global.toml");
    let sentinel = "# global sentinel\nsystem = 'global instructions' # keep this comment\n[permissions]\ndefault = 'ask'\n";
    std::fs::write(&path, sentinel).unwrap();
    let project = workspace.0.join(".bob.config.toml");
    let project_config = "system = 'project-only secret sentinel'\nreasoning = 'max'\n";
    std::fs::write(&project, project_config).unwrap();
    let defaults = commands::DefaultsWriter::at(path.clone());
    let (handle, thread, executor) = start(&workspace, false, true).await;
    let mut target = summary(&handle, &thread).await;
    target.name = "not-the-root-name".into();
    assert!(matches!(
        reasoning(&handle, &target, &defaults, ReasoningEffort::High).await,
        Outcome::Receipt(_)
    ));
    let before = handle.context(thread.clone()).await.unwrap();
    for (requested, qualified, resolved) in [
        ("anthropic", "anthropic", "anthropic:default"),
        (
            "anthropic/current",
            "anthropic:current",
            "anthropic:current",
        ),
        ("next", "anthropic:next", "anthropic:next"),
        ("openai:current", "openai:current", "openai:current"),
        ("next", "openai:next", "openai:next"),
    ] {
        let outcome = commands::update_model(
            &handle,
            &target,
            requested,
            &defaults,
            |discovery_request| {
                assert_eq!(discovery_request, qualified);
                executor.extra_models.write().unwrap().push(model(resolved));
                std::future::ready(Ok(resolved.to_owned()))
            },
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Receipt(_)));
        let profile = handle.context(thread.clone()).await.unwrap().profile;
        assert_eq!(profile.model, resolved);
        assert_eq!(profile.reasoning, ReasoningEffort::High);
        assert_eq!(profile.instructions, before.profile.instructions);
        assert_eq!(profile.max_requests, before.profile.max_requests);
        assert_eq!(profile.read_only, before.profile.read_only);
        let saved = std::fs::read_to_string(&path).unwrap();
        let (provider, model) = resolved.split_once(':').unwrap();
        assert!(
            saved.contains(&format!("provider = {provider:?}")),
            "{saved}"
        );
        assert!(saved.contains(&format!("model = {model:?}")), "{saved}");
    }
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("provider = \"openai\""), "{saved}");
    assert!(saved.contains("model = \"next\""), "{saved}");
    assert!(saved.contains("reasoning = \"high\""), "{saved}");
    assert!(saved.contains("# global sentinel"));
    assert!(saved.contains("system = 'global instructions' # keep this comment"));
    assert!(saved.contains("[permissions]\ndefault = 'ask'"));
    assert!(!saved.contains("project-only"));
    assert_eq!(std::fs::read_to_string(&project).unwrap(), project_config);

    reasoning(&handle, &target, &defaults, ReasoningEffort::Off).await;
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("reasoning ="));
    let session_id = target.session;
    handle.shutdown().await.unwrap();
    let reopened = Runtime::open_paused(&workspace.0.join("kernel.sqlite"), executor)
        .await
        .unwrap();
    let restored = reopened.session(session_id).await.unwrap();
    assert_eq!(restored.profile.model, "openai:next");
    assert_eq!(restored.profile.reasoning, ReasoningEffort::Off);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_default_saves_report_committed_model_and_reasoning_changes() {
    let workspace = Workspace::new();
    let (handle, thread, executor) = start(&workspace, false, true).await;
    let target = summary(&handle, &thread).await;
    let defaults = commands::DefaultsWriter::at(workspace.0.clone());
    let outcome = commands::update_model(
        &handle,
        &target,
        "anthropic:committed",
        &defaults,
        |requested| {
            executor
                .extra_models
                .write()
                .unwrap()
                .push(model(&requested));
            std::future::ready(Ok(requested))
        },
    )
    .await
    .unwrap();
    let Outcome::Notice(warning) = outcome else {
        panic!("a precise partial-success warning is required")
    };
    assert!(warning.contains("Model changed to anthropic:committed"));
    assert!(warning.contains("saving global defaults failed"));
    assert!(warning.contains("session change remains active"));
    assert_eq!(
        handle.context(thread.clone()).await.unwrap().profile.model,
        "anthropic:committed"
    );
    let Outcome::Notice(warning) =
        reasoning(&handle, &target, &defaults, ReasoningEffort::Low).await
    else {
        panic!("a precise partial-success warning is required")
    };
    assert!(warning.contains("Reasoning changed to low"));
    assert!(warning.contains("saving global defaults failed"));
    assert_eq!(
        handle.context(thread).await.unwrap().profile.reasoning,
        ReasoningEffort::Low
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_root_model_and_reasoning_saves_keep_both_narrow_updates() {
    let workspace = Workspace::new();
    let (handle, thread, executor) = start(&workspace, false, true).await;
    let first = summary(&handle, &thread).await;
    let second_handle = Runtime::open(&workspace.0.join("kernel.sqlite"), executor.clone())
        .await
        .unwrap();
    let second = second_handle
        .command(Command::CreateSession {
            id: CommandId::new(),
            workspace: workspace.0.to_string_lossy().into_owned(),
            profile: handle.context(thread).await.unwrap().profile,
            policy: Policy::default(),
        })
        .await
        .unwrap();
    let second = summary(&second_handle, &second.thread).await;
    let path = workspace.0.join("global.toml");
    std::fs::write(&path, "# concurrent sentinel\nsystem = 'keep'\n").unwrap();
    let model_defaults = commands::DefaultsWriter::at(path.clone());
    let reasoning_defaults = commands::DefaultsWriter::at(path.clone());
    let (model_result, reasoning_result) = tokio::join!(
        commands::update_model(
            &handle,
            &first,
            "openai:parallel",
            &model_defaults,
            |requested| {
                executor
                    .extra_models
                    .write()
                    .unwrap()
                    .push(model(&requested));
                std::future::ready(Ok(requested))
            }
        ),
        reasoning(
            &second_handle,
            &second,
            &reasoning_defaults,
            ReasoningEffort::Max
        ),
    );
    assert!(matches!(model_result.unwrap(), Outcome::Receipt(_)));
    assert!(matches!(reasoning_result, Outcome::Receipt(_)));
    let saved = std::fs::read_to_string(path).unwrap();
    assert!(saved.contains("provider = \"openai\""), "{saved}");
    assert!(saved.contains("model = \"parallel\""), "{saved}");
    assert!(saved.contains("reasoning = \"max\""), "{saved}");
    assert!(saved.contains("# concurrent sentinel"));
    assert!(saved.contains("system = 'keep'"));
    second_handle.shutdown().await.unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn app_reasoning_commands_use_injected_defaults_and_report_save_failure_as_success() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let mut app = App::new(80);
    let path = workspace.0.join("global.toml");
    app.defaults = commands::DefaultsWriter::at(path.clone());
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    let mut jobs = JoinSet::new();
    app.draft_mut().insert("/reasoning high");
    app.submit(&mut jobs);
    collect(&mut app, &mut jobs).await;
    assert!(app.draft_mut().text.is_empty());
    assert!(std::fs::read_to_string(&path)
        .unwrap()
        .contains("reasoning = \"high\""));
    app.defaults = commands::DefaultsWriter::at(workspace.0.clone());
    app.draft_mut().insert("/reasoning off");
    app.submit(&mut jobs);
    collect(&mut app, &mut jobs).await;
    assert!(app.draft_mut().text.is_empty());
    assert!(app.pane().unwrap().pending.is_none());
    assert!(app
        .pane()
        .unwrap()
        .notice
        .contains("Reasoning changed to off"));
    assert!(app
        .pane()
        .unwrap()
        .notice
        .contains("saving global defaults failed"));
    assert_eq!(
        handle.context(thread).await.unwrap().profile.reasoning,
        ReasoningEffort::Off
    );
    assert!(std::fs::read_to_string(path)
        .unwrap()
        .contains("reasoning = \"high\""));
    handle.shutdown().await.unwrap();
}

struct ChildFixture(std::sync::atomic::AtomicBool);

#[async_trait::async_trait]
impl Executor for ChildFixture {
    fn models(&self) -> Vec<ModelInfo> {
        [
            "anthropic:parent",
            "openai:child",
            "openai:next",
            "anthropic:explicit",
        ]
        .into_iter()
        .map(model)
        .collect()
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }

    async fn generate(
        &self,
        _: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _: bob_kernel::ModelProgress,
    ) -> Result<Completion> {
        if !self.0.swap(true, Ordering::SeqCst) {
            return Ok(Completion {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse {
                        id: "spawn-child".into(),
                        name: "spawn_agent".into(),
                        input: json!({"name":"worker","task":"wait for settings","model":"openai:child"}),
                    }],
                },
                stop_reason: StopReason::ToolUse,
                usage: Usage::default(),
            });
        }
        cancelled(&mut cancel).await;
        bail!("fixture cancelled")
    }

    async fn prepare(&self, _: Invocation) -> Result<PreparedTool> {
        unreachable!()
    }

    async fn execute(&self, _: PreparedTool, _: watch::Receiver<bool>) -> Result<ToolOutcome> {
        unreachable!()
    }
}

#[tokio::test]
async fn child_model_and_reasoning_changes_never_save_defaults_even_named_root() {
    let workspace = Workspace::new();
    let path = workspace.0.join("global.toml");
    let sentinel = "# no child defaults\nprovider = 'anthropic'\nmodel = 'parent'\n";
    std::fs::write(&path, sentinel).unwrap();
    let defaults = commands::DefaultsWriter::at(path.clone());
    let handle = Runtime::open(
        &workspace.0.join("kernel.sqlite"),
        Arc::new(ChildFixture(false.into())),
    )
    .await
    .unwrap();
    let root = handle
        .command(Command::CreateSessionConfigured {
            id: CommandId::new(),
            workspace: workspace.0.to_string_lossy().into_owned(),
            profile: Profile {
                model: "anthropic:parent".into(),
                instructions: "settings fixture".into(),
                read_only: false,
                max_requests: 8,
                reasoning: ReasoningEffort::High,
            },
            policy: Policy::default(),
            permissions: bob_kernel::PermissionPolicy {
                default: bob_kernel::RuleDecision::Allow,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: root.thread.clone(),
            text: "spawn a child".into(),
        })
        .await
        .unwrap();
    let view = until(&handle, &root.thread, |view| {
        view.threads
            .iter()
            .any(|thread| thread.parent_thread.as_ref() == Some(&root.thread))
    })
    .await;
    let mut child = view
        .threads
        .into_iter()
        .find(|thread| thread.parent_thread.as_ref() == Some(&root.thread))
        .unwrap();
    child.name = "root".into();
    let before = handle.context(child.id.clone()).await.unwrap();
    for (requested, expected) in [
        ("next", "openai:next"),
        ("anthropic/explicit", "anthropic:explicit"),
    ] {
        let outcome = commands::update_model(&handle, &child, requested, &defaults, |qualified| {
            assert_eq!(qualified, expected);
            std::future::ready(Ok(qualified))
        })
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Receipt(_)));
    }
    assert_eq!(
        handle
            .context(child.id.clone())
            .await
            .unwrap()
            .profile
            .reasoning,
        before.profile.reasoning
    );
    assert!(matches!(
        reasoning(&handle, &child, &defaults, ReasoningEffort::Off).await,
        Outcome::Receipt(_)
    ));
    let after = handle.context(child.id).await.unwrap();
    assert_eq!(after.profile.model, "anthropic:explicit");
    assert_eq!(after.profile.reasoning, ReasoningEffort::Off);
    assert_eq!(after.profile.instructions, before.profile.instructions);
    assert_eq!(after.profile.max_requests, before.profile.max_requests);
    assert_eq!(after.profile.read_only, before.profile.read_only);
    let parent = handle.session(root.session).await.unwrap();
    assert_eq!(parent.profile.model, "anthropic:parent");
    assert_eq!(parent.profile.reasoning, ReasoningEffort::High);
    assert_eq!(std::fs::read_to_string(path).unwrap(), sentinel);
    handle.shutdown().await.unwrap();
}
