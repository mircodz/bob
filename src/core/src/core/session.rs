//! Session persistence, backed by SQLite at $HOME/.bob/bob.db. Each session is
//! stored as a JSON blob in a row, with id/updated_at/title/provider/message
//! count pulled out as columns for fast listing. `--resume` loads one (or the
//! most recent) so a conversation can continue.

use crate::core::permissions::Grant;
use crate::core::types::{Message, Role};
use crate::core::usage::UsageEntry;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub provider: String,
    pub messages: Vec<Message>,
    /// "Always allow" grants the user made during this session.
    #[serde(default)]
    pub grants: Vec<Grant>,
    /// Per-completion token usage recorded during this session.
    #[serde(default)]
    pub usage: Vec<UsageEntry>,
    /// Subagent runs (the `task` tool's children), keyed by the task tool_use_id.
    /// Subagent tool calls aren't part of the root message history, so they're
    /// stored separately here to survive a restart.
    #[serde(default)]
    pub subagent_runs: Vec<SubagentRun>,
    /// The agent's todo list, persisted so the sticky panel survives a resume.
    #[serde(default)]
    pub todos: Vec<crate::tools::todo::TodoItem>,
}

/// All subagents spawned by one `task` tool call.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubagentRun {
    /// The `task` tool_use_id this run belongs to.
    pub task_use_id: String,
    pub subagents: Vec<PersistedSubagent>,
}

/// A single persisted subagent and the tools it invoked.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedSubagent {
    pub id: String,   // "task_N"
    pub task: String, // description
    pub tools: Vec<PersistedTool>,
}

/// A persisted subagent tool call with its input and output.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedTool {
    pub id: String, // tool_use_id
    pub name: String,
    pub input: Value,
    #[serde(default)]
    pub output: String,
    #[serde(default)]
    pub is_error: bool,
}

/// Path to the SQLite database file ($HOME/.bob/bob.db).
fn db_path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".bob").join("bob.db")
}

/// Open the database, creating the directory + schema if needed.
fn open_db() -> anyhow::Result<Connection> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(&path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            id           TEXT PRIMARY KEY,
            updated_at   TEXT NOT NULL,
            created_at   TEXT NOT NULL,
            title        TEXT NOT NULL,
            provider     TEXT NOT NULL,
            message_count INTEGER NOT NULL,
            data         TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_sessions_updated ON sessions(updated_at DESC);",
    )?;
    Ok(conn)
}

pub fn new_session(provider: &str, id: String, now: String) -> Session {
    Session {
        id,
        created_at: now.clone(),
        updated_at: now,
        provider: provider.to_string(),
        messages: vec![],
        grants: vec![],
        usage: vec![],
        subagent_runs: vec![],
        todos: vec![],
    }
}

pub fn save_session(s: &Session) -> anyhow::Result<()> {
    let conn = open_db()?;
    let data = serde_json::to_string(s)?;
    conn.execute(
        "INSERT INTO sessions (id, updated_at, created_at, title, provider, message_count, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET
            updated_at=excluded.updated_at,
            title=excluded.title,
            provider=excluded.provider,
            message_count=excluded.message_count,
            data=excluded.data",
        rusqlite::params![
            s.id,
            s.updated_at,
            s.created_at,
            title_of(s),
            s.provider,
            s.messages.len() as i64,
            data,
        ],
    )?;
    Ok(())
}

pub fn load_session(id: &str) -> anyhow::Result<Option<Session>> {
    let conn = open_db()?;
    let data: Option<String> = conn
        .query_row("SELECT data FROM sessions WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .ok();
    match data {
        Some(json) => Ok(Some(serde_json::from_str(&json)?)),
        None => Ok(None),
    }
}

/// Find the most recently updated session (for `--resume` with no id).
pub fn latest_session() -> anyhow::Result<Option<Session>> {
    let conn = open_db()?;
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM sessions ORDER BY updated_at DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    match data {
        Some(json) => Ok(Some(serde_json::from_str(&json)?)),
        None => Ok(None),
    }
}

/// Lightweight, transport-agnostic metadata about a stored session — enough to
/// render a picker (TUI) or a drawer (remote app) without loading full history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    /// Human title: the first user message, trimmed (or "New conversation").
    pub title: String,
    pub provider: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: usize,
}

impl SessionSummary {
    pub fn of(s: &Session) -> Self {
        SessionSummary {
            id: s.id.clone(),
            title: title_of(s),
            provider: s.provider.clone(),
            created_at: s.created_at.clone(),
            updated_at: s.updated_at.clone(),
            message_count: s.messages.len(),
        }
    }
}

/// A short display title for a session: its first user-message text, collapsed
/// to one line and truncated. Falls back to "New conversation" when empty.
pub fn title_of(session: &Session) -> String {
    for m in &session.messages {
        if m.role == Role::User {
            let t = m.text();
            let t = t.trim();
            if !t.is_empty() {
                let one_line = t.replace('\n', " ");
                return if one_line.chars().count() <= 48 {
                    one_line
                } else {
                    let cut: String = one_line.chars().take(48).collect();
                    format!("{cut}…")
                };
            }
        }
    }
    "New conversation".to_string()
}

/// Enumerate all stored sessions as summaries, most-recently-updated first.
/// Reads the indexed columns directly — no need to parse each full blob. Both
/// the TUI picker and the remote drawer use this so local and remote see one
/// identical list.
pub fn list_sessions() -> Vec<SessionSummary> {
    let conn = match open_db() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT id, title, provider, created_at, updated_at, message_count
         FROM sessions ORDER BY updated_at DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let rows = stmt.query_map([], |r| {
        Ok(SessionSummary {
            id: r.get(0)?,
            title: r.get(1)?,
            provider: r.get(2)?,
            created_at: r.get(3)?,
            updated_at: r.get(4)?,
            message_count: r.get::<_, i64>(5)? as usize,
        })
    });
    match rows {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}
