//! SQLite session persistence with JSON snapshots, indexed metadata, and event logs.

use crate::core::events::AgentEvent;
use crate::core::permissions::Grant;
use crate::core::types::{Message, Role, ToolOutput};
use crate::core::usage::UsageEntry;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub provider: String,
    /// Scopes resume/continue to the project; empty in legacy sessions.
    #[serde(default)]
    pub cwd: String,
    pub messages: Vec<Message>,
    /// Monotonic logical context; older checkpoints cannot undo a reset.
    #[serde(default)]
    pub context_epoch: u64,
    /// "Always allow" grants the user made during this session.
    #[serde(default)]
    pub grants: Vec<Grant>,
    /// Per-completion token usage recorded during this session.
    #[serde(default)]
    pub usage: Vec<UsageEntry>,
    /// Keyed by task tool_use_id; absent from root message history.
    #[serde(default)]
    pub subagent_runs: Vec<SubagentRun>,
    #[serde(default)]
    pub todos: Vec<crate::tools::todo::TodoItem>,
    #[serde(default)]
    pub agent_threads: Vec<PersistedThread>,
    /// Event-built workflow trees, anchored to their hand-off messages on resume.
    #[serde(default)]
    pub workflows: Vec<PersistedWorkflow>,
}

/// Resumed runs are finished and matched to `[workflow result]` messages in order.
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistedCell {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Tool {
        name: String,
        input: Value,
        output: ToolOutput,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedSubagent {
    pub id: String,
    pub task: String,
    pub tools: Vec<PersistedTool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PersistedTool {
    pub id: String, // tool_use_id
    pub name: String,
    pub input: Value,
    pub output: ToolOutput,
    #[serde(default)]
    pub is_error: bool,
}

pub(crate) fn db_path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".bob").join("bob.db")
}

fn open_db() -> anyhow::Result<Connection> {
    open_db_at(&db_path())
}

pub(crate) fn open_db_at(path: &Path) -> anyhow::Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    // WAL lets readers and a writer coexist; busy_timeout makes contended writes
    // wait rather than fail with SQLITE_BUSY. synchronous=NORMAL is the WAL pairing.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000; PRAGMA synchronous=NORMAL;",
    )?;
    init_schema(&conn)?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    let transaction = conn.unchecked_transaction()?;
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
    let columns = {
        let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    if !columns.iter().any(|name| name == "cwd") {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN cwd TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    // Append-only event log: versioned AgentEvent envelopes ordered by a
    // per-session monotonic `seq`.
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
    // Retain the deployed cursor column for compatibility; sequences come from MAX(seq).
    if !columns.iter().any(|name| name == "last_seq") {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN last_seq INTEGER NOT NULL DEFAULT -1",
            [],
        )?;
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memories (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            scope TEXT NOT NULL CHECK(scope IN ('project', 'global')),
            scope_key TEXT NOT NULL,
            content TEXT NOT NULL,
            UNIQUE(scope, scope_key, content)
         );",
    )?;
    transaction.commit()?;
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
        context_epoch: 0,
        grants: vec![],
        usage: vec![],
        subagent_runs: vec![],
        todos: vec![],
        agent_threads: vec![],
        workflows: vec![],
    }
}

/// Within an epoch, growing or same-length writes win and shrinking writes are
/// dropped. A newer context epoch may shrink; an older epoch can never overwrite it.
/// [`save_session_force`] also allows intentional truncation within the same epoch.
pub fn save_session(s: &Session) -> anyhow::Result<()> {
    write_session(s, false)
}

/// Allows intentional truncation within an epoch; older epochs are still rejected.
pub fn save_session_force(s: &Session) -> anyhow::Result<()> {
    write_session(s, true)
}

fn write_session(s: &Session, allow_shrink: bool) -> anyhow::Result<()> {
    let conn = open_db()?;
    write_session_to(&conn, s, allow_shrink)
}

/// The UPSERT guard rejects stale snapshots atomically against concurrent writers.
pub(crate) fn write_session_to(
    conn: &Connection,
    s: &Session,
    allow_shrink: bool,
) -> anyhow::Result<()> {
    let data = serde_json::to_string(s)?;
    let guard = if allow_shrink {
        "WHERE COALESCE(json_extract(excluded.data, '$.context_epoch'), 0) >= COALESCE(json_extract(sessions.data, '$.context_epoch'), 0)"
    } else {
        "WHERE COALESCE(json_extract(excluded.data, '$.context_epoch'), 0) > COALESCE(json_extract(sessions.data, '$.context_epoch'), 0)
         OR (COALESCE(json_extract(excluded.data, '$.context_epoch'), 0) = COALESCE(json_extract(sessions.data, '$.context_epoch'), 0)
             AND excluded.message_count >= sessions.message_count)"
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

/// Bump for payload changes requiring migration, not additive fields with defaults.
const EVENT_ENVELOPE_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
struct EventEnvelope {
    v: u32,
    #[serde(flatten)]
    event: AgentEvent,
}

/// `ts` is Unix milliseconds. Returns the next per-session sequence number.
/// Assignment and insertion are atomic across independent store instances.
pub fn append_event_to(
    conn: &Connection,
    session_id: &str,
    ts: i64,
    event: &AgentEvent,
) -> anyhow::Result<i64> {
    let envelope = EventEnvelope {
        v: EVENT_ENVELOPE_VERSION,
        event: event.clone(),
    };
    let data = serde_json::to_string(&envelope)?;
    let mut stmt = conn.prepare(
        "INSERT INTO events (session_id, seq, ts, kind, data)
         SELECT ?1, COALESCE(MAX(seq), -1) + 1, ?2, ?3, ?4
         FROM events WHERE session_id = ?1 RETURNING seq",
    )?;
    let mut rows = stmt.query(rusqlite::params![session_id, ts, event.kind(), data])?;
    let seq = rows
        .next()?
        .ok_or(rusqlite::Error::QueryReturnedNoRows)?
        .get(0)?;
    // RETURNING yields a row before commit; step to completion to observe write errors.
    rows.next()?;
    Ok(seq)
}

/// Replay in sequence order, skipping undecodable payloads.
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

pub fn has_events(conn: &Connection, session_id: &str) -> bool {
    has_events_in(conn, session_id).unwrap_or(false)
}

fn has_events_in(conn: &Connection, session_id: &str) -> anyhow::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM events WHERE session_id = ?1 LIMIT 1",
            [session_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// `None` marks a legacy blob-only session, allowing snapshot fallback.
pub fn load_events(session_id: &str) -> anyhow::Result<Option<Vec<AgentEvent>>> {
    load_events_from(&open_db()?, session_id)
}

pub(crate) fn load_events_from(
    conn: &Connection,
    session_id: &str,
) -> anyhow::Result<Option<Vec<AgentEvent>>> {
    if !has_events_in(conn, session_id)? {
        return Ok(None);
    }
    Ok(Some(replay_events(conn, session_id)?))
}

pub(crate) fn load_working_events_from(
    conn: &Connection,
    session_id: &str,
) -> anyhow::Result<Option<Vec<AgentEvent>>> {
    let mut stmt =
        conn.prepare("SELECT seq, data FROM events WHERE session_id = ?1 ORDER BY seq ASC")?;
    let rows = stmt.query_map([session_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut events = Vec::new();
    for row in rows {
        let (seq, json) = row?;
        let Ok(envelope) = serde_json::from_str::<EventEnvelope>(&json) else {
            return Ok(None);
        };
        if seq != events.len() as i64 || envelope.v != EVENT_ENVELOPE_VERSION {
            return Ok(None);
        }
        events.push(envelope.event);
    }
    Ok(Some(events))
}

/// Reconstruct root history, folding coordination messages and bundling tool results
/// in request order. Subagent events never enter root history.
pub fn root_history_from_events(events: &[AgentEvent]) -> Vec<Message> {
    let mut msgs: Vec<Message> = Vec::new();
    let mut pending_results: Vec<crate::core::types::ContentBlock> = Vec::new();

    fn flush(msgs: &mut Vec<Message>, pending: &mut Vec<crate::core::types::ContentBlock>) {
        if !pending.is_empty() {
            if let Some(calls) = msgs.iter().rev().find(|message| {
                message
                    .content
                    .iter()
                    .any(|block| matches!(block, crate::core::types::ContentBlock::ToolUse { .. }))
            }) {
                pending.sort_by_key(|block| match block {
                    crate::core::types::ContentBlock::ToolResult { tool_use_id, .. } => calls
                        .content.iter().position(|call| matches!(call, crate::core::types::ContentBlock::ToolUse { id, .. } if id == tool_use_id))
                        .unwrap_or(usize::MAX),
                    _ => usize::MAX,
                });
            }
            msgs.push(Message {
                role: Role::Tool,
                content: std::mem::take(pending),
            });
        }
    }

    for e in events {
        match e {
            AgentEvent::ContextReset { agent_id, .. } if agent_id == "root" => {
                msgs.clear();
                pending_results.clear();
            }
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

/// Prefer replay unless an incomplete log would lose snapshot history.
/// A logged reset permits shorter history; `None` marks a legacy blob-only session.
pub fn reconstructed_history(events: Option<&[AgentEvent]>, blob: &[Message]) -> Vec<Message> {
    match events {
        Some(evs) => {
            let rebuilt = root_history_from_events(evs);
            if context_epoch(evs) > 0 || rebuilt.len() >= blob.len() {
                rebuilt
            } else {
                blob.to_vec()
            }
        }
        None => blob.to_vec(),
    }
}

pub fn context_epoch(events: &[AgentEvent]) -> u64 {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ContextReset { agent_id, epoch } if agent_id == "root" => Some(*epoch),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

pub fn active_context_events(events: &[AgentEvent]) -> &[AgentEvent] {
    let start = events.iter().rposition(|event| {
        matches!(event, AgentEvent::ContextReset { agent_id, .. } if agent_id == "root")
    }).map_or(0, |index| index + 1);
    &events[start..]
}

pub(crate) fn reconstructed_session_history(
    events: Option<&[AgentEvent]>,
    session: &Session,
) -> Vec<Message> {
    match events {
        Some(events) if context_epoch(events) > session.context_epoch => {
            root_history_from_events(events)
        }
        Some(events) if context_epoch(events) < session.context_epoch => session.messages.clone(),
        Some(events) => {
            reconstructed_history(Some(active_context_events(events)), &session.messages)
        }
        None => session.messages.clone(),
    }
}

/// Repair provider context only; unknown outcomes must never trigger a tool rerun.
pub fn repair_interrupted_tool_calls(mut messages: Vec<Message>) -> Vec<Message> {
    use crate::core::types::ContentBlock;
    let mut index = 0;
    while index < messages.len() {
        let calls: Vec<String> = messages[index]
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            index += 1;
            continue;
        }
        let completed: std::collections::HashSet<String> = messages[index + 1..]
            .iter()
            .take_while(|message| message.role != Role::Assistant)
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect();
        let missing: Vec<_> = calls.iter().filter(|id| !completed.contains(*id)).map(|id| {
            ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: "[interrupted] No result was recorded before this session stopped. The tool outcome is unknown; it has not been rerun.".into(),
                is_error: Some(true),
            }
        }).collect();
        if !missing.is_empty() {
            if messages
                .get(index + 1)
                .is_some_and(|message| message.role == Role::Tool)
            {
                messages[index + 1].content.extend(missing);
                messages[index + 1]
                    .content
                    .sort_by_key(|block| match block {
                        ContentBlock::ToolResult { tool_use_id, .. } => calls
                            .iter()
                            .position(|id| id == tool_use_id)
                            .unwrap_or(usize::MAX),
                        _ => usize::MAX,
                    });
            } else {
                messages.insert(
                    index + 1,
                    Message {
                        role: Role::Tool,
                        content: missing,
                    },
                );
            }
        }
        index += 1;
    }
    messages
}

/// Restore the compacted provider context only when replay has a trustworthy
/// full-history baseline. Legacy or incomplete logs retain the full transcript.
pub(crate) fn reconstructed_working_history(
    events: Option<&[AgentEvent]>,
    blob: &[Message],
    full: &[Message],
) -> Vec<Message> {
    let Some(events) = events else {
        return full.to_vec();
    };
    let events = active_context_events(events);
    let rebuilt = root_history_from_events(events);
    let matches =
        |a: &[Message], b: &[Message]| match (serde_json::to_value(a), serde_json::to_value(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
    if events
        .iter()
        .any(|event| matches!(event, AgentEvent::Unknown))
        || !matches(&rebuilt, full)
        || rebuilt.len() < blob.len()
        || !matches(&rebuilt[..blob.len()], blob)
    {
        return full.to_vec();
    }

    let mut working = Vec::new();
    let mut start = 0;
    for (index, event) in events.iter().enumerate() {
        if let AgentEvent::Compaction {
            agent_id,
            summary,
            replaced_upto,
            ..
        } = event
        {
            if agent_id != "root" {
                continue;
            }
            // Reducing through this boundary flushes pending tool results before
            // interpreting the split index in the pre-compaction working set.
            working.extend(root_history_from_events(&events[start..index]));
            working = repair_interrupted_tool_calls(working);
            start = index + 1;
            if summary.trim().is_empty()
                || *replaced_upto == 0
                || *replaced_upto > working.len()
                || !crate::agent::compaction::tool_safe_boundaries(&working)
                    .ok()
                    .and_then(|boundaries| boundaries.get(*replaced_upto).copied())
                    .unwrap_or(false)
            {
                return full.to_vec();
            }
            working.splice(
                ..*replaced_upto,
                [Message::user_text(format!(
                    "[conversation summary]\n{summary}"
                ))],
            );
        }
    }
    working.extend(root_history_from_events(&events[start..]));
    working
}

/// A background thread owns the connection so appends do not block on SQLite I/O.
pub struct EventLogWriter {
    tx: Option<std::sync::mpsc::Sender<WriterMsg>>,
    thread: Option<std::thread::JoinHandle<()>>,
    startup_error: Option<String>,
}

enum WriterMsg {
    Append {
        session_id: String,
        event: Box<AgentEvent>,
    },
    Flush(std::sync::mpsc::SyncSender<Result<(), String>>),
}

impl EventLogWriter {
    pub fn spawn() -> EventLogWriter {
        Self::spawn_at(db_path())
    }

    /// Retains startup failures so append/flush can report them.
    pub fn spawn_at(path: impl AsRef<Path>) -> EventLogWriter {
        match Self::open(path) {
            Ok(writer) => writer,
            Err(error) => Self {
                tx: None,
                thread: None,
                startup_error: Some(format!("{error:#}")),
            },
        }
    }

    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<EventLogWriter> {
        Self::from_connection(open_db_at(path.as_ref())?)
    }

    fn from_connection(conn: Connection) -> anyhow::Result<EventLogWriter> {
        let (tx, rx) = std::sync::mpsc::channel::<WriterMsg>();
        let thread = std::thread::Builder::new()
            .name("bob-event-log".into())
            .spawn(move || {
                // A lost event cannot be repaired by a later successful write.
                let mut error = None;
                for msg in rx {
                    match msg {
                        WriterMsg::Append { session_id, event } => {
                            if let Err(err) =
                                append_event_to(&conn, &session_id, now_millis(), &event)
                            {
                                error.get_or_insert_with(|| format!("{err:#}"));
                            }
                        }
                        WriterMsg::Flush(ack) => {
                            let result = error.clone().map_or(Ok(()), Err);
                            let _ = ack.send(result);
                        }
                    }
                }
            })?;
        Ok(EventLogWriter {
            tx: Some(tx),
            thread: Some(thread),
            startup_error: None,
        })
    }

    pub(crate) fn check_startup(&self) -> anyhow::Result<()> {
        if let Some(error) = &self.startup_error {
            anyhow::bail!("event log writer startup failed: {error}");
        }
        Ok(())
    }

    /// Queue without waiting for SQLite. Background failures are reported by flush.
    pub fn append(&self, session_id: &str, event: &AgentEvent) -> anyhow::Result<()> {
        self.check_startup()?;
        self.tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("event log writer is closed"))?
            .send(WriterMsg::Append {
                session_id: session_id.to_string(),
                event: Box::new(event.clone()),
            })
            .map_err(|_| anyhow::anyhow!("event log writer disconnected"))
    }

    /// Wait for all earlier appends, reporting any startup or write failure.
    pub fn flush(&self) -> anyhow::Result<()> {
        self.check_startup()?;
        let (ack_tx, ack_rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("event log writer is closed"))?
            .send(WriterMsg::Flush(ack_tx))
            .map_err(|_| anyhow::anyhow!("event log writer disconnected"))?;
        ack_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("event log writer disconnected before flush"))?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for EventLogWriter {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
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
    load_session_from(&open_db()?, id)
}

pub(crate) fn load_session_from(conn: &Connection, id: &str) -> anyhow::Result<Option<Session>> {
    let data: Option<String> = conn
        .query_row("SELECT data FROM sessions WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    data.map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(Into::into)
}

/// Last `n` user/assistant text lines for the resume picker; empty on load failure.
pub fn session_preview(id: &str, n: usize) -> Vec<String> {
    let session = match load_session(id) {
        Ok(Some(s)) => s,
        _ => return Vec::new(),
    };
    preview_lines(&session.messages, n)
}

fn preview_lines(messages: &[Message], n: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in messages.iter().rev() {
        if out.len() >= n {
            break;
        }
        let role = match m.role {
            Role::User => "you",
            Role::Assistant => "bob",
            _ => continue,
        };
        let text = m.text();
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let one_line = text.replace('\n', " ");
        let snippet: String = if one_line.chars().count() <= 80 {
            one_line
        } else {
            let cut: String = one_line.chars().take(80).collect();
            format!("{cut}…")
        };
        out.push(format!("{role}: {snippet}"));
    }
    out.reverse();
    out
}

pub fn latest_session() -> anyhow::Result<Option<Session>> {
    latest_session_from(&open_db()?, None)
}

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

/// Newest-first summaries from indexed columns, without parsing full snapshots.
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
    open_db()
        .and_then(|conn| query_summaries_from(&conn, cwd))
        .unwrap_or_default()
}

const SESSION_ORDER: &str = "CAST(updated_at AS INTEGER) DESC, updated_at DESC, id ASC";

pub(crate) fn query_summaries_from(
    conn: &Connection,
    cwd: Option<&str>,
) -> anyhow::Result<Vec<SessionSummary>> {
    let map_row = |r: &rusqlite::Row| {
        Ok(SessionSummary {
            id: r.get(0)?,
            title: r.get(1)?,
            provider: r.get(2)?,
            created_at: r.get(3)?,
            updated_at: r.get(4)?,
            message_count: r.get::<_, i64>(5)? as usize,
            cwd: r.get(6)?,
        })
    };
    let base =
        "SELECT id, title, provider, created_at, updated_at, message_count, cwd FROM sessions";
    let rows = match cwd {
        Some(dir) => {
            let sql = format!("{base} WHERE cwd = ?1 ORDER BY {SESSION_ORDER}");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([dir], map_row)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
        None => {
            let sql = format!("{base} ORDER BY {SESSION_ORDER}");
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], map_row)?;
            rows.collect::<Result<Vec<_>, _>>()?
        }
    };
    Ok(rows)
}

/// The most recently updated session started in `cwd` (for `--continue`). None if
/// there is no session for this directory.
pub fn latest_session_in(cwd: &str) -> anyhow::Result<Option<Session>> {
    latest_session_from(&open_db()?, Some(cwd))
}

pub(crate) fn latest_session_from(
    conn: &Connection,
    cwd: Option<&str>,
) -> anyhow::Result<Option<Session>> {
    let data: Option<String> = match cwd {
        Some(dir) => conn
            .query_row(
                &format!(
                    "SELECT data FROM sessions WHERE cwd = ?1 ORDER BY {SESSION_ORDER} LIMIT 1"
                ),
                [dir],
                |r| r.get(0),
            )
            .optional()?,
        None => conn
            .query_row(
                &format!("SELECT data FROM sessions ORDER BY {SESSION_ORDER} LIMIT 1"),
                [],
                |r| r.get(0),
            )
            .optional()?,
    };
    data.map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod sqlite_failures {
        use super::*;

        #[test]
        fn query_helpers_distinguish_missing_rows_from_sql_errors() {
            let conn = Connection::open_in_memory().unwrap();
            init_schema(&conn).unwrap();
            assert!(load_session_from(&conn, "missing").unwrap().is_none());
            assert!(latest_session_from(&conn, None).unwrap().is_none());
            assert!(load_events_from(&conn, "missing").unwrap().is_none());
            conn.execute_batch("DROP TABLE sessions; DROP TABLE events;")
                .unwrap();
            assert!(load_session_from(&conn, "missing").is_err());
            assert!(latest_session_from(&conn, None).is_err());
            assert!(latest_session_from(&conn, Some(".")).is_err());
            assert!(query_summaries_from(&conn, None).is_err());
            assert!(query_summaries_from(&conn, Some(".")).is_err());
            assert!(load_events_from(&conn, "missing").is_err());
        }

        #[test]
        fn writer_flush_reports_read_only_sqlite_error() {
            let conn = Connection::open_in_memory().unwrap();
            init_schema(&conn).unwrap();
            conn.execute_batch("PRAGMA query_only = ON;").unwrap();
            let writer = EventLogWriter::from_connection(conn).unwrap();
            writer
                .append(
                    "s",
                    &AgentEvent::TurnStart {
                        agent_id: "root".into(),
                    },
                )
                .unwrap();
            let error = writer.flush().unwrap_err().to_string();
            assert!(error.contains("readonly"), "{error}");
            assert!(writer.flush().is_err());
        }

        #[test]
        fn writer_flush_reports_deferred_constraint_failure() {
            let conn = Connection::open_in_memory().unwrap();
            init_schema(&conn).unwrap();
            conn.execute_batch(
                "PRAGMA foreign_keys = ON;
                     CREATE TABLE parents (id INTEGER PRIMARY KEY);
                     CREATE TABLE children (
                         parent_id INTEGER REFERENCES parents(id) DEFERRABLE INITIALLY DEFERRED
                     );
                     CREATE TRIGGER reject_at_commit AFTER INSERT ON events
                     BEGIN INSERT INTO children (parent_id) VALUES (1); END;",
            )
            .unwrap();
            let writer = EventLogWriter::from_connection(conn).unwrap();
            writer
                .append(
                    "s",
                    &AgentEvent::TurnStart {
                        agent_id: "root".into(),
                    },
                )
                .unwrap();
            let error = writer.flush().unwrap_err().to_string();
            assert!(error.contains("FOREIGN KEY"), "{error}");
        }

        #[test]
        fn writer_reports_disconnected_channel() {
            let (tx, rx) = std::sync::mpsc::channel();
            drop(rx);
            let writer = EventLogWriter {
                tx: Some(tx),
                thread: None,
                startup_error: None,
            };
            assert!(writer
                .append(
                    "s",
                    &AgentEvent::TurnStart {
                        agent_id: "root".into()
                    }
                )
                .is_err());
            assert!(writer.flush().is_err());
        }

        #[test]
        fn writer_reports_disconnect_during_flush() {
            let (tx, rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                if let WriterMsg::Flush(ack) = rx.recv().unwrap() {
                    drop(ack);
                }
            });
            let writer = EventLogWriter {
                tx: Some(tx),
                thread: Some(thread),
                startup_error: None,
            };
            assert!(writer
                .flush()
                .unwrap_err()
                .to_string()
                .contains("before flush"));
        }
    }

    mod presentation_and_serialization {
        use super::*;

        fn assistant(text: &str) -> Message {
            Message {
                role: Role::Assistant,
                content: vec![crate::core::types::ContentBlock::Text { text: text.into() }],
            }
        }

        #[test]
        fn preview_lines_filters_roles_and_formats_a_chronological_unicode_tail() {
            let messages = vec![
                Message::user_text("first question"),
                assistant("first answer"),
                Message::user_text(" second\nquestion "),
                Message {
                    role: Role::System,
                    content: assistant("hidden system").content,
                },
                Message {
                    role: Role::Tool,
                    content: assistant("hidden tool").content,
                },
                assistant("   "),
                assistant(&"界".repeat(81)),
            ];
            for (messages, n, expected) in [
                (
                    messages.as_slice(),
                    2,
                    vec![
                        "you: second question".into(),
                        format!("bob: {}…", "界".repeat(80)),
                    ],
                ),
                (messages.as_slice(), 0, vec![]),
                (&[][..], 3, vec![]),
            ] {
                assert_eq!(preview_lines(messages, n), expected);
            }
            assert_eq!(
                preview_lines(&[assistant(&"界".repeat(80))], 1),
                vec![format!("bob: {}", "界".repeat(80))]
            );
        }

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
            assert_eq!(
                serde_json::to_value(back).unwrap(),
                serde_json::to_value(session).unwrap()
            );
        }

        #[test]
        fn grants_round_trip_with_session() {
            let session = Session {
                grants: vec![
                    Grant::Tool {
                        name: "edit_file".into(),
                    },
                    Grant::BashCommand {
                        name: "cargo".into(),
                        glob: "**".into(),
                    },
                ],
                ..new_session("anthropic", "s1".into(), "now".into(), String::new())
            };

            let value = serde_json::to_value(&session).unwrap();
            assert_eq!(
                value["grants"][0],
                serde_json::json!({"kind": "tool", "name": "edit_file"})
            );
            assert_eq!(
                value["grants"][1],
                serde_json::json!({"kind": "bash_command", "name": "cargo", "glob": "**"})
            );

            let back: Session = serde_json::from_value(value).unwrap();
            match &back.grants[0] {
                Grant::Tool { name } => assert_eq!(name, "edit_file"),
                other => panic!("expected tool grant, got {other:?}"),
            }
            match &back.grants[1] {
                Grant::BashCommand { name, glob } => {
                    assert_eq!(name, "cargo");
                    assert_eq!(glob, "**");
                }
                other => panic!("expected bash grant, got {other:?}"),
            }
        }

        #[test]
        fn old_session_without_agent_threads_loads() {
            let json = r#"{
                    "id": "s1", "created_at": "t", "updated_at": "t",
                    "provider": "anthropic", "messages": []
                }"#;
            let s: Session = serde_json::from_str(json).unwrap();
            assert!(s.agent_threads.is_empty());
            assert!(s.todos.is_empty());
            assert_eq!(s.cwd, "");
        }
    }

    mod snapshot_guards {
        use super::*;

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
        fn force_save_allows_intentional_shrink_but_rejects_an_older_epoch() {
            let conn = Connection::open_in_memory().unwrap();
            init_schema(&conn).unwrap();
            write_session_to(&conn, &sized("s", 10), false).unwrap();
            write_session_to(&conn, &sized("s", 0), true).unwrap();
            assert_eq!(count_on_disk(&conn, "s"), 0);
            assert_eq!(
                serde_json::to_value(load_session_from(&conn, "s").unwrap().unwrap()).unwrap(),
                serde_json::to_value(sized("s", 0)).unwrap()
            );
            write_session_to(&conn, &sized("s", 2), false).unwrap();
            assert_eq!(count_on_disk(&conn, "s"), 2);
            assert_eq!(
                serde_json::to_value(load_session_from(&conn, "s").unwrap().unwrap()).unwrap(),
                serde_json::to_value(sized("s", 2)).unwrap()
            );
            let mut current = sized("s", 1);
            current.context_epoch = 1;
            current.messages[0] = Message::user_text("new context");
            write_session_to(&conn, &current, false).unwrap();
            write_session_to(&conn, &sized("s", 10), true).unwrap();
            assert_eq!(count_on_disk(&conn, "s"), 1);
            assert_eq!(
                serde_json::to_value(load_session_from(&conn, "s").unwrap().unwrap()).unwrap(),
                serde_json::to_value(current).unwrap()
            );
        }
    }

    mod event_log {
        use super::*;

        #[test]
        fn events_append_and_replay_in_order() {
            let conn = Connection::open_in_memory().unwrap();
            init_schema(&conn).unwrap();
            assert!(load_session_from(&conn, "s").unwrap().is_none());
            assert!(!has_events(&conn, "s"));

            let e0 = AgentEvent::TurnStart {
                agent_id: "root".into(),
            };
            let e1 = AgentEvent::TextDelta {
                agent_id: "root".into(),
                text: "hi".into(),
            };
            assert_eq!(append_event_to(&conn, "s", 100, &e0).unwrap(), 0);
            assert_eq!(append_event_to(&conn, "other", 200, &e1).unwrap(), 0);
            assert_eq!(append_event_to(&conn, "s", 99, &e1).unwrap(), 1);
            assert!(load_session_from(&conn, "s").unwrap().is_none());
            write_session_to(
                &conn,
                &new_session("mock", "s".into(), "0".into(), ".".into()),
                false,
            )
            .unwrap();

            assert!(has_events(&conn, "s"));

            for _ in 0..2 {
                assert_eq!(
                    serde_json::to_value(replay_events(&conn, "s").unwrap()).unwrap(),
                    serde_json::to_value([&e0, &e1]).unwrap()
                );
            }
            assert_eq!(
                serde_json::to_value(replay_events(&conn, "other").unwrap()).unwrap(),
                serde_json::to_value([&e1]).unwrap()
            );
        }

        #[test]
        fn compaction_event_round_trips_summary() {
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
            assert_eq!(
                serde_json::to_value(replay_events(&conn, "s").unwrap()).unwrap(),
                serde_json::to_value([ev]).unwrap()
            );
        }

        #[test]
        fn unknown_future_variant_replays_as_noop() {
            let conn = Connection::open_in_memory().unwrap();
            init_schema(&conn).unwrap();
            write_session_to(
                &conn,
                &new_session("mock", "s".into(), "0".into(), ".".into()),
                false,
            )
            .unwrap();
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
    }

    mod replay {
        use super::*;

        #[test]
        fn root_history_reducer_mirrors_the_turn_loop() {
            use crate::core::types::ContentBlock;
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
            let expected = vec![
                Message::user_text("fix it"),
                assistant_with_tool,
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "ok".into(),
                        is_error: Some(false),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
                    }],
                },
            ];
            assert_eq!(
                serde_json::to_value(root_history_from_events(&events)).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }

        #[test]
        fn compacted_replay_restores_request_order_after_parallel_tool_completion() {
            use crate::core::types::ContentBlock;
            let assistant = Message {
                role: Role::Assistant,
                content: ["a", "b"]
                    .into_iter()
                    .map(|id| ContentBlock::ToolUse {
                        id: id.into(),
                        name: "read_file".into(),
                        input: serde_json::json!({}),
                    })
                    .collect(),
            };
            let results = Message {
                role: Role::Tool,
                content: ["a", "b"]
                    .into_iter()
                    .map(|id| ContentBlock::ToolResult {
                        tool_use_id: id.into(),
                        content: id.into(),
                        is_error: Some(false),
                    })
                    .collect(),
            };
            let full = vec![
                Message::user_text("question"),
                assistant.clone(),
                results,
                Message::user_text("continue"),
            ];
            let events = vec![
                AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "question".into(),
                },
                AgentEvent::Message {
                    agent_id: "root".into(),
                    message: assistant,
                },
                AgentEvent::ToolResult {
                    agent_id: "root".into(),
                    tool_use_id: "b".into(),
                    output: "b".into(),
                    is_error: false,
                },
                AgentEvent::ToolResult {
                    agent_id: "root".into(),
                    tool_use_id: "a".into(),
                    output: "a".into(),
                    is_error: false,
                },
                AgentEvent::Compaction {
                    agent_id: "root".into(),
                    before_tokens: 1000,
                    after_tokens: 100,
                    summary: "summary".into(),
                    replaced_upto: 3,
                },
                AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "continue".into(),
                },
            ];
            assert_eq!(
                serde_json::to_value(root_history_from_events(&events)).unwrap(),
                serde_json::to_value(&full).unwrap()
            );
            let working = reconstructed_working_history(Some(&events), &full, &full);
            assert_eq!(working.len(), 2);
            assert_eq!(working[0].text(), "[conversation summary]\nsummary");
            assert_eq!(working[1].text(), "continue");
        }

        #[test]
        fn working_replay_requires_matching_authority_and_an_intact_tool_boundary() {
            use crate::core::types::ContentBlock;
            let mut events = vec![
                AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "question".into(),
                },
                AgentEvent::Message {
                    agent_id: "root".into(),
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: "call".into(),
                            name: "read_file".into(),
                            input: serde_json::json!({}),
                        }],
                    },
                },
                AgentEvent::ToolResult {
                    agent_id: "root".into(),
                    tool_use_id: "call".into(),
                    output: "result".into(),
                    is_error: false,
                },
                AgentEvent::Compaction {
                    agent_id: "root".into(),
                    before_tokens: 100,
                    after_tokens: 10,
                    summary: "summary".into(),
                    replaced_upto: 2,
                },
            ];
            let full = root_history_from_events(&events);
            let working = reconstructed_working_history(Some(&events), &full, &full);
            assert_eq!(
                serde_json::to_value(&working).unwrap(),
                serde_json::to_value(&full).unwrap()
            );
            if let AgentEvent::Compaction { replaced_upto, .. } = events.last_mut().unwrap() {
                *replaced_upto = 3;
            }
            assert_eq!(
                serde_json::to_value(reconstructed_working_history(Some(&events), &full, &full))
                    .unwrap(),
                serde_json::to_value([Message::user_text("[conversation summary]\nsummary")])
                    .unwrap()
            );
            let mut interleaved = events.clone();
            interleaved.insert(
                2,
                AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "coordination while tools ran".into(),
                },
            );
            if let AgentEvent::Compaction { replaced_upto, .. } = interleaved.last_mut().unwrap() {
                *replaced_upto = 2;
            }
            let interleaved_full = root_history_from_events(&interleaved);
            let interleaved_working = reconstructed_working_history(
                Some(&interleaved),
                &interleaved_full,
                &interleaved_full,
            );
            assert_eq!(
                serde_json::to_value(interleaved_working).unwrap(),
                serde_json::to_value(interleaved_full).unwrap()
            );
            let mut authoritative = full.clone();
            authoritative[0] = Message::user_text("authoritative question");
            let working = reconstructed_working_history(Some(&events), &[], &authoritative);
            assert_eq!(
                serde_json::to_value(working).unwrap(),
                serde_json::to_value(authoritative).unwrap()
            );
        }

        #[test]
        fn interrupted_repair_is_idempotent_and_does_not_borrow_an_old_call_result() {
            use crate::core::types::ContentBlock;
            let call = Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "reused".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({}),
                }],
            };
            let full = vec![
                call.clone(),
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "reused".into(),
                        content: "earlier result".into(),
                        is_error: Some(false),
                    }],
                },
                call,
            ];
            let original = serde_json::to_value(&full).unwrap();
            let repaired = repair_interrupted_tool_calls(full.clone());
            assert_eq!(repaired.len(), 4);
            assert!(
                matches!(&repaired[3].content[0], ContentBlock::ToolResult { content, is_error: Some(true), .. } if content.as_text().unwrap().contains("outcome is unknown"))
            );
            assert_eq!(
                serde_json::to_value(repair_interrupted_tool_calls(repaired.clone())).unwrap(),
                serde_json::to_value(repaired).unwrap()
            );
            assert_eq!(serde_json::to_value(&full).unwrap(), original);
        }

        #[test]
        fn compaction_after_interrupted_recovery_replays_working_only_repairs() {
            use crate::core::types::ContentBlock;
            let events = vec![
                AgentEvent::Message {
                    agent_id: "root".into(),
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentBlock::ToolUse {
                            id: "unknown".into(),
                            name: "bash".into(),
                            input: serde_json::json!({}),
                        }],
                    },
                },
                AgentEvent::Compaction {
                    agent_id: "root".into(),
                    before_tokens: 100,
                    after_tokens: 10,
                    summary: "Interrupted command outcome unknown.".into(),
                    replaced_upto: 2,
                },
            ];
            let full = root_history_from_events(&events);
            let working = reconstructed_working_history(Some(&events), &full, &full);
            assert_eq!(working.len(), 1);
            assert_eq!(
                working[0].text(),
                "[conversation summary]\nInterrupted command outcome unknown."
            );
            assert_eq!(full.len(), 1);
            assert!(matches!(full[0].content[0], ContentBlock::ToolUse { .. }));
        }

        #[test]
        fn replay_flushes_results_before_coordination_and_reset_discards_pending_results() {
            use crate::core::types::ContentBlock;
            let call = |id: &str| Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: id.into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": id}),
                }],
            };
            let result = |id: &str| AgentEvent::ToolResult {
                agent_id: "root".into(),
                tool_use_id: id.into(),
                output: format!("output {id}").into(),
                is_error: true,
            };
            let events = vec![
                AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "obsolete".into(),
                },
                AgentEvent::Message {
                    agent_id: "root".into(),
                    message: call("discarded"),
                },
                result("discarded"),
                AgentEvent::ContextReset {
                    agent_id: "root".into(),
                    epoch: 1,
                },
                AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "active".into(),
                },
                AgentEvent::Message {
                    agent_id: "root".into(),
                    message: call("kept"),
                },
                result("kept"),
                AgentEvent::ContextReset {
                    agent_id: "child".into(),
                    epoch: 99,
                },
                AgentEvent::AgentMessage {
                    to: "child".into(),
                    from: "root".into(),
                    text: "hidden".into(),
                },
                AgentEvent::AgentMessage {
                    to: "root".into(),
                    from: "reviewer".into(),
                    text: "finding".into(),
                },
            ];
            let expected = vec![
                Message::user_text("active"),
                call("kept"),
                Message {
                    role: Role::Tool,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "kept".into(),
                        content: "output kept".into(),
                        is_error: Some(true),
                    }],
                },
                Message::user_text(crate::agent::team::format_coord_message(
                    "reviewer", "finding",
                )),
            ];
            let before = serde_json::to_value(&events).unwrap();
            for _ in 0..2 {
                assert_eq!(
                    serde_json::to_value(root_history_from_events(&events)).unwrap(),
                    serde_json::to_value(&expected).unwrap()
                );
            }
            assert_eq!(context_epoch(&events), 1);
            assert_eq!(
                serde_json::to_value(active_context_events(&events)).unwrap(),
                serde_json::to_value(&events[4..]).unwrap()
            );
            assert_eq!(serde_json::to_value(&events).unwrap(), before);
        }

        #[test]
        fn session_epoch_overrides_length_but_equal_epoch_retains_complete_blob() {
            let events = vec![
                AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "archived".into(),
                },
                AgentEvent::ContextReset {
                    agent_id: "root".into(),
                    epoch: 2,
                },
                AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "current log".into(),
                },
            ];
            let mut session = new_session("mock", "s".into(), "0".into(), ".".into());
            session.messages = vec![
                Message::user_text("blob one"),
                Message::user_text("blob two"),
            ];
            for (epoch, expected) in [
                (1, vec![Message::user_text("current log")]),
                (2, session.messages.clone()),
                (3, session.messages.clone()),
            ] {
                session.context_epoch = epoch;
                assert_eq!(
                    serde_json::to_value(reconstructed_session_history(Some(&events), &session))
                        .unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
        }

        #[test]
        fn partial_tool_repair_preserves_real_failures_and_restores_request_order() {
            use crate::core::types::ContentBlock;
            let calls = Message {
                role: Role::Assistant,
                content: ["first", "second", "third"]
                    .into_iter()
                    .map(|id| ContentBlock::ToolUse {
                        id: id.into(),
                        name: "bash".into(),
                        input: serde_json::json!({"command": id}),
                    })
                    .collect(),
            };
            let real_result = ContentBlock::ToolResult {
                tool_use_id: "second".into(),
                content: "permission denied".into(),
                is_error: Some(true),
            };
            let full = vec![
                calls.clone(),
                Message {
                    role: Role::Tool,
                    content: vec![real_result.clone()],
                },
                Message::user_text("continue"),
            ];
            let repaired = repair_interrupted_tool_calls(full.clone());
            assert_eq!(repaired.len(), 3);
            assert_eq!(
                serde_json::to_value(&repaired[0]).unwrap(),
                serde_json::to_value(calls).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&repaired[1].content[1]).unwrap(),
                serde_json::to_value(real_result).unwrap()
            );
            assert_eq!(
                serde_json::to_value(&repaired[2]).unwrap(),
                serde_json::to_value(&full[2]).unwrap()
            );
            let results: Vec<_> = repaired[1]
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        assert_eq!(*is_error, Some(true));
                        if tool_use_id != "second" {
                            assert!(content.as_text().unwrap().contains("outcome is unknown"));
                            assert!(content.as_text().unwrap().contains("has not been rerun"));
                        }
                        tool_use_id.as_str()
                    }
                    other => panic!("unexpected block: {other:?}"),
                })
                .collect();
            assert_eq!(results, ["first", "second", "third"]);
            assert_eq!(
                serde_json::to_value(repair_interrupted_tool_calls(repaired.clone())).unwrap(),
                serde_json::to_value(repaired).unwrap()
            );
        }

        #[test]
        fn reconstructed_history_selects_contents_from_the_authoritative_source() {
            let blob: Vec<_> = ["blob a", "blob b", "blob c"]
                .into_iter()
                .map(Message::user_text)
                .collect();
            let events: Vec<_> = ["log a", "log b", "log c", "log d"]
                .into_iter()
                .map(|text| AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: text.into(),
                })
                .collect();
            for (log, expected) in [
                (None, vec!["blob a", "blob b", "blob c"]),
                (Some(&events[..0]), vec!["blob a", "blob b", "blob c"]),
                (Some(&events[..2]), vec!["blob a", "blob b", "blob c"]),
                (Some(&events[..3]), vec!["log a", "log b", "log c"]),
                (Some(&events[..]), vec!["log a", "log b", "log c", "log d"]),
            ] {
                assert_eq!(
                    reconstructed_history(log, &blob)
                        .iter()
                        .map(Message::text)
                        .collect::<Vec<_>>(),
                    expected
                );
            }
        }
    }
}
