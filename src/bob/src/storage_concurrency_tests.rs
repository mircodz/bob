use super::*;
use async_trait::async_trait;
use bob_kernel::{
    Executor, Invocation, ModelInfo, PreparedTool, RequestSpec, Runtime, ToolDescriptor,
    ToolOutcome,
};
use std::sync::Arc;
use tokio::sync::watch;

struct Directory(PathBuf);

impl Directory {
    fn new() -> Result<Self> {
        let path =
            std::env::temp_dir().join(format!("bob-shared-storage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path)?;
        Ok(Self(path.canonicalize()?))
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct NoWork;

#[async_trait]
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
        _: bob_kernel::ModelProgress,
    ) -> Result<bob_types::Completion> {
        anyhow::bail!("storage fixture must not call a provider")
    }
    async fn prepare(&self, _: Invocation) -> Result<PreparedTool> {
        anyhow::bail!("storage fixture must not prepare effects")
    }
    async fn execute(&self, _: PreparedTool, _: watch::Receiver<bool>) -> Result<ToolOutcome> {
        anyhow::bail!("storage fixture must not execute effects")
    }
}

async fn create_database(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    Runtime::open_paused(path, Arc::new(NoWork))
        .await?
        .shutdown()
        .await
}

#[test]
fn canonical_missing_parent_does_not_create_storage() -> Result<()> {
    let directory = Directory::new()?;
    let database = directory.0.join("missing/nested/sessions.sqlite");
    assert_eq!(canonical_database(&database)?, database);
    assert_eq!(std::fs::read_dir(&directory.0)?.count(), 0);
    Ok(())
}

#[tokio::test]
async fn simultaneous_store_and_memory_initialization_share_one_sidecar() -> Result<()> {
    let directory = Directory::new()?;
    let database = resolve_database(None, || Some(directory.0.clone()))?;
    std::fs::create_dir_all(database.parent().unwrap())?;
    let (left, right) = tokio::join!(
        Runtime::open_paused(&database, Arc::new(NoWork)),
        Runtime::open_paused(&database, Arc::new(NoWork)),
    );
    let left = left?;
    let right = right?;
    let one = database.clone();
    let two = database.clone();
    let (first, second) =
        tokio::join!(
            tokio::task::spawn_blocking(move || open_memories(&one)?
                .remember(&MemoryScope::Global, "shared fixture memory")),
            tokio::task::spawn_blocking(move || open_memories(&two)?
                .remember(&MemoryScope::Global, "shared fixture memory")),
        );
    assert_eq!(first??.id, second??.id);
    assert_eq!(
        open_memories(&database)?.list(&MemoryScope::Global)?.len(),
        1
    );
    left.shutdown().await?;
    assert!(SessionCatalog::open(&database).is_ok());
    right.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn database_file_and_parent_aliases_use_canonical_memories() -> Result<()> {
    use std::os::unix::fs::symlink;
    let directory = Directory::new()?;
    let data = directory.0.join("data");
    let database = data.join("sessions.sqlite");
    create_database(&database).await?;
    let file_alias = directory.0.join("alias.sqlite");
    symlink(&database, &file_alias)?;
    let parent_alias = directory.0.join("alias-dir");
    symlink(&data, &parent_alias)?;
    let real = open_memories(&database)?;
    let entry = real.remember(&MemoryScope::Global, "canonical memory")?;
    for alias in [&file_alias, &parent_alias.join("sessions.sqlite")] {
        assert_eq!(canonical_database(alias)?, database);
        assert_eq!(
            open_memories(alias)?.list(&MemoryScope::Global)?,
            vec![entry.clone()]
        );
    }
    assert!(!memory_database(&file_alias).exists());
    assert_eq!(std::fs::read_link(&file_alias)?, database);
    drop(real);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn alias_specific_companions_are_preserved_not_silently_stranded() -> Result<()> {
    use std::os::unix::fs::symlink;
    for suffix in [".memories.sqlite", "-journal", ".lock", ".schema.lock"] {
        let directory = Directory::new()?;
        let database = directory.0.join("sessions.sqlite");
        create_database(&database).await?;
        let alias = directory.0.join("alias.sqlite");
        symlink(&database, &alias)?;
        let companion = append_suffix(&alias, suffix);
        std::fs::write(&companion, b"preserve alias data")?;
        let original = std::fs::read(&database)?;
        let error = canonical_database(&alias).unwrap_err();
        assert!(format!("{error:#}").contains("companion"), "{error:#}");
        assert_eq!(std::fs::read(&companion)?, b"preserve alias data");
        assert_eq!(std::fs::read(&database)?, original);
        assert!(!memory_database(&database).exists());
        assert_eq!(std::fs::read_link(&alias)?, database);
        assert_eq!(canonical_database(&database)?, database);
    }
    Ok(())
}

#[test]
fn orphan_schema_guard_does_not_authorize_an_empty_history() -> Result<()> {
    let directory = Directory::new()?;
    let database = directory.0.join("sessions.sqlite");
    let guard = append_suffix(&database, ".schema.lock");
    std::fs::write(&guard, b"retained guard")?;
    assert!(canonical_database(&database).is_err());
    assert_eq!(std::fs::read(&guard)?, b"retained guard");
    assert!(!database.exists());
    assert_eq!(std::fs::read_dir(&directory.0)?.count(), 1);
    Ok(())
}
