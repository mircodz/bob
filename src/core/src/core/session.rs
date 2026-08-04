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
    /// The working directory this session was started in. Used to scope
    /// `--resume`/`--continue` to the current project. Empty for old sessions
    /// saved before this field existed.
    #[serde(default)]
    pub cwd: String,
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
    /// Per-agent drawer transcripts (the team drawer), so they survive a resume.
    #[serde(default)]
    pub agent_threads: Vec<PersistedThread>,
}

/// One spawned agent's persisted transcript for the team drawer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedThread {
    pub id: String,
    pub name: String,
    pub parent_id: String,
    pub task: String,
    /// "running" | "done" | "failed".
    pub status: String,
    pub cells: Vec<PersistedCell>,
}

/// A render-agnostic transcript cell (the persisted form of the drawer's `Cell`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistedCell {
    /// A user-style input line (the delegated prompt seeded into a subagent's
    /// thread), rendered like the main view's user input.
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Tool {
        name: String,
        input: Value,
        output: String,
        is_error: bool,
    },
    Message {
        from: String,
        text: String,
    },
    Notice {
        text: String,
    },
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
    // Migration: add the `cwd` column to pre-existing databases (a fresh CREATE
    // above doesn't include it). Ignore the error if it already exists.
    let _ = conn.execute(
        "ALTER TABLE sessions ADD COLUMN cwd TEXT NOT NULL DEFAULT ''",
        [],
    );
    Ok(conn)
}

pub fn new_session(provider: &str, id: String, now: String, cwd: String) -> Session {
    Session {
        id,
        created_at: now.clone(),
        updated_at: now,
        provider: provider.to_string(),
        cwd,
        messages: vec![],
        grants: vec![],
        usage: vec![],
        subagent_runs: vec![],
        todos: vec![],
        agent_threads: vec![],
    }
}

pub fn save_session(s: &Session) -> anyhow::Result<()> {
    let conn = open_db()?;
    let data = serde_json::to_string(s)?;
    conn.execute(
        "INSERT INTO sessions (id, updated_at, created_at, title, provider, message_count, cwd, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
            updated_at=excluded.updated_at,
            title=excluded.title,
            provider=excluded.provider,
            message_count=excluded.message_count,
            cwd=excluded.cwd,
            data=excluded.data",
        rusqlite::params![
            s.id,
            s.updated_at,
            s.created_at,
            title_of(s),
            s.provider,
            s.messages.len() as i64,
            s.cwd,
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
    /// The directory the session was started in (empty for legacy sessions).
    #[serde(default)]
    pub cwd: String,
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
    query_summaries(None)
}

/// Like [`list_sessions`], but only sessions started in `cwd` (scopes the picker
/// to the current project). An empty `cwd` returns everything.
pub fn list_sessions_in(cwd: &str) -> Vec<SessionSummary> {
    if cwd.is_empty() {
        list_sessions()
    } else {
        query_summaries(Some(cwd))
    }
}

fn query_summaries(cwd: Option<&str>) -> Vec<SessionSummary> {
    let conn = match open_db() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let map_row = |r: &rusqlite::Row| {
        Ok(SessionSummary {
            id: r.get(0)?,
            title: r.get(1)?,
            provider: r.get(2)?,
            created_at: r.get(3)?,
            updated_at: r.get(4)?,
            message_count: r.get::<_, i64>(5)? as usize,
            cwd: r.get::<_, String>(6).unwrap_or_default(),
        })
    };
    let base =
        "SELECT id, title, provider, created_at, updated_at, message_count, cwd FROM sessions";
    let rows: Result<Vec<SessionSummary>, _> = match cwd {
        Some(dir) => {
            let sql = format!("{base} WHERE cwd = ?1 ORDER BY updated_at DESC");
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let it = match stmt.query_map([dir], map_row) {
                Ok(it) => it,
                Err(_) => return Vec::new(),
            };
            it.collect()
        }
        None => {
            let sql = format!("{base} ORDER BY updated_at DESC");
            let mut stmt = match conn.prepare(&sql) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let it = match stmt.query_map([], map_row) {
                Ok(it) => it,
                Err(_) => return Vec::new(),
            };
            it.collect()
        }
    };
    rows.unwrap_or_default()
}

/// The most recently updated session started in `cwd` (for `--continue`). None if
/// there is no session for this directory.
pub fn latest_session_in(cwd: &str) -> anyhow::Result<Option<Session>> {
    let conn = open_db()?;
    let data: Option<String> = conn
        .query_row(
            "SELECT data FROM sessions WHERE cwd = ?1 ORDER BY updated_at DESC LIMIT 1",
            [cwd],
            |r| r.get(0),
        )
        .ok();
    match data {
        Some(json) => Ok(Some(serde_json::from_str(&json)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_threads_round_trip() {
        let session = Session {
            agent_threads: vec![PersistedThread {
                id: "reviewer".into(),
                name: "reviewer".into(),
                parent_id: "root".into(),
                task: "review the diff".into(),
                status: "done".into(),
                cells: vec![
                    PersistedCell::Assistant {
                        text: "looks good".into(),
                    },
                    PersistedCell::Tool {
                        name: "grep".into(),
                        input: serde_json::json!({"pattern": "x"}),
                        output: "3 matches".into(),
                        is_error: false,
                    },
                    PersistedCell::Message {
                        from: "user".into(),
                        text: "also check y".into(),
                    },
                ],
            }],
            ..new_session("anthropic", "s1".into(), "now".into(), String::new())
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_threads.len(), 1);
        let t = &back.agent_threads[0];
        assert_eq!(t.name, "reviewer");
        assert_eq!(t.cells.len(), 3);
    }

    #[test]
    fn old_session_without_agent_threads_loads() {
        // A session JSON from before the field existed must still deserialize
        // (serde default → empty vec).
        let json = r#"{
            "id": "s1", "created_at": "t", "updated_at": "t",
            "provider": "anthropic", "messages": []
        }"#;
        let s: Session = serde_json::from_str(json).unwrap();
        assert!(s.agent_threads.is_empty());
        assert!(s.todos.is_empty());
        // Legacy sessions predate `cwd` → defaults to empty (so they show in the
        // unscoped picker, never filtered out).
        assert_eq!(s.cwd, "");
    }

    #[test]
    fn new_session_records_cwd() {
        let s = new_session("openai", "s2".into(), "now".into(), "/home/x/proj".into());
        assert_eq!(s.cwd, "/home/x/proj");
    }
}
