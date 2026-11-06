use super::*;
use crate::contracts::{PermissionPolicy, PermissionRule, RuleDecision};
use bob_types::ReasoningEffort;

fn budget(db: &rusqlite::Connection, thread: &ThreadId) -> (String, i64, i64, i64) {
    db.query_row(
        "SELECT r.id,r.requests,r.epoch,json_extract(t.profile,'$.max_requests') FROM runs r JOIN threads t ON t.id=r.thread WHERE r.thread=?1 ORDER BY r.ordinal DESC LIMIT 1",
        [thread],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

fn wait_result(request: &Request, error: bool) -> String {
    let results: Vec<_> = request
        .spec
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if tool_use_id == "call-wait_message" => {
                assert_eq!(is_error.unwrap_or(false), error);
                Some(content.model_text())
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1, "one result per wait operation");
    results.into_iter().next().unwrap().into_owned()
}

fn assert_status(request: &Request, status: &str) {
    let output: Value = serde_json::from_str(&wait_result(request, false)).unwrap();
    assert_eq!(output["status"], status);
}

fn wait_results(db: &rusqlite::Connection, thread: &ThreadId) -> usize {
    db.query_row(
        "SELECT COUNT(*) FROM entries e JOIN operations o ON o.id=e.operation WHERE o.thread=?1 AND o.name='wait_message' AND e.kind IN ('tool_result','tool_error')",
        [thread],
        |r| r.get(0),
    )
    .unwrap()
}

async fn waiting(handle: &Handle, db: &rusqlite::Connection, thread: &ThreadId) {
    wait_view(handle, thread, |_| {
        db.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND name='wait_message' AND state='waiting')",
            [thread],
            |r| r.get::<_, bool>(0),
        )
        .unwrap()
    })
    .await;
}

async fn assert_quiet_wait(
    handle: &Handle,
    db: &rusqlite::Connection,
    requests: &Requests,
    thread: &ThreadId,
    before: &(String, i64, i64, i64),
) {
    for _ in 0..3 {
        let view = handle.view(Some(thread.clone()), None, 100).await.unwrap();
        assert!(view.error.is_none());
        assert_eq!(
            view.threads.iter().find(|t| t.id == *thread).unwrap().state,
            ThreadState::Active
        );
        assert_eq!(&budget(db, thread), before);
        assert_eq!(wait_results(db, thread), 0);
        assert!(db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND name='wait_message' AND state='waiting')",
                [thread],
                |r| r.get::<_, bool>(0),
            )
            .unwrap());
        assert!(requests.pending.is_empty());
        assert!(requests.receive.is_empty());
    }
}

fn child_id(db: &rusqlite::Connection, name: &str) -> ThreadId {
    ThreadId(
        db.query_row("SELECT id FROM threads WHERE name=?1", [name], |r| r.get(0))
            .unwrap(),
    )
}

#[tokio::test]
async fn readonly_child_waits_for_parent_reply_without_new_run_or_budget() {
    for task in [false, true] {
        let (dir, store, root, models) = fixture();
        drop(store);
        let path = dir.0.join("lab.db");
        let (handle, mut requests) = gated(models, &path).await;
        let db = readonly_db(&path);
        begin(&handle, &root).await;
        requests.next("root-owned-fixture").await.finish(if task {
            tool_call(
                "task",
                json!({"tasks":[{"description":"child","prompt":"reply-child-fixture"}],"read_only":true,"background":true}),
            )
        } else {
            tool_call(
                "spawn_agent",
                json!({"name":"child","task":"reply-child-fixture","read_only":true}),
            )
        });
        let child_request = requests.next("reply-child-fixture").await;
        let child = child_id(&db, "child");
        let descriptor = child_request
            .spec
            .tools
            .iter()
            .find(|tool| tool.name == "wait_message")
            .expect("read-only children can wait for coordination");
        let schema =
            crate::workflow::schema::CompiledSchema::compile(&descriptor.input_schema).unwrap();
        schema.validate(&json!({"from":root.thread})).unwrap();
        for invalid in [json!({}), json!({"from":1}), json!({"from":""})] {
            assert!(schema.validate(&invalid).is_err());
        }
        requests
            .next("root-owned-fixture")
            .await
            .finish(answer("root early final"));
        joining(&handle, &root.thread, "root").await;
        let root_budget = budget(&db, &root.thread);
        child_request.finish(tool_call(
            "send_message",
            json!({"to":root.thread,"message":"which-option-question-fixture"}),
        ));
        let parent = requests.next("root-owned-fixture").await;
        assert!(parent.contains("which-option-question-fixture"));
        let child_request = requests.next("reply-child-fixture").await;
        let before = budget(&db, &child);
        child_request.finish(tool_call("wait_message", json!({"from":root.thread})));
        waiting(&handle, &db, &child).await;
        assert_quiet_wait(&handle, &db, &requests, &child, &before).await;
        assert_eq!(budget(&db, &root.thread).0, root_budget.0);
        assert_eq!(budget(&db, &root.thread).1, root_budget.1 + 1);
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM coordination WHERE kind='message'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            1
        );
        parent.finish(tool_call(
            "send_message",
            json!({"to":"child","message":"choose-safe-option-reply-fixture"}),
        ));
        let child_request = requests.next("reply-child-fixture").await;
        assert_status(&child_request, "message_pending");
        assert!(child_request.contains("choose-safe-option-reply-fixture"));
        assert_eq!(wait_results(&db, &child), 1);
        let after = budget(&db, &child);
        assert_eq!(after, (before.0.clone(), before.1 + 1, before.2, before.3));
        let (owner, epoch, state): (String, i64, String) = db
            .query_row(
                "SELECT s.owner_run,r.epoch,s.state FROM submissions s JOIN runs r ON r.id=s.owner_run JOIN coordination c ON c.submission=s.id WHERE c.source=?1 AND c.destination=?2 AND c.kind='message'",
                params![root.thread, child],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (owner, epoch, state.as_str()),
            (before.0, before.2, "in_use")
        );
        requests
            .next("root-owned-fixture")
            .await
            .finish(answer("root joins guided child"));
        joining(&handle, &root.thread, "root").await;
        child_request.finish(answer("guided-child-result-fixture"));
        let parent = requests.next("root-owned-fixture").await;
        assert!(parent.contains("guided-child-result-fixture"));
        parent.finish(answer("root collected guided child"));
        wait_view(&handle, &root.thread, |view| {
            view.threads
                .iter()
                .all(|t| t.state == ThreadState::Completed)
        })
        .await;
        handle.shutdown().await.unwrap();
        assert_eq!(wait_results(&db, &child), 1);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get::<_, usize>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM coordination WHERE kind='message'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            2
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM coordination WHERE kind='completion'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            1
        );
    }
}

#[tokio::test]
async fn steering_wakes_reply_wait_with_explicit_coordination_pending() {
    let (dir, store, root, models) = fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let (handle, mut requests) = gated(models, &path).await;
    let db = readonly_db(&path);
    begin(&handle, &root).await;
    requests
        .next("root-owned-fixture")
        .await
        .finish(launch("child", "steered-child-fixture"));
    let child_request = requests.next("steered-child-fixture").await;
    let parent = requests.next("root-owned-fixture").await;
    let child = child_id(&db, "child");
    let before = budget(&db, &child);
    child_request.finish(tool_call("wait_message", json!({"from":root.thread})));
    waiting(&handle, &db, &child).await;
    assert_quiet_wait(&handle, &db, &requests, &child, &before).await;
    handle
        .command(Command::Steer {
            id: CommandId::new(),
            thread: child.clone(),
            text: "human-steering-not-parent-reply-fixture".into(),
        })
        .await
        .unwrap();
    let child_request = requests.next("steered-child-fixture").await;
    assert_status(&child_request, "coordination_pending");
    assert!(child_request.contains("human-steering-not-parent-reply-fixture"));
    assert_eq!(
        budget(&db, &child),
        (before.0, before.1 + 1, before.2, before.3)
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM coordination WHERE kind='message'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        0
    );
    parent.finish(answer("joining steered child"));
    joining(&handle, &root.thread, "root").await;
    child_request.finish(answer("steered-child-result-fixture"));
    let parent = requests.next("root-owned-fixture").await;
    assert!(parent.contains("steered-child-result-fixture"));
    parent.finish(answer("collected steered child"));
    wait_view(&handle, &root.thread, |view| {
        view.threads
            .iter()
            .all(|t| t.state == ThreadState::Completed)
    })
    .await;
    handle.shutdown().await.unwrap();
    assert_eq!(wait_results(&db, &child), 1);
}

#[tokio::test]
async fn stopped_sender_returns_unavailable_without_reply_or_resume() {
    let (dir, store, root, models) = fixture();
    drop(store);
    let path = dir.0.join("lab.db");
    let (handle, mut requests) = gated(models, &path).await;
    let db = readonly_db(&path);
    begin(&handle, &root).await;
    requests
        .next("root-owned-fixture")
        .await
        .finish(launch("sender", "unavailable-sender-fixture"));
    let sender_request = requests.next("unavailable-sender-fixture").await;
    requests
        .next("root-owned-fixture")
        .await
        .finish(launch("waiter", "unavailable-waiter-fixture"));
    let waiter_request = requests.next("unavailable-waiter-fixture").await;
    let parent = requests.next("root-owned-fixture").await;
    let sender = child_id(&db, "sender");
    let waiter = child_id(&db, "waiter");
    let before = budget(&db, &waiter);
    waiter_request.finish(tool_call("wait_message", json!({"from":"sender"})));
    waiting(&handle, &db, &waiter).await;
    assert_quiet_wait(&handle, &db, &requests, &waiter, &before).await;
    handle
        .command(Command::StopThread {
            id: CommandId::new(),
            thread: sender.clone(),
        })
        .await
        .unwrap();
    let waiter_request = requests.next("unavailable-waiter-fixture").await;
    assert_status(&waiter_request, "unavailable");
    assert_eq!(
        budget(&db, &waiter),
        (before.0, before.1 + 1, before.2, before.3)
    );
    wait_view(&handle, &root.thread, |view| {
        view.threads
            .iter()
            .any(|t| t.id == sender && t.state == ThreadState::Cancelled)
    })
    .await;
    assert!(db
        .query_row("SELECT held FROM threads WHERE id=?1", [&sender], |r| r
            .get::<_, bool>(0))
        .unwrap());
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM runs WHERE thread=?1",
            [&sender],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM coordination WHERE kind='message'",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        0
    );
    waiter_request.finish(answer("sender unavailable, no invented reply"));
    handle
        .command(Command::StopThread {
            id: CommandId::new(),
            thread: root.thread.clone(),
        })
        .await
        .unwrap();
    drop(sender_request);
    drop(parent);
    handle.shutdown().await.unwrap();
    assert_eq!(wait_results(&db, &waiter), 1);
}

#[tokio::test]
async fn waiting_child_stops_or_recovers_held_without_automatic_replay() {
    for recover in [false, true] {
        let (dir, store, root, models) = fixture();
        drop(store);
        let path = dir.0.join("lab.db");
        let (handle, mut requests) = gated(models.clone(), &path).await;
        let db = readonly_db(&path);
        begin(&handle, &root).await;
        requests
            .next("root-owned-fixture")
            .await
            .finish(launch("child", "cancel-wait-child-fixture"));
        let child_request = requests.next("cancel-wait-child-fixture").await;
        requests
            .next("root-owned-fixture")
            .await
            .finish(answer("root joins waiting child"));
        joining(&handle, &root.thread, "root").await;
        let child = child_id(&db, "child");
        let before = budget(&db, &child);
        child_request.finish(tool_call("wait_message", json!({"from":root.thread})));
        waiting(&handle, &db, &child).await;
        assert_quiet_wait(&handle, &db, &requests, &child, &before).await;
        if recover {
            handle.shutdown().await.unwrap();
            let (reopened, fresh_requests) = gated(models, &path).await;
            reopened
                .activate_session(root.session.clone(), vec![])
                .await
                .unwrap();
            for _ in 0..3 {
                let view = reopened.view(Some(child.clone()), None, 100).await.unwrap();
                assert!(view.error.is_none());
                assert!(db
                    .query_row("SELECT held FROM threads WHERE id=?1", [&child], |r| r
                        .get::<_, bool>(0))
                    .unwrap());
                assert_eq!(budget(&db, &child), before);
                assert_eq!(wait_results(&db, &child), 0);
                assert!(fresh_requests.pending.is_empty());
                assert!(fresh_requests.receive.is_empty());
            }
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM operations WHERE thread=?1 AND name='wait_message'",
                    [&child],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
                1
            );
            reopened.shutdown().await.unwrap();
        } else {
            handle
                .command(Command::StopThread {
                    id: CommandId::new(),
                    thread: root.thread.clone(),
                })
                .await
                .unwrap();
            wait_view(&handle, &root.thread, |view| {
                view.threads
                    .iter()
                    .all(|t| t.state == ThreadState::Cancelled)
            })
            .await;
            assert_eq!(budget(&db, &child), before);
            assert!(requests.pending.is_empty());
            assert!(requests.receive.is_empty());
            assert_eq!(db.query_row("SELECT COUNT(*) FROM operations WHERE name='wait_message' AND state IN ('pending','ready','started','waiting')", [], |r| r.get::<_, usize>(0)).unwrap(), 0);
            handle.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn wait_rejects_role_policy_and_invalid_targets_without_side_effects() {
    for case in [
        "role",
        "permission",
        "policy",
        "self",
        "cross_session",
        "ambiguous",
    ] {
        let (dir, store, root, models) = fixture();
        drop(store);
        let path = dir.0.join("lab.db");
        let (handle, mut requests) = gated(models, &path).await;
        let db = readonly_db(&path);
        let other = handle
            .command(Command::CreateSession {
                id: CommandId::new(),
                workspace: dir.0.to_string_lossy().into_owned(),
                profile: Profile {
                    model: "test".into(),
                    instructions: String::new(),
                    read_only: false,
                    max_requests: 32,
                    reasoning: ReasoningEffort::Off,
                },
                policy: Policy::default(),
            })
            .await
            .unwrap();
        handle
            .activate_session(root.session.clone(), vec![])
            .await
            .unwrap();
        if case == "policy" {
            let mut policy = Policy::default();
            policy.denied_tools.insert("wait_message".into());
            handle
                .command(Command::SetPolicy {
                    id: CommandId::new(),
                    session: root.session.clone(),
                    expected_revision: 0,
                    policy,
                })
                .await
                .unwrap();
        }
        if case == "permission" {
            handle
                .command(Command::SetPermissions {
                    id: CommandId::new(),
                    session: root.session.clone(),
                    expected_revision: 0,
                    permissions: PermissionPolicy {
                        rules: vec![PermissionRule {
                            tool: "wait_message".into(),
                            decision: RuleDecision::Deny,
                        }],
                        ..Default::default()
                    },
                })
                .await
                .unwrap();
        }
        begin(&handle, &root).await;
        requests
            .next("root-owned-fixture")
            .await
            .finish(if case == "role" {
                tool_call(
                    "explore",
                    json!({"query":"denied-wait-child-fixture","description":"child"}),
                )
            } else {
                launch("child", "denied-wait-child-fixture")
            });
        let child_request = requests.next("denied-wait-child-fixture").await;
        let child = child_id(&db, "child");
        if matches!(case, "role" | "permission" | "policy") {
            assert!(!child_request
                .spec
                .tools
                .iter()
                .any(|tool| tool.name == "wait_message"));
        }
        let mut parent = if case == "role" {
            None
        } else {
            Some(requests.next("root-owned-fixture").await)
        };
        let mut duplicates = Vec::new();
        if case == "ambiguous" {
            for task in ["duplicate-first-fixture", "duplicate-second-fixture"] {
                parent.take().unwrap().finish(launch("duplicate", task));
                duplicates.push(requests.next(task).await);
                parent = Some(requests.next("root-owned-fixture").await);
            }
        }
        let target = match case {
            "self" => child.0.as_str(),
            "cross_session" => other.thread.0.as_str(),
            "ambiguous" => "duplicate",
            _ => root.thread.0.as_str(),
        };
        child_request.finish(tool_call("wait_message", json!({"from":target})));
        let child_request = requests.next("denied-wait-child-fixture").await;
        let error = wait_result(&child_request, true);
        assert!(!error.is_empty(), "{case}");
        if case == "ambiguous" {
            assert!(error.contains("ambiguous"), "{error}");
        }
        assert!(!error.contains("message_pending"), "{case}: {error}");
        assert_eq!(wait_results(&db, &child), 1);
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM coordination WHERE kind='message'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM runs WHERE thread=?1",
                [&other.thread],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM submissions WHERE thread=?1",
                [&other.thread],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM operations WHERE name='wait_message' AND state='waiting'",
                [],
                |r| r.get::<_, usize>(0)
            )
            .unwrap(),
            0
        );
        handle
            .command(Command::StopThread {
                id: CommandId::new(),
                thread: root.thread.clone(),
            })
            .await
            .unwrap();
        drop(child_request);
        drop(parent);
        drop(duplicates);
        handle.shutdown().await.unwrap();
    }
}
