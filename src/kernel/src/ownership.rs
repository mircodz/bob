use crate::SessionId;
use anyhow::{ensure, Context};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn canonical_database_path(path: &Path) -> anyhow::Result<PathBuf> {
    ensure!(
        path != Path::new(":memory:"),
        "in-memory databases have no filesystem identity"
    );
    if path.exists() {
        return Ok(std::fs::canonicalize(path)?);
    }
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        missing.push(
            existing
                .file_name()
                .context("database path needs a file name")?
                .to_os_string(),
        );
        existing = existing
            .parent()
            .context("database path has no existing ancestor")?;
    }
    let mut result = std::fs::canonicalize(existing)?;
    for component in missing.into_iter().rev() {
        result.push(component);
    }
    Ok(result)
}

pub fn wait_for_database_initialization(path: &Path) -> anyhow::Result<()> {
    let path = canonical_database_path(path)?;
    let guard = match File::open(sidecar(&path, "schema.lock")) {
        Ok(guard) => guard,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).context("inspect database initialization lock"),
    };
    let start = Instant::now();
    loop {
        match guard.try_lock_shared() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(error).context(
                    "database initialization did not complete; existing files were preserved",
                )
            }
        }
    }
}

pub(crate) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    path.with_extension(format!(
        "{}.{suffix}",
        path.extension().and_then(|x| x.to_str()).unwrap_or("db")
    ))
}

pub(crate) fn lock_file(path: &Path) -> anyhow::Result<File> {
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?)
}

pub(crate) fn compatibility_guard(path: &Path) -> anyhow::Result<File> {
    let guard = lock_file(&sidecar(path, "lock"))?;
    guard
        .try_lock_shared()
        .context("database is owned by an incompatible execution owner")?;
    Ok(guard)
}

pub(crate) fn schema_guard(path: &Path) -> anyhow::Result<File> {
    let guard = lock_file(&sidecar(path, "schema.lock"))?;
    let start = Instant::now();
    loop {
        match guard.try_lock() {
            Ok(()) => return Ok(guard),
            Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error).context("database initialization is busy"),
        }
    }
}

pub struct SessionClaim {
    path: PathBuf,
    session: SessionId,
    pub(crate) generation: String,
    _session_lock: File,
    _compatibility: File,
    _database_file: Option<File>,
}

impl SessionClaim {
    pub fn acquire(path: &Path, session: SessionId) -> anyhow::Result<Self> {
        let path = canonical_database_path(path)?;
        ensure!(
            path.is_file(),
            "session claims require an existing database"
        );
        let initialized = crate::store::Store::open(&path)?;
        let claim = Self::acquire_initialized(&path, session)?;
        drop(initialized);
        Ok(claim)
    }

    pub(crate) fn acquire_initialized(path: &Path, session: SessionId) -> anyhow::Result<Self> {
        let path = canonical_database_path(path)?;
        let database_file = crate::store::database_file(&path)?;
        let compatibility = compatibility_guard(&path)?;
        let session_lock = lock_file(&session_lock_path(&path, &session))?;
        session_lock.try_lock().with_context(|| {
            format!(
                "session {} is already open in another bob process; close it there or start a new session",
                session.0
            )
        })?;
        Ok(Self {
            path,
            session,
            generation: uuid::Uuid::new_v4().to_string(),
            _session_lock: session_lock,
            _compatibility: compatibility,
            _database_file: database_file,
        })
    }

    pub fn session(&self) -> &SessionId {
        &self.session
    }

    pub fn database_path(&self) -> &Path {
        &self.path
    }
}

fn session_lock_path(database: &Path, session: &SessionId) -> PathBuf {
    let identity = format!("{:x}", Sha256::digest(session.0.as_bytes()));
    sidecar(database, &format!("session-{identity}.lock"))
}

/// Whether a live process holds `session` open; `None` when it was never opened from `database`.
pub fn session_in_use(database: &Path, session: &SessionId) -> Option<bool> {
    let database = canonical_database_path(database).ok()?;
    let lock = File::open(session_lock_path(&database, session)).ok()?;
    Some(lock.try_lock().is_err())
}

pub(crate) const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS session_owners(session TEXT PRIMARY KEY REFERENCES sessions(id), generation TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS session_changes(session TEXT PRIMARY KEY REFERENCES sessions(id), sequence INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS queued_submission_threads ON submissions(thread) WHERE state='queued';
CREATE INDEX IF NOT EXISTS running_task_groups ON task_groups(operation) WHERE state='running';
CREATE INDEX IF NOT EXISTS unsettled_workflows ON workflows(operation) WHERE result IS NULL;
CREATE TRIGGER IF NOT EXISTS session_change_watermark AFTER INSERT ON changes BEGIN
    INSERT INTO session_changes(session,sequence)
    SELECT id,NEW.seq FROM sessions WHERE id=COALESCE((SELECT session FROM threads WHERE id=NEW.thread),(SELECT session FROM jobs WHERE id=NEW.reference),CASE WHEN NEW.kind IN ('session_created','session_metadata','policy_updated','permissions_updated') THEN NEW.reference END)
    ON CONFLICT(session) DO UPDATE SET sequence=excluded.sequence;
END;
INSERT OR IGNORE INTO session_changes(session,sequence)
SELECT s.id,COALESCE((SELECT MAX(c.seq) FROM changes c WHERE c.thread IN (SELECT id FROM threads WHERE session=s.id) OR (c.thread IS NULL AND c.reference=s.id)),0) FROM sessions s;
";

pub(crate) fn sequence(db: &rusqlite::Connection, session: &SessionId) -> anyhow::Result<i64> {
    Ok(db.query_row(
        "SELECT COALESCE((SELECT sequence FROM session_changes WHERE session=?1),0)",
        [session],
        |r| r.get(0),
    )?)
}

pub(crate) fn busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| matches!(cause.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(code, _)) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)))
}
