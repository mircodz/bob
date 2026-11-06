use super::*;
use crate::cli::tests::TestDirectory;
use bob_kernel::{SessionId, ThreadId};
use bob_ui::theme::Palette;
use ratatui::{backend::TestBackend, buffer::Buffer, style::Modifier, Terminal};

fn item(id: &str, title: &str, preview: &str) -> PickerItem {
    PickerItem::new(SessionSummary {
        id: SessionId(id.into()),
        thread: ThreadId(format!("thread-{id}")),
        ordinal: 1,
        workspace: "/fixture".into(),
        model: "copilot:fixture".into(),
        preview: preview.into(),
        title: title.into(),
        message_count: 3,
        updated_at: 0,
        human_previews: vec![format!("you: {preview}")],
    })
}

fn list(items: Vec<PickerItem>) -> SessionList {
    SessionList {
        items,
        loaded: true,
        ..Default::default()
    }
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn screen(buffer: &Buffer) -> String {
    buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn draw(
    lists: &[SessionList; 2],
    state: &mut PickerState,
    width: u16,
    height: u16,
) -> Result<Buffer> {
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    let filtered = state.filtered(&lists[state.scope.index()].items);
    terminal.draw(|frame| {
        render::draw_picker(frame, lists, &filtered, state, Path::new("/fixture"), None)
    })?;
    Ok(terminal.backend().buffer().clone())
}

#[test]
fn filter_matches_titles_directories_ids_and_preview_with_unicode_subsequences() {
    for (query, haystack, expected) in [
        ("", "anything", true),
        ("fb", "foo bar", true),
        ("BAR", "foo bar", true),
        ("foobar", "foo bar", true),
        ("baz", "foo bar", false),
        ("rab", "foo bar", false),
        ("ÉA", "éclair", true),
    ] {
        assert_eq!(fuzzy_match(query, haystack), expected);
    }
    let mut summary = item("unique-123", "Fix database", "restore interrupted work").summary;
    summary.workspace = "/Projects/界/kusto".into();
    let items = [PickerItem::new(summary), item("other", "Other", "nothing")];
    for query in ["fxdtb", "KUSTO", "界", "unique-123", "intrpt"] {
        let mut state = PickerState {
            query: query.into(),
            ..Default::default()
        };
        assert_eq!(state.filtered(&items), [0], "{query}");
    }
}

#[test]
fn tabs_keep_query_and_empty_local_view_never_falls_back_implicitly() -> Result<()> {
    let lists = [
        list(vec![]),
        list(vec![item("elsewhere", "Other workspace", "history")]),
    ];
    let mut state = PickerState::default();
    assert!(state.scope == Scope::Local);
    let empty = state.filtered(&lists[0].items);
    assert!(empty.is_empty());
    assert_eq!(state.key(press(KeyCode::Enter), &empty), None);
    let buffer = draw(&lists, &mut state, 90, 24)?;
    assert!(screen(&buffer).contains("Press Tab to browse All"));
    state.insert("other");
    state.selected = 10;
    state.scroll = 8;
    assert_eq!(state.key(press(KeyCode::Tab), &[]), None);
    assert!(state.scope == Scope::All);
    assert_eq!(state.query, "other");
    assert_eq!((state.selected, state.scroll), (0, 0));
    let filtered = state.filtered(&lists[1].items);
    assert_eq!(filtered, [0]);
    assert_eq!(
        state.key(press(KeyCode::Enter), &filtered),
        Some(Action::Open(0))
    );
    state.key(press(KeyCode::BackTab), &filtered);
    assert!(state.scope == Scope::Local);
    assert_eq!(state.query, "other");
    Ok(())
}

#[test]
fn navigation_search_letters_and_empty_results_do_not_accept_wrong_session() {
    let items = [item("a", "First", "one"), item("b", "Second", "two")];
    let mut state = PickerState {
        page_size: 2,
        ..Default::default()
    };
    let filtered = state.filtered(&items);
    state.key(press(KeyCode::Down), &filtered);
    assert_eq!(state.selected, 1);
    assert_eq!(
        state.key(press(KeyCode::Enter), &filtered),
        Some(Action::Open(1))
    );
    assert_eq!(
        state.key(press(KeyCode::Esc), &filtered),
        Some(Action::Cancel)
    );
    assert_eq!(
        state.key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &filtered
        ),
        Some(Action::Cancel)
    );
    for c in ['j', 'k'] {
        state.key(press(KeyCode::Char(c)), &filtered);
    }
    assert_eq!(state.query, "jk");
    assert_eq!(state.selected, 0);
    let empty = state.filtered(&items);
    assert!(empty.is_empty());
    assert_eq!(state.key(press(KeyCode::Enter), &empty), None);
    state.key(
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        &empty,
    );
    assert_eq!(state.filtered(&items), [0, 1]);
    assert_eq!(
        state.key(
            KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Release),
            &[0, 1]
        ),
        None
    );
}

#[test]
fn pasted_search_is_bounded_sanitized_and_backspace_removes_a_grapheme() {
    let mut state = PickerState::default();
    state.insert("界e\u{301}");
    state.key(press(KeyCode::Backspace), &[]);
    assert_eq!(state.query, "界");
    state.insert("\nfolder\u{1b}]2;untrusted title\u{7}\tname");
    assert_eq!(state.query, "界 folder name");
    state.insert(&"界".repeat(2000));
    assert!(state.query.len() <= 1024);
    assert!(!state.query.chars().any(char::is_control));
}

#[test]
fn refresh_discards_stale_pages_and_search_loads_beyond_recent_results() {
    let mut entries = list(
        (0..200)
            .map(|id| item(&id.to_string(), "Recent", "text"))
            .collect(),
    );
    entries.before = Some(42);
    let mut state = PickerState {
        page_size: 5,
        ..Default::default()
    };
    assert!(!entries.needs_page(&state, 200));
    state.selected = 196;
    assert!(entries.needs_page(&state, 200));
    state.insert("older project");
    assert!(entries.needs_page(&state, 0));
    let old = entries.generation;
    entries.reset();
    entries.accept(
        old,
        Ok(SessionPage {
            sessions: vec![item("stale", "Stale", "old").summary],
            next_before: None,
        }),
    );
    assert!(entries.items.is_empty());
    assert!(!entries.loaded);
    entries.accept(
        entries.generation,
        Err(anyhow::anyhow!("catalog unavailable")),
    );
    assert!(!entries.needs_page(&state, 0));
    entries.reset();
    assert!(entries.needs_page(&state, 0));
}

#[test]
fn unavailable_workspaces_keep_selection_in_picker_without_changing_cwd() -> Result<()> {
    let fixture = TestDirectory::new("picker-validation")?;
    let before = std::env::current_dir()?;
    let file = fixture.0.join("not-a-folder");
    std::fs::write(&file, "fixture")?;
    let mut state = PickerState::default();
    for path in [fixture.0.join("missing"), file] {
        let mut missing = item("missing", "Missing workspace", "old");
        missing.summary.workspace = path.to_string_lossy().into_owned();
        assert!(state.choose(&missing).is_none());
        assert!(state.error.as_ref().unwrap().contains("Cannot resume"));
    }
    let mut valid = item("valid", "Available workspace", "new");
    valid.summary.workspace = fixture.0.to_string_lossy().into_owned();
    assert_eq!(state.choose(&valid), Some(valid.summary.id));
    assert_eq!(std::env::current_dir()?, before);
    Ok(())
}

#[test]
fn responsive_picker_shows_tabs_cwd_preview_and_clear_selection() -> Result<()> {
    let lists = [
        list(vec![
            item("01234567-89", "First", "one"),
            item("abcdef01-23", "Second", "two"),
        ]),
        SessionList::default(),
    ];
    for (width, height) in [(80, 24), (100, 30), (120, 40)] {
        let mut state = PickerState::default();
        let buffer = draw(&lists, &mut state, width, height)?;
        let text = screen(&buffer);
        for expected in [
            "Resume session",
            "Local 2",
            "All",
            "First",
            "Second",
            "/fixture",
            "3 messages",
            "copilot:fixture",
            "you: one",
            "Tab Local/All",
        ] {
            assert!(text.contains(expected), "missing {expected}:\n{text}");
        }
        assert!(text.contains(if width >= 112 {
            "Conversation"
        } else {
            "Preview"
        }));
        let marker = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "›")
            .unwrap();
        assert_eq!(marker.fg, Palette::ACCENT());
        let selected = buffer
            .content
            .iter()
            .find(|cell| cell.symbol() == "F" && cell.bg == Palette::SELECTED_BG())
            .unwrap();
        assert!(selected.modifier.contains(Modifier::BOLD));
        assert_eq!(state.scroll, 0);
    }
    for (width, height) in [(1, 1), (20, 4), (40, 8)] {
        let mut state = PickerState {
            query: "none".into(),
            scroll: 100,
            ..Default::default()
        };
        draw(&lists, &mut state, width, height)?;
    }
    Ok(())
}

#[test]
fn resizing_keeps_selected_two_line_row_visible_and_sanitizes_display_only() -> Result<()> {
    let mut entries: Vec<_> = (0..20)
        .map(|index| item(&index.to_string(), &format!("Session {index}"), "preview"))
        .collect();
    entries[19].summary.title = "界\u{1b}[31m最後の session\nend".into();
    entries[19].summary.workspace = format!("/very/{}/project-界", "long-path/".repeat(20));
    let original = entries[19].summary.title.clone();
    let lists = [list(entries), SessionList::default()];
    let mut state = PickerState {
        selected: 19,
        ..Default::default()
    };
    for (width, height) in [(120, 40), (60, 16), (40, 8), (80, 24)] {
        let buffer = draw(&lists, &mut state, width, height)?;
        let text = screen(&buffer);
        assert!(text.contains('最') && text.contains('後'), "{text}");
        assert!(!text.contains('\u{1b}'));
        assert!(state.scroll <= state.selected && state.selected < state.scroll + state.page_size);
    }
    assert_eq!(lists[0].items[19].summary.title, original);
    Ok(())
}

#[test]
fn tiny_picker_does_not_open_invisible_rows_and_keeps_errors_visible() -> Result<()> {
    let lists = [
        list(vec![item("a", "First", "one")]),
        SessionList::default(),
    ];
    let mut state = PickerState::default();
    for (width, height) in [(1, 1), (20, 4), (40, 4), (23, 24)] {
        draw(&lists, &mut state, width, height)?;
        assert_eq!(state.page_size, 0);
        assert_eq!(state.key(press(KeyCode::Enter), &[0]), None);
        assert_eq!(state.key(press(KeyCode::Esc), &[0]), Some(Action::Cancel));
    }
    state.error = Some("Cannot resume: workspace unavailable.".into());
    let buffer = draw(&lists, &mut state, 40, 4)?;
    assert!(screen(&buffer).contains("Cannot resume: workspace unavailable."));
    draw(&lists, &mut state, 80, 24)?;
    assert_eq!(
        state.key(press(KeyCode::Enter), &[0]),
        Some(Action::Open(0))
    );
    Ok(())
}

#[test]
fn scope_waiting_for_other_loader_does_not_report_false_empty_results() -> Result<()> {
    let lists = [
        list(vec![item("a", "First", "one")]),
        SessionList::default(),
    ];
    for query in ["", "older"] {
        let mut state = PickerState {
            scope: Scope::All,
            query: query.into(),
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| {
            render::draw_picker(
                frame,
                &lists,
                &[],
                &mut state,
                Path::new("/fixture"),
                Some(Scope::Local),
            )
        })?;
        let text = screen(terminal.backend().buffer());
        assert!(text.contains("Looking for sessions"), "{text}");
        assert!(!text.contains("No matching sessions") && !text.contains("No saved sessions"));
    }
    Ok(())
}

#[tokio::test]
async fn missing_database_picker_is_offline_and_does_not_create_storage() -> Result<()> {
    let fixture = TestDirectory::new("missing-picker")?;
    let database = fixture.0.join("missing.db");
    assert!(matches!(
        pick_session(&database, &fixture.0, false).await?,
        SessionSelection::New
    ));
    assert!(!database.exists());
    Ok(())
}

struct CatalogFixture;

#[tokio::test]
#[ignore = "private offline fixture for the resume picker PTY harness"]
async fn seed_resume_picker_pty_catalog() -> Result<()> {
    use bob_kernel::{Command, CommandId, Policy, Profile, Runtime};
    let directory = PathBuf::from(
        std::env::var_os("BOB_PICKER_FIXTURE_DIR").context("PTY fixture directory required")?,
    )
    .canonicalize()?;
    anyhow::ensure!(
        std::fs::read_dir(&directory)?.next().is_none(),
        "fixture directory must be empty"
    );
    let local = directory.join("local project");
    let database = local.join("sessions.sqlite");
    let global = directory.join("global project 界");
    let missing = directory.join("missing project");
    for path in [&local, &global, &missing] {
        std::fs::create_dir(path)?;
    }
    let handle = Runtime::open_paused(&database, std::sync::Arc::new(CatalogFixture)).await?;
    let result: Result<()> = async {
        for (workspace, title) in [
            (&local, "Local build notes"),
            (&global, "Global workspace notes"),
            (&missing, "Unavailable workspace notes"),
        ] {
            let receipt = handle
                .command(Command::CreateSession {
                    id: CommandId::new(),
                    workspace: workspace.to_string_lossy().into_owned(),
                    profile: Profile {
                        model: "copilot:fixture".into(),
                        instructions: String::new(),
                        read_only: false,
                        max_requests: 1,
                        reasoning: bob_types::ReasoningEffort::Off,
                    },
                    policy: Policy::default(),
                })
                .await?;
            handle
                .command(Command::Submit {
                    id: CommandId::new(),
                    thread: receipt.thread,
                    text: title.into(),
                })
                .await?;
        }
        Ok(())
    }
    .await;
    handle.shutdown().await?;
    result?;
    std::fs::rename(&missing, directory.join("moved project"))?;
    std::fs::write(
        directory.join("fixture.json"),
        serde_json::to_vec(
            &serde_json::json!({"database":database,"local":local,"global":global,"missing":missing}),
        )?,
    )?;
    Ok(())
}

#[async_trait::async_trait]
impl bob_kernel::Executor for CatalogFixture {
    fn models(&self) -> Vec<bob_kernel::ModelInfo> {
        vec![bob_kernel::ModelInfo {
            id: "copilot:fixture".into(),
            context_window: 32_768,
        }]
    }
    fn tools(&self) -> Vec<bob_kernel::ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        _: bob_kernel::RequestSpec,
        _: tokio::sync::watch::Receiver<bool>,
        _: bob_kernel::ModelProgress,
    ) -> Result<bob_types::Completion> {
        anyhow::bail!("catalog fixture cannot call a provider")
    }
    async fn prepare(&self, _: bob_kernel::Invocation) -> Result<bob_kernel::PreparedTool> {
        anyhow::bail!("catalog fixture cannot prepare tools")
    }
    async fn execute(
        &self,
        _: bob_kernel::PreparedTool,
        _: tokio::sync::watch::Receiver<bool>,
    ) -> Result<bob_kernel::ToolOutcome> {
        anyhow::bail!("catalog fixture cannot execute tools")
    }
}

#[tokio::test]
async fn catalog_pages_local_and_all_without_implicit_fallback_or_writes() -> Result<()> {
    use bob_kernel::{Command, CommandId, Policy, Profile, Runtime};
    use std::sync::Arc;
    let fixture = TestDirectory::new("catalog-picker")?;
    let workspace = fixture.0.join("project");
    let other = fixture.0.join("other");
    std::fs::create_dir(&workspace)?;
    std::fs::create_dir(&other)?;
    let database = fixture.0.join("lab.db");
    let handle = Runtime::open(&database, Arc::new(CatalogFixture)).await?;
    let result: Result<_> = async {
        let mut newest = None;
        for index in 0..202 {
            let workspace = if index == 0 { &other } else { &workspace };
            let receipt = handle
                .command(Command::CreateSession {
                    id: CommandId::new(),
                    workspace: workspace.to_string_lossy().into_owned(),
                    profile: Profile {
                        model: "copilot:fixture".into(),
                        instructions: String::new(),
                        read_only: false,
                        max_requests: 16,
                        reasoning: bob_types::ReasoningEffort::Off,
                    },
                    policy: Policy::default(),
                })
                .await?;
            newest = Some(receipt.session);
        }
        Ok(newest.unwrap())
    }
    .await;
    handle.shutdown().await?;
    let newest = result?;
    let before = std::fs::read(&database)?;
    let filter = Some(workspace.to_string_lossy().into_owned());
    let first = read_page(database.clone(), filter.clone(), None, 200).await?;
    assert_eq!(first.sessions.len(), 200);
    assert_eq!(first.sessions[0].id, newest);
    let second = read_page(database.clone(), filter, first.next_before, 200).await?;
    assert_eq!(second.sessions.len(), 1);
    assert!(second.next_before.is_none());
    assert!(first
        .sessions
        .iter()
        .chain(&second.sessions)
        .all(|s| s.workspace == workspace.to_string_lossy()));
    let first = read_page(database.clone(), None, None, 200).await?;
    let second = read_page(database.clone(), None, first.next_before, 200).await?;
    assert_eq!(first.sessions.len() + second.sessions.len(), 202);
    assert_eq!(first.sessions[0].id, newest);
    assert!(second
        .sessions
        .iter()
        .any(|session| session.workspace == other.to_string_lossy()));
    let local = read_page(
        database.clone(),
        Some(fixture.0.join("unseen").to_string_lossy().into_owned()),
        None,
        200,
    )
    .await?;
    assert!(local.sessions.is_empty());
    assert!(local.next_before.is_none());
    assert_eq!(std::fs::read(&database)?, before);
    Ok(())
}
