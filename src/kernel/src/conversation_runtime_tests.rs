use super::*;
use crate::store::tests::fixture;
use crate::{OperationState, ThreadState};
use async_trait::async_trait;

const NEEDLE: &str = "archivetailneedle";

struct RecallState {
    calls: usize,
    accumulated: String,
    input: Value,
    name: &'static str,
}

struct RecallExecutor {
    models: Vec<ModelInfo>,
    target: SessionId,
    sequence: i64,
    expected: String,
    state: Mutex<RecallState>,
    denied: bool,
}

#[async_trait]
impl Executor for RecallExecutor {
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
        let mut state = self.state.lock().unwrap();
        state.calls += 1;
        assert_eq!(
            request.tools.iter().any(|tool| tool.name == state.name),
            !self.denied
        );
        if state.calls > 1 {
            let (body, is_error) = request
                .messages
                .iter()
                .rev()
                .flat_map(|message| message.content.iter().rev())
                .find_map(|block| match block {
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } => Some((content.model_text(), is_error.unwrap_or(false))),
                    _ => None,
                })
                .expect("archive tool result must enter the next model request");
            assert!(!body.contains("PRIVATE_ARCHIVE_REASONING"));
            if self.denied {
                assert!(is_error);
                assert!(body.contains("denial"));
                return Ok(answer("Archive access denied."));
            }
            assert!(!is_error, "{body}");
            let value: Value = serde_json::from_str(&body).unwrap();
            if state.name == "search_conversation" {
                if let Some(hit) = value["matches"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|hit| hit["sequence"] == self.sequence)
                {
                    assert_eq!(hit["session_id"], self.target.0);
                    state.input = hit["read_ref"].clone();
                    state.input["limit_bytes"] = json!(2048);
                    state.name = "read_conversation";
                } else {
                    assert_eq!(
                        value["scan_complete"], false,
                        "expected archived entry missing: {value}"
                    );
                    assert!(value["next_cursor"].is_string());
                    state.input["cursor"] = value["next_cursor"].clone();
                }
            } else {
                assert_eq!(value["session_id"], self.target.0);
                assert_eq!(value["offset"], state.accumulated.len());
                state.accumulated.push_str(value["text"].as_str().unwrap());
                assert!(self.expected.starts_with(&state.accumulated));
                if value["next_offset"].is_null() {
                    assert_eq!(state.accumulated, self.expected);
                    return Ok(answer("Archived conversation recovered."));
                }
                state.input["offset"] = value["next_offset"].clone();
            }
        }
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: format!("recall-{}", state.calls),
                    name: state.name.into(),
                    input: state.input.clone(),
                }],
            },
            stop_reason: StopReason::ToolUse,
            usage: Default::default(),
        })
    }
    async fn prepare(&self, _: Invocation) -> anyhow::Result<PreparedTool> {
        panic!("archive reads must be kernel-owned")
    }
    async fn execute(
        &self,
        _: PreparedTool,
        _: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome> {
        panic!("archive reads must be kernel-owned")
    }
}

fn answer(text: &str) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text { text: text.into() }],
        },
        stop_reason: StopReason::EndTurn,
        usage: Default::default(),
    }
}

fn archive(store: &mut Store, models: &[ModelInfo]) -> (Receipt, i64, String) {
    let workspace: String = store
        .db
        .query_row("SELECT workspace FROM sessions LIMIT 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    let receipt = store
        .command(
            &Command::CreateSession {
                id: CommandId::new(),
                workspace,
                profile: Profile {
                    model: models[0].id.clone(),
                    instructions: String::new(),
                    read_only: false,
                    max_requests: 4,
                    reasoning: bob_types::ReasoningEffort::Off,
                },
                policy: Policy::default(),
            },
            models,
        )
        .unwrap();
    let text = format!(
        "{}\nKusto {NEEDLE}: Events | summarize count() by Service\n",
        "Retained Unicode 雪λ context.\n".repeat(200)
    );
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "PRIVATE_ARCHIVE_REASONING".into(),
                signature: "private-signature".into(),
            },
            ContentBlock::Text { text: text.clone() },
        ],
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
    store
        .db
        .execute(
            "UPDATE sessions SET workspace='/missing/archived/workspace' WHERE id=?1",
            [&receipt.session],
        )
        .unwrap();
    store.db.execute("UPDATE threads SET held=1,profile=json_set(profile,'$.model','unavailable-archived-model') WHERE id=?1", [&receipt.thread]).unwrap();
    (receipt, sequence, text)
}

fn target_state(db: &rusqlite::Connection, target: &Receipt) -> String {
    db.query_row("SELECT json_object('workspace',s.workspace,'policy',s.policy,'permissions',s.permissions,'updated',s.updated,'profile',t.profile,'held',t.held,'entries',(SELECT COUNT(*) FROM entries WHERE thread=t.id),'runs',(SELECT COUNT(*) FROM runs WHERE thread=t.id)) FROM sessions s JOIN threads t ON t.session=s.id WHERE t.id=?1", [&target.thread], |row| row.get(0)).unwrap()
}

async fn finish(handle: &Handle, thread: &ThreadId) -> View {
    let mut updates = handle.subscribe();
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: thread.clone(),
            text: format!("Find our earlier Kusto {NEEDLE} conversation."),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            updates.borrow_and_update();
            let view = handle.view(Some(thread.clone()), None, 200).await.unwrap();
            assert!(view.error.is_none(), "{:?}", view.error);
            assert!(view.decisions.is_empty());
            if view
                .threads
                .iter()
                .any(|item| &item.id == thread && item.state == ThreadState::Completed)
            {
                return view;
            }
            updates.changed().await.unwrap();
        }
    })
    .await
    .expect("archive recall did not finish")
}

#[tokio::test]
async fn model_searches_and_reads_full_archived_entry_without_activating_target() {
    let (dir, mut store, caller, models) = fixture();
    let (target, sequence, expected) = archive(&mut store, &models);
    store
        .db
        .execute(
            "UPDATE threads SET profile=json_set(profile,'$.read_only',json('true')) WHERE id=?1",
            [&caller.thread],
        )
        .unwrap();
    store
        .command(
            &Command::SetPermissions {
                id: CommandId::new(),
                session: caller.session.clone(),
                permissions: PermissionPolicy {
                    mode: PermissionMode::Plan,
                    ..Default::default()
                },
                expected_revision: 0,
            },
            &models,
        )
        .unwrap();
    let before = target_state(&store.db, &target);
    drop(store);
    let executor = Arc::new(RecallExecutor {
        models,
        target: target.session.clone(),
        sequence,
        expected,
        state: Mutex::new(RecallState {
            calls: 0,
            accumulated: String::new(),
            input: json!({"query":NEEDLE}),
            name: "search_conversation",
        }),
        denied: false,
    });
    let handle = Runtime::open(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    handle
        .activate_session(caller.session.clone(), Vec::new())
        .await
        .unwrap();
    let completed = finish(&handle, &caller.thread).await;
    assert!(completed
        .entries
        .iter()
        .any(|entry| entry.text == "Archived conversation recovered."));
    let operations: Vec<_> = completed
        .operations
        .iter()
        .filter(|op| {
            matches!(
                op.name.as_str(),
                "search_conversation" | "read_conversation"
            )
        })
        .collect();
    assert!(operations.len() > 2);
    assert!(operations
        .iter()
        .all(|op| op.state == OperationState::Done && op.result.is_some()));
    handle.shutdown().await.unwrap();
    let db = rusqlite::Connection::open_with_flags(
        dir.0.join("lab.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    assert_eq!(target_state(&db, &target), before);
    drop(db);
    let reopened = Runtime::open_paused(&dir.0.join("lab.db"), executor.clone())
        .await
        .unwrap();
    for operation in operations {
        let retained = reopened
            .tool_output(operation.id.clone(), 0, 24576)
            .await
            .unwrap();
        assert!(!retained.text.contains("PRIVATE_ARCHIVE_REASONING"));
        assert!(serde_json::from_str::<Value>(&retained.text).is_ok());
    }
    reopened.shutdown().await.unwrap();
    let state = executor.state.lock().unwrap();
    assert_eq!(state.accumulated, executor.expected);
    assert!(state.calls > 3);
}

#[tokio::test]
async fn archive_tool_denials_filter_advertisement_and_reject_forced_dispatch() {
    for name in ["search_conversation", "read_conversation"] {
        let (dir, mut store, caller, models) = fixture();
        let (target, sequence, expected) = archive(&mut store, &models);
        store
            .command(
                &Command::SetPermissions {
                    id: CommandId::new(),
                    session: caller.session.clone(),
                    permissions: PermissionPolicy {
                        rules: vec![PermissionRule {
                            tool: name.into(),
                            decision: RuleDecision::Deny,
                        }],
                        ..Default::default()
                    },
                    expected_revision: 0,
                },
                &models,
            )
            .unwrap();
        let before = target_state(&store.db, &target);
        drop(store);
        let input = if name == "search_conversation" {
            json!({"query":NEEDLE})
        } else {
            json!({"view":"entry","session_id":target.session,"thread_id":target.thread,"sequence":sequence})
        };
        let executor = Arc::new(RecallExecutor {
            models,
            target: target.session.clone(),
            sequence,
            expected,
            state: Mutex::new(RecallState {
                calls: 0,
                accumulated: String::new(),
                input,
                name,
            }),
            denied: true,
        });
        let handle = Runtime::open(&dir.0.join("lab.db"), executor.clone())
            .await
            .unwrap();
        handle
            .activate_session(caller.session.clone(), Vec::new())
            .await
            .unwrap();
        let completed = finish(&handle, &caller.thread).await;
        let operation = completed
            .operations
            .iter()
            .find(|op| op.name == name)
            .unwrap();
        assert_eq!(operation.state, OperationState::Failed);
        assert_eq!(executor.state.lock().unwrap().calls, 2);
        handle.shutdown().await.unwrap();
        let db = rusqlite::Connection::open_with_flags(
            dir.0.join("lab.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        assert_eq!(target_state(&db, &target), before);
    }
}
