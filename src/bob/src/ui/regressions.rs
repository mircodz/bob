use super::*;
use bob_kernel::{
    Decision, Entry, RunId, SessionId, TextPage, ToolBody, ToolPresentation,
    ToolResultPresentation, ToolSummary,
};
use bob_ui::theme::Palette;
use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect};
use serde_json::json;

#[path = "message_queue_tests.rs"]
mod message_queue_tests;
#[path = "permission_tests.rs"]
mod permission_tests;

#[test]
fn multiline_command_notices_preserve_lines_and_bottom_padding() {
    let mut app = app(Vec::new());
    app.pane_mut()
        .unwrap()
        .notify_current("\x1b[31mSession usage\x1b[0m\n0 requests\n\nAll-time usage\n0 requests");
    let rows = render::transcript(app.pane().unwrap(), true, 120);
    let lines: Vec<String> = rows
        .iter()
        .map(|row| {
            row.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    assert_eq!(
        lines,
        [
            "Session usage",
            "0 requests",
            "",
            "All-time usage",
            "0 requests",
            ""
        ]
    );
    assert!(!lines.iter().any(|line| line.chars().any(char::is_control)));
}

fn command_result(app: &mut App, submitted: &str, result: Result<Outcome, String>) {
    app.pane_mut().unwrap().pending = Some(1);
    app.reply(Reply::Command {
        thread: app.selected.clone().unwrap(),
        submitted: submitted.into(),
        revision: 0,
        focus_epoch: app.focus_epoch,
        epoch: 1,
        inspection_epoch: 0,
        result,
    });
}

#[test]
fn usage_notice_keeps_its_position_before_newly_committed_text() {
    let mut app = app(vec![entry(1, EntryKind::Assistant, "Earlier answer")]);
    app.pane_mut().unwrap().view.as_mut().unwrap().sequence = 1_000;
    app.sequence_floor = 1_100;
    app.accept_live(std::sync::Arc::new(bob_kernel::LiveSnapshot {
        instance: "runtime-one".into(),
        revision: 1,
        sequence: 1_200,
        error: None,
        attempts: Vec::new(),
    }));
    command_result(
        &mut app,
        "/usage",
        Ok(Outcome::Notice("Session usage\n0 requests".into())),
    );
    assert_eq!(app.pane().unwrap().notices[0].sequence, 1);
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(
            1_201,
            vec![
                entry(1, EntryKind::Assistant, "Earlier answer"),
                entry(2, EntryKind::Assistant, "New final answer"),
            ],
        )),
    });
    let rows = render::transcript(app.pane().unwrap(), true, 120);
    let lines: Vec<_> = rows.iter().map(|row| row.line.to_string()).collect();
    assert_eq!(
        lines,
        [
            "Earlier answer",
            "",
            "Session usage",
            "0 requests",
            "",
            "New final answer",
            ""
        ]
    );
    assert!(matches!(
        rows[2].anchor.identity,
        AnchorId::Notice { sequence: 1, .. }
    ));
    render(&mut app, 120, 24);
    let builds = app.pane().unwrap().render_builds;
    render(&mut app, 120, 24);
    assert_eq!(app.pane().unwrap().render_builds, builds);
    assert_eq!(app.pane().unwrap().notices.len(), 1);
    assert_eq!(app.pane().unwrap().view.as_ref().unwrap().entries.len(), 2);
}

#[test]
fn yolo_notices_are_separate_full_contrast_and_bottom_padded() {
    let mut app = app(vec![entry(1, EntryKind::Assistant, "Earlier answer")]);
    for (sequence, detail) in [
        (1_001, "YOLO on — permission prompts bypassed; explicit denials and plan mode still apply. /yolo to stop."),
        (1_002, "YOLO off — permission prompts restored."),
    ] {
        command_result(&mut app, "/yolo", Ok(Outcome::Receipt(Receipt {
            detail: detail.into(),
            ..receipt(sequence)
        })));
    }
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(
            1_003,
            vec![
                entry(1, EntryKind::Assistant, "Earlier answer"),
                entry(2, EntryKind::Assistant, "Later answer"),
            ],
        )),
    });
    for width in [40, 80, 120] {
        let rows = render::transcript(app.pane().unwrap(), true, width);
        let notices: Vec<_> = app.pane().unwrap().notices.iter().collect();
        for notice in &notices {
            assert_eq!(notice.sequence, 1);
            let rows: Vec<_> = rows
                .iter()
                .filter(|row| {
                    row.anchor.identity
                        == AnchorId::Notice {
                            id: notice.id,
                            sequence: notice.sequence,
                        }
                })
                .collect();
            assert!(rows.len() >= 2);
            assert_eq!(rows.last().unwrap().line.to_string(), "");
            assert!(rows[..rows.len() - 1].iter().all(|row| {
                row.line.style.fg == Some(Palette::TEXT())
                    || row
                        .line
                        .spans
                        .iter()
                        .all(|span| span.style.fg == Some(Palette::TEXT()))
            }));
        }
        assert_ne!(notices[0].id, notices[1].id);
        let text = rows
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.find("YOLO on").unwrap() < text.find("YOLO off").unwrap());
        assert!(text.find("YOLO off").unwrap() < text.find("Later answer").unwrap());
    }
}

#[test]
fn historical_notice_bounds_exclude_old_and_future_anchors() {
    let mut app = app(vec![entry(1, EntryKind::Assistant, "First")]);
    let pane = app.pane_mut().unwrap();
    pane.notify(10, "Old notice");
    pane.notify(25, "In-page notice");
    pane.notify(50, "Future notice");
    pane.view = Some(view(
        100,
        vec![
            entry(20, EntryKind::Assistant, "Page start"),
            entry(30, EntryKind::Assistant, "Page end"),
        ],
    ));
    pane.view.as_mut().unwrap().entry_sequence = 100;
    pane.before = Some(31);
    let rows = render::transcript(pane, true, 120);
    let lines: Vec<_> = rows.iter().map(|row| row.line.to_string()).collect();
    assert_eq!(
        lines,
        ["Page start", "", "In-page notice", "", "Page end", ""]
    );
    assert!(rows.iter().all(|row| row
        .anchor
        .identity
        .sequence()
        .is_none_or(|sequence| sequence < 31)));
    pane.before = None;
    pane.pinned_before = Some(31);
    assert_eq!(pane.visible_notices().count(), 1);
    pane.pinned_before = None;
    assert_eq!(pane.visible_notices().count(), 2);
    pane.view.as_mut().unwrap().entry_sequence = 40;
    assert_eq!(pane.visible_notices().count(), 1);
}

#[test]
fn historical_commands_anchor_at_authoritative_global_head_not_page_tail() {
    let mut app = app(vec![
        entry(1, EntryKind::Assistant, "Old page start"),
        entry(2, EntryKind::Assistant, "Old page end"),
    ]);
    let pane = app.pane_mut().unwrap();
    pane.view.as_mut().unwrap().sequence = 1_000;
    pane.view.as_mut().unwrap().entry_sequence = 50;
    pane.before = Some(3);
    command_result(
        &mut app,
        "/usage",
        Ok(Outcome::Notice("Session usage".into())),
    );
    command_result(&mut app, "/yolo", Ok(Outcome::Receipt(receipt(1_001))));
    let pane = app.pane_mut().unwrap();
    assert!(pane.notices.iter().all(|notice| notice.sequence == 50));
    assert_eq!(pane.visible_notices().count(), 0);
    pane.before = None;
    pane.pinned_before = Some(3);
    assert_eq!(pane.visible_notices().count(), 0);
    pane.pinned_before = None;
    pane.view = None;
    pane.notify_current("Notice while page is evicted");
    assert_eq!(pane.notices.back().unwrap().sequence, 50);
    pane.view = Some(view(
        1_002,
        vec![
            entry(2, EntryKind::Assistant, "Old page end"),
            entry(51, EntryKind::Assistant, "Later answer"),
        ],
    ));
    assert_eq!(pane.visible_notices().count(), 3);
    let rows = render::transcript(pane, true, 120);
    let lines: Vec<_> = rows.iter().map(|row| row.line.to_string()).collect();
    let notice = lines
        .iter()
        .position(|line| line == "Session usage")
        .unwrap();
    let answer = lines
        .iter()
        .position(|line| line == "Later answer")
        .unwrap();
    assert!(notice < answer);
    assert_eq!(pane.view.as_ref().unwrap().entries.len(), 2);
    pane.pinned_before = Some(50);
    assert_eq!(pane.visible_notices().count(), 0);
    pane.pinned_before = Some(51);
    assert_eq!(pane.visible_notices().count(), 3);
    pane.pinned_before = None;
    pane.view.as_mut().unwrap().entries.remove(0);
    assert_eq!(pane.visible_notices().count(), 0);
}

#[test]
fn notices_survive_pane_eviction_without_leaking_to_other_threads() {
    let mut app = app(vec![entry(1, EntryKind::Assistant, "Root answer")]);
    app.pane_mut().unwrap().notify(1, "Root notice");
    for index in 0..12 {
        app.select(ThreadId(format!("child-{index}")));
        app.pane_mut().unwrap().view = Some(view(1, Vec::new()));
        assert!(render::transcript(app.pane().unwrap(), false, 80).is_empty());
    }
    let root = app.panes.get(&ThreadId("root".into())).unwrap();
    assert!(root.view.is_none());
    assert_eq!(root.notices.len(), 1);
    app.select(ThreadId("root".into()));
    let generation = app.pane().unwrap().generation;
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation,
        result: Ok(view(
            2,
            vec![
                entry(1, EntryKind::Assistant, "Root answer"),
                entry(2, EntryKind::Assistant, "Next answer"),
            ],
        )),
    });
    let lines: Vec<_> = render::transcript(app.pane().unwrap(), true, 80)
        .iter()
        .map(|row| row.line.to_string())
        .collect();
    assert_eq!(
        lines,
        ["Root answer", "", "Root notice", "", "Next answer", ""]
    );
}

#[test]
fn notice_history_is_bounded_and_cleared_on_runtime_reset() {
    let mut app = app(Vec::new());
    app.live_instance = Some("old-runtime".into());
    let pane = app.pane_mut().unwrap();
    for _ in 0..100 {
        pane.notify(1, "x".repeat(10_000));
    }
    assert_eq!(pane.notices.len(), 64);
    assert!(pane.notices.iter().all(|notice| notice.text.len() < 2100));
    assert_eq!(pane.notices.front().unwrap().id, 37);
    app.accept_live(std::sync::Arc::new(bob_kernel::LiveSnapshot {
        instance: "new-runtime".into(),
        revision: 1,
        sequence: 0,
        error: None,
        attempts: Vec::new(),
    }));
    let pane = app.pane_mut().unwrap();
    assert!(pane.notice.is_empty());
    assert!(pane.notices.is_empty());
    pane.view = Some(view(1, Vec::new()));
    assert!(render::transcript(pane, true, 80).is_empty());
}

#[test]
fn local_notice_splits_exploration_before_subsequent_tools() {
    let mut app = app(Vec::new());
    app.pane_mut().unwrap().notify(3, "Session usage");
    let entries = (1..=6)
        .map(|sequence| {
            let mut entry = entry(sequence, EntryKind::ToolCall, "");
            let operation = OperationId(format!("read-{sequence}"));
            entry.operation = Some(operation.clone());
            entry.tool = Some(ToolSummary {
                id: operation,
                name: "read_file".into(),
                state: OperationState::Done,
                input_preview: json!({"path":format!("file-{sequence}.rs")}),
                presentation: None,
                result: None,
            });
            entry
        })
        .collect();
    app.pane_mut().unwrap().view = Some(view(6, entries));
    let rows = render::transcript(app.pane().unwrap(), true, 120);
    let groups: Vec<_> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.line.to_string().contains("Explored 3 files"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(groups.len(), 2);
    let notice = rows
        .iter()
        .position(|row| row.line.to_string() == "Session usage")
        .unwrap();
    assert!(groups[0] < notice && notice < groups[1]);
    assert_eq!(rows[notice + 1].line.to_string(), "");
}

#[test]
fn command_failures_append_once_and_stale_replies_do_not_emit() {
    let mut app = app(vec![entry(1, EntryKind::Assistant, "Before")]);
    command_result(
        &mut app,
        "/usage",
        Ok(Outcome::Notice("Session usage".into())),
    );
    command_result(&mut app, "/invalid", Err("useful failure".into()));
    app.reply(Reply::Command {
        thread: ThreadId("root".into()),
        submitted: "/stale".into(),
        revision: 0,
        focus_epoch: 0,
        epoch: 1,
        inspection_epoch: 0,
        result: Err("must not be shown".into()),
    });
    assert_eq!(app.pane().unwrap().notices.len(), 2);
    let rows = render::transcript(app.pane().unwrap(), true, 120);
    let lines: Vec<_> = rows.iter().map(|row| row.line.to_string()).collect();
    assert_eq!(
        lines,
        [
            "Before",
            "",
            "Session usage",
            "",
            "Command failed: useful failure",
            "Submitted: /invalid",
            ""
        ]
    );
}

fn thread(name: &str) -> ThreadSummary {
    ThreadSummary {
        id: ThreadId(name.into()),
        session: SessionId("session".into()),
        parent_run: None,
        parent_thread: None,
        needs_approval: false,
        name: name.into(),
        model: "test-model".into(),
        state: ThreadState::Active,
        activity: None,
        run: Some(RunId(format!("run-{name}"))),
    }
}
fn entry(sequence: i64, kind: EntryKind, text: &str) -> Entry {
    Entry {
        sequence,
        sender: None,
        parent_guidance: false,
        submission: None,
        thread: ThreadId("root".into()),
        kind,
        text: text.into(),
        artifact: None,
        operation: None,
        tool: None,
        is_preview: false,
        visible_text_bytes: None,
        thinking: None,
    }
}
fn view(sequence: i64, entries: Vec<Entry>) -> View {
    View {
        sequence,
        entry_sequence: entries
            .iter()
            .map(|entry| entry.sequence)
            .max()
            .unwrap_or(0),
        entries,
        threads: vec![thread("root")],
        policies: vec![],
        decisions: vec![],
        operations: vec![],
        error: None,
    }
}
fn app(entries: Vec<Entry>) -> App {
    let mut app = App::new(80);
    app.select(ThreadId("root".into()));
    app.threads = vec![thread("root")];
    app.pane_mut().unwrap().view = Some(view(1, entries));
    app
}
fn render(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    assert!(buffer
        .content
        .iter()
        .all(|cell| !cell.symbol().chars().any(char::is_control)));
    buffer
}
fn screen(buffer: &Buffer) -> String {
    buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
fn decision(revision: i64, operation: &str) -> Decision {
    Decision {
        thread: ThreadId("root".into()),
        operation: OperationId(operation.into()),
        revision,
        name: "edit_file".into(),
        preview: "Prepared replacement in src/main.rs".into(),
        presentation: Some(ToolPresentation {
            title: "Edit".into(),
            target: Some("src/main.rs".into()),
            body: ToolBody::Edit {
                old_string: "old".into(),
                new_string: "new".into(),
            },
            details: vec!["Exact replacement prepared".into()],
        }),
    }
}
fn refresh(app: &mut App, sequence: i64, decisions: Vec<Decision>) {
    let mut view = view(sequence, vec![]);
    view.decisions = decisions;
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view),
    });
}
fn install_options(app: &mut App, options: Vec<bob_kernel::ApprovalOption>) {
    let key = DecisionKey::from(&app.pane().unwrap().permission.as_ref().unwrap().decision);
    app.reply(Reply::ApprovalOptions {
        key: key.clone(),
        result: Ok(bob_kernel::ApprovalOptions {
            operation: key.operation,
            thread: key.thread,
            revision: key.revision,
            options,
        }),
    });
}
fn load_options(app: &mut App) {
    install_options(app, approval::initial_options());
}
fn receipt(sequence: i64) -> Receipt {
    Receipt {
        session: SessionId("session".into()),
        thread: ThreadId("root".into()),
        sequence,
        detail: "Accepted".into(),
    }
}
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

#[test]
fn narrow_sidebar_keeps_the_conversation_readable() {
    let mut app = app(vec![entry(
        1,
        EntryKind::Assistant,
        "The conversation stays readable beside agents.",
    )]);
    app.sidebar = true;
    app.focus = Focus::Sidebar;
    let text = screen(&render(&mut app, 72, 24));
    assert!(text.contains("conversation"), "{text}");
    assert!(text.contains("readable"), "{text}");
    assert!(text.contains("AGENTS"));
}

#[test]
fn transcript_user_bands_markdown_and_neutral_tool_outputs() {
    let mut call = entry(
        3,
        EntryKind::ToolCall,
        "raw operation should not be rendered",
    );
    call.operation = Some(OperationId("op".into()));
    call.tool = Some(ToolSummary {
        id: OperationId("op".into()),
        name: "bash".into(),
        state: OperationState::Failed,
        presentation: None,
        result: None,
        input_preview: json!({"command":"cargo check"}),
    });
    let mut result = entry(4, EntryKind::ToolError, "compiler output stays neutral");
    result.operation = call.operation.clone();
    result.tool = call.tool.clone();
    let mut app = app(vec![
        entry(1, EntryKind::Submission, "Please inspect"),
        entry(
            2,
            EntryKind::Assistant,
            "## Findings\n\n**Important** and `inline`\n\n```rust\nlet value = 42;\n```",
        ),
        call,
        result,
    ]);
    let buffer = render(&mut app, 100, 34);
    let text = screen(&buffer);
    assert!(text.contains("› Please inspect"));
    assert!(text.contains("Important and inline"));
    assert_eq!(text.matches("Bash").count(), 1);
    let row = text
        .lines()
        .position(|line| line.contains("Please inspect"))
        .unwrap() as u16;
    assert_eq!(buffer[(4, row)].bg, Palette::INPUT_BG());
    assert_eq!(buffer[(4, row - 1)].bg, Palette::INPUT_BG());
    assert_eq!(buffer[(4, row + 1)].bg, Palette::INPUT_BG());
    assert_eq!(buffer[(3, row)].bg, Palette::BG());
    assert_eq!(buffer[(5, row)].symbol(), "›");
    assert_eq!(buffer[(5, row)].fg, Palette::DIM());
    assert_eq!(buffer[(7, row)].symbol(), "P");
    assert_eq!(buffer[(0, row)].bg, Palette::BG());
    let output_row = text
        .lines()
        .position(|line| line.contains("compiler output"))
        .unwrap() as u16;
    assert_eq!(buffer[(8, output_row)].fg, Palette::FAINT());
    let tool_row = text.lines().position(|line| line.contains("Bash")).unwrap() as u16;
    assert_eq!(buffer[(4, tool_row)].fg, Palette::ERROR());
    assert!(buffer[(6, tool_row)].modifier.contains(Modifier::BOLD));
    assert_eq!(buffer[(4, tool_row)].symbol(), "•");
    assert!(text
        .lines()
        .nth(tool_row as usize)
        .unwrap()
        .contains("Bash cargo check"));
    assert_eq!(buffer[(11, tool_row)].symbol(), "c");
    let important_row = text
        .lines()
        .position(|line| line.contains("Important"))
        .unwrap() as u16;
    assert!(buffer[(4, important_row)].modifier.contains(Modifier::BOLD));
    assert!(buffer
        .content
        .iter()
        .any(|cell| cell.bg == Palette::INPUT_BG() && cell.symbol() == "l"));
}

#[test]
fn historical_result_uses_identity_metadata_and_unknown_is_not_success() {
    let mut result = entry(8, EntryKind::ToolError, "partial output");
    result.operation = Some(OperationId("outside-page".into()));
    result.tool = Some(ToolSummary {
        id: result.operation.clone().unwrap(),
        name: "bash".into(),
        state: OperationState::Unknown,
        presentation: None,
        result: None,
        input_preview: json!({"command":"slow command"}),
    });
    let mut app = app(vec![entry(7, EntryKind::Assistant, ""), result]);
    let buffer = render(&mut app, 100, 20);
    let text = screen(&buffer);
    assert!(text.contains("Bash"));
    assert!(text.contains("unknown outcome"));
    assert!(text.contains("slow command"));
    assert!(text.contains("partial output"));
    let row = text.lines().position(|line| line.contains("Bash")).unwrap() as u16;
    assert_eq!(buffer[(4, row)].fg, Palette::ERROR());
    assert_eq!(buffer[(11, row)].symbol(), "s");
    assert!(text
        .lines()
        .nth(row as usize)
        .unwrap()
        .contains("Bash slow command"));
    assert_eq!(
        app.pane()
            .unwrap()
            .viewport
            .rows
            .iter()
            .filter(|row| row.identity == AnchorId::Entry(7))
            .count(),
        0
    );
}

#[test]
fn root_coordination_is_hidden_and_children_keep_failure_content() {
    let mut coordination = entry(1, EntryKind::ToolError, "delivery failed");
    coordination.operation = Some(OperationId("send".into()));
    coordination.tool = Some(ToolSummary {
        id: OperationId("send".into()),
        name: "send_message".into(),
        state: OperationState::Failed,
        presentation: None,
        result: None,
        input_preview: json!({"text":"hello"}),
    });
    let mut app = app(vec![
        coordination,
        entry(2, EntryKind::Coordination, "inspectable child message"),
    ]);
    let root = screen(&render(&mut app, 100, 20));
    assert!(!root.contains("delivery failed"));
    let rows = render::transcript(app.pane().unwrap(), false, 100);
    assert!(rows
        .iter()
        .any(|row| row.line.to_string().contains("delivery failed")));
    assert!(rows
        .iter()
        .any(|row| row.line.to_string().contains("inspectable child message")));
    let rows = render::transcript(app.pane().unwrap(), true, 100);
    assert!(!rows
        .iter()
        .any(|row| row.line.to_string().contains("inspectable child message")));
}

#[test]
fn narrow_and_short_approvals_keep_choices_and_hide_preparation_details() {
    for (name, title, body) in [
        (
            "edit_file",
            "Edit",
            ToolBody::Edit {
                old_string: "old value".into(),
                new_string: "new value".into(),
            },
        ),
        (
            "create_file",
            "Create",
            ToolBody::Create {
                content: "fn main() {}".into(),
            },
        ),
        (
            "bash",
            "Bash",
            ToolBody::Command {
                command: "cargo check".into(),
            },
        ),
    ] {
        for (width, height) in [(24, 10), (32, 12), (80, 10), (80, 24)] {
            let mut app = app(vec![]);
            let mut decision = decision(1, "frozen-operation");
            decision.name = name.into();
            decision.preview = "raw preparation fingerprint ".repeat(100);
            decision.presentation = Some(ToolPresentation {
                title: title.into(),
                target: (name != "bash").then(|| "src/main.rs".into()),
                body: body.clone(),
                details: vec!["diagnostic frozen fingerprint ".repeat(100)],
            });
            assert_eq!(approval::title(&decision), format!("Allow {title}?"));
            refresh(&mut app, 2, vec![decision]);
            load_options(&mut app);
            app.draft_mut().insert("keep draft");
            let text = screen(&render(&mut app, width, height));
            assert!(text.contains(&format!("Allow {title}?")), "{text}");
            match &body {
                ToolBody::Edit { .. } => {
                    assert!(text.contains("− old value"), "{text}");
                    assert!(text.contains("+ new value"), "{text}");
                }
                ToolBody::Create { .. } => assert!(text.contains("+ fn main() {}"), "{text}"),
                ToolBody::Command { .. } => assert!(text.contains("$ cargo check"), "{text}"),
                ToolBody::Text { .. } => unreachable!(),
            }
            assert!(text.contains("Allow once"), "{width}x{height}: {text}");
            assert!(text.contains("Deny"), "{width}x{height}: {text}");
            assert!(!text.contains("diagnostic"), "{text}");
            assert!(!text.contains("fingerprint"), "{text}");
            assert!(!text.contains("frozen-operation"), "{text}");

            let mut jobs = JoinSet::new();
            assert_eq!(app.focus, Focus::Permission);
            assert_eq!(
                app.pane().unwrap().permission.as_ref().unwrap().choice,
                Some(0)
            );
            for (choice, index, label) in [('a', 0, "Allow once"), ('d', 1, "Deny")] {
                app.key(key(KeyCode::Char(choice)), &mut jobs);
                assert_eq!(
                    app.pane().unwrap().permission.as_ref().unwrap().choice,
                    Some(index)
                );
                let text = screen(&render(&mut app, width, height));
                assert!(
                    text.contains(&format!("❯ {}. {label}", index + 1)),
                    "{text}"
                );
                assert!(
                    text.contains("Allow once") && text.contains("Deny"),
                    "{text}"
                );
            }
            app.key(key(KeyCode::Char('i')), &mut jobs);
            assert!(app.pane().unwrap().permission.as_ref().unwrap().details);
            let mut text = screen(&render(&mut app, width, height));
            for _ in 0..8 {
                if text.contains("diagnostic") {
                    break;
                }
                app.key(key(KeyCode::PageDown), &mut jobs);
                text = screen(&render(&mut app, width, height));
            }
            assert!(text.contains("diagnostic"), "{width}x{height}: {text}");
            assert!(
                text.contains("Allow once") && text.contains("Deny"),
                "{text}"
            );
            app.key(key(KeyCode::Char('i')), &mut jobs);
            assert!(!app.pane().unwrap().permission.as_ref().unwrap().details);
            assert!(!screen(&render(&mut app, width, height)).contains("diagnostic"));
            assert_eq!(app.draft_mut().text, "keep draft");
            assert!(jobs.is_empty());
        }
    }
    let mut fallback = decision(1, "fallback");
    fallback.presentation = None;
    assert_eq!(approval::title(&fallback), "Allow edit file?");
}

#[test]
fn structured_tool_body_is_readable_while_raw_output_stays_inspectable() {
    let raw = r#"{"stdout":{"text":"compiler detail"},"exit_code":101}"#;
    let mut result = entry(1, EntryKind::ToolError, raw);
    let operation = OperationId("structured-shell".into());
    result.operation = Some(operation.clone());
    result.tool = Some(ToolSummary {
        id: operation.clone(),
        name: "bash".into(),
        state: OperationState::Failed,
        result: None,
        input_preview: json!({"command":"cargo check"}),
        presentation: Some(ToolResultPresentation {
            summary: None,
            body_preview: Some("compiler detail".into()),
            exit_code: Some(101),
        }),
    });
    let mut app = app(vec![result]);
    let text = screen(&render(&mut app, 100, 25));
    assert!(text.contains("compiler detail"));
    assert!(text.contains("exit 101"));
    assert!(!text.contains("\"stdout\""));
    app.pane_mut().unwrap().expanded.insert(operation.clone());
    let text = screen(&render(&mut app, 100, 25));
    assert!(text.contains("compiler detail"));
    assert!(!text.contains("\"stdout\""));
    assert_eq!(
        app.tool_inspection(&operation),
        Some(InspectionTarget::ToolOutput(operation.clone()))
    );
    assert!(app.hits.iter().any(|hit| matches!(
        &hit.action, HitAction::Output(id) if id == &operation
    )));
    app.pane_mut().unwrap().inspector = Some(Inspector {
        target: InspectionTarget::Entry(1),
        previous: Default::default(),
        title: "Raw tool result".into(),
        text: raw.into(),
        offset: 0,
        next: None,
        total: raw.len(),
        scroll: 0,
        pending: false,
    });
    app.focus = Focus::Inspector;
    assert!(screen(&render(&mut app, 100, 25)).contains("\"stdout\""));
    assert_eq!(
        app.pane().unwrap().view.as_ref().unwrap().entries[0].text,
        raw
    );
}

#[test]
fn huge_single_line_tool_output_is_visually_capped_and_neutral() {
    let mut result = entry(1, EntryKind::ToolError, &"q".repeat(20_000));
    result.operation = Some(OperationId("huge-output".into()));
    result.tool = Some(ToolSummary {
        id: result.operation.clone().unwrap(),
        name: "bash".into(),
        state: OperationState::Failed,
        result: None,
        input_preview: json!({"command":"produce output"}),
        presentation: Some(ToolResultPresentation {
            summary: None,
            body_preview: None,
            exit_code: Some(1),
        }),
    });
    for width in [40, 80] {
        let mut app = app(vec![result.clone()]);
        let rows = render::transcript(app.pane().unwrap(), true, width);
        let body: Vec<_> = rows
            .iter()
            .filter(|row| row.line.to_string().contains('q'))
            .collect();
        assert_eq!(body.len(), 6);
        assert!(rows.len() <= 11, "collapsed tool has {} rows", rows.len());
        assert!(body
            .iter()
            .all(|row| row.line.to_string().starts_with("  │ q")));
        let buffer = render(&mut app, width, 30);
        let text = screen(&buffer);
        assert_eq!(text.lines().filter(|line| line.contains('q')).count(), 6);
        assert!(text.contains("... more output"), "{text}");
        let footer = text
            .lines()
            .position(|line| line.contains("... more output"))
            .unwrap() as u16;
        assert!(app.hits.iter().any(|hit| {
            hit.area.contains((8, footer).into())
                && matches!(&hit.action, HitAction::Tool(id) if id.0 == "huge-output")
        }));
        for cell in buffer.content.iter().filter(|cell| cell.symbol() == "q") {
            assert_eq!(cell.fg, Palette::FAINT());
        }
        let header = text.lines().position(|line| line.contains("Bash")).unwrap() as u16;
        assert_eq!(buffer[(4, header)].fg, Palette::ERROR());
        assert_eq!(buffer[(11, header)].symbol(), "p");
        let exit = text
            .lines()
            .position(|line| line.contains("exit 1"))
            .unwrap() as u16;
        assert_eq!(buffer[(8, exit)].fg, Palette::ERROR());
        app.pane_mut()
            .unwrap()
            .expanded
            .insert(OperationId("huge-output".into()));
        let expanded = render::transcript(app.pane().unwrap(), true, width);
        assert!(
            expanded
                .iter()
                .filter(|row| row.line.to_string().contains('q'))
                .count()
                > 6
        );
    }
}

#[test]
fn wrapped_command_starts_on_header_with_hanging_indent() {
    let mut call = entry(1, EntryKind::ToolCall, "raw input");
    call.operation = Some(OperationId("wrapped-command".into()));
    let command = format!("printf {}", "c".repeat(48));
    call.tool = Some(ToolSummary {
        id: call.operation.clone().unwrap(),
        name: "bash".into(),
        state: OperationState::Unknown,
        result: None,
        input_preview: json!({"command":command}),
        presentation: None,
    });
    let continuation_color = bob_ui::highlight::highlight_line(&command, "sh")
        .into_iter()
        .find(|span| span.content.contains('c'))
        .unwrap()
        .style
        .fg
        .unwrap();
    let mut app = app(vec![call]);
    let buffer = render(&mut app, 40, 20);
    let text = screen(&buffer);
    let header = text.lines().position(|line| line.contains("Bash")).unwrap() as u16;
    assert!(text.contains("Bash"));
    assert!(text.contains("unknown outcome"));
    assert_eq!(buffer[(11, header)].symbol(), "p");
    assert_eq!(buffer[(11, header + 1)].symbol(), "c");
    for y in [header + 1, header + 2] {
        for x in 4..11 {
            assert_eq!(buffer[(x, y)].symbol(), " ");
        }
        assert_eq!(buffer[(11, y)].fg, continuation_color);
    }
}

#[test]
fn failed_permission_requires_a_fresh_authoritative_view_before_retry() {
    for committed in [false, true] {
        let mut app = app(vec![]);
        refresh(&mut app, 10, vec![decision(3, "op")]);
        load_options(&mut app);
        app.draft_mut().insert("/quit");
        app.focus = Focus::Permission;
        let (captured, _) = app.take_decision(true).unwrap();
        app.reply(Reply::Decision {
            key: captured.clone(),
            result: Err("commit outcome unknown".into()),
        });
        let generation = app.pane().unwrap().generation;
        assert!(app.pane().unwrap().decision_refresh);
        assert!(app.pane().unwrap().loaded_before.is_none());
        assert_eq!(app.pane().unwrap().decided.as_ref(), Some(&captured));
        assert!(app.take_decision(true).is_none());
        // A read started before the failed command cannot authorize a retry.
        refresh(&mut app, 10, vec![decision(3, "op")]);
        assert!(app.pane().unwrap().permission.is_none());
        app.reply(Reply::View {
            thread: ThreadId("root".into()),
            generation,
            result: Err("read unavailable".into()),
        });
        assert!(app.take_decision(true).is_none());
        let mut fresh = view(10, vec![]);
        if !committed {
            fresh.decisions.push(decision(3, "op"));
        }
        app.reply(Reply::View {
            thread: ThreadId("root".into()),
            generation,
            result: Ok(fresh),
        });
        assert!(!app.pane().unwrap().decision_refresh);
        assert_eq!(app.pane().unwrap().permission.is_some(), !committed);
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        let mut jobs = JoinSet::new();
        assert!(!app.key(key(KeyCode::Enter), &mut jobs));
        assert_eq!(app.draft_mut().text, "/quit");
        assert!(jobs.is_empty());
        if !committed {
            load_options(&mut app);
            assert_eq!(app.focus, Focus::PermissionInvalidated);
            app.key(key(KeyCode::Char('1')), &mut jobs);
            assert!(jobs.is_empty());
            app.key(key(KeyCode::Tab), &mut jobs);
            assert_eq!(app.focus, Focus::Permission);
            assert_eq!(
                app.pane().unwrap().permission.as_ref().unwrap().choice,
                Some(0)
            );
            assert!(!app.key(key(KeyCode::Enter), &mut jobs));
            assert!(app.take_decision(false).is_some());
        }
    }
}

#[test]
fn oversized_paste_is_atomic_and_byte_bounded_with_visible_feedback() {
    for loading in [false, true] {
        let mut app = if loading { App::new(80) } else { app(vec![]) };
        app.paste("keep 界 draft");
        app.draft_mut().key(key(KeyCode::Left));
        let text = app.draft_mut().text.clone();
        let cursor = app.draft_mut().cursor;
        let revision = app.draft_mut().revision;
        app.paste(&"界".repeat(composer::INPUT_LIMIT / 3));
        assert_eq!(app.draft_mut().text, text);
        assert_eq!(app.draft_mut().cursor, cursor);
        assert_eq!(app.draft_mut().revision, revision);
        assert!(screen(&render(&mut app, 100, 24)).contains("Input rejected: 64 KiB limit"));
        app.draft_mut().clear();
        app.paste(&"x".repeat(composer::INPUT_LIMIT - 3));
        app.paste("界");
        assert_eq!(
            app.draft_mut().resolved_text().unwrap().len(),
            composer::INPUT_LIMIT
        );
        let revision = app.draft_mut().revision;
        app.paste("x");
        app.draft_mut().key(key(KeyCode::Char('λ')));
        assert_eq!(
            app.draft_mut().resolved_text().unwrap().len(),
            composer::INPUT_LIMIT
        );
        assert_eq!(app.draft_mut().revision, revision);
        app.draft_mut().key(key(KeyCode::Backspace));
        app.paste("λ");
        assert_eq!(
            app.draft_mut().resolved_text().unwrap(),
            format!("{}λ", "x".repeat(composer::INPUT_LIMIT - 3))
        );
        assert!(!app.draft_mut().limit_reached);
    }
}

#[test]
fn composer_layout_cache_tracks_revision_cursor_width_and_sanitizes_once() {
    let mut draft = Draft::default();
    draft.insert("a界\t\u{1b}[2Jdraft\ne\u{301}");
    let first = composer::layout(&mut draft, 20);
    assert!(first
        .rows
        .iter()
        .all(|line| !line.to_string().chars().any(char::is_control)));
    assert!(std::sync::Arc::ptr_eq(
        &first,
        &composer::layout(&mut draft, 20)
    ));
    assert_eq!(draft.layout_builds, 1);
    draft.key(key(KeyCode::Left));
    let moved = composer::layout(&mut draft, 20);
    assert!(!std::sync::Arc::ptr_eq(&first, &moved));
    assert_ne!(
        (first.cursor_row, first.cursor_col),
        (moved.cursor_row, moved.cursor_col)
    );
    draft.insert("new");
    composer::layout(&mut draft, 20);
    composer::layout(&mut draft, 10);
    assert_eq!(draft.layout_builds, 4);
    composer::layout(&mut draft, 10);
    assert_eq!(draft.layout_builds, 4);
}

#[test]
fn authoritative_grant_options_install_default_without_changing_numeric_meaning() {
    let mut app = app(vec![]);
    let decision = decision(7, "grant-operation");
    let captured = DecisionKey::from(&decision);
    refresh(&mut app, 20, vec![decision]);
    app.draft_mut().insert("/quit");
    assert_eq!(app.focus, Focus::Permission);
    assert!(app.take_option(0).is_none());
    assert!(app.take_option(1).is_none());
    let command = format!("printf '%s' {}", "exact argument ".repeat(400));
    let grant = bob_kernel::PermissionGrant {
        tool: "bash".into(),
        shell_command: None,
        path_prefix: None,
        command: Some(command.clone()),
    };
    let mut options = approval::initial_options();
    options.insert(
        1,
        bob_kernel::ApprovalOption {
            label: "Always allow this exact command".into(),
            approve: true,
            grant: Some(grant),
        },
    );
    app.reply(Reply::ApprovalOptions {
        key: captured.clone(),
        result: Ok(bob_kernel::ApprovalOptions {
            operation: captured.operation.clone(),
            thread: captured.thread.clone(),
            revision: captured.revision,
            options,
        }),
    });
    assert_eq!(app.focus, Focus::Permission);
    assert_eq!(
        app.pane().unwrap().permission.as_ref().unwrap().choice,
        Some(0)
    );
    let mut jobs = JoinSet::new();
    assert_eq!(app.draft_mut().text, "/quit");
    assert!(jobs.is_empty());
    app.key(key(KeyCode::Char('2')), &mut jobs);
    assert_eq!(
        app.pane().unwrap().permission.as_ref().unwrap().choice,
        Some(1)
    );
    let (identity, decision) = app.take_option(1).unwrap();
    let Command::DecideWithGrant {
        operation,
        revision,
        approve,
        grant: Some(grant),
        ..
    } = decision
    else {
        panic!("atomic grant decision expected");
    };
    assert_eq!(identity, captured);
    assert_eq!(operation, captured.operation);
    assert_eq!(revision, 7);
    assert!(approve);
    assert_eq!(grant.command.as_deref(), Some(command.as_str()));
    assert!(app.take_option(1).is_none());
}

#[test]
fn stale_grant_options_cannot_replace_a_new_approval() {
    let mut app = app(vec![]);
    let old = decision(1, "old");
    let key = DecisionKey::from(&old);
    refresh(&mut app, 10, vec![old]);
    refresh(&mut app, 11, vec![decision(2, "new")]);
    app.reply(Reply::ApprovalOptions {
        key: key.clone(),
        result: Ok(bob_kernel::ApprovalOptions {
            operation: key.operation,
            thread: key.thread,
            revision: key.revision,
            options: vec![bob_kernel::ApprovalOption {
                label: "Wrong grant".into(),
                approve: true,
                grant: Some(bob_kernel::PermissionGrant {
                    tool: "bash".into(),
                    shell_command: None,
                    path_prefix: None,
                    command: Some("wrong".into()),
                }),
            }],
        }),
    });
    let permission = app.pane().unwrap().permission.as_ref().unwrap();
    assert_eq!(permission.decision.operation.0, "new");
    assert!(permission
        .options
        .iter()
        .all(|option| option.grant.is_none()));
    assert_eq!(permission.choice, None);
}

#[test]
fn permission_identity_revision_and_duplicate_confirmation_are_captured() {
    let mut app = app(vec![]);
    refresh(&mut app, 10, vec![decision(3, "first")]);
    app.focus = Focus::Permission;
    app.pane_mut().unwrap().permission.as_mut().unwrap().choice = Some(0);
    refresh(&mut app, 11, vec![decision(4, "first")]);
    assert_eq!(app.focus, Focus::PermissionInvalidated);
    assert_eq!(
        app.pane().unwrap().permission.as_ref().unwrap().choice,
        None
    );
    load_options(&mut app);
    assert_eq!(app.focus, Focus::PermissionInvalidated);
    let mut jobs = JoinSet::new();
    app.key(key(KeyCode::Enter), &mut jobs);
    app.key(key(KeyCode::Char('1')), &mut jobs);
    assert!(jobs.is_empty());
    let (captured, command) = app.take_decision(true).unwrap();
    let Command::DecideWithGrant {
        operation,
        revision,
        approve,
        ..
    } = command
    else {
        panic!("typed decision expected")
    };
    assert_eq!(operation.0, "first");
    assert_eq!(revision, 4);
    assert!(approve);
    assert!(app.take_decision(true).is_none());
    refresh(&mut app, 12, vec![decision(5, "second")]);
    app.pane_mut().unwrap().draft.insert("my next request");
    app.reply(Reply::Decision {
        key: captured,
        result: Ok(receipt(13)),
    });
    assert_eq!(
        app.pane()
            .unwrap()
            .permission
            .as_ref()
            .unwrap()
            .decision
            .operation
            .0,
        "second"
    );
    assert_eq!(app.draft_mut().text, "my next request");
    refresh(&mut app, 12, vec![decision(4, "first")]);
    assert_eq!(
        app.pane()
            .unwrap()
            .permission
            .as_ref()
            .unwrap()
            .decision
            .operation
            .0,
        "second"
    );
    refresh(&mut app, 14, vec![]);
    assert!(app.pane().unwrap().permission.is_none());
}

#[test]
fn late_receipt_cannot_clear_retyped_identical_text_and_floors_views() {
    let mut app = app(vec![]);
    app.draft_mut().insert("same text");
    let revision = app.draft_mut().revision;
    app.pane_mut().unwrap().pending = Some(2);
    app.draft_mut().clear();
    app.draft_mut().insert("same text");
    app.reply(Reply::Command {
        focus_epoch: 0,
        thread: ThreadId("root".into()),
        submitted: "same text".into(),
        revision,
        epoch: 2,
        inspection_epoch: 0,
        result: Ok(Outcome::Receipt(receipt(20))),
    });
    assert_eq!(app.draft_mut().text, "same text");
    refresh(&mut app, 19, vec![decision(1, "old")]);
    assert!(app.pane().unwrap().permission.is_none());
    assert_eq!(app.sequence_floor, 20);
}

#[test]
fn focus_prevents_paste_arrows_and_enter_from_leaking() {
    let mut app = app(vec![]);
    let mut jobs = JoinSet::new();
    app.draft_mut().insert("draft");
    refresh(&mut app, 10, vec![decision(1, "op")]);
    app.key(key(KeyCode::Enter), &mut jobs);
    assert!(!app.pane().unwrap().permission.as_ref().unwrap().pending);
    assert_eq!(app.focus, Focus::Permission);
    app.key(key(KeyCode::Tab), &mut jobs);
    assert_eq!(app.focus, Focus::Permission);
    app.key(key(KeyCode::Enter), &mut jobs);
    assert_eq!(
        app.pane().unwrap().permission.as_ref().unwrap().choice,
        None
    );
    for focus in [Focus::Permission, Focus::Sidebar, Focus::Inspector] {
        app.focus = focus;
        app.paste("accidental");
        app.key(key(KeyCode::Left), &mut jobs);
        app.key(key(KeyCode::Enter), &mut jobs);
        assert_eq!(app.draft_mut().text, "draft");
        assert_eq!(app.draft_mut().cursor, 5);
    }
    app.focus = Focus::Composer;
    app.paste("ignored during approval");
    assert_eq!(app.draft_mut().text, "draft");
    refresh(&mut app, 11, vec![]);
    assert_eq!(app.focus, Focus::PermissionInvalidated);
    app.key(key(KeyCode::Tab), &mut jobs);
    app.paste("\nmore");
    assert_eq!(app.draft_mut().text, "draft[Pasted text #1 +2 lines]");
    assert_eq!(
        app.draft_mut().resolved_text().as_deref(),
        Some("draft\nmore")
    );
    assert_eq!(app.focus, Focus::Composer);
    assert!(jobs.is_empty());
}

#[test]
fn switching_threads_invalidates_permission_choice_and_stale_inspection() {
    let mut app = app(vec![]);
    refresh(&mut app, 10, vec![decision(1, "op")]);
    app.pane_mut().unwrap().permission.as_mut().unwrap().choice = Some(0);
    app.pane_mut().unwrap().inspector = Some(Inspector {
        target: InspectionTarget::Entry(1),
        previous: Default::default(),
        title: "old".into(),
        text: "loading".into(),
        offset: 0,
        total: 0,
        next: None,
        scroll: 0,
        pending: true,
    });
    let epoch = app.pane().unwrap().inspection_epoch;
    app.focus = Focus::Inspector;
    app.draft_mut().insert("root draft");
    app.hits.push(Hit {
        area: Rect::new(0, 0, 10, 1),
        action: HitAction::Entry(1),
    });
    app.select(ThreadId("child".into()));
    app.draft_mut().insert("child draft");
    assert!(app.hits.is_empty());
    app.reply(Reply::Text {
        thread: ThreadId("root".into()),
        epoch,
        result: Ok(TextPage {
            text: "late".into(),
            next_offset: None,
            total_bytes: 4,
        }),
    });
    app.select(ThreadId("root".into()));
    assert_eq!(app.draft_mut().text, "root draft");
    assert!(app.pane().unwrap().inspector.is_none());
    assert_eq!(
        app.pane().unwrap().permission.as_ref().unwrap().choice,
        None
    );
    assert_eq!(
        app.panes[&ThreadId("child".into())].draft.text,
        "child draft"
    );
}

#[test]
fn dismissed_artifact_and_entry_page_replies_do_not_reopen_inspector() {
    for page_change in [false, true] {
        let mut app = app(vec![entry(2, EntryKind::Submission, "hello")]);
        let epoch = app.pane().unwrap().inspection_epoch;
        app.pane_mut().unwrap().pending = Some(7);
        if page_change {
            app.page(true);
        } else {
            app.key(key(KeyCode::Esc), &mut JoinSet::new());
        }
        app.reply(Reply::Command {
            focus_epoch: 0,
            thread: ThreadId("root".into()),
            submitted: "/artifact artifact".into(),
            revision: 0,
            epoch: 7,
            inspection_epoch: epoch,
            result: Ok(Outcome::Artifact(
                ArtifactId("artifact".into()),
                0,
                "late artifact".into(),
            )),
        });
        assert!(app.pane().unwrap().inspector.is_none());
        app.reply(Reply::Text {
            thread: ThreadId("root".into()),
            epoch,
            result: Ok(TextPage {
                text: "late entry".into(),
                next_offset: None,
                total_bytes: 10,
            }),
        });
        assert!(app.pane().unwrap().inspector.is_none());
        assert_eq!(app.focus, Focus::Composer);
    }
}

#[test]
fn scrolling_follows_bottom_but_reading_anchor_survives_resize_and_switch() {
    let entries = (1..=60)
        .map(|i| {
            entry(
                i,
                EntryKind::Submission,
                &format!("message {i}: {}", "wide 界 text ".repeat(12)),
            )
        })
        .collect();
    let mut app = app(entries);
    render(&mut app, 100, 20);
    assert!(app.pane().unwrap().viewport.anchor.is_none());
    app.pane_mut().unwrap().viewport.scroll(-20);
    let anchor = app.pane().unwrap().viewport.anchor.clone().unwrap();
    render(&mut app, 42, 16);
    assert_eq!(app.pane().unwrap().viewport.anchor, Some(anchor.clone()));
    let top = &app.pane().unwrap().viewport.rows[app.pane().unwrap().viewport.top];
    assert_eq!(top.identity, anchor.identity);
    assert_eq!(top.line, anchor.line);
    assert!(top.offset <= anchor.offset);
    app.select(ThreadId("child".into()));
    render(&mut app, 80, 20);
    app.select(ThreadId("root".into()));
    render(&mut app, 110, 30);
    assert_eq!(app.pane().unwrap().viewport.anchor, Some(anchor.clone()));
    app.pane_mut().unwrap().viewport.scroll(isize::MAX);
    render(&mut app, 100, 20);
    assert!(app.pane().unwrap().viewport.anchor.is_none());
}

#[test]
fn coordination_projects_sender_and_markdown_only_in_agent_views() {
    let mut message = entry(
        1,
        EntryKind::Coordination,
        "**Check this**\n\nUse `read_file`.",
    );
    message.sender = Some(bob_kernel::EntrySender {
        thread: ThreadId("sender-id".into()),
        name: Some("reviewer\u{1b}[31m\nagent".into()),
    });
    let app = app(vec![message.clone()]);
    let pane = app.pane().unwrap();
    let rows = render::transcript(pane, false, 100);
    assert_eq!(rows[0].line.to_string(), "Message from reviewer agent");
    assert!(rows[0].line.style.add_modifier.contains(Modifier::BOLD));
    let body = rows
        .iter()
        .find(|row| row.line.to_string().contains("Check this"))
        .unwrap();
    assert!(body.line.to_string().starts_with("   "));
    assert!(!body.line.to_string().contains("**"));
    assert!(body
        .line
        .spans
        .iter()
        .any(|span| span.style.add_modifier.contains(Modifier::BOLD)));
    assert!(rows
        .iter()
        .all(|row| !row.line.to_string().contains('\u{1b}')));
    assert!(render::transcript(pane, true, 100).is_empty());
    let original = pane.view.as_ref().unwrap();
    assert_eq!(original.entries[0].text, message.text);
    let mut renamed = original.clone();
    renamed.entries[0].sender.as_mut().unwrap().name = Some("new name".into());
    assert_ne!(render::content_key(original), render::content_key(&renamed));
}

#[test]
fn tiny_frames_and_untrusted_permission_previews_stay_inert() {
    let mut app = app(vec![entry(
        1,
        EntryKind::Assistant,
        "**hello**\n```sh\n\u{1b}]52;c;secret\u{7}\n```",
    )]);
    let mut permission = decision(1, "op");
    permission.preview = "\u{1b}[2J\u{9d}bad\u{9c}\nprepared change".into();
    permission.presentation = None;
    refresh(&mut app, 2, vec![permission]);
    load_options(&mut app);
    app.draft_mut().insert("界\t👩\u{200d}💻e\u{301}\u{1b}[2J");
    for (width, height) in [(1, 1), (12, 5), (24, 10), (40, 20), (120, 40)] {
        let buffer = render(&mut app, width, height);
        assert!(buffer
            .content
            .iter()
            .all(|cell| !cell.symbol().chars().any(char::is_control)));
    }
    app.focus = Focus::Permission;
    let buffer = render(&mut app, 100, 30);
    assert!(screen(&buffer).contains("Allow once"));
    assert!(screen(&buffer).contains("Deny"));
    assert!(screen(&buffer).contains("prepared change"));
}

#[test]
fn grapheme_editing_and_history_restore_unsent_draft() {
    let mut draft = Draft::default();
    draft.insert("a👩\u{200d}💻e\u{301}");
    draft.key(key(KeyCode::Left));
    draft.key(key(KeyCode::Backspace));
    assert_eq!(draft.text, "ae\u{301}");
    draft.clear();
    draft.insert("unsent");
    draft.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(draft.text, "ae\u{301}");
    draft.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert_eq!(draft.text, "unsent");
    let wrapped = style::wrap(&Line::raw("界👩\u{200d}💻e\u{301}"), 2);
    assert_eq!(
        wrapped
            .iter()
            .map(|(_, line)| line.to_string())
            .collect::<Vec<_>>(),
        ["界", "👩\u{200d}💻", "e\u{301}"]
    );
}

#[test]
fn sidebar_filters_session_sorts_finished_children_and_preserves_unknown() {
    let mut app = app(vec![]);
    for (name, state) in [
        ("done", ThreadState::Completed),
        ("uncertain", ThreadState::Interrupted),
        ("active", ThreadState::Active),
        ("failed", ThreadState::Failed),
        ("cancelled", ThreadState::Cancelled),
    ] {
        let mut child = thread(name);
        child.state = state;
        child.parent_thread = Some(ThreadId("root".into()));
        app.threads.push(child);
    }
    let mut other = thread("other session");
    other.session = SessionId("another".into());
    app.threads.push(other);
    let names: Vec<_> = app
        .visible_threads()
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["root", "active", "uncertain", "cancelled", "failed", "done"]
    );
    app.sidebar = true;
    let buffer = render(&mut app, 140, 30);
    let text = screen(&buffer);
    let y = text.lines().position(|line| line.contains("done")).unwrap() as u16;
    assert!(buffer[(95, y)].modifier.contains(Modifier::CROSSED_OUT));
    for name in ["failed", "cancelled"] {
        let y = text.lines().position(|line| line.contains(name)).unwrap() as u16;
        assert_eq!(buffer[(95, y)].fg, Palette::DIM());
        assert!(buffer[(95, y)].modifier.contains(Modifier::CROSSED_OUT));
    }
    let y = text
        .lines()
        .position(|line| line.contains("uncertain"))
        .unwrap() as u16;
    assert_eq!(buffer[(93, y)].fg, Palette::WARN());
    assert!(!buffer[(95, y)].modifier.contains(Modifier::CROSSED_OUT));
}

#[test]
fn view_cache_is_bounded_without_discarding_drafts() {
    let mut app = app(vec![]);
    for i in 0..20 {
        let id = ThreadId(format!("thread-{i}"));
        app.select(id);
        app.pane_mut().unwrap().view =
            Some(view(i, vec![entry(i, EntryKind::Assistant, "cached")]));
        app.draft_mut().insert("keep this draft");
    }
    app.select(ThreadId("root".into()));
    assert!(app.panes.values().filter(|p| p.view.is_some()).count() <= 8);
    assert_eq!(
        app.panes[&ThreadId("thread-0".into())].draft.text,
        "keep this draft"
    );
}

#[test]
fn invalidated_permission_cannot_submit_or_exit_with_a_waiting_draft() {
    for replacement in [vec![], vec![decision(2, "replacement")]] {
        let mut app = app(vec![]);
        refresh(&mut app, 10, vec![decision(1, "old")]);
        load_options(&mut app);
        app.draft_mut().insert("/quit");
        app.focus = Focus::Permission;
        let mut jobs = JoinSet::new();
        app.key(key(KeyCode::Left), &mut jobs);
        refresh(&mut app, 11, replacement);
        if app.pane().unwrap().permission.is_some() {
            load_options(&mut app);
        }
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        assert!(!app.key(key(KeyCode::Enter), &mut jobs));
        app.key(key(KeyCode::Char('1')), &mut jobs);
        app.paste("should not leak");
        assert_eq!(app.draft_mut().text, "/quit");
        assert!(jobs.is_empty());
        let text = screen(&render(&mut app, 100, 30));
        assert!(
            text.contains(if app.pane().unwrap().permission.is_some() {
                "Tab to choose"
            } else {
                "Permission changed or closed"
            }),
            "{text}"
        );
        if app.pane().unwrap().permission.is_some() {
            assert!(text.contains("Allow Edit?"), "{text}");
            assert!(text.contains("− old") && text.contains("+ new"), "{text}");
            assert!(
                text.contains("Allow once") && text.contains("Deny"),
                "{text}"
            );
        }
        app.key(key(KeyCode::Tab), &mut jobs);
        if app.pane().unwrap().permission.is_some() {
            assert_eq!(app.focus, Focus::Permission);
            assert_eq!(
                app.pane().unwrap().permission.as_ref().unwrap().choice,
                Some(0)
            );
        } else {
            assert_eq!(app.focus, Focus::Composer);
            assert!(app.key(key(KeyCode::Enter), &mut jobs));
        }
    }
}

#[test]
fn evicted_reading_position_reloads_pinned_page_and_frees_allocations() {
    let entries: Vec<_> = (1..=30)
        .map(|i| entry(i, EntryKind::Submission, &format!("message {i}")))
        .collect();
    let mut app = app(entries.clone());
    app.draft_mut().insert("sent history");
    app.draft_mut().clear();
    app.draft_mut().insert("unsent root draft");
    render(&mut app, 100, 20);
    app.pane_mut().unwrap().scroll(-20);
    let anchor = app.pane().unwrap().viewport.anchor.clone();
    assert!(anchor.is_some());
    assert_eq!(app.pane().unwrap().query_before(), Some(31));
    for i in 0..12 {
        app.select(ThreadId(format!("cached-{i}")));
        app.pane_mut().unwrap().view = Some(view(1, vec![entry(1, EntryKind::Assistant, "body")]));
        render(&mut app, 100, 20);
    }
    let root = &app.panes[&ThreadId("root".into())];
    assert!(root.view.is_none());
    assert!(root.rendered.is_none());
    assert_eq!(root.viewport.rows.capacity(), 0);
    assert_eq!(root.query_before(), Some(31));
    app.select(ThreadId("root".into()));
    let generation = app.pane().unwrap().generation;
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation,
        result: Ok(view(20, entries)),
    });
    assert!(app.pane().unwrap().view.is_some());
    render(&mut app, 60, 20);
    assert_eq!(app.pane().unwrap().viewport.anchor, anchor);
    app.draft_mut()
        .key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(app.draft_mut().text, "unsent root draft");
}

#[test]
fn artifact_reply_respects_newer_focus_intent_even_after_round_trip() {
    for return_to_composer in [false, true] {
        let mut app = app(vec![]);
        refresh(&mut app, 1, vec![decision(1, "op")]);
        app.pane_mut().unwrap().pending = Some(2);
        let inspection_epoch = app.pane().unwrap().inspection_epoch;
        let mut jobs = JoinSet::new();
        app.key(key(KeyCode::Tab), &mut jobs);
        if return_to_composer {
            app.key(key(KeyCode::Tab), &mut jobs);
        }
        let focus = app.focus;
        app.reply(Reply::Command {
            thread: ThreadId("root".into()),
            submitted: "/artifact retained".into(),
            revision: 0,
            focus_epoch: 0,
            epoch: 2,
            inspection_epoch,
            result: Ok(Outcome::Artifact(
                ArtifactId("retained".into()),
                0,
                "late body".into(),
            )),
        });
        assert_eq!(app.focus, focus);
        assert!(app.pane().unwrap().inspector.is_none());
    }
}

#[test]
fn previous_inspection_uses_exact_forward_offsets() {
    let mut inspector = Inspector {
        target: InspectionTarget::Entry(1),
        previous: Default::default(),
        title: String::new(),
        text: "界".repeat(5461),
        offset: 0,
        next: Some(16383),
        total: 65536,
        scroll: 0,
        pending: false,
    };
    let (_, offset, previous) = inspector.navigation(true).unwrap();
    inspector.offset = offset;
    inspector.previous = previous;
    inspector.next = Some(32766);
    let (_, offset, previous) = inspector.navigation(true).unwrap();
    inspector.offset = offset;
    inspector.previous = previous;
    assert_eq!(inspector.navigation(false).unwrap().1, 16383);
    assert_eq!(
        inspector
            .navigation(false)
            .unwrap()
            .2
            .into_iter()
            .collect::<Vec<_>>(),
        [0]
    );
}

#[test]
fn retained_tool_output_target_survives_page_separation() {
    let mut call = entry(1, EntryKind::ToolCall, "raw frozen input");
    let id = OperationId("op".into());
    call.operation = Some(id.clone());
    call.tool = Some(ToolSummary {
        id: id.clone(),
        name: "bash".into(),
        state: OperationState::Done,
        presentation: None,
        result: Some(ArtifactId("retained-output".into())),
        input_preview: json!({"command":"command"}),
    });
    let mut app = app(vec![call]);
    assert_eq!(
        app.tool_inspection(&id),
        Some(InspectionTarget::ToolOutput(id.clone()))
    );
    assert!(screen(&render(&mut app, 100, 20)).contains("Output outside this page"));
}

#[test]
fn shortened_whitespace_response_shows_no_loading_row() {
    let mut response = entry(8, EntryKind::Assistant, &" ".repeat(2048));
    response.is_preview = true;
    let mut app = app(vec![response]);
    let shown = screen(&render(&mut app, 100, 20));
    assert!(!shown.contains("Loading"), "{shown}");
    assert!(!shown.contains("click to inspect"));
}

#[test]
fn exact_width_composer_places_cursor_on_insertion_row() {
    for (width, height) in [
        (24, 10),
        (40, 20),
        (80, 24),
        (100, 30),
        (120, 40),
        (180, 48),
    ] {
        for content in [
            "x".repeat((width - 12) as usize),
            "界".repeat((width - 12) as usize / 2),
        ] {
            let mut app = app(vec![]);
            app.draft_mut().insert(&content);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            let cursor = terminal.get_cursor_position().unwrap();
            assert_eq!((cursor.x, cursor.y), (7, height - 3));
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(5, height - 4)].symbol(), "›");
            assert_eq!(buffer[(5, height - 4)].fg, Palette::DIM());
            assert_eq!(buffer[(5, height - 3)].symbol(), " ");
            assert_eq!(buffer[(7, height - 3)].symbol(), " ");
            for y in height - 5..height - 1 {
                for x in [4, width - 5] {
                    assert_eq!(buffer[(x, y)].bg, Palette::INPUT_BG());
                }
                for x in [3, width - 4] {
                    assert_eq!(buffer[(x, y)].bg, Palette::BG());
                }
            }
            assert!(app.hits.iter().any(|hit| {
                matches!(hit.action, HitAction::Composer)
                    && hit.area.contains((cursor.x, cursor.y).into())
            }));
            assert_eq!(app.draft_mut().text, content);
        }
    }
}

#[test]
fn composer_and_unchanged_refresh_reuse_markdown_cache() {
    let mut entries: Vec<_> = (1..20)
        .map(|i| {
            entry(
                i,
                EntryKind::Assistant,
                "## Heading\n**bold** and `code`\n```rust\nlet value = 42;\n```",
            )
        })
        .collect();
    let mut tool = entry(
        20,
        EntryKind::ToolResult,
        "one\ntwo\nthree\nfour\nfive\nsix",
    );
    tool.operation = Some(OperationId("tool".into()));
    tool.tool = Some(ToolSummary {
        id: OperationId("tool".into()),
        name: "bash".into(),
        state: OperationState::Done,
        presentation: None,
        input_preview: json!({"command":"check"}),
        result: None,
    });
    entries.push(tool);
    let mut app = app(entries.clone());
    render(&mut app, 100, 30);
    assert_eq!(app.pane().unwrap().render_builds, 1);
    app.key(key(KeyCode::Char('x')), &mut JoinSet::new());
    render(&mut app, 100, 30);
    assert_eq!(app.pane().unwrap().render_builds, 1);
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(2, entries.clone())),
    });
    render(&mut app, 100, 30);
    assert_eq!(app.pane().unwrap().render_builds, 1);
    entries[0].text.push_str("\nnew content");
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(3, entries)),
    });
    render(&mut app, 100, 30);
    assert_eq!(app.pane().unwrap().render_builds, 2);
    render(&mut app, 60, 30);
    assert_eq!(app.pane().unwrap().render_builds, 3);
    app.pane_mut()
        .unwrap()
        .expanded
        .insert(OperationId("tool".into()));
    render(&mut app, 60, 30);
    assert_eq!(app.pane().unwrap().render_builds, 4);
}

#[test]
fn jobs_and_queue_panels_filter_work_and_preserve_composer_geometry() {
    use bob_kernel::{JobId, JobKind, JobState, JobSummary, QueuedInput};
    let mut app = app(vec![entry(
        1,
        EntryKind::Assistant,
        "history stays visible",
    )]);
    app.draft_mut().insert("draft");
    let job = JobSummary {
        id: JobId("job_1".into()),
        session: SessionId("session".into()),
        thread: ThreadId("root".into()),
        operation: None,
        run: None,
        kind: JobKind::Shell,
        state: JobState::Running,
        title: "cargo check".into(),
        result: None,
        started_at: 0,
        background: true,
    };
    app.pane_mut().unwrap().jobs = vec![job.clone()];
    app.pane_mut().unwrap().queue = vec![QueuedInput {
        id: CommandId::new(),
        thread: ThreadId("root".into()),
        text: "next prompt".into(),
    }];
    for (width, height) in [(40, 20), (80, 24), (100, 30), (120, 40)] {
        let buffer = render(&mut app, width, height);
        let text = screen(&buffer);
        assert!(text.contains("background shell"));
        assert!(text.contains("queued · 1"));
        assert!(text.contains("next prompt"));
        assert_eq!(buffer[(5, height - 3)].symbol(), "›");
        assert_eq!(buffer[(7, height - 3)].symbol(), "d");
        assert_eq!(app.draft_mut().text, "draft");
    }
    for state in [
        JobState::Completed,
        JobState::Failed,
        JobState::Cancelled,
        JobState::Unknown,
    ] {
        app.pane_mut().unwrap().jobs[0].state = state;
        assert!(!screen(&render(&mut app, 100, 30)).contains("background shell"));
    }
    for kind in [JobKind::Task, JobKind::Workflow, JobKind::Turn] {
        app.pane_mut().unwrap().jobs[0] = JobSummary {
            kind,
            ..job.clone()
        };
        assert!(!screen(&render(&mut app, 100, 30)).contains("background shell"));
    }
    app.pane_mut().unwrap().queue.clear();
    assert!(!screen(&render(&mut app, 100, 30)).contains("queued ·"));
}

#[test]
fn full_command_reply_preserves_source_and_cannot_reopen_collapsed_output() {
    let id = OperationId("full-command".into());
    let mut call = entry(1, EntryKind::ToolCall, "raw source");
    call.operation = Some(id.clone());
    call.tool = Some(ToolSummary {
        id: id.clone(),
        name: "bash".into(),
        state: OperationState::Done,
        presentation: None,
        result: None,
        input_preview: json!({"command":"short preview"}),
    });
    let mut app = app(vec![call]);
    app.pane_mut().unwrap().expanded.insert(id.clone());
    app.pane_mut().unwrap().pending_inputs.insert(id.clone());
    let command = format!("printf {}", "source".repeat(1000));
    app.reply(Reply::ToolInput {
        thread: ThreadId("root".into()),
        operation: id.clone(),
        result: Ok(json!({"command":command})),
    });
    let rows = render::transcript(app.pane().unwrap(), true, 80);
    let restored = rows
        .iter()
        .filter_map(|row| {
            let line = row.line.to_string();
            line.strip_prefix("• Bash ")
                .or_else(|| line.strip_prefix("       "))
                .map(str::to_owned)
        })
        .collect::<String>();
    assert_eq!(restored, command);
    assert_eq!(
        app.pane().unwrap().view.as_ref().unwrap().entries[0]
            .tool
            .as_ref()
            .unwrap()
            .input_preview["command"],
        "short preview"
    );
    app.toggle_tool(id.clone(), &mut JoinSet::new());
    app.reply(Reply::ToolInput {
        thread: ThreadId("root".into()),
        operation: id.clone(),
        result: Ok(json!({"command":"late"})),
    });
    assert!(!app.pane().unwrap().expanded.contains(&id));
    assert!(app.pane().unwrap().full_inputs.is_empty());
}

#[test]
fn failed_child_reports_stay_in_child_views_without_exposing_internal_ids() {
    let id = uuid::Uuid::new_v4();
    let mut app = app(vec![
        entry(
            1,
            EntryKind::Coordination,
            &format!("Agent {id} failed: child failure evidence"),
        ),
        entry(
            2,
            EntryKind::Coordination,
            &format!("Agent {id} completed: routine report"),
        ),
    ]);
    let text = screen(&render(&mut app, 100, 20));
    assert!(!text.contains("child failure evidence"));
    assert!(!text.contains(&id.to_string()));
    assert!(!text.contains("routine report"));
    let rows = render::transcript(app.pane().unwrap(), false, 100);
    let failure = rows
        .iter()
        .find(|row| row.line.to_string().contains("child failure evidence"))
        .unwrap();
    assert!(!failure.line.to_string().contains(&id.to_string()));
    assert!(rows
        .iter()
        .any(|row| row.line.to_string().contains("routine report")));
    assert!(rows.iter().any(|row| row
        .line
        .spans
        .iter()
        .any(|span| span.style.fg == Some(Palette::ERROR()))));
}
