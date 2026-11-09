use super::super::{exploration, state::Pane};
use super::*;
use anyhow::Result;
use async_trait::async_trait;
use bob_kernel::{
    Command, CommandId, Executor, Handle, ModelInfo, ModelProgress, Policy, PreparedTool, Profile,
    RequestSpec, Runtime, ThreadState, ToolDescriptor, View,
};
use bob_types::{Completion, ContentBlock, Message, ReasoningEffort, Role, StopReason, Usage};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Reader {
    tools: bob::adapters::ToolService,
    input: Value,
    requests: Mutex<Vec<RequestSpec>>,
    outcome: Mutex<Option<ToolOutcome>>,
}

#[async_trait]
impl Executor for Reader {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "read-fixture".into(),
            context_window: 128_000,
        }]
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            spec: bob_types::ToolSpec {
                name: "read_file".into(),
                description: "Read the source fixture".into(),
                input_schema: json!({
                    "type":"object", "properties": {
                        "path":{"type":"string"}, "offset":{"type":"integer"},
                        "limit":{"type":"integer"}, "column_offset":{"type":"integer"}
                    }, "required":["path"], "additionalProperties":false
                }),
            },
            effect: bob_kernel::EffectClass::Read,
        }]
    }

    async fn generate(
        &self,
        request: RequestSpec,
        _cancel: watch::Receiver<bool>,
        _progress: ModelProgress,
    ) -> Result<Completion> {
        let mut requests = self.requests.lock().unwrap();
        let first = requests.is_empty();
        requests.push(request);
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![if first {
                    ContentBlock::ToolUse {
                        id: "read-source".into(),
                        name: "read_file".into(),
                        input: self.input.clone(),
                    }
                } else {
                    ContentBlock::Text {
                        text: "Read observed.".into(),
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
        let outcome = self.tools.execute(tool, cancel).await?;
        *self.outcome.lock().unwrap() = Some(outcome.clone());
        Ok(outcome)
    }
}

async fn completed(handle: &Handle, thread: &ThreadId) -> View {
    let mut changes = handle.subscribe();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            changes.borrow_and_update();
            let view = handle
                .transcript(Some(thread.clone()), None, 80)
                .await
                .unwrap();
            assert!(view.error.is_none(), "{:?}", view.error);
            if view
                .threads
                .iter()
                .any(|item| &item.id == thread && item.state == ThreadState::Completed)
            {
                return view;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("read fixture did not settle")
}

fn assert_group(view: &View, result: &Entry, expected_warning: &str) {
    let mut grouped = view.clone();
    grouped
        .entries
        .retain(|entry| entry.operation == result.operation);
    for sequence in 100..102 {
        let (mut call, _) = entries("glob", json!({"pattern":"*.rs"}), &outcome("source.rs"));
        call.sequence = sequence;
        if sequence == 100 {
            grouped.entries.insert(0, call);
        } else {
            grouped.entries.push(call);
        }
    }
    let mut pane = Pane::default();
    pane.view = Some(grouped);
    let groups = exploration::groups(&pane.view.as_ref().unwrap().entries);
    assert_eq!(groups.len(), 1);
    for expanded in [false, true] {
        if expanded {
            pane.expanded.insert(groups[0].first().clone());
        }
        let mut rows = Vec::new();
        groups[0].render(&pane, &mut rows, 512);
        let shown = text(&rows);
        assert!(!shown.contains("Result preview incomplete"));
        assert!(!shown.contains("click to expand"));
        assert_eq!(rows.iter().filter(|row| row.line.width() == 0).count(), 1);
        assert_eq!(rows.len(), if expanded { 5 } else { 2 });
        assert!(rows.iter().all(|row| matches!(row.hit, HitAction::Tool(_))));
        if !expected_warning.is_empty() {
            assert!(!shown.contains(expected_warning), "{shown}");
        }
    }
    assert!(pane.expanded_outputs.is_empty());
    assert!(pane.pending_outputs.is_empty());
}

#[tokio::test]
async fn real_reads_survive_kernel_preview_projection_and_reopen_without_false_warnings() {
    let ordinary = "let unique_value = 1;\n".repeat(400);
    let clipped = format!("{}{}\n", "prefix content\n".repeat(200), "界".repeat(2001));
    for (source, input, convention_failure, expected_summary, expected_warning) in [
        (
            format!("{}\n{}", "x".repeat(1000), "y".repeat(1033)),
            json!({"path":"source.rs"}),
            false,
            "lines 1–2",
            "",
        ),
        (
            ordinary.clone(),
            json!({"path":"source.rs"}),
            false,
            "lines 1–400",
            "",
        ),
        (
            ordinary.clone(),
            json!({"path":"source.rs", "limit":200}),
            false,
            "lines 1–200",
            "offset=201",
        ),
        (
            clipped,
            json!({"path":"source.rs"}),
            false,
            "lines 1–201",
            "column_offset=2001",
        ),
        (
            ordinary,
            json!({"path":"source.rs"}),
            true,
            "lines 1–400",
            "Project conventions",
        ),
    ] {
        let workspace = Workspace::new();
        std::fs::write(workspace.0.join("source.rs"), &source).unwrap();
        if convention_failure {
            std::fs::write(workspace.0.join("AGENTS.md"), [0xff, 0xfe]).unwrap();
        }
        let executor = Arc::new(Reader {
            tools: bob::adapters::ToolService::new(None, Some(workspace.0.join("home"))),
            input,
            requests: Mutex::new(Vec::new()),
            outcome: Mutex::new(None),
        });
        let db = workspace.0.join("read.sqlite");
        let handle = Runtime::open(&db, executor.clone()).await.unwrap();
        let receipt = handle
            .command(Command::CreateSession {
                id: CommandId::new(),
                workspace: workspace.0.to_string_lossy().into_owned(),
                profile: Profile {
                    model: "read-fixture".into(),
                    instructions: "Read presentation regression.".into(),
                    read_only: false,
                    max_requests: 4,
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
                text: "Read source.rs".into(),
            })
            .await
            .unwrap();
        let view = completed(&handle, &receipt.thread).await;
        let original = executor.outcome.lock().unwrap().clone().unwrap();
        assert!(
            !original.is_error && !original.interrupted,
            "{}",
            original.text
        );
        assert!(original.text.chars().count() >= 2048);
        if expected_summary == "lines 1–2" {
            assert_eq!(original.text.chars().count(), 2048);
        }
        if !expected_warning.is_empty() {
            let warning = if convention_failure { "[!]" } else { "... [" };
            let position = original.text.find(warning).unwrap();
            assert!(original.text[..position].chars().count() > 2048);
        }
        {
            let requests = executor.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[1].messages.iter().flat_map(|message| &message.content).any(|block| {
                matches!(block, ContentBlock::ToolResult { content, .. } if content.as_text() == Some(original.text.as_str()))
            }));
        }
        let overwrite = workspace.invocation(
            &receipt.thread,
            "write_file",
            json!({"path":"source.rs", "content":"replacement"}),
        );
        assert_eq!(
            executor.tools.prepare(overwrite).await.is_ok(),
            !matches!(expected_warning, "offset=201" | "column_offset=2001")
        );
        handle.shutdown().await.unwrap();
        let reopened = Runtime::open_paused(&db, executor.clone()).await.unwrap();
        let restored = reopened
            .transcript(Some(receipt.thread.clone()), None, 80)
            .await
            .unwrap();
        for view in [&view, &restored] {
            let call = view
                .entries
                .iter()
                .find(|entry| {
                    entry.kind == EntryKind::ToolCall
                        && entry
                            .tool
                            .as_ref()
                            .is_some_and(|tool| tool.name == "read_file")
                })
                .unwrap();
            let result = view
                .entries
                .iter()
                .find(|entry| {
                    entry.kind == EntryKind::ToolResult && entry.operation == call.operation
                })
                .unwrap();
            assert_eq!(
                result.text,
                original.text.chars().take(2048).collect::<String>()
            );
            if original.text.chars().count() > 2048 || expected_summary == "lines 1–2" {
                assert!(result.is_preview);
            }
            let presentation = call.tool.as_ref().unwrap().presentation.as_ref().unwrap();
            assert_eq!(presentation, original.presentation.as_ref().unwrap());
            assert_eq!(presentation.summary.as_deref(), Some(expected_summary));
            assert!(presentation.body_preview.as_ref().unwrap().len() < 2048);
            let rows = render(call, result, false);
            let shown = text(&rows);
            assert!(shown.contains("Read source.rs"), "{shown}");
            assert!(!shown.contains(" · lines"), "{shown}");
            assert!(!shown.contains("click to expand"));
            assert!(!shown.contains("Result preview incomplete"));
            assert_eq!(rows.len(), 2, "{shown}");
            if !expected_warning.is_empty() {
                assert!(!shown.contains(expected_warning), "{shown}");
            }
            assert_group(view, result, expected_warning);
            let mut standalone = Vec::new();
            tool_render::tool(&mut standalone, call, None, false, 512);
            assert!(!text(&standalone).contains(" · lines"));
            if !expected_warning.is_empty() {
                assert!(!text(&standalone).contains(expected_warning));
            }
            assert!(!text(&standalone).contains("click to expand"));
        }
        let operation = restored
            .entries
            .iter()
            .find(|entry| entry.kind == EntryKind::ToolResult)
            .unwrap()
            .operation
            .clone()
            .unwrap();
        let mut retained = String::new();
        loop {
            let page = reopened
                .tool_output(operation.clone(), retained.len(), 4096)
                .await
                .unwrap();
            retained.push_str(&page.text);
            if page.next_offset.is_none() {
                break;
            }
        }
        assert_eq!(retained, original.text);
        reopened.shutdown().await.unwrap();
    }
}

#[test]
fn read_agent_warnings_stay_out_of_the_transcript() {
    let mut result = outcome(&"body\n".repeat(1000));
    result.presentation = Some(ToolResultPresentation {
        summary: Some("lines 1–1000".into()),
        body_preview: Some(
            "[!] Output paged; use offset=1001 and limit to read further.\n[!] Applicable conventions may be incomplete.".into(),
        ),
        exit_code: None,
    });
    let (call, mut output) = entries("read_file", json!({"path":"source.rs"}), &result);
    output.text = output.text.chars().take(2048).collect();
    output.is_preview = true;
    for width in [40, 80, 120] {
        let mut rows = Vec::new();
        tool_render::tool(&mut rows, &call, Some(&output), false, width);
        let shown = text(&rows);
        for forbidden in ["[!]", "offset=1001", "incomplete", "click to expand"] {
            assert!(!shown.contains(forbidden), "{width}: {shown}");
        }
    }
}

#[test]
fn legacy_read_previews_are_neutral_at_and_beyond_the_preview_boundary() {
    for length in [2047, 2048, 2049, 16_000] {
        for is_preview in [false, true] {
            let (call, mut result) = entries(
                "read_file",
                json!({"path":"source.rs", "limit":2000}),
                &outcome(&"x".repeat(length)),
            );
            result.text = result.text.chars().take(2048).collect();
            result.is_preview = is_preview;
            let rows = render(&call, &result, false);
            assert_eq!(rows.len(), 2);
            let shown = text(&rows);
            for forbidden in ["incomplete", "complete", "lines 1", "click to expand"] {
                assert!(!shown.contains(forbidden), "{shown}");
            }
        }
    }
}
