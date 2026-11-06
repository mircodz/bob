use super::*;
use async_trait::async_trait;
use bob_kernel::{
    Executor, Invocation, ModelInfo, PreparedTool, RequestSpec, ToolDescriptor, ToolOutcome,
};
use bob_types::Completion;
use tokio::sync::watch;

struct NoWork;

#[async_trait]
impl Executor for NoWork {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "fixture".into(),
            context_window: 1_000_000,
        }]
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        _: RequestSpec,
        _: watch::Receiver<bool>,
        _: bob_kernel::ModelProgress,
    ) -> Result<Completion> {
        anyhow::bail!("creating a session must not call a provider")
    }
    async fn prepare(&self, _: Invocation) -> Result<PreparedTool> {
        unreachable!()
    }
    async fn execute(&self, _: PreparedTool, _: watch::Receiver<bool>) -> Result<ToolOutcome> {
        unreachable!()
    }
}

struct Directory(PathBuf);
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn new_session_keeps_execution_profile_when_disk_configuration_changes() {
    let dir =
        Directory(std::env::temp_dir().join(format!("bob-session-shell-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir(&dir.0).unwrap();
    let handle = Runtime::open_paused(&dir.0.join("lab.db"), Arc::new(NoWork))
        .await
        .unwrap();
    let profile = Profile {
        model: "fixture".into(),
        instructions: "Current runtime uses Bash. Keep its explicit worker selection.".into(),
        read_only: false,
        max_requests: 20,
        reasoning: ReasoningEffort::Off,
    };
    let original = handle
        .command(Command::CreateSessionConfigured {
            id: CommandId::new(),
            workspace: dir.0.to_str().unwrap().into(),
            profile: profile.clone(),
            policy: Policy::default(),
            permissions: PermissionPolicy {
                grants: vec![bob_kernel::PermissionGrant {
                    tool: "read_file".into(),
                    path_prefix: Some(dir.0.to_string_lossy().into_owned()),
                    command: None,
                    shell_command: None,
                }],
                allow_bash: vec!["cargo test".into()],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let source = handle.session(original.session).await.unwrap();
    std::fs::write(dir.0.join(".bob.config.toml"), "system = 'Do not advertise this new configuration in the existing executor.'\n[shell]\ndialect = 'pwsh'\nexecutable = 'C:\\Program Files\\PowerShell\\7\\pwsh.exe'\n").unwrap();
    let next = new_session(&handle, &source).await.unwrap();
    let session = handle.session(next.session).await.unwrap();
    assert_ne!(session.id, source.id);
    assert_eq!(session.workspace, source.workspace);
    assert_eq!(session.profile.instructions, profile.instructions);
    assert_eq!(session.profile.model, profile.model);
    assert_eq!(session.profile.reasoning, profile.reasoning);
    assert_eq!(session.profile.max_requests, profile.max_requests);
    assert!(session.permissions.grants.is_empty());
    assert_eq!(
        session.permissions.allow_bash,
        source.permissions.allow_bash
    );
    assert_eq!(
        handle
            .session(source.id.clone())
            .await
            .unwrap()
            .permissions
            .grants,
        source.permissions.grants
    );
    assert!(handle.queue(session.thread).await.unwrap().is_empty());
    handle.shutdown().await.unwrap();
}

#[cfg(unix)]
#[test]
fn invalid_initial_reasoning_can_be_corrected_without_orphaning_storage() -> Result<()> {
    const MARKER: &str = "BOB_STARTUP_CONFIG_FIXTURE";
    if let Some(directory) = std::env::var_os(MARKER) {
        let root = PathBuf::from(directory);
        let workspace = root.join("workspace");
        let database = storage::resolve_database(None, || Some(root.join("home")))?;
        let options = LaunchOptions {
            database: database.clone(),
            workspace: workspace.clone(),
            provider: Some("fixture-no-provider:fixture".into()),
            model: None,
            worker_model: None,
            selection: SessionSelection::New,
            page_size: 80,
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let error = runtime
            .block_on(open(options.clone()))
            .err()
            .context("invalid reasoning must fail")?;
        assert!(
            format!("{error:#}").contains("invalid configured reasoning effort"),
            "{error:#}"
        );
        assert!(!database.exists());
        assert!(!storage::memory_database(&database).exists());
        std::fs::write(workspace.join(".bob.config.toml"), "reasoning = 'off'\n")?;
        assert_eq!(
            storage::resolve_database(None, || Some(root.join("home")))?,
            database
        );
        let error = runtime
            .block_on(open(options))
            .err()
            .context("offline provider must fail")?;
        assert!(
            format!("{error:#}").contains("fixture-no-provider"),
            "{error:#}"
        );
        assert!(!database.parent().unwrap().exists());
        return Ok(());
    }
    let directory = Directory(
        std::env::temp_dir().join(format!("bob-startup-config-{}", uuid::Uuid::new_v4())),
    );
    let home = directory.0.join("home");
    let workspace = directory.0.join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir(&workspace)?;
    std::fs::write(
        workspace.join(".bob.config.toml"),
        "reasoning = 'invalid-fixture-value'\n",
    )?;
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "application::session_tests::invalid_initial_reasoning_can_be_corrected_without_orphaning_storage", "--nocapture"])
        .env(MARKER, &directory.0)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .current_dir(&workspace)
        .output()?;
    ensure!(
        output.status.success(),
        "startup regression fixture failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn owned_resume_fails_before_provider_startup_and_failed_startup_releases_claim() -> Result<()> {
    const MARKER: &str = "BOB_SESSION_CLAIM_STARTUP_FIXTURE";
    if let Some(root) = std::env::var_os(MARKER) {
        let root = PathBuf::from(root);
        let workspace = root.join("workspace");
        let database = storage::resolve_database(None, || Some(root.join("home")))?;
        std::fs::create_dir_all(database.parent().unwrap())?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        return runtime.block_on(async {
            let handle = Runtime::open_paused(&database, Arc::new(NoWork)).await?;
            let created = handle
                .command(Command::CreateSessionConfigured {
                    id: CommandId::new(),
                    workspace: workspace.to_string_lossy().into_owned(),
                    profile: Profile {
                        model: "fixture".into(),
                        instructions: String::new(),
                        read_only: false,
                        max_requests: 20,
                        reasoning: ReasoningEffort::Off,
                    },
                    policy: Policy::default(),
                    permissions: PermissionPolicy::default(),
                })
                .await?;
            handle
                .activate_session(created.session.clone(), Vec::new())
                .await?;
            let before = serde_json::to_value(handle.session(created.session.clone()).await?)?;
            let options = LaunchOptions {
                database: database.clone(),
                workspace: workspace.clone(),
                provider: Some("fixture-no-provider:fixture".into()),
                model: None,
                worker_model: None,
                selection: SessionSelection::Id(created.session.clone()),
                page_size: 80,
            };
            let error = open(options.clone())
                .await
                .err()
                .context("owned resume must fail")?;
            assert!(
                format!("{error:#}").contains("already open in another bob process"),
                "{error:#}"
            );
            assert_eq!(
                serde_json::to_value(handle.session(created.session.clone()).await?)?,
                before
            );
            assert!(!storage::memory_database(&database).exists());
            handle.shutdown().await?;
            let error = open(options.clone())
                .await
                .err()
                .context("fixture provider must fail")?;
            assert!(
                format!("{error:#}").contains("fixture-no-provider"),
                "{error:#}"
            );
            assert!(!storage::memory_database(&database).exists());
            let prepared = tokio::task::spawn_blocking(move || prepare_launch(options)).await??;
            assert_eq!(prepared.claim.as_ref().unwrap().session(), &created.session);
            assert!(SessionClaim::acquire(&database, created.session.clone()).is_err());
            drop(prepared);
            drop(SessionClaim::acquire(&database, created.session)?);
            Ok(())
        });
    }
    let directory =
        Directory(std::env::temp_dir().join(format!("bob-startup-claim-{}", uuid::Uuid::new_v4())));
    let home = directory.0.join("home");
    let workspace = directory.0.join("workspace");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir(&workspace)?;
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "application::session_tests::owned_resume_fails_before_provider_startup_and_failed_startup_releases_claim", "--nocapture"])
        .env(MARKER, &directory.0)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .current_dir(&workspace)
        .output()?;
    ensure!(
        output.status.success(),
        "isolated session-claim fixture failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
