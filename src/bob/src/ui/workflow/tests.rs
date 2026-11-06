use super::*;

#[path = "consistency_tests.rs"]
mod consistency_tests;
use bob_kernel::{
    Decision, Entry, JobId, ThreadDetails, ToolSummary, WorkflowArtifactKind, WorkflowArtifactLink,
    WorkflowBlocker, WorkflowDependency, WorkflowIteration, WorkflowNode, WorkflowNodeDetails,
    WorkflowOverview, WorkflowPage, WorkflowTermination, WorkflowTextPage,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect};

fn thread(name: &str) -> ThreadSummary {
    ThreadSummary {
        id: ThreadId(name.into()),
        session: SessionId("session".into()),
        parent_run: None,
        parent_thread: (name != "root").then(|| ThreadId("root".into())),
        needs_approval: false,
        activity: None,
        name: name.into(),
        model: "test".into(),
        state: ThreadState::Completed,
        run: None,
    }
}
fn view(entries: Vec<Entry>) -> View {
    View {
        sequence: 1,
        entry_sequence: 1,
        threads: vec![thread("root"), thread("worker")],
        entries,
        policies: vec![],
        decisions: vec![],
        operations: vec![],
        error: None,
    }
}
fn app() -> App {
    let mut app = App::new(50);
    app.threads = vec![thread("root"), thread("worker")];
    app.select(ThreadId("root".into()));
    app.pane_mut().unwrap().view = Some(view(vec![]));
    app
}
fn summary(name: &str, state: WorkflowState) -> WorkflowSummary {
    WorkflowSummary {
        operation: OperationId(name.into()),
        owner: ThreadId("root".into()),
        job: Some(JobId(format!("job-{name}"))),
        title: format!("Review {name}"),
        state,
        admitted: true,
        max_concurrency: 3,
        executions: 51,
        active_agents: 2,
        result: Some(ArtifactId("aggregate".into())),
    }
}
fn node(index: usize) -> WorkflowNode {
    WorkflowNode {
        path: format!("review/{index}"),
        label: format!("Step {index} 界"),
        kind: WorkflowNodeKind::Agent,
        state: WorkflowState::Future,
        instantiated: false,
        expandable: false,
        iteration: 0,
        child: None,
        result: None,
        error_preview: None,
        termination: None,
        selected_branch: None,
    }
}
fn overview(count: usize) -> WorkflowQueryResult {
    WorkflowQueryResult::Overview(WorkflowOverview {
        workflow: summary("workflow", WorkflowState::Running),
        container: None,
        nodes: WorkflowPage {
            rows: (0..count).map(node).collect(),
            next_cursor: Some("next-page".into()),
        },
        definition_available: true,
        definition_error: None,
    })
}
fn main_route() -> Route {
    Route::Workflow {
        operation: OperationId("workflow".into()),
        container: None,
        cursor: None,
    }
}
fn detail_route(path: &str) -> Route {
    Route::Node {
        operation: OperationId("workflow".into()),
        path: path.into(),
        iterations: false,
        cursor: None,
    }
}
fn artifact_route(offset: usize) -> Route {
    Route::Artifact {
        operation: OperationId("workflow".into()),
        source: WorkflowTextSource::Artifact {
            artifact: ArtifactId("result".into()),
        },
        offset,
    }
}
fn detail() -> WorkflowQueryResult {
    let mut node = node(0);
    node.state = WorkflowState::Completed;
    node.instantiated = true;
    node.child = Some(ThreadId("worker".into()));
    node.result = Some(ArtifactId("result".into()));
    node.kind = WorkflowNodeKind::Loop;
    node.expandable = true;
    node.iteration = 3;
    node.termination = Some(WorkflowTermination::MaxRounds);
    node.selected_branch = Some("else".into());
    WorkflowQueryResult::Node(WorkflowNodeDetails {
        operation: OperationId("workflow".into()),
        node,
        specification_preview: "opaque specification, not parsed".into(),
        specification_truncated: true,
        dependencies: vec![WorkflowDependency {
            path: "check".into(),
            state: WorkflowState::Failed,
        }],
        dependencies_truncated: false,
        blockers: vec![WorkflowBlocker {
            state: WorkflowState::WaitingHuman,
            path: None,
            thread: Some(ThreadId("worker".into())),
            message: "Choose an option in the agent conversation".into(),
        }],
        artifacts: vec![WorkflowArtifactLink {
            artifact: ArtifactId("report".into()),
            kind: WorkflowArtifactKind::ChildReport,
        }],
    })
}
fn text_page(offset: usize, source: &str) -> WorkflowQueryResult {
    WorkflowQueryResult::Text(WorkflowTextPage {
        session: SessionId("session".into()),
        operation: OperationId("workflow".into()),
        source: WorkflowTextSource::Artifact {
            artifact: ArtifactId("result".into()),
        },
        offset,
        text: source.into(),
        next_offset: Some(offset + source.len()),
        total_bytes: 100_000,
    })
}
fn pending(ui: &mut WorkflowUi, query: WorkflowQuery, result: WorkflowQueryResult) -> QueryReply {
    ui.ticket += 1;
    let ticket = ui.ticket;
    ui.pending.push((ticket, query.clone()));
    let (session, runtime) = ui.scope.clone().unwrap();
    QueryReply {
        session,
        runtime,
        epoch: ui.epoch,
        ticket,
        sequence: 1,
        query,
        result: Ok(result),
    }
}
fn deliver(ui: &mut WorkflowUi, query: WorkflowQuery, result: WorkflowQueryResult) {
    let reply = pending(ui, query, result);
    ui.accept(reply);
}
fn open(app: &mut App, route: Route, result: WorkflowQueryResult) {
    app.workflow_action(Action::Open(route.clone()), &mut JoinSet::new());
    deliver(&mut app.workflows, route.query().unwrap(), result);
}
fn render(app: &mut App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::ui::render::draw(frame, app))
        .unwrap();
    terminal.backend().buffer().clone()
}
fn screen(buffer: &Buffer) -> String {
    buffer
        .content
        .chunks(buffer.area.width.max(1) as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn click(app: &mut App, area: Rect) {
    app.mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: area.x + area.width.saturating_sub(1),
            row: area.y,
            modifiers: KeyModifiers::NONE,
        },
        &mut JoinSet::new(),
    );
}
fn select_action(app: &mut App, wanted: &Action) {
    render(app, 150, 40);
    app.workflows.selected = app
        .workflows
        .rendered
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .position(|action| action == wanted)
        .unwrap();
    render(app, 150, 40);
}
fn tool_entry() -> Entry {
    Entry {
        sequence: 1,
        sender: None,
        parent_guidance: false,
        submission: None,
        thread: ThreadId("root".into()),
        kind: EntryKind::ToolCall,
        text: "workflow".into(),
        artifact: None,
        operation: Some(OperationId("workflow".into())),
        tool: Some(ToolSummary {
            id: OperationId("workflow".into()),
            name: "workflow".into(),
            state: OperationState::Done,
            input_preview: serde_json::json!({"title":"Review workflow"}),
            result: Some(ArtifactId("result".into())),
            presentation: None,
        }),
        is_preview: false,
        visible_text_bytes: None,
    }
}

#[test]
fn workflow_header_mouse_and_f2_open_same_route_without_losing_secondary_inspection() {
    for mouse in [false, true] {
        let mut app = app();
        app.pane_mut().unwrap().view = Some(view(vec![tool_entry()]));
        render(&mut app, 150, 40);
        assert_eq!(
            app.tool_input_inspection(&OperationId("workflow".into())),
            Some(InspectionTarget::Entry(1))
        );
        assert_eq!(
            app.tool_inspection(&OperationId("workflow".into())),
            Some(InspectionTarget::ToolOutput(OperationId("workflow".into())))
        );
        if mouse {
            let hit = app
                .hits
                .iter()
                .find(|hit| matches!(&hit.action, HitAction::Tool(id) if id.0 == "workflow"))
                .unwrap()
                .area;
            click(&mut app, hit);
        } else {
            app.key(key(KeyCode::F(2)), &mut JoinSet::new());
        }
        assert_eq!(app.workflows.route, Some(main_route()));
        assert_eq!(app.focus, Focus::Workflow);
        assert_eq!(app.selected, Some(ThreadId("root".into())));
    }
}

#[test]
fn sidebar_groups_exact_members_once_and_keeps_unpaged_and_completed_agents() {
    let mut app = app();
    app.sidebar = true;
    let mut unpaged = thread("unpaged");
    unpaged.parent_run = app.threads[1].parent_run.clone();
    app.threads.push(unpaged);
    app.thread_details = vec![
        ThreadDetails {
            thread: ThreadId("worker".into()),
            task: "Grouped worker".into(),
            depth: 1,
            launch_sequence: 1,
            workflow_operation: Some(OperationId("workflow".into())),
        },
        ThreadDetails {
            thread: ThreadId("unpaged".into()),
            task: "Still visible".into(),
            depth: 1,
            launch_sequence: 2,
            workflow_operation: Some(OperationId("not-on-page".into())),
        },
    ];
    let query = app.workflows.list_query();
    deliver(
        &mut app.workflows,
        query,
        WorkflowQueryResult::List(WorkflowPage {
            rows: vec![summary("workflow", WorkflowState::Completed)],
            next_cursor: Some("more".into()),
        }),
    );
    let rows = presentation::sidebar_items(&app);
    for name in ["root", "worker", "unpaged"] {
        assert_eq!(rows.iter().filter(|row| row.action == Action::Open(Route::Conversation(ThreadId(name.into())))).count(), 1);
    }
    let group = rows
        .iter()
        .position(|row| row.action == Action::Open(main_route()))
        .unwrap();
    assert_eq!(
        rows[group + 1].action,
        Action::Open(Route::Conversation(ThreadId("worker".into())))
    );
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.contains("Grouped worker"));
    assert!(shown.contains("Still visible"));
    assert!(!shown.contains("Done"));
    let (row, line) = shown
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("Workflow:"))
        .unwrap();
    let byte = line.find("Workflow:").unwrap();
    let column = line[..byte].chars().count() as u16;
    click(&mut app, Rect::new(column, row as u16, 1, 1));
    assert_eq!(app.workflows.route, Some(main_route()));
    app.workflow_action(Action::Back, &mut JoinSet::new());
    app.focus = Focus::Sidebar;
    app.workflows.sidebar_selected = Some(rows[group.saturating_sub(1)].action.clone());
    app.key(key(KeyCode::Down), &mut JoinSet::new());
    app.key(key(KeyCode::Enter), &mut JoinSet::new());
    assert_eq!(app.workflows.route, Some(main_route()));
}

#[test]
fn node_agent_artifact_back_restores_selection_scroll_anchor_and_separate_drafts() {
    let mut app = app();
    app.draft_mut().insert("main unsent 界");
    let anchor = Anchor {
        identity: AnchorId::Entry(1),
        line: 2,
        offset: 3,
    };
    app.pane_mut().unwrap().viewport.anchor = Some(anchor.clone());
    open(&mut app, main_route(), overview(50));
    app.workflows.selected = 35;
    app.workflows.top = 20;
    app.workflows.expanded = vec!["review/parent".into()];
    open(&mut app, detail_route("review/0"), detail());
    select_action(
        &mut app,
        &Action::Open(Route::Conversation(ThreadId("worker".into()))),
    );
    let selected = app.workflows.selected;
    let top = app.workflows.top;
    app.key(key(KeyCode::Enter), &mut JoinSet::new());
    assert_eq!(app.selected, Some(ThreadId("worker".into())));
    app.draft_mut().insert("worker unsent");
    app.key(key(KeyCode::Esc), &mut JoinSet::new());
    assert_eq!(app.workflows.route, Some(detail_route("review/0")));
    assert_eq!((app.workflows.selected, app.workflows.top), (selected, top));
    assert_eq!(app.draft_mut().text, "main unsent 界");
    assert_eq!(
        app.panes[&ThreadId("worker".into())].draft.text,
        "worker unsent"
    );
    select_action(&mut app, &Action::Open(artifact_route(0)));
    let selected = app.workflows.selected;
    app.key(key(KeyCode::Enter), &mut JoinSet::new());
    deliver(
        &mut app.workflows,
        artifact_route(0).query().unwrap(),
        text_page(0, "first\nsecond\n界"),
    );
    assert!(screen(&render(&mut app, 150, 40)).contains("first"));
    app.key(key(KeyCode::Esc), &mut JoinSet::new());
    assert_eq!(app.workflows.route, Some(detail_route("review/0")));
    assert_eq!(app.workflows.selected, selected);
    app.key(key(KeyCode::Esc), &mut JoinSet::new());
    assert_eq!((app.workflows.selected, app.workflows.top), (35, 20));
    assert_eq!(app.workflows.expanded, ["review/parent"]);
    app.key(key(KeyCode::Esc), &mut JoinSet::new());
    assert_eq!(
        app.workflows.route,
        Some(Route::Conversation(ThreadId("root".into())))
    );
    assert_eq!(app.pane().unwrap().viewport.anchor, Some(anchor));
    assert_eq!(app.draft_mut().text, "main unsent 界");
}

#[test]
fn expansion_is_lazy_ordered_bounded_and_mouse_matches_keyboard() {
    let mut app = app();
    let WorkflowQueryResult::Overview(mut data) = overview(2) else {
        unreachable!()
    };
    data.nodes.rows[0].expandable = true;
    data.nodes.rows[0].kind = WorkflowNodeKind::Conditional;
    data.nodes.rows[0].selected_branch = Some("then".into());
    open(&mut app, main_route(), WorkflowQueryResult::Overview(data));
    assert!(app.workflows.expanded.is_empty());
    assert_eq!(app.workflows.cache.len(), 1);
    select_action(&mut app, &Action::Open(detail_route("review/0")));
    app.key(key(KeyCode::Right), &mut JoinSet::new());
    assert_eq!(app.workflows.expanded, ["review/0"]);
    let mut child = node(10);
    child.state = WorkflowState::NotSelected;
    let query = WorkflowUi::expansion_query(&OperationId("workflow".into()), "review/0");
    deliver(
        &mut app.workflows,
        query,
        WorkflowQueryResult::Overview(WorkflowOverview {
            workflow: summary("workflow", WorkflowState::Running),
            container: Some("review/0".into()),
            nodes: WorkflowPage {
                rows: vec![child],
                next_cursor: None,
            },
            definition_available: true,
            definition_error: None,
        }),
    );
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.find("Step 0").unwrap() < shown.find("Step 10").unwrap());
    assert!(shown.find("Step 10").unwrap() < shown.find("Step 1 界").unwrap());
    assert!(shown.contains("not selected"));
    assert!(shown.contains("Chosen branch: then"));
    let area = app.hits.iter().find(|hit| matches!(&hit.action, HitAction::WorkflowRow(_, Action::Expand(path)) if path == "review/0")).unwrap().area;
    click(&mut app, area);
    assert!(app.workflows.expanded.is_empty());
    for index in 0..50 {
        app.workflow_action(
            Action::Expand(format!("container/{index}")),
            &mut JoinSet::new(),
        );
    }
    assert_eq!(app.workflows.expanded.len(), EXPANSION_LIMIT);
}

#[test]
fn completed_reopened_projection_does_not_infer_goal_or_replay_tool() {
    let mut app = app();
    open(&mut app, detail_route("review/0"), detail());
    let shown = screen(&render(&mut app, 150, 40));
    for needle in [
        "completed",
        "iteration cap reached; goal not confirmed",
        "Chosen branch: else",
        "Dependency: check",
        "waiting for guidance",
        "Open agent conversation",
        "Agent report artifact",
    ] {
        assert!(shown.contains(needle), "missing {needle}: {shown}");
    }
    assert!(!shown.contains("condition met"));
    assert!(app.workflows.pending.is_empty());
    assert!(app.pane().unwrap().pending.is_none());
    let original = app.workflows.route.clone();
    app.workflow_action(
        Action::Open(Route::Conversation(ThreadId("worker".into()))),
        &mut JoinSet::new(),
    );
    app.workflow_action(Action::Back, &mut JoinSet::new());
    assert_eq!(app.workflows.route, original);
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.contains("iteration cap reached"));
}

#[test]
fn stale_navigation_session_runtime_and_wrong_text_source_are_rejected() {
    let mut app = app();
    let old = pending(
        &mut app.workflows,
        main_route().query().unwrap(),
        overview(2),
    );
    app.workflow_action(Action::Open(detail_route("review/0")), &mut JoinSet::new());
    assert_eq!(
        app.workflows.pending.len(),
        1,
        "navigation must retain the in-flight reservation"
    );
    app.accept_workflow(old);
    assert!(app.workflows.pending.is_empty());
    assert!(app.workflows.cache.is_empty());
    let old = pending(
        &mut app.workflows,
        detail_route("review/0").query().unwrap(),
        detail(),
    );
    let mut foreign = thread("foreign");
    foreign.session = SessionId("other-session".into());
    app.threads.push(foreign.clone());
    app.select(foreign.id);
    app.accept_workflow(old);
    assert!(app.workflows.cache.is_empty());
    assert!(!app.workflows.has_back());
    app.select(ThreadId("root".into()));
    let old = pending(
        &mut app.workflows,
        main_route().query().unwrap(),
        overview(2),
    );
    app.runtime_epoch += 1;
    app.threads.clear();
    app.accept_workflow(old);
    assert!(app.workflows.cache.is_empty());
    assert!(app.workflows.scope.is_none());
    app.threads.push(thread("root"));
    app.sync_workflow_scope();
    let mut wrong = text_page(0, "not this source");
    if let WorkflowQueryResult::Text(page) = &mut wrong {
        page.session = SessionId("other-session".into());
    }
    deliver(
        &mut app.workflows,
        artifact_route(0).query().unwrap(),
        wrong,
    );
    assert!(app
        .workflows
        .cached(&artifact_route(0).query().unwrap())
        .unwrap()
        .result
        .is_err());
}

#[test]
fn paged_artifacts_and_return_stack_are_bounded_without_truncating_the_source() {
    let mut app = app();
    open(&mut app, detail_route("review/0"), detail());
    let content = "界\n".repeat(2000);
    for offset in 0..80 {
        open(
            &mut app,
            artifact_route(offset * content.len()),
            text_page(offset * content.len(), &content),
        );
        assert!(app.workflows.bytes <= CACHE_BYTES);
        assert!(app.workflows.cache.len() <= CACHE_ENTRIES);
        assert!(app.workflows.stack.len() <= RETURN_LIMIT);
    }
    let Some(WorkflowQueryResult::Text(page)) = app
        .workflows
        .data(&artifact_route(79 * content.len()).query().unwrap())
    else {
        panic!("text page retained")
    };
    assert_eq!(page.text, content);
    assert_eq!(page.next_offset, Some(80 * content.len()));
    let query = artifact_route(0).query().unwrap();
    deliver(
        &mut app.workflows,
        query.clone(),
        text_page(0, &"x".repeat(CACHE_BYTES)),
    );
    assert!(app.workflows.cached(&query).unwrap().result.is_err());
    assert!(app.workflows.bytes <= CACHE_BYTES);
}

#[test]
fn cached_frames_reuse_visible_formatting_and_selection_tracks_node_identity() {
    let mut app = app();
    open(&mut app, main_route(), overview(50));
    select_action(&mut app, &Action::Open(detail_route("review/20")));
    let builds = app.workflows.frame_builds;
    let formatted = app.workflows.formatted_rows;
    for _ in 0..20 {
        render(&mut app, 150, 40);
    }
    assert_eq!(app.workflows.frame_builds, builds);
    assert_eq!(app.workflows.formatted_rows, formatted);
    deliver(
        &mut app.workflows,
        main_route().query().unwrap(),
        overview(50),
    );
    render(&mut app, 150, 40);
    assert_eq!(app.workflows.frame_builds, builds);
    let WorkflowQueryResult::Overview(mut changed) = overview(50) else {
        unreachable!()
    };
    changed.nodes.rows.insert(0, node(500));
    deliver(
        &mut app.workflows,
        main_route().query().unwrap(),
        WorkflowQueryResult::Overview(changed),
    );
    render(&mut app, 150, 40);
    assert_eq!(
        app.workflows.rendered.as_ref().unwrap().actions[app.workflows.selected],
        Action::Open(detail_route("review/20"))
    );
    assert!(app.workflows.formatted_rows - formatted <= app.workflows.height);
}

#[test]
fn narrow_unicode_and_terminal_controls_keep_existing_palette_and_composer_destination() {
    let mut app = app();
    app.draft_mut().insert("draft 界 e\u{301}");
    open(
        &mut app,
        artifact_route(0),
        text_page(0, "界e\u{301}👩\u{200d}💻\x1b[2J\nlast"),
    );
    for (width, height) in [(1, 1), (8, 4), (12, 5), (26, 8), (80, 24), (150, 40)] {
        let buffer = render(&mut app, width, height);
        assert!(buffer
            .content
            .iter()
            .all(|cell| !cell.symbol().chars().any(char::is_control)));
        assert_eq!(app.draft_mut().text, "draft 界 e\u{301}");
    }
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.contains("Message to main"));
    assert!(shown.contains("last"));
    app.key(key(KeyCode::Tab), &mut JoinSet::new());
    assert_eq!(app.focus, Focus::Composer);
    app.key(key(KeyCode::Char('λ')), &mut JoinSet::new());
    assert_eq!(app.draft_mut().text, "draft 界 e\u{301}λ");
    assert_eq!(app.selected, Some(ThreadId("root".into())));
}

#[test]
fn approvals_and_questions_preempt_navigation_and_preserve_both_drafts() {
    let mut app = app();
    app.draft_mut().insert("/quit");
    open(&mut app, main_route(), overview(2));
    app.focus = Focus::Composer;
    app.refresh_completion();
    app.picker = Some(commands::Picker::theme("dark"));
    let mut next = view(vec![]);
    next.decisions.push(Decision {
        thread: ThreadId("root".into()),
        operation: OperationId("approval".into()),
        revision: 1,
        name: "edit_file".into(),
        preview: "Prepared edit".into(),
        presentation: None,
    });
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(next),
    });
    assert_eq!(app.focus, Focus::Permission);
    assert!(app.picker.is_none());
    let before = app.workflows.route.clone();
    let mut jobs = JoinSet::new();
    app.key(key(KeyCode::Tab), &mut jobs);
    assert_eq!(app.focus, Focus::Permission);
    assert_eq!(app.workflows.route, before);
    assert!(!app.key(key(KeyCode::Enter), &mut jobs));
    app.key(
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
        &mut jobs,
    );
    assert_eq!(app.workflows.route, before);
    assert_eq!(app.draft_mut().text, "/quit");
    assert!(jobs.is_empty());
    app.pane_mut().unwrap().permission = None;
    app.focus = Focus::Composer;
    app.pane_mut().unwrap().question = Some(QuestionPrompt {
        question: bob_kernel::HumanQuestion {
            operation: OperationId("question".into()),
            revision: 1,
            question: "Proceed?".into(),
            detail: None,
            options: vec!["Yes".into(), "No".into()],
        },
        selected: None,
        other: Draft::default(),
        pending: false,
    });
    render(&mut app, 150, 40);
    assert_eq!(app.focus, Focus::Question);
    let hit = app
        .hits
        .iter()
        .find(|hit| matches!(&hit.action, HitAction::WorkflowRow(_, Action::Back)))
        .unwrap()
        .area;
    click(&mut app, hit);
    assert_eq!(app.workflows.route, before);
    app.key(key(KeyCode::Down), &mut jobs);
    assert_eq!(
        app.pane().unwrap().question.as_ref().unwrap().selected,
        Some(0)
    );
    assert_eq!(app.draft_mut().text, "/quit");
}

#[test]
fn stop_scope_uses_only_owned_job_and_escape_never_stops_a_workflow() {
    let mut app = app();
    open(&mut app, main_route(), overview(3));
    let unrelated = summary("other", WorkflowState::Running);
    let query = app.workflows.list_query();
    deliver(
        &mut app.workflows,
        query,
        WorkflowQueryResult::List(WorkflowPage {
            rows: vec![unrelated],
            next_cursor: None,
        }),
    );
    assert_eq!(
        app.workflows.stop_job(&OperationId("workflow".into())),
        Some(JobId("job-workflow".into()))
    );
    assert_eq!(
        app.workflows.stop_job(&OperationId("other".into())),
        Some(JobId("job-other".into()))
    );
    assert!(app
        .workflows
        .stop_job(&OperationId("missing".into()))
        .is_none());
    let mut jobs = JoinSet::new();
    app.key(key(KeyCode::Esc), &mut jobs);
    assert!(jobs.is_empty());
    assert!(!app.workflows.inspecting());
    for state in [
        WorkflowState::Completed,
        WorkflowState::Unknown,
        WorkflowState::Denied,
        WorkflowState::Cancelled,
        WorkflowState::Failed,
        WorkflowState::Approval,
    ] {
        let query = app.workflows.list_query();
        let mut reply = pending(
            &mut app.workflows,
            query,
            WorkflowQueryResult::List(WorkflowPage {
                rows: vec![summary("workflow", state)],
                next_cursor: None,
            }),
        );
        reply.sequence = 20;
        app.workflows.accept(reply);
        assert!(app
            .workflows
            .stop_job(&OperationId("workflow".into()))
            .is_none());
    }
}

#[test]
fn iteration_pages_and_blocker_links_use_typed_paths_without_graph_parsing() {
    let mut app = app();
    open(&mut app, detail_route("review/0"), detail());
    let iterations = Route::Node {
        operation: OperationId("workflow".into()),
        path: "review/0".into(),
        iterations: true,
        cursor: None,
    };
    select_action(&mut app, &Action::Open(iterations.clone()));
    app.key(key(KeyCode::Enter), &mut JoinSet::new());
    deliver(
        &mut app.workflows,
        iterations.query().unwrap(),
        WorkflowQueryResult::Iterations(WorkflowPage {
            rows: vec![WorkflowIteration {
                path: "review/0/pass/3".into(),
                iteration: 3,
                result: ArtifactId("iteration-output".into()),
                termination: Some(WorkflowTermination::MaxRounds),
            }],
            next_cursor: Some("next-iterations".into()),
        }),
    );
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.contains("Iteration 4"));
    assert!(shown.contains("goal not confirmed"));
    assert!(shown.contains("Next page · iterations"));
    assert!(app.workflows.rendered.as_ref().unwrap().actions.iter().any(|action| matches!(action, Action::Open(Route::Artifact { source: WorkflowTextSource::Artifact { artifact }, .. }) if artifact.0 == "iteration-output")));
}

#[test]
fn iteration_labels_are_one_based_and_dry_means_no_new_items() {
    let mut app = app();
    let WorkflowQueryResult::Node(mut details) = detail() else {
        unreachable!()
    };
    details.node.iteration = 0;
    details.node.termination = Some(WorkflowTermination::Dry);
    open(
        &mut app,
        detail_route("review/0"),
        WorkflowQueryResult::Node(details.clone()),
    );
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.contains("iteration 1"));
    assert!(shown.contains("no new items"));
    assert!(!shown.contains("dry run"));
    details.node.kind = WorkflowNodeKind::Agent;
    deliver(
        &mut app.workflows,
        detail_route("review/0").query().unwrap(),
        WorkflowQueryResult::Node(details),
    );
    let shown = screen(&render(&mut app, 150, 40));
    assert!(!shown.contains("iteration 0"));
    assert!(!shown.contains("iteration 1"));
}

#[test]
fn inspection_and_stop_errors_remain_visible_and_refresh_discards_stale_cursor() {
    let mut app = app();
    let route = Route::Workflow {
        operation: OperationId("workflow".into()),
        container: None,
        cursor: Some("expired".into()),
    };
    app.workflow_action(Action::Open(route.clone()), &mut JoinSet::new());
    let mut reply = pending(&mut app.workflows, route.query().unwrap(), overview(2));
    reply.result = Err("Workflow cursor belongs to an older snapshot".into());
    app.accept_workflow(reply);
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.contains("older snapshot"));
    assert!(shown.contains("Refresh inspection"));
    app.key(key(KeyCode::Char('r')), &mut JoinSet::new());
    assert_eq!(app.workflows.route, Some(main_route()));
    deliver(
        &mut app.workflows,
        main_route().query().unwrap(),
        overview(2),
    );
    app.pane_mut().unwrap().pending = Some(7);
    app.reply(Reply::Shortcut {
        thread: ThreadId("root".into()),
        epoch: 7,
        result: Err("stop not authorized".into()),
    });
    assert!(screen(&render(&mut app, 150, 40)).contains("stop not authorized"));
    assert_eq!(app.workflows.route, Some(main_route()));
}

#[test]
fn bounded_navigation_keeps_the_original_conversation_return_location() {
    let mut app = app();
    let root = ThreadId("root".into());
    let anchor = Anchor {
        identity: AnchorId::Entry(1),
        line: 2,
        offset: 3,
    };
    app.draft_mut().insert("original draft");
    app.pane_mut().unwrap().viewport.anchor = Some(anchor.clone());
    app.sidebar_scroll = 7;
    let mut jobs = JoinSet::new();
    app.workflow_action(Action::Open(main_route()), &mut jobs);
    app.workflow_action(
        Action::Open(Route::Conversation(ThreadId("worker".into()))),
        &mut jobs,
    );
    for offset in 0..RETURN_LIMIT * 3 {
        app.workflow_action(Action::Open(artifact_route(offset)), &mut jobs);
        assert!(app.workflows.stack.len() <= RETURN_LIMIT);
    }
    assert_eq!(app.workflows.stack.len(), RETURN_LIMIT);
    for _ in 0..RETURN_LIMIT {
        assert!(app.workflow_key(key(KeyCode::Esc), &mut jobs));
    }
    assert!(app.workflows.stack.is_empty());
    assert_eq!(app.workflows.route, Some(Route::Conversation(root.clone())));
    assert_eq!(app.selected, Some(root));
    assert_eq!(app.focus, Focus::Composer);
    assert_eq!(app.sidebar_scroll, 7);
    assert_eq!(app.pane().unwrap().viewport.anchor, Some(anchor));
    assert_eq!(app.draft_mut().text, "original draft");
    assert!(jobs.is_empty());
}

#[test]
fn empty_inspection_stack_consumes_escape_after_tabbing_to_composer() {
    let mut app = app();
    app.threads[0].state = ThreadState::Active;
    app.workflows.navigate(main_route());
    app.focus = Focus::Workflow;
    let mut jobs = JoinSet::new();
    app.key(key(KeyCode::Tab), &mut jobs);
    assert_eq!(app.focus, Focus::Composer);
    assert!(app.workflows.stack.is_empty());
    assert!(app.workflow_key(key(KeyCode::Esc), &mut jobs));
    assert_eq!(
        app.workflows.route,
        Some(Route::Conversation(ThreadId("root".into())))
    );
    assert_eq!(app.focus, Focus::Composer);
    assert!(jobs.is_empty());
}

#[test]
fn sidebar_selection_survives_completion_reordering_before_stop_or_open() {
    for redraw in [false, true] {
        for removed in [false, true] {
            let mut app = app();
            app.sidebar = true;
            app.focus = Focus::Sidebar;
            let query = app.workflows.list_query();
            deliver(
                &mut app.workflows,
                query.clone(),
                WorkflowQueryResult::List(WorkflowPage {
                    rows: vec![
                        summary("workflow", WorkflowState::Running),
                        summary("other", WorkflowState::Running),
                    ],
                    next_cursor: None,
                }),
            );
            let rows = presentation::sidebar_items(&app);
            let index = rows
                .iter()
                .position(|row| row.action == Action::Open(main_route()))
                .unwrap();
            let mut jobs = JoinSet::new();
            for _ in 0..index {
                app.key(key(KeyCode::Down), &mut jobs);
            }
            assert_eq!(
                app.workflows.sidebar_selected,
                Some(Action::Open(main_route()))
            );
            render(&mut app, 150, 40);
            let rows = if removed {
                vec![summary("other", WorkflowState::Running)]
            } else {
                vec![
                    summary("workflow", WorkflowState::Completed),
                    summary("other", WorkflowState::Running),
                ]
            };
            let mut reply = pending(
                &mut app.workflows,
                query,
                WorkflowQueryResult::List(WorkflowPage {
                    rows,
                    next_cursor: None,
                }),
            );
            reply.sequence = 2;
            app.accept_workflow(reply);
            if redraw {
                render(&mut app, 150, 40);
            }
            assert!(app.workflow_key(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
                &mut jobs,
            ));
            assert_eq!(
                app.workflows.sidebar_selected,
                Some(Action::Open(main_route()))
            );
            app.key(key(KeyCode::Enter), &mut jobs);
            assert_eq!(
                app.workflows.route,
                Some(if removed {
                    Route::Conversation(ThreadId("root".into()))
                } else {
                    main_route()
                })
            );
            assert!(jobs.is_empty());
        }
    }
}

#[test]
fn failed_and_empty_list_pages_keep_visible_mouse_and_keyboard_recovery() {
    for failed in [false, true] {
        for recovery in 0..4 {
            let mut app = app();
            app.sidebar = true;
            app.focus = Focus::Sidebar;
            app.workflows.list_cursor = Some("expired".into());
            let query = app.workflows.list_query();
            let mut reply = pending(
                &mut app.workflows,
                query.clone(),
                WorkflowQueryResult::List(WorkflowPage {
                    rows: vec![],
                    next_cursor: None,
                }),
            );
            if failed {
                reply.result = Err("cursor expired".into());
            }
            app.accept_workflow(reply);
            let shown = screen(&render(&mut app, 150, 40));
            assert!(shown.contains("Previous page · workflows"));
            assert!(!shown.contains("Refresh workflows"));
            assert_eq!(shown.contains("Retry workflows"), failed);
            if failed {
                assert!(shown.contains("cursor expired"));
            }
            assert!(!app.workflows.inspecting());
            let mut jobs = JoinSet::new();
            match recovery {
                0 | 1 => {
                    let wanted = if recovery == 0 || !failed {
                        Action::ListPage(None)
                    } else {
                        Action::Refresh
                    };
                    let area = app
                        .hits
                        .iter()
                        .find(|hit| matches!(&hit.action, HitAction::WorkflowSidebar(action) if action == &wanted))
                        .unwrap()
                        .area;
                    click(&mut app, area);
                }
                2 => {
                    app.workflows.sidebar_selected = Some(if failed {
                        Action::Refresh
                    } else {
                        Action::ListPage(None)
                    });
                    app.key(key(KeyCode::Enter), &mut jobs);
                }
                _ => {
                    app.key(key(KeyCode::Char('r')), &mut jobs);
                }
            }
            assert!(app.workflows.list_cursor.is_none());
            if recovery == 3 || (failed && recovery != 0) {
                assert_eq!(app.workflows.cached(&query).unwrap().sequence, -1);
            }
            assert_eq!(
                app.workflows.route,
                Some(Route::Conversation(ThreadId("root".into())))
            );
            assert_eq!(app.focus, Focus::Sidebar);
        }
    }
}

#[test]
fn empty_workflow_list_hides_refresh_row_but_keeps_keyboard_refresh() {
    let mut app = app();
    app.sidebar = true;
    app.focus = Focus::Sidebar;
    let query = app.workflows.list_query();
    let epoch = app.workflows.epoch;
    assert!(app.workflow_key(key(KeyCode::Char('r')), &mut JoinSet::new()));
    assert!(app.workflows.epoch > epoch);
    deliver(
        &mut app.workflows,
        query.clone(),
        WorkflowQueryResult::List(WorkflowPage {
            rows: vec![],
            next_cursor: None,
        }),
    );
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.contains("main"));
    assert!(!shown.contains("Refresh workflows"));
    assert!(!shown.contains("Retry workflows"));
    assert!(presentation::sidebar_items(&app)
        .iter()
        .all(|item| item.action != Action::Refresh));
    app.key(key(KeyCode::Char('r')), &mut JoinSet::new());
    assert_eq!(app.workflows.cached(&query).unwrap().sequence, -1);
    assert!(!app.workflows.inspecting());
}

#[test]
fn workflow_retry_row_disappears_after_successful_empty_reload() {
    let mut app = app();
    app.sidebar = true;
    app.focus = Focus::Sidebar;
    let query = app.workflows.list_query();
    let page = WorkflowQueryResult::List(WorkflowPage {
        rows: vec![],
        next_cursor: None,
    });
    let mut reply = pending(&mut app.workflows, query.clone(), page.clone());
    reply.result = Err("list unavailable".into());
    app.accept_workflow(reply);
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.contains("Workflow list unavailable"));
    assert!(shown.contains("Retry workflows"));
    app.workflows.sidebar_selected = Some(Action::Refresh);
    app.key(key(KeyCode::Enter), &mut JoinSet::new());
    deliver(&mut app.workflows, query, page);
    let shown = screen(&render(&mut app, 150, 40));
    assert!(shown.contains("main"));
    assert!(presentation::sidebar_items(&app)
        .iter()
        .all(|item| matches!(item.action, Action::Open(Route::Conversation(_)))));
    assert!(!shown.contains("Retry workflows"));
    assert!(app
        .hits
        .iter()
        .all(|hit| !matches!(hit.action, HitAction::WorkflowSidebar(Action::Refresh))));
}

#[test]
fn composer_completions_and_picker_get_tab_and_escape_before_workflow_back() {
    for file in [false, true] {
        let mut app = app();
        open(&mut app, main_route(), overview(1));
        app.key(key(KeyCode::Tab), &mut JoinSet::new());
        if file {
            app.reply(Reply::Files {
                epoch: 0,
                files: vec!["src/main.rs".into()],
            });
            app.draft_mut().insert("@src/ma");
        } else {
            app.draft_mut().insert("/the");
        }
        app.refresh_completion();
        assert!(!app.completion.items.is_empty());
        app.key(key(KeyCode::Tab), &mut JoinSet::new());
        assert_eq!(app.focus, Focus::Composer);
        assert_eq!(app.workflows.route, Some(main_route()));
        assert_eq!(
            app.draft_mut().text,
            if file { "@src/main.rs" } else { "/theme" }
        );
        app.refresh_completion();
        assert!(!app.completion.items.is_empty());
        app.key(key(KeyCode::Esc), &mut JoinSet::new());
        assert!(app.completion.items.is_empty());
        assert_eq!(app.workflows.route, Some(main_route()));
        assert_eq!(app.workflows.stack.len(), 1);
        app.key(key(KeyCode::Esc), &mut JoinSet::new());
        assert!(!app.workflows.inspecting());
    }
    let mut app = app();
    open(&mut app, main_route(), overview(1));
    app.key(key(KeyCode::Tab), &mut JoinSet::new());
    app.picker = Some(commands::Picker::theme("dark"));
    app.key(key(KeyCode::Tab), &mut JoinSet::new());
    assert_eq!(app.focus, Focus::Composer);
    assert!(app.picker.is_some());
    app.key(key(KeyCode::Esc), &mut JoinSet::new());
    assert!(app.picker.is_none());
    assert_eq!(app.workflows.route, Some(main_route()));
    assert_eq!(app.workflows.stack.len(), 1);
    app.key(key(KeyCode::Esc), &mut JoinSet::new());
    assert!(!app.workflows.inspecting());
}

#[test]
fn nested_expansion_mouse_hitbox_tracks_the_rendered_glyph() {
    let mut app = app();
    for depth in 0..=3 {
        let WorkflowQueryResult::Overview(mut data) = overview(1) else {
            unreachable!()
        };
        let path = format!("nested/{depth}");
        data.nodes.rows[0].path = path.clone();
        data.nodes.rows[0].expandable = true;
        data.nodes.next_cursor = None;
        if depth == 0 {
            open(&mut app, main_route(), WorkflowQueryResult::Overview(data));
        } else {
            let parent = format!("nested/{}", depth - 1);
            data.container = Some(parent.clone());
            app.workflow_action(Action::Expand(parent.clone()), &mut JoinSet::new());
            deliver(
                &mut app.workflows,
                WorkflowUi::expansion_query(&OperationId("workflow".into()), &parent),
                WorkflowQueryResult::Overview(data),
            );
        }
    }
    for depth in [3, 2] {
        let path = format!("nested/{depth}");
        let buffer = render(&mut app, 150, 40);
        let expansion = app
            .hits
            .iter()
            .find(|hit| matches!(&hit.action, HitAction::WorkflowRow(_, Action::Expand(target)) if target == &path))
            .unwrap()
            .area;
        let row = app
            .hits
            .iter()
            .find(|hit| matches!(&hit.action, HitAction::WorkflowRow(_, Action::Open(Route::Node { path: target, .. })) if target == &path))
            .unwrap()
            .area;
        assert_eq!(expansion.x, row.x + 2 + 2 * depth);
        let was_expanded = app.workflows.expanded.contains(&path);
        assert_eq!(
            buffer[(expansion.x, expansion.y)].symbol(),
            if was_expanded { "−" } else { "+" }
        );
        click(&mut app, Rect::new(expansion.x, expansion.y, 1, 1));
        assert_eq!(app.workflows.route, Some(main_route()));
        assert_eq!(app.workflows.expanded.contains(&path), !was_expanded);
    }
}

#[test]
fn returning_to_a_scope_does_not_reuse_an_old_query_reservation() {
    let mut app = app();
    let query = app.workflows.list_query();
    let result = WorkflowQueryResult::List(WorkflowPage {
        rows: vec![summary("workflow", WorkflowState::Running)],
        next_cursor: None,
    });
    let old = pending(&mut app.workflows, query.clone(), result.clone());
    let old_ticket = old.ticket;
    let mut foreign = thread("foreign");
    foreign.session = SessionId("other-session".into());
    app.threads.push(foreign.clone());
    app.select(foreign.id);
    let foreign_reply = pending(&mut app.workflows, query.clone(), result.clone());
    app.select(ThreadId("root".into()));
    let current = pending(&mut app.workflows, query.clone(), result);
    let ticket = current.ticket;
    assert!(ticket > foreign_reply.ticket && foreign_reply.ticket > old_ticket);
    app.accept_workflow(old);
    app.accept_workflow(foreign_reply);
    assert_eq!(app.workflows.pending, vec![(ticket, query.clone())]);
    assert!(app.workflows.cache.is_empty());
    app.accept_workflow(current);
    assert!(app.workflows.pending.is_empty());
    assert!(app.workflows.data(&query).is_some());
    app.runtime_epoch += 1;
    app.threads.clear();
    app.sync_workflow_scope();
    assert_eq!(app.workflows.ticket, ticket);
    app.threads.push(thread("root"));
    app.sync_workflow_scope();
    assert_eq!(app.workflows.ticket, ticket);
}

#[test]
#[ignore = "synthetic workflow paging/frame timing fixture; parent runs explicitly"]
fn large_workflow_pages_measure_cold_cached_frames_and_retained_iterations() {
    let mut app = app();
    let fixture: Vec<_> = (0..600).map(node).collect();
    let iterations: Vec<_> = (0..200)
        .map(|iteration| WorkflowIteration {
            path: format!("loop/pass/{iteration}"),
            iteration,
            result: ArtifactId(format!("iteration-{iteration}")),
            termination: (iteration == 199).then_some(WorkflowTermination::MaxRounds),
        })
        .collect();
    let start = std::time::Instant::now();
    for (index, nodes) in fixture.chunks(50).enumerate() {
        let route = Route::Workflow {
            operation: OperationId("workflow".into()),
            container: None,
            cursor: Some(format!("page-{index}")),
        };
        let result = WorkflowQueryResult::Overview(WorkflowOverview {
            workflow: summary("workflow", WorkflowState::Completed),
            container: None,
            nodes: WorkflowPage {
                rows: nodes.to_vec(),
                next_cursor: (index < 11).then(|| format!("page-{}", index + 1)),
            },
            definition_available: true,
            definition_error: None,
        });
        open(&mut app, route, result);
        render(&mut app, 150, 40);
    }
    for (index, rows) in iterations.chunks(50).enumerate() {
        let route = Route::Node {
            operation: OperationId("workflow".into()),
            path: "loop".into(),
            iterations: true,
            cursor: Some(format!("iterations-{index}")),
        };
        open(
            &mut app,
            route,
            WorkflowQueryResult::Iterations(WorkflowPage {
                rows: rows.to_vec(),
                next_cursor: None,
            }),
        );
        render(&mut app, 150, 40);
    }
    let cold = start.elapsed();
    let builds = app.workflows.frame_builds;
    let formatted = app.workflows.formatted_rows;
    let start = std::time::Instant::now();
    for _ in 0..1000 {
        render(&mut app, 150, 40);
    }
    let cached = start.elapsed();
    assert_eq!(app.workflows.frame_builds, builds);
    assert_eq!(app.workflows.formatted_rows, formatted);
    assert!(app.workflows.bytes <= CACHE_BYTES);
    assert!(app.workflows.cache.len() <= CACHE_ENTRIES);
    eprintln!("workflow fixture: 600 nodes / 200 retained iterations; cold={cold:?}; 1000 cached frames={cached:?}; frame_builds={builds}; formatted_rows={formatted}; cache_bytes={}", app.workflows.bytes);
}
