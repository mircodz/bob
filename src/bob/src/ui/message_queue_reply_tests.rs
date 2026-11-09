use super::*;
use bob_kernel::{Entry, EntrySubmission, QueueSnapshot, QueuedInput, SubmissionState};

fn submission(sequence: i64, state: SubmissionState) -> EntrySubmission {
    EntrySubmission {
        id: CommandId("input".into()),
        state,
        ordinal: 1,
        sequence,
    }
}

fn view(sequence: i64, state: SubmissionState) -> View {
    View {
        sequence,
        entry_sequence: 1,
        policies: Vec::new(),
        threads: Vec::new(),
        entries: vec![Entry {
            sequence: 1,
            thread: ThreadId("root".into()),
            kind: EntryKind::Submission,
            sender: None,
            parent_guidance: false,
            submission: Some(submission(sequence, state)),
            text: "queued text".into(),
            artifact: None,
            operation: None,
            tool: None,
            is_preview: false,
            visible_text_bytes: None,
            thinking: None,
        }],
        decisions: Vec::new(),
        operations: Vec::new(),
        error: None,
    }
}

fn metadata(app: &mut App, sequence: i64, state: SubmissionState) -> Reply {
    let mut reply = input_control_tests::metadata(app, 0);
    let runtime = app.runtime_epoch;
    let Reply::Metadata {
        key,
        result: Ok(metadata),
        ..
    } = &mut reply
    else {
        unreachable!()
    };
    *key = (runtime, sequence);
    app.pane_mut().unwrap().metadata_key = Some(*key);
    metadata.controls.questions.clear();
    metadata.queue = QueueSnapshot {
        sequence,
        pending: if state == SubmissionState::Pending {
            vec![QueuedInput {
                id: CommandId("input".into()),
                thread: ThreadId("root".into()),
                text: "queued text".into(),
            }]
        } else {
            Vec::new()
        },
        submissions: vec![submission(sequence, state)],
    };
    reply
}

fn deliver_view(app: &mut App, sequence: i64, state: SubmissionState) {
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: app.pane().unwrap().generation,
        result: Ok(view(sequence, state)),
    });
}

#[test]
fn actual_reply_orders_keep_terminal_states_monotonic_and_invalidate_cache() {
    for terminal in [
        SubmissionState::Consumed,
        SubmissionState::Recalled,
        SubmissionState::Cancelled,
    ] {
        for metadata_first in [true, false] {
            let mut app = App::new(80);
            app.select(ThreadId("root".into()));
            deliver_view(&mut app, 10, SubmissionState::Pending);
            let reply = metadata(&mut app, 10, SubmissionState::Pending);
            app.reply(reply);
            assert_eq!(app.pane().unwrap().visible_queue().len(), 1);
            let stale = metadata(&mut app, 15, SubmissionState::Pending);
            if metadata_first {
                let latest = metadata(&mut app, 20, terminal);
                app.reply(latest);
                deliver_view(&mut app, 10, SubmissionState::Pending);
            } else {
                deliver_view(&mut app, 20, terminal);
            }
            app.reply(stale);
            let pane = app.pane().unwrap();
            assert!(pane.visible_queue().is_empty());
            assert_eq!(
                pane.submission_visible(&pane.view.as_ref().unwrap().entries[0]),
                terminal == SubmissionState::Consumed
            );
            assert_eq!(pane.submissions[&CommandId("input".into())].state, terminal);
            assert_eq!(pane.submissions[&CommandId("input".into())].sequence, 20);
            assert_eq!(app.sequence_floor, if metadata_first { 10 } else { 20 });
        }
    }
}

#[test]
fn recall_receipt_targets_original_thread_draft_and_rejects_pending_metadata() {
    let mut app = App::new(80);
    let root = ThreadId("root".into());
    app.select(root.clone());
    deliver_view(&mut app, 10, SubmissionState::Pending);
    let pending = metadata(&mut app, 10, SubmissionState::Pending);
    app.reply(pending);
    let stale = metadata(&mut app, 15, SubmissionState::Pending);
    app.pane_mut().unwrap().pending = Some(1);
    let input = app.pane().unwrap().queue[0].clone();
    app.select(ThreadId("other".into()));
    app.draft_mut().insert("other draft");
    app.reply(Reply::Shortcut {
        thread: root.clone(),
        epoch: 1,
        result: Ok(Outcome::Recalled(
            Receipt {
                session: bob_kernel::SessionId("session".into()),
                thread: root.clone(),
                sequence: 20,
                detail: "recalled".into(),
            },
            input,
        )),
    });
    app.reply(stale);
    assert_eq!(app.draft_mut().text, "other draft");
    app.select(root);
    assert_eq!(app.draft_mut().text, "queued text");
    let pane = app.pane().unwrap();
    assert!(pane.visible_queue().is_empty());
    assert!(render::transcript(pane, true, 120).is_empty());
}

#[test]
fn pruned_queue_only_id_reintroduced_at_same_metadata_key_is_tracked_again() {
    let mut app = App::new(80);
    let root = ThreadId("root".into());
    let id = CommandId("input".into());
    app.select(root.clone());
    let mut empty = view(30, SubmissionState::Pending);
    empty.entries.clear();
    app.reply(Reply::View {
        thread: root.clone(),
        generation: app.pane().unwrap().generation,
        result: Ok(empty),
    });
    let pending = metadata(&mut app, 30, SubmissionState::Pending);
    app.reply(pending);
    app.pane_mut().unwrap().metadata_tracked.insert(id.clone());
    let key = app.pane().unwrap().metadata_key;
    let tracked: HashSet<_> = app
        .pane_mut()
        .unwrap()
        .tracked_submissions()
        .into_iter()
        .collect();
    assert_eq!(app.pane().unwrap().metadata_tracked, tracked);

    let mut consumed = metadata(&mut app, 30, SubmissionState::Consumed);
    let Reply::Metadata {
        result: Ok(data), ..
    } = &mut consumed
    else {
        unreachable!()
    };
    data.queue.sequence = 40;
    data.queue.submissions[0].sequence = 40;
    app.reply(consumed);
    assert!(app.pane().unwrap().queue.is_empty());
    assert!(app.pane().unwrap().submissions.is_empty());
    assert_eq!(app.pane().unwrap().metadata_key, key);

    let mut stitched = view(30, SubmissionState::Pending);
    stitched.entries[0].submission.as_mut().unwrap().sequence = 20;
    app.reply(Reply::View {
        thread: root,
        generation: app.pane().unwrap().generation,
        result: Ok(stitched),
    });
    let pane = app.pane_mut().unwrap();
    let reintroduced: HashSet<_> = pane.tracked_submissions().into_iter().collect();
    assert_eq!(reintroduced, tracked);
    assert_eq!(pane.metadata_key, key);
    assert_ne!(pane.metadata_tracked, reintroduced);
    assert!(pane.visible_queue().is_empty());
    assert!(!pane.submission_visible(&pane.view.as_ref().unwrap().entries[0]));
    assert_eq!(pane.submissions[&id].state, SubmissionState::Pending);
    assert_eq!(app.sequence_floor, 30);

    let mut refreshed = metadata(&mut app, 30, SubmissionState::Consumed);
    let Reply::Metadata {
        result: Ok(data), ..
    } = &mut refreshed
    else {
        unreachable!()
    };
    data.queue.sequence = 40;
    data.queue.submissions[0].sequence = 40;
    app.pane_mut().unwrap().metadata_tracked = reintroduced;
    app.reply(refreshed);
    let pane = app.pane().unwrap();
    assert_eq!(pane.submissions.len(), 1);
    assert_eq!(pane.submissions[&id].state, SubmissionState::Consumed);
    assert!(pane.submission_visible(&pane.view.as_ref().unwrap().entries[0]));
    assert!(pane.visible_queue().is_empty());
    assert_eq!(app.sequence_floor, 30);
}

#[test]
fn runtime_replacement_discards_old_queue_snapshot_revision_without_losing_draft() {
    let mut app = App::new(80);
    app.select(ThreadId("root".into()));
    app.accept_live(std::sync::Arc::new(bob_kernel::LiveSnapshot {
        instance: "old".into(),
        revision: 1,
        sequence: 100,
        error: None,
        attempts: Vec::new(),
    }));
    deliver_view(&mut app, 100, SubmissionState::Recalled);
    let reply = metadata(&mut app, 100, SubmissionState::Recalled);
    app.reply(reply);
    app.draft_mut().insert("keep draft");
    app.accept_live(std::sync::Arc::new(bob_kernel::LiveSnapshot {
        instance: "new".into(),
        revision: 1,
        sequence: 1,
        error: None,
        attempts: Vec::new(),
    }));
    assert!(app.pane().unwrap().submissions.is_empty());
    assert_eq!(app.pane().unwrap().queue_sequence, 0);
    deliver_view(&mut app, 1, SubmissionState::Pending);
    let reply = metadata(&mut app, 1, SubmissionState::Pending);
    app.reply(reply);
    assert_eq!(app.pane().unwrap().visible_queue().len(), 1);
    assert_eq!(app.draft_mut().text, "keep draft");
}
