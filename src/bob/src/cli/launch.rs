use anyhow::{ensure, Context, Result};
use bob::application::SessionSelection;
use bob_kernel::SessionCatalog;
use std::path::{Path, PathBuf};

pub fn pin_database(path: PathBuf) -> Result<PathBuf> {
    ensure!(
        path != Path::new(":memory:"),
        "Bob requires a database file"
    );
    bob::storage::canonical_database(&path).with_context(|| {
        format!(
            "resolve database path {} from launch directory",
            path.display()
        )
    })
}

pub fn validate_workspace(path: &Path) -> Result<PathBuf> {
    let workspace = path.canonicalize().with_context(|| {
        format!(
            "workspace {} is unavailable; restore the directory at this path or choose another workspace/session",
            path.display()
        )
    })?;
    ensure!(
        workspace.is_dir(),
        "workspace {} is not a directory; restore the directory at this path or choose another workspace/session",
        path.display()
    );
    Ok(workspace)
}

pub fn resume_workspace(
    database: &Path,
    current: &Path,
    selection: &SessionSelection,
) -> Result<PathBuf> {
    match selection {
        SessionSelection::Id(id) => {
            let catalog = SessionCatalog::open(database)
                .with_context(|| format!("open session catalog {}", database.display()))?;
            let session = catalog.session(id.clone()).with_context(|| {
                format!("open session {id}; choose an existing session to resume")
            })?;
            ensure!(
                bob_kernel::session_in_use(database, id) != Some(true),
                "session {id} is already open in another bob process; close it there or start a new session"
            );
            validate_workspace(Path::new(&session.workspace))
                .with_context(|| format!("cannot resume session {id}"))
        }
        SessionSelection::New | SessionSelection::Latest => validate_workspace(current),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bob_kernel::{
        Command, CommandId, Executor, Invocation, ModelInfo, ModelProgress, Policy, PreparedTool,
        Profile, RequestSpec, Runtime, SessionId, ToolDescriptor, ToolOutcome,
    };
    use std::sync::Arc;
    use tokio::sync::watch;

    struct Directory(PathBuf);

    impl Directory {
        fn new() -> Result<Self> {
            let path = std::env::temp_dir().join(format!("bob-launch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path)?;
            Ok(Self(path.canonicalize()?))
        }

        fn workspace(&self, name: &str) -> Result<PathBuf> {
            let path = self.0.join(name);
            std::fs::create_dir(&path)?;
            Ok(path)
        }
    }

    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct NoWork;

    #[async_trait::async_trait]
    impl Executor for NoWork {
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "fixture".into(),
                context_window: 32_768,
            }]
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
            anyhow::bail!("launch fixture cannot call a provider")
        }

        async fn prepare(&self, _: Invocation) -> Result<PreparedTool> {
            anyhow::bail!("launch fixture cannot prepare tools")
        }

        async fn execute(&self, _: PreparedTool, _: watch::Receiver<bool>) -> Result<ToolOutcome> {
            anyhow::bail!("launch fixture cannot execute tools")
        }
    }

    async fn create_session(database: &Path, workspace: &Path) -> Result<SessionId> {
        let handle = Runtime::open_paused(database, Arc::new(NoWork)).await?;
        let receipt = handle
            .command(Command::CreateSession {
                id: CommandId::new(),
                workspace: workspace.to_string_lossy().into_owned(),
                profile: Profile {
                    model: "fixture".into(),
                    instructions: String::new(),
                    read_only: false,
                    max_requests: 16,
                    reasoning: bob_types::ReasoningEffort::Off,
                },
                policy: Policy::default(),
            })
            .await;
        handle.shutdown().await?;
        Ok(receipt?.session)
    }

    #[test]
    fn relative_database_stays_bound_to_launch_directory() -> Result<()> {
        let launch = std::env::current_dir()?;
        let relative = PathBuf::from(format!("bob-launch-{}.db", uuid::Uuid::new_v4()));
        let pinned = pin_database(relative.clone())?;
        let fixture = Directory::new()?;
        let resumed = fixture.workspace("resumed")?;
        assert_eq!(pinned, launch.join(&relative));
        assert_eq!(resumed.join(&pinned), pinned);
        assert_ne!(resumed.join(&relative), pinned);
        assert_eq!(pin_database(pinned.clone())?, pinned);
        assert!(!pinned.exists());
        assert_eq!(std::env::current_dir()?, launch);
        assert!(pin_database(PathBuf::from(":memory:")).is_err());
        Ok(())
    }

    #[test]
    fn workspace_validation_canonicalizes_and_rejects_missing_or_file() -> Result<()> {
        let fixture = Directory::new()?;
        let workspace = fixture.workspace("project")?;
        assert_eq!(validate_workspace(&workspace.join("."))?, workspace);
        let missing = fixture.0.join("missing");
        let error = validate_workspace(&missing).unwrap_err().to_string();
        assert!(error.contains(&missing.display().to_string()));
        assert!(error.contains("restore the directory"));
        let file = fixture.0.join("not-a-directory");
        std::fs::write(&file, "fixture")?;
        let error = validate_workspace(&file).unwrap_err().to_string();
        assert!(error.contains("not a directory"));
        assert!(error.contains(&file.display().to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn explicit_id_resolves_other_workspace_without_writes_or_chdir() -> Result<()> {
        let cwd = std::env::current_dir()?;
        let fixture = Directory::new()?;
        let local = fixture.workspace("local")?;
        let other = fixture.workspace("other")?;
        let database = fixture.0.join("lab.db");
        let id = create_session(&database, &other.join(".")).await?;
        let before = std::fs::read(&database)?;
        assert_eq!(
            resume_workspace(&database, &local, &SessionSelection::Id(id))?,
            other
        );
        assert_eq!(std::fs::read(&database)?, before);
        assert_eq!(std::env::current_dir()?, cwd);
        Ok(())
    }

    #[tokio::test]
    async fn unknown_session_and_moved_workspace_fail_without_fallback() -> Result<()> {
        let fixture = Directory::new()?;
        let local = fixture.workspace("local")?;
        let other = fixture.workspace("other")?;
        let database = fixture.0.join("lab.db");
        let id = create_session(&database, &other).await?;
        let before = std::fs::read(&database)?;
        let unknown = SessionId::new();
        let error = resume_workspace(&database, &local, &SessionSelection::Id(unknown.clone()))
            .unwrap_err();
        assert!(error.to_string().contains(&unknown.to_string()));
        std::fs::rename(&other, fixture.0.join("moved"))?;
        let error = resume_workspace(&database, &local, &SessionSelection::Id(id)).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&other.display().to_string()));
        assert!(message.contains("restore the directory"));
        assert_eq!(std::fs::read(&database)?, before);
        Ok(())
    }

    #[tokio::test]
    async fn new_and_latest_preserve_validated_local_workspace() -> Result<()> {
        let fixture = Directory::new()?;
        let local = fixture.workspace("local")?;
        let other = fixture.workspace("other")?;
        let database = fixture.0.join("lab.db");
        create_session(&database, &other).await?;
        let before = std::fs::read(&database)?;
        for selection in [SessionSelection::New, SessionSelection::Latest] {
            assert_eq!(
                resume_workspace(&database, &local.join("."), &selection)?,
                local
            );
            assert_eq!(
                resume_workspace(&fixture.0.join("absent.db"), &local, &selection)?,
                local
            );
            assert!(resume_workspace(&database, &fixture.0.join("missing"), &selection).is_err());
        }
        assert_eq!(std::fs::read(&database)?, before);
        assert!(!fixture.0.join("absent.db").exists());
        Ok(())
    }
}
