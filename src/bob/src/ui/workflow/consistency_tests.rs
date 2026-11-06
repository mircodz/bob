use super::*;

#[test]
fn tree_navigation_skips_metadata_and_left_never_reopens_a_container() {
    let mut app = app();
    let WorkflowQueryResult::Overview(mut data) = overview(2) else {
        unreachable!()
    };
    data.nodes.rows[0].expandable = true;
    data.nodes.rows[0].selected_branch = Some("chosen".into());
    open(&mut app, main_route(), WorkflowQueryResult::Overview(data));
    select_action(&mut app, &Action::Open(detail_route("review/0")));
    let mut jobs = JoinSet::new();
    app.workflow_key(key(KeyCode::Left), &mut jobs);
    assert!(app.workflows.expanded.is_empty());
    app.workflow_key(key(KeyCode::Right), &mut jobs);
    assert_eq!(app.workflows.expanded, ["review/0"]);
    let query = WorkflowUi::expansion_query(&OperationId("workflow".into()), "review/0");
    let WorkflowQueryResult::Overview(mut children) = overview(1) else {
        unreachable!()
    };
    children.container = Some("review/0".into());
    children.nodes.rows = vec![node(10)];
    children.nodes.next_cursor = None;
    deliver(
        &mut app.workflows,
        query,
        WorkflowQueryResult::Overview(children),
    );
    render(&mut app, 150, 40);
    app.workflow_key(key(KeyCode::Right), &mut jobs);
    assert_eq!(
        selected_action(&app),
        &Action::Open(detail_route("review/10"))
    );
    app.workflow_key(key(KeyCode::Left), &mut jobs);
    assert_eq!(
        selected_action(&app),
        &Action::Open(detail_route("review/0"))
    );
    app.workflow_key(key(KeyCode::Left), &mut jobs);
    app.workflow_key(key(KeyCode::Left), &mut jobs);
    assert!(app.workflows.expanded.is_empty());
    render(&mut app, 150, 40);
    app.workflow_key(key(KeyCode::Down), &mut jobs);
    assert_eq!(
        selected_action(&app),
        &Action::Open(detail_route("review/1"))
    );
    app.workflow_key(key(KeyCode::Home), &mut jobs);
    app.workflow_key(key(KeyCode::Up), &mut jobs);
    assert_eq!(selected_action(&app), &Action::Back);
}

fn selected_action(app: &App) -> &Action {
    &app.workflows.rendered.as_ref().unwrap().actions[app.workflows.selected]
}

#[test]
fn artifact_scrolling_never_selects_body_rows_or_changes_data_pages() {
    let mut app = app();
    let body = (0..100)
        .map(|i| format!("body line {i}\n"))
        .collect::<String>();
    open(&mut app, artifact_route(0), text_page(0, &body));
    render(&mut app, 100, 24);
    let selected = app.workflows.selected;
    let mut jobs = JoinSet::new();
    for code in [KeyCode::Down, KeyCode::PageDown, KeyCode::End] {
        app.workflow_key(key(code), &mut jobs);
        render(&mut app, 100, 24);
        assert_eq!(app.workflows.selected, selected);
        assert_eq!(app.workflows.route, Some(artifact_route(0)));
    }
    let shown = screen(&render(&mut app, 100, 24));
    assert!(shown.contains("body line 99"));
    assert!(shown.contains("Next page"));
    app.workflow_scroll(isize::MAX);
    render(&mut app, 100, 24);
    assert_eq!(app.workflows.selected, selected);
    app.workflow_key(key(KeyCode::Home), &mut jobs);
    assert_eq!(app.workflows.top, 0);
    app.workflow_key(key(KeyCode::Right), &mut jobs);
    assert_eq!(
        selected_action(&app),
        &Action::Open(artifact_route(body.len()))
    );
    app.workflow_key(key(KeyCode::Enter), &mut jobs);
    assert_eq!(app.workflows.route, Some(artifact_route(body.len())));
    app.workflow_key(key(KeyCode::Esc), &mut jobs);
    assert_eq!(app.workflows.route, Some(artifact_route(0)));
    assert_eq!(app.workflows.top, 0);
}

#[test]
fn workflow_keyboard_selection_is_focus_aware_and_body_mouse_is_not_selection() {
    let mut app = app();
    open(
        &mut app,
        artifact_route(0),
        text_page(0, "visible body text\nsecond line"),
    );
    let focused = render(&mut app, 100, 24);
    let selected_background = crate::ui::style::Palette::SELECTED_BG();
    let action_area = app
        .hits
        .iter()
        .find(|hit| matches!(&hit.action, HitAction::WorkflowRow(_, Action::Back)))
        .unwrap()
        .area;
    assert_eq!(
        focused[(action_area.x, action_area.y)].bg,
        selected_background
    );
    app.focus = Focus::Composer;
    let blurred = render(&mut app, 100, 24);
    assert_ne!(
        blurred[(action_area.x, action_area.y)].bg,
        selected_background
    );
    let body_row = screen(&blurred)
        .lines()
        .position(|line| line.contains("visible body text"))
        .unwrap();
    let before = app.workflows.selected;
    click(
        &mut app,
        ratatui::layout::Rect::new(12, body_row as u16, 1, 1),
    );
    assert_eq!(app.focus, Focus::Workflow);
    assert_eq!(app.workflows.selected, before);
}

#[test]
fn sidebar_page_keys_skip_error_rows_and_previous_page_is_not_first_page() {
    let mut app = app();
    app.sidebar = true;
    app.focus = Focus::Sidebar;
    let mut jobs = JoinSet::new();
    app.workflow_action(Action::ListPage(Some("second".into())), &mut jobs);
    app.workflow_action(Action::ListPage(Some("third".into())), &mut jobs);
    let query = app.workflows.list_query();
    let mut reply = pending(
        &mut app.workflows,
        query,
        WorkflowQueryResult::List(bob_kernel::WorkflowPage {
            rows: vec![],
            next_cursor: None,
        }),
    );
    reply.result = Err("test unavailable".into());
    app.accept_workflow(reply);
    render(&mut app, 120, 25);
    app.workflow_key(key(KeyCode::End), &mut jobs);
    assert_eq!(
        app.workflows.sidebar_selected,
        Some(Action::ListPage(Some("second".into())))
    );
    app.workflow_key(key(KeyCode::Up), &mut jobs);
    assert_eq!(app.workflows.sidebar_selected, Some(Action::Refresh));
    app.workflow_key(key(KeyCode::Up), &mut jobs);
    assert!(matches!(
        app.workflows.sidebar_selected,
        Some(Action::Open(Route::Conversation(_)))
    ));
    app.workflow_key(key(KeyCode::End), &mut jobs);
    app.workflow_key(key(KeyCode::Enter), &mut jobs);
    assert_eq!(app.workflows.list_cursor.as_deref(), Some("second"));
    let query = app.workflows.list_query();
    deliver(
        &mut app.workflows,
        query,
        WorkflowQueryResult::List(bob_kernel::WorkflowPage {
            rows: vec![],
            next_cursor: None,
        }),
    );
    render(&mut app, 120, 25);
    app.workflow_key(key(KeyCode::End), &mut jobs);
    assert_eq!(app.workflows.sidebar_selected, Some(Action::ListPage(None)));
    app.workflow_key(key(KeyCode::Enter), &mut jobs);
    assert_eq!(app.workflows.list_cursor, None);
}

#[test]
fn duplicate_actions_keep_distinct_selection_through_refresh_and_back() {
    let mut app = app();
    let WorkflowQueryResult::Node(mut data) = detail() else {
        unreachable!()
    };
    data.dependencies_truncated = true;
    open(
        &mut app,
        detail_route("review/0"),
        WorkflowQueryResult::Node(data.clone()),
    );
    render(&mut app, 150, 40);
    let action = Action::Open(Route::Artifact {
        operation: OperationId("workflow".into()),
        source: WorkflowTextSource::NodeSpecification {
            path: "review/0".into(),
        },
        offset: 0,
    });
    app.workflows.selected = app
        .workflows
        .rendered
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .rposition(|candidate| candidate == &action)
        .unwrap();
    render(&mut app, 150, 40);
    let identity = app
        .workflows
        .rendered
        .as_ref()
        .unwrap()
        .identity(app.workflows.selected);
    data.node.error_preview = Some("new warning".into());
    data.node.state = WorkflowState::Blocked;
    deliver(
        &mut app.workflows,
        detail_route("review/0").query().unwrap(),
        WorkflowQueryResult::Node(data),
    );
    render(&mut app, 150, 40);
    assert_eq!(
        app.workflows
            .rendered
            .as_ref()
            .unwrap()
            .identity(app.workflows.selected),
        identity
    );
    let first = app
        .workflows
        .rendered
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|candidate| candidate == &action)
        .unwrap();
    assert_ne!(app.workflows.selected, first);
    let selected = app.workflows.selected;
    let top = app.workflows.top;
    app.workflow_key(key(KeyCode::Enter), &mut JoinSet::new());
    app.workflow_key(key(KeyCode::Esc), &mut JoinSet::new());
    render(&mut app, 150, 40);
    assert_eq!(app.workflows.selected, selected);
    assert_eq!(app.workflows.top, top);
}

#[test]
fn refresh_before_redraw_opens_the_same_node_after_reordering() {
    let mut app = app();
    open(&mut app, main_route(), overview(10));
    select_action(&mut app, &Action::Open(detail_route("review/5")));
    let WorkflowQueryResult::Overview(mut changed) = overview(10) else {
        unreachable!()
    };
    changed.nodes.rows.insert(0, node(500));
    changed.nodes.rows[6].state = WorkflowState::WaitingHuman;
    deliver(
        &mut app.workflows,
        main_route().query().unwrap(),
        WorkflowQueryResult::Overview(changed),
    );
    app.workflow_key(key(KeyCode::Enter), &mut JoinSet::new());
    assert_eq!(app.workflows.route, Some(detail_route("review/5")));
}

#[test]
fn node_preview_tail_is_readable_without_selectable_body_rows() {
    let mut app = app();
    let WorkflowQueryResult::Node(mut data) = detail() else {
        unreachable!()
    };
    data.specification_preview = format!("{}preview tail 界", "long body line\n".repeat(80));
    open(
        &mut app,
        detail_route("review/0"),
        WorkflowQueryResult::Node(data),
    );
    render(&mut app, 80, 20);
    let mut jobs = JoinSet::new();
    app.workflow_key(key(KeyCode::End), &mut jobs);
    let shown = screen(&render(&mut app, 80, 20));
    assert!(shown.contains("preview tail 界"));
    assert_ne!(selected_action(&app), &Action::None);
    let selected = app.workflows.selected;
    app.workflow_scroll(-3);
    render(&mut app, 80, 20);
    assert_eq!(app.workflows.selected, selected);
}

#[test]
fn eligible_navigation_and_scroll_primitives_handle_empty_and_extreme_bounds() {
    use bob_ui::widgets::{eligible_selection, scroll_offset, SelectionMove};
    assert_eq!(eligible_selection(Some(0), &[], SelectionMove::Next), None);
    let rows = [2, 6, 9];
    for (current, movement, expected) in [
        (2, SelectionMove::Previous, 2),
        (9, SelectionMove::Next, 9),
        (3, SelectionMove::Next, 6),
        (8, SelectionMove::Previous, 6),
        (usize::MAX, SelectionMove::Nearest, 9),
        (6, SelectionMove::First, 2),
        (6, SelectionMove::Last, 9),
    ] {
        assert_eq!(
            eligible_selection(Some(current), &rows, movement),
            Some(expected)
        );
    }
    assert_eq!(scroll_offset(5, isize::MIN, 100, 10), 0);
    assert_eq!(scroll_offset(5, isize::MAX, 100, 10), 90);
    assert_eq!(scroll_offset(5, 1, 0, 0), 0);
    assert_eq!(scroll_offset(5, 1, 3, 10), 0);
}
