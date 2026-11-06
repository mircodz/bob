use super::*;
use async_trait::async_trait;
use bob_kernel::{
    EffectClass, Entry, Executor, Invocation, PreparedTool, RequestSpec, Runtime, ToolDescriptor,
    ToolOutcome, ToolSummary,
};
use bob_types::{Completion, ContentBlock, Message, Role, StopReason, ToolSpec, Usage};
use bob_ui::{highlight::highlight_line, theme::Palette};
use ratatui::text::Span;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::{sync::watch, time::timeout};
use tool_render::BashInputState;
use unicode_width::UnicodeWidthChar;

fn shell(input: Value) -> Entry {
    let id = OperationId("bash-parity".into());
    Entry {
        sequence: 1,
        sender: None,
        parent_guidance: false,
        submission: None,
        thread: ThreadId("root".into()),
        kind: EntryKind::ToolCall,
        text: String::new(),
        artifact: None,
        operation: Some(id.clone()),
        tool: Some(ToolSummary {
            id,
            name: "bash".into(),
            state: OperationState::Done,
            presentation: None,
            result: None,
            input_preview: input,
        }),
        is_preview: false,
        visible_text_bytes: None,
    }
}

fn text_rows(rows: &[render::Row]) -> String {
    rows.iter()
        .map(|row| row.line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn styled_chars(line: &Line<'_>) -> Vec<(char, Style)> {
    line.spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect()
}

// Production cli/tui/render.rs:270,684 and cli/tui/mod.rs:2397.
fn production_command(command: &str, viewport: u16) -> Vec<Line<'static>> {
    let width = usize::from(viewport).saturating_sub(15).max(1);
    let mut out = Vec::new();
    for source in command.replace('\t', "    ").split('\n') {
        let line = Line::from(highlight_line(source, "sh"));
        let wrapped = if line.width() <= width {
            vec![line]
        } else {
            let mut rows = Vec::new();
            let mut spans = Vec::new();
            let mut col = 0;
            for span in line.spans {
                let mut chunk = String::new();
                for ch in span.content.chars() {
                    let columns = ch.width().unwrap_or(0);
                    if col + columns > width && col > 0 {
                        if !chunk.is_empty() {
                            spans.push(Span::styled(std::mem::take(&mut chunk), span.style));
                        }
                        rows.push(Line::from(std::mem::take(&mut spans)));
                        col = 0;
                    }
                    chunk.push(ch);
                    col += columns;
                }
                if !chunk.is_empty() {
                    spans.push(Span::styled(chunk, span.style));
                }
            }
            if !spans.is_empty() {
                rows.push(Line::from(spans));
            }
            rows
        };
        for row in wrapped {
            let mut spans = if out.is_empty() {
                vec![
                    Span::styled("• ", Style::default().fg(Palette::OK())),
                    Span::styled("Bash", style::bold()),
                    Span::raw(" "),
                ]
            } else {
                vec![Span::raw("       ")]
            };
            spans.extend(row.spans);
            out.push(Line::from(spans));
        }
    }
    out
}

#[test]
fn ordinary_bash_matches_production_highlight_tabs_quotes_pipelines_and_unicode() {
    let command = "printf\t'%s\\n' \"$HOME/日本語\" | sed 's/ /_/g'\n\nif [ -n \"$USER\" ]; then\n\tprintf '%s' 'cafe\u{301} 界'; fi\n";
    assert!(command.lines().all(|line| line.len() < 2048));
    let colors: std::collections::HashSet<_> =
        highlight_line(command.lines().next().unwrap(), "sh")
            .iter()
            .filter(|span| !span.content.is_empty())
            .filter_map(|span| span.style.fg)
            .collect();
    assert!(
        colors.len() > 1,
        "ordinary shell syntax must not use plain fallback"
    );
    let entry = shell(json!({"command": command}));
    for width in [24, 40, 80, 120, 180, 240] {
        let expected = production_command(command, width);
        for expanded in [false, true] {
            let mut rows = Vec::new();
            tool_render::tool(&mut rows, &entry, None, expanded, width);
            let count = if expanded {
                expected.len()
            } else {
                expected.len().min(6)
            };
            for (actual, expected) in rows.iter().take(count).zip(&expected) {
                assert_eq!(
                    styled_chars(&actual.line),
                    styled_chars(expected),
                    "width={width}, expanded={expanded}"
                );
                assert!(actual.line.width() <= usize::from(width).saturating_sub(8));
                assert!(matches!(&actual.hit, HitAction::Tool(id) if id.0 == "bash-parity"));
            }
            let status = if !expanded && expected.len() > 6 {
                style::prose_wrap(
                    &Line::styled(
                        "    … more command (click to expand)",
                        Style::default().fg(Palette::FAINT()),
                    ),
                    usize::from(width).saturating_sub(8).max(1),
                )
            } else {
                Vec::new()
            };
            assert_eq!(rows.len(), count + status.len() + 1);
            for (actual, (_, expected)) in rows[count..].iter().zip(&status) {
                assert_eq!(actual.line, *expected);
                assert!(matches!(&actual.hit, HitAction::Tool(id) if id.0 == "bash-parity"));
            }
            assert!(rows.last().unwrap().line.to_string().is_empty());
        }
    }
    assert_eq!(entry.tool.unwrap().input_preview["command"], command);
}

#[test]
fn truncated_preview_always_has_affordance_and_honest_expanded_status() {
    for command in ["printf '%s' 'prefix", ""] {
        let entry = shell(json!({"command": command, "preview_truncated": true}));
        let id = entry.operation.as_ref().unwrap();
        let mut rows = Vec::new();
        tool_render::tool(&mut rows, &entry, None, false, 240);
        assert!(text_rows(&rows).contains("command preview truncated (click to expand)"));
        assert!(rows
            .iter()
            .all(|row| matches!(&row.hit, HitAction::Tool(hit) if hit == id)));
        for (state, expected, failure) in [
            (BashInputState::Loading, "loading full command", false),
            (
                BashInputState::Unavailable,
                "full command not available",
                false,
            ),
            (
                BashInputState::Failed("read failed\n\u{1b}[31mretained input"),
                "full command unavailable: read failed retained input",
                true,
            ),
        ] {
            let mut rows = Vec::new();
            tool_render::tool_with_input_state(&mut rows, &entry, None, true, 240, state);
            let status = rows
                .iter()
                .find(|row| row.line.to_string().contains(expected))
                .unwrap();
            assert!(status
                .line
                .to_string()
                .contains("command preview truncated"));
            assert!(!status.line.to_string().contains('\u{1b}'));
            assert_eq!(
                status
                    .line
                    .style
                    .patch(status.line.spans.last().unwrap().style)
                    .fg,
                Some(if failure {
                    Palette::ERROR()
                } else {
                    Palette::FAINT()
                })
            );
            assert!(!text_rows(&rows).contains("click to expand"));
        }
    }
}

#[test]
fn six_visual_rows_remain_the_collapse_boundary() {
    for count in [6, 7] {
        let command = "x".repeat((80 - 15) * count);
        let entry = shell(json!({"command": command}));
        let mut rows = Vec::new();
        tool_render::tool(&mut rows, &entry, None, false, 80);
        assert_eq!(text_rows(&rows).contains("more command"), count == 7);
        assert_eq!(rows.len(), 7 + usize::from(count == 7));
        let mut expanded = Vec::new();
        tool_render::tool(&mut expanded, &entry, None, true, 80);
        assert_eq!(expanded.len(), count + 1);
        assert!(!text_rows(&expanded).contains("more command"));
    }
}

struct Workspace(PathBuf);
impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-bash-parity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct BashFixture {
    command: String,
    calls: AtomicUsize,
}

#[async_trait]
impl Executor for BashFixture {
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
                name: "bash".into(),
                description: "Presentation fixture; never executes a shell".into(),
                input_schema: json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}),
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
                    id: "bash-fixture-call".into(),
                    name: "bash".into(),
                    input: json!({"command": self.command}),
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
            preview: "Presentation only".into(),
            presentation: None,
        })
    }

    async fn execute(
        &self,
        _tool: PreparedTool,
        _cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        Ok(ToolOutcome {
            text: String::new(),
            is_error: false,
            interrupted: false,
            presentation: None,
        })
    }
}

fn transcript(app: &App) -> String {
    text_rows(&render::transcript(app.pane().unwrap(), true, 240))
}

async fn collect(app: &mut App, jobs: &mut JoinSet<Reply>) {
    let reply = timeout(Duration::from_secs(10), jobs.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    app.reply(reply);
}

#[tokio::test]
async fn durable_preview_expand_fetch_collapse_and_failure_use_real_tool_input() {
    let workspace = Workspace::new();
    let command = format!(
        "printf '%s\\n' '{}'\n{}printf 'FULL_COMMAND_TAIL\\n'",
        "x".repeat(1100),
        "printf '%s\\n' \"$HOME\" | sed 's/x/y/'\n".repeat(100)
    );
    assert!(
        serde_json::to_string(&json!({"command": command}))
            .unwrap()
            .len()
            > 4096
    );
    let handle = Runtime::open(
        &workspace.0.join("kernel.sqlite"),
        Arc::new(BashFixture {
            command: command.clone(),
            calls: AtomicUsize::new(0),
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
                instructions: "Presentation fixture".into(),
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
            text: "Render the shell fixture".into(),
        })
        .await
        .unwrap();
    let waiting = timeout(Duration::from_secs(10), async {
        loop {
            let view = handle
                .transcript(Some(receipt.thread.clone()), None, 80)
                .await
                .unwrap();
            assert!(view.error.is_none(), "{:?}", view.error);
            if !view.decisions.is_empty() {
                break view;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("default Ask policy must leave the fixture awaiting approval");
    assert!(waiting.entries.iter().any(|entry| entry
        .tool
        .as_ref()
        .is_some_and(|tool| tool.name == "bash" && tool.state == OperationState::Approval)));
    assert!(!waiting
        .entries
        .iter()
        .any(|entry| entry.kind == EntryKind::ToolResult));
    let session_id = waiting
        .threads
        .iter()
        .find(|thread| thread.id == receipt.thread)
        .unwrap()
        .session
        .clone();
    let session = handle.session(session_id.clone()).await.unwrap();
    assert_eq!(session.permissions.default, bob_kernel::RuleDecision::Ask);
    assert!(!session.policy.yolo);
    let mut permissions = session.permissions;
    permissions.grants.push(bob_kernel::PermissionGrant {
        tool: "bash".into(),
        shell_command: None,
        path_prefix: None,
        command: Some(command.clone()),
    });
    handle
        .command(Command::SetPermissions {
            id: CommandId::new(),
            session: session_id,
            permissions,
            expected_revision: session.policy_revision,
        })
        .await
        .unwrap();
    let view = timeout(Duration::from_secs(10), async {
        loop {
            let view = handle
                .transcript(Some(receipt.thread.clone()), None, 80)
                .await
                .unwrap();
            assert!(view.error.is_none(), "{:?}", view.error);
            assert!(
                view.decisions.is_empty(),
                "exact fixture command grant must allow execution"
            );
            if view.entries.iter().any(|entry| {
                entry.kind == EntryKind::Assistant && entry.text.contains("fixture complete")
            }) {
                break view;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let tool = view
        .entries
        .iter()
        .find_map(|entry| entry.tool.as_ref())
        .unwrap();
    let id = tool.id.clone();
    assert_eq!(tool.input_preview["preview_truncated"], true);
    let prefix = tool.input_preview["command"].as_str().unwrap();
    assert_eq!(prefix.chars().count(), 1024);
    assert!(command.starts_with(prefix));
    assert!(production_command(prefix, 240).len() < 6);
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((handle.clone(), view, receipt.thread))));
    let collapsed = transcript(&app);
    assert!(collapsed.contains("command preview truncated (click to expand)"));
    assert!(!collapsed.contains("FULL_COMMAND_TAIL"));
    let mut jobs = JoinSet::new();
    app.toggle_tool(id.clone(), &mut jobs);
    assert!(app.pane().unwrap().pending_inputs.contains(&id));
    assert!(transcript(&app).contains("loading full command"));
    collect(&mut app, &mut jobs).await;
    assert_eq!(app.pane().unwrap().full_inputs[&id]["command"], command);
    assert!(!app.pane().unwrap().pending_inputs.contains(&id));
    let expanded = transcript(&app);
    assert!(expanded.contains("FULL_COMMAND_TAIL"));
    assert!(!expanded.contains("command preview truncated"));
    assert!(!expanded.contains("loading full command"));
    app.toggle_tool(id.clone(), &mut jobs);
    assert!(!app.pane().unwrap().full_inputs.contains_key(&id));
    assert_eq!(transcript(&app), collapsed);

    app.toggle_tool(id.clone(), &mut jobs);
    app.toggle_tool(id.clone(), &mut jobs);
    collect(&mut app, &mut jobs).await;
    assert!(!app.pane().unwrap().full_inputs.contains_key(&id));
    assert_eq!(transcript(&app), collapsed);

    handle.shutdown().await.unwrap();
    app.toggle_tool(id.clone(), &mut jobs);
    assert!(transcript(&app).contains("loading full command"));
    collect(&mut app, &mut jobs).await;
    assert!(app.pane().unwrap().input_errors.contains_key(&id));
    let failed = transcript(&app);
    assert!(failed.contains("command preview truncated · full command unavailable:"));
    assert!(!failed.contains("loading full command"));
    assert!(!failed.contains("FULL_COMMAND_TAIL"));
    app.toggle_tool(id.clone(), &mut jobs);
    assert!(!app.pane().unwrap().input_errors.contains_key(&id));
    app.handle = None;
    app.toggle_tool(id.clone(), &mut jobs);
    assert!(jobs.is_empty());
    assert!(transcript(&app).contains("command preview truncated · full command not available"));
}
