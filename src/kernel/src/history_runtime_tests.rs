use super::tests::{seed_child, seed_run, seed_tool};
use crate::model::*;
use crate::store::{artifact, encode, entry, finish_tool_entry, tests::fixture, EntryData, Store};
use crate::{Handle, Runtime};
use async_trait::async_trait;
use bob_types::{Completion, ContentBlock, Message, Role, StopReason};
use rusqlite::params;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

struct ReaderState {
    calls: usize,
    accumulated: String,
    finished: bool,
    request: Value,
    aggregate_read: bool,
}
struct Reader {
    models: Vec<ModelInfo>,
    expected: String,
    aggregate: Option<OperationId>,
    state: Mutex<ReaderState>,
}
impl Reader {
    fn new(
        models: Vec<ModelInfo>,
        expected: String,
        request: Value,
        aggregate: Option<OperationId>,
    ) -> Self {
        Self {
            models,
            expected,
            aggregate,
            state: Mutex::new(ReaderState {
                calls: 0,
                accumulated: String::new(),
                finished: false,
                request,
                aggregate_read: false,
            }),
        }
    }
}

#[async_trait]
impl Executor for Reader {
    fn models(&self) -> Vec<ModelInfo> {
        self.models.clone()
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        request: RequestSpec,
        _: watch::Receiver<bool>,
        _: crate::ModelProgress,
    ) -> anyhow::Result<Completion> {
        assert!(request.tools.iter().any(|tool| tool.name == "read_history"));
        let mut state = self.state.lock().unwrap();
        state.calls += 1;
        if state.calls > 1 {
            let latest = request
                .messages
                .iter()
                .rev()
                .flat_map(|message| message.content.iter().rev())
                .find_map(|block| match block {
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => {
                        assert!(
                            !is_error.unwrap_or(false),
                            "history error: {}",
                            content.model_text()
                        );
                        Some(content.model_text().into_owned())
                    }
                    _ => None,
                })
                .expect("actual history output must reach the next RequestSpec");
            let page: Value = serde_json::from_str(&latest).unwrap();
            if self.aggregate.is_some() && !state.aggregate_read {
                let aggregate: Value =
                    serde_json::from_str(page["text"].as_str().unwrap()).unwrap();
                assert_eq!(aggregate["agents"][0]["output_omitted"], true);
                assert!(aggregate["agents"][0]["output"].is_null());
                state.request = json!({"view":"artifact", "thread_id":aggregate["agents"][0]["thread_id"], "artifact_id":aggregate["agents"][0]["result_artifact"], "limit_bytes":super::MAX_BYTES});
                state.aggregate_read = true;
            } else {
                assert_eq!(
                    page["offset"].as_u64().unwrap() as usize,
                    state.accumulated.len()
                );
                state.accumulated.push_str(page["text"].as_str().unwrap());
                assert!(self.expected.starts_with(&state.accumulated));
                if page["next_offset"].is_null() {
                    assert_eq!(state.accumulated, self.expected);
                    state.finished = true;
                    return Ok(Completion {
                        message: Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::Text {
                                text: "History reconstructed through model-issued pages.".into(),
                            }],
                        },
                        stop_reason: StopReason::EndTurn,
                        usage: Default::default(),
                    });
                }
                state.request["offset"] = page["next_offset"].clone();
            }
        }
        let input = if state.calls == 1 {
            self.aggregate
                .as_ref()
                .map(|operation| json!({"view":"tool_output", "operation_id":operation}))
                .unwrap_or_else(|| state.request.clone())
        } else {
            state.request.clone()
        };
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("history-{}", state.calls),
                    name: "read_history".into(),
                    input,
                }],
            },
            stop_reason: StopReason::ToolUse,
            usage: Default::default(),
        })
    }
    async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
        unreachable!("kernel must own read_history execution")
    }
    async fn execute(
        &self,
        _: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        unreachable!("kernel must own read_history execution")
    }
}

async fn run_reader(handle: &Handle, thread: &ThreadId) {
    let mut updates = handle.subscribe();
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: thread.clone(),
            text: "Retrieve retained history using read_history, without rerunning earlier work."
                .into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let view = handle.view(Some(thread.clone()), None, 20).await.unwrap();
            assert!(view.error.is_none(), "{:?}", view.error);
            if view
                .entries
                .iter()
                .any(|entry| entry.text == "History reconstructed through model-issued pages.")
            {
                break;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .expect("history model loop did not finish");
}

#[tokio::test]
async fn model_recovers_omitted_child_report_in_actual_followup_requests() {
    let (dir, store, receipt, models) = fixture();
    let run = seed_run(&store.db, &receipt.thread);
    let child = seed_child(&store.db, &receipt.thread, &run);
    let operation = seed_tool(&store.db, &receipt.thread, &run, &json!({}));
    let report = format!(
        "BEGIN-REPORT:{}:END-REPORT",
        "retained λ🦀\n".repeat(36_000)
    );
    assert!(report.len() > crate::store::MAX_ARTIFACT / 8);
    let outcome = artifact(&store.db, &report).unwrap();
    store
        .db
        .execute(
            "UPDATE agents SET operation=?1,outcome=?2,reported=1 WHERE thread=?3",
            params![operation, outcome, child],
        )
        .unwrap();
    let (_, aggregate) = crate::delegation::reports(&store.db, &operation).unwrap();
    assert_eq!(aggregate["agents"][0]["output_omitted"], true);
    let call = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: operation.0.clone(),
            name: "read_file".into(),
            input: json!({}),
        }],
    };
    entry(
        &store.db,
        &receipt.thread,
        "assistant",
        "",
        EntryData {
            message: Some(&call),
            ..Default::default()
        },
    )
    .unwrap();
    finish_tool_entry(&store.db, &operation, &encode(&aggregate).unwrap(), false).unwrap();
    drop(store);
    let executor = Arc::new(Reader::new(models, report, Value::Null, Some(operation)));
    let handle = Runtime::open(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    handle
        .activate_session(receipt.session.clone(), Vec::new())
        .await
        .unwrap();
    run_reader(&handle, &receipt.thread).await;
    handle.shutdown().await.unwrap();
    {
        let state = executor.state.lock().unwrap();
        assert!(state.finished);
        assert!(state.aggregate_read);
        assert!(state.calls > 20);
    }
    let store = Store::open(&dir.0.join("lab.db")).unwrap();
    let durable_reads:usize = store.db.query_row("SELECT COUNT(*) FROM operations WHERE thread=?1 AND name='read_history' AND state='done' AND result IS NOT NULL", [&receipt.thread], |r|r.get(0)).unwrap();
    assert_eq!(durable_reads, executor.state.lock().unwrap().calls - 1);
}

#[tokio::test]
async fn model_reads_original_entry_after_committed_compaction_and_reopen() {
    let (dir, mut store, receipt, models) = fixture();
    let text = "Before compaction: exact retained text 雪🦀\n".repeat(1600);
    let message = Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: text.clone() }],
    };
    entry(
        &store.db,
        &receipt.thread,
        "assistant",
        &text,
        EntryData {
            message: Some(&message),
            ..Default::default()
        },
    )
    .unwrap();
    let sequence = store.db.last_insert_rowid();
    let snapshot =
        crate::context::snapshot(&store.db, &receipt.thread, &models[0], &Default::default())
            .unwrap();
    let checkpoint = OperationId::new();
    let tx = store.db.transaction().unwrap();
    crate::context::checkpoint(
        &tx,
        &receipt.thread,
        &checkpoint,
        snapshot.epoch,
        snapshot.revision,
        &[Message::user_text(
            "Prior conversation compacted; retrieve the original entry explicitly.",
        )],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(store);
    let executor = Arc::new(Reader::new(
        models,
        text,
        json!({"view":"entry", "sequence":sequence}),
        None,
    ));
    let handle = Runtime::open(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    handle
        .activate_session(receipt.session.clone(), Vec::new())
        .await
        .unwrap();
    assert_eq!(
        handle
            .context(receipt.thread.clone())
            .await
            .unwrap()
            .checkpoint,
        Some(checkpoint)
    );
    run_reader(&handle, &receipt.thread).await;
    handle.shutdown().await.unwrap();
    assert!(executor.state.lock().unwrap().finished);
    assert!(executor.state.lock().unwrap().calls > 2);
}
