use anyhow::{ensure, Context, Result};
use bob_core::core::memory::{MemoryEntry, MemoryRepository, MemoryScope, SqliteMemoryRepository};
use bob_kernel::SessionCatalog;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub fn resolve_database(
    explicit: Option<&Path>,
    home: impl FnOnce() -> Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        validate_database(path)?;
        return Ok(path.to_owned());
    }
    let home = home().context("no home directory; use --database to select a Bob store")?;
    ensure!(home.is_absolute(), "home directory must be absolute");
    let database = home.join(".bob").join("sessions.sqlite");
    let previous = home.join(".bob-lab").join("sessions.sqlite");
    for path in std::iter::once(previous.clone()).chain(database_companions(&previous)) {
        ensure!(
            !entry_exists(&path).with_context(|| {
                format!(
                    "cannot check previous durable store at {}; use --database <path> to select a store explicitly",
                    path.display()
                )
            })?,
            "previous durable store data exists at {}; refusing to select or create {} automatically. Use --database <path> to explicitly select {} or {}. No data has been migrated or merged",
            path.display(), database.display(), previous.display(), database.display()
        );
    }
    validate_database(&database)?;
    Ok(database)
}

pub fn canonical_database(database: &Path) -> Result<PathBuf> {
    validate_database(database)?;
    let supplied = std::path::absolute(database)?;
    let canonical = bob_kernel::canonical_database_path(database)?;
    if supplied != canonical {
        let suffixes = [
            "-wal",
            "-shm",
            "-journal",
            ".lock",
            ".schema.lock",
            ".memories.sqlite",
            ".memories.sqlite-wal",
            ".memories.sqlite-shm",
            ".memories.sqlite-journal",
            ".memories.sqlite.lock",
            ".memories.sqlite.schema.lock",
        ];
        let mut companions: Vec<_> = suffixes
            .into_iter()
            .map(|suffix| {
                (
                    append_suffix(&supplied, suffix),
                    append_suffix(&canonical, suffix),
                )
            })
            .collect();
        for suffix in ["lock", "schema.lock"] {
            companions.push((
                store_sidecar(&supplied, suffix),
                store_sidecar(&canonical, suffix),
            ));
        }
        for (alias, target) in companions {
            if entry_exists(&alias)? {
                ensure!(
                    entry_exists(&target)?
                        && std::fs::canonicalize(&alias)? == std::fs::canonicalize(&target)?,
                    "store alias {} has separate companion data at {}; refusing to ignore or relocate it. Use --database {} to explicitly select the canonical store and its companions, or resolve the alias data first. No data has been moved or merged",
                    supplied.display(), alias.display(), canonical.display()
                );
            }
        }
        validate_database(&canonical)?;
    }
    Ok(canonical)
}

pub fn memory_database(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(".memories.sqlite");
    PathBuf::from(path)
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn store_sidecar(database: &Path, suffix: &str) -> PathBuf {
    database.with_extension(format!(
        "{}.{suffix}",
        database
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("db")
    ))
}

fn sqlite_companions(database: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = ["-wal", "-shm", "-journal", ".lock", ".schema.lock"]
        .into_iter()
        .map(|suffix| append_suffix(database, suffix))
        .collect();
    for suffix in ["lock", "schema.lock"] {
        let lock = store_sidecar(database, suffix);
        if !paths.contains(&lock) {
            paths.push(lock);
        }
    }
    paths
}

fn database_companions(database: &Path) -> Vec<PathBuf> {
    let memories = memory_database(database);
    let mut paths = sqlite_companions(database);
    paths.extend(sqlite_companions(&memories));
    paths.push(memories);
    paths
}

fn entry_exists(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            for parent in path
                .ancestors()
                .skip(1)
                .filter(|path| !path.as_os_str().is_empty())
            {
                match std::fs::symlink_metadata(parent) {
                    Ok(_) => {
                        let metadata = std::fs::metadata(parent).with_context(|| {
                            format!("inspect store directory {}", parent.display())
                        })?;
                        ensure!(
                            metadata.is_dir(),
                            "store parent {} is not a directory",
                            parent.display()
                        );
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("inspect store directory {}", parent.display())
                        });
                    }
                }
            }
            Ok(false)
        }
        Err(error) => Err(error).with_context(|| format!("inspect store path {}", path.display())),
    }
}

fn validate_file(path: &Path) -> Result<()> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("inspect existing store file {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "store path {} is not a file",
        path.display()
    );
    readable(path).with_context(|| format!("read existing store file {}", path.display()))?;
    Ok(())
}

/// Closing any descriptor for an open SQLite file drops this process's POSIX
/// locks on it, so check readability without opening the file.
#[cfg(unix)]
fn readable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::access(path.as_ptr(), libc::R_OK) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn readable(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path).map(drop)
}

pub(crate) fn validate_database(database: &Path) -> Result<()> {
    let Err(original) = validate_database_once(database) else {
        return Ok(());
    };
    let canonical = bob_kernel::canonical_database_path(database)?;
    if entry_exists(&store_sidecar(&canonical, "lock"))?
        || entry_exists(&store_sidecar(&canonical, "schema.lock"))?
    {
        bob_kernel::wait_for_database_initialization(&canonical)?;
        return validate_database_once(database);
    }
    Err(original)
}

fn validate_database_once(database: &Path) -> Result<()> {
    ensure!(
        database != Path::new(":memory:"),
        "Bob requires a database file"
    );
    let exists = entry_exists(database)?;
    let memories = memory_database(database);
    for path in database_companions(database) {
        if entry_exists(&path)? {
            ensure!(
                exists,
                "orphaned store companion {} exists without {}; refusing to create an empty history. Restore the complete store or use --database <path> to select another store",
                path.display(), database.display()
            );
            validate_file(&path)?;
        }
    }
    for path in sqlite_companions(&memories) {
        ensure!(
            !entry_exists(&path)? || entry_exists(&memories)?,
            "orphaned memory store companion {} exists without {}; restore the complete store or use --database <path> to select another store",
            path.display(), memories.display()
        );
    }
    if exists {
        validate_file(database)?;
        let canonical = bob_kernel::canonical_database_path(database)?;
        SessionCatalog::open(&canonical).with_context(|| {
            format!(
                "validate Bob database {}; preserved without replacement",
                database.display()
            )
        })?;
    }
    Ok(())
}

pub(crate) fn open_memories(database: &Path) -> Result<SqliteMemoryRepository> {
    let database = canonical_database(database)?;
    ensure!(
        database.is_file(),
        "open the session store before its memory repository"
    );
    SqliteMemoryRepository::open(memory_database(&database))
}

#[derive(Default)]
pub(crate) struct DeferredMemories {
    repository: OnceLock<SqliteMemoryRepository>,
}

impl DeferredMemories {
    pub(crate) fn initialize(&self, database: &Path) -> Result<()> {
        self.repository
            .set(open_memories(database)?)
            .map_err(|_| anyhow::anyhow!("memory repository already initialized"))
    }

    fn repository(&self) -> Result<&SqliteMemoryRepository> {
        self.repository
            .get()
            .context("memory repository is not initialized")
    }
}

impl MemoryRepository for DeferredMemories {
    fn remember(&self, scope: &MemoryScope, content: &str) -> Result<MemoryEntry> {
        self.repository()?.remember(scope, content)
    }

    fn list(&self, scope: &MemoryScope) -> Result<Vec<MemoryEntry>> {
        self.repository()?.list(scope)
    }

    fn list_page(
        &self,
        scope: &MemoryScope,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<MemoryEntry>> {
        self.repository()?.list_page(scope, offset, limit)
    }

    fn forget(&self, scope: &MemoryScope, id: i64) -> Result<bool> {
        self.repository()?.forget(scope, id)
    }
}

#[cfg(test)]
#[path = "storage_concurrency_tests.rs"]
mod concurrency_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{self, LaunchOptions, SessionSelection};
    use bob_kernel::{
        Executor, Invocation, ModelInfo, ModelProgress, PreparedTool, RequestSpec, Runtime,
        SessionId, ToolDescriptor, ToolOutcome,
    };
    use std::sync::Arc;
    use tokio::sync::watch;

    struct NoWork;

    #[async_trait::async_trait]
    impl Executor for NoWork {
        fn models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }

        fn tools(&self) -> Vec<ToolDescriptor> {
            Vec::new()
        }

        async fn generate(
            &self,
            _: RequestSpec,
            _: watch::Receiver<bool>,
            _: ModelProgress,
        ) -> Result<bob_types::Completion> {
            anyhow::bail!("storage fixture cannot call a provider")
        }

        async fn prepare(&self, _: Invocation) -> Result<PreparedTool> {
            anyhow::bail!("storage fixture cannot prepare tools")
        }

        async fn execute(&self, _: PreparedTool, _: watch::Receiver<bool>) -> Result<ToolOutcome> {
            anyhow::bail!("storage fixture cannot execute tools")
        }
    }

    async fn create_database(database: &Path) -> Result<()> {
        std::fs::create_dir_all(database.parent().unwrap())?;
        Runtime::open_paused(database, Arc::new(NoWork))
            .await?
            .shutdown()
            .await
    }

    struct Home(PathBuf);

    impl Home {
        fn new() -> Result<Self> {
            let path = std::env::temp_dir().join(format!("bob-storage-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path)?;
            Ok(Self(path.canonicalize()?))
        }

        fn database(&self) -> PathBuf {
            resolve_database(None, || Some(self.0.clone())).unwrap()
        }

        fn options(&self, selection: SessionSelection) -> LaunchOptions {
            LaunchOptions {
                database: self.database(),
                workspace: self.0.clone(),
                provider: None,
                model: None,
                worker_model: None,
                selection,
                page_size: 80,
            }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn default_is_stable_absolute_and_independent_of_workspace() -> Result<()> {
        let home = Home::new()?;
        let expected = home.0.join(".bob/sessions.sqlite");
        for workspace in [home.0.join("one"), home.0.join("two/nested")] {
            let mut options = home.options(SessionSelection::New);
            options.workspace = workspace;
            assert_eq!(options.database, expected);
            assert!(options.database.is_absolute());
        }
        assert_eq!(std::fs::read_dir(&home.0)?.count(), 0);
        assert!(resolve_database(None, || None).is_err());
        assert!(resolve_database(None, || Some("relative".into())).is_err());
        Ok(())
    }

    #[test]
    fn explicit_database_never_looks_up_home() -> Result<()> {
        let home = Home::new()?;
        let path = home.0.join("isolated/custom.db");
        assert_eq!(
            resolve_database(Some(&path), || panic!("home lookup"))?,
            path
        );
        assert_eq!(std::fs::read_dir(&home.0)?.count(), 0);
        assert!(resolve_database(Some(Path::new(":memory:")), || panic!("home lookup")).is_err());
        Ok(())
    }

    #[test]
    fn memory_names_keep_full_filename_and_never_alias_database() {
        let paths = ["bob.db", "bob.sqlite", "bob", "bob.memories.sqlite"];
        let memories: Vec<_> = paths
            .iter()
            .map(|path| memory_database(Path::new(path)))
            .collect();
        assert_eq!(memories[0], Path::new("bob.db.memories.sqlite"));
        assert_eq!(memories[1], Path::new("bob.sqlite.memories.sqlite"));
        for (index, path) in paths.iter().enumerate() {
            assert_ne!(memories[index], Path::new(path));
            for other in &memories[index + 1..] {
                assert_ne!(&memories[index], other);
            }
        }
    }

    #[test]
    fn missing_inspection_has_no_filesystem_effects() -> Result<()> {
        let home = Home::new()?;
        assert!(SessionCatalog::open(&home.database()).is_err());
        assert_eq!(std::fs::read_dir(&home.0)?.count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn missing_explicit_session_has_no_filesystem_effects() -> Result<()> {
        let home = Home::new()?;
        assert!(
            application::open(home.options(SessionSelection::Id(SessionId::new())))
                .await
                .is_err()
        );
        assert_eq!(std::fs::read_dir(&home.0)?.count(), 0);
        Ok(())
    }

    #[tokio::test]
    async fn execution_creates_session_store_before_memory_sidecar() -> Result<()> {
        let home = Home::new()?;
        let database = home.database();
        let memories = DeferredMemories::default();
        assert!(memories.initialize(&database).is_err());
        assert!(!database.parent().unwrap().exists());
        assert_eq!(resolve_database(None, || Some(home.0.clone()))?, database);
        std::fs::create_dir_all(database.parent().unwrap())?;
        let runtime = Runtime::open_paused(&database, Arc::new(NoWork)).await?;
        memories.initialize(&database)?;
        assert!(database.is_file());
        assert!(memory_database(&database).is_file());
        let saved = memories.remember(&MemoryScope::Global, "startup succeeded")?;
        assert_eq!(memories.list_page(&MemoryScope::Global, 0, 1)?, vec![saved]);
        runtime.shutdown().await?;
        drop(memories);
        assert_eq!(resolve_database(None, || Some(home.0.clone()))?, database);
        assert_eq!(
            open_memories(&database)?.list(&MemoryScope::Global)?.len(),
            1
        );
        Ok(())
    }

    #[test]
    fn failed_initialization_does_not_create_memory_or_orphan_history() -> Result<()> {
        let home = Home::new()?;
        let database = home.database();
        let memories = DeferredMemories::default();
        assert!(memories.list(&MemoryScope::Global).is_err());
        assert!(memories
            .remember(&MemoryScope::Global, "not ready")
            .is_err());
        assert!(memories.forget(&MemoryScope::Global, 1).is_err());
        drop(memories);
        assert_eq!(std::fs::read_dir(&home.0)?.count(), 0);
        assert_eq!(resolve_database(None, || Some(home.0.clone()))?, database);
        Ok(())
    }

    #[tokio::test]
    async fn execution_preserves_legacy_store_config_and_old_memory_sidecar() -> Result<()> {
        let home = Home::new()?;
        let legacy = home.0.join(".bob");
        std::fs::create_dir(&legacy)?;
        let sentinels = [
            (legacy.join("bob.db"), b"legacy sessions".as_slice()),
            (legacy.join("config.toml"), b"# legacy config".as_slice()),
            (legacy.join("auth.json"), b"{\"sentinel\":true}".as_slice()),
            (
                home.0.join(".bob.config.toml"),
                b"# project config".as_slice(),
            ),
            (
                legacy.join("memories.sqlite"),
                b"legacy memories".as_slice(),
            ),
        ];
        for (path, bytes) in &sentinels {
            std::fs::write(path, bytes)?;
        }
        let database = home.database();
        let old_sidecar = database.with_extension("memories.sqlite");
        std::fs::write(&old_sidecar, b"old sidecar")?;
        create_database(&database).await?;
        drop(open_memories(&database)?);
        assert!(memory_database(&database).is_file());
        assert!(database.is_file());
        assert_eq!(std::fs::read(old_sidecar)?, b"old sidecar");
        for (path, bytes) in sentinels {
            assert_eq!(std::fs::read(path)?, bytes);
        }
        let companions = sqlite_companions(&database);
        let entries = std::fs::read_dir(legacy)?.collect::<std::io::Result<Vec<_>>>()?;
        assert_eq!(
            entries
                .iter()
                .filter(|entry| !companions.contains(&entry.path()))
                .count(),
            7
        );
        assert_eq!(std::fs::read_dir(&home.0)?.count(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn canonical_existing_store_is_validated_and_preserved() -> Result<()> {
        let home = Home::new()?;
        let database = home.database();
        create_database(&database).await?;
        drop(open_memories(&database)?);
        let before = std::fs::read(&database)?;
        let memories = std::fs::read(memory_database(&database))?;
        assert_eq!(resolve_database(None, || Some(home.0.clone()))?, database);
        assert_eq!(std::fs::read(&database)?, before);
        assert_eq!(std::fs::read(memory_database(&database))?, memories);
        assert!(!home.0.join(".bob-lab").exists());
        Ok(())
    }

    #[tokio::test]
    async fn previous_store_requires_explicit_selection_even_with_canonical_store() -> Result<()> {
        for both in [false, true] {
            let home = Home::new()?;
            let database = home.database();
            let previous = home.0.join(".bob-lab/sessions.sqlite");
            create_database(&previous).await?;
            let previous_bytes = std::fs::read(&previous)?;
            if both {
                create_database(&database).await?;
            }
            let canonical_bytes = std::fs::read(&database).ok();
            let error = resolve_database(None, || Some(home.0.clone()))
                .unwrap_err()
                .to_string();
            assert!(error.contains("--database"));
            assert!(error.contains(&previous.display().to_string()));
            assert!(error.contains(&database.display().to_string()));
            for selected in [&previous, &database] {
                assert_eq!(
                    resolve_database(Some(selected), || panic!("home lookup"))?,
                    *selected
                );
            }
            assert_eq!(std::fs::read(&previous)?, previous_bytes);
            assert_eq!(std::fs::read(&database).ok(), canonical_bytes);
            assert_eq!(database.exists(), both);
        }
        Ok(())
    }

    #[test]
    fn previous_store_companions_alone_block_default_creation() -> Result<()> {
        for suffix in [
            "-wal",
            "-shm",
            "-journal",
            ".lock",
            ".memories.sqlite",
            ".memories.sqlite-wal",
            ".memories.sqlite-shm",
            ".memories.sqlite-journal",
            ".memories.sqlite.lock",
        ] {
            let home = Home::new()?;
            let previous = home.0.join(".bob-lab/sessions.sqlite");
            std::fs::create_dir(previous.parent().unwrap())?;
            let companion = append_suffix(&previous, suffix);
            std::fs::write(&companion, b"preserve")?;
            let error = resolve_database(None, || Some(home.0.clone()))
                .unwrap_err()
                .to_string();
            assert!(error.contains("--database"));
            assert!(error.contains(&companion.display().to_string()));
            assert_eq!(std::fs::read(&companion)?, b"preserve");
            assert!(!home.0.join(".bob").exists());
            assert!(!previous.exists());
        }
        Ok(())
    }

    #[test]
    fn canonical_orphan_companions_block_creation_even_when_explicit() -> Result<()> {
        for suffix in [
            "-wal",
            "-shm",
            "-journal",
            ".lock",
            ".memories.sqlite",
            ".memories.sqlite-wal",
            ".memories.sqlite-shm",
            ".memories.sqlite-journal",
            ".memories.sqlite.lock",
        ] {
            let home = Home::new()?;
            let database = home.database();
            std::fs::create_dir(database.parent().unwrap())?;
            let companion = append_suffix(&database, suffix);
            std::fs::write(&companion, b"preserve")?;
            assert!(resolve_database(None, || Some(home.0.clone())).is_err());
            assert!(resolve_database(Some(&database), || panic!("home lookup")).is_err());
            assert!(open_memories(&database).is_err());
            assert_eq!(std::fs::read(&companion)?, b"preserve");
            assert!(!database.exists());
            assert_eq!(std::fs::read_dir(database.parent().unwrap())?.count(), 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn orphan_memory_journal_is_rejected_with_existing_sessions() -> Result<()> {
        let home = Home::new()?;
        let database = home.database();
        create_database(&database).await?;
        let before = std::fs::read(&database)?;
        let journal = append_suffix(&memory_database(&database), "-journal");
        std::fs::write(&journal, b"preserve")?;
        assert!(resolve_database(None, || Some(home.0.clone())).is_err());
        assert!(open_memories(&database).is_err());
        assert_eq!(std::fs::read(&database)?, before);
        assert_eq!(std::fs::read(journal)?, b"preserve");
        assert!(!memory_database(&database).exists());
        Ok(())
    }

    #[test]
    fn invalid_canonical_store_is_never_replaced() -> Result<()> {
        for bytes in [b"".as_slice(), b"not sqlite".as_slice()] {
            let home = Home::new()?;
            let database = home.database();
            std::fs::create_dir(database.parent().unwrap())?;
            std::fs::write(&database, bytes)?;
            assert!(resolve_database(None, || Some(home.0.clone())).is_err());
            assert!(open_memories(&database).is_err());
            assert_eq!(std::fs::read(&database)?, bytes);
            assert!(!memory_database(&database).exists());
        }
        let home = Home::new()?;
        let database = home.database();
        std::fs::create_dir_all(&database)?;
        assert!(resolve_database(None, || Some(home.0.clone())).is_err());
        assert!(database.is_dir());
        assert_eq!(std::fs::read_dir(&database)?.count(), 0);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn dangling_store_links_and_directories_never_fall_through() -> Result<()> {
        use std::os::unix::fs::symlink;
        for directory in [".bob", ".bob-lab"] {
            for suffix in ["", "-wal", ".memories.sqlite", ".memories.sqlite-shm"] {
                let home = Home::new()?;
                let database = home.0.join(directory).join("sessions.sqlite");
                std::fs::create_dir(database.parent().unwrap())?;
                let link = append_suffix(&database, suffix);
                let missing = home.0.join("missing");
                symlink(&missing, &link)?;
                assert!(resolve_database(None, || Some(home.0.clone())).is_err());
                assert_eq!(std::fs::read_link(&link)?, missing);
                assert!(!missing.exists());
                assert_eq!(std::fs::read_dir(database.parent().unwrap())?.count(), 1);
            }
            let home = Home::new()?;
            let missing = home.0.join("missing");
            symlink(&missing, home.0.join(directory))?;
            assert!(resolve_database(None, || Some(home.0.clone())).is_err());
            assert!(!missing.exists());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_canonical_store_is_not_recreated() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let home = Home::new()?;
        let database = home.database();
        create_database(&database).await?;
        let before = std::fs::read(&database)?;
        let permissions = std::fs::metadata(&database)?.permissions();
        std::fs::set_permissions(&database, std::fs::Permissions::from_mode(0o000))?;
        let unreadable = std::fs::File::open(&database).is_err();
        let result = resolve_database(None, || Some(home.0.clone()));
        std::fs::set_permissions(&database, permissions)?;
        if unreadable {
            assert!(result.is_err());
        }
        assert_eq!(std::fs::read(&database)?, before);
        assert!(!memory_database(&database).exists());
        Ok(())
    }

    #[tokio::test]
    async fn foreign_database_is_rejected_before_memory_or_runtime_writes() -> Result<()> {
        let home = Home::new()?;
        for name in ["foreign.db", "foreign.memories.sqlite"] {
            let database = home.0.join(name);
            let connection = rusqlite::Connection::open(&database)?;
            connection.execute_batch(
                "CREATE TABLE sentinel(value TEXT); INSERT INTO sentinel VALUES('keep');",
            )?;
            drop(connection);
            let before = std::fs::read(&database)?;
            let sidecar = memory_database(&database);
            std::fs::write(&sidecar, b"keep sidecar bytes")?;
            assert!(open_memories(&database).is_err());
            let mut options = home.options(SessionSelection::New);
            options.database = database.clone();
            assert!(application::open(options).await.is_err());
            assert_eq!(std::fs::read(&database)?, before);
            assert_eq!(std::fs::read(sidecar)?, b"keep sidecar bytes");
        }
        assert_eq!(std::fs::read_dir(&home.0)?.count(), 4);
        Ok(())
    }
}
