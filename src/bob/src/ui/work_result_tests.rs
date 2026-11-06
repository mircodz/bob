use super::super::{render, state::Pane};
use super::*;
use bob_kernel::{ArtifactId, OperationId, ThreadId, ToolSummary, View};
use ratatui::{backend::TestBackend, style::Modifier, widgets::Paragraph, Terminal};
use serde_json::json;

#[path = "archive_recall_tests.rs"]
mod archive_recall_tests;

fn agent(name: &str, status: &str, output: Value) -> Value {
    json!({
        "thread_id": format!("thread-{name}"), "name": name, "status": status,
        "output": output, "result_artifact": format!("artifact-{name}"),
        "output_omitted": false, "note": null
    })
}

fn entry(name: &str, source: &str) -> Entry {
    Entry {
        sequence: 1,
        sender: None,
        parent_guidance: false,
        submission: None,
        thread: ThreadId("root".into()),
        kind: EntryKind::ToolResult,
        text: source.into(),
        artifact: Some(ArtifactId("raw-result".into())),
        operation: Some(OperationId(name.into())),
        tool: Some(ToolSummary {
            id: OperationId(name.into()),
            name: name.into(),
            state: OperationState::Done,
            input_preview: json!({"title":"Review changes", "name":"reader", "description":"Inspect Unicode", "thread_id":"thread-reader"}),
            result: Some(ArtifactId("raw-result".into())),
            presentation: None,
        }),
        is_preview: false,
        visible_text_bytes: None,
    }
}

fn rows(entry: &Entry, expanded: bool, width: u16) -> Vec<Row> {
    let mut rows = Vec::new();
    tool(&mut rows, entry, Some(entry), expanded, width);
    rows
}

fn screen(rows: &[Row]) -> String {
    rows.iter()
        .map(|row| row.line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_color(rows: &[Row], needle: &str, color: ratatui::style::Color) -> bool {
    rows.iter()
        .flat_map(|row| &row.line.spans)
        .any(|span| span.content.contains(needle) && span.style.fg == Some(color))
}

fn assert_inspectable(rows: &[Row], name: &str) {
    assert!(rows
        .iter()
        .any(|row| matches!(&row.hit, HitAction::Output(id) if id.0 == name)));
    assert!(rows.last().unwrap().line.to_string().is_empty());
}

#[test]
fn task_and_explore_success_show_reports_not_json() {
    for name in ["task", "explore"] {
        let source = json!({"status":"success", "agents":[agent("reader", "completed", json!("Found parser in src/parser.rs:17"))]}).to_string();
        let entry = entry(name, &source);
        let rendered = rows(&entry, false, 100);
        let shown = screen(&rendered);
        assert!(shown.contains("success · 1 completed"));
        assert!(shown.contains("• reader · completed"));
        assert!(shown.contains("Found parser in src/parser.rs:17"));
        assert!(!shown.contains("\"agents\""));
        assert!(has_color(&rendered, "•", Palette::OK()));
        assert!(has_color(&rendered, "Found parser", Palette::TEXT()));
        assert_inspectable(&rendered, name);
        assert_eq!(entry.text, source);
    }
}

#[test]
fn partial_task_failure_is_not_green_even_when_tool_execution_succeeded() {
    let source = json!({"status":"failed", "agents":[
        agent("successful", "completed", json!("Long success\n".repeat(40))),
        agent("broken", "failed", json!("Compiler rejected change")),
        agent("stopped", "cancelled", json!("Partial edits retained")),
        agent("uncertain", "unknown", Value::Null)
    ]})
    .to_string();
    let entry = entry("task", &source);
    let collapsed = rows(&entry, false, 140);
    let shown = screen(&collapsed);
    assert!(shown.contains("1 completed · 1 failed · 1 cancelled · 1 unknown"));
    assert!(shown.contains("Compiler rejected change"));
    assert!(has_color(&collapsed[..1], "•", Palette::ERROR()));
    assert!(!has_color(&collapsed[..1], "•", Palette::OK()));
    let expanded = rows(&entry, true, 140);
    assert!(has_color(&expanded, "· cancelled", Palette::DIM()));
    assert!(has_color(&expanded, "· unknown", Palette::WARN()));
    assert!(screen(&expanded).contains("Partial edits retained"));
    assert!(screen(&expanded).contains("Long success"));
    assert_inspectable(&expanded, "task");
}

#[test]
fn workflow_partial_map_keeps_outcomes_and_failure_details() {
    let source = json!({
        "status":"failed", "error":"map phase incomplete; dependent reduction skipped",
        "output": {
            "results":[{"summary":"Good result"}, null],
            "outcomes":[
                {"input":"good.rs", "status":"success", "output":{"summary":"Good result"}, "error":null},
                {"input":"bad.rs", "status":"failed", "output":null, "error":"child failed; inspect its retained report"}
            ]
        },
        "agents":[agent("map/0", "completed", json!("{\"summary\":\"Good result\"}")), agent("map/1", "failed", json!("Validation rejected output"))]
    }).to_string();
    let entry = entry("workflow", &source);
    let collapsed = rows(&entry, false, 120);
    assert!(has_color(&collapsed[..1], "•", Palette::ERROR()));
    assert!(screen(&collapsed).contains("dependent reduction skipped"));
    let rendered = rows(&entry, true, 120);
    let shown = screen(&rendered);
    assert!(shown.contains("good.rs"));
    assert!(shown.contains("bad.rs"));
    assert!(shown.contains("child failed; inspect its retained report"));
    assert!(shown.contains("Validation rejected output"));
    assert_eq!(shown.matches("Good result").count(), 1);
    assert!(has_color(&rendered, "child failed", Palette::ERROR()));
    assert!(!shown.contains("\"outcomes\""));
    assert_inspectable(&rendered, "workflow");
}

#[test]
fn successful_workflow_preserves_nested_typed_output_and_reduction() {
    let source = json!({"status":"success", "error":null, "agents":[], "output":{
        "review":{"branches":{"left":{"findings":["Unicode λ", "Parser is sound"]}, "right":{"verified":true, "count":2}}},
        "reduced":{"summary":"Ready to merge"}, "rounds":3, "termination_reason":"dry"
    }}).to_string();
    let rendered = rows(&entry("workflow", &source), true, 100);
    let shown = screen(&rendered);
    for expected in [
        "review",
        "branches",
        "left",
        "right",
        "Unicode λ",
        "verified: true",
        "count: 2",
        "Ready to merge",
        "rounds: 3",
        "termination reason: dry",
    ] {
        assert!(shown.contains(expected), "missing {expected}: {shown}");
    }
    assert!(has_color(&rendered[..1], "•", Palette::OK()));
    assert!(!shown.contains('{'));
    assert_inspectable(&rendered, "workflow");
}

#[test]
fn canned_workflow_reduction_and_loop_termination_are_readable() {
    for output in [
        json!({"results":["Map report"], "outcomes":[{"input":"src", "status":"success", "output":"Map report", "error":null}], "reduced":"Final synthesis"}),
        json!({"findings":["First finding"], "rounds":2, "termination_reason":"max_rounds"}),
        json!({"review":{"findings":["First finding"]}, "_iterations":2, "_stop_reason":"condition_met"}),
    ] {
        let source =
            json!({"status":"success", "output":output, "error":null, "agents":[]}).to_string();
        let rendered = rows(&entry("workflow", &source), true, 100);
        let shown = screen(&rendered);
        assert!(has_color(&rendered[..1], "•", Palette::OK()));
        if output.get("reduced").is_some() {
            assert!(shown.contains("Final synthesis"));
            assert_eq!(shown.matches("Map report").count(), 1);
        } else {
            assert!(shown.contains("First finding"));
            assert!(shown.contains("max_rounds") || shown.contains("condition_met"));
        }
        assert_inspectable(&rendered, "workflow");
    }
}

#[test]
fn wait_outcomes_do_not_inherit_success_from_tool_execution() {
    for status in ["completed", "failed", "cancelled"] {
        let source = format!("Agent thread-reader {status}. Its report is queued in your context.");
        let rendered = rows(&entry("wait_agent", &source), true, 120);
        assert!(screen(&rendered).contains(&source));
        assert!(has_color(
            &rendered[..1],
            "•",
            if status == "completed" {
                Palette::OK()
            } else {
                Palette::ERROR()
            }
        ));
        assert!(has_color(
            &rendered,
            "Its report is queued",
            work_style(status).fg.unwrap()
        ));
    }
}

#[test]
fn cancelled_workflow_shows_node_states_and_retained_artifacts() {
    let source = json!({"status":"failed", "error":"Workflow stopping; child effects are retained and are not replayed automatically.", "agents":[], "output":[
        {"path":"root/check", "state":"completed", "result_artifact":"node-good", "error":null},
        {"path":"root/edit", "state":"cancelled", "result_artifact":"node-partial", "error":"Stopped after partial write"},
        {"path":"root/review", "state":"unknown", "result_artifact":null, "error":"Outcome uncertain"}
    ]}).to_string();
    let rendered = rows(&entry("workflow", &source), true, 120);
    let shown = screen(&rendered);
    assert!(has_color(
        &rendered,
        "root/check · completed",
        Palette::OK()
    ));
    assert!(has_color(
        &rendered,
        "root/edit · cancelled",
        Palette::DIM()
    ));
    assert!(has_color(
        &rendered,
        "root/review · unknown",
        Palette::WARN()
    ));
    assert!(has_color(
        &rendered,
        "Stopped after partial write",
        Palette::ERROR()
    ));
    assert!(shown.contains("Retained output: node-partial"));
    assert_inspectable(&rendered, "workflow");
}

#[test]
fn omitted_task_output_is_explicit_and_inspectable() {
    let mut report = agent("large", "completed", Value::Null);
    report["output_omitted"] = json!(true);
    report["note"] = json!("Aggregate output budget exceeded; full output retained in result_artifact and child transcript.");
    let source = json!({"status":"success", "agents":[report]}).to_string();
    let rendered = rows(&entry("task", &source), true, 100);
    assert!(screen(&rendered).contains("Output omitted from aggregate"));
    assert!(screen(&rendered).contains("artifact-large"));
    assert_inspectable(&rendered, "task");
}

#[test]
fn background_launch_and_agent_listing_are_compact() {
    let source = json!({"job_id":"job-1", "message":"Tasks launched; successful and failed results remain inspectable."}).to_string();
    let rendered = rows(&entry("task", &source), false, 120);
    assert!(screen(&rendered)
        .contains("Tasks launched; successful and failed results remain inspectable."));
    assert!(!screen(&rendered).contains("job_id"));
    assert_inspectable(&rendered, "task");
    let source = json!({"agents":[
        {"thread_id":"a", "name":"reader", "role":"task", "state":"running", "result_artifact":null, "run_id":"run-a"},
        {"thread_id":"b", "name":"reviewer", "role":"collaborator", "state":"failed", "result_artifact":"artifact-b", "run_id":"run-b"},
        {"thread_id":"c", "name":"writer", "role":"task", "state":"completed", "result_artifact":"artifact-c", "run_id":"run-c"}
    ], "next_cursor":"c"}).to_string();
    let rendered = rows(&entry("list_agents", &source), false, 120);
    assert!(screen(&rendered).contains("3 agents · 1 completed · 1 failed · 1 running"));
    assert!(has_color(&rendered, "· running", Palette::RUNNING()));
    assert!(has_color(&rendered, "· failed", Palette::ERROR()));
    assert!(has_color(&rendered[..1], "•", Palette::OK()));
    assert_inspectable(&rendered, "list_agents");
    assert!(screen(&rendered).contains("More agents available"));
    let empty = rows(
        &entry("list_agents", "{\"agents\":[],\"next_cursor\":null}"),
        false,
        80,
    );
    assert!(screen(&empty).contains("0 agents"));
}

#[test]
fn history_pages_stay_compact_inspectable_and_source_preserving() {
    let source = json!({
        "thread_id":"thread-reader", "view":"transcript", "entries":[
            {"text":"retained conversation content must not fill the transcript\n".repeat(200)}
        ], "next_cursor":"page-two"
    })
    .to_string();
    let entry = entry("read_history", &source);
    for width in [40, 100] {
        for expanded in [false, true] {
            let rendered = rows(&entry, expanded, width);
            let shown = screen(&rendered);
            assert!(shown.contains("Read history"));
            assert!(shown.contains("1 history entries"));
            assert!(!shown.contains("retained conversation content"));
            assert!(!shown.contains("\"entries\""));
            assert!(rendered.len() <= 7);
            assert_inspectable(&rendered, "read_history");
        }
    }
    assert_eq!(entry.text, source);
    let mut clipped = entry.clone();
    clipped.text = "{\"entries\":[".into();
    clipped.is_preview = true;
    let rendered = rows(&clipped, false, 100);
    assert!(screen(&rendered).contains("Retained history page"));
    assert_inspectable(&rendered, "read_history");
    let mut failed = entry;
    failed.text = "cannot read another session".into();
    failed.kind = EntryKind::ToolError;
    failed.tool.as_mut().unwrap().state = OperationState::Failed;
    let rendered = rows(&failed, false, 100);
    assert!(screen(&rendered).contains("cannot read another session"));
    assert!(has_color(&rendered[..1], "•", Palette::ERROR()));
}

#[test]
fn spawn_remains_static_and_stop_wait_and_errors_remain_readable() {
    let launch = entry("spawn_agent", "Started agent thread-reader. Completion reports are durable; inspect and message it by thread ID.");
    let rendered = rows(&launch, false, 100);
    assert_eq!(rendered.len(), 2);
    assert!(screen(&rendered).contains("Started agent: Inspect Unicode"));
    assert!(rendered[0]
        .line
        .spans
        .iter()
        .any(|span| span.content.contains("Started agent:")
            && span.style.add_modifier.contains(Modifier::BOLD)));
    assert!(matches!(rendered[0].hit, HitAction::Entry(1)));
    for (name, source) in [
        ("stop_agent", "Stop requested for thread-reader."),
        (
            "wait_agent",
            "Agent thread-reader cancelled. Its report is queued in your context.",
        ),
    ] {
        let rendered = rows(&entry(name, source), true, 120);
        assert!(screen(&rendered).contains(source));
    }
    let mut failure = entry("spawn_agent", "unknown subagent_type invalid");
    failure.kind = EntryKind::ToolError;
    failure.tool.as_mut().unwrap().state = OperationState::Failed;
    let rendered = rows(&failure, false, 100);
    assert!(screen(&rendered).contains("unknown subagent_type invalid"));
    assert!(!screen(&rendered).contains("Started agent:"));
    assert!(has_color(&rendered[..1], "•", Palette::ERROR()));
}

#[test]
fn incomplete_preview_and_display_limits_keep_full_output_access() {
    let mut partial = entry("workflow", "{\"status\":\"failed\",\"output\":");
    partial.is_preview = true;
    let rendered = rows(&partial, true, 100);
    assert!(screen(&rendered).contains("Result preview incomplete"));
    assert!(!screen(&rendered).contains("\"status\""));
    assert_inspectable(&rendered, "workflow");
    let source = json!({"status":"success", "agents":[agent("long", "completed", json!("line\n".repeat(1000)))]}).to_string();
    let entry = entry("task", &source);
    let collapsed = rows(&entry, false, 100);
    assert!(collapsed.len() <= PREVIEW + 3);
    assert!(screen(&collapsed).contains("more output (click to expand)"));
    let expanded = rows(&entry, true, 100);
    assert!(expanded.len() <= EXPANDED_MAX + 3);
    assert!(screen(&expanded).contains("display truncated"));
    assert_inspectable(&expanded, "task");
}

#[test]
fn unicode_and_nested_ansi_are_safe_in_actual_terminal_cells() {
    let source = json!({"status":"failed", "error":"\u{1b}[31mFailure λ\u{1b}[0m", "agents":[agent("\u{1b}]0;evil\u{7}审查", "failed", json!("\u{1b}[2JPartial 👩‍💻 é retained"))], "output":{"nested":{"summary":"\u{1b}[32m日本語\u{1b}[0m", "detail":"\u{1b}]8;;https://example.invalid\u{7}visible\u{1b}]8;;\u{7}"}}}).to_string();
    for width in [24, 40, 80, 120] {
        let rendered = rows(&entry("workflow", &source), true, width);
        let shown = screen(&rendered);
        assert!(!shown.contains('\u{1b}'));
        assert!(!shown.contains("evil"));
        assert!(!shown.contains("example.invalid"));
        assert!(shown.contains("日本語"));
        let mut terminal = Terminal::new(TestBackend::new(width, rendered.len() as u16)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(
                        rendered
                            .iter()
                            .map(|row| row.line.clone())
                            .collect::<Vec<_>>(),
                    ),
                    frame.area(),
                )
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "•");
        assert_eq!(buffer[(0, 0)].fg, Palette::ERROR());
        assert!(buffer
            .content
            .iter()
            .all(|cell| !cell.symbol().chars().any(char::is_control)));
    }
}

#[test]
fn root_keeps_aggregate_results_but_not_coordination_and_polling() {
    let aggregate = json!({"status":"failed", "agents":[agent("broken", "failed", json!("Required aggregate outcome"))]}).to_string();
    let mut coordination = entry("unused", "Internal completion report");
    coordination.sequence = 5;
    coordination.kind = EntryKind::Coordination;
    coordination.operation = None;
    coordination.tool = None;
    let mut pane = Pane::default();
    pane.view = Some(View {
        sequence: 5,
        entry_sequence: 5,
        entries: vec![
            entry("task", &aggregate),
            entry("job_status", "internal status"),
            entry("job_output", "internal output"),
            entry("send_message", "internal message"),
            coordination,
        ],
        threads: vec![],
        policies: vec![],
        decisions: vec![],
        operations: vec![],
        error: None,
    });
    for root in [true, false] {
        let rendered = render::transcript(&pane, root, 120);
        let shown = screen(&rendered);
        assert!(shown.contains("Required aggregate outcome"));
        assert!(!shown.contains("internal status"));
        assert!(!shown.contains("internal output"));
        assert_eq!(shown.contains("Internal completion report"), !root);
        assert_eq!(shown.contains("Message to"), !root);
    }
}
