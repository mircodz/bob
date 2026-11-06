use super::*;
use async_trait::async_trait;
use bob_kernel::{
    EffectClass, Executor, Invocation, PreparedTool, RequestSpec, Runtime, ToolDescriptor,
    ToolOutcome, ToolResultPresentation,
};
use bob_types::{Completion, ContentBlock, Message, Role, StopReason, ToolSpec, Usage};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::{
    sync::{watch, Semaphore},
    time::timeout,
};

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-output-parity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct RepositoryFixture {
    name: String,
    calls: AtomicUsize,
    gate: Arc<Semaphore>,
}

fn report(name: &str) -> String {
    let mut text = format!("{name} readable repository report\n");
    for line in 1..=2400 {
        text.push_str(&format!("src/日本語.rs:{line}: matching repository line\n"));
    }
    text.push_str("READABLE_OUTPUT_TAIL\n");
    text
}

#[async_trait]
impl Executor for RepositoryFixture {
    fn models(&self) -> Vec<bob_kernel::ModelInfo> {
        vec![bob_kernel::ModelInfo {
            id: "fixture".into(),
            context_window: 100_000,
        }]
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            effect: EffectClass::Read,
            spec: ToolSpec {
                name: self.name.clone(),
                description: "Repository output presentation fixture".into(),
                input_schema: json!({"type":"object","properties":{}}),
            },
        }]
    }

    async fn generate(
        &self,
        _request: RequestSpec,
        _cancel: watch::Receiver<bool>,
        _progress: bob_kernel::ModelProgress,
    ) -> Result<Completion> {
        let (content, stop_reason) = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            (
                vec![ContentBlock::ToolUse {
                    id: "repository-fixture-call".into(),
                    name: self.name.clone(),
                    input: json!({}),
                }],
                StopReason::ToolUse,
            )
        } else {
            (
                vec![ContentBlock::Text {
                    text: "fixture complete".into(),
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
            specification: json!({}),
            preview: "Repository fixture".into(),
            presentation: None,
        })
    }

    async fn execute(
        &self,
        _tool: PreparedTool,
        _cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        self.gate.acquire().await.unwrap().forget();
        Ok(ToolOutcome {
            text: json!({"model_only_json": ["not human output"]}).to_string(),
            is_error: false,
            interrupted: false,
            presentation: Some(ToolResultPresentation {
                summary: Some("2400 repository matches".into()),
                body_preview: Some("src/日本語.rs:1: matching repository line".into()),
                exit_code: None,
            }),
        })
    }

    fn display_text(&self, _tool: &PreparedTool, _outcome: &ToolOutcome) -> Result<Option<String>> {
        Ok(Some(report(&self.name)))
    }
}

async fn start(workspace: &Workspace, name: &str, gate: Arc<Semaphore>) -> (Handle, ThreadId) {
    let handle = Runtime::open(
        &workspace.0.join("kernel.sqlite"),
        Arc::new(RepositoryFixture {
            name: name.into(),
            calls: AtomicUsize::new(0),
            gate,
        }),
    )
    .await
    .unwrap();
    let receipt = handle
        .command(Command::CreateSession {
            id: CommandId::new(),
            workspace: workspace.0.to_string_lossy().into_owned(),
            profile: Profile {
                model: "fixture".into(),
                instructions: "Repository fixture".into(),
                read_only: false,
                max_requests: 4,
                reasoning: ReasoningEffort::Off,
            },
            policy: Policy {
                yolo: true,
                ..Default::default()
            },
        })
        .await
        .unwrap();
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: receipt.thread.clone(),
            text: "Run the repository fixture".into(),
        })
        .await
        .unwrap();
    (handle, receipt.thread)
}

async fn until(handle: &Handle, thread: &ThreadId, state: OperationState) -> View {
    timeout(Duration::from_secs(10), async {
        loop {
            let view = handle
                .transcript(Some(thread.clone()), None, 80)
                .await
                .unwrap();
            assert!(view.error.is_none(), "{:?}", view.error);
            if view
                .entries
                .iter()
                .any(|entry| entry.tool.as_ref().is_some_and(|tool| tool.state == state))
                && (state != OperationState::Done
                    || view.entries.iter().any(|entry| {
                        entry.kind == EntryKind::Assistant && entry.text == "fixture complete"
                    }))
            {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}

async fn next(jobs: &mut JoinSet<Reply>) -> Reply {
    timeout(Duration::from_secs(10), jobs.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

fn is_output(reply: &Reply) -> bool {
    match reply {
        Reply::ToolOutput { .. } => true,
        Reply::Runtime { reply, .. } => is_output(reply),
        _ => false,
    }
}

async fn drain(app: &mut App, jobs: &mut JoinSet<Reply>) -> usize {
    let mut outputs = 0;
    while !jobs.is_empty() {
        let reply = next(jobs).await;
        outputs += usize::from(is_output(&reply));
        app.reply(reply);
    }
    outputs
}

fn operation(view: &View) -> OperationId {
    view.entries
        .iter()
        .find_map(|entry| entry.tool.as_ref())
        .unwrap()
        .id
        .clone()
}

#[tokio::test]
async fn repository_expansion_and_inspector_use_bounded_readable_pages_not_model_json() {
    for name in ["grep", "glob", "list_dir"] {
        let workspace = Workspace::new();
        let (handle, thread) = start(&workspace, name, Arc::new(Semaphore::new(1))).await;
        let view = until(&handle, &thread, OperationState::Done).await;
        let id = operation(&view);
        let sequence = view
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::ToolResult)
            .unwrap()
            .sequence;
        let mut app = App::new(80);
        app.reply(Reply::Ready(Ok((handle.clone(), view, thread))));
        let mut jobs = JoinSet::new();
        app.refresh(&mut jobs);
        assert_eq!(drain(&mut app, &mut jobs).await, 0);
        assert!(app.pane().unwrap().expanded_outputs.is_empty());
        assert!(app.pane().unwrap().pending_outputs.is_empty());
        let collapsed = render::transcript(app.pane().unwrap(), true, 120)
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!collapsed.contains("model_only_json"));
        assert!(!collapsed.contains("READABLE_OUTPUT_TAIL"));

        app.toggle_tool(id.clone(), &mut jobs);
        assert!(app.pane().unwrap().pending_outputs.contains_key(&id));
        assert_eq!(drain(&mut app, &mut jobs).await, 1);
        let page = &app.pane().unwrap().expanded_outputs[&id];
        assert!(page.text.len() <= 16384);
        assert!(page.text.len() > 16000);
        assert_eq!(page.next_offset, Some(page.text.len()));
        assert_eq!(page.total_bytes, report(name).len());
        assert!(report(name).starts_with(&page.text));
        assert!(!page.text.contains("model_only_json"));
        assert!(app.pane().unwrap().pending_outputs.is_empty());
        app.refresh(&mut jobs);
        assert_eq!(drain(&mut app, &mut jobs).await, 0);

        let target = app.tool_inspection(&id).unwrap();
        assert_eq!(target, InspectionTarget::ToolOutput(id.clone()));
        app.open_inspector(target, 0, Default::default(), &mut jobs);
        let mut full = String::new();
        loop {
            drain(&mut app, &mut jobs).await;
            let inspector = app.pane().unwrap().inspector.as_ref().unwrap();
            assert!(matches!(inspector.target, InspectionTarget::ToolOutput(_)));
            assert!(inspector.text.len() <= 16384);
            assert!(!inspector.text.contains("model_only_json"));
            full.push_str(&inspector.text);
            let Some((target, offset, previous)) = inspector.navigation(true) else {
                break;
            };
            app.open_inspector(target, offset, previous, &mut jobs);
        }
        assert_eq!(full, report(name));
        app.inspect(sequence, 0, &mut jobs);
        drain(&mut app, &mut jobs).await;
        assert!(app
            .pane()
            .unwrap()
            .inspector
            .as_ref()
            .unwrap()
            .text
            .contains("model_only_json"));
        app.toggle_tool(id.clone(), &mut jobs);
        assert!(app.pane().unwrap().expanded_outputs.is_empty());
        handle.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn expanded_running_repository_output_loads_once_after_authoritative_settlement() {
    let workspace = Workspace::new();
    let gate = Arc::new(Semaphore::new(0));
    let (handle, thread) = start(&workspace, "grep", gate.clone()).await;
    let view = until(&handle, &thread, OperationState::Started).await;
    let id = operation(&view);
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    let mut jobs = JoinSet::new();
    app.toggle_tool(id.clone(), &mut jobs);
    assert!(jobs.is_empty());
    for _ in 0..3 {
        app.refresh(&mut jobs);
        assert_eq!(drain(&mut app, &mut jobs).await, 0);
    }
    assert!(app.pane().unwrap().expanded.contains(&id));
    assert!(app.pane().unwrap().pending_outputs.is_empty());
    gate.add_permits(1);
    let view = until(&handle, &thread, OperationState::Done).await;
    app.reply(Reply::View {
        thread,
        generation: app.pane().unwrap().generation,
        result: Ok(view),
    });
    for _ in 0..3 {
        app.refresh(&mut jobs);
    }
    assert_eq!(app.pane().unwrap().pending_outputs.len(), 1);
    assert_eq!(drain(&mut app, &mut jobs).await, 1);
    assert!(app.pane().unwrap().expanded_outputs[&id]
        .next_offset
        .is_some());
    for _ in 0..3 {
        app.refresh(&mut jobs);
        assert_eq!(drain(&mut app, &mut jobs).await, 0);
    }
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn collapsed_reopened_evicted_and_replaced_runtime_ignore_late_output_replies() {
    let workspace = Workspace::new();
    let (handle, thread) = start(&workspace, "glob", Arc::new(Semaphore::new(1))).await;
    let view = until(&handle, &thread, OperationState::Done).await;
    let id = operation(&view);
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((
        handle.clone(),
        view.clone(),
        thread.clone(),
    ))));
    let mut jobs = JoinSet::new();

    app.toggle_tool(id.clone(), &mut jobs);
    let stale = next(&mut jobs).await;
    app.toggle_tool(id.clone(), &mut jobs);
    app.reply(stale);
    assert!(app.pane().unwrap().expanded_outputs.is_empty());
    assert!(app.pane().unwrap().output_errors.is_empty());

    app.toggle_tool(id.clone(), &mut jobs);
    let stale = next(&mut jobs).await;
    app.toggle_tool(id.clone(), &mut jobs);
    app.toggle_tool(id.clone(), &mut jobs);
    let epoch = app.pane().unwrap().pending_outputs[&id];
    app.reply(stale);
    assert_eq!(app.pane().unwrap().pending_outputs[&id], epoch);
    assert!(app.pane().unwrap().expanded_outputs.is_empty());
    assert_eq!(drain(&mut app, &mut jobs).await, 1);

    app.toggle_tool(id.clone(), &mut jobs);
    app.toggle_tool(id.clone(), &mut jobs);
    let stale = next(&mut jobs).await;
    for index in 0..9 {
        app.select(ThreadId(format!("eviction-pane-{index}")));
        app.pane_mut().unwrap().view = Some(view.clone());
    }
    assert!(app
        .panes
        .get(&thread)
        .is_none_or(|pane| pane.view.is_none()));
    app.select(thread.clone());
    app.pane_mut().unwrap().view = Some(view.clone());
    app.toggle_tool(id.clone(), &mut jobs);
    let epoch = app.pane().unwrap().pending_outputs[&id];
    app.reply(stale);
    assert_eq!(app.pane().unwrap().pending_outputs[&id], epoch);
    assert!(app.pane().unwrap().expanded_outputs.is_empty());
    drain(&mut app, &mut jobs).await;

    app.toggle_tool(id.clone(), &mut jobs);
    app.toggle_tool(id.clone(), &mut jobs);
    let stale = next(&mut jobs).await;
    app.runtime_epoch += 1;
    app.toggle_tool(id.clone(), &mut jobs);
    app.toggle_tool(id.clone(), &mut jobs);
    app.reply(stale);
    assert!(app.pane().unwrap().expanded_outputs.is_empty());
    assert!(app.pane().unwrap().pending_outputs.contains_key(&id));
    drain(&mut app, &mut jobs).await;
    assert!(app.pane().unwrap().expanded_outputs.contains_key(&id));
    handle.shutdown().await.unwrap();

    app.toggle_tool(id.clone(), &mut jobs);
    app.toggle_tool(id.clone(), &mut jobs);
    assert_eq!(drain(&mut app, &mut jobs).await, 1);
    assert!(app.pane().unwrap().output_errors.contains_key(&id));
    assert!(app.pane().unwrap().pending_outputs.is_empty());
    for _ in 0..3 {
        app.refresh(&mut jobs);
        assert_eq!(drain(&mut app, &mut jobs).await, 0);
    }
    app.toggle_tool(id, &mut jobs);
    assert!(app.pane().unwrap().output_errors.is_empty());
}
