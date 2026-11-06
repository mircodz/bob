use super::{render, state::HitAction, tool_render};
use bob_kernel::{
    Entry, EntryKind, Invocation, OperationId, OperationState, RunId, ThreadId, ToolOutcome,
    ToolResultPresentation, ToolSummary,
};
use bob_ui::theme::Palette;
use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::sync::watch;

#[path = "read_presentation_tests.rs"]
mod read_presentation_tests;

fn entries(name: &str, input: Value, outcome: &ToolOutcome) -> (Entry, Entry) {
    let id = OperationId::new();
    let call = Entry {
        sequence: 1,
        sender: None,
        parent_guidance: false,
        submission: None,
        thread: ThreadId("tool-parity".into()),
        kind: EntryKind::ToolCall,
        text: String::new(),
        artifact: None,
        operation: Some(id.clone()),
        tool: Some(ToolSummary {
            id: id.clone(),
            name: name.into(),
            state: if outcome.is_error {
                OperationState::Failed
            } else {
                OperationState::Done
            },
            presentation: outcome.presentation.clone(),
            result: None,
            input_preview: input,
        }),
        is_preview: false,
        visible_text_bytes: None,
    };
    let result = Entry {
        sequence: 2,
        sender: None,
        parent_guidance: false,
        submission: None,
        thread: call.thread.clone(),
        kind: if outcome.is_error {
            EntryKind::ToolError
        } else {
            EntryKind::ToolResult
        },
        text: outcome.text.clone(),
        artifact: None,
        operation: Some(id),
        tool: call.tool.clone(),
        is_preview: false,
        visible_text_bytes: None,
    };
    (call, result)
}

fn outcome(text: &str) -> ToolOutcome {
    ToolOutcome {
        text: text.into(),
        is_error: false,
        interrupted: false,
        presentation: None,
    }
}

fn render(call: &Entry, result: &Entry, expanded: bool) -> Vec<render::Row> {
    let mut rows = Vec::new();
    tool_render::tool(&mut rows, call, Some(result), expanded, 512);
    rows
}

fn text(rows: &[render::Row]) -> String {
    rows.iter()
        .map(|row| row.line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-tool-parity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(path.join("home")).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn invocation(&self, thread: &ThreadId, name: &str, input: Value) -> Invocation {
        Invocation {
            operation_id: OperationId::new(),
            run_id: RunId::new(),
            thread_id: thread.clone(),
            workspace: self.0.to_str().unwrap().into(),
            name: name.into(),
            input,
        }
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn execute(service: &bob::adapters::ToolService, invocation: Invocation) -> ToolOutcome {
    let prepared = service.prepare(invocation).await.unwrap();
    let (_sender, cancel) = watch::channel(false);
    service.execute(prepared, cancel).await.unwrap()
}

#[tokio::test]
async fn real_file_mutations_render_unfenced_diffs_and_retain_original_notices() {
    let before = "fn demo() {\n    let value = \"old | 日本語\";\n}\n";
    let after = before.replace("old", "new");
    for (name, input) in [
        (
            "edit_file",
            json!({"path":"source.rs", "old_string":"old", "new_string":"new"}),
        ),
        (
            "multi_edit",
            json!({"path":"source.rs", "edits":[{"old_string":"old", "new_string":"new"}]}),
        ),
        ("write_file", json!({"path":"source.rs", "content":after})),
    ] {
        let workspace = Workspace::new();
        std::fs::write(workspace.0.join("source.rs"), before).unwrap();
        let service = bob::adapters::ToolService::new(None, Some(workspace.0.join("home")));
        let thread = ThreadId::new();
        assert!(service
            .prepare(workspace.invocation(&thread, name, input.clone()))
            .await
            .is_err());
        let read = execute(
            &service,
            workspace.invocation(&thread, "read_file", json!({"path":"source.rs"})),
        )
        .await;
        assert!(!read.is_error, "{}", read.text);
        let prepared = service
            .prepare(workspace.invocation(&thread, name, input.clone()))
            .await
            .unwrap();
        let recovery = prepared
            .presentation
            .as_ref()
            .unwrap()
            .details
            .iter()
            .find_map(|detail| detail.strip_prefix("Displaced original retained at: "))
            .unwrap()
            .to_owned();
        let (_sender, cancel) = watch::channel(false);
        let result = service.execute(prepared, cancel).await.unwrap();
        assert!(!result.is_error && !result.interrupted, "{}", result.text);
        assert_eq!(
            std::fs::read_to_string(workspace.0.join("source.rs")).unwrap(),
            after
        );
        assert_eq!(std::fs::read_to_string(&recovery).unwrap(), before);
        assert!(!result.text.contains("```diff"));
        assert!(result.text.contains("+1 -1\n@@ -1,3 +1,3 @@"));
        let (call, output) = entries(name, input, &result);
        let original = output.text.clone();
        for expanded in [false, true] {
            let rows = render(&call, &output, expanded);
            let shown = text(&rows);
            assert!(
                shown.contains("displaced original retained"),
                "{name}: {shown}"
            );
            assert!(shown.contains("+1 -1"));
            assert!(shown.contains("@@ -1,3 +1,3 @@"));
            for (word, sign, color, background) in [
                (
                    "old",
                    '-',
                    Palette::DIFF_REMOVE(),
                    Palette::DIFF_REMOVE_BG(),
                ),
                ("new", '+', Palette::DIFF_ADD(), Palette::DIFF_ADD_BG()),
            ] {
                let content = format!("    let value = \"{word} | 日本語\";");
                let row = rows
                    .iter()
                    .find(|row| row.line.to_string().contains(&content))
                    .unwrap();
                assert_eq!(row.line.to_string(), format!("    {sign}      {content}"));
                assert!(
                    row.line
                        .spans
                        .iter()
                        .any(|span| span.content == format!("{sign} ")
                            && span.style.fg == Some(color))
                );
                assert!(
                    row.line
                        .spans
                        .iter()
                        .filter(|span| span.style.bg == Some(background))
                        .count()
                        > 0
                );
                assert!(
                    matches!(&row.hit, HitAction::Tool(id) if Some(id) == call.operation.as_ref())
                );
            }
            if expanded {
                assert!(shown.contains(&recovery));
            }
        }
        assert_eq!(output.text, original);
        assert_eq!(output.text, result.text);
    }
}

#[test]
fn diff_safety_notices_survive_raw_and_fenced_rendering() {
    for source in [
        "+1 -1\n@@ -1,1 +1,1 @@\n-  old | value\n+  new | value\n\\ No newline at end of file\n... [diff preview truncated; exact contents retained in prepared action]\nDisplaced original retained at /safe/original",
        "Edited source.rs\n```diff rs\n-    1|   old | value\n+    1|   new | value\n```\nDisplaced original retained at /safe/original",
    ] {
        let (call, result) = entries("edit_file", json!({"path":"source.rs"}), &outcome(source));
        let rows = render(&call, &result, true);
        let shown = text(&rows);
        assert!(shown.contains("Displaced original retained at /safe/original"));
        assert!(shown.contains("  old | value"));
        assert!(shown.contains("  new | value"));
        if source.contains("No newline") {
            assert!(shown.contains("\\ No newline at end of file"));
            assert!(shown.contains("exact contents retained in prepared action"));
        }
        assert_eq!(result.text, source);
    }
}

#[tokio::test]
async fn real_successful_standalone_exploration_stays_compact_until_expanded() {
    let workspace = Workspace::new();
    std::fs::write(workspace.0.join("source.rs"), "let unique_value = 1;\n").unwrap();
    let service = bob::adapters::ToolService::new(None, Some(workspace.0.join("home")));
    let thread = ThreadId::new();
    for (name, input, expected) in [
        ("read_file", json!({"path":"source.rs"}), "unique_value"),
        (
            "grep",
            json!({"path":".", "pattern":"unique_value"}),
            "source.rs",
        ),
        ("glob", json!({"path":".", "pattern":"*.rs"}), "source.rs"),
        ("list_dir", json!({"path":"."}), "source.rs"),
    ] {
        let result = execute(&service, workspace.invocation(&thread, name, input.clone())).await;
        assert!(!result.is_error, "{name}: {}", result.text);
        let (call, output) = entries(name, input, &result);
        let collapsed = render(&call, &output, false);
        assert_eq!(collapsed.len(), 2, "{name}: {}", text(&collapsed));
        assert!(collapsed.last().unwrap().line.to_string().is_empty());
        assert!(collapsed.iter().all(
            |row| matches!(&row.hit, HitAction::Tool(id) if Some(id) == call.operation.as_ref())
        ));
        let expanded = render(&call, &output, true);
        assert!(expanded.len() > 2);
        assert!(
            text(&expanded[1..]).contains(expected),
            "{name}: {}",
            text(&expanded)
        );
        assert_eq!(output.text, result.text);
    }
}

#[test]
fn collapsed_exploration_keeps_errors_visible_and_hides_agent_warnings() {
    for name in ["read_file", "grep", "glob", "list_dir"] {
        let mut failed = outcome("permission denied for target");
        failed.is_error = true;
        let (call, output) = entries(name, json!({"path":"target"}), &failed);
        let rows = render(&call, &output, false);
        assert!(text(&rows).contains("permission denied for target"));
        assert_eq!(rows[0].line.spans[0].style.fg, Some(Palette::ERROR()));

        let mut incomplete = outcome("{\"truncated\":true,\"entries\":[]}");
        incomplete.presentation = Some(ToolResultPresentation {
            summary: Some("12 results (incomplete)".into()),
            body_preview: Some("[!] Search incomplete; narrow the query or inspect continuation details.\nentry body\n[Preview; inspect for full result]".into()),
            exit_code: None,
        });
        let (call, output) = entries(name, json!({"path":"target"}), &incomplete);
        let rows = render(&call, &output, false);
        let shown = text(&rows);
        assert!(!shown.contains("incomplete"), "{name}: {shown}");
        assert!(!shown.contains("[!]"));
        assert!(!shown.contains("entry body"));
        assert!(text(&render(&call, &output, true)).contains("entry body"));
    }
    let page = format!(
        "{}\n... [output truncated; use offset=21 and limit to read further]",
        "file contents\n".repeat(20)
    );
    let (call, mut result) = entries("read_file", json!({"path":"target"}), &outcome(&page));
    let shown = text(&render(&call, &result, false));
    assert!(!shown.contains("use offset=21"));
    assert!(!shown.contains("file contents"));
    result.text =
        "     1\tlong... [line truncated; use offset=1 and column_offset=2001 to continue]".into();
    assert!(!text(&render(&call, &result, false)).contains("column_offset=2001"));
    result.text = "prefix".into();
    result.is_preview = true;
    let shown = text(&render(&call, &result, false));
    assert!(!shown.contains("incomplete"));
    assert!(!shown.contains("click to expand"));
}

#[test]
fn send_message_aliases_match_kernel_precedence_without_mutating_source() {
    for input in [
        json!({"to":"reviewer", "message":"Check the diff\nKeep the original"}),
        json!({"thread_id":"reviewer", "text":"Check the diff\nKeep the original"}),
        json!({"to":"reviewer", "thread_id":"ignored", "message":"Check the diff\nKeep the original", "text":"ignored"}),
        json!({"to":null, "thread_id":"reviewer", "message":null, "text":"Check the diff\nKeep the original"}),
    ] {
        let (call, output) = entries("send_message", input.clone(), &outcome("Message queued"));
        for expanded in [false, true] {
            let rows = render(&call, &output, expanded);
            let shown = text(&rows);
            assert!(shown.contains("Message to reviewer"));
            assert!(shown.contains("Check the diff"));
            assert!(shown.contains("Keep the original"));
            assert!(!shown.contains("ignored"));
            assert_eq!(shown.contains("Message queued"), expanded);
        }
        assert_eq!(call.tool.unwrap().input_preview, input);
    }
    assert_eq!(
        tool_render::display("send_message", &json!({"to":"", "thread_id":"ignored"})),
        ("Message to".into(), "".into())
    );
    let (call, output) = entries(
        "send_message",
        json!({"thread_id":"reviewer", "text":"\u{1b}[31mKeep 日本語 safe"}),
        &outcome("queued"),
    );
    let shown = text(&render(&call, &output, false));
    assert!(shown.contains("Keep 日本語 safe"));
    assert!(!shown.contains('\u{1b}'));
    assert!(call.tool.unwrap().input_preview["text"]
        .as_str()
        .unwrap()
        .contains('\u{1b}'));
}
