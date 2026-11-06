use crate::model::*;
use crate::presentation::*;
use anyhow::{ensure, Context};
use bob_types::{ContentBlock, Message, Role};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::path::Path;

pub(crate) const APPLICATION: i64 = 0x424f4232;
pub(crate) const SCHEMA: i64 = 10;
const CHUNK: usize = 64 * 1024;
pub(crate) const PAGE_SQL: &str = "SELECT e.seq,e.kind,e.text,e.result,e.operation,
    CASE WHEN o.kind='tool' THEN o.id END,o.name,o.state,
    CASE WHEN length(o.input)<=4096 THEN o.input ELSE json_object(
        'path',substr(json_extract(o.input,'$.path'),1,1024),
        'command',substr(json_extract(o.input,'$.command'),1,1024),
        'name',substr(json_extract(o.input,'$.name'),1,256),
        'model',substr(json_extract(o.input,'$.model'),1,256),
        'thread_id',substr(json_extract(o.input,'$.thread_id'),1,128),
        'preview_truncated',json('true')) END,o.result,o.presentation,
    CASE WHEN e.kind='coordination' THEN s.sender END,substr(sender.name,1,256),
    COALESCE(e.kind='coordination' AND s.source='coordination' AND s.thread=e.thread
        AND recipient.parent_run IS NOT NULL AND sender.session=recipient.session
        AND ((c.kind='message' AND (parent.thread=s.sender OR sender.parent_run IS NULL))
            OR (c.id IS NULL AND parent.thread=s.sender)),0),
    CASE WHEN s.source='human' AND s.thread=e.thread THEN s.id END,s.state,s.rowid,visible.total_bytes
    FROM entries e LEFT JOIN assistant_visible_text visible ON visible.source=COALESCE(e.model,e.result) AND e.kind='assistant' AND visible.version=1
    LEFT JOIN operations o ON o.id=e.operation
    LEFT JOIN submissions s ON s.id=e.submission
    LEFT JOIN threads sender ON sender.id=s.sender
    LEFT JOIN threads recipient ON recipient.id=e.thread
    LEFT JOIN runs parent ON parent.id=recipient.parent_run
    LEFT JOIN coordination c ON c.id=s.id
    WHERE e.thread=?1 AND e.seq<?2 ORDER BY e.seq DESC LIMIT ?3";
pub(crate) const MAX_ARTIFACT: usize = 4 * 1024 * 1024;

fn state_column<T: std::str::FromStr<Err = anyhow::Error>>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<T> {
    row.get::<_, String>(index)?
        .parse()
        .map_err(|error: anyhow::Error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                error.into(),
            )
        })
}

fn optional_json_column<T: DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<String>>(index)?
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

#[derive(Deserialize)]
struct ActivityRecord {
    name: String,
    kind: String,
    state: OperationState,
}
impl ActivityRecord {
    fn project(self) -> anyhow::Result<ThreadActivity> {
        if self.state == OperationState::Approval {
            return Ok(ThreadActivity::Approval);
        }
        if self.state == OperationState::Waiting
            && (self.kind == "model" || matches!(self.name.as_str(), "wait_agent" | "wait_message"))
        {
            return Ok(ThreadActivity::Agent);
        }
        match self.kind.as_str() {
            "model" => Ok(ThreadActivity::Model),
            "tool" => Ok(ThreadActivity::Tool { name: self.name }),
            kind => anyhow::bail!("unknown operation kind: {kind}"),
        }
    }
}

pub(crate) struct Store {
    pub db: Connection,
    _lock: Option<File>,
    _database_file: Option<File>,
    pub(crate) path: Option<std::path::PathBuf>,
    pub(crate) claim: Option<crate::SessionClaim>,
}

/// On Unix, closing any descriptor for a SQLite file releases every POSIX
/// lock this process holds on it, including locks owned by live connections.
/// Inspect identity by path there and never retain or close a descriptor.
#[cfg(unix)]
pub(crate) fn database_file(path: &Path) -> anyhow::Result<Option<File>> {
    use std::os::unix::fs::MetadataExt;
    if std::fs::symlink_metadata(path).is_err() {
        if let Err(error) = OpenOptions::new().write(true).create_new(true).open(path) {
            ensure!(
                error.kind() == std::io::ErrorKind::AlreadyExists,
                "create durable database: {error}"
            );
        }
    }
    let metadata = std::fs::metadata(path).context("open durable database identity")?;
    ensure!(metadata.is_file(), "database must be a regular file");
    ensure!(
        metadata.nlink() == 1,
        "hard-linked durable databases are unsupported; use a single local database pathname"
    );
    Ok(None)
}

#[cfg(not(unix))]
pub(crate) fn database_file(path: &Path) -> anyhow::Result<Option<File>> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options
        .open(path)
        .context("open durable database identity")?;
    ensure!(
        file.metadata()?.is_file(),
        "database must be a regular file"
    );
    #[cfg(windows)]
    let links = {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(std::io::Error::last_os_error()).context("inspect database hard links");
        }
        u64::from(info.nNumberOfLinks)
    };
    #[cfg(windows)]
    ensure!(
        links == 1,
        "hard-linked durable databases are unsupported; use a single local database pathname"
    );
    Ok(Some(file))
}

pub(crate) fn encode(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(serde_json::to_string(value)?)
}
pub(crate) fn decode<T: DeserializeOwned>(value: String) -> anyhow::Result<T> {
    Ok(serde_json::from_str(&value)?)
}
pub(crate) fn digest(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(format!("{:x}", Sha256::digest(encode(value)?.as_bytes())))
}
pub(crate) fn preview(text: &str) -> String {
    text.chars().take(2048).collect()
}

pub(crate) fn change(
    db: &Connection,
    thread: Option<&ThreadId>,
    kind: &str,
    reference: &str,
) -> anyhow::Result<i64> {
    db.execute(
        "INSERT INTO changes(thread,kind,reference) VALUES(?1,?2,?3)",
        params![thread, kind, reference],
    )?;
    Ok(db.last_insert_rowid())
}

pub(crate) fn artifact(db: &Connection, text: &str) -> anyhow::Result<ArtifactId> {
    ensure!(
        text.len() <= MAX_ARTIFACT,
        "artifact exceeds the 4 MiB retention limit"
    );
    let id = ArtifactId::new();
    db.execute(
        "INSERT INTO artifacts(id,length) VALUES(?1,?2)",
        params![id, text.len()],
    )?;
    for (ordinal, bytes) in text.as_bytes().chunks(CHUNK).enumerate() {
        db.execute(
            "INSERT INTO chunks(artifact,ordinal,data) VALUES(?1,?2,?3)",
            params![id, ordinal, bytes],
        )?;
    }
    Ok(id)
}

pub(crate) fn full_artifact(db: &Connection, id: &ArtifactId) -> anyhow::Result<String> {
    let mut stmt = db.prepare("SELECT data FROM chunks WHERE artifact=?1 ORDER BY ordinal")?;
    let chunks = stmt.query_map([id], |row| row.get::<_, Vec<u8>>(0))?;
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend(chunk?);
    }
    Ok(String::from_utf8(bytes)?)
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum ModelRecord {
    Message(Message),
    ToolResult {
        call: String,
        output: ArtifactId,
        error: bool,
    },
}

pub(crate) fn load_message(db: &Connection, id: &ArtifactId) -> anyhow::Result<Message> {
    match decode::<ModelRecord>(full_artifact(db, id)?)? {
        ModelRecord::Message(message) => Ok(message),
        ModelRecord::ToolResult {
            call,
            output,
            error,
        } => Ok(Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: call,
                content: full_artifact(db, &output)?.into(),
                is_error: Some(error),
            }],
        }),
    }
}

#[derive(Default)]
pub(crate) struct EntryData<'a> {
    pub message: Option<&'a Message>,
    pub operation: Option<&'a OperationId>,
    pub submission: Option<&'a CommandId>,
    pub result: Option<&'a ArtifactId>,
}

pub(crate) fn entry(
    db: &Connection,
    thread: &ThreadId,
    kind: &str,
    text: &str,
    data: EntryData<'_>,
) -> anyhow::Result<()> {
    let EntryData {
        message,
        operation,
        submission,
        result,
    } = data;
    let model = match (message, result) {
        (Some(message), Some(result)) if kind == "assistant" => {
            debug_assert_eq!(message.role, Role::Assistant);
            Some(result.clone())
        }
        (Some(message), result) => {
            let record = match (message.content.as_slice(), result) {
                (
                    [ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    }],
                    Some(result),
                ) => ModelRecord::ToolResult {
                    call: tool_use_id.clone(),
                    output: result.clone(),
                    error: is_error.unwrap_or(false),
                },
                _ => ModelRecord::Message(message.clone()),
            };
            Some(artifact(db, &encode(&record)?)?)
        }
        (None, _) => None,
    };
    db.execute("INSERT INTO entries(thread,kind,text,model,operation,submission,result,model_order) VALUES(?1,?2,?3,?4,?5,?6,?7,CASE WHEN ?4 IS NULL THEN NULL ELSE (SELECT COALESCE(MAX(model_order),0)+1 FROM entries WHERE thread=?1) END)",
        params![thread,kind,preview(text),model,operation,submission,result])?;
    Ok(())
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let start = std::time::Instant::now();
        loop {
            match Self::open_once(path) {
                Err(error)
                    if crate::ownership::busy(&error)
                        && start.elapsed() < std::time::Duration::from_secs(2) =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                result => return result,
            }
        }
    }

    fn open_once(path: &Path) -> anyhow::Result<Self> {
        ensure!(
            path != Path::new(":memory:"),
            "bob needs a session database file, not an in-memory database"
        );
        let path = crate::canonical_database_path(path)?;
        let _schema_guard = crate::ownership::schema_guard(&path)?;
        let current = if path.exists() {
            let probe =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let application: i64 = probe.query_row("PRAGMA application_id", [], |r| r.get(0))?;
            let version: i64 = probe.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            application == APPLICATION && version == SCHEMA
        } else {
            false
        };
        let lock = crate::ownership::lock_file(&crate::ownership::sidecar(&path, "lock"))?;
        if current {
            lock.try_lock_shared()
                .context("database is owned by an incompatible execution owner")?;
        } else {
            lock.try_lock()
                .context("database schema upgrade requires exclusive compatibility ownership")?;
        }
        let database_file = database_file(&path)?;
        let mut db = Connection::open(&path)?;
        db.busy_timeout(std::time::Duration::from_millis(25))?;
        let application: i64 = db.query_row("PRAGMA application_id", [], |row| row.get(0))?;
        let tables: i64 = db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        ensure!(
            application == APPLICATION || (application == 0 && tables == 0),
            "not a bob session database; refusing to change it"
        );
        if application == APPLICATION {
            let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            ensure!(
                matches!(version, 8 | 9 | SCHEMA),
                "unsupported bob session database version {version}"
            );
        }
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=25;")?;
        if tables == 0 {
            db.execute_batch(&format!("BEGIN IMMEDIATE;
                PRAGMA application_id={APPLICATION}; PRAGMA user_version=9;
                CREATE TABLE sessions(id TEXT PRIMARY KEY, workspace TEXT NOT NULL, policy TEXT NOT NULL, revision INTEGER NOT NULL DEFAULT 0, permissions TEXT NOT NULL DEFAULT '{{}}', updated INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
                CREATE TABLE threads(id TEXT PRIMARY KEY, session TEXT NOT NULL REFERENCES sessions(id), parent_run TEXT, name TEXT NOT NULL, profile TEXT NOT NULL, held INTEGER NOT NULL DEFAULT 0, profile_revision INTEGER NOT NULL DEFAULT 0);
                CREATE INDEX thread_session ON threads(session);
                CREATE INDEX thread_parent ON threads(parent_run);
                CREATE TABLE runs(ordinal INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, thread TEXT NOT NULL REFERENCES threads(id), state TEXT NOT NULL, requests INTEGER NOT NULL DEFAULT 0, retry_count INTEGER NOT NULL DEFAULT 0, retry_at INTEGER NOT NULL DEFAULT 0, output_override INTEGER, compact_only INTEGER NOT NULL DEFAULT 0, epoch INTEGER NOT NULL DEFAULT 0, failure TEXT, outcome TEXT REFERENCES artifacts(id));
                CREATE INDEX thread_runs ON runs(thread);
                CREATE INDEX run_outcomes ON runs(thread,outcome) WHERE outcome IS NOT NULL;
                CREATE INDEX run_states ON runs(state);
                CREATE INDEX live_runs ON runs(ordinal) WHERE state IN ('active','stopping');
                CREATE UNIQUE INDEX one_live_run ON runs(thread) WHERE state IN ('active','interrupted','stopping');
                CREATE TABLE commands(id TEXT PRIMARY KEY, digest TEXT NOT NULL, receipt TEXT NOT NULL);
                CREATE TABLE submissions(id TEXT PRIMARY KEY, thread TEXT NOT NULL REFERENCES threads(id), source TEXT NOT NULL, sender TEXT, text TEXT NOT NULL, state TEXT NOT NULL, run TEXT REFERENCES runs(id), owner_run TEXT REFERENCES runs(id));
                CREATE INDEX pending_submissions ON submissions(thread,state);
                CREATE TABLE artifacts(id TEXT PRIMARY KEY, length INTEGER NOT NULL);
                CREATE TABLE chunks(artifact TEXT NOT NULL REFERENCES artifacts(id), ordinal INTEGER NOT NULL, data BLOB NOT NULL, PRIMARY KEY(artifact,ordinal)) WITHOUT ROWID;
                CREATE TABLE entries(seq INTEGER PRIMARY KEY AUTOINCREMENT, thread TEXT NOT NULL REFERENCES threads(id), kind TEXT NOT NULL, text TEXT NOT NULL, model TEXT REFERENCES artifacts(id), operation TEXT, submission TEXT, result TEXT REFERENCES artifacts(id), model_order INTEGER);
                CREATE INDEX thread_entries ON entries(thread,seq);
                CREATE INDEX thread_model_order ON entries(thread,model_order);
                CREATE TABLE operations(id TEXT PRIMARY KEY, run TEXT NOT NULL REFERENCES runs(id), thread TEXT NOT NULL REFERENCES threads(id), name TEXT NOT NULL, kind TEXT NOT NULL, call_id TEXT, input TEXT NOT NULL, state TEXT NOT NULL, prepared TEXT, authorizations TEXT NOT NULL DEFAULT '[]', policy_revision INTEGER NOT NULL DEFAULT 0, result TEXT REFERENCES artifacts(id), display_result TEXT REFERENCES artifacts(id), presentation TEXT, effect_write INTEGER NOT NULL DEFAULT 0, epoch INTEGER NOT NULL DEFAULT 0, profile_revision INTEGER NOT NULL DEFAULT 0);
                CREATE INDEX runnable_operations ON operations(run,state);
                CREATE INDEX thread_operations ON operations(thread);
                CREATE INDEX thread_workflows ON operations(thread,name) WHERE kind='tool' AND name='workflow';
                CREATE INDEX operation_states ON operations(state);
                CREATE INDEX thread_approvals ON operations(thread) WHERE state='approval';
                CREATE TABLE decisions(operation TEXT PRIMARY KEY REFERENCES operations(id), revision INTEGER NOT NULL, preview TEXT NOT NULL, state TEXT NOT NULL, options TEXT NOT NULL DEFAULT '[]');
                CREATE TABLE changes(seq INTEGER PRIMARY KEY AUTOINCREMENT, thread TEXT, kind TEXT NOT NULL, reference TEXT NOT NULL);
                CREATE INDEX session_updated ON sessions(updated);
                CREATE TRIGGER session_activity AFTER INSERT ON changes WHEN NEW.thread IS NOT NULL BEGIN UPDATE sessions SET updated=NEW.seq,updated_at=unixepoch() WHERE id=(SELECT session FROM threads WHERE id=NEW.thread); END;
                CREATE TABLE model_context(thread TEXT PRIMARY KEY REFERENCES threads(id), system TEXT NOT NULL, epoch INTEGER NOT NULL, profile_revision INTEGER NOT NULL);
                CREATE TABLE compactions(thread TEXT PRIMARY KEY REFERENCES threads(id), state TEXT NOT NULL);
                CREATE TABLE contexts(thread TEXT PRIMARY KEY REFERENCES threads(id), epoch INTEGER NOT NULL DEFAULT 0, revision INTEGER NOT NULL DEFAULT 0, checkpoint TEXT, compact_pending INTEGER NOT NULL DEFAULT 0, newest_request TEXT NOT NULL DEFAULT '');
                CREATE TABLE working(ordinal INTEGER PRIMARY KEY AUTOINCREMENT, thread TEXT NOT NULL REFERENCES threads(id), model TEXT NOT NULL REFERENCES artifacts(id), tokens INTEGER NOT NULL DEFAULT 0);
                CREATE INDEX working_thread ON working(thread,ordinal);
                CREATE TABLE checkpoints(operation TEXT PRIMARY KEY, thread TEXT NOT NULL, epoch INTEGER NOT NULL, revision INTEGER NOT NULL, manifest TEXT NOT NULL REFERENCES artifacts(id));
                CREATE TABLE usage(operation TEXT PRIMARY KEY REFERENCES operations(id), thread TEXT NOT NULL, model TEXT NOT NULL, compact INTEGER NOT NULL, input INTEGER NOT NULL, output INTEGER NOT NULL, cache_write INTEGER NOT NULL, cache_read INTEGER NOT NULL);
                CREATE INDEX usage_thread ON usage(thread);
                CREATE TABLE human_controls(operation TEXT PRIMARY KEY REFERENCES operations(id), kind TEXT NOT NULL, revision INTEGER NOT NULL, epoch INTEGER NOT NULL, payload TEXT NOT NULL, state TEXT NOT NULL);
                CREATE TABLE todos(thread TEXT PRIMARY KEY REFERENCES threads(id), items TEXT NOT NULL);
                CREATE TRIGGER context_for_thread AFTER INSERT ON threads BEGIN INSERT INTO contexts(thread) VALUES(NEW.id); END;
                CREATE TRIGGER run_fence AFTER INSERT ON runs BEGIN UPDATE runs SET epoch=(SELECT epoch FROM contexts WHERE thread=NEW.thread) WHERE id=NEW.id; END;
                CREATE TRIGGER operation_fence AFTER INSERT ON operations BEGIN UPDATE operations SET epoch=(SELECT epoch FROM contexts WHERE thread=NEW.thread),profile_revision=(SELECT profile_revision FROM threads WHERE id=NEW.thread) WHERE id=NEW.id; END;
                CREATE TRIGGER working_entry AFTER INSERT ON entries WHEN NEW.model IS NOT NULL AND (NEW.operation IS NULL OR EXISTS(SELECT 1 FROM operations o JOIN contexts c ON c.thread=o.thread WHERE o.id=NEW.operation AND o.epoch=c.epoch)) BEGIN INSERT INTO working(thread,model,tokens) VALUES(NEW.thread,NEW.model,(SELECT (length+3)/4+4 FROM artifacts WHERE id=COALESCE(NEW.result,NEW.model))); UPDATE contexts SET revision=revision+1 WHERE thread=NEW.thread; END;
                CREATE TRIGGER working_consumed AFTER UPDATE OF model ON entries WHEN OLD.model IS NULL AND NEW.model IS NOT NULL BEGIN INSERT INTO working(thread,model,tokens) VALUES(NEW.thread,NEW.model,(SELECT (length+3)/4+4 FROM artifacts WHERE id=COALESCE(NEW.result,NEW.model))); UPDATE contexts SET revision=revision+1 WHERE thread=NEW.thread; END;
                {title_schema}
                {jobs_schema}
                {delegation_schema}
                {workflow_schema}
                CREATE INDEX workflow_node_children ON workflow_nodes(operation,child) WHERE child IS NOT NULL;
                COMMIT;", title_schema=crate::titles::SCHEMA, jobs_schema=crate::jobs::SCHEMA, delegation_schema=crate::delegation::SCHEMA, workflow_schema=crate::workflow::SCHEMA))?;
        }
        let version: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < SCHEMA {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if version == 8 {
                tx.execute_batch(crate::titles::SCHEMA)?;
            }
            tx.execute_batch(crate::ownership::SCHEMA_SQL)?;
            tx.execute_batch(crate::effect_claims::SCHEMA_SQL)?;
            tx.execute_batch(crate::visible_text::SCHEMA_SQL)?;
            tx.pragma_update(None, "user_version", SCHEMA)?;
            tx.commit()?;
        }
        if !current {
            lock.unlock()?;
            lock.try_lock_shared()
                .context("retain database compatibility guard")?;
        }
        Ok(Self {
            db,
            _lock: Some(lock),
            _database_file: database_file,
            path: Some(path),
            claim: None,
        })
    }

    pub(crate) fn adopt_claim(
        &mut self,
        claim: crate::SessionClaim,
        recover: bool,
    ) -> anyhow::Result<()> {
        self.adopt_claim_ref(&claim, recover)?;
        self.claim = Some(claim);
        Ok(())
    }

    pub(crate) fn adopt_claim_ref(
        &mut self,
        claim: &crate::SessionClaim,
        recover: bool,
    ) -> anyhow::Result<()> {
        ensure!(
            self.path.as_deref() == Some(claim.database_path()),
            "session claim belongs to another database"
        );
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let session = claim.session();
        tx.execute("INSERT INTO session_owners(session,generation) VALUES(?1,?2) ON CONFLICT(session) DO UPDATE SET generation=excluded.generation", params![session,claim.generation])?;
        if recover {
            crate::jobs::recover(&tx, session)?;
            crate::titles::recover(&tx, session)?;
            let rows: Vec<(String, String, String, String)> = {
                let mut stmt = tx.prepare(
                "SELECT id,thread,kind,name FROM operations WHERE state IN ('started','preparing') AND thread IN (SELECT id FROM threads WHERE session=?1)",
            )?;
                let mapped = stmt.query_map([session], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?;
                mapped.collect::<Result<_, _>>()?
            };
            for (id, thread, kind, name) in rows {
                let state: String =
                    tx.query_row("SELECT state FROM operations WHERE id=?1", [&id], |r| {
                        r.get(0)
                    })?;
                if state == "preparing" && kind == "tool" {
                    tx.execute("UPDATE operations SET state='pending' WHERE id=?1", [&id])?;
                } else if kind == "model" {
                    tx.execute(
                        "UPDATE operations SET state='interrupted' WHERE id=?1",
                        [&id],
                    )?;
                    entry(
                        &tx,
                        &ThreadId(thread.clone()),
                        "recovery",
                        "Model request interrupted; no incomplete tool calls were executed.",
                        EntryData {
                            operation: Some(&OperationId(id.clone())),
                            ..Default::default()
                        },
                    )?;
                } else {
                    tx.execute("UPDATE operations SET state='unknown' WHERE id=?1", [&id])?;
                    finish_tool_entry(&tx,&OperationId(id.clone()),&format!("Interrupted {name}; the outcome is unknown. Do not repeat this operation without inspecting its effects."),true)?;
                }
                change(&tx, Some(&ThreadId(thread)), "recovery", &id)?;
            }
            tx.execute("UPDATE threads SET held=1 WHERE session=?1 AND (id IN (SELECT thread FROM runs WHERE state IN ('active','stopping')) OR parent_run IN (SELECT id FROM runs WHERE state IN ('active','stopping','interrupted')) OR id IN (SELECT thread FROM agents WHERE state IN ('queued','running','unknown')))", [session])?;
            tx.execute(
            "UPDATE agents SET state='unknown' WHERE state IN ('queued','running') AND thread IN (SELECT id FROM threads WHERE session=?1)",
            [session],
        )?;
            tx.execute(
            "UPDATE runs SET state='interrupted' WHERE state='active' AND thread IN (SELECT id FROM threads WHERE session=?1)",
            [session],
        )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn ensure_owner(&self) -> anyhow::Result<()> {
        let claim = self
            .claim
            .as_ref()
            .context("runtime has no claimed session")?;
        let current: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_owners WHERE session=?1 AND generation=?2)",
            params![claim.session(), claim.generation],
            |r| r.get(0),
        )?;
        ensure!(current, "session execution owner generation changed");
        Ok(())
    }

    pub fn command(&mut self, command: &Command, models: &[ModelInfo]) -> anyhow::Result<Receipt> {
        self.command_with_titles(command, models, false)
    }

    pub fn command_with_titles(
        &mut self,
        command: &Command,
        models: &[ModelInfo],
        auto_titles: bool,
    ) -> anyhow::Result<Receipt> {
        self.command_with_session(command, models, auto_titles, None)
    }

    pub(crate) fn command_with_session(
        &mut self,
        command: &Command,
        models: &[ModelInfo],
        auto_titles: bool,
        new_session: Option<&SessionId>,
    ) -> anyhow::Result<Receipt> {
        let hash = digest(command)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT digest,receipt FROM commands WHERE id=?1",
                [command.id()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((old, receipt)) = existing {
            ensure!(
                old == hash,
                "command identity was already accepted with a different payload"
            );
            return decode(receipt);
        }
        let (session, thread, detail) = match command {
            Command::RecallSubmission { .. }
            | Command::Background { .. }
            | Command::StopJob { .. } => crate::jobs::command(&tx, command)?,
            Command::CreateSession {
                workspace,
                profile,
                policy,
                ..
            }
            | Command::CreateSessionConfigured {
                workspace,
                profile,
                policy,
                ..
            } => {
                validate_profile(profile, models)?;
                let path = std::fs::canonicalize(workspace).context("workspace must exist")?;
                ensure!(path.is_dir(), "workspace must be a directory");
                let session = new_session.cloned().unwrap_or_else(SessionId::new);
                let thread = ThreadId::new();
                tx.execute(
                    "INSERT INTO sessions(id,workspace,policy) VALUES(?1,?2,?3)",
                    params![session, path.to_string_lossy(), encode(policy)?],
                )?;
                tx.execute(
                    "INSERT INTO threads(id,session,name,profile) VALUES(?1,?2,'root',?3)",
                    params![thread, session, encode(profile)?],
                )?;
                if let Command::CreateSessionConfigured { permissions, .. } = command {
                    tx.execute(
                        "UPDATE sessions SET permissions=?1 WHERE id=?2",
                        params![encode(permissions)?, session],
                    )?;
                }
                (session, thread, "session created".to_owned())
            }
            Command::Submit { thread, text, id } => {
                ensure!(
                    !text.trim().is_empty() && text.len() <= 64 * 1024,
                    "input must contain 1..65536 bytes of text"
                );
                let session = thread_session(&tx, thread)?;
                submit(&tx, id, thread, "human", None, text)?;
                crate::lifecycle::continue_for_input(&tx, thread)?;
                let detail = crate::jobs::promote_for_input(&tx, thread)?
                    .unwrap_or_else(|| "input durably queued".into());
                (session, thread.clone(), detail)
            }
            Command::Stop { run, .. } => {
                let thread: String =
                    tx.query_row("SELECT thread FROM runs WHERE id=?1 AND state IN ('active','interrupted','stopping')", [run], |r| r.get(0))?;
                let thread = ThreadId(thread);
                let session = thread_session(&tx, &thread)?;
                crate::delegation::stop_run(&tx, run)?;
                (
                    session,
                    thread,
                    "stop requested; queued input is held".into(),
                )
            }
            Command::Resume { thread, .. } => {
                let session = thread_session(&tx, thread)?;
                let stopping: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runs WHERE thread=?1 AND state='stopping')",
                    [thread],
                    |r| r.get(0),
                )?;
                ensure!(!stopping, "wait for cancellation to settle before resuming");
                let unresolved: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM operations WHERE thread=?1 AND state='unknown'",
                    [thread],
                    |r| r.get(0),
                )?;
                ensure!(
                    unresolved == 0,
                    "inspect and resolve unknown operations before resuming"
                );
                let blocked: Option<String> = tx.query_row("SELECT json_extract(p.state,'$.blocked') FROM compactions p JOIN contexts c ON c.thread=p.thread JOIN threads t ON t.id=p.thread WHERE p.thread=?1 AND json_extract(p.state,'$.phase')='blocked' AND json_extract(p.state,'$.epoch')=c.epoch AND json_extract(p.state,'$.revision')=c.revision AND json_extract(p.state,'$.profile_revision')=t.profile_revision",[thread],|r|r.get(0)).optional()?;
                ensure!(blocked.is_none(), "{}", blocked.unwrap_or_default());
                tx.execute(
                    "UPDATE runs SET state='active' WHERE thread=?1 AND state='interrupted'",
                    [thread],
                )?;
                tx.execute("UPDATE threads SET held=0 WHERE id=?1", [thread])?;
                tx.execute("UPDATE agents SET state='running' WHERE thread=?1 AND state='unknown' AND EXISTS(SELECT 1 FROM runs WHERE thread=?1 AND state='active')", [thread])?;
                (session, thread.clone(), "resume accepted".into())
            }
            Command::Decide {
                operation,
                revision,
                approve,
                ..
            }
            | Command::DecideWithGrant {
                operation,
                revision,
                approve,
                ..
            } => {
                let thread: String = tx.query_row(
                    "SELECT thread FROM operations WHERE id=?1",
                    [operation],
                    |r| r.get(0),
                )?;
                let thread = ThreadId(thread);
                let session = thread_session(&tx, &thread)?;
                let current: i64 = tx.query_row(
                    "SELECT revision FROM sessions WHERE id=?1",
                    [&session],
                    |r| r.get(0),
                )?;
                if let Command::DecideWithGrant {
                    grant: Some(grant), ..
                } = command
                {
                    ensure!(*approve, "denials cannot create permission grants");
                    let options = crate::approvals::options(&tx, operation, *revision)?;
                    ensure!(
                        options
                            .options
                            .iter()
                            .any(|option| option.approve && option.grant.as_ref() == Some(grant)),
                        "grant was not offered for this prepared operation"
                    );
                }
                ensure!(current == *revision, "decision is stale; policy changed");
                let changed=tx.execute("UPDATE decisions SET state=?1 WHERE operation=?2 AND revision=?3 AND state='pending'",params![if *approve {"approved"} else {"denied"},operation,revision])?;
                ensure!(changed == 1, "decision is no longer pending");
                let state = if *approve { "ready" } else { "failed" };
                let changed=tx.execute("UPDATE operations SET state=?1 WHERE id=?2 AND state='approval' AND run IN (SELECT id FROM runs WHERE state='active')",params![state,operation])?;
                ensure!(changed == 1, "owning run is no longer active");
                if !approve {
                    crate::workflow::fail(&tx, operation, "Permission denied by user.")?;
                    finish_tool_entry(&tx, operation, "Permission denied by user.", true)?;
                }
                if let Command::DecideWithGrant {
                    grant: Some(grant), ..
                } = command
                {
                    ensure!(*approve, "denials cannot create permission grants");
                    crate::session_commands::grant_approved(&tx, &session, operation, grant)?;
                }
                (
                    session,
                    thread,
                    format!("decision {}", if *approve { "approved" } else { "denied" }),
                )
            }
            Command::SetPolicy {
                session,
                policy,
                expected_revision,
                ..
            } => {
                ensure!(tx.execute("UPDATE sessions SET policy=?1,revision=revision+1 WHERE id=?2 AND revision=?3",params![encode(policy)?,session,expected_revision])?==1,"session policy changed; refresh before updating it");
                let thread:String=tx.query_row("SELECT id FROM threads WHERE session=?1 AND parent_run IS NULL ORDER BY rowid LIMIT 1",[session],|r|r.get(0))?;
                tx.execute("UPDATE decisions SET state='stale' WHERE operation IN (SELECT o.id FROM operations o JOIN threads t ON t.id=o.thread WHERE t.session=?1 AND o.state IN ('approval','ready'))",[session])?;
                tx.execute("UPDATE operations SET state='pending',prepared=NULL WHERE thread IN (SELECT id FROM threads WHERE session=?1) AND state IN ('approval','ready')",[session])?;
                let mut permissions: crate::PermissionPolicy = decode(tx.query_row(
                    "SELECT permissions FROM sessions WHERE id=?1",
                    [session],
                    |r| r.get(0),
                )?)?;
                if policy.plan {
                    permissions.mode = crate::PermissionMode::Plan;
                } else if permissions.mode == crate::PermissionMode::Plan {
                    permissions.mode = crate::PermissionMode::Normal;
                }
                tx.execute(
                    "UPDATE sessions SET permissions=?1 WHERE id=?2",
                    params![encode(&permissions)?, session],
                )?;
                crate::session_commands::invalidate_approvals(&tx, session)?;
                (
                    session.clone(),
                    ThreadId(thread),
                    "policy updated; previous approvals invalidated".into(),
                )
            }
            Command::ResolveUnknown {
                operation,
                evidence,
                ..
            } => {
                ensure!(
                    !evidence.trim().is_empty() && evidence.len() <= 65536,
                    "supply bounded recovery evidence"
                );
                let thread: String = tx.query_row(
                    "SELECT o.thread FROM operations o WHERE o.id=?1 AND (o.state='unknown' OR (o.state='done' AND COALESCE(json_extract(o.input,'$.run_in_background'),0)=1 AND EXISTS(SELECT 1 FROM jobs j WHERE j.operation=o.id AND j.kind='shell' AND j.background=1 AND j.state='unknown')))",
                    [operation],
                    |r| r.get(0),
                )?;
                let thread = ThreadId(thread);
                let session = thread_session(&tx, &thread)?;
                tx.execute(
                    "UPDATE operations SET state='resolved' WHERE id=?1 AND state='unknown'",
                    [operation],
                )?;
                tx.execute(
                    "UPDATE jobs SET state='cancelled' WHERE operation=?1 AND state='unknown'",
                    [operation],
                )?;
                crate::effect_claims::release_settled(&tx, operation)?;
                submit(
                    &tx,
                    command.id(),
                    &thread,
                    "recovery",
                    None,
                    &format!("Recovery evidence for {operation}: {evidence}. The original operation was not rerun."),
                )?;
                (
                    session,
                    thread,
                    "uncertainty acknowledged with evidence; no operation rerun".into(),
                )
            }
            other => crate::session_commands::apply(&tx, other, models)?,
        };
        crate::titles::command(&tx, command, &session, &thread, auto_titles)?;
        let sequence = change(&tx, Some(&thread), "command", &command.id().0)?;
        let receipt = Receipt {
            session,
            thread,
            sequence,
            detail,
        };
        tx.execute(
            "INSERT INTO commands(id,digest,receipt) VALUES(?1,?2,?3)",
            params![command.id(), hash, encode(&receipt)?],
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn read_only(db: Connection) -> Self {
        Self {
            db,
            _lock: None,
            _database_file: None,
            path: None,
            claim: None,
        }
    }

    pub fn view(
        &self,
        thread: Option<ThreadId>,
        before: Option<i64>,
        limit: usize,
        error: Option<String>,
    ) -> anyhow::Result<View> {
        self.read_view(thread, before, limit, error, true, false)
    }

    pub fn transcript(
        &self,
        thread: Option<ThreadId>,
        before: Option<i64>,
        limit: usize,
        error: Option<String>,
    ) -> anyhow::Result<View> {
        self.read_view(thread, before, limit, error, false, false)
    }

    pub fn transcript_after(
        &self,
        thread: Option<ThreadId>,
        after: i64,
        limit: usize,
        error: Option<String>,
    ) -> anyhow::Result<View> {
        self.read_view(thread, Some(after), limit, error, false, true)
    }

    fn read_view(
        &self,
        thread: Option<ThreadId>,
        before: Option<i64>,
        limit: usize,
        error: Option<String>,
        detailed: bool,
        ascending: bool,
    ) -> anyhow::Result<View> {
        let limit = limit.clamp(1, 100);
        let snapshot = self.db.unchecked_transaction()?;
        let sequence =
            snapshot.query_row("SELECT COALESCE(MAX(seq),0) FROM changes", [], |r| r.get(0))?;
        let entry_sequence =
            snapshot.query_row("SELECT COALESCE(MAX(seq),0) FROM entries", [], |r| r.get(0))?;
        let thread = thread.or_else(|| {
            self.claim.as_ref().and_then(|claim| {
                self.db
                    .query_row(
                        "SELECT id FROM threads WHERE session=?1 AND parent_run IS NULL",
                        [claim.session()],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                    .map(ThreadId)
            })
        });
        let threads = {
            let mut stmt=self.db.prepare("SELECT t.id,t.session,t.parent_run,t.name,json_extract(t.profile,'$.model'),COALESCE(r.state,(SELECT CASE WHEN a.state IN ('completed','failed','cancelled') THEN a.state END FROM agents a WHERE a.thread=t.id),CASE WHEN t.held=1 THEN 'held' ELSE 'idle' END),r.id,parent.thread,EXISTS(SELECT 1 FROM operations o JOIN decisions d ON d.operation=o.id WHERE o.run=r.id AND r.state='active' AND o.state='approval' AND d.state='pending' AND d.revision=o.policy_revision AND d.revision=(SELECT revision FROM sessions WHERE id=t.session)),(SELECT json_object('name',a.name,'kind',a.kind,'state',a.state) FROM operations a WHERE a.run=r.id AND r.state='active' AND a.state IN ('pending','preparing','started','approval','ready','waiting') ORDER BY a.rowid LIMIT 1) FROM threads t LEFT JOIN runs r ON r.rowid=(SELECT MAX(x.rowid) FROM runs x WHERE x.thread=t.id) LEFT JOIN runs parent ON parent.id=t.parent_run WHERE t.session=COALESCE((SELECT session FROM threads WHERE id=?1),(SELECT session FROM threads WHERE parent_run IS NULL ORDER BY rowid DESC LIMIT 1)) AND t.rowid IN (
                SELECT rowid FROM (SELECT rowid FROM threads WHERE session=COALESCE((SELECT session FROM threads WHERE id=?1),(SELECT session FROM threads WHERE parent_run IS NULL ORDER BY rowid DESC LIMIT 1)) ORDER BY rowid DESC LIMIT 1000)
                UNION SELECT rowid FROM threads WHERE id=?1
                UNION SELECT COALESCE(
                    (SELECT MIN(rowid) FROM threads WHERE session=(SELECT session FROM threads WHERE id=?1)),
                    (SELECT rowid FROM threads WHERE parent_run IS NULL ORDER BY rowid DESC LIMIT 1))
                ) ORDER BY t.rowid DESC")?;
            let rows = stmt.query_map([&thread], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, bool>(8)?,
                    r.get::<_, Option<String>>(9)?,
                ))
            })?;
            rows.map(|r| {
                let (
                    id,
                    session,
                    parent,
                    name,
                    model,
                    state,
                    run,
                    parent_thread,
                    needs_approval,
                    activity,
                ) = r?;
                Ok(ThreadSummary {
                    id: ThreadId(id),
                    session: SessionId(session),
                    parent_run: parent.map(RunId),
                    parent_thread: parent_thread.map(ThreadId),
                    needs_approval,
                    activity: activity
                        .map(|value| decode::<ActivityRecord>(value)?.project())
                        .transpose()?,
                    name,
                    model,
                    state: state.parse()?,
                    run: run.map(RunId),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
        };
        let mut entries = Vec::new();
        let mut decisions = Vec::new();
        let mut operations = Vec::new();
        if let Some(thread) = thread.as_ref() {
            let mut sql = if ascending {
                PAGE_SQL.replace(
                    "e.seq<?2 ORDER BY e.seq DESC",
                    "e.seq>?2 ORDER BY e.seq ASC",
                )
            } else {
                PAGE_SQL.to_owned()
            };
            let has_visible: bool = self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='assistant_visible_text')",
                [],
                |r| r.get(0),
            )?;
            if !has_visible {
                sql = sql.replace(
                    "assistant_visible_text visible",
                    "(SELECT NULL AS source,NULL AS total_bytes,1 AS version) visible",
                );
            }
            let mut stmt = self.db.prepare(&sql)?;
            let rows = stmt.query_map(params![thread, before.unwrap_or(i64::MAX), limit], |r| {
                let text: String = r.get(2)?;
                let tool = if let Some(id) = r.get::<_, Option<String>>(5)? {
                    let input: String = r.get(8)?;
                    Some(ToolSummary {
                        id: OperationId(id),
                        name: r.get(6)?,
                        state: state_column(r, 7)?,
                        presentation: optional_json_column(r, 10)?,
                        result: r.get::<_, Option<String>>(9)?.map(ArtifactId),
                        input_preview: serde_json::from_str(&input).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                8,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?,
                    })
                } else {
                    None
                };
                Ok(Entry {
                    sequence: r.get(0)?,
                    thread: thread.clone(),
                    kind: state_column(r, 1)?,
                    sender: match r.get::<_, Option<String>>(11)? {
                        Some(id) => Some(EntrySender {
                            thread: ThreadId(id),
                            name: r.get(12)?,
                        }),
                        None => None,
                    },
                    parent_guidance: r.get(13)?,
                    submission: r
                        .get::<_, Option<String>>(14)?
                        .map(|id| {
                            Ok::<_, rusqlite::Error>(EntrySubmission {
                                id: CommandId(id),
                                state: crate::jobs::submission_state(
                                    r.get::<_, String>(15)?.as_str(),
                                )?,
                                sequence,
                                ordinal: r.get(16)?,
                            })
                        })
                        .transpose()?,
                    is_preview: r.get::<_, Option<usize>>(17)?.map_or_else(
                        || text.chars().count() >= 2048,
                        |length| length > text.len(),
                    ),
                    visible_text_bytes: r.get(17)?,
                    text,
                    artifact: r.get::<_, Option<String>>(3)?.map(ArtifactId),
                    operation: r.get::<_, Option<String>>(4)?.map(OperationId),
                    tool,
                })
            })?;
            entries = rows.collect::<Result<_, _>>()?;
            if !ascending {
                entries.reverse();
            }
            let mut stmt=self.db.prepare("SELECT d.operation,d.revision,d.preview,o.name,CASE WHEN length(json_extract(o.prepared,'$.presentation'))<=32768 THEN json_extract(o.prepared,'$.presentation') END FROM decisions d JOIN operations o ON o.id=d.operation JOIN runs r ON r.id=o.run JOIN threads dt ON dt.id=o.thread JOIN sessions ds ON ds.id=dt.session WHERE o.thread=?1 AND r.state='active' AND d.state='pending' AND o.state='approval' AND d.revision=o.policy_revision AND d.revision=ds.revision ORDER BY o.rowid LIMIT 100")?;
            let rows = stmt.query_map([&thread], |r| {
                Ok(Decision {
                    operation: OperationId(r.get(0)?),
                    thread: thread.clone(),
                    revision: r.get(1)?,
                    preview: r.get(2)?,
                    name: r.get(3)?,
                    presentation: optional_json_column(r, 4)?,
                })
            })?;
            decisions = rows.collect::<Result<_, _>>()?;
            if detailed {
                let mut stmt=self.db.prepare("SELECT id,run,name,state,CASE WHEN length(COALESCE(prepared,input))<=65536 THEN COALESCE(prepared,input) ELSE json_object('preview_truncated',json('true')) END,result FROM operations WHERE thread=?1 ORDER BY rowid DESC LIMIT 100")?;
                let rows = stmt.query_map([&thread], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    ))
                })?;
                for row in rows {
                    let (id, run, name, state, specification, result) = row?;
                    operations.push(OperationSummary {
                        id: OperationId(id),
                        run: RunId(run),
                        name,
                        state: state.parse()?,
                        specification: decode(specification)?,
                        result: result.map(ArtifactId),
                    });
                }
            }
        }
        let policies = {
            let mut stmt = self.db.prepare(
                "SELECT id,policy,revision FROM sessions WHERE id=COALESCE((SELECT session FROM threads WHERE id=?1),(SELECT session FROM threads WHERE parent_run IS NULL ORDER BY rowid DESC LIMIT 1))",
            )?;
            let rows = stmt.query_map([&thread], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            rows.map(|row| {
                let (id, policy, revision) = row?;
                Ok(SessionPolicy {
                    session: SessionId(id),
                    policy: decode(policy)?,
                    revision,
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
        };
        snapshot.commit()?;
        Ok(View {
            policies,
            sequence,
            entry_sequence,
            threads,
            entries,
            decisions,
            operations,
            error,
        })
    }

    pub fn entry_text(
        &self,
        thread: &ThreadId,
        sequence: i64,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<TextPage> {
        let (kind, preview, model, result, operation, submitted): (String, String, Option<String>, Option<String>, Option<String>, Option<String>) = self.db.query_row(
            "SELECT e.kind,e.text,e.model,e.result,e.operation,s.text FROM entries e LEFT JOIN submissions s ON s.id=e.submission WHERE e.thread=?1 AND e.seq=?2",
            params![thread, sequence],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)),
        )?;
        let text = if let Some(text) = submitted {
            text
        } else if kind == "assistant" {
            return crate::visible_text::Cache::default().page(
                &self.db,
                &ArtifactId(model.or(result).context("assistant content missing")?),
                offset,
                limit,
            );
        } else if matches!(kind.as_str(), "tool_result" | "tool_error") {
            let id = ArtifactId(result.context("tool output missing")?);
            let total_bytes =
                self.db
                    .query_row("SELECT length FROM artifacts WHERE id=?1", [&id], |r| {
                        r.get(0)
                    })?;
            let text = self.artifact_page(&id, offset, limit.clamp(4, 65536))?;
            let end = offset + text.len();
            return Ok(TextPage {
                text,
                next_offset: (end < total_bytes).then_some(end),
                total_bytes,
            });
        } else if kind == "tool_call" {
            let id = operation.context("tool operation missing")?;
            let (name, state, specification): (String, String, String) = self.db.query_row(
                "SELECT name,state,COALESCE(prepared,input) FROM operations WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            format!("{name} · {state}\nOperation: {id}\n{specification}")
        } else {
            preview
        };
        ensure!(
            offset <= text.len() && text.is_char_boundary(offset),
            "entry offset must be a retained UTF-8 boundary"
        );
        let mut end = offset.saturating_add(limit.clamp(4, 65536)).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Ok(TextPage {
            text: text[offset..end].to_owned(),
            next_offset: (end < text.len()).then_some(end),
            total_bytes: text.len(),
        })
    }

    pub fn tool_output(
        &self,
        operation: &OperationId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<TextPage> {
        let id: Option<String> = self.db.query_row(
            "SELECT COALESCE(display_result,result) FROM operations WHERE id=?1 AND kind='tool'",
            [operation],
            |r| r.get(0),
        )?;
        let id = ArtifactId(id.context("tool output is not available yet")?);
        let total_bytes: usize =
            self.db
                .query_row("SELECT length FROM artifacts WHERE id=?1", [&id], |r| {
                    r.get(0)
                })?;
        let text = self.artifact_page(&id, offset, limit.clamp(4, 65536))?;
        let end = offset + text.len();
        Ok(TextPage {
            text,
            next_offset: (end < total_bytes).then_some(end),
            total_bytes,
        })
    }

    pub fn artifact_page(
        &self,
        id: &ArtifactId,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<String> {
        artifact_page(&self.db, id, offset, limit)
    }
}

pub(crate) fn artifact_page(
    db: &Connection,
    id: &ArtifactId,
    offset: usize,
    limit: usize,
) -> anyhow::Result<String> {
    let length: usize = db.query_row("SELECT length FROM artifacts WHERE id=?1", [id], |r| {
        r.get(0)
    })?;
    ensure!(
        offset <= length,
        "artifact offset exceeds retained length {length}"
    );
    if offset == length {
        return Ok(String::new());
    }
    let end = offset
        .saturating_add(limit.clamp(1, 256 * 1024))
        .min(length);
    let mut stmt = db.prepare(
        "SELECT data FROM chunks WHERE artifact=?1 AND ordinal BETWEEN ?2 AND ?3 ORDER BY ordinal",
    )?;
    let rows = stmt.query_map(params![id, offset / CHUNK, (end - 1) / CHUNK], |r| {
        r.get::<_, Vec<u8>>(0)
    })?;
    let mut bytes = Vec::new();
    for row in rows {
        bytes.extend(row?);
    }
    let start = offset % CHUNK;
    let mut end = end - offset + start;
    if let Err(error) = std::str::from_utf8(&bytes[start..end]) {
        if error.error_len().is_some() {
            return Err(error.into());
        }
        end = start + error.valid_up_to();
    }
    ensure!(
        end > start,
        "artifact page limit is too small for the next UTF-8 character"
    );
    Ok(std::str::from_utf8(&bytes[start..end])?.to_owned())
}

pub(crate) fn validate_profile(profile: &Profile, models: &[ModelInfo]) -> anyhow::Result<()> {
    ensure!(
        models.iter().any(|m| m.id == profile.model),
        "unsupported model {}; select an advertised model",
        profile.model
    );
    ensure!(profile.max_requests > 0, "request budget must be positive");
    ensure!(
        profile.instructions.len() <= 65536,
        "instructions too large"
    );
    Ok(())
}
pub(crate) fn thread_session(db: &Connection, thread: &ThreadId) -> anyhow::Result<SessionId> {
    Ok(SessionId(db.query_row(
        "SELECT session FROM threads WHERE id=?1",
        [thread],
        |r| r.get(0),
    )?))
}
pub(crate) fn submit(
    db: &Connection,
    id: &CommandId,
    thread: &ThreadId,
    source: &str,
    sender: Option<&ThreadId>,
    text: &str,
) -> anyhow::Result<()> {
    db.execute("INSERT INTO submissions(id,thread,source,sender,text,state,owner_run) VALUES(?1,?2,?3,?4,?5,CASE WHEN ?3='coordination' AND NOT EXISTS(SELECT 1 FROM runs WHERE thread=?2 AND state IN ('active','interrupted')) AND NOT EXISTS(SELECT 1 FROM agents WHERE thread=?2 AND state='queued' AND NOT EXISTS(SELECT 1 FROM runs WHERE thread=?2)) THEN 'retained' ELSE 'queued' END,CASE WHEN ?3='coordination' THEN (SELECT id FROM runs WHERE thread=?2 AND state IN ('active','interrupted') ORDER BY ordinal DESC LIMIT 1) END)",params![id,thread,source,sender,text])?;
    entry(
        db,
        thread,
        match source {
            "human" | "steering" => "submission",
            "recovery" => "recovery",
            _ => "coordination",
        },
        text,
        EntryData {
            submission: Some(id),
            ..Default::default()
        },
    )?;
    Ok(())
}
pub(crate) fn consume(db: &Connection, id: &CommandId, run: &RunId) -> anyhow::Result<()> {
    let (text, source, sender): (String, String, Option<String>) = db.query_row(
        "SELECT text,source,sender FROM submissions WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    if matches!(source.as_str(), "human" | "steering") {
        db.execute("UPDATE contexts SET newest_request=?1 WHERE thread=(SELECT thread FROM submissions WHERE id=?2)",params![text,id])?;
    }
    let message = Message::user_text(if source != "coordination" {
        text
    } else {
        format!(
            "[Coordination from {} — not a human instruction]\n{text}",
            sender.unwrap_or_else(|| "runtime".into())
        )
    });
    let model = artifact(db, &encode(&ModelRecord::Message(message))?)?;
    db.execute(
        "UPDATE submissions SET state='in_use',run=?1 WHERE id=?2 AND state='queued'",
        params![run, id],
    )?;
    db.execute(
        "UPDATE entries SET model=?1,model_order=(SELECT COALESCE(MAX(model_order),0)+1 FROM entries e WHERE e.thread=entries.thread) WHERE submission=?2 AND thread=(SELECT thread FROM submissions WHERE id=?2)",
        params![model, id],
    )?;
    Ok(())
}
pub(crate) fn finish_tool_entry(
    db: &Connection,
    op: &OperationId,
    text: &str,
    is_error: bool,
) -> anyhow::Result<()> {
    let (thread, call): (String, String) = db.query_row(
        "SELECT thread,call_id FROM operations WHERE id=?1",
        [op],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let output = artifact(db, text)?;
    let message = Message {
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: call,
            content: text.into(),
            is_error: Some(is_error),
        }],
    };
    db.execute(
        "UPDATE operations SET result=?1 WHERE id=?2",
        params![output, op],
    )?;
    entry(
        db,
        &ThreadId(thread),
        if is_error {
            "tool_error"
        } else {
            "tool_result"
        },
        text,
        EntryData {
            message: Some(&message),
            operation: Some(op),
            result: Some(&output),
            ..Default::default()
        },
    )?;
    Ok(())
}
pub(crate) fn settle_run(
    tx: &Transaction<'_>,
    run: &RunId,
    state: &str,
    text: &str,
) -> anyhow::Result<()> {
    let (thread, previous): (String, String) =
        tx.query_row("SELECT thread,state FROM runs WHERE id=?1", [run], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    if !matches!(previous.as_str(), "active" | "interrupted" | "stopping") {
        return Ok(());
    }
    let thread = ThreadId(thread);
    if state == "completed" {
        crate::lifecycle::collect(tx, run)?;
        ensure!(
            !crate::lifecycle::pending(tx, run)?
                && !crate::lifecycle::blocked(tx, run)?
                && !crate::lifecycle::actionable(tx, run)?,
            "owned agent work or accepted coordination remains unsettled"
        );
    }
    let changed = tx.execute(
        "UPDATE runs SET state=?1 WHERE id=?2 AND state IN ('active','interrupted','stopping')",
        params![state, run],
    )?;
    if changed == 0 {
        return Ok(());
    }
    crate::jobs::settle_turn(tx, run, state, text)?;
    crate::delegation::settle(tx, &thread, state, text)?;
    tx.execute("UPDATE runs SET outcome=(SELECT outcome FROM agents WHERE thread=?2) WHERE id=?1 AND outcome IS NULL", params![run,thread])?;
    tx.execute("UPDATE submissions SET state='settled' WHERE run=?1", [run])?;
    tx.execute(
        "UPDATE submissions SET owner_run=NULL WHERE owner_run=?1 AND state='queued' AND source='human'",
        [run],
    )?;
    tx.execute(
        "UPDATE submissions SET state='cancelled' WHERE owner_run=?1 AND state='queued' AND source!='human'",
        [run],
    )?;
    crate::lifecycle::publish(tx, &thread)?;
    change(tx, Some(&thread), "run_settled", &run.0)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use bob_types::ReasoningEffort;
    use rusqlite::StatementStatus;
    use std::{path::PathBuf, time::Instant};

    pub(crate) struct TempDir(pub PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn fixture() -> (TempDir, Store, Receipt, Vec<ModelInfo>) {
        let dir =
            TempDir(std::env::temp_dir().join(format!("bob-kernel-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir(&dir.0).unwrap();
        let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
        let models = vec![ModelInfo {
            id: "test".into(),
            context_window: 10_000_000,
        }];
        let receipt = store
            .command(
                &Command::CreateSession {
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
                },
                &models,
            )
            .unwrap();
        (dir, store, receipt, models)
    }

    #[test]
    fn view_entry_head_is_global_and_independent_of_changes_and_page_bounds() {
        let (dir, mut store, receipt, models) = fixture();
        let empty = store.view(None, None, 1, None).unwrap();
        assert_eq!(empty.entry_sequence, 0);
        for revision in 0..100 {
            store
                .command(
                    &Command::SetPolicy {
                        id: CommandId::new(),
                        session: receipt.session.clone(),
                        policy: Policy {
                            yolo: revision % 2 == 0,
                            ..Policy::default()
                        },
                        expected_revision: revision,
                    },
                    &models,
                )
                .unwrap();
        }
        let changed = store.view(None, None, 1, None).unwrap();
        assert!(changed.sequence >= empty.sequence + 100);
        assert_eq!(changed.entry_sequence, 0);
        for text in ["Earlier answer", "Later answer"] {
            entry(
                &store.db,
                &receipt.thread,
                "assistant",
                text,
                EntryData::default(),
            )
            .unwrap();
        }
        let other = ThreadId::new();
        store.db.execute(
            "INSERT INTO threads(id,session,name,profile) SELECT ?1,session,'other',profile FROM threads WHERE id=?2",
            params![other, receipt.thread],
        ).unwrap();
        entry(
            &store.db,
            &other,
            "assistant",
            "Other thread",
            EntryData::default(),
        )
        .unwrap();
        let catalog = crate::SessionCatalog::open(&dir.0.join("lab.db")).unwrap();
        for view in [
            store
                .view(Some(receipt.thread.clone()), None, 1, None)
                .unwrap(),
            store
                .transcript(Some(receipt.thread.clone()), Some(2), 1, None)
                .unwrap(),
            store
                .transcript_after(Some(receipt.thread.clone()), 1, 1, None)
                .unwrap(),
            catalog.view(Some(receipt.thread.clone()), None, 1).unwrap(),
            catalog
                .transcript(Some(receipt.thread.clone()), Some(2), 1)
                .unwrap(),
            catalog
                .transcript_after(Some(receipt.thread.clone()), 1, 1)
                .unwrap(),
        ] {
            assert_eq!(view.entry_sequence, 3);
            assert!(view.sequence > view.entry_sequence + 90);
            assert_eq!(view.entries.len(), 1);
            assert_eq!(view.entries[0].thread, receipt.thread);
            assert!(view.entries[0].sequence < view.entry_sequence);
        }
        let page = catalog
            .transcript(Some(receipt.thread.clone()), Some(2), 1)
            .unwrap();
        assert_eq!(page.entries[0].sequence, 1);
        let page = catalog
            .transcript_after(Some(receipt.thread), 1, 1)
            .unwrap();
        assert_eq!(page.entries[0].sequence, 2);
        assert_eq!(catalog.view(None, None, 1).unwrap().entry_sequence, 3);
    }

    #[test]
    fn failed_receipt_commit_rolls_back_the_entire_admission() {
        let (_dir, mut store, receipt, models) = fixture();
        let before = store.view(None, None, 1, None).unwrap().sequence;
        store.db.execute_batch("CREATE TRIGGER receipt_failure BEFORE INSERT ON commands BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;").unwrap();
        let command = Command::Submit {
            id: CommandId::new(),
            thread: receipt.thread.clone(),
            text: "accepted only with its receipt".into(),
        };
        assert!(store
            .command(&command, &models)
            .unwrap_err()
            .to_string()
            .contains("injected receipt failure"));
        assert_eq!(
            store
                .db
                .query_row("SELECT COUNT(*) FROM submissions", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let view = store
            .view(Some(receipt.thread.clone()), None, 100, None)
            .unwrap();
        assert!(view.entries.is_empty());
        assert_eq!(view.sequence, before);
        store
            .db
            .execute_batch("DROP TRIGGER receipt_failure")
            .unwrap();
        let accepted = store.command(&command, &models).unwrap();
        assert!(accepted.sequence > before);
        assert_eq!(
            store.command(&command, &models).unwrap().sequence,
            accepted.sequence
        );
    }

    #[test]
    fn ownership_and_foreign_database_boundaries_fail_closed() {
        let (dir, store, root, _) = fixture();
        let claim =
            crate::SessionClaim::acquire(&dir.0.join("lab.db"), root.session.clone()).unwrap();
        assert!(Store::open(&dir.0.join("lab.db")).is_ok());
        assert!(
            crate::SessionClaim::acquire(&dir.0.join("lab.db"), root.session)
                .err()
                .unwrap()
                .to_string()
                .contains("already open in another bob process")
        );
        drop(claim);
        drop(store);
        let foreign = dir.0.join("unrelated.db");
        Connection::open(&foreign)
            .unwrap()
            .execute_batch(
                "CREATE TABLE precious(value TEXT); INSERT INTO precious VALUES('unchanged');",
            )
            .unwrap();
        let bytes = std::fs::read(&foreign).unwrap();
        assert!(Store::open(&foreign)
            .err()
            .unwrap()
            .to_string()
            .contains("refusing"));
        assert_eq!(std::fs::read(&foreign).unwrap(), bytes);
    }

    #[test]
    fn hard_link_database_aliases_are_rejected_without_second_owner() {
        let (dir, store, _, _) = fixture();
        let path = dir.0.join("lab.db");
        let alias = dir.0.join("alias.db");
        drop(store);
        std::fs::hard_link(&path, &alias).unwrap();
        for candidate in [&path, &alias] {
            let error = Store::open(candidate).err().unwrap();
            assert!(error.to_string().contains("hard-linked"), "{error:#}");
        }
        assert!(Store::open(&alias)
            .err()
            .unwrap()
            .to_string()
            .contains("hard-linked"));
        std::fs::remove_file(alias).unwrap();
        assert!(Store::open(&path).is_ok());
    }

    #[test]
    fn equivalent_database_paths_share_one_execution_owner() {
        let (dir, store, root, _) = fixture();
        let alias = dir.0.join(".").join("lab.db");
        let canonical = std::fs::canonicalize(&alias).unwrap();
        let claim = crate::SessionClaim::acquire(&canonical, root.session.clone()).unwrap();
        for candidate in [&alias, &canonical] {
            let error = crate::SessionClaim::acquire(candidate, root.session.clone())
                .err()
                .unwrap();
            assert!(
                error
                    .to_string()
                    .contains("already open in another bob process"),
                "{error:#}"
            );
        }
        #[cfg(windows)]
        {
            let different_case = dir.0.join("LAB.DB");
            if different_case.exists() {
                let error = crate::SessionClaim::acquire(&different_case, root.session.clone())
                    .err()
                    .unwrap();
                assert!(
                    error
                        .to_string()
                        .contains("already open in another bob process"),
                    "{error:#}"
                );
            }
        }
        drop(claim);
        drop(store);
        assert!(crate::SessionClaim::acquire(&alias, root.session).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn owned_database_cannot_be_replaced_or_deleted() {
        let (dir, store, _, _) = fixture();
        let path = dir.0.join("lab.db");
        assert!(std::fs::remove_file(&path).is_err());
        assert!(std::fs::rename(&path, dir.0.join("moved.db")).is_err());
        drop(store);
        std::fs::rename(&path, dir.0.join("moved.db")).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn database_identity_handle_allows_sqlite_wal_and_concurrent_readers() {
        let (dir, store, _, _) = fixture();
        let path = dir.0.join("lab.db");
        store.db.execute_batch("CREATE TABLE ownership_probe(value INTEGER); INSERT INTO ownership_probe VALUES(1);").unwrap();
        let reader =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let value: i64 = reader
            .query_row("SELECT value FROM ownership_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 1);
        store
            .db
            .execute_batch("UPDATE ownership_probe SET value=2; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let value: i64 = reader
            .query_row("SELECT value FROM ownership_probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(value, 2);
        drop(reader);
        drop(store);
        assert!(Store::open(&path).is_ok());
    }

    #[test]
    #[ignore = "release long-history measurement"]
    fn long_history_reads_are_bounded() {
        let mut previous_steps = None;
        for size in [10_000, 100_000] {
            let (dir, mut store, receipt, _) = fixture();
            let tx = store.db.transaction().unwrap();
            let cold = artifact(&tx, &"cold output\n".repeat(12_000)).unwrap();
            tx.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?2) INSERT INTO runs(id,thread,state) SELECT 'r-'||x,?1,'completed' FROM n", params![receipt.thread,size]).unwrap();
            tx.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state,result) SELECT 'o-'||ordinal,id,thread,'shell','tool','call-'||ordinal,'{}','done',?1 FROM runs", [&cold]).unwrap();
            tx.execute("INSERT INTO entries(thread,kind,text,result,operation) SELECT thread,'tool_result',?1,result,id FROM operations", ["bounded preview ".repeat(64)]).unwrap();
            tx.commit().unwrap();
            drop(store);
            let start = Instant::now();
            let store = Store::open(&dir.0.join("lab.db")).unwrap();
            let first = store
                .transcript(Some(receipt.thread.clone()), None, 100, None)
                .unwrap();
            let reopen = start.elapsed();
            assert_eq!(first.entries.len(), 100);
            assert!(first.operations.is_empty());
            assert_eq!(first.threads.len(), 1);
            let start = Instant::now();
            let page = store
                .transcript(Some(receipt.thread.clone()), Some(size / 2), 100, None)
                .unwrap();
            let page_time = start.elapsed();
            assert_eq!(page.entries.len(), 100);
            assert!(page
                .entries
                .iter()
                .all(|entry| entry
                    .tool
                    .as_ref()
                    .is_some_and(|tool| entry.operation.as_ref() == Some(&tool.id)
                        && tool.state == OperationState::Done)));
            assert_eq!(
                store
                    .view(Some(receipt.thread.clone()), None, 100, None)
                    .unwrap()
                    .operations
                    .len(),
                100
            );
            let mut stmt = store.db.prepare(PAGE_SQL).unwrap();
            let rows = stmt
                .query_map(params![receipt.thread, size / 2, 100], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(rows.len(), 100);
            assert_eq!(stmt.get_status(StatementStatus::FullscanStep), 0);
            let steps = stmt.get_status(StatementStatus::VmStep);
            if let Some(previous) = previous_steps {
                assert_eq!(steps, previous);
            }
            previous_steps = Some(steps);
            println!("{size} historical runs/operations/entries: reopen + first page {reopen:?}; older page {page_time:?}; page VM steps {steps}; response {} bytes", encode(&first).unwrap().len());
        }
    }
}
