use super::*;
use bob_kernel::{ContextSnapshot, SessionId, SessionSnapshot};

fn thread(name: &str, session: &str, parent: Option<&str>) -> ThreadSummary {
    ThreadSummary {
        id: ThreadId(name.into()),
        session: SessionId(session.into()),
        parent_run: None,
        parent_thread: parent.map(|id| ThreadId(id.into())),
        needs_approval: false,
        activity: None,
        name: name.into(),
        model: "test".into(),
        state: ThreadState::Completed,
        run: None,
    }
}

fn app() -> App {
    let mut app = App::new(80);
    app.configure(PathBuf::from("/workspace/project"), "test".into());
    app.threads = vec![
        thread("root", "session", None),
        thread("child", "session", Some("root")),
    ];
    app.select(ThreadId("root".into()));
    app
}

fn metadata(app: &mut App, thread: &str, sequence: i64, title: &str) -> Reply {
    metadata_snapshot(app, thread, sequence, sequence, title)
}

fn metadata_snapshot(
    app: &mut App,
    thread: &str,
    sequence: i64,
    title_revision: i64,
    title: &str,
) -> Reply {
    let thread = ThreadId(thread.into());
    let summary = app
        .threads
        .iter()
        .find(|summary| summary.id == thread)
        .unwrap();
    let session = summary.session.clone();
    let profile = Profile {
        model: "test".into(),
        instructions: String::new(),
        read_only: false,
        max_requests: 32,
        reasoning: ReasoningEffort::Off,
    };
    let key = (app.runtime_epoch, sequence);
    let pane = app.panes.entry(thread.clone()).or_default();
    pane.metadata_epoch += 1;
    pane.metadata_key = Some(key);
    pane.metadata_pending = true;
    Reply::Metadata {
        thread: thread.clone(),
        key,
        epoch: pane.metadata_epoch,
        result: Ok(Metadata {
            context: ContextSnapshot {
                profile: profile.clone(),
                thread: thread.clone(),
                epoch: 0,
                revision: 0,
                profile_revision: 0,
                message_count: 0,
                checkpoint: None,
                compact_pending: false,
                budget: Default::default(),
            },
            controls: Default::default(),
            session: SessionSnapshot {
                id: session,
                title: title.into(),
                title_revision,
                thread: ThreadId("root".into()),
                workspace: "/workspace/project".into(),
                profile,
                profile_revision: 0,
                context_epoch: 0,
                policy: Default::default(),
                permissions: Default::default(),
                policy_revision: 0,
            },
            details: Vec::new(),
            jobs: Vec::new(),
            queue: bob_kernel::QueueSnapshot {
                sequence,
                pending: Vec::new(),
                submissions: Vec::new(),
            },
        }),
    }
}

#[test]
fn workspace_then_accepted_session_title_is_stable_across_child_navigation() {
    let mut app = app();
    assert_eq!(app.terminal_title(), "Bob · project");
    let reply = metadata(&mut app, "root", 1, "Fix\nparser\x07\x1b");
    app.reply(reply);
    assert_eq!(app.terminal_title(), "Bob · Fix parser");
    app.select(ThreadId("child".into()));
    assert!(app.pane().unwrap().session.is_none());
    assert_eq!(app.terminal_title(), "Bob · Fix parser");
    app.status = "Working".into();
    app.pane_mut()
        .unwrap()
        .draft
        .insert("A different unsent topic");
    app.panes.remove(&ThreadId("root".into()));
    assert_eq!(app.terminal_title(), "Bob · Fix parser");
    let reply = metadata(&mut app, "child", 2, "Improve parser error handling");
    app.reply(reply);
    app.select(ThreadId("root".into()));
    assert_eq!(app.terminal_title(), "Bob · Improve parser error handling");
}

#[test]
fn stale_metadata_never_changes_the_cached_title() {
    for invalidation in 0..4 {
        let mut app = app();
        let reply = metadata(&mut app, "root", 10, "Accepted title");
        app.reply(reply);
        let reply = metadata(&mut app, "root", 11, "Stale title");
        match invalidation {
            0 => app.pane_mut().unwrap().invalidate_metadata(),
            1 => app.sequence_floor = 12,
            2 => app.pane_mut().unwrap().metadata_key = Some((0, 12)),
            _ => {
                app.runtime_epoch += 1;
                let current = metadata(&mut app, "child", 1, "Accepted title");
                app.reply(current);
            }
        }
        app.reply(reply);
        assert_eq!(app.terminal_title(), "Bob · Accepted title");
    }
}

#[test]
fn older_root_metadata_cannot_overwrite_a_newer_child_snapshot() {
    let mut app = app();
    let old = metadata(&mut app, "root", 10, "Old fallback");
    let new = metadata(&mut app, "child", 11, "Generated session name");
    app.reply(new);
    app.reply(old);
    assert_eq!(app.terminal_title(), "Bob · Generated session name");
}

#[test]
fn actual_snapshot_revision_orders_titles_with_identical_query_start_keys() {
    for old_first in [false, true] {
        let mut app = app();
        let old = metadata_snapshot(&mut app, "root", 10, 11, "Old fallback");
        let new = metadata_snapshot(&mut app, "child", 10, 12, "Generated session name");
        if old_first {
            app.reply(old);
            app.reply(new);
        } else {
            app.reply(new);
            app.reply(old);
        }
        assert_eq!(app.terminal_title(), "Bob · Generated session name");
    }
    let mut app = app();
    let old = metadata_snapshot(&mut app, "root", 12, 12, "Old fallback");
    let new = metadata_snapshot(&mut app, "child", 10, 13, "Generated session name");
    app.reply(old);
    app.reply(new);
    assert_eq!(app.terminal_title(), "Bob · Generated session name");
}

#[test]
fn another_sessions_metadata_does_not_change_the_selected_session_title() {
    let mut app = app();
    app.threads.push(thread("other", "other-session", None));
    let reply = metadata(&mut app, "root", 1, "Original session");
    app.reply(reply);
    let reply = metadata(&mut app, "other", 2, "Other session");
    app.reply(reply);
    assert_eq!(app.terminal_title(), "Bob · Original session");
    app.select(ThreadId("other".into()));
    assert_eq!(app.terminal_title(), "Bob · Other session");
    app.select(ThreadId("child".into()));
    assert_eq!(app.terminal_title(), "Bob · Original session");
}

#[test]
fn new_session_receipt_resets_title_before_thread_metadata_arrives() {
    let mut app = app();
    let reply = metadata(&mut app, "root", 1, "Old session");
    app.reply(reply);
    app.pane_mut().unwrap().pending = Some(1);
    app.reply(Reply::Command {
        thread: ThreadId("root".into()),
        submitted: "/new".into(),
        revision: 0,
        focus_epoch: app.focus_epoch,
        epoch: 1,
        inspection_epoch: 0,
        result: Ok(Outcome::Session(Receipt {
            session: SessionId("new-session".into()),
            thread: ThreadId("new-root".into()),
            sequence: 2,
            detail: "created".into(),
        })),
    });
    assert_eq!(app.selected, Some(ThreadId("new-root".into())));
    assert_eq!(app.terminal_title(), "Bob · project");
    let reply = metadata(&mut app, "root", 3, "Late old session title");
    app.reply(reply);
    assert_eq!(app.terminal_title(), "Bob · project");
}

#[test]
fn empty_titles_and_runtime_changes_use_the_workspace_fallback() {
    let mut app = app();
    let reply = metadata(&mut app, "root", 1, " \x07\n ");
    app.reply(reply);
    assert_eq!(app.terminal_title(), "Bob · project");
    let reply = metadata(&mut app, "root", 2, "Saved title");
    app.reply(reply);
    app.runtime_epoch += 1;
    assert_eq!(app.terminal_title(), "Bob · project");
    let reply = metadata(&mut app, "root", 1, "New runtime title");
    app.reply(reply);
    assert_eq!(app.terminal_title(), "Bob · New runtime title");
}
