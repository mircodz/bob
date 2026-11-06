use crate::{adapters, provider::ProviderService, services::Services, storage};
use anyhow::{ensure, Context, Result};
use bob_core::core::{
    config::{load_config, BobConfig},
    shell::CommandShell,
};
use bob_core::providers::provider::StreamTimeouts;
use bob_kernel::{
    Command, CommandId, Handle, PermissionPolicy, PermissionRule, Policy, Profile, Receipt,
    RuleDecision, Runtime, RuntimeOptions, SessionCatalog, SessionClaim, SessionId, SessionQuery,
    SessionSnapshot,
};
use bob_types::ReasoningEffort;
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone, Debug)]
pub enum SessionSelection {
    New,
    Latest,
    Id(SessionId),
}

#[derive(Clone, Debug)]
pub struct LaunchOptions {
    pub database: PathBuf,
    pub workspace: PathBuf,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub worker_model: Option<String>,
    pub selection: SessionSelection,
    pub page_size: usize,
}

struct PreparedLaunch {
    options: LaunchOptions,
    workspace: PathBuf,
    selected: Option<SessionSnapshot>,
    claim: Option<SessionClaim>,
    config: BobConfig,
    permission_policy: PermissionPolicy,
    reasoning: ReasoningEffort,
}

pub struct Application {
    pub handle: Handle,
    pub services: Arc<Services>,
    pub provider: ProviderService,
    pub session: SessionSnapshot,
    pub config: BobConfig,
}

fn selected_session(options: &LaunchOptions) -> Result<Option<SessionSnapshot>> {
    match &options.selection {
        SessionSelection::New => Ok(None),
        SessionSelection::Id(id) => SessionCatalog::open(&options.database)?
            .session(id.clone())
            .map(Some)
            .with_context(|| format!("open session {id}")),
        SessionSelection::Latest if !options.database.exists() => Ok(None),
        SessionSelection::Latest => {
            let catalog = SessionCatalog::open(&options.database)?;
            let latest = catalog.sessions(SessionQuery {
                workspace: Some(options.workspace.to_string_lossy().into_owned()),
                limit: 1,
                ..Default::default()
            })?;
            latest
                .sessions
                .first()
                .map(|session| catalog.session(session.id.clone()))
                .transpose()
        }
    }
}

pub fn provider_spec(options: &LaunchOptions, config: &BobConfig) -> String {
    launch_model_spec(options, config, None)
}

fn launch_model_spec(
    options: &LaunchOptions,
    config: &BobConfig,
    restored: Option<&Profile>,
) -> String {
    if let Some(model) = &options.model {
        if [
            "copilot:",
            "openai:",
            "anthropic:",
            "copilot/",
            "openai/",
            "anthropic/",
        ]
        .iter()
        .any(|prefix| model.starts_with(prefix))
        {
            return model.clone();
        }
    }
    let split = |value: &str| {
        value.split_once([':', '/']).map_or_else(
            || (value.to_owned(), None),
            |(provider, model)| (provider.to_owned(), Some(model.to_owned())),
        )
    };
    let configured = split(&config.provider);
    let restored = restored.map(|profile| split(&profile.model));
    let explicit = options.provider.as_deref().map(split);
    let provider = explicit
        .as_ref()
        .or(restored.as_ref())
        .unwrap_or(&configured)
        .0
        .as_str();
    let model = options
        .model
        .as_deref()
        .or_else(|| explicit.as_ref().and_then(|(_, model)| model.as_deref()))
        .or_else(|| {
            restored
                .as_ref()
                .and_then(|(kind, model)| (kind == provider).then_some(model.as_deref()).flatten())
        })
        .or_else(|| {
            (configured.0 == provider)
                .then_some({
                    if config.model.is_empty() {
                        configured.1.as_deref()
                    } else {
                        Some(config.model.as_str())
                    }
                })
                .flatten()
        })
        .filter(|model| !model.is_empty());
    model.map_or_else(
        || provider.to_owned(),
        |model| format!("{provider}:{model}"),
    )
}

fn launch_reasoning(config: &BobConfig, restored: Option<&Profile>) -> Result<ReasoningEffort> {
    if let Some(profile) = restored {
        return Ok(profile.reasoning);
    }
    if config.reasoning.is_empty() {
        Ok(ReasoningEffort::Off)
    } else {
        ReasoningEffort::parse(&config.reasoning).context("invalid configured reasoning effort")
    }
}

fn permissions(config: &BobConfig) -> Result<PermissionPolicy> {
    let default = match config.permissions.default.as_str() {
        "allow" => RuleDecision::Allow,
        "ask" => RuleDecision::Ask,
        "deny" => RuleDecision::Deny,
        other => anyhow::bail!("invalid default permission decision: {other}"),
    };
    let mut rules: Vec<_> = config
        .permissions
        .deny
        .iter()
        .map(|tool| PermissionRule {
            tool: tool.clone(),
            decision: RuleDecision::Deny,
        })
        .collect();
    rules.extend(config.permissions.allow.iter().map(|tool| PermissionRule {
        tool: tool.clone(),
        decision: RuleDecision::Allow,
    }));
    Ok(PermissionPolicy {
        default,
        rules,
        allow_bash: config.permissions.allow_bash.clone(),
        ..Default::default()
    })
}

async fn compose_instructions(
    config: &BobConfig,
    workspace: PathBuf,
    worker: Option<String>,
) -> Result<String> {
    let configured_shell = config.shell.clone();
    let system_override = config.system.clone();
    let mut instructions = tokio::task::spawn_blocking(move || {
        let shell = CommandShell::resolve(&workspace, configured_shell.as_ref());
        bob_core::agent::prompt::build_system_prompt(system_override.as_deref(), &workspace, &shell)
    })
    .await
    .context("compose project context")?;
    if let Some(worker) = worker {
        instructions.push_str(&format!("\nUse model {worker} for delegated worker requests unless the user specifies another model."));
    }
    Ok(instructions)
}

fn resumed_permissions(config: PermissionPolicy, session: &SessionSnapshot) -> PermissionPolicy {
    PermissionPolicy {
        mode: session.permissions.mode,
        grants: session.permissions.grants.clone(),
        ..config
    }
}

fn prepare_launch(mut options: LaunchOptions) -> Result<PreparedLaunch> {
    options.workspace = std::fs::canonicalize(&options.workspace).context("resolve workspace")?;
    ensure!(options.workspace.is_dir(), "workspace must be a directory");
    options.database = storage::canonical_database(&options.database)?;
    let mut selected = selected_session(&options)?;
    let workspace = selected.as_ref().map_or_else(
        || options.workspace.clone(),
        |session| PathBuf::from(&session.workspace),
    );
    let workspace = std::fs::canonicalize(workspace).context("resolve stored session workspace")?;
    ensure!(workspace.is_dir(), "stored workspace must be a directory");
    let config = load_config(&workspace)?;
    let permission_policy = permissions(&config)?;
    launch_reasoning(&config, selected.as_ref().map(|session| &session.profile))?;
    let claim = selected
        .as_ref()
        .map(|session| SessionClaim::acquire(&options.database, session.id.clone()))
        .transpose()?;
    if let Some(claim) = &claim {
        let fresh = SessionCatalog::open(&options.database)?.session(claim.session().clone())?;
        ensure!(
            std::fs::canonicalize(&fresh.workspace)? == workspace,
            "session workspace changed during startup; select the session again"
        );
        selected = Some(fresh);
    }
    let reasoning = launch_reasoning(&config, selected.as_ref().map(|session| &session.profile))?;
    Ok(PreparedLaunch {
        options,
        workspace,
        selected,
        claim,
        config,
        permission_policy,
        reasoning,
    })
}

pub async fn new_session(handle: &Handle, source: &SessionSnapshot) -> Result<Receipt> {
    let view = handle
        .transcript(Some(source.thread.clone()), None, 1)
        .await?;
    let jobs = handle.jobs(source.id.clone()).await?;
    ensure!(
        !view.threads.iter().any(|thread| thread.session == source.id
            && matches!(
                thread.state,
                bob_kernel::ThreadState::Active | bob_kernel::ThreadState::Stopping
            ))
            && !jobs.iter().any(|job| matches!(
                job.state,
                bob_kernel::JobState::Queued
                    | bob_kernel::JobState::Running
                    | bob_kernel::JobState::Stopping
            ))
            && handle.queue(source.thread.clone()).await?.is_empty(),
        "finish or stop active agents and jobs first"
    );
    let profile = source.profile.clone();
    let mut permissions = source.permissions.clone();
    permissions.grants.clear();
    let receipt = handle
        .command(Command::CreateSessionConfigured {
            id: CommandId::new(),
            workspace: source.workspace.clone(),
            profile,
            policy: source.policy.clone(),
            permissions,
        })
        .await?;
    handle
        .activate_session(receipt.session.clone(), Vec::new())
        .await?;
    Ok(receipt)
}

pub async fn open(options: LaunchOptions) -> Result<Application> {
    open_with_titles(options, false).await
}

pub async fn open_with_titles(options: LaunchOptions, auto_titles: bool) -> Result<Application> {
    let PreparedLaunch {
        options,
        workspace,
        selected,
        claim,
        config,
        permission_policy,
        reasoning,
    } = tokio::task::spawn_blocking(move || prepare_launch(options))
        .await
        .context("prepare session startup")??;
    let requested = launch_model_spec(
        &options,
        &config,
        selected.as_ref().map(|session| &session.profile),
    );
    let mut models = vec![requested.clone()];
    if let Some(worker) = &options.worker_model {
        if !models.contains(worker) {
            models.push(worker.clone());
        }
    }
    let provider = ProviderService::create_configured(
        &models,
        StreamTimeouts {
            startup: Duration::from_secs(config.provider_timeouts.startup_secs),
            idle: Duration::from_secs(config.provider_timeouts.idle_secs),
        },
    )
    .await?;
    let model = provider.resolved_model_id(&requested)?;
    let worker = options
        .worker_model
        .as_deref()
        .map(|model| provider.resolved_model_id(model))
        .transpose()?;
    let instructions = compose_instructions(&config, workspace.clone(), worker).await?;
    let memories = Arc::new(storage::DeferredMemories::default());
    let services = Arc::new(Services::create(&config, &workspace, memories.clone()).await?);
    let configured_shell = config.shell.clone();
    let executor =
        adapters::configured_executor(provider.clone(), configured_shell, services.clone());
    if let Some(parent) = options
        .database
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create Bob store directory {}", parent.display()))?;
    }
    let runtime_options = RuntimeOptions { auto_titles };
    let handle = if let Some(claim) = claim {
        Runtime::open_claimed_with_options(&options.database, executor, runtime_options, claim)
            .await?
    } else {
        Runtime::open_paused_with_options(&options.database, executor, runtime_options).await?
    };
    let initialized: Result<SessionSnapshot> = async {
        let database = options.database.clone();
        tokio::task::spawn_blocking(move || memories.initialize(&database))
            .await
            .context("initialize session memories")??;
        if let Some(selected) = selected {
            let session = handle.session(selected.id).await?;
            let mut profile = session.profile.clone();
            profile.model = model;
            profile.instructions = instructions;
            profile.reasoning = reasoning;
            profile.max_requests = config.max_turns.unwrap_or(profile.max_requests);
            let mut policy = session.policy.clone();
            policy.denied_tools = config.permissions.deny.iter().cloned().collect();
            let permissions = resumed_permissions(permission_policy, &session);
            handle
                .activate_session(
                    session.id.clone(),
                    vec![
                        Command::SetPermissions {
                            id: CommandId::new(),
                            session: session.id.clone(),
                            permissions,
                            expected_revision: session.policy_revision,
                        },
                        Command::SetPolicy {
                            id: CommandId::new(),
                            session: session.id.clone(),
                            policy,
                            expected_revision: session.policy_revision + 1,
                        },
                        Command::UpdateProfile {
                            id: CommandId::new(),
                            thread: session.thread.clone(),
                            profile,
                            expected_revision: session.profile_revision,
                        },
                    ],
                )
                .await?;
            return handle.session(session.id).await;
        }
        let receipt = handle
            .command(Command::CreateSessionConfigured {
                id: CommandId::new(),
                workspace: workspace
                    .to_str()
                    .context("workspace must be UTF-8")?
                    .into(),
                profile: Profile {
                    model,
                    instructions,
                    read_only: false,
                    max_requests: config.max_turns.unwrap_or(200),
                    reasoning,
                },
                policy: Policy {
                    denied_tools: config.permissions.deny.iter().cloned().collect(),
                    ..Default::default()
                },
                permissions: permission_policy,
            })
            .await?;
        handle
            .activate_session(receipt.session.clone(), Vec::new())
            .await?;
        handle.session(receipt.session).await
    }
    .await;
    match initialized {
        Ok(session) => Ok(Application {
            handle,
            services,
            provider,
            session,
            config,
        }),
        Err(error) => {
            let _ = handle.shutdown().await;
            Err(error)
        }
    }
}

#[cfg(test)]
#[path = "application_model_tests.rs"]
mod model_tests;

#[cfg(test)]
#[path = "application_session_tests.rs"]
mod session_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> LaunchOptions {
        LaunchOptions {
            database: "fixture.sqlite".into(),
            workspace: ".".into(),
            provider: None,
            model: None,
            worker_model: None,
            selection: SessionSelection::New,
            page_size: 80,
        }
    }

    #[test]
    fn model_precedence_and_explicit_provider_identity() {
        let mut options = options();
        let mut config = BobConfig {
            provider: "copilot:embedded".into(),
            model: "configured".into(),
            ..Default::default()
        };
        assert_eq!(provider_spec(&options, &config), "copilot:configured");
        options.model = Some("flag".into());
        assert_eq!(provider_spec(&options, &config), "copilot:flag");
        options.model = Some("openai:explicit".into());
        assert_eq!(provider_spec(&options, &config), "openai:explicit");
        options.model = None;
        config.model.clear();
        assert_eq!(provider_spec(&options, &config), "copilot:embedded");
    }

    #[test]
    fn explicit_missing_session_does_not_create_a_database() {
        let mut options = options();
        options.database =
            std::env::temp_dir().join(format!("bob-missing-{}.sqlite", uuid::Uuid::new_v4()));
        options.selection = SessionSelection::Id(SessionId::new());
        assert!(selected_session(&options).is_err());
        assert!(!options.database.exists());
    }
}
