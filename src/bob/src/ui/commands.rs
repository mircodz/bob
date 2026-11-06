use super::{
    completion::Item,
    state::{Action, Outcome},
};
use anyhow::{bail, Context, Result};
use bob_core::core::commands::CustomCommand;
use bob_kernel::{Command, CommandId, Handle, SessionPolicy, ThreadSummary};
use bob_types::ReasoningEffort;
use std::{path::PathBuf, sync::Arc};

#[derive(Clone, Debug)]
pub(super) enum Request {
    New,
    Clear,
    Copy,
    Usage,
    Context,
    Compact,
    Model(Option<String>),
    Models,
    Reasoning(Option<ReasoningEffort>),
    Theme(Option<String>),
    Jobs,
    JobOutput(bob_kernel::JobId),
    StopJob(bob_kernel::JobId),
    Background,
    Recall {
        input: bob_kernel::QueuedInput,
        entry: Option<i64>,
    },
    Memory(usize),
    Mcp,
    Lsp,
    CycleMode,
}

pub(super) struct Picker {
    pub title: String,
    pub items: Vec<Item>,
    pub selected: usize,
    command: &'static str,
}
impl Picker {
    pub fn theme(current: &str) -> Self {
        Self {
            title: "Theme (↑↓ to preview, Enter to keep)".into(),
            items: bob_ui::theme::Theme::NAMES
                .iter()
                .map(|name| Item {
                    label: (*name).into(),
                    description: String::new(),
                })
                .collect(),
            selected: bob_ui::theme::Theme::NAMES
                .iter()
                .position(|name| *name == current)
                .unwrap_or(0),
            command: "/theme",
        }
    }
    pub fn is_theme(&self) -> bool {
        self.command == "/theme"
    }
    pub fn command(&self) -> Option<String> {
        self.items
            .get(self.selected)
            .map(|item| format!("{} {}", self.command, item.label))
    }
}

pub(super) fn route(
    text: &str,
    thread: &ThreadSummary,
    policy: Option<&SessionPolicy>,
    custom: &[CustomCommand],
) -> Result<Action> {
    let (verb, arg) = text
        .trim()
        .split_once(char::is_whitespace)
        .map_or((text.trim(), ""), |(verb, arg)| (verb, arg.trim()));
    let request = match verb {
        "/new" => Request::New,
        "/clear" => Request::Clear,
        "/copy" => Request::Copy,
        "/usage" => Request::Usage,
        "/context" => Request::Context,
        "/compact" => Request::Compact,
        "/model" => Request::Model((!arg.is_empty()).then(|| arg.to_owned())),
        "/models" if arg.is_empty() => Request::Models,
        "/models" => Request::Model(Some(arg.to_owned())),
        "/reasoning" => Request::Reasoning(if arg.is_empty() {
            None
        } else {
            Some(ReasoningEffort::parse(arg).context("Reasoning must be off/low/medium/high/max")?)
        }),
        "/theme" => Request::Theme((!arg.is_empty()).then(|| arg.to_owned())),
        "/jobs" => match arg.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] => Request::Jobs,
            ["stop", id] => Request::StopJob(bob_kernel::JobId((*id).into())),
            [id] => Request::JobOutput(bob_kernel::JobId((*id).into())),
            _ => bail!("Usage: /jobs [job-id] or /jobs stop <job-id>"),
        },
        "/memory" => Request::Memory(if arg.is_empty() {
            0
        } else {
            arg.parse()
                .context("Memory offset must be a nonnegative integer")?
        }),
        "/mcp" => Request::Mcp,
        "/lsp" => Request::Lsp,
        "/init" => {
            return Ok(Action::Command(Command::Submit {
                id: CommandId::new(),
                thread: thread.id.clone(),
                text: INIT_PROMPT.into(),
            }))
        }
        _ => {
            if !COMMANDS.iter().any(|(name, _)| *name == verb) {
                if let Some(command) = custom
                    .iter()
                    .find(|command| Some(command.name.as_str()) == verb.strip_prefix('/'))
                {
                    let expanded = command.expand(arg);
                    anyhow::ensure!(
                        expanded.len() <= super::composer::INPUT_LIMIT,
                        "Expanded command exceeds 64 KiB; draft preserved"
                    );
                    return Ok(Action::Command(Command::Submit {
                        id: CommandId::new(),
                        thread: thread.id.clone(),
                        text: expanded,
                    }));
                }
            }
            return super::route(text, thread, policy);
        }
    };
    Ok(Action::Application(request))
}

#[cfg(test)]
pub(super) async fn execute(
    request: Request,
    handle: &Handle,
    thread: &ThreadSummary,
    workspace: Option<PathBuf>,
    services: Option<Arc<bob::services::Services>>,
    provider: Option<bob::provider::ProviderService>,
) -> Result<Outcome> {
    execute_with_defaults(
        request,
        handle,
        thread,
        workspace,
        services,
        provider,
        &DefaultsWriter::default(),
    )
    .await
}

pub(super) async fn execute_with_defaults(
    request: Request,
    handle: &Handle,
    thread: &ThreadSummary,
    workspace: Option<PathBuf>,
    services: Option<Arc<bob::services::Services>>,
    provider: Option<bob::provider::ProviderService>,
    defaults: &DefaultsWriter,
) -> Result<Outcome> {
    let id = CommandId::new();
    let command = match request {
        Request::New => {
            let view = handle.view(Some(thread.id.clone()), None, 1).await?;
            anyhow::ensure!(
                view.error.is_none(),
                "Kernel state is unavailable; retry after recovery"
            );
            let threads: Vec<_> = view
                .threads
                .iter()
                .filter(|item| item.session == thread.session)
                .collect();
            let jobs = handle.jobs(thread.session.clone()).await?;
            ensure_session_idle(&threads, &jobs)?;
            for thread in threads {
                anyhow::ensure!(
                    handle.queue(thread.id.clone()).await?.is_empty(),
                    "finish or stop active agents and jobs first; queued input remains"
                );
            }
            let session = handle.session(thread.session.clone()).await?;
            let receipt = bob::application::new_session(handle, &session).await?;
            return Ok(Outcome::Session(receipt));
        }
        Request::Clear => {
            let context = handle.context(thread.id.clone()).await?;
            Command::ClearContext {
                id,
                thread: thread.id.clone(),
                expected_epoch: context.epoch,
            }
        }
        Request::Compact => {
            let context = handle.context(thread.id.clone()).await?;
            Command::Compact {
                id,
                thread: thread.id.clone(),
                expected_revision: context.revision,
            }
        }
        Request::Context => {
            let context = handle.context(thread.id.clone()).await?;
            let budget = &context.budget;
            return Ok(Outcome::Notice(format!("Context · {} messages\n{} estimated input tokens · {} context window\n{} input limit · {} reserved output tokens{}", context.message_count, budget.estimated_input_tokens, budget.context_window, budget.input_limit, budget.output_reserve, if context.compact_pending { "\nCompaction pending" } else { "" })));
        }
        Request::Usage => {
            let session = handle.usage(thread.session.clone()).await?;
            let all = handle.usage_all().await?;
            let text = [("Session", session), ("All-time", all)].into_iter().map(|(label, usage)| format!("{label} usage · {} requests ({} compaction)\n{} input · {} output · {} cache read · {} cache write tokens", usage.requests, usage.compaction_requests, usage.usage.input_tokens, usage.usage.output_tokens, usage.usage.cache_read_input_tokens, usage.usage.cache_creation_input_tokens)).collect::<Vec<_>>().join("\n\n");
            return Ok(Outcome::Notice(text));
        }
        Request::Model(model) => {
            let Some(model) = model else {
                let session = handle.context(thread.id.clone()).await?;
                return Ok(Outcome::Notice(format!(
                    "model: {} {}",
                    session.profile.model,
                    session.profile.reasoning.label()
                )));
            };
            return update_model(handle, thread, &model, defaults, |requested| async move {
                provider
                    .context("Provider service has not connected")?
                    .ensure_model(&requested)
                    .await
            })
            .await;
        }
        Request::Models => {
            let current = handle.context(thread.id.clone()).await?.profile.model;
            let items: Vec<_> = handle
                .refresh_models()
                .await?
                .into_iter()
                .map(|entry| Item {
                    label: entry.model.id,
                    description: format!("{} context tokens", entry.model.context_window),
                })
                .collect();
            let selected = items
                .iter()
                .position(|item| item.label == current)
                .unwrap_or(0);
            return Ok(Outcome::Picker(Picker {
                title: format!("Select a model (current: {current})"),
                items,
                selected,
                command: "/model",
            }));
        }
        Request::Reasoning(effort) => {
            let session = handle.context(thread.id.clone()).await?;
            let Some(effort) = effort else {
                let values = [
                    ReasoningEffort::Off,
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::Max,
                ];
                let selected = values
                    .iter()
                    .position(|value| *value == session.profile.reasoning)
                    .unwrap_or(0);
                return Ok(Outcome::Picker(Picker {
                    title: format!(
                        "Reasoning effort (current: {})",
                        session.profile.reasoning.label()
                    ),
                    items: values
                        .iter()
                        .map(|value| Item {
                            label: value.label().into(),
                            description: String::new(),
                        })
                        .collect(),
                    selected,
                    command: "/reasoning",
                }));
            };
            let root = handle.session(thread.session.clone()).await?.thread == thread.id;
            let mut context = session;
            context.profile.reasoning = effort;
            return commit_profile(
                handle,
                context,
                root.then_some(DefaultChange::Reasoning(effort)),
                defaults,
            )
            .await;
        }
        Request::Theme(name) => {
            let Some(name) = name else {
                return Ok(Outcome::Picker(Picker::theme("dark")));
            };
            let workspace = workspace.context("Workspace metadata unavailable")?;
            let saved = name.clone();
            tokio::task::spawn_blocking(move || {
                bob_core::core::config::set_theme_in_project(&workspace, &saved)
            })
            .await??;
            return Ok(Outcome::Theme(name));
        }
        Request::Copy => {
            let mut before = None;
            loop {
                let view = handle
                    .transcript(Some(thread.id.clone()), before, 200)
                    .await?;
                if let Some(entry) = view
                    .entries
                    .iter()
                    .rev()
                    .find(|entry| entry.kind == bob_kernel::EntryKind::Assistant)
                {
                    let mut text = String::new();
                    let mut offset = 0;
                    loop {
                        let page = handle
                            .entry_text(thread.id.clone(), entry.sequence, offset, 16384)
                            .await?;
                        text.push_str(&page.text);
                        if let Some(next) = page.next_offset {
                            offset = next;
                        } else {
                            break;
                        }
                    }
                    return Ok(Outcome::Copy(text));
                }
                let Some(first) = view.entries.first() else {
                    bail!("No assistant message to copy");
                };
                before = Some(first.sequence);
            }
        }
        Request::Memory(offset) => {
            let services = services.context("Memory service has not connected")?;
            let text = tokio::task::spawn_blocking(move || -> Result<String> {
                use bob_core::core::memory::MemoryScope;
                let mut lines = Vec::new();
                for (label, scope) in [
                    ("Project", services.project_scope()),
                    ("Global", MemoryScope::Global),
                ] {
                    let entries = services.memory_entries(&scope, offset)?;
                    lines.push(format!("{label} memories · offset {offset}"));
                    if entries.is_empty() {
                        lines.push("No saved memories.".into());
                    }
                    for entry in &entries {
                        lines.push(format!("#{} {}", entry.id, entry.content));
                    }
                    if entries.len() == 50 {
                        lines.push(format!("Next page: /memory {}", offset + 50));
                    }
                }
                Ok(lines.join("\n"))
            })
            .await??;
            return Ok(Outcome::Notice(text));
        }
        Request::Mcp => {
            let services = services.context("MCP service has not connected")?;
            let statuses = services.mcp_statuses();
            let text = if statuses.is_empty() {
                "No configured MCP servers.".into()
            } else {
                statuses
                    .into_iter()
                    .map(|status| {
                        format!(
                            "{} · {:?} · {:?} · {} tools{}",
                            status.name,
                            status.transport,
                            status.state,
                            status.tools,
                            status
                                .message
                                .map_or(String::new(), |message| format!(" · {message}"))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            return Ok(Outcome::Notice(text));
        }
        Request::Lsp => {
            let services = services.context("LSP service has not connected")?;
            let statuses = services.lsp_statuses();
            let text = if statuses.is_empty() {
                "No language servers started.".into()
            } else {
                statuses
                    .into_iter()
                    .map(|(name, health)| format!("{name} · {health:?}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            return Ok(Outcome::Notice(text));
        }
        Request::CycleMode => {
            let session = handle.session(thread.session.clone()).await?;
            let mut permissions = session.permissions;
            permissions.mode = match permissions.mode {
                bob_kernel::PermissionMode::Normal => bob_kernel::PermissionMode::AutoAccept,
                bob_kernel::PermissionMode::AutoAccept => bob_kernel::PermissionMode::Plan,
                bob_kernel::PermissionMode::Plan => bob_kernel::PermissionMode::Normal,
            };
            let receipt = handle
                .command(Command::SetPermissions {
                    id,
                    session: thread.session.clone(),
                    permissions,
                    expected_revision: session.policy_revision,
                })
                .await?;
            return Ok(Outcome::PermissionsUpdated(receipt));
        }
        Request::Jobs => {
            return Ok(Outcome::Notice(super::jobs::notice(
                &handle.jobs(thread.session.clone()).await?,
            )));
        }
        Request::JobOutput(job) => {
            anyhow::ensure!(
                handle
                    .jobs(thread.session.clone())
                    .await?
                    .iter()
                    .any(|item| item.id == job),
                "Job is not in this session"
            );
            let page = handle.job_output(job.clone(), 0, 16384).await?;
            return Ok(Outcome::JobOutput(job, page));
        }
        Request::StopJob(job) => {
            anyhow::ensure!(
                handle
                    .jobs(thread.session.clone())
                    .await?
                    .iter()
                    .any(|item| item.id == job),
                "Job is not in this session"
            );
            Command::StopJob { id, job }
        }
        Request::Background => Command::Background {
            id,
            thread: thread.id.clone(),
        },
        Request::Recall { mut input, entry } => {
            anyhow::ensure!(
                input.thread == thread.id,
                "Queued input belongs to another thread"
            );
            if let Some(sequence) = entry {
                input.text.clear();
                let mut offset = 0;
                loop {
                    let page = handle
                        .entry_text(thread.id.clone(), sequence, offset, 16384)
                        .await?;
                    anyhow::ensure!(
                        page.total_bytes <= super::composer::INPUT_LIMIT,
                        "Queued input exceeds 64 KiB; input remains queued"
                    );
                    input.text.push_str(&page.text);
                    if let Some(next) = page.next_offset {
                        offset = next;
                    } else {
                        break;
                    }
                }
            }
            anyhow::ensure!(
                input.text.len() <= super::composer::INPUT_LIMIT,
                "Queued input exceeds 64 KiB; input remains queued"
            );
            let receipt = handle
                .command(Command::RecallSubmission {
                    id,
                    submission: input.id.clone(),
                })
                .await?;
            return Ok(Outcome::Recalled(receipt, input));
        }
    };
    handle.command(command).await.map(Outcome::Receipt)
}

#[derive(Clone, Default)]
pub(super) struct DefaultsWriter {
    path: Option<PathBuf>,
}

impl DefaultsWriter {
    #[cfg(test)]
    pub(super) fn at(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn save(&self, change: DefaultChange) -> Result<()> {
        use bob_core::core::config;
        match change {
            DefaultChange::Model(model) => {
                let (provider, model) = model
                    .split_once(':')
                    .context("resolved model has no provider identity")?;
                match &self.path {
                    Some(path) => config::set_default_model_at(path, provider, model),
                    None => config::set_default_model(provider, model),
                }
            }
            DefaultChange::Reasoning(effort) => match &self.path {
                Some(path) => config::set_default_reasoning_at(path, effort.label()),
                None => config::set_default_reasoning(effort.label()),
            },
        }
    }
}

enum DefaultChange {
    Model(String),
    Reasoning(ReasoningEffort),
}

async fn commit_profile(
    handle: &Handle,
    context: bob_kernel::ContextSnapshot,
    change: Option<DefaultChange>,
    defaults: &DefaultsWriter,
) -> Result<Outcome> {
    static WRITES: std::sync::OnceLock<Arc<tokio::sync::Mutex<()>>> = std::sync::OnceLock::new();
    let guard = WRITES
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    let receipt = handle
        .command(Command::UpdateProfile {
            id: CommandId::new(),
            thread: context.thread,
            profile: context.profile,
            expected_revision: context.profile_revision,
        })
        .await?;
    if let Some(change) = change {
        let setting = match &change {
            DefaultChange::Model(model) => format!("Model changed to {model}"),
            DefaultChange::Reasoning(effort) => format!("Reasoning changed to {}", effort.label()),
        };
        let defaults = defaults.clone();
        let saved = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            defaults.save(change)
        })
        .await
        .context("defaults writer task failed")
        .and_then(|saved| saved);
        if let Err(error) = saved {
            return Ok(Outcome::Notice(format!(
                "{setting} for this session, but saving global defaults failed: {error:#}. The session change remains active."
            )));
        }
    }
    Ok(Outcome::Receipt(receipt))
}

pub(super) async fn update_model<F, Fut>(
    handle: &Handle,
    thread: &ThreadSummary,
    model: &str,
    defaults: &DefaultsWriter,
    discover: F,
) -> Result<Outcome>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let mut context = handle.context(thread.id.clone()).await?;
    let root = handle.session(thread.session.clone()).await?.thread == thread.id;
    let requested = bob::provider::qualify_model_id(model, &context.profile.model)?;
    let model = discover(requested).await?;
    handle.refresh_models().await?;
    context.profile.model = model.clone();
    commit_profile(
        handle,
        context,
        root.then_some(DefaultChange::Model(model)),
        defaults,
    )
    .await
}

pub(super) fn ensure_session_idle(
    threads: &[&ThreadSummary],
    jobs: &[bob_kernel::JobSummary],
) -> Result<()> {
    use bob_kernel::{JobState, ThreadState};
    anyhow::ensure!(
        threads.iter().all(|thread| !matches!(
            thread.state,
            ThreadState::Active
                | ThreadState::Stopping
                | ThreadState::Interrupted
                | ThreadState::Held
        )) && jobs.iter().all(|job| matches!(
            job.state,
            JobState::Completed | JobState::Failed | JobState::Cancelled
        )),
        "finish or stop active agents and jobs first"
    );
    Ok(())
}

pub(super) use bob_ui::commands::{COMMANDS, INIT_PROMPT};
