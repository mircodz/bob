use crate::support::TestDir;
use bob_core::core::events::AgentEvent;
use bob_core::core::session::{self, Session, SessionSummary};
use bob_core::core::store::{MemoryStore, SessionStore, SqliteStore};
use bob_core::core::types::{Message, Role};

fn prompt(text: &str) -> AgentEvent {
    AgentEvent::UserPrompt {
        agent_id: "root".into(),
        text: text.into(),
    }
}

fn snapshot(id: &str, timestamp: &str, cwd: &str) -> Session {
    let mut s = session::new_session("mock", id.into(), timestamp.into(), cwd.into());
    s.messages.push(Message::user_text(" first\nquestion "));
    s
}

fn exercise_store(store: &dyn SessionStore) -> Vec<SessionSummary> {
    assert!(store.load("missing").unwrap().is_none());
    assert!(store.latest_in(".").unwrap().is_none());
    assert!(store.list().unwrap().is_empty());
    assert!(store.load_events("missing").unwrap().is_none());
    for (id, timestamp, cwd) in [
        ("old", "9", "."),
        ("b", "10", "."),
        ("a", "10", "."),
        ("other", "11", "elsewhere"),
    ] {
        let expected = snapshot(id, timestamp, cwd);
        store.save(&expected).unwrap();
        let loaded = store.load(id).unwrap().unwrap();
        assert_eq!(loaded.cwd, cwd);
        assert_eq!(
            serde_json::to_value(loaded).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
    let mut s = store.load("a").unwrap().unwrap();
    s.messages.push(Message::user_text("second"));
    store.save(&s).unwrap();
    let summaries_before_stale = serde_json::to_value(store.list().unwrap()).unwrap();
    let mut stale = s.clone();
    stale.updated_at = "99".into();
    stale.provider = "stale provider".into();
    stale.cwd = "stale directory".into();
    stale.messages = vec![Message::user_text("stale title")];
    store.save(&stale).unwrap();
    let kept = store.load("a").unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&kept).unwrap(),
        serde_json::to_value(&s).unwrap()
    );
    assert_eq!(
        serde_json::to_value(store.list().unwrap()).unwrap(),
        summaries_before_stale
    );
    s.messages[1] = Message::user_text("equal-length replacement");
    s.provider = "updated provider".into();
    store.save(&s).unwrap();
    assert_eq!(
        serde_json::to_value(store.load("a").unwrap().unwrap()).unwrap(),
        serde_json::to_value(&s).unwrap()
    );
    assert_eq!(store.latest_in(".").unwrap().unwrap().id, "a");
    assert_eq!(store.latest_in("elsewhere").unwrap().unwrap().id, "other");
    assert!(store.latest_in("unknown").unwrap().is_none());
    assert!(store.latest_in("").unwrap().is_none());
    assert_messages(&store.history_for(&s).unwrap(), &s.messages);
    store.append_event("a", &prompt("rebuilt first")).unwrap();
    store.flush().unwrap();
    assert_messages(&store.history_for(&s).unwrap(), &s.messages);
    store.append_event("a", &prompt("rebuilt second")).unwrap();
    store.flush().unwrap();
    assert_eq!(store.load_events("a").unwrap().unwrap().len(), 2);
    assert_messages(
        &store.history_for(&s).unwrap(),
        &[
            Message::user_text("rebuilt first"),
            Message::user_text("rebuilt second"),
        ],
    );
    let summaries = store.list().unwrap();
    assert_eq!(
        summaries.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
        ["other", "a", "b", "old"]
    );
    assert_eq!(summaries[1].title, "first question");
    assert_eq!(summaries[1].provider, "updated provider");
    assert_eq!(summaries[1].message_count, 2);
    assert_eq!(summaries[1].created_at, "10");
    assert_eq!(summaries[1].updated_at, "10");
    assert_eq!(summaries[1].cwd, ".");
    summaries
}

#[test]
fn memory_and_sqlite_have_matching_summaries_history_and_latest() {
    let memory = exercise_store(&MemoryStore::new());
    let dir = TestDir::new();
    let sqlite = exercise_store(&SqliteStore::open(dir.join("nested/sessions.db")).unwrap());
    assert_eq!(
        serde_json::to_value(memory).unwrap(),
        serde_json::to_value(sqlite).unwrap()
    );
}

#[test]
fn sqlite_reopens_selected_file_with_sessions_and_ordered_events() {
    let dir = TestDir::new();
    let path = dir.join("nested/sessions.db");
    let s = snapshot("s", "1700000000", ".");
    {
        let store = SqliteStore::open(&path).unwrap();
        store.save(&s).unwrap();
        for i in 0..100 {
            store
                .append_event("s", &prompt(&format!("event {i}")))
                .unwrap();
        }
        store.flush().unwrap();
        assert_eq!(store.load_events("s").unwrap().unwrap().len(), 100);
    }
    let reopened = SqliteStore::open(&path).unwrap();
    let loaded = reopened.load("s").unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&loaded).unwrap(),
        serde_json::to_value(&s).unwrap()
    );
    let history = reopened.history_for(&loaded).unwrap();
    assert_eq!(history.len(), 100);
    assert_eq!(history[0].text(), "event 0");
    assert_eq!(history[99].text(), "event 99");
    reopened.append_event("s", &prompt("after reopen")).unwrap();
    reopened.flush().unwrap();
    assert_eq!(reopened.load_events("s").unwrap().unwrap().len(), 101);
    let other = SqliteStore::open(dir.0.join("other.db")).unwrap();
    assert!(other.load("s").unwrap().is_none());
    assert!(other.load_events("s").unwrap().is_none());
}

fn compaction(summary: &str, replaced_upto: usize) -> AgentEvent {
    AgentEvent::Compaction {
        agent_id: "root".into(),
        before_tokens: 100,
        after_tokens: 20,
        summary: summary.into(),
        replaced_upto,
    }
}

fn assert_messages(actual: &[Message], expected: &[Message]) {
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
}

fn exercise_working_history(store: &dyn SessionStore) {
    use bob_core::core::types::ContentBlock;
    let mut events = vec![
        prompt("original question"),
        AgentEvent::Message {
            agent_id: "root".into(),
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "file"}),
                }],
            },
        },
        AgentEvent::ToolResult {
            agent_id: "root".into(),
            tool_use_id: "call".into(),
            output: "file contents".into(),
            is_error: false,
        },
        AgentEvent::Compaction {
            agent_id: "child".into(),
            before_tokens: 100,
            after_tokens: 1,
            summary: "must not replace root history".into(),
            replaced_upto: 999,
        },
        AgentEvent::UserPrompt {
            agent_id: "child".into(),
            text: "child prompt".into(),
        },
        compaction("first summary", 3),
        prompt("after first compaction"),
    ];
    let mut saved = session::new_session("mock", "working".into(), "0".into(), ".".into());
    saved.messages = session::root_history_from_events(&events);
    for event in &events {
        store.append_event(&saved.id, event).unwrap();
    }
    store.save(&saved).unwrap();
    store.flush().unwrap();
    assert_messages(&store.history_for(&saved).unwrap(), &saved.messages);
    assert_messages(
        &store.working_history_for(&saved).unwrap(),
        &[
            Message::user_text("[conversation summary]\nfirst summary"),
            Message::user_text("after first compaction"),
        ],
    );
    let next = [
        compaction("second summary", 2),
        prompt("after second compaction"),
    ];
    for event in &next {
        store.append_event(&saved.id, event).unwrap();
    }
    events.extend(next);
    store.flush().unwrap();
    assert_messages(
        &store.history_for(&saved).unwrap(),
        &session::root_history_from_events(&events),
    );
    assert_messages(
        &store.working_history_for(&saved).unwrap(),
        &[
            Message::user_text("[conversation summary]\nsecond summary"),
            Message::user_text("after second compaction"),
        ],
    );
    saved.messages = store.history_for(&saved).unwrap();
    store.save(&saved).unwrap();
    assert_messages(
        &store.load(&saved.id).unwrap().unwrap().messages,
        &saved.messages,
    );
}

#[test]
fn memory_and_sqlite_keep_full_history_and_replay_root_compactions() {
    exercise_working_history(&MemoryStore::new());
    let dir = TestDir::new();
    exercise_working_history(&SqliteStore::open(dir.join("nested/sessions.db")).unwrap());
    let reopened = SqliteStore::open(dir.join("nested/sessions.db")).unwrap();
    let saved = reopened.load("working").unwrap().unwrap();
    assert_messages(&reopened.history_for(&saved).unwrap(), &saved.messages);
    assert_messages(
        &reopened.working_history_for(&saved).unwrap(),
        &[
            Message::user_text("[conversation summary]\nsecond summary"),
            Message::user_text("after second compaction"),
        ],
    );
}

#[test]
fn working_history_falls_back_for_legacy_incomplete_and_invalid_logs() {
    let dir = TestDir::new();
    let stores: Vec<Box<dyn SessionStore>> = vec![
        Box::new(MemoryStore::new()),
        Box::new(SqliteStore::open(dir.join("nested/sessions.db")).unwrap()),
    ];
    for store in stores {
        for (id, events) in [
            ("legacy", vec![]),
            (
                "legacy-compaction",
                vec![compaction("cannot locate baseline", 2)],
            ),
            ("short", vec![prompt("a"), compaction("partial", 1)]),
            (
                "empty-summary",
                vec![prompt("a"), prompt("b"), compaction("  ", 1)],
            ),
            (
                "zero-index",
                vec![prompt("a"), prompt("b"), compaction("invalid", 0)],
            ),
            (
                "bad-index",
                vec![prompt("a"), prompt("b"), compaction("invalid", 3)],
            ),
            (
                "unknown",
                vec![
                    prompt("a"),
                    AgentEvent::Unknown,
                    prompt("b"),
                    compaction("unsafe", 1),
                ],
            ),
            (
                "mismatched",
                vec![
                    prompt("wrong a"),
                    prompt("wrong b"),
                    compaction("unsafe", 1),
                ],
            ),
        ] {
            let mut saved = session::new_session("mock", id.into(), "0".into(), ".".into());
            saved.messages = vec![Message::user_text("a"), Message::user_text("b")];
            store.save(&saved).unwrap();
            for event in events {
                store.append_event(id, &event).unwrap();
            }
            store.flush().unwrap();
            assert_messages(
                &store.working_history_for(&saved).unwrap(),
                &store.history_for(&saved).unwrap(),
            );
        }
    }
}
