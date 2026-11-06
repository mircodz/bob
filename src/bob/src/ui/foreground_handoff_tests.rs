use super::*;

struct ShellExecutor {
    calls: AtomicUsize,
    executions: AtomicUsize,
}

#[async_trait]
impl Executor for ShellExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "fixture".into(),
            context_window: 100_000,
        }]
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            effect: EffectClass::Write,
            spec: ToolSpec {
                name: "bash".into(),
                description: "Controlled shell fixture".into(),
                input_schema: json!({"type":"object"}),
            },
        }]
    }

    async fn generate(
        &self,
        request: RequestSpec,
        _cancel: watch::Receiver<bool>,
        _progress: bob_kernel::ModelProgress,
    ) -> Result<Completion> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let content = if first {
            vec![ContentBlock::ToolUse {
                id: "held-shell".into(),
                name: "bash".into(),
                input: json!({"command":"printf held", "timeout":30}),
            }]
        } else {
            assert!(request.messages.iter().any(|message| {
                message.role == Role::User && message.text() == "Answer this while the shell runs"
            }));
            assert!(request.messages.iter().any(|message| {
                message.content.iter().any(|block| {
                    matches!(block, ContentBlock::ToolResult { content, .. } if content.as_text().is_some_and(|text| text.contains("job_id")))
                })
            }));
            vec![ContentBlock::Text {
                text: "Responded before the shell finished.".into(),
            }]
        };
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            usage: Usage::default(),
        })
    }

    async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        Ok(PreparedTool {
            invocation,
            specification: json!({}),
            preview: "Controlled foreground shell".into(),
            presentation: None,
        })
    }

    async fn execute(
        &self,
        tool: PreparedTool,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        assert_eq!(tool.invocation.input["timeout"], 30);
        assert_ne!(tool.invocation.input["run_in_background"], true);
        self.executions.fetch_add(1, Ordering::SeqCst);
        cancelled(&mut cancel).await;
        Ok(ToolOutcome {
            text: "Stopped the original shell worker.".into(),
            is_error: false,
            interrupted: true,
            presentation: None,
        })
    }
}

async fn handoff_through_composer(steer: bool) {
    let workspace = Workspace::new();
    let executor = Arc::new(ShellExecutor {
        calls: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
    });
    let handle = Runtime::open(&workspace.0.join("kernel.sqlite"), executor.clone())
        .await
        .unwrap();
    let receipt = handle
        .command(Command::CreateSession {
            id: CommandId::new(),
            workspace: workspace.0.to_string_lossy().into_owned(),
            profile: Profile {
                model: "fixture".into(),
                instructions: "UI handoff fixture".into(),
                read_only: false,
                max_requests: 8,
                reasoning: ReasoningEffort::Off,
            },
            policy: Policy {
                yolo: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let thread = receipt.thread;
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: thread.clone(),
            text: "Start a foreground shell".into(),
        })
        .await
        .unwrap();
    let view = until(&handle, &thread, |_| {
        executor.executions.load(Ordering::SeqCst) == 1
    })
    .await;
    assert!(handle
        .jobs(receipt.session.clone())
        .await
        .unwrap()
        .is_empty());
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    let mut tasks = JoinSet::new();
    app.paste("Answer this while the shell runs");
    app.key(
        KeyEvent::new(
            KeyCode::Enter,
            if steer {
                KeyModifiers::ALT
            } else {
                KeyModifiers::NONE
            },
        ),
        &mut tasks,
    );
    while !tasks.is_empty() {
        collect(&mut app, &mut tasks).await;
    }
    let view = until(&handle, &thread, |view| {
        view.entries
            .iter()
            .any(|entry| entry.text == "Responded before the shell finished.")
    })
    .await;
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
    assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
    assert!(handle.queue(thread.clone()).await.unwrap().is_empty());
    let retained = handle.jobs(receipt.session.clone()).await.unwrap();
    assert_eq!(retained.len(), 1);
    let job = &retained[0];
    assert_eq!(job.state, bob_kernel::JobState::Running);
    assert!(job.background);
    let notices: Vec<_> = view
        .entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Notice && entry.text.contains(&job.id.0))
        .collect();
    assert_eq!(notices.len(), 1);

    app.accept_live(handle.subscribe().borrow().clone());
    app.refresh(&mut tasks);
    while !tasks.is_empty() {
        collect(&mut app, &mut tasks).await;
    }
    let screen = render(&mut app).1;
    assert!(
        screen.contains("background shell commands · 1 running"),
        "{screen}"
    );
    assert_eq!(
        screen.matches("Answer this while the shell runs").count(),
        1,
        "{screen}"
    );
    assert!(!screen.contains("queued ·"), "{screen}");
    assert!(app.pane().unwrap().draft.text.is_empty());
    let notice_text = super::super::render::transcript(app.pane().unwrap(), true, 120)
        .iter()
        .map(|row| row.line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(notice_text.contains("Shell continues in background as job"));

    app.paste(&format!("/jobs stop {}", job.id));
    app.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut tasks,
    );
    while !tasks.is_empty() {
        collect(&mut app, &mut tasks).await;
    }
    until(&handle, &thread, |view| {
        view.entries.iter().any(|entry| {
            entry.tool.as_ref().is_some_and(|tool| {
                tool.id == *job.operation.as_ref().unwrap() && tool.state == OperationState::Unknown
            })
        })
    })
    .await;
    let stopped = handle.jobs(receipt.session).await.unwrap();
    assert_eq!(stopped[0].state, bob_kernel::JobState::Unknown);
    assert!(jobs::panel_lines(&stopped, 120).is_empty());
    assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn enter_hands_off_foreground_shell_and_shows_job_without_duplicate_input() {
    handoff_through_composer(false).await;
}

#[tokio::test]
async fn alt_enter_hands_off_foreground_shell_and_keeps_job_cancellable() {
    handoff_through_composer(true).await;
}
