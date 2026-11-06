use super::*;
use crate::ui::style::{Palette, BAND_INSET};

#[test]
fn working_footer_matches_production_alignment_color_and_spacing() {
    let mut app = app(vec![entry(1, "A short reply", None)]);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let lines: Vec<String> = buffer
        .content
        .chunks(100)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect())
        .collect();
    let row = lines
        .iter()
        .position(|line| line.contains("Working ("))
        .unwrap();
    assert!(
        row < 10,
        "working footer should follow the short transcript"
    );
    assert!(lines[row].trim_start().starts_with("• Working ("));
    assert!(lines[row].trim_end().ends_with("s · esc to interrupt)"));
    assert!(lines[row + 1].trim().is_empty());
    assert_eq!(buffer[(BAND_INSET, row as u16)].symbol(), "•");
    assert_eq!(buffer[(BAND_INSET, row as u16)].fg, Palette::RUNNING());
    let suffix = lines[row].chars().position(|ch| ch == '(').unwrap() as u16;
    assert_eq!(buffer[(suffix, row as u16)].fg, Palette::FAINT());
    let pane = app.pane().unwrap();
    assert_eq!(
        pane.viewport.rows.last().unwrap().identity,
        AnchorId::Activity
    );
    assert!(pane
        .rendered
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .all(|row| row.anchor.identity != AnchorId::Activity));
}

#[test]
fn working_footer_scrolls_with_history_and_does_not_rebuild_transcript() {
    let history = (1..=80)
        .map(|id| entry(id, &format!("History {id}"), None))
        .collect();
    let mut app = app(history);
    assert!(render(&mut app, 90, 24).contains("esc to interrupt"));
    let builds = app.pane().unwrap().render_builds;
    app.started_at -= Duration::from_secs(2);
    assert!(render(&mut app, 90, 24).contains("esc to interrupt"));
    assert_eq!(app.pane().unwrap().render_builds, builds);
    app.pane_mut().unwrap().scroll(-30);
    let screen = render(&mut app, 90, 24);
    assert!(screen.contains("History"));
    assert!(!screen.contains("Working ("));
    let anchor = app.pane().unwrap().viewport.anchor.clone();
    render(&mut app, 72, 24);
    assert_eq!(app.pane().unwrap().viewport.anchor, anchor);
    app.pane_mut().unwrap().scroll(1000);
    assert!(render(&mut app, 72, 24).contains("esc to interrupt"));
    app.pane_mut().unwrap().before = Some(81);
    assert!(!render(&mut app, 72, 24).contains("Working ("));
}

#[test]
fn narrow_working_footer_wraps_without_dropping_the_hint() {
    for (width, sidebar) in [(24, false), (40, false), (72, true), (120, true)] {
        let mut app = app(vec![]);
        app.sidebar = sidebar;
        let screen = render(&mut app, width, 30);
        let conversation_width = if sidebar {
            width - 52.min(width.saturating_sub(24))
        } else {
            width
        };
        let joined: String = screen
            .lines()
            .flat_map(|line| {
                line.chars()
                    .take(conversation_width as usize)
                    .filter(|ch| !ch.is_whitespace())
            })
            .collect();
        assert!(joined.contains("Working("), "{width}/{sidebar}: {screen}");
        assert!(
            joined.contains("esctointerrupt)"),
            "{width}/{sidebar}: {screen}"
        );
        assert!(
            app.pane()
                .unwrap()
                .viewport
                .rows
                .iter()
                .filter(|row| row.identity == AnchorId::Activity)
                .count()
                >= 2
        );
    }
}

#[test]
fn child_working_hint_matches_escape_return_without_losing_drafts() {
    let mut app = app(vec![]);
    app.draft_mut().insert("root draft");
    let mut child = thread("child");
    child.parent_thread = Some(ThreadId("root".into()));
    app.threads.push(child);
    app.select(ThreadId("child".into()));
    app.draft_mut().insert("child draft");
    let screen = render(&mut app, 100, 30);
    assert!(screen.contains("^X stop · esc main"));
    assert!(!screen.contains("esc to interrupt"));
    let mut jobs = JoinSet::new();
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut jobs);
    assert!(jobs.is_empty());
    assert_eq!(app.selected, Some(ThreadId("root".into())));
    assert_eq!(app.draft_mut().text, "root draft");
    app.select(ThreadId("child".into()));
    assert_eq!(app.draft_mut().text, "child draft");
    app.sidebar = true;
    app.focus = Focus::Sidebar;
    assert!(render(&mut app, 160, 30).contains("^X stop · esc main"));
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut jobs);
    assert!(jobs.is_empty());
    assert_eq!(app.selected, Some(ThreadId("root".into())));
    assert_eq!(app.draft_mut().text, "root draft");
}

#[test]
fn workflow_back_and_completion_hints_do_not_advertise_cancellation() {
    let mut app = app(vec![]);
    let mut jobs = JoinSet::new();
    app.open_workflow(OperationId("workflow".into()), &mut jobs);
    assert!(!render(&mut app, 100, 30).contains("esc to interrupt"));
    app.workflow_action(
        workflow::Action::Open(workflow::Route::Conversation(ThreadId("root".into()))),
        &mut jobs,
    );
    assert!(render(&mut app, 100, 30).contains("^X stop · esc back"));
    app.paste("/the");
    assert!(!app.completion.items.is_empty());
    assert!(render(&mut app, 100, 30).contains("^X stop · esc close"));
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut jobs);
    assert!(app.completion.items.is_empty());
    assert!(!app.workflows.inspecting());
    assert!(app.workflows.has_back());
    assert!(render(&mut app, 100, 30).contains("^X stop · esc back"));
    app.draft_mut().clear();
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut jobs);
    assert!(app.workflows.inspecting());
    assert!(jobs.is_empty());
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut jobs);
    assert!(!app.workflows.inspecting());
    app.paste("/the");
    assert!(!app.completion.items.is_empty());
    assert!(render(&mut app, 100, 30).contains("^X stop · esc close"));
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut jobs);
    assert!(app.completion.items.is_empty());
    assert!(jobs.is_empty());
    assert!(render(&mut app, 100, 30).contains("esc to interrupt"));
}

#[test]
fn settled_work_keeps_full_inline_answer_without_working_footer() {
    let body = format!(
        "{}\ninline-tail-marker",
        "Complete answer paragraph\n".repeat(180)
    );
    let mut app = app(vec![entry(1, &body, None)]);
    app.threads[0].state = ThreadState::Completed;
    app.threads[0].activity = None;
    let screen = render(&mut app, 90, 24);
    assert!(screen.contains("inline-tail-marker"));
    assert!(!screen.contains("Working ("));
    assert!(app
        .pane()
        .unwrap()
        .viewport
        .rows
        .iter()
        .all(|row| row.identity != AnchorId::Activity));
}
