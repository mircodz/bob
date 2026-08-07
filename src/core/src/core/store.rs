//! The persistence seam: a [`SessionStore`] trait plus the default
//! [`SqliteStore`] backed by `~/.bob/bob.db`.
//!
//! Persistence used to be a bag of free functions in [`super::session`] that
//! every caller invoked directly, each opening its own SQLite connection. That
//! made storage un-swappable — a test, an in-memory backend, or an alternate
//! path had no seam to plug into. This trait is that seam. `SqliteStore`
//! delegates to the existing, battle-tested `session::*` functions (event log +
//! blob + `reconstructed_history`), so this is a *wrap*, not a rewrite: identical
//! on-disk behavior, now behind an interface the SDK builder can accept.

use super::session::{self, EventLogWriter, Session, SessionSummary};
use crate::core::events::AgentEvent;
use crate::core::types::Message;
use std::sync::Arc;

/// A conversation store: create/load/save sessions, append events to the
/// append-only log, and reconstruct history on resume. The event log is the
/// source of truth; the blob is a derived read-model kept for fast listing and
/// as a fallback when a log is truncated.
pub trait SessionStore: Send + Sync {
    /// Persist a session's current state (blob). Best-effort durability; the
    /// authoritative per-event record is written via [`append_event`].
    fn save(&self, session: &Session) -> anyhow::Result<()>;

    /// Load a session by id (blob), or `None` if it doesn't exist.
    fn load(&self, id: &str) -> anyhow::Result<Option<Session>>;

    /// The most-recently-updated session in a given working directory, if any.
    fn latest_in(&self, cwd: &str) -> anyhow::Result<Option<Session>>;

    /// List stored sessions (newest first) as lightweight summaries.
    fn list(&self) -> Vec<SessionSummary>;

    /// Append one event to the session's append-only log. Non-blocking: the
    /// write happens on a dedicated thread so the agent loop never stalls on IO.
    fn append_event(&self, session_id: &str, event: &AgentEvent);

    /// Block until every queued event is durable. Call at turn-end / quit.
    fn flush(&self);

    /// The message history to seed an agent on resume: the reconstructed log when
    /// present (source of truth), falling back to the blob when the log is absent
    /// or shorter (a truncated/corrupt log must never lose history).
    fn history_for(&self, session: &Session) -> Vec<Message> {
        let events = self.load_events(&session.id);
        session::reconstructed_history(events.as_deref(), &session.messages)
    }

    /// Raw event log for a session, or `None` for a legacy (log-less) session.
    fn load_events(&self, session_id: &str) -> Option<Vec<AgentEvent>>;
}

/// The default store: the shared SQLite database at `~/.bob/bob.db`. Owns the
/// background [`EventLogWriter`] thread so a single instance serializes all event
/// appends, exactly as the TUI did inline before.
pub struct SqliteStore {
    writer: Arc<EventLogWriter>,
}

impl Default for SqliteStore {
    fn default() -> Self {
        Self {
            writer: Arc::new(EventLogWriter::spawn()),
        }
    }
}

impl SqliteStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionStore for SqliteStore {
    fn save(&self, session: &Session) -> anyhow::Result<()> {
        session::save_session(session)
    }

    fn load(&self, id: &str) -> anyhow::Result<Option<Session>> {
        session::load_session(id)
    }

    fn latest_in(&self, cwd: &str) -> anyhow::Result<Option<Session>> {
        session::latest_session_in(cwd)
    }

    fn list(&self) -> Vec<SessionSummary> {
        session::list_sessions()
    }

    fn append_event(&self, session_id: &str, event: &AgentEvent) {
        self.writer.append(session_id, event);
    }

    fn flush(&self) {
        self.writer.flush();
    }

    fn load_events(&self, session_id: &str) -> Option<Vec<AgentEvent>> {
        session::load_events(session_id).ok().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::Role;
    use std::sync::Mutex;

    /// An in-memory store — the whole point of the trait. Proves a non-SQLite
    /// backend satisfies the seam (used by future agent end-to-end tests without
    /// touching disk).
    #[derive(Default)]
    struct MemStore {
        sessions: Mutex<std::collections::HashMap<String, Session>>,
        events: Mutex<std::collections::HashMap<String, Vec<AgentEvent>>>,
    }

    impl SessionStore for MemStore {
        fn save(&self, session: &Session) -> anyhow::Result<()> {
            self.sessions
                .lock()
                .unwrap()
                .insert(session.id.clone(), session.clone());
            Ok(())
        }
        fn load(&self, id: &str) -> anyhow::Result<Option<Session>> {
            Ok(self.sessions.lock().unwrap().get(id).cloned())
        }
        fn latest_in(&self, _cwd: &str) -> anyhow::Result<Option<Session>> {
            Ok(self.sessions.lock().unwrap().values().next().cloned())
        }
        fn list(&self) -> Vec<SessionSummary> {
            Vec::new()
        }
        fn append_event(&self, session_id: &str, event: &AgentEvent) {
            self.events
                .lock()
                .unwrap()
                .entry(session_id.to_string())
                .or_default()
                .push(event.clone());
        }
        fn flush(&self) {}
        fn load_events(&self, session_id: &str) -> Option<Vec<AgentEvent>> {
            self.events.lock().unwrap().get(session_id).cloned()
        }
    }

    #[test]
    fn in_memory_store_round_trips_and_reconstructs_history() {
        let store = MemStore::default();
        let mut s = session::new_session("mock", "s1".into(), "0".into(), ".".into());
        s.messages.push(Message {
            role: Role::User,
            content: vec![],
        });
        store.save(&s).unwrap();
        assert!(store.load("s1").unwrap().is_some());
        // With no events, history_for falls back to the blob (1 message).
        assert_eq!(store.history_for(&s).len(), 1);
        // Appended events are retrievable through the same seam.
        store.append_event(
            "s1",
            &AgentEvent::UserPrompt {
                agent_id: "root".into(),
                text: "hi".into(),
            },
        );
        assert_eq!(store.load_events("s1").unwrap().len(), 1);
    }
}
