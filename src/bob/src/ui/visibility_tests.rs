use super::*;
use crate::ui::{exploration, render, state::Pane};
use bob_kernel::{OperationId, ThreadId, ToolResultPresentation, ToolSummary, View};
use serde_json::json;

fn entry(sequence: i64, name: &str, state: OperationState, kind: EntryKind) -> Entry {
    let id = OperationId(format!("tool-{sequence}"));
    Entry {
        sequence,
        thread: ThreadId("root".into()),
        kind,
        text: "Retained convention contents".into(),
        artifact: None,
        operation: Some(id.clone()),
        sender: None,
        parent_guidance: false,
        submission: None,
        tool: Some(ToolSummary {
            id,
            name: name.into(),
            state,
            input_preview: json!({"path":"."}),
            result: None,
            presentation: Some(ToolResultPresentation {
                summary: Some("2 convention sources".into()),
                body_preview: None,
                exit_code: None,
            }),
        }),
        is_preview: false,
        visible_text_bytes: None,
        thinking: None,
    }
}

fn pane(entries: Vec<Entry>) -> Pane {
    let view = View {
        sequence: 10,
        entry_sequence: 10,
        entries,
        threads: vec![],
        policies: vec![],
        decisions: vec![],
        operations: vec![],
        error: None,
    };
    let mut pane = Pane::default();
    pane.view = Some(serde_json::from_slice(&serde_json::to_vec(&view).unwrap()).unwrap());
    pane
}

fn screen(pane: &Pane, root: bool) -> String {
    render::transcript(pane, root, 120)
        .iter()
        .map(|row| row.line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn todo_entry(sequence: i64, state: OperationState, kind: EntryKind) -> Entry {
    let mut entry = entry(sequence, "todo_write", state, kind);
    entry.text = "Task list updated (3 items).".into();
    entry.tool.as_mut().unwrap().presentation = None;
    entry
}

#[test]
fn routine_todo_calls_and_results_are_hidden_in_root_and_agent_replay() {
    for state in [
        OperationState::Pending,
        OperationState::Preparing,
        OperationState::Ready,
        OperationState::Started,
        OperationState::Waiting,
        OperationState::Done,
    ] {
        for kind in [EntryKind::ToolCall, EntryKind::ToolResult] {
            let update = todo_entry(1, state, kind);
            let before = serde_json::to_value(&update).unwrap();
            let pane = pane(vec![update]);
            for root in [true, false] {
                assert!(
                    screen(&pane, root).trim().is_empty(),
                    "{state:?}/{kind:?}/{root}"
                );
            }
            assert_eq!(
                serde_json::to_value(&pane.view.as_ref().unwrap().entries[0]).unwrap(),
                before
            );
        }
    }
}

#[test]
fn todo_errors_and_plan_approvals_are_not_hidden() {
    for state in [
        OperationState::Failed,
        OperationState::Approval,
        OperationState::Unknown,
        OperationState::Interrupted,
        OperationState::Cancelled,
        OperationState::Resolved,
    ] {
        let update = todo_entry(1, state, EntryKind::ToolResult);
        assert!(!hidden_tool(&update));
        for root in [true, false] {
            assert!(screen(&pane(vec![update.clone()]), root).contains("Plan"));
        }
    }
    assert!(!hidden_tool(&todo_entry(
        1,
        OperationState::Done,
        EntryKind::ToolError
    )));
    for state in [OperationState::Approval, OperationState::Done] {
        let plan = entry(1, "exit_plan", state, EntryKind::ToolCall);
        assert!(!hidden_tool(&plan));
        for root in [true, false] {
            assert!(screen(&pane(vec![plan.clone()]), root).contains("Plan"));
        }
    }
}

#[test]
fn hidden_todo_updates_preserve_task_panel_and_exploration_groups() {
    let entries = vec![
        entry(1, "read_file", OperationState::Done, EntryKind::ToolCall),
        todo_entry(2, OperationState::Done, EntryKind::ToolResult),
        entry(3, "grep", OperationState::Done, EntryKind::ToolCall),
        entry(4, "glob", OperationState::Done, EntryKind::ToolCall),
    ];
    let groups = exploration::groups(&entries);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].entries.len(), 3);
    let mut pane = pane(entries);
    pane.controls = Some(bob_kernel::ControlSnapshot {
        todos: vec![bob_kernel::TodoItem {
            content: "Verify the task panel".into(),
            active_form: Some("Verifying the task panel".into()),
            status: bob_kernel::TodoStatus::InProgress,
        }],
        ..Default::default()
    });
    for root in [true, false] {
        let shown = screen(&pane, root);
        assert!(shown.contains("Explored"));
        assert!(!shown.contains("Plan"));
        assert!(!shown.contains("Task list updated"));
    }
    let tasks = crate::ui::control_render::todo_lines(&pane, 120)
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(tasks.contains("1 in progress"));
    assert!(tasks.contains("Verifying the task panel"));
    let entries = &mut pane.view.as_mut().unwrap().entries;
    entries[1].kind = EntryKind::ToolError;
    entries[1].tool.as_mut().unwrap().state = OperationState::Failed;
    assert!(exploration::groups(entries).is_empty());
    assert!(screen(&pane, true).contains("Plan"));
}

#[test]
fn routine_instruction_calls_and_results_are_hidden_in_root_and_agent_replay() {
    for state in [
        OperationState::Pending,
        OperationState::Preparing,
        OperationState::Ready,
        OperationState::Started,
        OperationState::Waiting,
        OperationState::Done,
    ] {
        for kind in [EntryKind::ToolCall, EntryKind::ToolResult] {
            let instruction = entry(1, "project_instructions", state, kind);
            let before = serde_json::to_value(&instruction).unwrap();
            let pane = pane(vec![instruction]);
            for root in [true, false] {
                assert!(
                    screen(&pane, root).trim().is_empty(),
                    "{state:?}/{kind:?}/{root}"
                );
            }
            assert_eq!(
                serde_json::to_value(&pane.view.as_ref().unwrap().entries[0]).unwrap(),
                before
            );
        }
    }
}

#[test]
fn instruction_errors_approvals_and_uncertain_outcomes_remain_visible() {
    for state in [
        OperationState::Failed,
        OperationState::Approval,
        OperationState::Unknown,
        OperationState::Interrupted,
        OperationState::Cancelled,
        OperationState::Resolved,
    ] {
        let pane = pane(vec![entry(
            1,
            "project_instructions",
            state,
            EntryKind::ToolResult,
        )]);
        for root in [true, false] {
            assert!(
                screen(&pane, root).contains("project_instructions"),
                "{state:?}/{root}"
            );
        }
    }
    let error = entry(
        1,
        "project_instructions",
        OperationState::Done,
        EntryKind::ToolError,
    );
    assert!(!hidden_tool(&error));
    for root in [true, false] {
        assert!(screen(&pane(vec![error.clone()]), root).contains("project_instructions"));
    }
}

#[test]
fn incomplete_results_are_hidden_but_missing_metadata_is_not() {
    let mut incomplete = entry(
        1,
        "project_instructions",
        OperationState::Done,
        EntryKind::ToolResult,
    );
    incomplete
        .tool
        .as_mut()
        .unwrap()
        .presentation
        .as_mut()
        .unwrap()
        .summary = Some("2 convention sources (incomplete)".into());
    for root in [true, false] {
        assert!(!screen(&pane(vec![incomplete.clone()]), root).contains("(incomplete)"));
    }
    let mut unknown = incomplete.clone();
    unknown.tool.as_mut().unwrap().presentation = None;
    assert!(!hidden_tool(&unknown));
    unknown.tool = None;
    assert!(!hidden_tool(&unknown));
    for root in [true, false] {
        assert!(screen(&pane(vec![unknown.clone()]), root).contains("Retained convention contents"));
    }
    incomplete
        .tool
        .as_mut()
        .unwrap()
        .presentation
        .as_mut()
        .unwrap()
        .summary = Some("2 convention sources".into());
    incomplete
        .tool
        .as_mut()
        .unwrap()
        .presentation
        .as_mut()
        .unwrap()
        .exit_code = Some(1);
    assert!(!hidden_tool(&incomplete));
}

#[test]
fn hidden_instruction_loading_neither_counts_nor_splits_exploration_groups() {
    let mut entries = vec![
        entry(1, "read_file", OperationState::Done, EntryKind::ToolCall),
        entry(
            2,
            "project_instructions",
            OperationState::Done,
            EntryKind::ToolResult,
        ),
        entry(3, "grep", OperationState::Done, EntryKind::ToolCall),
        entry(4, "glob", OperationState::Done, EntryKind::ToolCall),
    ];
    let groups = exploration::groups(&entries);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].entries.len(), 3);
    assert!(groups[0]
        .entries
        .iter()
        .all(|entry| entry.tool.as_ref().unwrap().name != "project_instructions"));
    for root in [true, false] {
        let shown = screen(&pane(entries.clone()), root);
        assert!(shown.contains("Explored"));
        assert!(!shown.contains("project_instructions"));
    }
    entries[1].kind = EntryKind::ToolError;
    entries[1].tool.as_mut().unwrap().state = OperationState::Failed;
    assert!(exploration::groups(&entries).is_empty());
    for root in [true, false] {
        assert!(screen(&pane(entries.clone()), root).contains("project_instructions"));
    }
}

#[test]
fn thinking_is_one_collapsed_line_above_the_reply_until_expanded() {
    let mut reply = entry(1, "", OperationState::Done, EntryKind::Assistant);
    reply.operation = None;
    reply.tool = None;
    reply.text = "Final answer".into();
    reply.thinking = Some(
        [
            "step one",
            "step two",
            "step three",
            "step four",
            "step five",
        ]
        .join("\n"),
    );
    let mut pane = pane(vec![reply]);

    let collapsed = screen(&pane, true);
    let header = collapsed
        .lines()
        .find(|line| line.contains("Thinking"))
        .expect("thinking header");
    assert_eq!(header.trim_end(), "Thinking", "{collapsed}");
    assert!(
        collapsed.find("Thinking").unwrap() < collapsed.find("Final answer").unwrap(),
        "{collapsed}"
    );
    assert!(!collapsed.contains("step"), "{collapsed}");
    assert!(
        !collapsed.contains('▸') && !collapsed.contains('▾'),
        "{collapsed}"
    );

    pane.expanded_thinking.insert(1);
    let expanded = screen(&pane, true);
    assert!(expanded.contains("Thinking\n"), "{expanded}");
    assert!(expanded.contains("  step five"), "{expanded}");
}

#[test]
fn collapsed_thinking_never_previews_long_reasoning() {
    let mut reply = entry(1, "", OperationState::Done, EntryKind::Assistant);
    reply.operation = None;
    reply.tool = None;
    reply.text = String::new();
    reply.thinking = Some("word ".repeat(100));
    let shown = screen(&pane(vec![reply]), true);
    assert!(shown.contains("Thinking"), "{shown}");
    assert!(!shown.contains("word"), "{shown}");
}
