//! Conversation-session helpers for the remote host. Thin wrappers over
//! bob-core's session store (`~/.bob/sessions/*.json`) — the SAME store and
//! format the TUI uses, so local and remote conversations are interchangeable.
//! Listing/metadata now lives in core (`list_sessions`, `SessionSummary`); this
//! module only adds host-lifecycle conveniences.

use bob_core::core::session::{list_sessions, load_session, new_session, save_session, Session};
use bob_core::core::types::Message;
use bob_protocol::SessionMeta;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

pub fn make_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Create a fresh, empty conversation session for the host's working directory.
pub fn fresh(provider: &str) -> Session {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    new_session(provider, make_id(), now_stamp(), cwd)
}

/// Load the most-recently-updated session, or create a fresh one if none exist.
/// Uses core's canonical listing so it matches the TUI's `latest_session`.
pub fn latest_or_new(provider: &str) -> Session {
    match list_sessions().first().and_then(|s| load(&s.id)) {
        Some(s) => s,
        None => fresh(provider),
    }
}

/// Load a session by id.
pub fn load(id: &str) -> Option<Session> {
    load_session(id).ok().flatten()
}

/// The message history to seed the agent + greet the controller with, preferring
/// the event log (the single source of truth) over the stored blob. Falls back to
/// the blob when the log is absent (legacy sessions) or reconstructs FEWER
/// messages than the blob — a truncated/corrupt log must never lose history.
/// Mirrors the TUI's replay-with-guard on resume.
pub fn history_for(session: &Session) -> Vec<Message> {
    let events = bob_core::core::session::load_events(&session.id)
        .ok()
        .flatten();
    bob_core::core::session::reconstructed_history(events.as_deref(), &session.messages)
}

/// Persist a session's current messages under its id, bumping updated_at.
pub fn persist(session: &mut Session, messages: Vec<Message>) {
    session.messages = messages;
    session.updated_at = now_stamp();
    if let Err(e) = save_session(session) {
        eprintln!("[host] failed to save session {}: {e}", session.id);
    }
}

/// Enumerate all stored sessions as wire metadata (newest first), delegating to
/// core so the drawer shows exactly what the TUI picker shows.
pub fn list_all() -> Vec<SessionMeta> {
    list_sessions().iter().map(SessionMeta::from).collect()
}
