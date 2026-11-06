use super::*;
use bob_ui::theme::Theme;

fn key(app: &mut App, code: KeyCode, jobs: &mut JoinSet<Reply>) {
    assert!(!app.key(KeyEvent::new(code, KeyModifiers::NONE), jobs));
}

async fn ready(workspace: &Workspace) -> (App, Handle, ThreadId) {
    let (handle, thread, _) = start(workspace, false, true).await;
    let view = handle
        .transcript(Some(thread.clone()), None, 80)
        .await
        .unwrap();
    let mut app = App::new(80);
    app.defaults = commands::DefaultsWriter::at(workspace.0.join("global.toml"));
    app.configure(workspace.0.clone(), "fixture".into());
    app.reply(Reply::Ready(Ok((handle.clone(), view, thread.clone()))));
    (app, handle, thread)
}

async fn open(app: &mut App, command: &str, jobs: &mut JoinSet<Reply>) {
    assert!(app.draft_mut().text.is_empty());
    app.draft_mut().insert(command);
    assert!(!app.submit(jobs));
    collect(app, jobs).await;
    assert!(app.picker.is_some());
    assert!(app.draft_mut().text.is_empty());
}

struct ResetPalette;
impl Drop for ResetPalette {
    fn drop(&mut self) {
        bob_ui::theme::set_theme(Theme::dark());
    }
}

#[test]
fn theme_picker_selects_current_theme_and_uses_production_title() {
    for (index, name) in Theme::NAMES.iter().enumerate() {
        let picker = commands::Picker::theme(name);
        assert_eq!(picker.selected, index);
        assert_eq!(picker.title, "Theme (↑↓ to preview, Enter to keep)");
        assert_eq!(picker.command(), Some(format!("/theme {name}")));
        assert_eq!(picker.items.len(), Theme::NAMES.len());
    }
    assert_eq!(commands::Picker::theme("unknown").selected, 0);
}

#[tokio::test]
async fn reasoning_picker_selects_current_effort_and_uses_production_title() {
    let workspace = Workspace::new();
    let (mut app, handle, thread) = ready(&workspace).await;
    let summary = app
        .threads
        .iter()
        .find(|item| item.id == thread)
        .unwrap()
        .clone();
    for (index, effort) in [
        ReasoningEffort::Off,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::Max,
    ]
    .into_iter()
    .enumerate()
    {
        commands::execute_with_defaults(
            commands::Request::Reasoning(Some(effort)),
            &handle,
            &summary,
            None,
            None,
            None,
            &commands::DefaultsWriter::at(workspace.0.join("global.toml")),
        )
        .await
        .unwrap();
        let mut jobs = JoinSet::new();
        open(&mut app, "/reasoning", &mut jobs).await;
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.selected, index);
        assert_eq!(
            picker.title,
            format!("Reasoning effort (current: {})", effort.label())
        );
        assert_eq!(
            picker.command(),
            Some(format!("/reasoning {}", effort.label()))
        );
        key(&mut app, KeyCode::Esc, &mut jobs);
        assert!(app.picker.is_none());
        assert_eq!(
            handle
                .context(thread.clone())
                .await
                .unwrap()
                .profile
                .reasoning,
            effort
        );
    }
    handle.shutdown().await.unwrap();
}

#[tokio::test]
async fn reasoning_picker_navigation_and_numeric_confirmation_match_production() {
    let workspace = Workspace::new();
    let (mut app, handle, thread) = ready(&workspace).await;
    let mut jobs = JoinSet::new();
    open(&mut app, "/reasoning", &mut jobs).await;
    key(&mut app, KeyCode::Up, &mut jobs);
    assert_eq!(app.picker.as_ref().unwrap().selected, 0);
    key(&mut app, KeyCode::Char('j'), &mut jobs);
    assert_eq!(app.picker.as_ref().unwrap().selected, 1);
    key(&mut app, KeyCode::Char('k'), &mut jobs);
    assert_eq!(app.picker.as_ref().unwrap().selected, 0);
    for _ in 0..8 {
        key(&mut app, KeyCode::Down, &mut jobs);
    }
    assert_eq!(app.picker.as_ref().unwrap().selected, 4);
    key(&mut app, KeyCode::Char('9'), &mut jobs);
    assert!(app.picker.is_some());
    key(&mut app, KeyCode::Char('4'), &mut jobs);
    assert!(app.picker.is_none());
    collect(&mut app, &mut jobs).await;
    assert_eq!(
        handle
            .context(thread.clone())
            .await
            .unwrap()
            .profile
            .reasoning,
        ReasoningEffort::High
    );
    open(&mut app, "/reasoning", &mut jobs).await;
    assert_eq!(app.picker.as_ref().unwrap().selected, 3);
    app.draft_mut().insert("new draft");
    key(&mut app, KeyCode::Enter, &mut jobs);
    assert!(app.picker.is_none());
    assert_eq!(app.draft_mut().text, "new draft");
    assert!(jobs.is_empty());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "mutates global production palette; run this test alone with --ignored --test-threads=1"]
async fn theme_preview_is_ui_local_and_escape_restores_committed_theme() {
    let _reset = ResetPalette;
    let workspace = Workspace::new();
    let config = workspace.0.join(".bob.config.toml");
    let original = "model = \"fixture\"\ntheme = \"light\"\n";
    std::fs::write(&config, original).unwrap();
    let (mut app, handle, _) = ready(&workspace).await;
    app.theme_name = "light".into();
    bob_ui::theme::set_theme(Theme::light());
    let mut jobs = JoinSet::new();
    open(&mut app, "/theme", &mut jobs).await;
    assert_eq!(
        app.picker.as_ref().unwrap().command().as_deref(),
        Some("/theme light")
    );
    let (_, text) = render(&mut app);
    assert!(text.contains("Theme (↑↓ to preview, Enter to keep)"));
    key(&mut app, KeyCode::Up, &mut jobs);
    assert_eq!(Palette::BG(), Theme::dark().bg);
    assert_eq!(app.theme_name, "light");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
    assert!(app.pane().unwrap().rendered.is_none());
    key(&mut app, KeyCode::Esc, &mut jobs);
    assert_eq!(Palette::BG(), Theme::light().bg);
    assert_eq!(app.theme_name, "light");
    assert!(app.picker.is_none());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);
    assert!(jobs.is_empty());
    handle.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "mutates global production palette; run this test alone with --ignored --test-threads=1"]
async fn theme_confirmation_persists_and_failed_save_rolls_back_preview() {
    let _reset = ResetPalette;
    bob_ui::theme::set_theme(Theme::dark());
    let workspace = Workspace::new();
    let config = workspace.0.join(".bob.config.toml");
    let (mut app, handle, _) = ready(&workspace).await;
    let mut jobs = JoinSet::new();
    open(&mut app, "/theme", &mut jobs).await;
    key(&mut app, KeyCode::Down, &mut jobs);
    assert_eq!(Palette::BG(), Theme::light().bg);
    assert!(!config.exists());
    key(&mut app, KeyCode::Enter, &mut jobs);
    assert_eq!(Palette::BG(), Theme::light().bg);
    assert_eq!(app.theme_name, "dark");
    collect(&mut app, &mut jobs).await;
    assert_eq!(app.theme_name, "light");
    assert_eq!(Palette::BG(), Theme::light().bg);
    assert!(std::fs::read_to_string(&config)
        .unwrap()
        .contains("theme = \"light\""));

    open(&mut app, "/theme", &mut jobs).await;
    assert_eq!(
        app.picker.as_ref().unwrap().command().as_deref(),
        Some("/theme light")
    );
    key(&mut app, KeyCode::Up, &mut jobs);
    assert_eq!(Palette::BG(), Theme::dark().bg);
    std::fs::write(&config, "invalid = [").unwrap();
    key(&mut app, KeyCode::Enter, &mut jobs);
    app.draft_mut().insert(" edited");
    collect(&mut app, &mut jobs).await;
    assert_eq!(Palette::BG(), Theme::light().bg);
    assert_eq!(app.theme_name, "light");
    assert_eq!(app.draft_mut().text, "/theme dark edited");
    assert!(app.pane().unwrap().notice.contains("Command failed:"));
    assert!(app.pane().unwrap().pending.is_none());
    assert!(app.pane().unwrap().receipt.is_none());
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "invalid = [");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "mutates global production palette; run this test alone with --ignored --test-threads=1"]
async fn theme_dismissal_and_thread_replacement_restore_preview_and_preserve_drafts() {
    let _reset = ResetPalette;
    bob_ui::theme::set_theme(Theme::dark());
    let workspace = Workspace::new();
    let (mut app, handle, thread) = ready(&workspace).await;
    let mut jobs = JoinSet::new();
    open(&mut app, "/theme", &mut jobs).await;
    key(&mut app, KeyCode::Down, &mut jobs);
    app.dismiss_picker();
    assert_eq!(Palette::BG(), Theme::dark().bg);
    assert!(app.picker.is_none());

    open(&mut app, "/theme", &mut jobs).await;
    key(&mut app, KeyCode::Down, &mut jobs);
    let other = ThreadId::new();
    app.select(other.clone());
    app.draft_mut().insert("other thread draft");
    assert_eq!(Palette::BG(), Theme::dark().bg);
    assert!(app.picker.is_none());
    app.select(thread.clone());
    open(&mut app, "/theme", &mut jobs).await;
    key(&mut app, KeyCode::Down, &mut jobs);
    key(&mut app, KeyCode::Enter, &mut jobs);
    app.select(other.clone());
    assert_eq!(Palette::BG(), Theme::dark().bg);
    app.select(thread);
    collect(&mut app, &mut jobs).await;
    assert_eq!(Palette::BG(), Theme::dark().bg);
    assert_eq!(app.theme_name, "dark");
    app.select(other);
    assert_eq!(app.draft_mut().text, "other thread draft");
    handle.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "mutates global production palette; run this test alone with --ignored --test-threads=1"]
async fn runtime_replacement_reverts_theme_preview_and_rejects_late_confirmation() {
    let _reset = ResetPalette;
    bob_ui::theme::set_theme(Theme::dark());
    let workspace = Workspace::new();
    let (mut app, handle, _) = ready(&workspace).await;
    let mut jobs = JoinSet::new();
    open(&mut app, "/theme", &mut jobs).await;
    key(&mut app, KeyCode::Down, &mut jobs);
    key(&mut app, KeyCode::Enter, &mut jobs);
    let mut replacement = app.live_snapshot.as_ref().unwrap().as_ref().clone();
    replacement.instance = "replacement-picker-runtime".into();
    app.accept_live(Arc::new(replacement));
    assert_eq!(Palette::BG(), Theme::dark().bg);
    assert!(app.picker.is_none());
    app.pane_mut().unwrap().notice = "new runtime notice".into();
    app.draft_mut().insert(" edited after replacement");
    collect(&mut app, &mut jobs).await;
    assert_eq!(Palette::BG(), Theme::dark().bg);
    assert_eq!(app.theme_name, "dark");
    assert_eq!(
        app.draft_mut().text,
        "/theme light edited after replacement"
    );
    assert_eq!(app.pane().unwrap().notice, "new runtime notice");
    handle.shutdown().await.unwrap();
}
