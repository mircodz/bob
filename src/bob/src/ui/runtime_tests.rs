use super::*;
use async_trait::async_trait;
use bob_kernel::{
    cancelled, EffectClass, Executor, Invocation, ModelInfo, PreparedTool, RequestSpec, Runtime,
    ToolBody, ToolDescriptor, ToolOutcome, ToolPresentation,
};
use bob_types::{Completion, ContentBlock, Message, Role, StopReason, ToolSpec, Usage};
use bob_ui::theme::Palette;
use ratatui::{backend::TestBackend, buffer::Buffer};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::{
    sync::watch,
    time::{sleep, timeout},
};

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct FakeExecutor {
    calls: AtomicUsize,
    executions: AtomicUsize,
    interrupted: bool,
    hold: bool,
    extra_models: std::sync::RwLock<Vec<ModelInfo>>,
}
#[async_trait]
impl Executor for FakeExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        let mut models = vec![ModelInfo {
            id: "fixture".into(),
            context_window: 100_000,
        }];
        models.extend(self.extra_models.read().unwrap().iter().cloned());
        models
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            effect: EffectClass::Write,
            spec: ToolSpec {
                name: "write_file".into(),
                description: "Fixture write".into(),
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
            },
        }]
    }
    async fn generate(
        &self,
        _request: RequestSpec,
        mut cancel: watch::Receiver<bool>,
        _progress: bob_kernel::ModelProgress,
    ) -> Result<Completion> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.hold {
            cancelled(&mut cancel).await;
            bail!("fixture cancelled");
        }
        let (content, stop_reason) = if call == 0 {
            (
                vec![ContentBlock::ToolUse {
                    id: "fixture-call".into(),
                    name: "write_file".into(),
                    input: json!({"path":"src/main.rs"}),
                }],
                StopReason::ToolUse,
            )
        } else {
            (
                vec![ContentBlock::Text {
                    text: "**Observed** the failed operation.".into(),
                }],
                StopReason::EndTurn,
            )
        };
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: Usage::default(),
        })
    }
    async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        Ok(PreparedTool {
            invocation,
            specification: json!({"prepared":"immutable change"}),
            preview: "Prepared creation in src/main.rs".into(),
            presentation: Some(ToolPresentation {
                title: "Create".into(),
                target: Some("src/main.rs".into()),
                body: ToolBody::Create {
                    content: "fn main() {}".into(),
                },
                details: vec!["Path does not yet exist".into()],
            }),
        })
    }
    async fn execute(
        &self,
        _tool: PreparedTool,
        _cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome {
            text: "retained failure body".into(),
            is_error: true,
            interrupted: self.interrupted,
            presentation: None,
        })
    }
}
async fn until(handle: &Handle, thread: &ThreadId, predicate: impl Fn(&View) -> bool) -> View {
    timeout(Duration::from_secs(10), async {
        loop {
            let view = handle
                .transcript(Some(thread.clone()), None, 80)
                .await
                .unwrap();
            assert!(view.error.is_none(), "{:?}", view.error);
            if predicate(&view) {
                return view;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("runtime fixture did not settle")
}
async fn start(
    workspace: &Workspace,
    interrupted: bool,
    hold: bool,
) -> (Handle, ThreadId, Arc<FakeExecutor>) {
    let executor = Arc::new(FakeExecutor {
        calls: AtomicUsize::new(0),
        executions: AtomicUsize::new(0),
        interrupted,
        hold,
        extra_models: Default::default(),
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
                instructions: "UI integration fixture".into(),
                read_only: false,
                max_requests: 8,
                reasoning: ReasoningEffort::Off,
            },
            policy: Policy::default(),
        })
        .await
        .unwrap();
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: receipt.thread.clone(),
            text: "Please apply the prepared change".into(),
        })
        .await
        .unwrap();
    (handle, receipt.thread, executor)
}
fn render(app: &mut App) -> (Buffer, String) {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let text = buffer
        .content
        .chunks(120)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    (buffer, text)
}
async fn stop_and_settle(handle: &Handle, summary: &ThreadSummary) {
    handle
        .command(Command::Stop {
            id: CommandId::new(),
            run: summary.run.clone().unwrap(),
        })
        .await
        .unwrap();
    until(handle, &summary.id, |view| {
        view.threads
            .iter()
            .any(|thread| thread.id == summary.id && thread.state == ThreadState::Cancelled)
    })
    .await;
}

async fn collect(app: &mut App, jobs: &mut JoinSet<Reply>) {
    let reply = timeout(Duration::from_secs(10), jobs.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    app.reply(reply);
}

#[path = "foreground_handoff_tests.rs"]
mod foreground_handoff_tests;
#[path = "message_queue_runtime_tests.rs"]
mod message_queue_runtime_tests;
#[path = "model_tests.rs"]
mod model_tests;
#[path = "nested_agent_tests.rs"]
mod nested_agent_tests;
#[path = "picker_parity_tests.rs"]
mod picker_parity_tests;

#[tokio::test]
async fn background_turn_keeps_queue_and_recall_preserves_new_draft() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    until(&handle, &thread, |view| {
        view.threads
            .iter()
            .any(|item| item.id == thread && item.state == ThreadState::Active)
    })
    .await;
    for text in ["first queued", "last queued 界"] {
        handle
            .command(Command::Submit {
                id: CommandId::new(),
                thread: thread.clone(),
                text: text.into(),
            })
            .await
            .unwrap();
    }
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    let mut jobs = JoinSet::new();
    app.refresh(&mut jobs);
    while !jobs.is_empty() {
        collect(&mut app, &mut jobs).await;
    }
    assert_eq!(app.pane().unwrap().queue.len(), 2);
    assert!(render(&mut app).1.contains("queued · 2"));

    app.key(
        KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        &mut jobs,
    );
    collect(&mut app, &mut jobs).await;
    assert_eq!(handle.queue(thread.clone()).await.unwrap().len(), 2);
    let summary = app
        .threads
        .iter()
        .find(|item| item.id == thread)
        .unwrap()
        .clone();
    let retained = handle.jobs(summary.session.clone()).await.unwrap();
    assert!(retained
        .iter()
        .any(|job| job.kind == bob_kernel::JobKind::Turn));
    assert!(jobs::panel_lines(&retained, 100).is_empty());
    let Outcome::Notice(notice) =
        commands::execute(commands::Request::Jobs, &handle, &summary, None, None, None)
            .await
            .unwrap()
    else {
        panic!("jobs notice expected")
    };
    assert!(notice.contains("turn"));

    app.key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut jobs,
    );
    app.paste("new draft");
    collect(&mut app, &mut jobs).await;
    assert_eq!(app.draft_mut().text, "new draft");
    assert_eq!(
        app.pane().unwrap().recalled.as_deref(),
        Some("last queued 界")
    );
    assert_eq!(handle.queue(thread.clone()).await.unwrap().len(), 1);
    app.draft_mut().clear();
    app.key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut jobs,
    );
    assert_eq!(app.draft_mut().text, "last queued 界");
    assert!(jobs.is_empty());
    let turn = retained
        .iter()
        .find(|job| job.kind == bob_kernel::JobKind::Turn)
        .unwrap();
    commands::execute(
        commands::Request::StopJob(turn.id.clone()),
        &handle,
        &summary,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn paste_placeholder_submits_full_content_and_preserves_new_focus() {
    let workspace = Workspace::new();
    let (handle, root, _) = start(&workspace, false, true).await;
    let initial = handle
        .transcript(Some(root.clone()), None, 80)
        .await
        .unwrap();
    let summary = initial
        .threads
        .iter()
        .find(|summary| summary.id == root)
        .unwrap();
    stop_and_settle(&handle, summary).await;
    let Outcome::Session(receipt) = commands::execute(
        commands::Request::New,
        &handle,
        summary,
        Some(workspace.0.clone()),
        None,
        None,
    )
    .await
    .unwrap() else {
        panic!("new session expected")
    };
    let thread = receipt.thread;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    let content = format!("pasted source\n{}", "界".repeat(200));
    app.paste(&content);
    assert!(app.draft_mut().text.starts_with("[Pasted text #1"));
    let mut jobs = JoinSet::new();
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut jobs);
    assert_eq!(jobs.len(), 1);
    app.focus = Focus::Sidebar;
    collect(&mut app, &mut jobs).await;
    assert_eq!(app.focus, Focus::Sidebar);
    assert!(app.draft_mut().text.is_empty());
    let view = handle.transcript(Some(thread), None, 80).await.unwrap();
    assert!(view
        .entries
        .iter()
        .any(|entry| entry.kind == EntryKind::Submission && entry.text == content));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn new_command_preserves_policy_not_session_grants_and_usage_keeps_both_scopes() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let summary = view
        .threads
        .iter()
        .find(|summary| summary.id == thread)
        .unwrap();
    let session = handle.session(summary.session.clone()).await.unwrap();
    let mut permissions = session.permissions;
    permissions.mode = bob_kernel::PermissionMode::AutoAccept;
    permissions.rules.push(bob_kernel::PermissionRule {
        tool: "bash".into(),
        decision: bob_kernel::RuleDecision::Deny,
    });
    permissions.grants.push(bob_kernel::PermissionGrant {
        tool: "write_file".into(),
        shell_command: None,
        path_prefix: None,
        command: None,
    });
    handle
        .command(Command::SetPermissions {
            id: CommandId::new(),
            session: summary.session.clone(),
            permissions: permissions.clone(),
            expected_revision: session.policy_revision,
        })
        .await
        .unwrap();
    stop_and_settle(&handle, summary).await;
    let Outcome::Session(receipt) = commands::execute(
        commands::Request::New,
        &handle,
        summary,
        Some(workspace.0.clone()),
        None,
        None,
    )
    .await
    .unwrap() else {
        panic!("new session expected");
    };
    assert_ne!(receipt.session, summary.session);
    let fresh = handle.session(receipt.session).await.unwrap();
    let original = handle.session(summary.session.clone()).await.unwrap();
    assert_eq!(
        serde_json::to_value(original.permissions).unwrap(),
        serde_json::to_value(&permissions).unwrap()
    );
    permissions.grants.clear();
    assert_eq!(
        serde_json::to_value(fresh.permissions).unwrap(),
        serde_json::to_value(permissions).unwrap()
    );
    assert_eq!(fresh.profile.model, "fixture");
    assert_eq!(
        fresh.workspace,
        workspace.0.canonicalize().unwrap().to_string_lossy()
    );
    let Outcome::Notice(usage) = commands::execute(
        commands::Request::Usage,
        &handle,
        summary,
        Some(workspace.0.clone()),
        None,
        None,
    )
    .await
    .unwrap() else {
        panic!("usage notice expected");
    };
    assert!(usage.contains("Session usage"));
    assert!(usage.contains("All-time usage"));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn theme_command_persists_only_in_the_selected_workspace() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let summary = view
        .threads
        .iter()
        .find(|summary| summary.id == thread)
        .unwrap();
    std::fs::write(
        workspace.0.join(".bob.config.toml"),
        "model = \"fixture\"\n",
    )
    .unwrap();
    let Outcome::Theme(theme) = commands::execute(
        commands::Request::Theme(Some("light".into())),
        &handle,
        summary,
        Some(workspace.0.clone()),
        None,
        None,
    )
    .await
    .unwrap() else {
        panic!("theme outcome expected");
    };
    assert_eq!(theme, "light");
    let config = std::fs::read_to_string(workspace.0.join(".bob.config.toml")).unwrap();
    assert!(config.contains("theme = \"light\""));
    assert!(config.contains("model = \"fixture\""));
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn kernel_grant_option_survives_atomic_ui_confirmation() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, false).await;
    let view = until(&handle, &thread, |view| !view.decisions.is_empty()).await;
    let decision = &view.decisions[0];
    let key = DecisionKey::from(decision);
    let session_id = view
        .threads
        .iter()
        .find(|summary| summary.id == thread)
        .unwrap()
        .session
        .clone();
    let options = handle
        .approval_options(decision.operation.clone(), decision.revision)
        .await
        .unwrap();
    let index = options
        .options
        .iter()
        .position(|option| option.grant.is_some())
        .expect("ordinary write should offer scoped grant");
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    app.reply(Reply::ApprovalOptions {
        key: key.clone(),
        result: Ok(options),
    });
    assert_eq!(app.focus, Focus::Permission);
    assert_eq!(
        app.pane().unwrap().permission.as_ref().unwrap().choice,
        Some(0)
    );
    app.draft_mut().insert("keep grant draft");
    let mut jobs = JoinSet::new();
    app.key(
        KeyEvent::new(
            KeyCode::Char(char::from(b'1' + index as u8)),
            KeyModifiers::NONE,
        ),
        &mut jobs,
    );
    assert!(!jobs.is_empty());
    assert!(app.take_option(index).is_none());
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut jobs);
    collect(&mut app, &mut jobs).await;
    assert_eq!(app.draft_mut().text, "keep grant draft");
    let session = handle.session(session_id).await.unwrap();
    assert!(session
        .permissions
        .grants
        .iter()
        .any(|grant| grant.tool == "write_file"));
    let view = until(&handle, &thread, |view| {
        view.entries
            .iter()
            .any(|entry| entry.kind == EntryKind::ToolError)
    })
    .await;
    let operation = view
        .entries
        .iter()
        .find(|entry| entry.kind == EntryKind::ToolError)
        .unwrap()
        .operation
        .clone()
        .unwrap();
    let mut jobs = JoinSet::new();
    app.open_inspector(
        InspectionTarget::ToolOutput(operation),
        0,
        Default::default(),
        &mut jobs,
    );
    collect(&mut app, &mut jobs).await;
    assert_eq!(
        app.pane().unwrap().inspector.as_ref().unwrap().text,
        "retained failure body"
    );
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn kernel_submission_approval_and_failed_or_unknown_output_render_end_to_end() {
    for interrupted in [false, true] {
        let workspace = Workspace::new();
        let (handle, thread, executor) = start(&workspace, interrupted, false).await;
        let view = until(&handle, &thread, |v| v.decisions.len() == 1).await;
        assert!(view.operations.is_empty());
        assert!(view.entries.iter().any(|e| e.kind == EntryKind::Submission));
        assert!(view.entries.iter().any(|e| e
            .tool
            .as_ref()
            .is_some_and(|t| t.state == OperationState::Approval)));
        let mut app = App::new(80);
        app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
        let (buffer, text) = render(&mut app);
        let row = text
            .lines()
            .position(|line| line.contains("Please apply"))
            .unwrap() as u16;
        assert_eq!(buffer[(4, row)].bg, Palette::INPUT_BG());
        assert!(text.contains("approval needed"));
        assert!(text.contains("Allow Create?"));
        assert!(text.contains("src/main.rs"));
        assert!(text.contains("+ fn main() {}"));
        assert!(!text.contains("Prepared creation"));
        assert!(!text.contains("Path does not yet exist"));
        let mut jobs = JoinSet::new();
        assert_eq!(app.focus, Focus::Permission);
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut jobs);
        app.key(
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
            &mut jobs,
        );
        assert!(jobs.is_empty());
        assert_eq!(executor.executions.load(Ordering::SeqCst), 0);
        app.draft_mut().insert("unsent separate draft");
        let key = DecisionKey::from(&app.pane().unwrap().permission.as_ref().unwrap().decision);
        let options = handle
            .approval_options(key.operation.clone(), key.revision)
            .await
            .unwrap();
        app.reply(Reply::ApprovalOptions {
            key,
            result: Ok(options),
        });
        assert_eq!(app.focus, Focus::Permission);
        assert_eq!(
            app.pane().unwrap().permission.as_ref().unwrap().choice,
            Some(0)
        );
        for code in [KeyCode::Enter, KeyCode::Char('1'), KeyCode::Esc] {
            let mut repeat = KeyEvent::new(code, KeyModifiers::NONE);
            repeat.kind = crossterm::event::KeyEventKind::Repeat;
            app.key(repeat, &mut jobs);
        }
        assert!(jobs.is_empty());
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut jobs);
        assert!(!jobs.is_empty());
        assert!(app.take_decision(true).is_none());
        collect(&mut app, &mut jobs).await;
        let view = until(&handle, &thread, |v| {
            v.threads.iter().any(|t| {
                t.id == thread
                    && t.state
                        == if interrupted {
                            ThreadState::Interrupted
                        } else {
                            ThreadState::Completed
                        }
            })
        })
        .await;
        let error = view
            .entries
            .iter()
            .find(|e| e.kind == EntryKind::ToolError)
            .unwrap();
        let sequence = error.sequence;
        let operation = error.operation.clone().unwrap();
        let call_sequence = view
            .entries
            .iter()
            .find(|e| e.kind == EntryKind::ToolCall && e.operation.as_ref() == Some(&operation))
            .unwrap()
            .sequence;
        assert_eq!(
            error.tool.as_ref().unwrap().state,
            if interrupted {
                OperationState::Unknown
            } else {
                OperationState::Failed
            }
        );
        app.reply(Reply::View {
            thread: thread.clone(),
            generation: 0,
            result: Ok(view),
        });
        let (buffer, text) = render(&mut app);
        assert!(text.contains("retained failure body"), "{text}");
        assert!(text.contains(if interrupted {
            "unknown outcome"
        } else {
            "failed"
        }));
        let row = text
            .lines()
            .position(|line| line.contains("retained failure body"))
            .unwrap() as u16;
        assert_eq!(buffer[(8, row)].fg, Palette::ERROR());
        assert_eq!(app.draft_mut().text, "unsent separate draft");
        assert_eq!(executor.executions.load(Ordering::SeqCst), 1);
        app.inspect(sequence, 0, &mut jobs);
        collect(&mut app, &mut jobs).await;
        assert_eq!(
            app.pane().unwrap().inspector.as_ref().unwrap().text,
            "retained failure body"
        );
        app.dismiss_inspector();
        let historical = handle
            .transcript(Some(thread.clone()), Some(call_sequence + 1), 1)
            .await
            .unwrap();
        assert_eq!(historical.entries.len(), 1);
        assert_eq!(historical.entries[0].kind, EntryKind::ToolCall);
        app.pane_mut().unwrap().before = Some(call_sequence + 1);
        app.pane_mut().unwrap().loaded_before = Some(Some(call_sequence + 1));
        app.reply(Reply::View {
            thread: thread.clone(),
            generation: 0,
            result: Ok(historical),
        });
        let target = app.tool_inspection(&operation).unwrap();
        assert!(matches!(&target, InspectionTarget::ToolOutput(id) if id == &operation));
        app.open_inspector(target, 0, Default::default(), &mut jobs);
        collect(&mut app, &mut jobs).await;
        assert_eq!(
            app.pane().unwrap().inspector.as_ref().unwrap().text,
            "retained failure body"
        );
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn permission_mode_shortcut_updates_policy_without_confirmation_notice() {
    let workspace = Workspace::new();
    let (handle, thread, _) = start(&workspace, false, true).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    let session = app
        .threads
        .iter()
        .find(|summary| summary.id == thread)
        .unwrap()
        .session
        .clone();
    let mut jobs = JoinSet::new();
    for expected in [
        bob_kernel::PermissionMode::AutoAccept,
        bob_kernel::PermissionMode::Plan,
        bob_kernel::PermissionMode::Normal,
    ] {
        app.key(
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            &mut jobs,
        );
        collect(&mut app, &mut jobs).await;
        assert_eq!(
            handle
                .session(session.clone())
                .await
                .unwrap()
                .permissions
                .mode,
            expected
        );
        assert!(app.pane().unwrap().notices.is_empty());
        assert!(!render(&mut app).1.contains("permissions updated"));
    }
    handle.shutdown().await.unwrap();
    app.key(
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        &mut jobs,
    );
    collect(&mut app, &mut jobs).await;
    assert!(app.pane().unwrap().notice.contains("Shortcut failed"));
}

#[tokio::test]
async fn permission_escape_and_numbered_deny_act_immediately_without_executing() {
    for escape in [false, true] {
        let workspace = Workspace::new();
        let (handle, thread, executor) = start(&workspace, false, false).await;
        let view = until(&handle, &thread, |view| !view.decisions.is_empty()).await;
        let decision = &view.decisions[0];
        let key = DecisionKey::from(decision);
        let options = handle
            .approval_options(key.operation.clone(), key.revision)
            .await
            .unwrap();
        let deny = options
            .options
            .iter()
            .position(|option| !option.approve)
            .unwrap();
        let mut app = App::new(80);
        app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
        app.reply(Reply::ApprovalOptions {
            key,
            result: Ok(options),
        });
        app.draft_mut().insert("/quit");
        let mut jobs = JoinSet::new();
        let code = if escape {
            KeyCode::Esc
        } else {
            KeyCode::Char(char::from(b'1' + deny as u8))
        };
        assert!(!app.key(KeyEvent::new(code, KeyModifiers::NONE), &mut jobs));
        assert!(!jobs.is_empty());
        assert!(app.take_decision(false).is_none());
        collect(&mut app, &mut jobs).await;
        let view = until(&handle, &thread, |view| {
            view.decisions.is_empty()
                && view
                    .threads
                    .iter()
                    .any(|summary| summary.id == thread && summary.state == ThreadState::Completed)
        })
        .await;
        app.reply(Reply::View {
            thread,
            generation: 0,
            result: Ok(view),
        });
        assert_eq!(executor.executions.load(Ordering::SeqCst), 0);
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        assert!(!app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut jobs));
        assert_eq!(app.draft_mut().text, "/quit");
        assert!(jobs.is_empty());
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn escape_stops_actual_active_run_without_touching_draft() {
    let workspace = Workspace::new();
    let (handle, thread, executor) = start(&workspace, false, true).await;
    let view = until(&handle, &thread, |v| {
        executor.calls.load(Ordering::SeqCst) > 0
            && v.threads
                .iter()
                .any(|t| t.id == thread && t.state == ThreadState::Active)
    })
    .await;
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    app.draft_mut().insert("keep while stopping");
    assert!(render(&mut app).1.contains("Working"));
    let mut jobs = JoinSet::new();
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut jobs);
    collect(&mut app, &mut jobs).await;
    let view = until(&handle, &thread, |v| {
        v.threads
            .iter()
            .any(|t| t.id == thread && t.state == ThreadState::Cancelled)
    })
    .await;
    app.reply(Reply::View {
        thread,
        generation: 0,
        result: Ok(view),
    });
    assert_eq!(app.draft_mut().text, "keep while stopping");
    assert!(render(&mut app).1.contains("cancelled"));
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut jobs);
    assert!(jobs.is_empty());
    handle.shutdown().await.unwrap();
}
