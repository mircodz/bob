use super::*;
use bob_kernel::{EntrySender, EntrySubmission, QueueSnapshot, QueuedInput, SubmissionState};

fn submission(id: &str, ordinal: i64, sequence: i64, state: SubmissionState) -> EntrySubmission {
    EntrySubmission {
        id: CommandId(id.into()),
        ordinal,
        sequence,
        state,
    }
}

fn human(id: &str, ordinal: i64, sequence: i64, state: SubmissionState) -> Entry {
    let mut entry = entry(ordinal, EntryKind::Submission, "identical message");
    entry.submission = Some(submission(id, ordinal, sequence, state));
    entry
}

fn queue(sequence: i64, states: Vec<EntrySubmission>) -> QueueSnapshot {
    QueueSnapshot {
        sequence,
        pending: states
            .iter()
            .filter(|submission| submission.state == SubmissionState::Pending)
            .map(|submission| QueuedInput {
                id: submission.id.clone(),
                thread: ThreadId("root".into()),
                text: "identical message".into(),
            })
            .collect(),
        submissions: states,
    }
}

fn copies(app: &mut App, expected: usize) {
    let screen = screen(&render(app, 120, 40));
    assert_eq!(
        screen.matches("identical message").count(),
        expected,
        "{screen}"
    );
    let builds = app.pane().unwrap().render_builds;
    render(app, 120, 40);
    assert_eq!(app.pane().unwrap().render_builds, builds);
}

#[test]
fn pending_transcript_first_is_queue_only_until_consumed_metadata_arrives() {
    let mut app = app(vec![human("one", 1, 10, SubmissionState::Pending)]);
    app.pane_mut().unwrap().reconcile_submissions();
    copies(&mut app, 1);
    assert!(render::transcript(app.pane().unwrap(), true, 120).is_empty());
    app.pane_mut().unwrap().accept_queue(queue(
        10,
        vec![submission("one", 1, 10, SubmissionState::Pending)],
    ));
    copies(&mut app, 1);
    let builds = app.pane().unwrap().render_builds;
    app.pane_mut().unwrap().accept_queue(queue(
        20,
        vec![submission("one", 1, 20, SubmissionState::Consumed)],
    ));
    copies(&mut app, 1);
    assert!(app.pane().unwrap().render_builds > builds);
    assert!(app.pane().unwrap().visible_queue().is_empty());
    assert!(render::transcript(app.pane().unwrap(), true, 120)
        .iter()
        .any(|row| row.band));
    app.pane_mut().unwrap().accept_queue(queue(
        10,
        vec![submission("one", 1, 10, SubmissionState::Pending)],
    ));
    copies(&mut app, 1);
    assert!(app.pane().unwrap().visible_queue().is_empty());
}

#[test]
fn consumed_transcript_first_removes_stale_queue_and_ignores_late_pending_snapshot() {
    let mut app = app(vec![human("one", 1, 10, SubmissionState::Pending)]);
    app.pane_mut().unwrap().accept_queue(queue(
        10,
        vec![submission("one", 1, 10, SubmissionState::Pending)],
    ));
    copies(&mut app, 1);
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(
            20,
            vec![human("one", 1, 20, SubmissionState::Consumed)],
        )),
    });
    copies(&mut app, 1);
    app.pane_mut().unwrap().accept_queue(queue(
        15,
        vec![submission("one", 1, 15, SubmissionState::Pending)],
    ));
    copies(&mut app, 1);
    assert!(app.pane().unwrap().queue.is_empty());
}

#[test]
fn recalled_and_cancelled_inputs_never_reappear_as_delivered() {
    for terminal in [SubmissionState::Recalled, SubmissionState::Cancelled] {
        let mut app = app(vec![human("one", 1, 10, SubmissionState::Pending)]);
        app.pane_mut().unwrap().accept_queue(queue(
            10,
            vec![submission("one", 1, 10, SubmissionState::Pending)],
        ));
        copies(&mut app, 1);
        app.pane_mut()
            .unwrap()
            .accept_queue(queue(20, vec![submission("one", 1, 20, terminal)]));
        copies(&mut app, 0);
        app.pane_mut().unwrap().accept_queue(queue(
            10,
            vec![submission("one", 1, 10, SubmissionState::Pending)],
        ));
        copies(&mut app, 0);
        if terminal == SubmissionState::Recalled {
            app.pane_mut().unwrap().draft.insert("identical message");
            copies(&mut app, 1);
            assert!(render::transcript(app.pane().unwrap(), true, 120).is_empty());
        }
    }
}

#[test]
fn identical_text_uses_submission_identity_not_content_or_queue_absence() {
    let mut app = app(vec![
        human("one", 1, 10, SubmissionState::Consumed),
        human("two", 2, 10, SubmissionState::Pending),
        human("three", 3, 10, SubmissionState::Recalled),
    ]);
    app.pane_mut().unwrap().accept_queue(queue(20, Vec::new()));
    copies(&mut app, 1);
    assert!(!app
        .pane()
        .unwrap()
        .submission_visible(&app.pane().unwrap().view.as_ref().unwrap().entries[1]));
    app.pane_mut().unwrap().accept_queue(queue(
        20,
        vec![submission("two", 2, 20, SubmissionState::Pending)],
    ));
    copies(&mut app, 2);
    assert_eq!(app.pane().unwrap().visible_queue().len(), 1);
    assert_eq!(
        app.pane().unwrap().visible_queue()[0].id,
        CommandId("two".into())
    );
    app.pane_mut().unwrap().accept_queue(queue(
        30,
        vec![submission("two", 2, 30, SubmissionState::Consumed)],
    ));
    copies(&mut app, 2);
    assert!(app.pane().unwrap().visible_queue().is_empty());
}

#[test]
fn partial_transcript_queue_order_and_eviction_remain_bounded() {
    let mut app = app(vec![human("middle", 2, 10, SubmissionState::Pending)]);
    app.pane_mut().unwrap().accept_queue(queue(
        10,
        vec![
            submission("first", 1, 10, SubmissionState::Pending),
            submission("last", 3, 10, SubmissionState::Pending),
        ],
    ));
    assert_eq!(
        app.pane()
            .unwrap()
            .visible_queue()
            .iter()
            .map(|input| input.id.0.as_str())
            .collect::<Vec<_>>(),
        ["first", "middle", "last"]
    );
    for ordinal in 4..800 {
        let pane = app.pane_mut().unwrap();
        pane.view = Some(view(
            ordinal,
            vec![human("moving", ordinal, ordinal, SubmissionState::Consumed)],
        ));
        pane.reconcile_submissions();
        assert!(pane.submissions.len() <= 3);
    }
    app.pane_mut().unwrap().view = None;
    app.pane_mut().unwrap().tracked_submissions();
    assert_eq!(app.pane().unwrap().submissions.len(), 2);
}

#[test]
fn parent_guidance_has_the_exact_human_prompt_layout_and_cache_identity() {
    let mut guidance = entry(
        42,
        EntryKind::Coordination,
        "ordinary guidance\nsecond line",
    );
    guidance.sender = Some(EntrySender {
        thread: ThreadId("parent".into()),
        name: Some("parent".into()),
    });
    guidance.parent_guidance = true;
    let mut app = app(vec![guidance.clone()]);
    let expected = {
        let mut human = guidance.clone();
        human.kind = EntryKind::Submission;
        let human_app = super::app(vec![human]);
        render::transcript(human_app.pane().unwrap(), false, 120)
    };
    let actual = render::transcript(app.pane().unwrap(), false, 120);
    assert_eq!(
        actual
            .iter()
            .map(|row| (&row.line, row.band, &row.anchor))
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|row| (&row.line, row.band, &row.anchor))
            .collect::<Vec<_>>()
    );
    assert!(render::transcript(app.pane().unwrap(), true, 120).is_empty());
    let key = render::content_key(app.pane().unwrap().view.as_ref().unwrap());
    app.threads[0].parent_thread = Some(ThreadId("parent".into()));
    app.pane_mut().unwrap().content_key = Some(key);
    assert!(!screen(&render(&mut app, 120, 40)).contains("Message from parent"));
    let builds = app.pane().unwrap().render_builds;
    app.pane_mut().unwrap().view.as_mut().unwrap().entries[0].parent_guidance = false;
    let next_key = render::content_key(app.pane().unwrap().view.as_ref().unwrap());
    assert_ne!(key, next_key);
    app.pane_mut().unwrap().content_key = Some(next_key);
    assert!(screen(&render(&mut app, 120, 40)).contains("Message from parent"));
    assert!(app.pane().unwrap().render_builds > builds);
    let rows = render::transcript(app.pane().unwrap(), false, 120);
    assert!(rows
        .iter()
        .any(|row| row.line.to_string() == "Message from parent"));
    assert!(rows.iter().all(|row| !row.band));
}
