use super::*;
use bob_kernel::{Entry, LiveAttempt, LiveSnapshot, ModelActivity, RunId, SessionId};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use futures::FutureExt;
use ratatui::backend::TestBackend;
use std::{sync::Arc, time::Instant};

#[path = "working_parity_tests.rs"]
mod working_parity_tests;

fn thread(name: &str) -> ThreadSummary {
    ThreadSummary {
        id: ThreadId(name.into()),
        session: SessionId("session".into()),
        parent_run: None,
        parent_thread: None,
        needs_approval: false,
        activity: Some(ThreadActivity::Model),
        name: name.into(),
        model: "fixture".into(),
        state: ThreadState::Active,
        run: Some(RunId(format!("run-{name}"))),
    }
}
fn entry(sequence: i64, text: &str, operation: Option<&str>) -> Entry {
    Entry {
        sequence,
        sender: None,
        parent_guidance: false,
        submission: None,
        thread: ThreadId("root".into()),
        kind: EntryKind::Assistant,
        text: text.into(),
        artifact: None,
        operation: operation.map(|id| OperationId(id.into())),
        tool: None,
        is_preview: false,
        visible_text_bytes: None,
        thinking: None,
    }
}
fn view(sequence: i64, entries: Vec<Entry>) -> View {
    View {
        sequence,
        entry_sequence: entries
            .iter()
            .map(|entry| entry.sequence)
            .max()
            .unwrap_or(0),
        threads: vec![thread("root")],
        policies: vec![],
        entries,
        decisions: vec![],
        operations: vec![],
        error: None,
    }
}
fn app(entries: Vec<Entry>) -> App {
    let mut app = App::new(80);
    app.select(ThreadId("root".into()));
    app.threads = vec![thread("root")];
    app.pane_mut().unwrap().view = Some(view(1, entries));
    app
}
fn attempt(thread: &str, operation: &str, text: &str, revision: u64) -> Arc<LiveAttempt> {
    Arc::new(LiveAttempt {
        thread: ThreadId(thread.into()),
        run: RunId(format!("run-{thread}")),
        operation: OperationId(operation.into()),
        revision,
        activity: ModelActivity::Answering,
        text: text.into(),
        reasoning: vec![],
        truncated: false,
        started_at: Instant::now(),
        compacting: false,
        stopping: false,
        settled_sequence: None,
        error: None,
    })
}
fn snapshot(revision: u64, attempts: Vec<Arc<LiveAttempt>>) -> Arc<LiveSnapshot> {
    Arc::new(LiveSnapshot {
        instance: "runtime-one".into(),
        revision,
        sequence: 1,
        error: None,
        attempts,
    })
}
fn render(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}
fn render_prepared(app: &mut App, width: u16, height: u16) -> String {
    for _ in 0..1024 {
        let screen = render(app, width, height);
        if app
            .pane()
            .is_some_and(|pane| pane.rendered.is_some() && !pane.live_segments.pending())
        {
            return screen;
        }
    }
    panic!("bounded live layout did not converge")
}

fn final_view(app: &mut App) {
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(
            2,
            vec![entry(2, "Authoritative replacement", Some("model-op"))],
        )),
    });
}

#[test]
fn partial_text_is_visible_until_authoritative_handoff_in_either_order() {
    for durable_first in [false, true] {
        let mut app = app(vec![]);
        app.accept_live(snapshot(
            1,
            vec![attempt("root", "model-op", "Partial delta", 1)],
        ));
        assert!(render(&mut app, 90, 24).contains("Partial delta"));
        let mut settled = (*attempt("root", "model-op", "Partial delta", 2)).clone();
        settled.settled_sequence = Some(2);
        if durable_first {
            final_view(&mut app);
        }
        app.accept_live(snapshot(2, vec![Arc::new(settled)]));
        if !durable_first {
            assert!(render(&mut app, 90, 24).contains("Partial delta"));
            final_view(&mut app);
        }
        let screen = render(&mut app, 90, 24);
        assert_eq!(screen.matches("Authoritative replacement").count(), 1);
        assert!(!screen.contains("Partial delta"));
    }
}

#[test]
fn live_redraw_keeps_history_cache_reading_anchor_drafts_and_permission_focus() {
    let mut app = app((1..=35)
        .map(|i| {
            entry(
                i,
                &format!("Historical message {i} with **styled** prose"),
                None,
            )
        })
        .collect());
    app.draft_mut().insert("unsent 界 draft");
    app.focus = Focus::PermissionInvalidated;
    app.accept_live(snapshot(
        1,
        vec![
            attempt("root", "model-op", "Live first", 1),
            attempt("child", "child-op", "Private child progress", 1),
        ],
    ));
    let screen = render(&mut app, 90, 24);
    assert!(screen.contains("Live first"));
    assert!(!screen.contains("Private child progress"));
    app.pane_mut().unwrap().scroll(-100);
    let anchor = app.pane().unwrap().viewport.anchor.clone();
    let builds = app.pane().unwrap().render_builds;
    for revision in 2..12 {
        app.accept_live(snapshot(
            revision,
            vec![attempt(
                "root",
                "model-op",
                &format!("Live revision {revision}"),
                revision,
            )],
        ));
        render(&mut app, 90, 24);
        assert_eq!(app.pane().unwrap().render_builds, builds);
        assert_eq!(app.pane().unwrap().viewport.anchor, anchor);
        assert_eq!(app.focus, Focus::PermissionInvalidated);
        assert_eq!(app.draft_mut().text, "unsent 界 draft");
    }
    let root_snapshot = app.live_snapshot.clone().unwrap();
    app.select(ThreadId("child".into()));
    app.draft_mut().insert("child draft");
    app.accept_live(snapshot(
        12,
        vec![
            attempt("root", "model-op", "Private root progress", 12),
            attempt("child", "child-op", "Child answer", 2),
        ],
    ));
    let screen = render(&mut app, 90, 24);
    assert!(screen.contains("Child answer"));
    assert!(!screen.contains("Private root progress"));
    app.select(ThreadId("root".into()));
    assert_eq!(app.draft_mut().text, "unsent 界 draft");
    app.accept_live(root_snapshot);
    assert_eq!(app.live_snapshot.as_ref().unwrap().revision, 12);
}

#[tokio::test]
async fn watch_reconnect_gets_current_snapshot_and_new_instance_invalidates_old_projection() {
    let latest = snapshot(4, vec![attempt("root", "model-op", "Already streaming", 4)]);
    let (sender, _) = tokio::sync::watch::channel(latest.clone());
    let mut receiver = sender.subscribe();
    let mut app = app(vec![]);
    app.accept_live(receiver.borrow_and_update().clone());
    app.draft_mut().insert("survives reconnect");
    assert!(render(&mut app, 90, 24).contains("Already streaming"));
    app.focus = Focus::Permission;
    let mut restarted = (*latest).clone();
    restarted.instance = "runtime-two".into();
    restarted.revision = 0;
    restarted.attempts.clear();
    sender.send_replace(Arc::new(restarted));
    let mut receiver = Some(receiver);
    let snapshot = tokio::time::timeout(Duration::from_secs(2), live::changed(&mut receiver))
        .await
        .unwrap()
        .unwrap();
    app.accept_live(snapshot);
    assert_eq!(app.focus, Focus::PermissionInvalidated);
    assert_eq!(app.draft_mut().text, "survives reconnect");
    assert!(!render(&mut app, 90, 24).contains("Already streaming"));
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(999, vec![entry(999, "Stale old runtime reply", None)])),
    });
    assert!(app.pane().unwrap().view.is_none());
}

#[test]
fn working_indicator_preserves_reasoning_approval_and_stopping_state() {
    let mut selected = thread("root");
    let start = Some(Instant::now());
    let label = |thread: &ThreadSummary, attempt: Option<&LiveAttempt>| {
        activity::line(
            thread,
            attempt,
            start,
            Duration::from_secs(2),
            "esc to interrupt",
        )
        .unwrap()
        .to_string()
    };
    assert!(label(&selected, None).contains("Working"));
    selected.activity = Some(ThreadActivity::Tool {
        name: "grep".into(),
    });
    assert!(label(&selected, None).contains("Working (0s · esc to interrupt)"));
    selected.activity = Some(ThreadActivity::Agent);
    assert!(label(&selected, None).contains("Working (0s · esc to interrupt)"));
    selected.activity = Some(ThreadActivity::Approval);
    assert!(label(&selected, None).contains("Approval needed"));
    selected.state = ThreadState::Stopping;
    assert!(label(&selected, None).contains("Stopping"));
    let mut app = app(vec![]);
    let mut draft = (*attempt("root", "model-op", "Visible answer", 1)).clone();
    draft.reasoning.push(bob_kernel::ReasoningPreview {
        block_id: "public-summary".into(),
        kind: bob_types::ReasoningKind::Summary,
        text: "Actual provider summary".into(),
    });
    app.accept_live(snapshot(1, vec![Arc::new(draft)]));
    let screen = render(&mut app, 90, 24);
    assert!(screen.contains("Thinking"));
    assert!(!screen.contains("Actual provider summary"));
    assert!(screen.contains("Visible answer"));
}

#[test]
fn clicking_live_thinking_expands_it_and_keeps_it_open_after_the_reply_is_saved() {
    let mut app = app(vec![]);
    let mut draft = (*attempt("root", "model-op", "Visible answer", 1)).clone();
    draft.reasoning.push(bob_kernel::ReasoningPreview {
        block_id: "thinking".into(),
        kind: bob_types::ReasoningKind::Thinking,
        text: "Weighing the first option".into(),
    });
    app.accept_live(snapshot(1, vec![Arc::new(draft)]));
    let screen = render(&mut app, 90, 24);
    assert!(!screen.contains("Weighing the first option"), "{screen}");
    let row = screen
        .lines()
        .position(|line| line.trim() == "Thinking")
        .expect("live thinking line") as u16;
    let mut jobs = JoinSet::new();
    app.mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row,
            modifiers: KeyModifiers::NONE,
        },
        &mut jobs,
    );
    let screen = render(&mut app, 90, 24);
    assert!(screen.contains("Weighing the first option"), "{screen}");
    assert!(screen.contains("Visible answer"), "{screen}");

    let mut reply = entry(2, "Visible answer", Some("model-op"));
    reply.thinking = Some("Weighing the first option".into());
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(2, vec![reply])),
    });
    let screen = render(&mut app, 90, 24);
    assert!(screen.contains("Weighing the first option"), "{screen}");
}

#[test]
fn concise_receipts_expire_without_hiding_failure_notices() {
    let mut app = app(vec![]);
    app.threads[0].state = ThreadState::Idle;
    app.pane_mut().unwrap().pending = Some(1);
    app.reply(Reply::Command {
        thread: ThreadId("root".into()),
        submitted: "request".into(),
        revision: 0,
        focus_epoch: 0,
        epoch: 1,
        inspection_epoch: 0,
        result: Ok(Outcome::Receipt(Receipt {
            thread: ThreadId("root".into()),
            session: SessionId("session".into()),
            sequence: 2,
            detail: "input durably queued".into(),
        })),
    });
    assert!(render(&mut app, 90, 24).contains("Sent"));
    app.pane_mut().unwrap().receipt.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(5);
    app.pane_mut()
        .unwrap()
        .notify_current("Command failed: useful evidence");
    let screen = render(&mut app, 90, 24);
    assert!(screen.contains("Command failed: useful evidence"));
    assert!(!screen.contains("Sent"));
}

#[test]
fn live_continuation_frames_reach_tail_without_another_snapshot() {
    let body = format!(
        "{}\nCONTINUATION_TAIL",
        "Streaming paragraph\n\n".repeat(200)
    );
    let mut app = app(vec![]);
    app.accept_live(snapshot(1, vec![attempt("root", "model-op", &body, 1)]));
    let first = render(&mut app, 90, 24);
    assert!(app.pane().unwrap().live_segments.pending());
    assert!(!first.contains("CONTINUATION_TAIL"));
    let complete = render_prepared(&mut app, 90, 24);
    assert!(complete.contains("CONTINUATION_TAIL"), "{complete}");
    assert!(!complete.contains("Preparing live text"));
    assert_eq!(app.live_snapshot.as_ref().unwrap().revision, 1);
}

#[test]
fn long_thinking_as_the_last_live_step_never_shows_paging_labels() {
    let mut app = app(vec![entry(1, "History", None)]);
    let attempts = (0..4)
        .map(|step| {
            let mut draft = (*attempt(
                "root",
                &format!("step-{step}"),
                &format!("Step {step} answer\n\n").repeat(40),
                1,
            ))
            .clone();
            draft.reasoning.push(bob_kernel::ReasoningPreview {
                block_id: "thinking".into(),
                kind: bob_types::ReasoningKind::Thinking,
                text: "Considering the next step. ".repeat(2000),
            });
            if step == 3 {
                draft.text.clear();
            }
            Arc::new(draft)
        })
        .collect();
    app.accept_live(snapshot(1, attempts));
    let bottom = render_prepared(&mut app, 90, 24);
    assert!(bottom.contains("Thinking"), "{bottom}");
    let mut reached_first_step = false;
    for _ in 0..60 {
        let screen = render(&mut app, 90, 24);
        assert!(
            !screen.contains("Earlier") && !screen.contains("Later"),
            "{screen}"
        );
        reached_first_step |= screen.contains("Step 0 answer");
        app.pane_mut().unwrap().scroll(-10);
    }
    assert!(reached_first_step);
}

#[test]
fn scrolled_live_answer_settles_in_place_in_both_arrival_orders() {
    let body = (0..100)
        .map(|i| format!("Answer paragraph {i}\n\n"))
        .collect::<String>();
    for durable_first in [false, true] {
        let mut app = app(vec![entry(1, "History", None)]);
        app.accept_live(snapshot(1, vec![attempt("root", "model-op", &body, 1)]));
        assert!(render_prepared(&mut app, 90, 24).contains("Answer paragraph 99"));
        app.pane_mut().unwrap().scroll(-40);
        let before = render_prepared(&mut app, 90, 24);
        let anchor = app.pane().unwrap().viewport.anchor.clone().unwrap();
        assert!(matches!(anchor.identity, AnchorId::Live { .. }));
        assert_eq!(app.pane().unwrap().query_before(), None);
        let commit = |app: &mut App| {
            app.reply(Reply::View {
                thread: ThreadId("root".into()),
                generation: 0,
                result: Ok(view(
                    2,
                    vec![entry(1, "History", None), entry(2, &body, Some("model-op"))],
                )),
            });
        };
        let mut settled = (*attempt("root", "model-op", &body, 2)).clone();
        settled.settled_sequence = Some(2);
        if durable_first {
            commit(&mut app);
        }
        app.accept_live(snapshot(2, vec![Arc::new(settled)]));
        if !durable_first {
            assert_eq!(
                render_prepared(&mut app, 90, 24).lines().next(),
                before.lines().next()
            );
            commit(&mut app);
        }
        let after = render_prepared(&mut app, 90, 24);
        assert_eq!(after.lines().next(), before.lines().next());
        let pane = app.pane().unwrap();
        let final_anchor = pane.viewport.anchor.as_ref().unwrap();
        assert_eq!(final_anchor.identity, AnchorId::Body(2));
        assert_eq!(
            (final_anchor.line, final_anchor.offset),
            (anchor.line, anchor.offset)
        );
        assert!(pane.live_rendered.as_ref().unwrap().rows.is_empty());
        assert_eq!(pane.viewport.rows[pane.viewport.top], *final_anchor);
    }
}

#[test]
fn pinned_reply_cannot_hide_a_live_answer_after_scrolling_into_it() {
    let body = "Live answer paragraph\n\n".repeat(80);
    let mut app = app(vec![entry(1, "History", None)]);
    app.accept_live(snapshot(1, vec![attempt("root", "op", &body, 1)]));
    render(&mut app, 90, 24);
    app.pane_mut().unwrap().scroll(-20);
    app.pane_mut().unwrap().loaded_before = Some(Some(2));
    let mut settled = (*attempt("root", "op", &body, 2)).clone();
    settled.settled_sequence = Some(2);
    app.accept_live(snapshot(2, vec![Arc::new(settled)]));
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(2, vec![entry(1, "History", None)])),
    });
    assert_eq!(app.pane().unwrap().view.as_ref().unwrap().sequence, 1);
    assert!(app.pane().unwrap().loaded_before.is_none());
    assert!(render(&mut app, 90, 24).contains("Live answer paragraph"));
}

#[test]
fn settled_live_answer_preserves_older_durable_page_anchors() {
    for older_page in [false, true] {
        let history: Vec<_> = (1..=35)
            .map(|i| entry(i, &format!("Historical message {i}"), None))
            .collect();
        let mut app = app(history.clone());
        if older_page {
            app.pane_mut().unwrap().before = Some(36);
        }
        app.accept_live(snapshot(
            1,
            vec![attempt("root", "op", "Partial answer", 1)],
        ));
        render(&mut app, 90, 24);
        app.pane_mut().unwrap().scroll(-25);
        let before = render(&mut app, 90, 24);
        let anchor = app.pane().unwrap().viewport.anchor.clone();
        assert_eq!(app.pane().unwrap().query_before(), Some(36));
        let mut settled = (*attempt("root", "op", "Final answer", 2)).clone();
        settled.settled_sequence = Some(100);
        app.accept_live(snapshot(2, vec![Arc::new(settled)]));
        app.reply(Reply::View {
            thread: ThreadId("root".into()),
            generation: 0,
            result: Ok(view(100, history)),
        });
        let after = render(&mut app, 90, 24);
        assert_eq!(app.pane().unwrap().viewport.anchor, anchor);
        assert_eq!(after.lines().next(), before.lines().next());
        assert_eq!(app.pane().unwrap().query_before(), Some(36));
    }
}

#[test]
fn live_anchor_does_not_alias_another_attempt_when_predecessor_disappears() {
    let body = "A long answer line\n\n".repeat(80);
    let mut app = app(vec![]);
    app.accept_live(snapshot(
        1,
        vec![
            attempt("root", "earlier", "earlier answer", 1),
            attempt("root", "reading", &body, 1),
        ],
    ));
    render(&mut app, 90, 24);
    app.pane_mut().unwrap().scroll(-20);
    let anchor = app.pane().unwrap().viewport.anchor.clone().unwrap();
    assert!(
        matches!(&anchor.identity, AnchorId::Live { operation, .. } if operation.0 == "reading")
    );
    app.accept_live(snapshot(2, vec![attempt("root", "reading", &body, 2)]));
    render(&mut app, 90, 24);
    let pane = app.pane().unwrap();
    assert_eq!(pane.viewport.rows[pane.viewport.top], anchor);
}

#[test]
fn durable_terminal_or_new_run_does_not_resurrect_stopping_activity() {
    for state in [
        ThreadState::Completed,
        ThreadState::Interrupted,
        ThreadState::Active,
    ] {
        let mut app = app(vec![]);
        let mut stopping = (*attempt("root", "old-op", "", 1)).clone();
        stopping.stopping = true;
        app.accept_live(snapshot(1, vec![Arc::new(stopping)]));
        assert!(render(&mut app, 90, 24).contains("Stopping"));
        let mut durable = view(2, vec![entry(2, "Final answer", Some("old-op"))]);
        durable.threads[0].state = state;
        if state == ThreadState::Active {
            durable.threads[0].run = Some(RunId("new-run".into()));
        }
        app.reply(Reply::View {
            thread: ThreadId("root".into()),
            generation: 0,
            result: Ok(durable),
        });
        let screen = render(&mut app, 90, 24);
        assert!(!screen.contains("Stopping"), "{screen}");
        if state == ThreadState::Active {
            assert!(screen.contains("Working"));
        }
    }
    let mut app = app(vec![]);
    let mut stopping = (*attempt("root", "committed-op", "", 1)).clone();
    stopping.stopping = true;
    app.accept_live(snapshot(1, vec![Arc::new(stopping)]));
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(2, vec![entry(2, "Tool phase", Some("committed-op"))])),
    });
    assert!(render(&mut app, 90, 24).contains("Working"));
}

#[test]
fn runtime_replacement_discards_inspections_actions_and_old_receipts_after_disconnect() {
    let mut app = app(vec![]);
    app.accept_live(snapshot(1, vec![]));
    app.draft_mut().insert("same draft");
    let revision = app.draft_mut().revision;
    app.sequence_floor = 500;
    let pane = app.pane_mut().unwrap();
    pane.pending = Some(1);
    pane.action_epoch = 1;
    pane.inspection_epoch = 7;
    pane.decided = Some(DecisionKey {
        thread: ThreadId("root".into()),
        operation: OperationId("old".into()),
        revision: 1,
    });
    pane.inspector = Some(Inspector {
        target: InspectionTarget::Entry(1),
        previous: Default::default(),
        title: "Old inspector".into(),
        text: "Old content".into(),
        offset: 0,
        next: None,
        total: 0,
        scroll: 0,
        pending: true,
    });
    app.focus = Focus::Inspector;
    app.disconnect_live();
    assert_eq!(app.live_instance.as_deref(), Some("runtime-one"));
    let mut replacement = (*snapshot(0, vec![])).clone();
    replacement.instance = "runtime-two".into();
    app.accept_live(Arc::new(replacement));
    assert_eq!(app.runtime_epoch, 1);
    assert_eq!(app.sequence_floor, 0);
    assert_eq!(app.focus, Focus::Composer);
    let pane = app.pane().unwrap();
    assert!(pane.inspector.is_none() && pane.pending.is_none() && pane.decided.is_none());
    assert!(pane.inspection_epoch > 7 && pane.action_epoch > 1);
    assert!(pane.rendered.is_none() && pane.live_rendered.is_none() && pane.permission.is_none());
    app.reply(Reply::Command {
        thread: ThreadId("root".into()),
        submitted: "same draft".into(),
        revision,
        focus_epoch: 0,
        epoch: 1,
        inspection_epoch: 7,
        result: Ok(Outcome::Receipt(Receipt {
            thread: ThreadId("root".into()),
            session: SessionId("session".into()),
            sequence: 999,
            detail: "old".into(),
        })),
    });
    app.reply(Reply::Text {
        thread: ThreadId("root".into()),
        epoch: 7,
        result: Ok(bob_kernel::TextPage {
            text: "Late old content".into(),
            next_offset: None,
            total_bytes: 16,
        }),
    });
    for reply in [
        Reply::Control {
            thread: ThreadId("root".into()),
            result: Ok(Receipt {
                thread: ThreadId("root".into()),
                session: SessionId("session".into()),
                sequence: 999,
                detail: "old".into(),
            }),
        },
        Reply::Decision {
            key: DecisionKey {
                thread: ThreadId("root".into()),
                operation: OperationId("old".into()),
                revision: 1,
            },
            result: Err("late error".into()),
        },
        Reply::View {
            thread: ThreadId("root".into()),
            generation: 0,
            result: Ok(view(999, vec![])),
        },
    ] {
        app.reading = true;
        app.reply(Reply::Runtime {
            epoch: 0,
            reply: Box::new(reply),
        });
        assert!(app.reading);
    }
    assert_eq!(app.draft_mut().text, "same draft");
    assert_eq!(app.sequence_floor, 0);
    assert!(app.pane().unwrap().inspector.is_none());
    assert!(app.pane().unwrap().notice.is_empty());
    assert!(!app.pane().unwrap().decision_refresh);
}

#[test]
fn unchanged_composer_is_cached_across_live_and_timer_frames() {
    let mut app = app(vec![entry(1, "Durable history", None)]);
    app.draft_mut().insert(&"x".repeat(composer::INPUT_LIMIT));
    for revision in 1..10 {
        app.accept_live(snapshot(
            revision,
            vec![attempt(
                "root",
                "op",
                &format!("Answer {revision}"),
                revision,
            )],
        ));
        render(&mut app, 100, 30);
        render(&mut app, 100, 30);
        assert_eq!(app.draft_mut().layout_builds, 1);
    }
}

#[test]
#[ignore = "manual cached composer/live frame measurement"]
fn cached_composer_live_frame_measurement() {
    let mut app = app((1..=80)
        .map(|i| entry(i, &format!("Historical message {i} **styled** prose"), None))
        .collect());
    app.draft_mut()
        .insert(&"draft 界 text\n".repeat(composer::INPUT_LIMIT / "draft 界 text\n".len()));
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let cold = Instant::now();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let cold = cold.elapsed();
    let start = Instant::now();
    for _ in 0..500 {
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    }
    let cached = start.elapsed();
    let start = Instant::now();
    for revision in 1..=200 {
        app.accept_live(snapshot(
            revision,
            vec![attempt(
                "root",
                "op",
                &"Streaming **answer**\n\n".repeat(100 + revision as usize),
                revision,
            )],
        ));
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    }
    let live = start.elapsed();
    assert_eq!(app.draft_mut().layout_builds, 1);
    eprintln!(
        "draft={} bytes; cold={cold:?}; cached={:?}/frame (500); live={:?}/frame (200)",
        app.draft_mut().text.len(),
        cached / 500,
        live / 200
    );
}

#[tokio::test]
async fn ready_timer_does_not_starve_input_commands_or_watch() {
    let mut events = futures::stream::repeat_with(|| {
        Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )))
    });
    let mut jobs = JoinSet::new();
    jobs.spawn(async { Reply::Ready(Err("fixture command".into())) });
    tokio::task::yield_now().await;
    let (sender, receiver) = tokio::sync::watch::channel(snapshot(0, vec![]));
    let mut receiver = Some(receiver);
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut observed = [false; 4];
    for revision in 1..=512 {
        sender.send_replace(snapshot(revision, vec![]));
        tick.reset_at(tokio::time::Instant::now() - Duration::from_secs(1));
        match next_event(&mut events, &mut jobs, &mut tick, &mut receiver).await {
            NextEvent::Input(_) => observed[0] = true,
            NextEvent::Reply(_) => observed[1] = true,
            NextEvent::Tick => observed[2] = true,
            NextEvent::Live(_) => observed[3] = true,
        }
        if observed.iter().all(|seen| *seen) {
            break;
        }
    }
    assert_eq!(observed, [true; 4]);
    tick.reset_at(tokio::time::Instant::now() - Duration::from_secs(1));
    tick.tick().await;
    assert!(
        tick.tick().now_or_never().is_none(),
        "missed ticks must not burst"
    );
}

struct StreamingExecutor {
    finish: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl bob_kernel::Executor for StreamingExecutor {
    fn models(&self) -> Vec<bob_kernel::ModelInfo> {
        vec![bob_kernel::ModelInfo {
            id: "fixture".into(),
            context_window: 100_000,
        }]
    }
    fn tools(&self) -> Vec<bob_kernel::ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        _request: bob_kernel::RequestSpec,
        mut cancel: tokio::sync::watch::Receiver<bool>,
        progress: bob_kernel::ModelProgress,
    ) -> Result<bob_types::Completion> {
        progress
            .emit(bob_kernel::ModelUpdate::Text(
                "Observable streaming delta".into(),
            ))
            .await?;
        tokio::select! {
            _ = self.finish.notified() => {},
            _ = bob_kernel::cancelled(&mut cancel) => bail!("cancelled fixture"),
        }
        Ok(bob_types::Completion {
            message: bob_types::Message {
                role: bob_types::Role::Assistant,
                content: vec![bob_types::ContentBlock::Text {
                    text: "Committed final answer".into(),
                }],
            },
            stop_reason: bob_types::StopReason::EndTurn,
            usage: bob_types::Usage::default(),
        })
    }
    async fn prepare(
        &self,
        _invocation: bob_kernel::Invocation,
    ) -> Result<bob_kernel::PreparedTool> {
        bail!("no fixture tools")
    }
    async fn execute(
        &self,
        _tool: bob_kernel::PreparedTool,
        _cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<bob_kernel::ToolOutcome> {
        bail!("no fixture tools")
    }
}

async fn load_metadata_without_history(app: &mut App) {
    let mut jobs = JoinSet::new();
    app.refresh(&mut jobs);
    assert_eq!(jobs.len(), 1, "only the initial metadata query is needed");
    let reply = jobs.join_next().await.unwrap().unwrap();
    let Reply::Runtime { reply: inner, .. } = &reply else {
        panic!("runtime-scoped metadata reply expected");
    };
    assert!(matches!(
        inner.as_ref(),
        Reply::Metadata { result: Ok(_), .. }
    ));
    app.reply(reply);
}

#[tokio::test]
async fn unchanged_watermark_still_refreshes_uncertain_decisions_and_global_halt() {
    let database =
        std::env::temp_dir().join(format!("bob-ui-refresh-{}.sqlite", uuid::Uuid::new_v4()));
    let handle = bob_kernel::Runtime::open(
        &database,
        Arc::new(StreamingExecutor {
            finish: tokio::sync::Notify::new(),
        }),
    )
    .await
    .unwrap();
    let created = handle
        .command(Command::CreateSession {
            id: CommandId::new(),
            workspace: std::env::temp_dir().to_string_lossy().into_owned(),
            profile: Profile {
                model: "fixture".into(),
                instructions: "test".into(),
                read_only: true,
                max_requests: 2,
                reasoning: ReasoningEffort::Off,
            },
            policy: Policy::default(),
        })
        .await
        .unwrap();
    let mut initial = handle
        .transcript(Some(created.thread.clone()), None, 80)
        .await
        .unwrap();
    let sequence = initial.sequence;
    initial.decisions.push(bob_kernel::Decision {
        thread: created.thread.clone(),
        operation: OperationId("ambiguous".into()),
        revision: 1,
        name: "write_file".into(),
        preview: "pending".into(),
        presentation: None,
    });
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((
        handle.clone(),
        initial,
        created.thread.clone(),
    ))));
    let key = DecisionKey::from(&app.pane().unwrap().permission.as_ref().unwrap().decision);
    app.reply(Reply::ApprovalOptions {
        key: key.clone(),
        result: Ok(bob_kernel::ApprovalOptions {
            operation: key.operation,
            thread: key.thread,
            revision: key.revision,
            options: approval::initial_options(),
        }),
    });
    load_metadata_without_history(&mut app).await;
    let mut jobs = JoinSet::new();
    app.refresh(&mut jobs);
    assert!(jobs.is_empty());
    let (key, _) = app.take_decision(true).unwrap();
    app.reply(Reply::Decision {
        key,
        result: Err("unknown commit result".into()),
    });
    app.refresh(&mut jobs);
    assert_eq!(
        jobs.len(),
        1,
        "unchanged sequence must not skip the confirming read"
    );
    app.reply(jobs.join_next().await.unwrap().unwrap());
    assert!(!app.pane().unwrap().decision_refresh);
    assert!(app.pane().unwrap().permission.is_none());
    assert_eq!(app.sequence_floor, sequence);
    let generation = app.pane().unwrap().generation;
    let mut halted = (**app.live_snapshot.as_ref().unwrap()).clone();
    halted.revision += 1;
    halted.error = Some("Global halt: storage unavailable".into());
    assert!(halted.attempts.is_empty());
    assert_eq!(halted.sequence, sequence);
    app.accept_live(Arc::new(halted));
    assert!(render(&mut app, 100, 24).contains("Kernel error: Global halt: storage unavailable"));
    app.reply(Reply::View {
        thread: created.thread.clone(),
        generation,
        result: Ok(handle
            .transcript(Some(created.thread.clone()), None, 80)
            .await
            .unwrap()),
    });
    assert!(
        app.pane().unwrap().force_refresh,
        "pre-halt read must not consume the forced refresh"
    );
    app.refresh(&mut jobs);
    assert_eq!(jobs.len(), 1);
    app.reply(jobs.join_next().await.unwrap().unwrap());
    assert!(!app.pane().unwrap().force_refresh);
    app.refresh(&mut jobs);
    assert!(
        jobs.is_empty(),
        "one fresh read suffices without polling unchanged history"
    );
    app.disconnect_live();
    assert!(render(&mut app, 100, 24).contains("Kernel error: Global halt: storage unavailable"));
    handle.shutdown().await.unwrap();
    std::fs::remove_file(database).unwrap();
}

#[tokio::test]
async fn runtime_watch_streams_before_completion_and_final_replaces_delta() {
    let database =
        std::env::temp_dir().join(format!("bob-ui-live-{}.sqlite", uuid::Uuid::new_v4()));
    let executor = Arc::new(StreamingExecutor {
        finish: tokio::sync::Notify::new(),
    });
    let handle = bob_kernel::Runtime::open(&database, executor.clone())
        .await
        .unwrap();
    let created = handle
        .command(Command::CreateSession {
            id: CommandId::new(),
            workspace: std::env::temp_dir().to_string_lossy().into_owned(),
            profile: Profile {
                model: "fixture".into(),
                instructions: "test".into(),
                read_only: true,
                max_requests: 2,
                reasoning: ReasoningEffort::Off,
            },
            policy: Policy::default(),
        })
        .await
        .unwrap();
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: created.thread.clone(),
            text: "stream".into(),
        })
        .await
        .unwrap();
    let initial = handle
        .transcript(Some(created.thread.clone()), None, 80)
        .await
        .unwrap();
    let mut app = App::new(80);
    app.reply(Reply::Ready(Ok((
        handle.clone(),
        initial,
        created.thread.clone(),
    ))));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !app.live_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .attempts
                .iter()
                .any(|attempt| attempt.text.contains("Observable streaming delta"))
        }) {
            let snapshot = live::changed(&mut app.live_receiver).await.unwrap();
            app.accept_live(snapshot);
        }
    })
    .await
    .unwrap();
    assert!(render(&mut app, 90, 24).contains("Observable streaming delta"));
    executor.finish.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = live::changed(&mut app.live_receiver).await.unwrap();
            let settled = snapshot
                .attempts
                .iter()
                .any(|attempt| attempt.settled_sequence.is_some());
            app.accept_live(snapshot);
            if settled {
                break;
            }
        }
    })
    .await
    .unwrap();
    let final_view = handle
        .transcript(Some(created.thread.clone()), None, 80)
        .await
        .unwrap();
    app.reply(Reply::View {
        thread: created.thread,
        generation: 0,
        result: Ok(final_view),
    });
    let screen = render(&mut app, 90, 24);
    assert!(screen.contains("Committed final answer"));
    assert!(!screen.contains("Observable streaming delta"));
    load_metadata_without_history(&mut app).await;
    let mut jobs = JoinSet::new();
    for _ in 0..4 {
        let mut snapshot = (**app.live_snapshot.as_ref().unwrap()).clone();
        snapshot.revision += 1;
        app.accept_live(Arc::new(snapshot));
        app.refresh(&mut jobs);
        assert!(jobs.is_empty(), "live-only redraw must not issue any query");
    }
    app.pane_mut().unwrap().generation += 1;
    app.refresh(&mut jobs);
    assert!(
        jobs.is_empty(),
        "history generation must not invalidate metadata"
    );
    handle.shutdown().await.unwrap();
    std::fs::remove_file(database).unwrap();
}
