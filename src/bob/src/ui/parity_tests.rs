use super::*;
use bob_kernel::{Entry, RunId, SessionId, ToolResultPresentation, ToolSummary};
use bob_ui::theme::{Palette, Theme};
use ratatui::{backend::TestBackend, buffer::Buffer, style::Color};
use serde_json::json;

const SIZES: [(u16, u16); 4] = [(80, 24), (100, 30), (120, 40), (180, 48)];

fn thread(name: &str) -> ThreadSummary {
    ThreadSummary {
        id: ThreadId(name.into()),
        session: SessionId("session".into()),
        parent_run: None,
        parent_thread: None,
        needs_approval: false,
        name: name.into(),
        model: "test-model".into(),
        state: ThreadState::Idle,
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
    }
}

fn app(entries: Vec<Entry>) -> App {
    let mut app = App::new(80);
    app.select(ThreadId("root".into()));
    app.threads = vec![thread("root")];
    app.pane_mut().unwrap().view = Some(View {
        sequence: 1,
        entry_sequence: entries
            .iter()
            .map(|entry| entry.sequence)
            .max()
            .unwrap_or(0),
        entries,
        threads: app.threads.clone(),
        policies: vec![SessionPolicy {
            session: SessionId("session".into()),
            policy: Policy::default(),
            revision: 0,
        }],
        decisions: vec![],
        operations: vec![],
        error: None,
    });
    app
}

fn render(app: &mut App, width: u16, height: u16) -> (Buffer, (u16, u16)) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    let cursor = terminal.get_cursor_position().unwrap();
    let buffer = terminal.backend().buffer().clone();
    assert!(buffer
        .content
        .iter()
        .all(|cell| !cell.symbol().chars().any(char::is_control)));
    (buffer, (cursor.x, cursor.y))
}

fn row_text(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

fn find_row(buffer: &Buffer, needle: &str) -> u16 {
    (0..buffer.area.height)
        .find(|&y| row_text(buffer, y).contains(needle))
        .unwrap_or_else(|| panic!("missing {needle:?} in {buffer:?}"))
}

fn assert_band(buffer: &Buffer, y: u16, main_width: u16) {
    // Production paints the band with Block + Paragraph (cli/tui/draw.rs:734).
    // TestBackend resets wide-glyph continuation cells; compare the same widget output.
    let mut reference = Terminal::new(TestBackend::new(main_width, 1)).unwrap();
    reference
        .draw(|frame| {
            frame.render_widget(
                Block::default().style(Style::default().bg(Palette::BG())),
                frame.area(),
            );
            let area = ratatui::layout::Rect::new(4, 0, main_width - 8, 1);
            let mut content = String::new();
            let mut x = 4;
            while x < main_width - 4 {
                let symbol = buffer[(x, y)].symbol();
                content.push_str(symbol);
                x += Line::raw(symbol).width().max(1) as u16;
            }
            frame.render_widget(
                Block::default().style(Style::default().bg(Palette::INPUT_BG())),
                area,
            );
            frame.render_widget(
                Paragraph::new(content).style(Style::default().bg(Palette::INPUT_BG())),
                area,
            );
        })
        .unwrap();
    for x in 0..main_width {
        assert_eq!(
            buffer[(x, y)].bg,
            reference.backend().buffer()[(x, 0)].bg,
            "band cell ({x}, {y})"
        );
    }
}

fn assert_hit(app: &App, x: u16, y: u16, matches: impl Fn(&HitAction) -> bool) {
    assert!(
        app.hits
            .iter()
            .any(|hit| hit.area.contains((x, y).into()) && matches(&hit.action)),
        "no matching hit at ({x}, {y})"
    );
}

fn shell(command: &str, body: &str, state: OperationState) -> Entry {
    let mut result = entry(1, EntryKind::ToolResult, body);
    let id = OperationId("shell-operation".into());
    result.operation = Some(id.clone());
    result.tool = Some(ToolSummary {
        id,
        name: "bash".into(),
        state,
        presentation: Some(ToolResultPresentation {
            summary: None,
            body_preview: None,
            exit_code: Some(if state == OperationState::Failed {
                17
            } else {
                0
            }),
        }),
        result: None,
        input_preview: json!({"command": command}),
    });
    result
}

fn command_style(command: &str) -> Style {
    bob_ui::highlight::highlight_line(command, "sh")
        .into_iter()
        .find(|span| !span.content.is_empty())
        .unwrap()
        .style
}

#[test]
fn composer_cells_cursor_footer_and_hits_match_production_geometry() {
    for (width, height) in SIZES.into_iter().chain([(24, 10), (40, 20)]) {
        let mut app = app(vec![]);
        app.draft_mut().insert("draft");
        let (buffer, cursor) = render(&mut app, width, height);
        for y in height - 4..height - 1 {
            assert_band(&buffer, y, width);
        }
        assert_eq!(buffer[(5, height - 3)].symbol(), "›");
        assert_eq!(buffer[(5, height - 3)].fg, Palette::DIM());
        assert_eq!(buffer[(5, height - 3)].modifier, Modifier::empty());
        assert_eq!(buffer[(7, height - 3)].symbol(), "d");
        assert_eq!(buffer[(7, height - 3)].fg, Palette::TEXT());
        assert_eq!(buffer[(7, height - 3)].modifier, Modifier::empty());
        assert_eq!(cursor, (12, height - 3));
        let footer = row_text(&buffer, height - 1);
        assert_eq!(footer.chars().position(|ch| ch != ' '), Some(3));
        assert_eq!(buffer[(3, height - 1)].bg, Palette::BG());
        assert_hit(&app, 7, height - 3, |action| {
            matches!(action, HitAction::Composer)
        });
        assert!(app
            .hits
            .iter()
            .all(|hit| { hit.area.right() <= width && hit.area.bottom() <= height }));
    }
}

#[test]
fn idle_composer_placeholder_has_production_faint_text_and_dim_marker() {
    for (width, height) in SIZES {
        let mut app = app(vec![]);
        let (buffer, cursor) = render(&mut app, width, height);
        let y = height - 3;
        assert!(
            row_text(&buffer, y).contains("send a message...  (Ctrl+J or Shift+Enter for newline)")
        );
        assert_eq!(buffer[(5, y)].symbol(), "›");
        assert_eq!(buffer[(5, y)].fg, Palette::DIM());
        assert_eq!(buffer[(5, y)].modifier, Modifier::empty());
        assert_eq!(buffer[(7, y)].fg, Palette::FAINT());
        assert_eq!(buffer[(7, y)].bg, Palette::INPUT_BG());
        assert_eq!(cursor, (7, y));
    }
}

#[test]
fn composer_wraps_at_content_width_minus_twelve_in_display_columns() {
    for (width, height) in SIZES.into_iter().chain([(24, 10), (40, 20)]) {
        for unit in ["x", "界"] {
            let content = unit.repeat((width as usize - 12) / if unit == "界" { 2 } else { 1 });
            let mut app = app(vec![]);
            app.draft_mut().insert(&content);
            let (buffer, cursor) = render(&mut app, width, height);
            assert_eq!(cursor, (7, height - 3), "{width}x{height}: {unit}");
            assert_eq!(buffer[(7, height - 4)].symbol(), unit);
            assert_eq!(buffer[(5, height - 4)].symbol(), "›");
            assert_eq!(buffer[(5, height - 3)].symbol(), " ");
            for y in height - 5..height - 1 {
                assert_band(&buffer, y, width);
            }
            app.draft_mut().insert("z");
            let (buffer, cursor) = render(&mut app, width, height);
            assert_eq!(buffer[(7, height - 3)].symbol(), "z");
            assert_eq!(cursor, (8, height - 3));
        }
    }
}

fn assert_scrolled_composer(width: u16, height: u16) {
    let mut app = app(vec![]);
    let content = (0..16)
        .map(|index| format!("row {index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.draft_mut().insert(&content);
    let (buffer, cursor) = render(&mut app, width, height);
    let visible = 12.min(height - 4);
    let first_y = height - visible - 2;
    for offset in 0..visible {
        let y = first_y + offset;
        assert_band(&buffer, y, width);
        assert_eq!(
            buffer[(5, y)].symbol(),
            " ",
            "scrolled prefix at {width}x{height}, row {y}"
        );
        assert_eq!(buffer[(6, y)].symbol(), " ");
        assert!(row_text(&buffer, y).contains(&format!("row {:02}", 16 - visible + offset)));
        assert_eq!(buffer[(7, y)].fg, Palette::TEXT());
        assert_hit(&app, 7, y, |action| matches!(action, HitAction::Composer));
    }
    assert_eq!(cursor, (13, height - 3));
    assert_eq!(app.draft_mut().text, content);
    app.key(
        KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL),
        &mut JoinSet::new(),
    );
    let (buffer, cursor) = render(&mut app, width, height);
    assert_eq!(buffer[(5, first_y)].symbol(), "›");
    assert_eq!(buffer[(5, first_y)].fg, Palette::DIM());
    assert_eq!(cursor, (7, first_y));
    assert_eq!(app.draft_mut().text, content);
}

#[test]
fn scrolled_composer_marker_belongs_to_first_logical_row_not_first_visible_row() {
    for (width, height) in SIZES.into_iter().chain([(24, 10), (40, 20)]) {
        assert_scrolled_composer(width, height);
    }
}

fn assert_completion_cells(width: u16, height: u16, kind: completion::Kind) {
    let mut app = app(vec![]);
    app.draft_mut().insert("draft");
    let items = match kind {
        completion::Kind::Slash => vec![
            completion::Item {
                label: "/model".into(),
                description: "model".into(),
            },
            completion::Item {
                label: "/theme".into(),
                description: "theme".into(),
            },
        ],
        completion::Kind::File => vec![
            completion::Item {
                label: "a.rs".into(),
                description: String::new(),
            },
            completion::Item {
                label: "b.rs".into(),
                description: String::new(),
            },
        ],
    };
    app.completion.items = items.clone();
    app.completion.kind = Some(kind);
    for selected in 0..items.len() {
        app.completion.selected = selected;
        let (buffer, cursor) = render(&mut app, width, height);
        for (index, item) in items.iter().enumerate() {
            let y = height - 6 + index as u16;
            let selected = index == selected;
            let bg = if selected {
                Palette::SELECTED_BG()
            } else {
                Palette::POPUP_BG()
            };
            let label = match kind {
                completion::Kind::Slash => format!("{:<8}  {}", item.label, item.description),
                completion::Kind::File => item.label.clone(),
            };
            let expected = format!(" {} {label}", if selected { "❯" } else { " " });
            for (x, glyph) in expected.chars().enumerate() {
                let cell = &buffer[(x as u16, y)];
                assert_eq!(
                    cell.symbol(),
                    glyph.to_string(),
                    "{kind:?} glyph ({x}, {y})"
                );
                assert_eq!(cell.bg, bg, "{kind:?} background ({x}, {y})");
                let fg = if x < 3 || (selected && x < 11) {
                    Palette::ACCENT()
                } else if kind == completion::Kind::Slash && x >= 11 {
                    Palette::FAINT()
                } else {
                    Palette::TEXT()
                };
                assert_eq!(cell.fg, fg, "{kind:?} foreground ({x}, {y})");
                let bold = selected && x >= 3 && (kind == completion::Kind::File || x < 11);
                assert_eq!(
                    cell.modifier,
                    if bold {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }
                );
            }
            for x in expected.chars().count() as u16..width {
                assert_eq!(buffer[(x, y)].symbol(), " ");
                assert_eq!(
                    buffer[(x, y)].bg,
                    Palette::POPUP_BG(),
                    "{kind:?} unstyled menu tail ({x}, {y})"
                );
            }
            for x in [0, 3, width - 1] {
                let hit = app
                    .hits
                    .iter()
                    .rev()
                    .find(|hit| hit.area.contains((x, y).into()))
                    .unwrap();
                assert!(matches!(hit.action, HitAction::Completion(found) if found == index));
            }
        }
        assert_eq!(cursor, (12, height - 3));
        assert_band(&buffer, height - 4, width);
        assert_eq!(app.focus, Focus::Composer);
        assert_eq!(app.draft_mut().text, "draft");
    }
    app.completion.clear();
    render(&mut app, width, height);
    assert!(!app
        .hits
        .iter()
        .any(|hit| matches!(hit.action, HitAction::Completion(_))));
}

#[test]
fn completion_rows_match_production_glyphs_styles_tail_and_hit_priority() {
    for (width, height) in SIZES.into_iter().chain([(24, 10), (40, 20)]) {
        for kind in [completion::Kind::Slash, completion::Kind::File] {
            assert_completion_cells(width, height, kind);
        }
    }
}

#[test]
fn footer_keeps_safety_mode_first_with_production_colors_and_three_column_inset() {
    for (width, height) in SIZES.into_iter().chain([(24, 10), (40, 20)]) {
        for yolo in [false, true] {
            let mut app = app(vec![]);
            app.pane_mut().unwrap().view.as_mut().unwrap().policies[0]
                .policy
                .yolo = yolo;
            app.branch = Some("main".into());
            let (buffer, _) = render(&mut app, width, height);
            let (mode, color) = if yolo {
                ("YOLO", Palette::ERROR())
            } else {
                ("normal", Palette::DIM())
            };
            let expected = format!("   {mode}  ⎇ main");
            assert!(row_text(&buffer, height - 1).starts_with(&expected));
            for x in 3..3 + mode.len() as u16 {
                assert_eq!(buffer[(x, height - 1)].fg, color);
                assert_eq!(buffer[(x, height - 1)].modifier, Modifier::empty());
            }
            for x in 5 + mode.len() as u16..expected.chars().count() as u16 {
                assert_eq!(buffer[(x, height - 1)].fg, Palette::LINK());
            }
            for x in 0..width {
                assert_eq!(buffer[(x, height - 1)].bg, Palette::BG());
                assert!(!app
                    .hits
                    .iter()
                    .any(|hit| hit.area.contains((x, height - 1).into())));
            }
        }
    }
}

#[test]
fn user_band_matches_composer_insets_colors_padding_and_hit_identity() {
    for (width, height) in SIZES.into_iter().chain([(24, 10), (40, 20)]) {
        let mut app = app(vec![entry(7, EntryKind::Submission, "user text")]);
        let (buffer, _) = render(&mut app, width, height);
        let y = find_row(&buffer, "user text");
        assert_eq!(buffer[(5, y)].symbol(), "›");
        assert_eq!(buffer[(5, y)].fg, Palette::DIM());
        assert_eq!(buffer[(5, y)].modifier, Modifier::empty());
        assert_eq!(buffer[(7, y)].symbol(), "u");
        assert_eq!(buffer[(7, y)].fg, Palette::TEXT());
        for band_y in y - 1..=y + 1 {
            assert_band(&buffer, band_y, width);
            assert_hit(&app, 7, band_y, |action| {
                matches!(action, HitAction::Entry(7))
            });
        }
        assert!(row_text(&buffer, y - 1).trim().is_empty());
        assert!(row_text(&buffer, y + 1).trim().is_empty());
        assert_eq!(buffer[(4, y + 2)].bg, Palette::BG());
    }
}

#[test]
fn user_wrap_keeps_unicode_continuation_aligned_and_band_padding_intact() {
    for (width, height) in SIZES.into_iter().chain([(24, 10), (40, 20)]) {
        let text = format!("{}z", "界".repeat((width as usize - 12) / 2));
        let mut app = app(vec![entry(9, EntryKind::Submission, &text)]);
        let (buffer, _) = render(&mut app, width, height);
        let y = find_row(&buffer, "界");
        assert_eq!(buffer[(5, y)].symbol(), "›");
        assert_eq!(buffer[(7, y)].symbol(), "界");
        assert_eq!(buffer[(5, y + 1)].symbol(), " ");
        assert_eq!(buffer[(7, y + 1)].symbol(), "z");
        assert_eq!(buffer[(7, y + 1)].fg, Palette::TEXT());
        assert_eq!(buffer[(7, y + 1)].modifier, Modifier::empty());
        for band_y in y - 1..=y + 2 {
            assert_band(&buffer, band_y, width);
            assert_hit(&app, 7, band_y, |action| {
                matches!(action, HitAction::Entry(9))
            });
        }
        assert_eq!(buffer[(4, y + 3)].bg, Palette::BG());
    }
}

#[test]
fn bash_header_hanging_command_neutral_output_exit_status_and_hits() {
    for (width, height) in SIZES.into_iter().chain([(40, 20)]) {
        let mut app = app(vec![shell(
            "cargo check",
            "compiler detail",
            OperationState::Failed,
        )]);
        let (buffer, _) = render(&mut app, width, height);
        let y = find_row(&buffer, "Bash cargo check");
        assert_eq!(buffer[(4, y)].symbol(), "•");
        assert_eq!(buffer[(4, y)].fg, Palette::ERROR());
        assert_eq!(buffer[(6, y)].symbol(), "B");
        assert_eq!(buffer[(6, y)].fg, Palette::TEXT());
        assert!(buffer[(6, y)].modifier.contains(Modifier::BOLD));
        assert_eq!(buffer[(11, y)].symbol(), "c");
        let highlighted = command_style("cargo check");
        assert_eq!(Some(buffer[(11, y)].fg), highlighted.fg);
        assert_eq!(buffer[(11, y)].bg, Palette::BG());
        assert_eq!(buffer[(11, y)].modifier, highlighted.add_modifier);
        let output = find_row(&buffer, "compiler detail");
        assert_eq!(buffer[(6, output)].symbol(), "│");
        assert_eq!(buffer[(8, output)].symbol(), "c");
        assert_eq!(buffer[(8, output)].fg, Palette::FAINT());
        assert_eq!(buffer[(8, output)].bg, Palette::BG());
        assert_eq!(buffer[(8, output)].modifier, Modifier::empty());
        let exit = find_row(&buffer, "exit 17");
        assert_eq!(buffer[(8, exit)].symbol(), "e");
        assert_eq!(buffer[(8, exit)].fg, Palette::ERROR());
        for (x, row) in [(6, y), (11, y), (8, output), (8, exit)] {
            assert_hit(
                &app,
                x,
                row,
                |action| matches!(action, HitAction::Tool(id) if id.0 == "shell-operation"),
            );
        }
        assert!(!(0..height).any(|row| row_text(&buffer, row).contains("Shell")));
    }
}

#[test]
fn shell_output_caps_visual_rows_at_six_and_five_hundred_without_losing_source() {
    for width in [24, 40, 80, 100, 120] {
        for body in ["q".repeat(80_000), vec!["q"; 501].join("\n")] {
            let mut app = app(vec![shell("check", &body, OperationState::Done)]);
            for (expanded, expected) in [(false, 6), (true, 500)] {
                if expanded {
                    app.pane_mut()
                        .unwrap()
                        .expanded
                        .insert(OperationId("shell-operation".into()));
                }
                let rows = render::transcript(app.pane().unwrap(), true, width);
                let output: Vec<_> = rows
                    .iter()
                    .filter(|row| row.line.to_string().contains('q'))
                    .collect();
                assert_eq!(output.len(), expected, "width={width}, expanded={expanded}");
                assert!(output
                    .iter()
                    .all(|row| row.line.to_string().starts_with("  │ q")));
                let footer = if expanded {
                    "display truncated"
                } else if body.contains('\n') {
                    "... 495 more lines (click to expand)"
                } else {
                    "... more output (click to expand)"
                };
                assert!(
                    rows.iter().any(|row| row.line.to_string().contains(footer)),
                    "width={width}, expanded={expanded}: {footer}"
                );
                assert_eq!(
                    app.pane().unwrap().view.as_ref().unwrap().entries[0].text,
                    body
                );
            }
        }
    }
}

#[test]
fn long_shell_command_highlight_and_wrap_preserve_source() {
    let command = "c".repeat(92_000);
    let plain = style::wrap(&Line::raw(command.as_str()), 70);
    assert_eq!(
        plain
            .iter()
            .map(|(_, line)| line.to_string())
            .collect::<String>(),
        command
    );
    let highlighted = bob_ui::highlight::highlight_line(&command, "sh");
    let wrapped = style::wrap(&Line::from(highlighted), 70);
    assert_eq!(
        wrapped
            .iter()
            .map(|(_, line)| line.to_string())
            .collect::<String>(),
        command
    );
}

#[test]
fn command_wrap_has_six_row_preview_and_unbounded_expansion() {
    for (width, height) in SIZES.into_iter().chain([(40, 20)]) {
        let command = "c".repeat(width as usize * 510);
        let mut app = app(vec![shell(&command, "", OperationState::Done)]);
        let rows = render::transcript(app.pane().unwrap(), true, width);
        let commands: Vec<_> = rows
            .iter()
            .filter(|row| {
                row.line.to_string().trim_start().starts_with('c')
                    || row.line.to_string().starts_with("• Bash c")
            })
            .collect();
        assert_eq!(commands.len(), 6);
        assert!(commands[0].line.to_string().starts_with("• Bash c"));
        assert!(commands[1..]
            .iter()
            .all(|row| row.line.to_string().starts_with("       c")));
        let (buffer, _) = render(&mut app, width, height);
        let y = find_row(&buffer, "Bash c");
        let highlighted = command_style(&command);
        for offset in 0..6 {
            assert_eq!(buffer[(11, y + offset)].symbol(), "c");
            assert_eq!(Some(buffer[(11, y + offset)].fg), highlighted.fg);
            assert_eq!(buffer[(11, y + offset)].modifier, highlighted.add_modifier);
            if offset > 0 {
                for x in 4..11 {
                    assert_eq!(buffer[(x, y + offset)].symbol(), " ");
                }
            }
            assert_hit(&app, 11, y + offset, |action| {
                matches!(action, HitAction::Tool(_))
            });
        }
        app.pane_mut()
            .unwrap()
            .expanded
            .insert(OperationId("shell-operation".into()));
        let rows = render::transcript(app.pane().unwrap(), true, width);
        let reconstructed: String = rows
            .iter()
            .filter_map(|row| {
                let line = row.line.to_string();
                line.strip_prefix("• Bash ")
                    .or_else(|| line.strip_prefix("       "))
                    .map(str::to_owned)
            })
            .collect();
        assert_eq!(reconstructed, command);
    }
}

#[test]
fn sidebar_is_fifty_two_columns_without_stealing_composer_focus() {
    for (width, height) in SIZES.into_iter().chain([(40, 20), (72, 24), (76, 24)]) {
        let mut app = app(vec![]);
        let mut child = thread("finished child");
        child.parent_thread = Some(ThreadId("root".into()));
        child.state = ThreadState::Completed;
        app.threads.push(child);
        app.draft_mut().insert("draft");
        let mut jobs = JoinSet::new();
        app.key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut jobs,
        );
        assert!(app.sidebar);
        assert_eq!(app.focus, Focus::Composer);
        assert_eq!(app.selected, Some(ThreadId("root".into())));
        let (buffer, cursor) = render(&mut app, width, height);
        let main_width = width - 52.min(width.saturating_sub(24));
        assert!(main_width >= 24);
        assert_eq!(buffer[(main_width, 0)].bg, Palette::INPUT_BG());
        assert_eq!(buffer[(main_width - 1, 0)].bg, Palette::BG());
        assert_band(&buffer, height - 3, main_width);
        assert_eq!(cursor, (12, height - 3));
        let root_hit = app
            .hits
            .iter()
            .find(|hit| matches!(&hit.action, HitAction::Thread(id) if id.0 == "root"))
            .unwrap();
        assert!(root_hit.area.x >= main_width);
        assert_eq!(root_hit.area.right(), width - 1);
        assert_eq!(
            buffer[(main_width + 5, root_hit.area.y)].fg,
            Palette::TEXT()
        );
        assert!(buffer[(main_width + 5, root_hit.area.y)]
            .modifier
            .contains(Modifier::BOLD));
        let child_hit = app
            .hits
            .iter()
            .find(|hit| matches!(&hit.action, HitAction::Thread(id) if id.0 == "finished child"))
            .unwrap();
        assert_eq!(
            buffer[(main_width + 7, child_hit.area.y)].fg,
            Palette::DIM()
        );
        assert!(buffer[(main_width + 7, child_hit.area.y)]
            .modifier
            .contains(Modifier::CROSSED_OUT));
        assert!(app
            .hits
            .iter()
            .filter(|hit| matches!(hit.action, HitAction::Composer))
            .all(|hit| hit.area.right() == main_width));
        app.key(
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut jobs,
        );
        assert!(!app.sidebar);
        assert_eq!(app.focus, Focus::Composer);
        assert_eq!(app.draft_mut().text, "draft");
        render(&mut app, width, height);
        assert!(!app
            .hits
            .iter()
            .any(|hit| matches!(hit.action, HitAction::Thread(_))));
        assert!(jobs.is_empty());
    }
}

#[test]
fn sidebar_cells_match_production_roster_padding_and_task_labels() {
    for (width, height) in SIZES.into_iter().chain([(40, 20), (72, 24), (100, 6)]) {
        let mut app = app(vec![]);
        app.sidebar = true;
        let mut child = thread("internal-agent-name");
        child.parent_thread = Some(ThreadId("root".into()));
        child.state = ThreadState::Active;
        let mut finished = thread("finished");
        finished.parent_thread = Some(child.id.clone());
        finished.state = ThreadState::Completed;
        let task = "Check shell quoting and Unicode 界 paths without changing files";
        app.thread_details = vec![
            bob_kernel::ThreadDetails {
                thread: child.id.clone(),
                task: task.into(),
                depth: 1,
                launch_sequence: 1,
                workflow_operation: None,
            },
            bob_kernel::ThreadDetails {
                thread: finished.id.clone(),
                task: "Review complete".into(),
                depth: 2,
                launch_sequence: 2,
                workflow_operation: None,
            },
        ];
        app.threads.extend([child, finished]);
        let (actual, _) = render(&mut app, width, height);
        let side = 52.min(width.saturating_sub(24));
        let mut reference = Terminal::new(TestBackend::new(side, height)).unwrap();
        reference
            .draw(|frame| {
                let bg = Style::default().bg(Palette::INPUT_BG());
                let area = frame.area();
                frame.render_widget(ratatui::widgets::Clear, area);
                frame.render_widget(Block::default().style(bg), area);
                let pane = bob_ui::widgets::inset(area, 1);
                let mut lines = vec![
                    Line::default(),
                    Line::styled("  AGENTS · 1 running", bg.fg(Palette::DIM())),
                ];
                for (label, depth, color, done, selected) in [
                    ("main", 0, Palette::FAINT(), false, true),
                    (task, 1, Palette::RUNNING(), false, false),
                    ("Review complete", 2, Palette::OK(), true, false),
                ]
                .into_iter()
                .take(height.saturating_sub(4) as usize)
                {
                    lines.push(bob_ui::sidebar::row(
                        label,
                        depth,
                        color,
                        done,
                        selected,
                        pane.width as usize,
                    ));
                }
                lines.push(Line::default());
                lines.push(Line::styled(
                    "  ↑↓ select · ^X stop · esc main · ⌃t close",
                    bg.fg(Palette::FAINT()),
                ));
                frame.render_widget(Paragraph::new(lines).style(bg), pane);
            })
            .unwrap();
        for y in 0..height {
            for x in 0..side {
                assert_eq!(
                    actual[(width - side + x, y)],
                    reference.backend().buffer()[(x, y)],
                    "{width}x{height} sidebar ({x},{y})"
                );
            }
        }
        for (index, hit) in app
            .hits
            .iter()
            .filter(|hit| matches!(hit.action, HitAction::Thread(_)))
            .enumerate()
        {
            assert_eq!(
                hit.area,
                ratatui::layout::Rect::new(width - side + 1, index as u16 + 2, side - 2, 1)
            );
        }
    }
}

#[test]
fn sidebar_retains_the_production_scroll_window_when_selection_moves_up() {
    let mut app = app(vec![]);
    app.sidebar = true;
    for index in 1..=12 {
        let mut child = thread(&format!("agent-{index}"));
        child.parent_thread = Some(ThreadId("root".into()));
        child.state = ThreadState::Active;
        app.thread_details.push(bob_kernel::ThreadDetails {
            thread: child.id.clone(),
            task: format!("Task {index}"),
            depth: 1,
            launch_sequence: index,
            workflow_operation: None,
        });
        app.threads.push(child);
    }
    let mut reference = bob_ui::widgets::SelectList::new();
    let order: Vec<_> = app
        .visible_threads()
        .into_iter()
        .map(|thread| thread.id.clone())
        .collect();
    for (selected, height) in [
        (0, 10u16),
        (6, 10),
        (10, 10),
        (9, 10),
        (8, 10),
        (4, 10),
        (3, 10),
        (3, 20),
        (12, 8),
        (11, 8),
    ] {
        app.select(order[selected].clone());
        reference.selected = selected;
        let range = reference.window(order.len(), height.saturating_sub(4) as usize);
        render(&mut app, 100, height);
        assert_eq!(app.sidebar_scroll, reference.scroll);
        let visible: Vec<_> = app
            .hits
            .iter()
            .filter_map(|hit| match &hit.action {
                HitAction::Thread(id) => Some(id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(visible, order[range]);
    }
}

#[test]
fn control_p_n_history_is_independent_of_sidebar_visibility() {
    for sidebar in [false, true] {
        let mut app = app(vec![]);
        app.sidebar = sidebar;
        app.draft_mut().insert("sent request");
        app.draft_mut().clear();
        app.draft_mut().insert("unsent draft");
        let mut jobs = JoinSet::new();
        app.key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &mut jobs,
        );
        assert_eq!(app.draft_mut().text, "sent request");
        app.key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &mut jobs,
        );
        assert_eq!(app.draft_mut().text, "unsent draft");
        assert_eq!(app.focus, Focus::Composer);
        assert_eq!(app.selected, Some(ThreadId("root".into())));
        assert!(jobs.is_empty());
    }
}

fn theme_colors(theme: Theme) -> [Color; 25] {
    [
        theme.bg,
        theme.text,
        theme.dim,
        theme.faint,
        theme.accent,
        theme.ok,
        theme.error,
        theme.warn,
        theme.running,
        theme.heading,
        theme.list_marker,
        theme.inline_code,
        theme.link,
        theme.blockquote_bar,
        theme.rule,
        theme.table_border,
        theme.code_default,
        theme.diff_add,
        theme.diff_remove,
        theme.diff_add_bg,
        theme.diff_remove_bg,
        theme.diff_gutter,
        theme.input_bg,
        theme.popup_bg,
        theme.selected_bg,
    ]
}

#[test]
fn production_dark_and_light_theme_roles_have_exact_rgb_values() {
    for (name, expected) in [
        (
            "dark",
            [
                0x000000, 0xdddddd, 0x8a8f98, 0x555a63, 0x51afef, 0x8cc265, 0xf76b6b, 0xefbd5d,
                0xefbd5d, 0x51afef, 0x8a8f98, 0xe89d6a, 0x56c8d8, 0x3a3e46, 0x2a2e36, 0x2a2e36,
                0xd4d4d4, 0x8cc265, 0xf76b6b, 0x0d1a0d, 0x1f0e0e, 0x555a63, 0x1a1a20, 0x121316,
                0x1e334a,
            ],
        ),
        (
            "light",
            [
                0xffffff, 0x1d1d1f, 0x6a737d, 0x9da5b4, 0x1f6fb2, 0x2e7d32, 0xc12f2f, 0x9a6300,
                0x9a6300, 0x1f6fb2, 0x6a737d, 0xa63d11, 0x0a7d6b, 0xc4cbd3, 0xd8dde3, 0xc4cbd3,
                0x2a2f35, 0x2e7d32, 0xc12f2f, 0xe4f2e4, 0xfbe6e6, 0x9da5b4, 0xf2f3f5, 0xeceef1,
                0xd4e4f4,
            ],
        ),
    ] {
        for (index, (actual, rgb)) in theme_colors(Theme::by_name(name))
            .into_iter()
            .zip(expected)
            .enumerate()
        {
            assert_eq!(
                actual,
                Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8),
                "{name} role {index}"
            );
        }
    }
}

#[test]
fn terminal_theme_name_uses_production_dark_fallback_not_terminal_reset() {
    let colors = theme_colors(Theme::by_name("terminal"));
    assert_eq!(colors, theme_colors(Theme::dark()));
    assert!(colors.into_iter().all(|color| color != Color::Reset));
}

#[test]
#[ignore = "mutates global production palette; run this test alone with --ignored --test-threads=1"]
fn isolated_theme_switch_repaints_cached_transcript_composer_and_menu() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            bob_ui::theme::set_theme(Theme::dark());
        }
    }
    let _reset = Reset;
    for (width, height) in SIZES {
        let mut app = app(vec![entry(
            3,
            EntryKind::Assistant,
            "# Heading\n\n`code` and **bold**",
        )]);
        app.draft_mut().insert("draft");
        let mut previous_key = None;
        for name in Theme::NAMES.iter().copied().chain(["terminal"]) {
            bob_ui::theme::set_theme(Theme::by_name(name));
            let theme = Theme::by_name(name);
            let (buffer, cursor) = render(&mut app, width, height);
            assert_eq!(buffer[(0, 0)].bg, theme.bg);
            assert_eq!(buffer[(4, height - 3)].bg, theme.input_bg);
            assert_eq!(buffer[(7, height - 3)].fg, theme.text);
            assert_eq!(buffer[(5, height - 3)].symbol(), "›");
            assert_eq!(buffer[(5, height - 3)].fg, theme.dim);
            for y in height - 4..height - 1 {
                assert_band(&buffer, y, width);
            }
            assert_eq!(buffer[(3, height - 1)].fg, theme.dim);
            assert_eq!(buffer[(3, height - 1)].bg, theme.bg);
            assert_eq!(cursor, (12, height - 3));
            let heading = find_row(&buffer, "Heading");
            assert_eq!(buffer[(6, heading)].fg, theme.heading);
            let key = app.pane().unwrap().rendered.as_ref().unwrap().key;
            assert_ne!(previous_key, Some(key));
            previous_key = Some(key);
            app.completion.items = vec![completion::Item {
                label: "/model".into(),
                description: "show the current model".into(),
            }];
            let (menu, _) = render(&mut app, width, height);
            let menu_y = find_row(&menu, "/model");
            assert_eq!(menu[(1, menu_y)].fg, theme.accent);
            assert_eq!(menu[(3, menu_y)].bg, theme.selected_bg);
            assert!(menu[(3, menu_y)].modifier.contains(Modifier::BOLD));
            app.completion.items.clear();
            assert_scrolled_composer(width, height);
            for kind in [completion::Kind::Slash, completion::Kind::File] {
                assert_completion_cells(width, height, kind);
            }
        }
    }
}

#[test]
fn exploration_groups_use_compact_flat_production_headers_and_group_hits() {
    let mut entries = Vec::new();
    for (sequence, name, input) in [
        (1, "read_file", json!({"path":"src/main.rs"})),
        (2, "grep", json!({"pattern":"needle"})),
        (3, "list_dir", json!({"path":"src"})),
    ] {
        let mut item = entry(sequence, EntryKind::ToolResult, "hidden successful body");
        let id = OperationId(format!("op-{sequence}"));
        item.operation = Some(id.clone());
        item.tool = Some(ToolSummary {
            id,
            name: name.into(),
            state: OperationState::Done,
            input_preview: input,
            result: None,
            presentation: None,
        });
        entries.push(item);
    }
    let mut app = app(entries);
    let collapsed = render::transcript(app.pane().unwrap(), true, 80);
    assert_eq!(collapsed.len(), 2);
    assert_eq!(
        collapsed[0].line.to_string(),
        "• Explored 1 file · 1 search · 1 directory"
    );
    app.pane_mut()
        .unwrap()
        .expanded
        .insert(OperationId("op-1".into()));
    let expanded = render::transcript(app.pane().unwrap(), true, 80);
    assert_eq!(expanded.len(), 5);
    assert_eq!(expanded[1].line.to_string(), "  • Read src/main.rs");
    assert_eq!(expanded[2].line.to_string(), "  • Grep needle");
    assert_eq!(expanded[3].line.to_string(), "  • List src");
    assert!(expanded
        .iter()
        .all(|row| matches!(&row.hit, HitAction::Tool(id) if id.0 == "op-1")));
    assert!(!expanded
        .iter()
        .any(|row| row.line.to_string().contains("hidden")));
}
