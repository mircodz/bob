use super::*;
use crate::EntryKind;

#[test]
fn persisted_guidance_projection_preserves_provenance_across_pages_and_reopen() {
    let (dir, mut store, receipt, _) = fixture();
    let root = receipt.thread;
    let root_run = run(&store.db, &root, "active");
    let parent = child(&store.db, &root, &root_run, "parent", "collaborator");
    let parent_run = run(&store.db, &parent, "active");
    let nested = child(&store.db, &parent, &parent_run, "nested", "collaborator");
    let nested_run = run(&store.db, &nested, "active");
    let peer = child(&store.db, &root, &root_run, "peer", "collaborator");
    let peer_run = run(&store.db, &peer, "active");
    send(&mut store, &parent, &parent_run, &nested).unwrap();
    send(&mut store, &root, &root_run, &nested).unwrap();
    send(&mut store, &peer, &peer_run, &nested).unwrap();
    send(&mut store, &nested, &nested_run, &root).unwrap();
    let tx = store.db.transaction().unwrap();
    crate::lifecycle::queue_completion(&tx, &nested_run, Some(&parent), "guidance")
        .unwrap()
        .unwrap();
    tx.commit().unwrap();
    let ids: Vec<String> = store
        .db
        .prepare("SELECT id FROM submissions WHERE thread=?1 AND state='queued'")
        .unwrap()
        .query_map([&nested], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for id in ids {
        consume(&store.db, &CommandId(id), &nested_run).unwrap();
    }
    let model: Vec<String> = store
        .db
        .prepare("SELECT model FROM entries WHERE thread=?1 AND model IS NOT NULL")
        .unwrap()
        .query_map([&nested], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for artifact in &model {
        let text = store
            .artifact_page(&ArtifactId(artifact.clone()), 0, 4096)
            .unwrap();
        assert!(text.contains("Coordination from"));
        assert!(text.contains("not a human instruction"));
    }
    let expected = store
        .transcript(Some(nested.clone()), None, 100, None)
        .unwrap();
    assert_eq!(expected.entries.len(), 6);
    assert_eq!(
        expected
            .entries
            .iter()
            .map(|entry| entry.parent_guidance)
            .collect::<Vec<_>>(),
        [true, true, true, false, false, false]
    );
    assert!(expected
        .entries
        .iter()
        .all(|entry| entry.kind == EntryKind::Coordination && entry.submission.is_none()));
    assert_eq!(expected.entries[0].sender.as_ref().unwrap().thread, parent);
    assert_eq!(expected.entries[2].sender.as_ref().unwrap().thread, root);
    let parent_view = store
        .transcript(Some(parent.clone()), None, 100, None)
        .unwrap();
    assert!(parent_view.entries[0].parent_guidance);
    assert!(parent_view
        .entries
        .iter()
        .skip(1)
        .all(|entry| !entry.parent_guidance));
    let root_view = store
        .transcript(Some(root.clone()), None, 100, None)
        .unwrap();
    assert!(root_view
        .entries
        .iter()
        .all(|entry| !entry.parent_guidance && !entry.visible_in_root()));
    drop(store);
    let reopened = Store::open(&dir.0.join("lab.db")).unwrap();
    let catalog = crate::SessionCatalog::open(&dir.0.join("lab.db")).unwrap();
    for entry in &expected.entries {
        for view in [
            reopened
                .transcript(Some(nested.clone()), Some(entry.sequence + 1), 1, None)
                .unwrap(),
            reopened
                .transcript_after(Some(nested.clone()), entry.sequence - 1, 1, None)
                .unwrap(),
            catalog
                .transcript(Some(nested.clone()), Some(entry.sequence + 1), 1)
                .unwrap(),
        ] {
            assert_eq!(view.entries.len(), 1);
            assert_eq!(
                serde_json::to_value(&view.entries[0]).unwrap(),
                serde_json::to_value(entry).unwrap()
            );
        }
    }
    let after_model: Vec<String> = reopened
        .db
        .prepare("SELECT model FROM entries WHERE thread=?1 AND model IS NOT NULL")
        .unwrap()
        .query_map([&nested], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(model, after_model);
}

#[test]
fn human_submission_queries_track_identity_state_and_capped_queue_without_mutation() {
    let (dir, mut store, receipt, models) = fixture();
    let root_run = run(&store.db, &receipt.thread, "active");
    let child = child(
        &store.db,
        &receipt.thread,
        &root_run,
        "child",
        "collaborator",
    );
    let child_run = run(&store.db, &child, "active");
    let ids: Vec<_> = (0..132).map(|_| CommandId::new()).collect();
    for id in &ids {
        store
            .command(
                &Command::Submit {
                    id: id.clone(),
                    thread: child.clone(),
                    text: "identical input".into(),
                },
                &models,
            )
            .unwrap();
    }
    let pending = crate::jobs::queue_snapshot(&store.db, &child, &ids).unwrap();
    assert_eq!(pending.pending.len(), 128);
    assert_eq!(pending.submissions.len(), ids.len());
    assert!(pending
        .submissions
        .iter()
        .all(|submission| submission.state == SubmissionState::Pending));
    consume(&store.db, &ids[0], &child_run).unwrap();
    store
        .command(
            &Command::RecallSubmission {
                id: CommandId::new(),
                submission: ids[1].clone(),
            },
            &models,
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE submissions SET state='cancelled' WHERE id=?1",
            [&ids[2]],
        )
        .unwrap();
    let missing = CommandId::new();
    let root_id = CommandId::new();
    store
        .command(
            &Command::Submit {
                id: root_id.clone(),
                thread: receipt.thread,
                text: "private other thread".into(),
            },
            &models,
        )
        .unwrap();
    let mut tracked = ids.clone();
    tracked.extend([missing, root_id]);
    let snapshot = crate::jobs::queue_snapshot(&store.db, &child, &tracked).unwrap();
    assert_eq!(snapshot.submissions.len(), ids.len());
    for (id, expected) in [
        (ids[0].clone(), SubmissionState::Consumed),
        (ids[1].clone(), SubmissionState::Recalled),
        (ids[2].clone(), SubmissionState::Cancelled),
        (ids[131].clone(), SubmissionState::Pending),
    ] {
        assert_eq!(
            snapshot
                .submissions
                .iter()
                .find(|submission| submission.id == id)
                .unwrap()
                .state,
            expected
        );
    }
    assert_eq!(snapshot.pending.len(), 128);
    assert!(!snapshot.pending.iter().any(|input| input.id == ids[131]));
    let oversized = vec![CommandId::new(); 1025];
    assert!(crate::jobs::queue_snapshot(&store.db, &child, &oversized).is_err());
    drop(store);
    let reopened = Store::open(&dir.0.join("lab.db")).unwrap();
    let mut after = 0;
    let mut projected = Vec::new();
    loop {
        let page = reopened
            .transcript_after(Some(child.clone()), after, 17, None)
            .unwrap();
        if page.entries.is_empty() {
            break;
        }
        after = page.entries.last().unwrap().sequence;
        projected.extend(
            page.entries
                .into_iter()
                .filter_map(|entry| entry.submission),
        );
    }
    assert_eq!(projected.len(), ids.len());
    assert_eq!(
        projected
            .iter()
            .map(|submission| &submission.id)
            .collect::<Vec<_>>(),
        ids.iter().collect::<Vec<_>>()
    );
    assert!(projected
        .windows(2)
        .all(|pair| pair[0].ordinal < pair[1].ordinal));
    assert_eq!(projected[0].state, SubmissionState::Consumed);
    assert_eq!(projected[1].state, SubmissionState::Recalled);
    assert_eq!(projected[2].state, SubmissionState::Cancelled);
    let delivered: usize = reopened
        .db
        .query_row(
            "SELECT COUNT(*) FROM entries WHERE thread=?1 AND model IS NOT NULL",
            [&child],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(delivered, 1);
    assert_eq!(
        reopened
            .db
            .query_row(
                "SELECT COUNT(*) FROM submissions WHERE thread=?1 AND source='human'",
                [&child],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
        ids.len()
    );
}
