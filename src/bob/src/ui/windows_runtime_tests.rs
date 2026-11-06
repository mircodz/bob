use super::*;
use async_trait::async_trait;
use bob_kernel::{
    Command, CommandId, EffectClass, EntryKind, Executor, Handle, Invocation, ModelInfo, Policy,
    PreparedTool, Profile, RequestSpec, Runtime, ThreadId, ThreadState, ToolDescriptor,
    ToolOutcome, View,
};
use bob_types::{
    Completion, ContentBlock, Message, ReasoningEffort, Role, StopReason, ToolSpec, Usage,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::watch;

const READY_FIXTURE: &str = "ui::windows_tests::runtime::ready_runtime_fixture";
const ISOLATED_FIXTURE: &str = "ui::windows_tests::runtime::isolated_live_fixture";

#[test]
fn isolated_live_fixture() {
    let Some(directory) = std::env::var_os(FIXTURE_DIR) else {
        return;
    };
    let directory = PathBuf::from(directory);
    assert!(
        ConsoleInput::open().is_err(),
        "managed child inherited the UI console"
    );
    publish(&directory.join("isolated-ready"), b"ready");
    let start = Instant::now();
    while !directory.join("isolated-stop").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "isolated child release timed out"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Default)]
struct ExecutorFixture {
    calls: AtomicUsize,
    executions: AtomicUsize,
}

#[async_trait]
impl Executor for ExecutorFixture {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "native-fixture".into(),
            context_window: 100_000,
        }]
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            effect: EffectClass::Write,
            spec: ToolSpec {
                name: "write_file".into(),
                description: "Synthetic native UI acceptance effect".into(),
                input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
            },
        }]
    }
    async fn generate(
        &self,
        _request: RequestSpec,
        _cancel: watch::Receiver<bool>,
        _progress: bob_kernel::ModelProgress,
    ) -> anyhow::Result<Completion> {
        let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
        let content = if first {
            vec![ContentBlock::ToolUse {
                id: "native-tool".into(),
                name: "write_file".into(),
                input: json!({"path":"synthetic.txt"}),
            }]
        } else {
            vec![ContentBlock::Text {
                text: "Native fixture finished".into(),
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
    async fn prepare(&self, invocation: Invocation) -> anyhow::Result<PreparedTool> {
        Ok(PreparedTool {
            invocation,
            specification: json!({"synthetic":true}),
            preview: "Synthetic permission fixture".into(),
            presentation: None,
        })
    }
    async fn execute(
        &self,
        _tool: PreparedTool,
        _cancel: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutcome {
            text: "Synthetic effect completed".into(),
            is_error: false,
            interrupted: false,
            presentation: None,
        })
    }
}

async fn until(handle: &Handle, thread: &ThreadId, predicate: impl Fn(&View) -> bool) -> View {
    timeout(EVENT_TIMEOUT, async {
        loop {
            let view = handle
                .transcript(Some(thread.clone()), None, 80)
                .await
                .unwrap();
            assert!(view.error.is_none(), "{:?}", view.error);
            if predicate(&view) {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("ready kernel fixture did not settle")
}

async fn collect(app: &mut App, jobs: &mut JoinSet<Reply>) {
    app.reply(
        timeout(EVENT_TIMEOUT, jobs.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
    );
}

#[tokio::test]
async fn ready_runtime_fixture() {
    let Some(directory) = std::env::var_os(FIXTURE_DIR) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let mut guard = TerminalGuard::activate().unwrap();
    let input = ConsoleInput::open().unwrap();
    let baseline = input.mode().unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            ISOLATED_FIXTURE,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FIXTURE_DIR, &directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut isolated = ManagedProcess::spawn(&mut command).unwrap();
    timeout(EVENT_TIMEOUT, async {
        while !directory.join("isolated-ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("isolated child did not become ready");
    let mut events = guard.events();
    for round in 0..3 {
        let database = directory.join(format!("ready-{round}.sqlite"));
        let executor = Arc::new(ExecutorFixture::default());
        let handle = Runtime::open(&database, executor.clone()).await.unwrap();
        let receipt = handle
            .command(Command::CreateSession {
                id: CommandId::new(),
                workspace: directory.to_string_lossy().into_owned(),
                profile: Profile {
                    model: "native-fixture".into(),
                    instructions: "Synthetic native input fixture".into(),
                    read_only: false,
                    max_requests: 8,
                    reasoning: ReasoningEffort::Off,
                },
                policy: Policy::default(),
            })
            .await
            .unwrap();
        let thread = receipt.thread;
        let view = handle
            .transcript(Some(thread.clone()), None, 80)
            .await
            .unwrap();
        let mut app = App::new(80);
        app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
        let mut jobs = JoinSet::new();
        let first = ready_event(
            &mut events,
            &input,
            baseline,
            &directory.join(format!("submit-{round}")),
            b"ready",
        )
        .await;
        let mut next = Some(first);
        let mut enters = 0;
        while enters < 2 {
            let event = match next.take() {
                Some(event) => event,
                None => timeout(EVENT_TIMEOUT, input_event(&mut events))
                    .await
                    .unwrap(),
            };
            let Event::Key(key) = event else {
                panic!("expected native key: {event:?}");
            };
            assert!(!app.key(key, &mut jobs));
            if key.code == KeyCode::Enter {
                enters += 1;
                assert_eq!(
                    app.draft_mut().text,
                    "native a",
                    "draft cleared before acknowledgement"
                );
                assert_eq!(jobs.len(), 1, "duplicate Enter launched another submission");
                if enters == 1 {
                    publish(&directory.join(format!("resubmit-{round}")), b"ready");
                }
            }
        }
        collect(&mut app, &mut jobs).await;
        assert_eq!(app.draft_mut().text, "");
        app.draft_mut().insert("preserve é 界 🦀 draft");
        let revision = app.draft_mut().revision;
        app.sidebar = true;
        app.focus = Focus::Sidebar;
        let view = until(&handle, &thread, |view| view.decisions.len() == 1).await;
        assert_eq!(
            view.entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Submission)
                .count(),
            1
        );
        let decision = DecisionKey::from(&view.decisions[0]);
        app.reply(Reply::View {
            thread: thread.clone(),
            generation: 0,
            result: Ok(view),
        });
        assert_eq!(
            app.focus,
            Focus::Permission,
            "permission did not preempt sidebar focus"
        );
        app.reply(Reply::ApprovalOptions {
            key: decision.clone(),
            result: Ok(handle
                .approval_options(decision.operation.clone(), decision.revision)
                .await
                .unwrap()),
        });
        assert_eq!(
            app.pane().unwrap().permission.as_ref().unwrap().choice,
            Some(0)
        );
        for code in [KeyCode::Enter, KeyCode::Char('1'), KeyCode::Esc] {
            let mut repeat = KeyEvent::new(code, KeyModifiers::NONE);
            repeat.kind = KeyEventKind::Repeat;
            app.key(repeat, &mut jobs);
        }
        app.paste("1\r\n/approve\r\n");
        assert!(
            jobs.is_empty(),
            "typed repeat or Event::Paste approved a prompt"
        );
        // These are application-event regressions, not claims about native paste detection.
        for (width, height) in [(100, 30), (12, 5)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| super::super::render::draw(frame, &mut app))
                .unwrap();
        }
        let first = ready_event(
            &mut events,
            &input,
            baseline,
            &directory.join(format!("decision-{round}")),
            b"ready",
        )
        .await;
        let mut next = Some(first);
        for attempt in 0..2 {
            let event = match next.take() {
                Some(event) => event,
                None => timeout(EVENT_TIMEOUT, input_event(&mut events))
                    .await
                    .unwrap(),
            };
            let expected = match round {
                0 => KeyCode::Char('1'),
                1 => KeyCode::Enter,
                _ => KeyCode::Esc,
            };
            assert!(!app.key(key(event, expected, KeyModifiers::NONE), &mut jobs));
            assert_eq!(
                jobs.len(),
                1,
                "duplicate decision key launched another decision"
            );
            assert_eq!(app.draft_mut().text, "preserve é 界 🦀 draft");
            assert_eq!(app.draft_mut().revision, revision);
            if attempt == 0 {
                publish(&directory.join(format!("redecision-{round}")), b"ready");
            }
        }
        collect(&mut app, &mut jobs).await;
        let view = until(&handle, &thread, |view| {
            view.threads
                .iter()
                .any(|item| item.id == thread && item.state == ThreadState::Completed)
        })
        .await;
        assert!(view.decisions.is_empty());
        assert_eq!(
            view.entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Submission)
                .count(),
            1
        );
        assert_eq!(
            executor.executions.load(Ordering::SeqCst),
            usize::from(round != 2)
        );
        assert_eq!(app.draft_mut().text, "preserve é 界 🦀 draft");
        handle.shutdown().await.unwrap();
        let connection = rusqlite::Connection::open_with_flags(
            &database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let count: i64 = connection
            .query_row("SELECT count(*) FROM decisions", [], |row| row.get(0))
            .unwrap();
        let state: String = connection
            .query_row("SELECT state FROM decisions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(state, if round == 2 { "denied" } else { "approved" });
    }
    publish(&directory.join("isolated-stop"), b"stop");
    let output = timeout(EVENT_TIMEOUT, isolated.collect_output(4096))
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success(), "{}", output.stderr.text());
    assert_eq!(input.mode().unwrap(), baseline);
    drop(events);
    guard.restore().unwrap();
    publish(&directory.join("done"), b"restored");
}

#[test]
fn conpty_ready_runtime_submits_once_and_focuses_numeric_default_and_deny() {
    let directory = FixtureDirectory::new();
    let mut fixture = ConptyFixture::launch_test(&directory.0, READY_FIXTURE);
    let result: io::Result<()> = (|| {
        for round in 0..3 {
            fixture.wait_marker(&directory.0.join(format!("submit-{round}")))?;
            let resized = unsafe {
                windows_sys::Win32::System::Console::ResizePseudoConsole(
                    fixture.console.as_ref().unwrap().0,
                    if round == 1 {
                        COORD { X: 40, Y: 10 }
                    } else {
                        COORD { X: 100, Y: 30 }
                    },
                )
            };
            if resized < 0 {
                return Err(io::Error::other(format!(
                    "ResizePseudoConsole: {resized:#x}"
                )));
            }
            fixture.input.as_mut().unwrap().write_all(b"native a\r")?;
            fixture.wait_marker(&directory.0.join(format!("resubmit-{round}")))?;
            fixture.input.as_mut().unwrap().write_all(b"\r")?;
            let decision = match round {
                0 => b"1",
                1 => b"\r",
                _ => b"\x1b",
            };
            fixture.wait_marker(&directory.0.join(format!("decision-{round}")))?;
            fixture.input.as_mut().unwrap().write_all(decision)?;
            fixture.wait_marker(&directory.0.join(format!("redecision-{round}")))?;
            fixture.input.as_mut().unwrap().write_all(decision)?;
        }
        fixture.wait_exit(&directory.0)
    })();
    let output = fixture.close();
    assert!(
        result.is_ok(),
        "{result:?}\nConPTY output:\n{}",
        String::from_utf8_lossy(&output)
    );
}
