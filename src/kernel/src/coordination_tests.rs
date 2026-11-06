use crate::contracts::{PermissionPolicy, PermissionRule, RuleDecision};
use crate::coordination::{execute, is_descendant, list, resolve, subtree, tools};
use crate::model::*;
use crate::store::tests::fixture;
use crate::store::{consume, decode, encode, submit, Store};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::HashSet;

#[path = "message_projection_tests.rs"]
mod message_projection_tests;

fn run(db: &Connection, thread: &ThreadId, state: &str) -> RunId {
    let id = RunId::new();
    db.execute(
        "INSERT INTO runs(id,thread,state) VALUES(?1,?2,?3)",
        params![id, thread, state],
    )
    .unwrap();
    id
}

fn child(
    db: &Connection,
    parent: &ThreadId,
    parent_run: &RunId,
    name: &str,
    role: &str,
) -> ThreadId {
    let id = ThreadId::new();
    db.execute("INSERT INTO threads(id,session,parent_run,name,profile) SELECT ?1,session,?2,?3,profile FROM threads WHERE id=?4", params![id, parent_run, name, parent]).unwrap();
    db.execute(
        "INSERT INTO agents(thread,role,blocking) VALUES(?1,?2,0)",
        params![id, role],
    )
    .unwrap();
    submit(
        db,
        &CommandId::new(),
        &id,
        "coordination",
        Some(parent),
        "A self-contained task",
    )
    .unwrap();
    id
}

fn operation(db: &Connection, thread: &ThreadId, run: &RunId, name: &str) -> OperationId {
    let id = OperationId::new();
    db.execute("INSERT INTO operations(id,thread,run,name,kind,input,state) VALUES(?1,?2,?3,?4,'tool','{}','pending')", params![id, thread, run, name]).unwrap();
    id
}

fn send(
    store: &mut Store,
    source: &ThreadId,
    run: &RunId,
    target: &ThreadId,
) -> anyhow::Result<String> {
    let op = operation(&store.db, source, run, "send_message");
    let tx = store.db.transaction()?;
    let result = execute(
        &tx,
        &op,
        source,
        "send_message",
        &json!({"to":target,"message":"guidance"}),
    )?;
    tx.commit()?;
    Ok(result)
}

fn item(db: &Connection, caller: &ThreadId, target: &ThreadId) -> Value {
    list(db, caller, &json!({})).unwrap()["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["thread_id"] == target.0)
        .unwrap()
        .clone()
}

#[test]
fn exact_ids_win_over_names_and_ambiguous_or_cross_session_targets_fail_closed() {
    let (_dir, mut store, receipt, models) = fixture();
    let root = receipt.thread;
    let root_run = run(&store.db, &root, "active");
    let first = child(&store.db, &root, &root_run, "duplicate", "collaborator");
    child(&store.db, &root, &root_run, &first.0, "collaborator");
    assert_eq!(
        resolve(&store.db, &receipt.session, &first.0).unwrap(),
        first
    );
    child(&store.db, &root, &root_run, "duplicate", "task");
    assert!(resolve(&store.db, &receipt.session, "duplicate")
        .unwrap_err()
        .to_string()
        .contains("ambiguous"));
    assert!(resolve(&store.db, &receipt.session, "missing").is_err());
    let profile: Profile = decode(
        store
            .db
            .query_row("SELECT profile FROM threads WHERE id=?1", [&root], |r| {
                r.get(0)
            })
            .unwrap(),
    )
    .unwrap();
    let other = store
        .command(
            &Command::CreateSession {
                id: CommandId::new(),
                workspace: _dir.0.to_string_lossy().into_owned(),
                profile,
                policy: Policy::default(),
            },
            &models,
        )
        .unwrap();
    child(&store.db, &root, &root_run, &other.thread.0, "collaborator");
    assert!(resolve(&store.db, &receipt.session, &other.thread.0).is_err());
    assert!(!is_descendant(&store.db, &root, &other.thread).unwrap());
    assert!(list(&store.db, &root, &json!({"cursor":other.thread})).is_err());
    assert!(send(&mut store, &root, &root_run, &other.thread).is_err());
    assert!(!list(&store.db, &root, &json!({})).unwrap()["agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|agent| agent["thread_id"] == other.thread.0));
}

#[test]
fn parent_sibling_cousin_and_root_can_receive_without_recipient_role_restrictions() {
    let (_dir, mut store, receipt, _) = fixture();
    let root = receipt.thread;
    let root_run = run(&store.db, &root, "active");
    let parent = child(&store.db, &root, &root_run, "parent", "collaborator");
    let parent_run = run(&store.db, &parent, "active");
    let sibling = child(&store.db, &root, &root_run, "sibling", "collaborator");
    let sibling_run = run(&store.db, &sibling, "active");
    let source = child(&store.db, &parent, &parent_run, "source", "collaborator");
    let source_run = run(&store.db, &source, "active");
    for role in ["collaborator", "task", "workflow", "explore"] {
        let target = child(&store.db, &sibling, &sibling_run, role, role);
        assert_eq!(item(&store.db, &source, &target)["can_message"], true);
        assert!(send(&mut store, &source, &source_run, &target).is_ok());
        assert!(send(&mut store, &parent, &parent_run, &target).is_ok());
    }
    for (target, relation) in [(&parent, "parent"), (&root, "ancestor"), (&sibling, "peer")] {
        assert_eq!(item(&store.db, &source, target)["relationship"], relation);
        assert!(send(&mut store, &source, &source_run, target).is_ok());
    }
    assert_eq!(
        item(&store.db, &parent, &sibling)["relationship"],
        "sibling"
    );
    assert!(send(&mut store, &parent, &parent_run, &sibling).is_ok());
    assert!(send(&mut store, &root, &root_run, &source).is_ok());
    assert!(!item(&store.db, &source, &source)["can_message"]
        .as_bool()
        .unwrap());
    assert!(send(&mut store, &source, &source_run, &source).is_err());
}

#[test]
fn list_and_send_share_queued_active_joining_approval_and_terminal_eligibility() {
    let (_dir, mut store, receipt, _) = fixture();
    let root = receipt.thread;
    let root_run = run(&store.db, &root, "active");
    for (name, run_state, op_kind, op_state, held, expected) in [
        ("queued", None, "tool", "done", false, true),
        ("active", Some("active"), "tool", "done", false, true),
        ("joining", Some("active"), "model", "waiting", false, true),
        ("approval", Some("active"), "tool", "approval", false, true),
        ("waiting", Some("active"), "tool", "waiting", false, true),
        (
            "interrupted",
            Some("interrupted"),
            "tool",
            "done",
            false,
            true,
        ),
        ("held", Some("active"), "tool", "done", true, false),
        ("completed", Some("completed"), "tool", "done", false, false),
        ("failed", Some("failed"), "tool", "done", false, false),
        ("cancelled", Some("cancelled"), "tool", "done", false, false),
        ("stopping", Some("stopping"), "tool", "done", false, false),
        (
            "recovery",
            Some("interrupted"),
            "tool",
            "unknown",
            true,
            false,
        ),
    ] {
        let target = child(&store.db, &root, &root_run, name, "collaborator");
        if let Some(state) = run_state {
            store
                .db
                .execute(
                    "UPDATE submissions SET state='settled' WHERE thread=?1",
                    [&target],
                )
                .unwrap();
            let target_run = run(&store.db, &target, state);
            let op = operation(&store.db, &target, &target_run, "test");
            store
                .db
                .execute(
                    "UPDATE operations SET kind=?2,state=?3 WHERE id=?1",
                    params![op, op_kind, op_state],
                )
                .unwrap();
        }
        store
            .db
            .execute(
                "UPDATE threads SET held=?2 WHERE id=?1",
                params![target, held],
            )
            .unwrap();
        let before = item(&store.db, &root, &target);
        assert_eq!(before["state"], name);
        assert_eq!(before["can_message"], expected, "{name}");
        assert_eq!(
            send(&mut store, &root, &root_run, &target).is_ok(),
            expected,
            "{name}"
        );
    }
    let stopping = child(&store.db, &root, &root_run, "stopping-queued", "task");
    run(&store.db, &stopping, "stopping");
    assert_eq!(
        item(&store.db, &root, &stopping)["message_reason"],
        "recipient_stopping"
    );
    assert!(send(&mut store, &root, &root_run, &stopping).is_err());
}

#[test]
fn explorer_and_policy_denials_match_projection_but_read_only_collaborators_can_coordinate() {
    let (_dir, mut store, receipt, _) = fixture();
    let root = receipt.thread;
    let root_run = run(&store.db, &root, "active");
    let caller = child(&store.db, &root, &root_run, "reviewer", "collaborator");
    let caller_run = run(&store.db, &caller, "active");
    let target = child(&store.db, &caller, &caller_run, "target", "task");
    let mut profile: Profile = decode(
        store
            .db
            .query_row("SELECT profile FROM threads WHERE id=?1", [&caller], |r| {
                r.get(0)
            })
            .unwrap(),
    )
    .unwrap();
    profile.read_only = true;
    store
        .db
        .execute(
            "UPDATE threads SET profile=?2 WHERE id=?1",
            params![caller, encode(&profile).unwrap()],
        )
        .unwrap();
    assert_eq!(item(&store.db, &caller, &target)["can_message"], true);
    assert!(send(&mut store, &caller, &caller_run, &target).is_ok());
    store
        .db
        .execute(
            "UPDATE agents SET role='explore' WHERE thread=?1",
            [&caller],
        )
        .unwrap();
    let projected = item(&store.db, &caller, &target);
    assert_eq!(projected["message_reason"], "caller_role_denied");
    assert_eq!(projected["stop_reason"], "caller_role_denied");
    assert!(send(&mut store, &caller, &caller_run, &target).is_err());
    let stop_op = operation(&store.db, &caller, &caller_run, "stop_agent");
    {
        let tx = store.db.transaction().unwrap();
        assert!(execute(
            &tx,
            &stop_op,
            &caller,
            "stop_agent",
            &json!({"name":target})
        )
        .is_err());
    }
    store
        .db
        .execute(
            "UPDATE agents SET role='collaborator' WHERE thread=?1",
            [&caller],
        )
        .unwrap();
    let mut policy = Policy::default();
    policy.denied_tools.insert("send_message".into());
    store
        .db
        .execute(
            "UPDATE sessions SET policy=?2 WHERE id=?1",
            params![receipt.session, encode(&policy).unwrap()],
        )
        .unwrap();
    assert_eq!(
        item(&store.db, &caller, &target)["message_reason"],
        "caller_policy_denied"
    );
    assert!(send(&mut store, &caller, &caller_run, &target).is_err());
    store
        .db
        .execute(
            "UPDATE sessions SET policy=?2 WHERE id=?1",
            params![receipt.session, encode(&Policy::default()).unwrap()],
        )
        .unwrap();
    let mut permissions = PermissionPolicy::default();
    permissions.rules.push(PermissionRule {
        tool: "send_message".into(),
        decision: RuleDecision::RequireApproval,
    });
    store
        .db
        .execute(
            "UPDATE sessions SET permissions=?2 WHERE id=?1",
            params![receipt.session, encode(&permissions).unwrap()],
        )
        .unwrap();
    let projected = item(&store.db, &caller, &target);
    assert_eq!(projected["can_message"], true);
    assert_eq!(projected["message_requires_approval"], true);
    permissions.rules[0].decision = RuleDecision::Deny;
    store
        .db
        .execute(
            "UPDATE sessions SET permissions=?2 WHERE id=?1",
            params![receipt.session, encode(&permissions).unwrap()],
        )
        .unwrap();
    assert_eq!(item(&store.db, &caller, &target)["can_message"], false);
}

#[test]
fn stop_authority_includes_finished_bridges_but_not_self_siblings_or_ancestors() {
    let (_dir, mut store, receipt, _) = fixture();
    let root = receipt.thread;
    let root_run = run(&store.db, &root, "active");
    let parent = child(&store.db, &root, &root_run, "parent", "collaborator");
    let parent_run = run(&store.db, &parent, "active");
    let bridge = child(&store.db, &parent, &parent_run, "bridge", "collaborator");
    let bridge_run = run(&store.db, &bridge, "completed");
    store
        .db
        .execute(
            "UPDATE submissions SET state='settled' WHERE thread=?1",
            [&bridge],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE agents SET state='completed' WHERE thread=?1",
            [&bridge],
        )
        .unwrap();
    let leaf = child(&store.db, &bridge, &bridge_run, "leaf", "collaborator");
    let sibling = child(&store.db, &root, &root_run, "sibling", "collaborator");
    assert_eq!(
        subtree(&store.db, &parent)
            .unwrap()
            .into_iter()
            .collect::<HashSet<_>>(),
        HashSet::from([parent.clone(), bridge.clone(), leaf.clone()])
    );
    for forbidden in [&parent, &root, &sibling] {
        assert!(!is_descendant(&store.db, &parent, forbidden).unwrap());
        assert_eq!(item(&store.db, &parent, forbidden)["can_stop"], false);
        let op = operation(&store.db, &parent, &parent_run, "stop_agent");
        let tx = store.db.transaction().unwrap();
        assert!(execute(&tx, &op, &parent, "stop_agent", &json!({"name":forbidden})).is_err());
    }
    assert_eq!(item(&store.db, &parent, &bridge)["can_stop"], true);
    let op = operation(&store.db, &parent, &parent_run, "stop_agent");
    let tx = store.db.transaction().unwrap();
    execute(&tx, &op, &parent, "stop_agent", &json!({"name":bridge})).unwrap();
    tx.commit().unwrap();
    for target in [&bridge, &leaf] {
        assert!(store
            .db
            .query_row("SELECT held FROM threads WHERE id=?1", [target], |r| r
                .get::<_, bool>(0))
            .unwrap());
    }
    assert!(!store
        .db
        .query_row("SELECT held FROM threads WHERE id=?1", [&sibling], |r| r
            .get::<_, bool>(
            0
        ))
        .unwrap());
}

#[test]
fn message_submission_link_sender_and_operation_dedup_survive_commit_and_reopen() {
    let (dir, mut store, receipt, _) = fixture();
    let source = receipt.thread;
    let source_run = run(&store.db, &source, "active");
    let target = child(&store.db, &source, &source_run, "target", "collaborator");
    let target_run = run(&store.db, &target, "active");
    let op = operation(&store.db, &source, &source_run, "send_message");
    let text = "shared evidence λ ".repeat(300);
    let input = json!({"to":target,"message":text});
    {
        let tx = store.db.transaction().unwrap();
        execute(&tx, &op, &source, "send_message", &input).unwrap();
    }
    assert_eq!(
        store
            .db
            .query_row("SELECT COUNT(*) FROM coordination", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let submission;
    {
        let tx = store.db.transaction().unwrap();
        let ack = execute(&tx, &op, &source, "send_message", &input).unwrap();
        assert!(ack.contains("not that the recipient read it"));
        let replay = execute(&tx, &op, &source, "send_message", &input).unwrap();
        assert_eq!(ack, replay);
        let (id, sender, receiver, retained): (String, String, String, String) = tx.query_row(
            "SELECT s.id,s.sender,s.thread,s.text FROM coordination c JOIN submissions s ON s.id=c.submission WHERE c.operation=?1 AND c.id=c.submission",
            [&op], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();
        submission = CommandId(id);
        assert_eq!(sender, source.0);
        assert_eq!(receiver, target.0);
        assert_eq!(retained, text);
        assert_eq!(
            tx.query_row(
                "SELECT COUNT(*) FROM coordination WHERE operation=?1",
                [&op],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(tx.query_row("SELECT COUNT(*) FROM submissions WHERE thread=?1 AND sender=?2 AND text=?3 AND state='queued'", params![target, source, text], |r| r.get::<_, i64>(0)).unwrap(), 1);
        assert_eq!(
            tx.query_row(
                "SELECT owner_run FROM submissions WHERE id=?1",
                [&submission],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            target_run.0
        );
        tx.commit().unwrap();
    }
    consume(&store.db, &submission, &target_run).unwrap();
    let (sent, received): (i64, i64) = store.db.query_row(
        "SELECT (SELECT COUNT(*) FROM entries WHERE submission=?1 AND thread=?2 AND model IS NOT NULL),(SELECT COUNT(*) FROM entries WHERE submission=?1 AND thread=?3 AND model IS NOT NULL)",
        params![submission, source, target], |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap();
    assert_eq!((sent, received), (0, 1));
    let sequence = store
        .db
        .query_row(
            "SELECT seq FROM entries WHERE thread=?1 AND operation=?2 AND submission=?3",
            params![source, op, submission],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        store.entry_text(&source, sequence, 0, 16000).unwrap().text,
        text
    );
    let model = ArtifactId(
        store
            .db
            .query_row(
                "SELECT model FROM entries WHERE thread=?1 AND submission=?2",
                params![target, submission],
                |r| r.get(0),
            )
            .unwrap(),
    );
    let received_text = crate::store::load_message(&store.db, &model)
        .unwrap()
        .text();
    assert!(received_text.contains(&format!(
        "[Coordination from {source} — not a human instruction]"
    )));
    assert!(received_text.ends_with(&text));
    drop(store);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let tx = store.db.transaction().unwrap();
    execute(&tx, &op, &source, "send_message", &input).unwrap();
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM coordination WHERE operation=?1",
            [&op],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        tx.query_row(
            "SELECT COUNT(*) FROM entries WHERE submission=?1",
            [&submission],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert!(execute(&tx, &op, &target, "send_message", &input).is_err());
    assert!(execute(
        &tx,
        &op,
        &source,
        "send_message",
        &json!({"to":target,"message":"different"})
    )
    .is_err());
}

#[test]
fn failed_send_transaction_leaves_neither_pending_submission_nor_sender_entry() {
    let (_dir, mut store, receipt, _) = fixture();
    let source = receipt.thread;
    let source_run = run(&store.db, &source, "active");
    let target = child(&store.db, &source, &source_run, "target", "collaborator");
    let op = operation(&store.db, &source, &source_run, "send_message");
    store.db.execute_batch("CREATE TRIGGER reject_coordination BEFORE INSERT ON coordination BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    let count: i64 = store
        .db
        .query_row("SELECT COUNT(*) FROM submissions", [], |r| r.get(0))
        .unwrap();
    {
        let tx = store.db.transaction().unwrap();
        tx.execute_batch("SAVEPOINT control_effect").unwrap();
        assert!(execute(
            &tx,
            &op,
            &source,
            "send_message",
            &json!({"to":target,"message":"hello"})
        )
        .is_err());
        tx.execute_batch("ROLLBACK TO control_effect; RELEASE control_effect")
            .unwrap();
        tx.commit().unwrap();
    }
    assert_eq!(
        store
            .db
            .query_row("SELECT COUNT(*) FROM submissions", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        count
    );
    assert_eq!(
        store
            .db
            .query_row(
                "SELECT COUNT(*) FROM entries WHERE operation=?1",
                [&op],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
}

#[test]
fn unicode_names_and_messages_obey_the_advertised_character_limits() {
    let (_dir, mut store, receipt, _) = fixture();
    let source = receipt.thread;
    let source_run = run(&store.db, &source, "active");
    let name = "雪".repeat(80);
    let target = child(&store.db, &source, &source_run, &name, "collaborator");
    assert_eq!(resolve(&store.db, &receipt.session, &name).unwrap(), target);
    let text = "雪".repeat(16000);
    let input = json!({"to":name,"message":text});
    let descriptor = tools()
        .into_iter()
        .find(|tool| tool.spec.name == "send_message")
        .unwrap();
    crate::workflow::schema::CompiledSchema::compile(&descriptor.spec.input_schema)
        .unwrap()
        .validate(&input)
        .unwrap();
    let op = operation(&store.db, &source, &source_run, "send_message");
    let tx = store.db.transaction().unwrap();
    execute(&tx, &op, &source, "send_message", &input).unwrap();
    let retained: String = tx
        .query_row(
            "SELECT text FROM coordination WHERE operation=?1",
            [&op],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(retained, text);
}

#[test]
fn schemas_require_recipient_and_text_and_execution_rejects_conflicting_aliases() {
    let descriptor = tools()
        .into_iter()
        .find(|tool| tool.spec.name == "send_message")
        .unwrap();
    let schema =
        crate::workflow::schema::CompiledSchema::compile(&descriptor.spec.input_schema).unwrap();
    for invalid in [
        json!({}),
        json!({"to":"agent"}),
        json!({"message":"hello"}),
        json!({"to":1,"text":"hello"}),
    ] {
        assert!(schema.validate(&invalid).is_err());
    }
    for valid in [
        json!({"to":"agent","message":"hello"}),
        json!({"thread_id":"agent","text":"hello"}),
    ] {
        schema.validate(&valid).unwrap();
    }
    let (_dir, mut store, receipt, _) = fixture();
    let source = receipt.thread;
    let source_run = run(&store.db, &source, "active");
    let target = child(&store.db, &source, &source_run, "target", "collaborator");
    for input in [
        json!({"to":target,"thread_id":source,"message":"hello"}),
        json!({"to":target,"message":"hello","text":"different"}),
        json!({"to":target,"message":"  "}),
        json!({"to":target,"message":null}),
        json!({"to":target}),
        json!({"message":"hello"}),
        json!({"to":target,"message":"x".repeat(16001)}),
    ] {
        let op = operation(&store.db, &source, &source_run, "send_message");
        let tx = store.db.transaction().unwrap();
        assert!(
            execute(&tx, &op, &source, "send_message", &input).is_err(),
            "{input}"
        );
    }
    let op = operation(&store.db, &source, &source_run, "send_message");
    let tx = store.db.transaction().unwrap();
    execute(
        &tx,
        &op,
        &source,
        "send_message",
        &json!({"to":target,"thread_id":target,"message":"same","text":"same"}),
    )
    .unwrap();
}

#[test]
fn pagination_exposes_more_than_one_thousand_agents_with_bounded_topology_metadata() {
    let (_dir, mut store, receipt, _) = fixture();
    let root = receipt.thread;
    let root_run = run(&store.db, &root, "active");
    {
        let tx = store.db.transaction().unwrap();
        for index in 0..1105 {
            let role = ["collaborator", "explore", "task", "workflow"][index % 4];
            let target = child(
                &tx,
                &root,
                &root_run,
                &format!("{index}{}", "λ".repeat(200)),
                role,
            );
            tx.execute(
                "UPDATE submissions SET text=?2 WHERE thread=?1",
                params![target, "λ".repeat(1000)],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let mut seen = HashSet::new();
    let mut input = json!({"limit":100});
    loop {
        let page = list(&store.db, &root, &input).unwrap();
        assert!(encode(&page).unwrap().len() < 256 * 1024);
        assert_eq!(page["root_thread_id"], root.0);
        assert_eq!(page["self_thread_id"], root.0);
        let agents = page["agents"].as_array().unwrap();
        assert!(agents.len() <= 100);
        for agent in agents {
            let id = agent["thread_id"].as_str().unwrap();
            assert!(seen.insert(id.to_owned()));
            assert!(agent["name"].as_str().unwrap().len() <= 160);
            if id == root.0 {
                assert_eq!(agent["role"], "root");
                assert_eq!(agent["relationship"], "self");
                assert_eq!(agent["can_stop"], false);
            } else {
                assert_eq!(agent["parent_thread_id"], root.0);
                assert_eq!(agent["relationship"], "child");
                assert_eq!(agent["can_message"], true);
                assert_eq!(agent["can_stop"], true);
                assert!(agent["task"].as_str().unwrap().len() <= 240);
            }
        }
        match page["next_cursor"].as_str() {
            Some(cursor) => input = json!({"limit":100,"cursor":cursor}),
            None => break,
        }
    }
    assert_eq!(seen.len(), 1106);
    for invalid in [
        json!({"limit":0}),
        json!({"limit":101}),
        json!({"limit":1.5}),
        json!({"cursor":"missing"}),
    ] {
        assert!(list(&store.db, &root, &invalid).is_err());
    }
}
