//! Session storage through [`SessionStore`], with [`MemoryStore`] and [`SqliteStore`] backends.

use super::memory::{InMemoryMemories, MemoryRepository, SqliteMemories};
use super::session::{self, EventLogWriter, Session, SessionSummary};
use crate::core::events::AgentEvent;
use crate::core::types::Message;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Event logs are authoritative; snapshots support listing and recovery from incomplete logs.
pub trait SessionStore: Send + Sync {
    /// Optional learned-fact storage, separate from session transcripts. Custom
    /// session backends may leave it disabled; built-in backends provide it.
    fn memory_repository(&self) -> Option<Arc<dyn MemoryRepository>> {
        None
    }

    /// Reject older epochs and shorter histories within the same epoch.
    fn save(&self, session: &Session) -> anyhow::Result<()>;

    /// Load a session by id (blob), or `None` if it doesn't exist.
    fn load(&self, id: &str) -> anyhow::Result<Option<Session>>;

    /// The most-recently-updated session in a given working directory, if any.
    fn latest_in(&self, cwd: &str) -> anyhow::Result<Option<Session>>;

    /// List stored sessions (newest first) as lightweight summaries.
    fn list(&self) -> anyhow::Result<Vec<SessionSummary>>;

    /// Append one event to the session's append-only log. Non-blocking: the
    /// write happens on a dedicated thread so the agent loop never stalls on IO.
    fn append_event(&self, session_id: &str, event: &AgentEvent) -> anyhow::Result<()>;

    /// Block until every queued event is durable. Call at turn-end / quit.
    fn flush(&self) -> anyhow::Result<()>;

    /// The full transcript on resume, never compacted: the reconstructed log when
    /// present (source of truth), falling back to the blob when the log is absent
    /// or shorter (a truncated/corrupt log must never lose history).
    fn history_for(&self, session: &Session) -> anyhow::Result<Vec<Message>> {
        let events = self.load_events(&session.id)?;
        Ok(session::reconstructed_session_history(
            events.as_deref(),
            session,
        ))
    }

    /// Provider context on resume, kept separate from the full transcript.
    /// Compactions require a complete matching event log; legacy or incomplete
    /// logs fall back to the full history without applying their summaries.
    fn working_history_for(&self, session: &Session) -> anyhow::Result<Vec<Message>> {
        let full = self.history_for(session)?;
        let events = self.load_events(&session.id)?;
        let blob = if events
            .as_deref()
            .is_some_and(|events| session::context_epoch(events) > session.context_epoch)
        {
            &[][..]
        } else {
            session.messages.as_slice()
        };
        Ok(session::repair_interrupted_tool_calls(
            session::reconstructed_working_history(events.as_deref(), blob, &full),
        ))
    }

    /// Raw event log for a session, or `None` for a legacy (log-less) session.
    fn load_events(&self, session_id: &str) -> anyhow::Result<Option<Vec<AgentEvent>>>;
}

/// Owns a background event writer; defaults to `~/.bob/bob.db`.
pub struct SqliteStore {
    path: PathBuf,
    writer: EventLogWriter,
}

impl Default for SqliteStore {
    fn default() -> Self {
        Self::at_default_path(session::db_path())
    }
}

impl SqliteStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = std::path::absolute(path)?;
        let writer = EventLogWriter::open(&path)?;
        Ok(Self { path, writer })
    }

    fn at_default_path(path: PathBuf) -> Self {
        let writer = EventLogWriter::spawn_at(&path);
        Self { path, writer }
    }

    fn connection(&self) -> anyhow::Result<rusqlite::Connection> {
        self.writer.check_startup()?;
        session::open_db_at(&self.path)
    }
}

impl SessionStore for SqliteStore {
    fn memory_repository(&self) -> Option<Arc<dyn MemoryRepository>> {
        Some(Arc::new(SqliteMemories {
            path: self.path.clone(),
        }))
    }

    fn save(&self, session: &Session) -> anyhow::Result<()> {
        session::write_session_to(&self.connection()?, session, false)
    }

    fn load(&self, id: &str) -> anyhow::Result<Option<Session>> {
        session::load_session_from(&self.connection()?, id)
    }

    fn latest_in(&self, cwd: &str) -> anyhow::Result<Option<Session>> {
        session::latest_session_from(&self.connection()?, Some(cwd))
    }

    fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
        session::query_summaries_from(&self.connection()?, None)
    }

    fn append_event(&self, session_id: &str, event: &AgentEvent) -> anyhow::Result<()> {
        self.writer.append(session_id, event)
    }

    fn flush(&self) -> anyhow::Result<()> {
        self.writer.flush()
    }

    fn working_history_for(&self, session: &Session) -> anyhow::Result<Vec<Message>> {
        let full = self.history_for(session)?;
        let events = session::load_working_events_from(&self.connection()?, &session.id)?;
        let blob = if events
            .as_deref()
            .is_some_and(|events| session::context_epoch(events) > session.context_epoch)
        {
            &[][..]
        } else {
            session.messages.as_slice()
        };
        Ok(session::repair_interrupted_tool_calls(
            session::reconstructed_working_history(events.as_deref(), blob, &full),
        ))
    }

    fn load_events(&self, session_id: &str) -> anyhow::Result<Option<Vec<AgentEvent>>> {
        session::load_events_from(&self.connection()?, session_id)
    }
}

/// Process-local storage with no filesystem writes.
#[derive(Default)]
pub struct MemoryStore {
    sessions: std::sync::Mutex<std::collections::HashMap<String, Session>>,
    events: std::sync::Mutex<std::collections::HashMap<String, Vec<AgentEvent>>>,
    memories: Arc<InMemoryMemories>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for MemoryStore {
    fn memory_repository(&self) -> Option<Arc<dyn MemoryRepository>> {
        Some(self.memories.clone())
    }

    fn save(&self, session: &Session) -> anyhow::Result<()> {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.get(&session.id).is_some_and(|stored| {
            stored.context_epoch > session.context_epoch
                || (stored.context_epoch == session.context_epoch
                    && stored.messages.len() > session.messages.len())
        }) {
            return Ok(());
        }
        sessions.insert(session.id.clone(), session.clone());
        Ok(())
    }

    fn load(&self, id: &str) -> anyhow::Result<Option<Session>> {
        Ok(self.sessions.lock().unwrap().get(id).cloned())
    }

    fn latest_in(&self, cwd: &str) -> anyhow::Result<Option<Session>> {
        let g = self.sessions.lock().unwrap();
        let latest = g
            .values()
            .filter(|s| s.cwd == cwd)
            .min_by(|a, b| session_order(&a.updated_at, &a.id, &b.updated_at, &b.id))
            .cloned();
        Ok(latest)
    }

    fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
        let mut summaries: Vec<_> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| SessionSummary {
                id: s.id.clone(),
                title: session::title_of(s),
                provider: s.provider.clone(),
                created_at: s.created_at.clone(),
                updated_at: s.updated_at.clone(),
                message_count: s.messages.len(),
                cwd: s.cwd.clone(),
            })
            .collect();
        summaries.sort_by(|a, b| session_order(&a.updated_at, &a.id, &b.updated_at, &b.id));
        Ok(summaries)
    }

    fn append_event(&self, session_id: &str, event: &AgentEvent) -> anyhow::Result<()> {
        self.events
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_default()
            .push(event.clone());
        Ok(())
    }

    fn flush(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn load_events(&self, session_id: &str) -> anyhow::Result<Option<Vec<AgentEvent>>> {
        Ok(self.events.lock().unwrap().get(session_id).cloned())
    }
}

fn session_order(a_time: &str, a_id: &str, b_time: &str, b_id: &str) -> std::cmp::Ordering {
    b_time
        .parse::<i64>()
        .unwrap_or(0)
        .cmp(&a_time.parse::<i64>().unwrap_or(0))
        .then_with(|| b_time.cmp(a_time))
        .then_with(|| a_id.cmp(b_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TestDir;

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

    mod backend_state {
        use super::*;

        #[test]
        fn summaries_order_numeric_times_then_ids_and_latest_is_project_scoped() {
            let dir = TestDir::new();
            let stores: Vec<Box<dyn SessionStore>> = vec![
                Box::new(MemoryStore::new()),
                Box::new(SqliteStore::open(dir.join("order.db")).unwrap()),
            ];
            for store in stores {
                for (id, time, cwd) in [
                    ("z", "9", "project"),
                    ("b", "10", "project"),
                    ("a", "10", "project"),
                    ("elsewhere", "11", "other"),
                ] {
                    store.save(&snapshot(id, time, cwd)).unwrap();
                }
                let summaries = store.list().unwrap();
                assert_eq!(
                    summaries.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                    ["elsewhere", "a", "b", "z"]
                );
                for summary in summaries {
                    let saved = store.load(&summary.id).unwrap().unwrap();
                    assert_eq!(summary.title, "first question");
                    assert_eq!(summary.provider, saved.provider);
                    assert_eq!(summary.cwd, saved.cwd);
                    assert_eq!(summary.created_at, saved.created_at);
                    assert_eq!(summary.updated_at, saved.updated_at);
                    assert_eq!(summary.message_count, saved.messages.len());
                }
                assert_eq!(store.latest_in("project").unwrap().unwrap().id, "a");
                assert_eq!(store.latest_in("other").unwrap().unwrap().id, "elsewhere");
                assert!(store.latest_in("missing").unwrap().is_none());
                assert!(store.latest_in("").unwrap().is_none());
            }
        }

        #[test]
        fn snapshot_guards_preserve_all_fields_and_allow_new_epoch_shrink() {
            let dir = TestDir::new();
            let stores: Vec<Box<dyn SessionStore>> = vec![
                Box::new(MemoryStore::new()),
                Box::new(SqliteStore::open(dir.join("guards.db")).unwrap()),
            ];
            for store in stores {
                let mut original = snapshot("s", "10", "project");
                original.messages.push(Message::user_text("second"));
                store.save(&original).unwrap();
                let mut shorter = snapshot("s", "11", "wrong project");
                shorter.provider = "wrong provider".into();
                store.save(&shorter).unwrap();
                assert_eq!(
                    serde_json::to_value(store.load("s").unwrap().unwrap()).unwrap(),
                    serde_json::to_value(&original).unwrap()
                );
                let mut replacement = original.clone();
                replacement.messages[1] = Message::user_text("corrected");
                replacement.updated_at = "12".into();
                store.save(&replacement).unwrap();
                assert_eq!(
                    serde_json::to_value(store.load("s").unwrap().unwrap()).unwrap(),
                    serde_json::to_value(&replacement).unwrap()
                );
                replacement.context_epoch = 1;
                replacement.messages.clear();
                store.save(&replacement).unwrap();
                store.save(&original).unwrap();
                let mut loaded = store.load("s").unwrap().unwrap();
                assert_eq!(
                    serde_json::to_value(&loaded).unwrap(),
                    serde_json::to_value(&replacement).unwrap()
                );
                loaded.messages.push(Message::user_text("local only"));
                assert_eq!(
                    serde_json::to_value(store.load("s").unwrap().unwrap()).unwrap(),
                    serde_json::to_value(&replacement).unwrap()
                );
            }
        }
    }

    mod sqlite_failures {
        use super::*;

        #[test]
        fn sqlite_missing_corrupt_and_query_failures_are_distinct() {
            let dir = TestDir::new();
            let path = dir.join("nested/sessions.db");
            let store = SqliteStore::open(&path).unwrap();
            assert!(store.load("s").unwrap().is_none());
            store.save(&snapshot("s", "10", ".")).unwrap();
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("UPDATE sessions SET data = 'not json' WHERE id = 's'", [])
                .unwrap();
            assert!(store.load("s").is_err());
            assert!(store.latest_in(".").is_err());
            assert_eq!(
                serde_json::to_value(store.list().unwrap()).unwrap(),
                serde_json::json!([{
                    "id": "s", "title": "first question", "provider": "mock",
                    "created_at": "10", "updated_at": "10", "message_count": 1, "cwd": "."
                }])
            );
            conn.execute("ALTER TABLE sessions RENAME COLUMN data TO broken", [])
                .unwrap();
            assert!(store.load("missing").is_err());
            assert!(store.latest_in("missing").is_err());
            assert!(store.save(&snapshot("s", "11", ".")).is_err());
            conn.execute(
                "ALTER TABLE sessions RENAME COLUMN title TO broken_title",
                [],
            )
            .unwrap();
            assert!(store.list().is_err());
            conn.execute("ALTER TABLE events RENAME COLUMN session_id TO broken", [])
                .unwrap();
            assert!(store.load_events("missing").is_err());
            assert!(store.history_for(&snapshot("s", "10", ".")).is_err());
        }

        #[test]
        fn corrupt_event_json_is_tolerated_and_history_falls_back() {
            let dir = TestDir::new();
            let path = dir.join("nested/sessions.db");
            let store = SqliteStore::open(&path).unwrap();
            let s = snapshot("s", "10", ".");
            store.save(&s).unwrap();
            store
                .append_event("s", &prompt("will be corrupted"))
                .unwrap();
            store.flush().unwrap();
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute("UPDATE events SET data = 'invalid json'", [])
                .unwrap();
            assert!(store.load_events("s").unwrap().unwrap().is_empty());
            assert_eq!(
                store.history_for(&s).unwrap()[0].text(),
                s.messages[0].text()
            );
        }

        #[test]
        fn sqlite_open_rejects_bad_paths_and_corrupt_databases() {
            let dir = TestDir::new();
            assert!(SqliteStore::open(&dir.0).is_err());
            let file = dir.0.join("not-a-directory");
            std::fs::write(&file, "not a database").unwrap();
            assert!(SqliteStore::open(file.join("db")).is_err());
            assert!(SqliteStore::open(&file).is_err());
        }

        #[test]
        fn infallible_constructor_retains_startup_failure_for_all_operations() {
            let dir = TestDir::new();
            let store = SqliteStore::at_default_path(dir.0.clone());
            assert!(store.save(&snapshot("s", "10", ".")).is_err());
            assert!(store.load("s").is_err());
            assert!(store.latest_in(".").is_err());
            assert!(store.list().is_err());
            assert!(store.load_events("s").is_err());
            assert!(store.append_event("s", &prompt("lost")).is_err());
            assert!(store.flush().is_err());
        }

        #[test]
        fn sqlite_flush_retains_background_write_failure() {
            let dir = TestDir::new();
            let path = dir.join("nested/sessions.db");
            let store = SqliteStore::open(&path).unwrap();
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TRIGGER reject_event BEFORE INSERT ON events
                     BEGIN SELECT RAISE(FAIL, 'injected event failure'); END;",
            )
            .unwrap();
            store.append_event("s", &prompt("rejected")).unwrap();
            let error = store.flush().unwrap_err().to_string();
            assert!(error.contains("injected event failure"), "{error}");
            conn.execute_batch("DROP TRIGGER reject_event;").unwrap();
            store.append_event("s", &prompt("persisted")).unwrap();
            assert!(store.flush().is_err());
            assert_eq!(
                serde_json::to_value(store.load_events("s").unwrap().unwrap()).unwrap(),
                serde_json::to_value([prompt("persisted")]).unwrap()
            );
        }
    }

    mod event_durability {
        use super::*;

        #[test]
        fn independent_writers_assign_unique_sequences() {
            let dir = TestDir::new();
            let path = dir.join("nested/sessions.db");
            let first = SqliteStore::open(&path).unwrap();
            let second = SqliteStore::open(&path).unwrap();
            for i in 0..50 {
                first
                    .append_event("s", &prompt(&format!("first {i}")))
                    .unwrap();
                second
                    .append_event("s", &prompt(&format!("second {i}")))
                    .unwrap();
            }
            first.flush().unwrap();
            second.flush().unwrap();
            let events = first.load_events("s").unwrap().unwrap();
            let texts: Vec<_> = events
                .iter()
                .map(|event| match event {
                    AgentEvent::UserPrompt { agent_id, text } => {
                        assert_eq!(agent_id, "root");
                        text.clone()
                    }
                    other => panic!("unexpected event: {other:?}"),
                })
                .collect();
            for writer in ["first", "second"] {
                assert_eq!(
                    texts
                        .iter()
                        .filter(|text| text.starts_with(writer))
                        .cloned()
                        .collect::<Vec<_>>(),
                    (0..50).map(|i| format!("{writer} {i}")).collect::<Vec<_>>()
                );
            }
            let connection = first.connection().unwrap();
            let mut statement = connection
                .prepare("SELECT seq FROM events WHERE session_id = 's' ORDER BY seq")
                .unwrap();
            let sequences = statement
                .query_map([], |row| row.get::<_, i64>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(sequences, (0..100).collect::<Vec<_>>());
        }

        #[test]
        fn dropping_writer_drains_pending_events_and_joins() {
            let dir = TestDir::new();
            let path = dir.join("nested/sessions.db");
            {
                let store = SqliteStore::open(&path).unwrap();
                for i in 0..20 {
                    store.append_event("s", &prompt(&format!("{i}"))).unwrap();
                }
            }
            let store = SqliteStore::open(&path).unwrap();
            assert_eq!(
                serde_json::to_value(store.load_events("s").unwrap().unwrap()).unwrap(),
                serde_json::to_value((0..20).map(|i| prompt(&i.to_string())).collect::<Vec<_>>())
                    .unwrap()
            );
        }
    }

    mod working_recovery {
        use super::*;

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

        #[test]
        fn sqlite_working_history_rejects_corrupt_or_missing_compaction_rows() {
            for mutation in [
                "UPDATE events SET data = 'invalid json' WHERE seq = 2",
                "DELETE FROM events WHERE seq = 2",
                "UPDATE events SET data = json_set(data, '$.v', 999) WHERE seq = 2",
            ] {
                let dir = TestDir::new();
                let store = SqliteStore::open(dir.join("nested/sessions.db")).unwrap();
                let events = [
                    prompt("a"),
                    prompt("b"),
                    compaction("first", 2),
                    prompt("c"),
                    compaction("second", 2),
                ];
                let mut saved =
                    session::new_session("mock", "corrupt".into(), "0".into(), ".".into());
                saved.messages = session::root_history_from_events(&events);
                store.save(&saved).unwrap();
                for event in events {
                    store.append_event(&saved.id, &event).unwrap();
                }
                store.flush().unwrap();
                assert_messages(
                    &store.working_history_for(&saved).unwrap(),
                    &[Message::user_text("[conversation summary]\nsecond")],
                );
                store.connection().unwrap().execute(mutation, []).unwrap();
                assert_messages(&store.history_for(&saved).unwrap(), &saved.messages);
                assert_messages(&store.working_history_for(&saved).unwrap(), &saved.messages);
            }
        }
    }
}
