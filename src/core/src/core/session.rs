//! Session persistence, backed by SQLite at $HOME/.bob/bob.db. Each session is
//! stored as a JSON blob in a row, with id/updated_at/title/provider/message
//! count pulled out as columns for fast listing. `--resume` loads one (or the
//! most recent) so a conversation can continue.

use crate::core::events::AgentEvent;
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
    /// Workflow runs (the `/workflow` tree cells in the main transcript). Stored
    /// separately because they're built from live events, not the message history,
    /// so they'd otherwise vanish on resume. Anchored to their hand-off message.
    #[serde(default)]
    pub workflows: Vec<PersistedWorkflow>,
}

/// A persisted workflow run: its id/title and phases + agents. On resume the tree
/// is re-inserted just before its `[workflow result]` hand-off message; runs are
/// matched to hand-off messages in order, so no explicit index is stored. `done`
/// is implied — a persisted run is always finished.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedWorkflow {
    pub id: String,
    pub title: String,
    pub phases: Vec<PersistedWfPhase>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedWfPhase {
    pub title: String,
    pub index: usize,
    pub total: usize,
    pub agents: Vec<PersistedWfAgent>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedWfAgent {
    pub agent_id: String,
    pub label: String,
    /// "running" | "done" | "failed".
    pub status: String,
    pub tools: usize,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub tokens: u64,
    #[serde(default)]
    pub duration_secs: Option<u64>,
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
    // Concurrency hardening: multiple `bob` instances (a TUI + `bob remote`, or two
    // `--continue` in the same dir) can hold the same session id. WAL lets readers
    // and a writer coexist; busy_timeout makes a contended write wait rather than
    // fail with SQLITE_BUSY. synchronous=NORMAL is the safe WAL pairing.
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA synchronous=NORMAL;",
    )
    .ok();
    init_schema(&conn)?;
    Ok(conn)
}

/// Create the `sessions` table + index if absent, and migrate the `cwd` column onto
/// pre-existing databases. Split out of [`open_db`] so tests can build the same
/// schema on an in-memory connection.
fn init_schema(conn: &Connection) -> anyhow::Result<()> {
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
    // Append-only event log: the emerging single source of truth for a session's
    // transcript. Each row is one serialized `AgentEvent` in a versioned envelope,
    // ordered by a per-session monotonic `seq`. Stage 1 only shadow-writes here; the
    // blob in `sessions.data` is still authoritative until replay is proven.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS events (
            session_id TEXT NOT NULL,
            seq        INTEGER NOT NULL,
            ts         INTEGER NOT NULL,
            kind       TEXT NOT NULL,
            data       TEXT NOT NULL,
            PRIMARY KEY (session_id, seq)
         ) WITHOUT ROWID;",
    )?;
    // Reserved header cursor (−1 = none). Not currently read — seq is derived from
    // MAX(seq) over the log so a new session's pre-blob events don't collide — but
    // kept as a cheap future cursor (dropping a deployed SQLite column is
    // migration-hostile). Left unwritten on purpose.
    let _ = conn.execute(
        "ALTER TABLE sessions ADD COLUMN last_seq INTEGER NOT NULL DEFAULT -1",
        [],
    );
    Ok(())
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
        workflows: vec![],
    }
}

/// Persist a session. Refuses to let a SHORTER history overwrite a longer one for
/// the same id — the exact stale-clobber that erased a user's MCP turns (a second
/// `bob` on the same id saving its older, pre-MCP snapshot over the newer row).
/// Growing or same-length writes always win; shrinking writes are dropped. Use
/// [`save_session_force`] for intentional truncation (context-clear / new session).
pub fn save_session(s: &Session) -> anyhow::Result<()> {
    write_session(s, false)
}

/// Persist a session, ALLOWING the message count to shrink. Only for intentional
/// resets (ClearContext) where a shorter history is the desired state.
pub fn save_session_force(s: &Session) -> anyhow::Result<()> {
    write_session(s, true)
}

fn write_session(s: &Session, allow_shrink: bool) -> anyhow::Result<()> {
    let conn = open_db()?;
    write_session_to(&conn, s, allow_shrink)
}

/// The UPSERT itself, taking an explicit connection so it can be exercised against
/// an in-memory DB in tests. The UPDATE's WHERE clause is the anti-clobber guard:
/// unless forced, only overwrite when the incoming history is at least as long as
/// what's on disk. Evaluated inside the single UPSERT, so it's atomic against a
/// concurrent writer.
fn write_session_to(conn: &Connection, s: &Session, allow_shrink: bool) -> anyhow::Result<()> {
    let data = serde_json::to_string(s)?;
    let guard = if allow_shrink {
        ""
    } else {
        "WHERE excluded.message_count >= sessions.message_count"
    };
    let sql = format!(
        "INSERT INTO sessions (id, updated_at, created_at, title, provider, message_count, cwd, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
            updated_at=excluded.updated_at,
            title=excluded.title,
            provider=excluded.provider,
            message_count=excluded.message_count,
            cwd=excluded.cwd,
            data=excluded.data
         {guard}"
    );
    conn.execute(
        &sql,
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

/// Current schema version for the event envelope. Bumped only when a *payload*
/// shape changes in a way replay must migrate; additive fields don't need it
/// (serde defaults handle those).
const EVENT_ENVELOPE_VERSION: u32 = 1;

/// The on-disk form of one logged event: a version tag wrapping the flattened
/// `AgentEvent` (itself internally-tagged with `kind`). The `v` gives future
/// builds a hook to transform old payloads before deserializing.
#[derive(Serialize, Deserialize)]
struct EventEnvelope {
    v: u32,
    #[serde(flatten)]
    event: AgentEvent,
}

/// Append one event to a session's log, assigning it the next monotonic `seq`.
/// `ts` is supplied by the caller (unix millis) so this stays pure/testable.
/// Returns the assigned seq.
///
/// Callers append here in addition to the blob save (the log is the source of
/// truth on resume; the blob is a derived read-model). The single writer task
/// owns the connection, so `seq` assignment races nothing.
pub fn append_event_to(
    conn: &Connection,
    session_id: &str,
    ts: i64,
    event: &AgentEvent,
) -> anyhow::Result<i64> {
    // Derive the next seq from the log itself: a new session logs its first events
    // BEFORE the blob save creates its sessions row, so a header-based cursor would
    // make every event collide on seq 0. MAX(seq) over the events table is always
    // correct regardless of row/write ordering.
    let last: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), -1) FROM events WHERE session_id = ?1",
            [session_id],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let seq = last + 1;
    let envelope = EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        event: event.clone(),
    };
    let data = serde_json::to_string(&envelope)?;
    conn.execute(
        "INSERT INTO events (session_id, seq, ts, kind, data) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![session_id, seq, ts, event.kind(), data],
    )?;
    Ok(seq)
}

/// Replay a session's event log in order, returning the reconstructed events.
/// Rows that fail to decode (a corrupt or far-future payload) are skipped rather
/// than aborting the whole replay — a resumed session should degrade, never fail.
pub fn replay_events(conn: &Connection, session_id: &str) -> anyhow::Result<Vec<AgentEvent>> {
    let mut stmt =
        conn.prepare("SELECT data FROM events WHERE session_id = ?1 ORDER BY seq ASC")?;
    let rows = stmt.query_map([session_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        let json = row?;
        if let Ok(env) = serde_json::from_str::<EventEnvelope>(&json) {
            out.push(env.event);
        }
    }
    Ok(out)
}

/// Whether a session has any events logged yet (used to choose replay vs. the
/// legacy blob path on load).
pub fn has_events(conn: &Connection, session_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM events WHERE session_id = ?1 LIMIT 1",
        [session_id],
        |_| Ok(()),
    )
    .is_ok()
}

/// Load a session's full event log, opening the database. Returns `None` when the
/// session has no events yet (a legacy blob-only session), so callers can fall
/// back to blob hydration. This is the resume entry point for the replay path.
pub fn load_events(session_id: &str) -> anyhow::Result<Option<Vec<AgentEvent>>> {
    let conn = open_db()?;
    if !has_events(&conn, session_id) {
        return Ok(None);
    }
    Ok(Some(replay_events(&conn, session_id)?))
}

/// Reconstruct the ROOT agent's full message history from its event log, mirroring
/// exactly what [`crate::agent::Agent::run`] pushes into `full_history`:
///
/// - `UserPrompt` → a user text message
/// - `AgentMessage` delivered to root → a folded coordination user message
/// - `Message` (root) → the assistant turn verbatim (carries any tool_use blocks)
/// - consecutive root `ToolResult`s → one `Role::Tool` message bundling them
///
/// Subagent events (agent_id != "root") never touch root history — they're the
/// team-drawer's concern. This is the seed the provider needs on resume, derived
/// from the log instead of the separately-stored `messages` blob.
pub fn root_history_from_events(events: &[AgentEvent]) -> Vec<Message> {
    let mut msgs: Vec<Message> = Vec::new();
    let mut pending_results: Vec<crate::core::types::ContentBlock> = Vec::new();

    // Flush any buffered tool_results as a single Role::Tool message. Called before
    // appending a non-result message so ordering matches the live turn loop.
    fn flush(msgs: &mut Vec<Message>, pending: &mut Vec<crate::core::types::ContentBlock>) {
        if !pending.is_empty() {
            msgs.push(Message {
                role: Role::Tool,
                content: std::mem::take(pending),
            });
        }
    }

    for e in events {
        match e {
            AgentEvent::UserPrompt { agent_id, text } if agent_id == "root" => {
                flush(&mut msgs, &mut pending_results);
                msgs.push(Message::user_text(text.clone()));
            }
            AgentEvent::AgentMessage { to, from, text } if to == "root" => {
                flush(&mut msgs, &mut pending_results);
                msgs.push(Message::user_text(
                    crate::agent::team::format_coord_message(from, text),
                ));
            }
            AgentEvent::Message { agent_id, message } if agent_id == "root" => {
                flush(&mut msgs, &mut pending_results);
                msgs.push(message.clone());
            }
            AgentEvent::ToolResult {
                agent_id,
                tool_use_id,
                output,
                is_error,
            } if agent_id == "root" => {
                pending_results.push(crate::core::types::ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: output.clone(),
                    is_error: Some(*is_error),
                });
            }
            _ => {}
        }
    }
    flush(&mut msgs, &mut pending_results);
    msgs
}

/// Choose the authoritative message history for a resumed session: the event-log
/// reconstruction when it's at least as complete as the stored blob, else the
/// blob. Centralizes the "a truncated/corrupt log must never lose history" guard
/// that both the TUI and the remote host apply on resume, so the rule can't drift
/// between them. `events` is `None` for a legacy (pre-log) session.
pub fn reconstructed_history(events: Option<&[AgentEvent]>, blob: &[Message]) -> Vec<Message> {
    match events {
        Some(evs) => {
            let rebuilt = root_history_from_events(evs);
            if rebuilt.len() >= blob.len() {
                rebuilt
            } else {
                blob.to_vec()
            }
        }
        None => blob.to_vec(),
    }
}

/// A single-writer sink for the event log. One background thread owns a DB
/// connection and drains a channel; callers (a bus listener on the async runtime)
/// only `send`, so appends never block the runtime on a SQLite fsync. Being the
/// sole writer also means `seq` assignment races nothing.
pub struct EventLogWriter {
    tx: std::sync::mpsc::Sender<WriterMsg>,
}

enum WriterMsg {
    Append {
        session_id: String,
        event: Box<AgentEvent>,
    },
    /// Barrier: append everything queued so far, then signal. Used to guarantee
    /// durability at turn-end / quit before the process may exit.
    Flush(std::sync::mpsc::SyncSender<()>),
}

impl EventLogWriter {
    /// Spawn the writer thread. The thread opens its own connection; if that
    /// fails, appends are silently dropped (the blob save still persists the
    /// session, so shadow-write failure is non-fatal in Stage 1).
    pub fn spawn() -> EventLogWriter {
        let (tx, rx) = std::sync::mpsc::channel::<WriterMsg>();
        std::thread::spawn(move || {
            let conn = match open_db() {
                Ok(c) => c,
                Err(_) => {
                    // Drain and discard so senders don't block on a full channel.
                    for msg in rx {
                        if let WriterMsg::Flush(ack) = msg {
                            let _ = ack.send(());
                        }
                    }
                    return;
                }
            };
            for msg in rx {
                match msg {
                    WriterMsg::Append { session_id, event } => {
                        let ts = now_millis();
                        let _ = append_event_to(&conn, &session_id, ts, &event);
                    }
                    WriterMsg::Flush(ack) => {
                        let _ = ack.send(());
                    }
                }
            }
        });
        EventLogWriter { tx }
    }

    /// Queue one event for the given session. Non-blocking; a dead writer thread
    /// (channel closed) is ignored.
    pub fn append(&self, session_id: &str, event: &AgentEvent) {
        let _ = self.tx.send(WriterMsg::Append {
            session_id: session_id.to_string(),
            event: Box::new(event.clone()),
        });
    }

    /// Block until every event queued before this call has been written. Call
    /// before the process may exit (quit, context-clear) so the log is durable.
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(0);
        if self.tx.send(WriterMsg::Flush(ack_tx)).is_ok() {
            let _ = ack_rx.recv();
        }
    }
}

/// Milliseconds since the Unix epoch (0 if the clock is before the epoch).
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

    // Build a session with `n` user messages under a fixed id.
    fn sized(id: &str, n: usize) -> Session {
        let mut s = new_session("anthropic", id.into(), "t".into(), String::new());
        s.messages = (0..n)
            .map(|i| Message::user_text(format!("m{i}")))
            .collect();
        s
    }

    fn count_on_disk(conn: &Connection, id: &str) -> i64 {
        conn.query_row(
            "SELECT message_count FROM sessions WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn shorter_history_cannot_clobber_longer() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        // A full 10-message history lands.
        write_session_to(&conn, &sized("s", 10), false).unwrap();
        assert_eq!(count_on_disk(&conn, "s"), 10);
        // A stale writer with only 3 messages (the exact MCP-clobber scenario) is
        // dropped — the longer history survives.
        write_session_to(&conn, &sized("s", 3), false).unwrap();
        assert_eq!(count_on_disk(&conn, "s"), 10);
        // Growing (or equal) writes still win.
        write_session_to(&conn, &sized("s", 12), false).unwrap();
        assert_eq!(count_on_disk(&conn, "s"), 12);
    }

    #[test]
    fn force_save_allows_intentional_shrink() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        write_session_to(&conn, &sized("s", 10), false).unwrap();
        // A context-clear (force) legitimately shrinks to 0.
        write_session_to(&conn, &sized("s", 0), true).unwrap();
        assert_eq!(count_on_disk(&conn, "s"), 0);
        // ...and afterwards normal growing saves resume from the reset baseline.
        write_session_to(&conn, &sized("s", 2), false).unwrap();
        assert_eq!(count_on_disk(&conn, "s"), 2);
    }

    #[test]
    fn events_append_and_replay_in_order() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        write_session_to(&conn, &sized("s", 1), false).unwrap();
        assert!(!has_events(&conn, "s"));

        let e0 = AgentEvent::TurnStart {
            agent_id: "root".into(),
        };
        let e1 = AgentEvent::TextDelta {
            agent_id: "root".into(),
            text: "hi".into(),
        };
        assert_eq!(append_event_to(&conn, "s", 100, &e0).unwrap(), 0);
        assert_eq!(append_event_to(&conn, "s", 101, &e1).unwrap(), 1);

        assert!(has_events(&conn, "s"));

        let replayed = replay_events(&conn, "s").unwrap();
        assert_eq!(replayed.len(), 2);
        assert!(matches!(replayed[0], AgentEvent::TurnStart { .. }));
        match &replayed[1] {
            AgentEvent::TextDelta { text, .. } => assert_eq!(text, "hi"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
    }

    #[test]
    fn compaction_event_round_trips_summary() {
        // The summary + split index must survive the log — they're what lets
        // replay rebuild the compacted working set on resume.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let ev = AgentEvent::Compaction {
            agent_id: "root".into(),
            before_tokens: 900,
            after_tokens: 300,
            summary: "we did X and Y".into(),
            replaced_upto: 7,
        };
        append_event_to(&conn, "s", 1, &ev).unwrap();
        match &replay_events(&conn, "s").unwrap()[0] {
            AgentEvent::Compaction {
                summary,
                replaced_upto,
                ..
            } => {
                assert_eq!(summary, "we did X and Y");
                assert_eq!(*replaced_upto, 7);
            }
            other => panic!("expected Compaction, got {other:?}"),
        }
    }

    #[test]
    fn unknown_future_variant_replays_as_noop() {
        // A row written by a newer bob (unrecognized `kind`) must not abort replay —
        // it decodes to AgentEvent::Unknown and is otherwise ignored.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        write_session_to(&conn, &sized("s", 1), false).unwrap();
        conn.execute(
            "INSERT INTO events (session_id, seq, ts, kind, data) VALUES ('s', 0, 1, 'FutureThing', ?1)",
            [r#"{"v":2,"kind":"FutureThing","payload":{"whatever":true}}"#],
        )
        .unwrap();
        let good = AgentEvent::TurnStart {
            agent_id: "root".into(),
        };
        append_event_to(&conn, "s", 2, &good).unwrap();
        let replayed = replay_events(&conn, "s").unwrap();
        assert_eq!(replayed.len(), 2);
        assert!(matches!(replayed[0], AgentEvent::Unknown));
        assert!(matches!(replayed[1], AgentEvent::TurnStart { .. }));
    }

    #[test]
    fn seq_is_derived_from_the_log_not_a_missing_header() {
        // Regression: a brand-new session logs events BEFORE its sessions row
        // exists. seq must come from MAX(seq) over events, or every event collides
        // on seq 0 and all but the first are silently dropped (the bug that made a
        // resumed transcript render almost nothing).
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        // NOTE: no sessions row yet.
        for i in 0..5 {
            append_event_to(
                &conn,
                "s",
                i,
                &AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: format!("m{i}"),
                },
            )
            .unwrap();
        }
        assert_eq!(replay_events(&conn, "s").unwrap().len(), 5);
    }

    #[test]
    fn root_history_reducer_mirrors_the_turn_loop() {
        use crate::core::types::ContentBlock;
        // A user turn, an assistant turn with a tool_use, its tool_result, then a
        // final assistant text — the reducer must yield exactly what run() pushes
        // into full_history, bundling the tool_result into one Role::Tool message.
        let assistant_with_tool = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read_file".into(),
                input: serde_json::json!({"path": "a.rs"}),
            }],
        };
        let events = vec![
            AgentEvent::UserPrompt {
                agent_id: "root".into(),
                text: "fix it".into(),
            },
            AgentEvent::Message {
                agent_id: "root".into(),
                message: assistant_with_tool.clone(),
            },
            AgentEvent::ToolResult {
                agent_id: "root".into(),
                tool_use_id: "t1".into(),
                output: "ok".into(),
                is_error: false,
            },
            // A subagent's result must NOT enter root history.
            AgentEvent::ToolResult {
                agent_id: "task_1".into(),
                tool_use_id: "x".into(),
                output: "ignored".into(),
                is_error: false,
            },
            AgentEvent::Message {
                agent_id: "root".into(),
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                },
            },
        ];
        let h = root_history_from_events(&events);
        assert_eq!(h.len(), 4);
        assert_eq!(h[0].role, Role::User);
        assert_eq!(h[0].text(), "fix it");
        assert_eq!(h[1].role, Role::Assistant);
        assert_eq!(h[2].role, Role::Tool);
        // Exactly one tool_result, and the subagent's was excluded.
        match &h[2].content[..] {
            [ContentBlock::ToolResult { tool_use_id, .. }] => assert_eq!(tool_use_id, "t1"),
            other => panic!("expected one root tool_result, got {other:?}"),
        }
        assert_eq!(h[3].text(), "done");
    }

    #[test]
    fn reconstructed_history_guards_against_a_short_log() {
        let blob = vec![
            Message::user_text("a"),
            Message::user_text("b"),
            Message::user_text("c"),
        ];
        // Legacy session (no events) → blob verbatim.
        assert_eq!(reconstructed_history(None, &blob).len(), 3);
        // A complete log (>= blob) wins.
        let full = vec![
            AgentEvent::UserPrompt {
                agent_id: "root".into(),
                text: "a".into(),
            },
            AgentEvent::UserPrompt {
                agent_id: "root".into(),
                text: "b".into(),
            },
            AgentEvent::UserPrompt {
                agent_id: "root".into(),
                text: "c".into(),
            },
        ];
        assert_eq!(reconstructed_history(Some(&full), &blob).len(), 3);
        // A truncated/corrupt log (fewer than the blob) must NOT lose history —
        // fall back to the blob. This is the guard that saved the day after the
        // seq-collision bug.
        let short = vec![AgentEvent::UserPrompt {
            agent_id: "root".into(),
            text: "a".into(),
        }];
        let got = reconstructed_history(Some(&short), &blob);
        assert_eq!(got.len(), 3, "short log must fall back to the blob");
    }
}
