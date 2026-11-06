use super::tests::{run, TestDir};
use super::*;
use async_trait::async_trait;
use bob_kernel::{
    Command, CommandId, Executor, ModelInfo, ModelProgress, OperationId, Policy, Profile,
    RequestSpec, Runtime, SessionCatalog, ThreadState,
};
use bob_types::{Completion, ContentBlock, Message, ReasoningEffort, Role, StopReason, Usage};
use std::sync::{Arc, Mutex as StdMutex};

#[test]
fn full_display_preserves_incomplete_details_and_serialized_report() {
    let report = repository::SearchReport {
        tool: "grep",
        scope: "src".into(),
        entries: vec![
            repository::SearchEntry {
                path: "src/日本語.rs".into(),
                kind: "context",
                line: Some(1),
                text: Some("before".into()),
            },
            repository::SearchEntry {
                path: "src/日本語.rs".into(),
                kind: "match",
                line: Some(2),
                text: Some("é".repeat(8192)),
            },
            repository::SearchEntry {
                path: "src/日本語.rs".into(),
                kind: "match",
                line: Some(9),
                text: Some("final retained match 終".into()),
            },
        ],
        errors: vec!["first read error".into(), "last traversal error".into()],
        cancelled: true,
        truncated: true,
        limits_reached: vec!["cancelled", "results", "line_bytes"],
        offset: 1,
        next_offset: Some(4),
        ..Default::default()
    };
    let original = serde_json::to_string(&report).unwrap();
    let decoded: repository::SearchReport<String> = serde_json::from_str(&original).unwrap();
    assert_eq!(serde_json::to_string(&decoded).unwrap(), original);
    let display = presentation::repository_display(&original).unwrap();
    assert_eq!(
        display,
        format!("[!] Search cancelled.\n{}", repository::render(&report))
    );
    assert!(display.len() > 16 * 1024);
    assert!(display.starts_with("[!] Search cancelled.\n日本語.rs-1-before\n日本語.rs:2:"));
    assert!(display.contains("\n--\n日本語.rs:9:final retained match 終"));
    assert!(display.contains("[!] first read error\n[!] last traversal error"));
    assert!(display.contains("search incomplete (cancelled, results, line_bytes)"));
    assert!(display.contains("use offset=4 with the same query"));
    assert!(display.contains("Matching lines were clipped at 16384 bytes; use read_file"));
    assert!(!display.contains("[Preview;"));
    let preview = presentation::repository_result(&report).unwrap();
    assert!(preview.len() <= 2048);
    assert!(!preview.contains("final retained match"));
}

#[tokio::test]
async fn repository_display_handles_modes_empty_results_and_continuation() {
    let root = TestDir::new();
    fs::create_dir(root.file("src")).unwrap();
    fs::write(
        root.file("src/日本語.rs"),
        "before\nneedle\nneedle\nafter\n",
    )
    .unwrap();
    let tools = ToolService::default();
    let thread = ThreadId::new();
    for (name, input, expected) in [
        ("list_dir", json!({"path":"src"}), "日本語.rs"),
        ("glob", json!({"path":"src", "pattern":"*.rs"}), "日本語.rs"),
        (
            "grep",
            json!({"path":"src", "pattern":"needle", "output_mode":"count"}),
            "日本語.rs:2",
        ),
        (
            "grep",
            json!({"path":"src", "pattern":"needle", "output_mode":"files_with_matches"}),
            "日本語.rs",
        ),
        (
            "grep",
            json!({"path":"src", "pattern":"absent"}),
            "(no matches)",
        ),
    ] {
        let prepared = tools
            .prepare(root.invocation(&thread, name, input))
            .await
            .unwrap();
        let outcome = run(&tools, prepared.clone()).await;
        assert!(!outcome.is_error, "{}", outcome.text);
        let before = serde_json::to_value(&outcome).unwrap();
        assert_eq!(
            tools.display_text(&prepared, &outcome).unwrap().unwrap(),
            expected
        );
        assert_eq!(serde_json::to_value(&outcome).unwrap(), before);
    }
    let prepared = tools
        .prepare(root.invocation(
            &thread,
            "grep",
            json!({
                "path":"src", "pattern":"needle", "max_results":1
            }),
        ))
        .await
        .unwrap();
    let outcome = run(&tools, prepared.clone()).await;
    let display = tools.display_text(&prepared, &outcome).unwrap().unwrap();
    assert!(display.starts_with("日本語.rs:2:needle\n"));
    assert!(display.contains("search incomplete (results)"));
    assert!(display.contains("use offset=2"));
    let mut unrelated = prepared.clone();
    unrelated.invocation.name = "read_file".into();
    assert!(tools.display_text(&unrelated, &outcome).unwrap().is_none());
}

#[derive(Default)]
struct Observed {
    requests: Vec<RequestSpec>,
    result: Option<(OperationId, ToolOutcome, String)>,
}

struct RepositoryExecutor {
    tools: ToolService,
    name: &'static str,
    input: Value,
    observed: StdMutex<Observed>,
}

#[async_trait]
impl Executor for RepositoryExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "repository-display-test".into(),
            context_window: 128_000,
        }]
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        self.tools.tools()
    }

    fn display_text(&self, tool: &PreparedTool, outcome: &ToolOutcome) -> Result<Option<String>> {
        self.tools.display_text(tool, outcome)
    }

    async fn generate(
        &self,
        request: RequestSpec,
        _cancel: watch::Receiver<bool>,
        _progress: ModelProgress,
    ) -> Result<Completion> {
        let mut observed = self.observed.lock().unwrap();
        let first = observed.requests.is_empty();
        observed.requests.push(request);
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![if first {
                    ContentBlock::ToolUse {
                        id: "repository-call".into(),
                        name: self.name.into(),
                        input: self.input.clone(),
                    }
                } else {
                    ContentBlock::Text {
                        text: "done".into(),
                    }
                }],
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
        self.tools.prepare(invocation).await
    }

    async fn execute(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        let outcome = self.tools.execute(tool.clone(), cancel).await?;
        let before = serde_json::to_value(&outcome)?;
        let display = self.display_text(&tool, &outcome)?.unwrap();
        assert_eq!(serde_json::to_value(&outcome)?, before);
        self.observed.lock().unwrap().result =
            Some((tool.invocation.operation_id, outcome.clone(), display));
        Ok(outcome)
    }
}

#[tokio::test]
async fn repository_display_pages_full_utf8_results_and_reopens_read_only_without_changing_model_json(
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        let root = TestDir::new();
        fs::create_dir(root.file("workspace")).unwrap();
        let workspace = root.file("workspace");
        let content = (0..300)
            .map(|n| format!("needle {n:03} {} 終\n", "é日本語".repeat(12)))
            .collect::<String>();
        fs::write(workspace.join("matches.txt"), content).unwrap();
        for n in 0..240 {
            fs::write(
                workspace.join(format!("file-{n:03}-{}.rs", "日本語".repeat(9))),
                "",
            )
            .unwrap();
        }
        for (name, input) in [
            (
                "grep",
                json!({"path":"matches.txt", "pattern":"needle", "max_results":500}),
            ),
            ("glob", json!({"pattern":"*.rs"})),
            ("list_dir", json!({})),
        ] {
            let executor = Arc::new(RepositoryExecutor {
                tools: ToolService::default(),
                name,
                input,
                observed: StdMutex::new(Observed::default()),
            });
            let db = root.file(&format!("{name}.sqlite"));
            let handle = Runtime::open(&db, executor.clone()).await.unwrap();
            let receipt = handle
                .command(Command::CreateSession {
                    id: CommandId::new(),
                    workspace: workspace.to_str().unwrap().into(),
                    profile: Profile {
                        model: "repository-display-test".into(),
                        instructions: "Run the scripted repository query.".into(),
                        read_only: true,
                        max_requests: 3,
                        reasoning: ReasoningEffort::Off,
                    },
                    policy: Policy::default(),
                })
                .await
                .unwrap();
            let mut changes = handle.subscribe();
            handle
                .command(Command::Submit {
                    id: CommandId::new(),
                    thread: receipt.thread.clone(),
                    text: "Search.".into(),
                })
                .await
                .unwrap();
            loop {
                changes.borrow_and_update();
                let view = handle
                    .view(Some(receipt.thread.clone()), None, 100)
                    .await
                    .unwrap();
                assert!(view.error.is_none(), "{:?}", view.error);
                assert!(view.decisions.is_empty());
                if view.threads.iter().any(|thread| {
                    thread.id == receipt.thread && thread.state == ThreadState::Completed
                }) {
                    break;
                }
                changes.changed().await.unwrap();
            }
            let (operation, outcome, expected) =
                executor.observed.lock().unwrap().result.clone().unwrap();
            assert!(!outcome.is_error, "{}", outcome.text);
            let report: repository::SearchReport<String> =
                serde_json::from_str(&outcome.text).unwrap();
            assert!(!report.truncated);
            assert_eq!(serde_json::to_string(&report).unwrap(), outcome.text);
            assert_eq!(expected, repository::render(&report));
            assert!(expected.len() > 16 * 1024);
            let last = report.entries.last().unwrap();
            let final_entry = if name == "grep" {
                format!(
                    "{}:{}:{}",
                    last.path,
                    last.line.unwrap(),
                    last.text.as_deref().unwrap()
                )
            } else {
                last.path.clone()
            };
            assert!(expected.ends_with(&final_entry));
            let preview = outcome
                .presentation
                .as_ref()
                .unwrap()
                .body_preview
                .as_deref()
                .unwrap();
            assert!(preview.len() <= 2048);
            assert!(!preview.contains(&final_entry));
            {
                let observed = executor.observed.lock().unwrap();
                assert_eq!(observed.requests.len(), 2);
                let model_result = observed.requests[1]
                    .messages
                    .iter()
                    .flat_map(|message| &message.content)
                    .find_map(|block| match block {
                        ContentBlock::ToolResult { content, .. } => content.as_text(),
                        _ => None,
                    })
                    .unwrap();
                assert_eq!(model_result, outcome.text);
            }
            let mut collected = String::new();
            let mut offset = 0;
            let mut pages = 0;
            let mut split_unicode = false;
            loop {
                let page = handle
                    .tool_output(operation.clone(), offset, 16 * 1024)
                    .await
                    .unwrap();
                assert_eq!(page.total_bytes, expected.len());
                assert!(page.text.len() <= 16 * 1024);
                assert!(!page.text.is_empty());
                split_unicode |= page.next_offset.is_some() && page.text.len() < 16 * 1024;
                collected.push_str(&page.text);
                pages += 1;
                match page.next_offset {
                    Some(next) => {
                        assert_eq!(next, collected.len());
                        offset = next;
                    }
                    None => break,
                }
            }
            assert!(pages > 1);
            assert_eq!(collected, expected);
            assert!(collected.ends_with(&final_entry));
            if name == "grep" {
                assert!(
                    split_unicode,
                    "fixture must exercise UTF-8 clipping at a page boundary"
                );
            }
            handle.shutdown().await.unwrap();
            drop(handle);
            let before = fs::read(&db).unwrap();
            let catalog = SessionCatalog::open(&db).unwrap();
            let mut reopened = String::new();
            loop {
                let page = catalog
                    .tool_output(operation.clone(), reopened.len(), 16381)
                    .unwrap();
                assert_eq!(page.total_bytes, expected.len());
                assert!(!page.text.is_empty());
                reopened.push_str(&page.text);
                match page.next_offset {
                    Some(next) => assert_eq!(next, reopened.len()),
                    None => break,
                }
            }
            assert_eq!(reopened, expected);
            assert_eq!(fs::read(&db).unwrap(), before);
            assert_eq!(executor.observed.lock().unwrap().requests.len(), 2);
        }
    })
    .await
    .expect("repository display runtime fixture timed out");
}
