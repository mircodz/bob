use anyhow::Result;
use async_trait::async_trait;
use bob::services::Services;
use bob_core::core::{config::BobConfig, memory::InMemoryMemories};
use bob_kernel::{
    Command, CommandId, EntryKind, Executor, Invocation, ModelInfo, ModelProgress, OperationState,
    Policy, PreparedTool, Profile, RequestSpec, Runtime, ToolCapabilities, ToolDescriptor,
    ToolOutcome,
};
use bob_types::{Completion, ContentBlock, Message, ReasoningEffort, Role, StopReason};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-lsp-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        std::fs::write(path.join("a.rs"), "fn a() {}\n").unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct ServiceExecutor {
    services: Services,
    name: &'static str,
    input: Value,
    diagnostic: &'static str,
    generated: AtomicUsize,
    executed: AtomicUsize,
}

#[async_trait]
impl Executor for ServiceExecutor {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "fixture".into(),
            context_window: 100_000,
        }]
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        self.services
            .descriptors()
            .into_iter()
            .filter(|tool| tool.spec.name == self.name)
            .collect()
    }

    fn tool_capabilities(&self, name: &str) -> ToolCapabilities {
        self.services.tool_capabilities(name)
    }

    fn prepared_capabilities(&self, tool: &PreparedTool) -> Result<ToolCapabilities> {
        self.services.prepared_capabilities(tool)
    }

    async fn generate(
        &self,
        request: RequestSpec,
        _: watch::Receiver<bool>,
        _: ModelProgress,
    ) -> Result<Completion> {
        let call = self.generated.fetch_add(1, Ordering::SeqCst);
        let (content, stop_reason) = if call == 0 {
            (
                ContentBlock::ToolUse {
                    id: "lsp-query".into(),
                    name: self.name.into(),
                    input: self.input.clone(),
                },
                StopReason::ToolUse,
            )
        } else {
            assert_eq!(call, 1, "unexpected extra model request");
            let results: Vec<_> = request
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } if tool_use_id == "lsp-query" => Some((content, is_error)),
                    _ => None,
                })
                .collect();
            assert_eq!(results.len(), 1);
            assert_eq!(*results[0].1, Some(true));
            let diagnostic = results[0].0.as_text().expect("text LSP error result");
            assert!(diagnostic.contains(self.diagnostic));
            assert!(!diagnostic.contains("unknown effect"));
            (
                ContentBlock::Text {
                    text: "LSP unavailable; continue with read/search tools.".into(),
                },
                StopReason::EndTurn,
            )
        };
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![content],
            },
            stop_reason,
            usage: Default::default(),
        })
    }

    async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        self.services.prepare(invocation).await
    }

    async fn prepare_with_cancel(
        &self,
        invocation: Invocation,
        cancel: watch::Receiver<bool>,
    ) -> Result<PreparedTool> {
        self.services.prepare_with_cancel(invocation, cancel).await
    }

    async fn execute(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        self.executed.fetch_add(1, Ordering::SeqCst);
        self.services.execute(tool, cancel).await
    }
}

async fn assert_query_recovers(name: &'static str, input: Value, diagnostic: &'static str) {
    let fixture = Fixture::new();
    let config = BobConfig {
        lsp_servers: Vec::new(),
        mcp_servers: Vec::new(),
        ..Default::default()
    };
    let services = Services::create(&config, &fixture.0, Arc::new(InMemoryMemories::default()))
        .await
        .unwrap();
    let executor = Arc::new(ServiceExecutor {
        services,
        name,
        input,
        diagnostic,
        generated: AtomicUsize::new(0),
        executed: AtomicUsize::new(0),
    });
    let database = fixture.0.join("lab.db");
    let handle = Runtime::open(&database, executor.clone()).await.unwrap();
    let receipt = handle
        .command(Command::CreateSession {
            id: CommandId::new(),
            workspace: fixture.0.to_string_lossy().into_owned(),
            profile: Profile {
                model: "fixture".into(),
                instructions: "Use the requested LSP query, then recover from errors.".into(),
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
            text: "Inspect symbol a.".into(),
        })
        .await
        .unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = handle
                .view(Some(receipt.thread.clone()), None, 100)
                .await
                .unwrap();
            assert!(view.error.is_none(), "kernel failure: {:?}", view.error);
            let thread = view
                .threads
                .iter()
                .find(|t| t.id == receipt.thread)
                .unwrap();
            assert!(!matches!(
                thread.state,
                bob_kernel::ThreadState::Held
                    | bob_kernel::ThreadState::Interrupted
                    | bob_kernel::ThreadState::Failed
            ));
            if thread.state == bob_kernel::ThreadState::Completed {
                break view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    handle.shutdown().await.unwrap();
    let completed = completed.expect("LSP failure must allow the next model request to complete");
    assert_eq!(executor.generated.load(Ordering::SeqCst), 2);
    assert_eq!(executor.executed.load(Ordering::SeqCst), 1);
    assert!(executor.services.lsp_statuses().is_empty());
    let operation = completed
        .operations
        .iter()
        .find(|op| op.name == name)
        .unwrap();
    assert_eq!(operation.state, OperationState::Failed);
    assert!(completed.entries.iter().any(|entry| {
        entry.kind == EntryKind::Assistant && entry.text.contains("continue with read/search")
    }));
    let connection = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let held: bool = connection
        .query_row(
            "SELECT held FROM threads WHERE id=?1",
            [&receipt.thread.0],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!held);
    drop(connection);
    let reopened = Runtime::open(&database, executor).await.unwrap();
    let output = reopened.tool_output(operation.id.clone(), 0, 4096).await;
    reopened.shutdown().await.unwrap();
    assert!(output.unwrap().text.contains(diagnostic));
}

#[tokio::test]
async fn missing_file_server_is_failed_not_unknown_and_model_continues() {
    assert_query_recovers(
        "lsp",
        json!({"operation":"hover","filePath":"a.rs","line":1,"character":4}),
        "no initialized language server configured for",
    )
    .await;
}

#[tokio::test]
async fn missing_workspace_servers_are_failed_not_unknown_and_model_continues() {
    assert_query_recovers(
        "lsp",
        json!({"operation":"workspace_symbols","query":"a"}),
        "no language servers configured",
    )
    .await;
}

#[tokio::test]
async fn list_only_code_action_failure_allows_model_continuation() {
    assert_query_recovers(
        "code_action",
        json!({"filePath":"a.rs","line":1,"character":1}),
        "no initialized language server configured for",
    )
    .await;
}
