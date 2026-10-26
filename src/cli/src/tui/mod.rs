mod clipboard;
mod diffview;
mod draw;
mod exploration;
mod files;
mod highlight;
mod input;
mod markdown;
mod prompts;
mod render;
mod scrollback;
mod session_ui;
mod team;
mod terminal;
pub(super) use terminal::TerminalGuard;
mod theme;
mod view;
mod widgets;

use bob_core::agent::agent::{Agent, RunStopReason};
use bob_core::core::config::BobConfig;
use bob_core::core::events::{AgentEvent, EventBus};
use bob_core::core::permissions::{
    Asker, Decision, Mode, PermissionEngine, PermissionOption, PermissionRequest,
};
use bob_core::core::policies::{
    allow_bash_commands, allow_code_action_list, allow_read_only, allow_tools, deny_tools,
    flag_dangerous_bash, flag_out_of_workspace_paths,
};
use bob_core::core::session::Session;
use bob_core::providers::create_provider_with_options;
use bob_core::providers::provider::Provider;

use async_trait::async_trait;
use crossterm::event::{
    EnableBracketedPaste, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, Mutex};
use view::{Cell, ViewModel};

pub(super) fn shimmer_spans(text: &str, tick: usize) -> Vec<Span<'static>> {
    const BASE: (f32, f32, f32) = (0x66 as f32, 0x66 as f32, 0x66 as f32);
    const PEAK: (f32, f32, f32) = (0xf0 as f32, 0xf0 as f32, 0xf0 as f32);
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(1) as f32;
    // Travel beyond the text for a pause between sweeps.
    let span = n + 6.0;
    let pos = (tick as f32 * 0.6) % span;
    let width = 3.0_f32;
    chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let d = (i as f32 - pos).abs();
            let t = if d >= width { 0.0 } else { 1.0 - d / width };
            let t = t * t * (3.0 - 2.0 * t); // smoothstep
            let lerp = |a: f32, b: f32| (a + (b - a) * t) as u8;
            let color = Color::Rgb(
                lerp(BASE.0, PEAK.0),
                lerp(BASE.1, PEAK.1),
                lerp(BASE.2, PEAK.2),
            );
            Span::styled(c.to_string(), Style::default().fg(color))
        })
        .collect()
}

struct PermPrompt {
    title: String,
    detail: String,
    preview: Option<String>,
    options: Vec<PermissionOption>,
    resp: oneshot::Sender<Option<usize>>,
}

struct PendingPerm {
    title: String,
    detail: String,
    preview: Option<String>,
    options: Vec<PermissionOption>,
    list: widgets::SelectList,
    resp: oneshot::Sender<Option<usize>>,
}

struct TuiAsker {
    tx: mpsc::UnboundedSender<PermPrompt>,
}

struct QueryPrompt {
    query: bob_core::tools::registry::UserQuery,
    resp: oneshot::Sender<Option<String>>,
}

struct TuiUserAsker {
    tx: mpsc::UnboundedSender<QueryPrompt>,
}

#[async_trait]
impl bob_core::tools::registry::UserAsker for TuiUserAsker {
    async fn ask(&self, query: &bob_core::tools::registry::UserQuery) -> Option<String> {
        let (resp_tx, resp_rx) = oneshot::channel();
        if self
            .tx
            .send(QueryPrompt {
                query: query.clone(),
                resp: resp_tx,
            })
            .is_err()
        {
            return None;
        }
        resp_rx.await.unwrap_or(None)
    }
}

enum QueryPurpose {
    /// Return tool answers over the oneshot; UI picker answers become actions.
    Tool(oneshot::Sender<Option<String>>),
    ModelPicker,
    ReasoningPicker,
    /// Enter persists the live preview; Esc restores the original theme.
    ThemePicker {
        original: String,
    },
}

struct PendingQuery {
    query: bob_core::tools::registry::UserQuery,
    /// Selection cursor over the options (+ the "Other" row when allowed).
    list: widgets::SelectList,
    /// When the user picks "Other", this holds the typed answer (Some = typing).
    other_text: Option<String>,
    inspection: prompts::QueryScroll,
    purpose: QueryPurpose,
}

#[async_trait]
impl Asker for TuiAsker {
    async fn ask(&self, req: &PermissionRequest, options: &[PermissionOption]) -> Option<usize> {
        let (title, detail, preview) = if req.tool == "bash" {
            let bash = req.bash.as_ref();
            let raw = bash.map(|b| b.raw.clone()).unwrap_or_default();
            // Include programs inside pipelines and substitutions in the approval preview.
            let preview = bash.and_then(|b| {
                if !b.analyzable {
                    Some("[!] couldn't fully parse this command — review it carefully.".to_string())
                } else {
                    let names = b.program_names();
                    if names.len() > 1 {
                        Some(format!("runs: {}", names.join(", ")))
                    } else {
                        None
                    }
                }
            });
            ("Run shell command?".to_string(), raw, preview)
        } else {
            (
                format!("Allow {}?", req.tool),
                describe_input(&req.tool, &req.input),
                req.preview.clone(),
            )
        };
        let title = match &req.agent {
            Some(name) => format!("{title}  — for {name}"),
            None => title,
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        let prompt = PermPrompt {
            title,
            detail,
            preview,
            options: options.to_vec(),
            resp: resp_tx,
        };
        if self.tx.send(prompt).is_err() {
            return None;
        }
        resp_rx.await.unwrap_or(None)
    }
}

fn describe_input(tool: &str, input: &serde_json::Value) -> String {
    let arg = |k: &str| {
        input
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    match tool {
        "write_file" | "edit_file" | "multi_edit" | "read_file" | "list_dir" => arg("path"),
        "web_fetch" => arg("url"),
        "glob" | "grep" => arg("pattern"),
        _ => String::new(),
    }
}

fn now_stamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

pub(super) fn fmt_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let (m, s) = (secs / 60, secs % 60);
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s:02}s")
        }
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn fmt_ctx_window(tokens: usize) -> String {
    let n = tokens as f64;
    let mag = if tokens >= 1_000_000 {
        let v = n / 1_000_000.0;
        format!("{}M", trim_dot(v))
    } else if tokens >= 1_000 {
        let v = n / 1_000.0;
        format!("{}k", trim_dot(v))
    } else {
        tokens.to_string()
    };
    format!("{} ctx", mag)
}

fn trim_dot(v: f64) -> String {
    let s = format!("{:.1}", v);
    s.trim_end_matches(".0").to_string()
}

fn abbreviate_home(path: &std::path::Path) -> String {
    let full = path.to_string_lossy().to_string();
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy().to_string();
        if let Some(rest) = full.strip_prefix(&home) {
            return format!("~{}", rest);
        }
    }
    full
}

async fn git_branch(path: &std::path::Path) -> Option<String> {
    let out = files::git_output(path, &["rev-parse", "--abbrev-ref", "HEAD"], 4096).await?;
    let name = String::from_utf8_lossy(&out).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct TurnCompletion {
    error: Option<String>,
    stopped: bool,
    panicked: bool,
}

impl TurnCompletion {
    fn from_outcome(outcome: bob_core::agent::agent::RunOutcome) -> Self {
        let has_summary = matches!(
            outcome.stop_reason,
            RunStopReason::Completed
                | RunStopReason::OutputLimit
                | RunStopReason::TurnLimit
                | RunStopReason::ToolPanic
        );
        Self {
            error: (!has_summary).then_some(outcome.text),
            stopped: outcome.stop_reason != RunStopReason::Completed,
            panicked: false,
        }
    }

    fn panicked() -> Self {
        Self {
            error: Some("Root agent panicked; outcome unknown and changes may have taken effect. Automatic work is stopped. Exit and restart Bob before continuing; inspect changes before retrying.".into()),
            stopped: true,
            panicked: true,
        }
    }
}

fn spawn_turn(
    agent: &Arc<Mutex<Agent>>,
    done_tx: &mpsc::UnboundedSender<TurnCompletion>,
    text: String,
) -> tokio::task::JoinHandle<()> {
    let agent = agent.clone();
    let done_tx = done_tx.clone();
    tokio::spawn(async move {
        let result = bob_core::core::panic::catch(async {
            let mut agent = agent.lock().await;
            agent
                .cancel_handle()
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let budget = agent.turn_budget();
            agent.run_with_budget(&text, budget).await
        })
        .await;
        let completion = match result {
            Ok(Ok(outcome)) => TurnCompletion::from_outcome(outcome),
            Ok(Err(error)) => TurnCompletion {
                error: Some(error.to_string()),
                stopped: true,
                panicked: false,
            },
            Err(_) => TurnCompletion::panicked(),
        };
        let _ = done_tx.send(completion);
    })
}

/// Off-loop work delivered back to the UI through `apply_bg`.
enum BgOutcome {
    ProviderSwitched(Result<(String, Arc<dyn Provider>), String>),
    ModelList(Result<Vec<String>, String>),
    FileList(Vec<String>),
    Compacted {
        cell: usize,
        result: Result<(bool, usize, usize), String>,
    },
    CompactionPanicked {
        cell: usize,
    },
    SessionSaveFailed(String),
}

fn spawn_bg<F>(tx: &mpsc::UnboundedSender<BgOutcome>, fut: F) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = BgOutcome> + Send + 'static,
{
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(fut.await);
    })
}

pub async fn run(
    config: BobConfig,
    provider: Arc<dyn Provider>,
    provider_spec: String,
    cwd: PathBuf,
    mut session: Session,
) -> anyhow::Result<String> {
    theme::set_theme(theme::Theme::by_name(
        config.theme.as_deref().unwrap_or("dark"),
    ));

    let bus = EventBus::new();

    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
    {
        let evt_tx = evt_tx.clone();
        bus.on(Arc::new(move |e: &AgentEvent| {
            // Route by agent id when applying events, not when forwarding them.
            let _ = evt_tx.send(e.clone());
        }));
    }

    // The store's writer thread keeps event appends off the async task.
    let store: Arc<dyn bob_core::core::store::SessionStore> =
        Arc::new(tokio::task::spawn_blocking(bob_core::core::store::SqliteStore::default).await?);

    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<PermPrompt>();
    let asker = Arc::new(TuiAsker { tx: perm_tx });

    let (query_tx, mut query_rx) = mpsc::unbounded_channel::<QueryPrompt>();
    let user_asker = Arc::new(TuiUserAsker { tx: query_tx });

    let default_decision = match config.permissions.default.as_str() {
        "allow" => Decision::Allow,
        "deny" => Decision::Deny,
        _ => Decision::Ask,
    };
    let mut engine = PermissionEngine::new(default_decision, Some(asker));
    engine.add(deny_tools(config.permissions.deny.clone()));
    engine.add(flag_out_of_workspace_paths());
    engine.add(flag_dangerous_bash());
    engine.add(allow_read_only());
    engine.add(allow_code_action_list());
    engine.add(allow_bash_commands(config.permissions.allow_bash.clone()));
    engine.add(allow_tools(config.permissions.allow.clone()));
    let permissions = Arc::new(engine);
    if !session.grants.is_empty() {
        permissions.import_grants(session.grants.clone());
    }

    let jobs = bob_core::tools::jobs::JobRegistry::new();

    let team = bob_core::agent::team::AgentRegistry::new();

    let (mcp_tools, mcp_notices) = bob_core::mcp::connect_all(&config.mcp_servers).await;

    let lsp = if config.lsp_servers.is_empty() {
        None
    } else {
        Some(bob_core::lsp::LspManager::start(&config.lsp_servers, &cwd))
    };

    let shell = Arc::new(bob_core::core::shell::BashShell::resolve(
        &cwd,
        config.bash_executable.as_deref(),
    ));
    let system_prompt =
        bob_core::agent::prompt::build_system_prompt(config.system.as_deref(), &cwd, &shell);
    let memory = store.memory_repository();

    let mut agent =
        bob_core::agent::assembly::build_root_agent(bob_core::agent::assembly::RootAgentParams {
            provider: provider.clone(),
            permissions: permissions.clone(),
            bus: bus.clone(),
            jobs: jobs.clone(),
            team: team.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            system_prompt: system_prompt.clone(),
            shell,
            mcp_tools: mcp_tools.clone(),
            extra_tools: Vec::new(),
            lsp: lsp.clone(),
            user_asker: user_asker.clone(),
            max_turns: config.max_turns,
            definitions: std::collections::HashMap::new(),
            hooks: bob_core::agent::hooks::Hooks::default(),
            memory,
        });
    let history = store.history_for(&session)?;
    let working = store.working_history_for(&session)?;
    let archived_events = store.load_events(&session.id)?.unwrap_or_default();
    let epoch = bob_core::core::session::context_epoch(&archived_events);
    if epoch > session.context_epoch {
        session.context_epoch = epoch;
        session.todos.clear();
        session.subagent_runs.clear();
        session.agent_threads.clear();
        session.workflows.clear();
    }
    session.messages = history.clone();
    bob_core::core::session_runtime::reserve_runtime_ids(&session, &archived_events, &jobs);
    agent.load_histories(history, working);
    // Cancellation must not need the mutex held by the running turn.
    let cancel = agent.cancel_handle();
    let todos = agent.todos();
    if !session.todos.is_empty() {
        todos.set(session.todos.clone());
    }
    let startup_reasoning = bob_core::core::types::ReasoningEffort::parse(&config.reasoning)
        .unwrap_or(bob_core::core::types::ReasoningEffort::Off);
    agent.set_reasoning(startup_reasoning);
    let agent = Arc::new(Mutex::new(agent));

    // Activate the restore guard before any fallible terminal setup.
    let _guard = TerminalGuard::activate()?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !bob_core::core::panic::is_contained() {
            default_hook(info);
        }
    }));
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(
        jobs.clone(),
        permissions.clone(),
        cancel.clone(),
        team.clone(),
    );
    let owner = bob_core::core::session_runtime::SessionOwner::new(store.clone(), &session);
    let mut session_subscription = owner.subscribe(&bus);
    app.provider_id = provider_spec.split(':').next().unwrap_or("").to_string();
    app.model_label = provider.model().to_string();
    app.reasoning = startup_reasoning;
    app.cwd_label = abbreviate_home(&cwd);
    app.branch = git_branch(&cwd).await;
    app.context_usage = agent.try_lock().ok().map(|agent| agent.context_usage());
    app.lsp = lsp.clone();
    app.bus = bus.clone();
    app.system_prompt = system_prompt.clone();
    app.todos = Some(todos);
    app.custom_commands = bob_core::core::commands::load_custom_commands(&cwd);
    if !session.agent_threads.is_empty() {
        app.teams = team::AgentTranscripts::from_persisted(&session.agent_threads);
    }
    app.theme_name = config.theme.clone().unwrap_or_else(|| "dark".to_string());
    app.session_usage = bob_core::core::usage::total_of(&session.usage);
    app.global_usage = bob_core::core::usage::global_total();
    if !session.messages.is_empty() || !archived_events.is_empty() {
        // Replay must not lose history or revive an older context epoch.
        let replayed = match store.load_events(&session.id) {
            Ok(Some(events)) => {
                let rebuilt = bob_core::core::session::root_history_from_events(&events);
                if bob_core::core::session::context_epoch(&events) >= session.context_epoch
                    && rebuilt.len() >= session.messages.len()
                {
                    app.teams = team::AgentTranscripts::replay(&events);
                    for evt in &events {
                        app.view.apply(evt);
                    }
                    app.view.reset_after_replay();
                    true
                } else {
                    false
                }
            }
            Err(error) => {
                app.view
                    .push_notice(format!("could not replay session events: {error}"));
                false
            }
            Ok(None) => false,
        };
        if !replayed {
            app.view.hydrate(&session.messages, &session.workflows);
        }
        app.view.push_event(format!(
            "Resumed session {} ({} msgs)",
            session.id,
            session.messages.len()
        ));
    }
    for id in app.teams.display_order() {
        app.jobs.reserve_id(&id);
        bob_core::tools::task::reserve_subagent_id(&id);
    }
    for note in &mcp_notices {
        app.view.push_event(note.clone());
    }

    let mut keys = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    // Coalesce input bursts into one repaint per frame to avoid scroll backlog.
    const FRAME: Duration = Duration::from_millis(16);
    draw_frame(&mut terminal, &mut app)?;
    let mut last_draw = tokio::time::Instant::now();
    let mut dirty = false;
    let mut root_turn: Option<tokio::task::JoinHandle<()>> = None;
    let timeouts = bob_core::providers::provider::StreamTimeouts {
        startup: Duration::from_secs(config.provider_timeouts.startup_secs),
        idle: Duration::from_secs(config.provider_timeouts.idle_secs),
    };

    let result: anyhow::Result<()> = 'outer: loop {
        dirty |= app.clear_closed_prompts();
        if dirty && last_draw.elapsed() >= FRAME {
            if let Err(error) = draw_frame(&mut terminal, &mut app) {
                break 'outer Err(error.into());
            }
            last_draw = tokio::time::Instant::now();
            dirty = false;
        }
        // Wake at the frame deadline only while a repaint is pending.
        let redraw_deadline = if dirty {
            last_draw + FRAME
        } else {
            tokio::time::Instant::now() + Duration::from_secs(3600)
        };

        tokio::select! {
            _ = tokio::time::sleep_until(redraw_deadline) => {
                // Wake to repaint at the frame boundary; the top of the loop draws.
            }
            maybe_key = keys.next() => {
                match maybe_key {
                    Some(Ok(event)) => {
                        let outcome = app.on_terminal_event(event, &mut dirty);
                        if matches!(outcome, KeyOutcome::ClearContext | KeyOutcome::NewSession) {
                            while let Ok(event) = evt_rx.try_recv() {
                                app.apply_session_event(&mut session, &event);
                            }
                            app.transition_session(
                                matches!(outcome, KeyOutcome::NewSession), &agent, &owner,
                                &mut session, &mut session_subscription, &cwd,
                            ).await;
                            continue 'outer;
                        }
                        match outcome {
                            KeyOutcome::Quit => break 'outer Ok(()),
                            KeyOutcome::Submit(text) => {
                                if app.runtime_failed {
                                    app.notify("Root runtime failed; exit and restart Bob before continuing");
                                    continue 'outer;
                                }
                                if let Some(id) = app.focused_agent.clone() {
                                    app.send_agent_message(&id, &text);
                                } else if app.running {
                                    // Queued prompts enter the transcript only when dispatched.
                                    app.queue.push_back(text);
                                } else {
                                    // UserPrompt owns rendering for both live turns and replay.
                                    app.stick_to_bottom();
                                    app.running = true;
                                    app.current_prompt = Some(text.clone());
                                    app.turn_started = Some(std::time::Instant::now());
                                    root_turn = Some(spawn_turn(&agent, &app.turn_done_tx, text));
                                }
                            }
                            KeyOutcome::Steer(text) => {
                                // Steering goes to the running turn's inbox, not the prompt queue.
                                if app.agent_team.send("root", "user", &text) {
                                    app.bus.emit(AgentEvent::UserMessageQueued {
                                        to: "root".into(), text,
                                    });
                                    app.stick_to_bottom();
                                } else {
                                    app.notify("couldn't steer — no running agent");
                                }
                            }
                            KeyOutcome::SwitchModel(spec) => {
                                if app.running {
                                    app.notify("finish the current turn first");
                                } else {
                                    let full = if spec.contains(':') {
                                        spec.clone()
                                    } else {
                                        format!("{}:{}", app.provider_id, spec)
                                    };
                                    app.notify("switching model…");
                                    spawn_bg(&app.bg_tx, async move {
                                        let id = full.split(':').next().unwrap_or("").to_string();
                                        BgOutcome::ProviderSwitched(
                                            create_provider_with_options(&full, None, timeouts)
                                                .await
                                                .map(|p| (id, p))
                                                .map_err(|e| e.to_string()),
                                        )
                                    });
                                }
                            }
                            KeyOutcome::ListReasoning => {
                                let cur = app.reasoning;
                                app.open_reasoning_picker(cur);
                            }
                            KeyOutcome::SetReasoning(label) => {
                                let effort = bob_core::core::types::ReasoningEffort::parse(&label)
                                    .unwrap_or(bob_core::core::types::ReasoningEffort::Off);
                                if let Ok(mut a) = agent.try_lock() {
                                    a.set_reasoning(effort);
                                    app.reasoning = effort;
                                    if let Err(e) = bob_core::core::config::set_default_reasoning(
                                        effort.label(),
                                    ) {
                                        app.notify(format!(
                                            "reasoning set, but couldn't save default: {e}"
                                        ));
                                    }
                                    app.view.push_event(format!(
                                        "Model changed to {} {}",
                                        app.model_label,
                                        effort.label()
                                    ));
                                    app.stick_to_bottom();
                                } else {
                                    app.notify("busy — try again after this turn");
                                }
                            }
                            KeyOutcome::ListModels => {
                                let prov = match agent.try_lock() {
                                    Ok(a) => a.provider(),
                                    Err(_) => {
                                        app.notify("busy — try again after this turn");
                                        continue 'outer;
                                    }
                                };
                                app.notify("fetching models…");
                                spawn_bg(&app.bg_tx, async move {
                                    let res = prov.list_models_detailed().await.map(|entries| {
                                        entries
                                            .into_iter()
                                            .map(|e| match e.context_window {
                                                Some(w) => {
                                                    format!("{}\t{}", e.id, fmt_ctx_window(w))
                                                }
                                                None => e.id,
                                            })
                                            .collect::<Vec<_>>()
                                    });
                                    BgOutcome::ModelList(res.map_err(|e| e.to_string()))
                                });
                            }
                            KeyOutcome::ShowContext => {
                                if let Ok(a) = agent.try_lock() {
                                    let (used, window) = a.context_usage();
                                    let msgs = a.working_messages().len();
                                    drop(a);
                                    app.show_context(used, msgs, window);
                                } else {
                                    app.notify("busy — try again after this turn");
                                }
                            }
                            KeyOutcome::Compact => {
                                if app.runtime_failed {
                                    app.notify("Root runtime failed; exit and restart Bob before continuing");
                                    continue 'outer;
                                }
                                if app.running {
                                    app.view.push_notice(
                                        "finish the current turn first".into(),
                                    );
                                    app.stick_to_bottom();
                                } else {
                                    // The persisted event renders success; the outcome ends the operation.
                                    let cell = app.view.begin_compaction();
                                    app.running = true;
                                    app.turn_started = Some(std::time::Instant::now());
                                    app.stick_to_bottom();
                                    let agent = agent.clone();
                                    root_turn = Some(spawn_bg(&app.bg_tx, async move {
                                        match bob_core::core::panic::catch(async {
                                            let mut a = agent.lock().await;
                                            a.compact().await.map_err(|error| format!("{error:#}"))
                                        }).await {
                                            Ok(result) => BgOutcome::Compacted { cell, result },
                                            Err(_) => BgOutcome::CompactionPanicked { cell },
                                        }
                                    }));
                                }
                            }
                            KeyOutcome::ClearContext | KeyOutcome::NewSession => unreachable!("session actions handled before dispatch"),
                            KeyOutcome::None => {}
                        }
                    }
                    Some(Err(_)) | None => break 'outer Ok(()),
                }
            }
            Some(evt) = evt_rx.recv() => {
                dirty = true;
                app.apply_session_event(&mut session, &evt);
                // Streaming must not pull a scrolled-up reader back to the bottom.
                if app.scrollback.at_bottom() {
                    app.stick_to_bottom();
                }
            }
            Some(prompt) = perm_rx.recv() => {
                dirty = true;
                // Keep queued response channels alive; replacing one silently denies its agent.
                app.perm_queue.push_back(PendingPerm {
                    title: prompt.title,
                    detail: prompt.detail,
                    preview: prompt.preview,
                    options: prompt.options,
                    list: widgets::SelectList::new(),
                    resp: prompt.resp,
                });
            }
            Some(q) = query_rx.recv() => {
                dirty = true;
                // Long plans belong in scrollback so the fixed approval modal stays visible.
                let mut query = q.query;
                if query.title == "Ready to code?" && !query.detail.is_empty() {
                    app.view.push_plan(std::mem::take(&mut query.detail));
                    if app.scrollback.at_bottom() {
                        app.stick_to_bottom();
                    }
                }
                app.enqueue_query(PendingQuery {
                    query,
                    list: widgets::SelectList::new(),
                    other_text: None,
                    inspection: Default::default(),
                    purpose: QueryPurpose::Tool(q.resp),
                });
            }
            Some(completion) = app.turn_done_rx.recv() => {
                dirty = true;
                app.view.busy = false;
                app.runtime_failed |= completion.panicked;
                if completion.stopped {
                    app.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    app.queue.clear();
                    app.agent_team.stop_descendants("root");
                }
                if let Some(message) = completion.error {
                    app.bus.emit(AgentEvent::Error { agent_id: "root".into(), message });
                    app.stick_to_bottom();
                }
                while let Ok(event) = evt_rx.try_recv() {
                    app.apply_session_event(&mut session, &event);
                }
                let a = agent.lock().await;
                if !app.runtime_failed {
                    app.context_usage = Some(a.context_usage());
                }
                session.messages = a.messages().to_vec();
                session.grants = permissions.export_grants();
                if let Some(todos) = &app.todos {
                    session.todos = todos.items();
                }
                session.agent_threads = app.teams.to_persisted();
                session.workflows = app.view.to_persisted_workflows();
                session.updated_at = now_stamp();
                // SQLite contention must not block the event loop.
                {
                    let snapshot = session.clone();
                    let tx = app.bg_tx.clone();
                    let owner = owner.clone();
                    let token = owner.token();
                    tokio::spawn(async move {
                        let res = tokio::task::spawn_blocking(move || {
                            owner.checkpoint(token, snapshot).map(|_| ())
                        }).await;
                        let err = match res {
                            Ok(Ok(())) => None,
                            Ok(Err(e)) => Some(e.to_string()),
                            Err(e) => Some(e.to_string()),
                        };
                        if let Some(e) = err {
                            let _ = tx.send(BgOutcome::SessionSaveFailed(e));
                        }
                    });
                }
                drop(a);
                if let Some(job_id) = app.detached_job.take() {
                    let status = if completion.stopped { bob_core::tools::jobs::JobStatus::Failed } else { bob_core::tools::jobs::JobStatus::Done };
                    app.jobs.finish(&job_id, status, "turn finished".to_string());
                }
                app.current_prompt = None;
                match app.queue.pop_front() {
                    Some(next) => {
                        // UserPrompt renders the queued turn; a direct push would duplicate it.
                        app.stick_to_bottom();
                        app.current_prompt = Some(next.clone());
                        app.turn_started = Some(std::time::Instant::now());
                        root_turn = Some(spawn_turn(&agent, &app.turn_done_tx, next));
                    }
                    None => {
                        app.running = false;
                        app.turn_started = None;
                    }
                }
            }
            Some(outcome) = app.bg_rx.recv() => {
                dirty = true;
                app.apply_bg(outcome, &agent);
            }
            _ = ticker.tick() => {
                dirty |= app.refresh_job_panel();
                if app.focused_activity().0 {
                    app.spinner = app.spinner.wrapping_add(1);
                    dirty = true;
                }
                // Resume idle work for completion notices, but never revive a cancelled turn.
                if !app.runtime_failed && !app.running && !app.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    let finished = app.jobs.take_finished();
                    if !finished.is_empty() {
                        let list = finished
                            .iter()
                            .map(|(id, desc)| format!("{id} ({desc})"))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let prompt = format!(
                            "Background job(s) finished: {list}. Use job_output to read the \
                             result(s) and continue the task accordingly."
                        );
                        app.view.push_event(format!("Resumed — {list} finished"));
                        app.running = true;
                        app.turn_started = Some(std::time::Instant::now());
                        root_turn = Some(spawn_turn(&agent, &app.turn_done_tx, prompt));
                        dirty = true;
                    } else {
                        let pending = {
                            // Cancelled children may still report back. Only a user prompt clears cancellation.
                            if app.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                                false
                            } else {
                                match agent.try_lock() {
                                    Ok(mut a) => a.has_pending_coordination(),
                                    Err(_) => false,
                                }
                            }
                        };
                        if pending {
                            app.running = true;
                            app.turn_started = Some(std::time::Instant::now());
                            root_turn = Some(spawn_turn(&agent, &app.turn_done_tx, String::new()));
                            dirty = true;
                        }
                    }
                }
            }
        }
    };

    // Save partial turns as well as completed ones on exit.
    {
        // Cancel before acquiring the turn's mutex; bounded waits keep wedged tools from blocking exit.
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        app.agent_team.stop_descendants("root");
        app.perm_queue.clear();
        app.pending_query = None;
        app.query_queue.clear();
        if !app.jobs.shutdown().await {
            eprintln!("bob: warning: background jobs exceeded shutdown deadline");
        }
        if let Some(mut turn) = root_turn.take() {
            if tokio::time::timeout(Duration::from_secs(2), &mut turn)
                .await
                .is_err()
            {
                turn.abort();
                let _ = turn.await;
            }
        }
        if tokio::time::timeout(Duration::from_secs(2), async {
            while app.agent_team.has_unsettled_descendants("root") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_err()
        {
            eprintln!("bob: warning: agents exceeded shutdown deadline");
        }
        let locked = tokio::time::timeout(std::time::Duration::from_secs(2), agent.lock()).await;
        match locked {
            Ok(a) => {
                session.messages = a.messages().to_vec();
                drop(a);
            }
            Err(_) => {
                // Fall back to the last snapshot; the event log retains the full stream.
                eprintln!(
                    "bob: warning: turn didn't stop in time on quit; \
                     saved history may be one turn stale"
                );
            }
        }
        while let Ok(event) = evt_rx.try_recv() {
            app.apply_session_event(&mut session, &event);
        }
        app.snapshot_session(&mut session);
        if let Err(e) = owner.checkpoint(owner.token(), session.clone()) {
            eprintln!("bob: warning: couldn't save session on exit: {e}");
        }
        if let Err(error) = owner.flush() {
            eprintln!("bob: warning: couldn't flush session events on exit: {error}");
        }
    }

    result.map(|()| session.id)
}

/// DEC mode 2026 batches frame output; unsupported terminals ignore it.
fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> std::io::Result<()> {
    let _ = execute!(std::io::stdout(), BeginSynchronizedUpdate);
    let res = terminal.draw(|f| app.draw(f));
    let _ = execute!(std::io::stdout(), EndSynchronizedUpdate);
    res.map(|_| ())
}

enum KeyOutcome {
    None,
    Submit(String),
    /// Deliver to the running root's inbox instead of queueing another turn.
    Steer(String),
    SwitchModel(String),
    ListReasoning,
    SetReasoning(String),
    ListModels,
    ShowContext,
    ClearContext,
    NewSession,
    Compact,
    Quit,
}

struct App {
    view: ViewModel,
    input: input::Input,
    spinner: usize,
    /// FIFO; waiting response channels stay alive until their prompt is answered.
    perm_queue: std::collections::VecDeque<PendingPerm>,
    menu: Vec<(String, String)>,
    menu_sel: usize,
    custom_commands: Vec<bob_core::core::commands::CustomCommand>,
    all_files: Vec<String>,
    /// True while a background `@file` gather is in flight (so we kick it off once).
    files_loading: bool,
    file_menu: Vec<String>,
    file_sel: usize,
    /// Char index of the active `@`, not a byte offset.
    file_at: Option<usize>,
    /// Prompts submitted while a turn is running, sent in order once it frees.
    queue: std::collections::VecDeque<String>,
    running: bool,
    runtime_failed: bool,
    compaction_completion: Option<usize>,
    /// If the running turn has been detached (Ctrl+B), the job id tracking it.
    detached_job: Option<String>,
    /// The prompt of the running turn (used as the job label on detach).
    current_prompt: Option<String>,
    session_usage: bob_core::core::types::Usage,
    global_usage: bob_core::core::types::Usage,
    jobs: bob_core::tools::jobs::JobRegistry,
    running_shell_jobs: Vec<String>,
    permissions: Arc<PermissionEngine>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    pending_query: Option<PendingQuery>,
    query_queue: std::collections::VecDeque<PendingQuery>,
    input_scroll: usize,
    context_usage: Option<(usize, usize)>,
    model_label: String,
    reasoning: bob_core::core::types::ReasoningEffort,
    /// Config provider id; Copilot and OpenAI both report "openai" through the trait.
    provider_id: String,
    turn_done_tx: mpsc::UnboundedSender<TurnCompletion>,
    turn_done_rx: mpsc::UnboundedReceiver<TurnCompletion>,
    bg_tx: mpsc::UnboundedSender<BgOutcome>,
    bg_rx: mpsc::UnboundedReceiver<BgOutcome>,
    turn_started: Option<std::time::Instant>,
    cwd_label: String,
    branch: Option<String>,
    lsp: Option<Arc<bob_core::lsp::LspManager>>,
    todos: Option<Arc<bob_core::tools::todo::TodoStore>>,
    theme_name: String,
    scrollback: scrollback::ScrollbackRenderer,
    /// Independent from root scrollback; reset when the focused agent changes.
    focused_scrollback: scrollback::ScrollbackRenderer,
    teams: team::AgentTranscripts,
    workflow_view: Option<team::WorkflowView>,
    /// Screen row → agent id hit map, rebuilt each draw.
    wf_view_agents: Option<Vec<(u16, String)>>,
    show_todos: bool,
    agent_team: bob_core::agent::team::AgentRegistry,
    agent_control_notice: Option<(String, String)>,
    bus: EventBus,
    system_prompt: String,
    sidebar_open: bool,
    /// Index 0 is main; subsequent entries follow `display_order`.
    sidebar: widgets::SelectList,
    /// Both the displayed transcript and input recipient; None means root.
    focused_agent: Option<String>,
    /// Screen row → agent id hit map; an empty id means main.
    sidebar_rows: Option<Vec<(u16, String)>>,
    sidebar_rect: Option<Rect>,
}

const INIT_PROMPT: &str = "Create an AGENTS.md file in the project root that will help future \
    coding assistants work effectively here. Explore the codebase first (build files, README, \
    directory layout, existing conventions), then write a concise AGENTS.md covering: what the \
    project is, how to build/test/lint/run it (the exact commands), the high-level architecture \
    and where key things live, and any important conventions or gotchas. Keep it tight and \
    factual — no fluff. If an AGENTS.md already exists, improve it rather than overwriting good \
    content.";

const COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a fresh session (new conversation)"),
    ("/clear", "clear the conversation context (agent forgets)"),
    ("/copy", "copy the last message to the clipboard"),
    ("/usage", "show token usage (session + all-time)"),
    ("/context", "show context-window usage"),
    ("/compact", "summarize & free up context now"),
    (
        "/yolo",
        "bypass permission prompts (denials and plan mode still apply)",
    ),
    ("/model", "show the current model"),
    ("/models", "list & switch models"),
    (
        "/reasoning",
        "set reasoning effort (off/low/medium/high/max)",
    ),
    ("/theme", "switch color theme (dark/light/terminal)"),
    ("/jobs", "list background jobs"),
    ("/memory", "show saved memories (project + global)"),
    ("/init", "generate an AGENTS.md describing this project"),
    ("/mcp", "list configured MCP servers"),
    ("/lsp", "show language servers & their health"),
    ("/exit", "quit bob"),
];

impl App {
    fn new(
        jobs: bob_core::tools::jobs::JobRegistry,
        permissions: Arc<PermissionEngine>,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        agent_team: bob_core::agent::team::AgentRegistry,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        App {
            view: ViewModel::new(),
            input: input::Input::new(),
            spinner: 0,
            perm_queue: std::collections::VecDeque::new(),
            menu: Vec::new(),
            custom_commands: Vec::new(),
            menu_sel: 0,
            all_files: Vec::new(),
            files_loading: false,
            file_menu: Vec::new(),
            file_sel: 0,
            file_at: None,
            queue: std::collections::VecDeque::new(),
            running: false,
            runtime_failed: false,
            compaction_completion: None,
            detached_job: None,
            current_prompt: None,
            session_usage: bob_core::core::types::Usage::default(),
            global_usage: bob_core::core::types::Usage::default(),
            jobs,
            running_shell_jobs: Vec::new(),
            permissions,
            cancel,
            pending_query: None,
            query_queue: std::collections::VecDeque::new(),
            input_scroll: 0,
            context_usage: None,
            model_label: String::new(),
            reasoning: bob_core::core::types::ReasoningEffort::default(),
            provider_id: String::new(),
            turn_done_tx: tx,
            turn_done_rx: rx,
            bg_tx,
            bg_rx,
            turn_started: None,
            cwd_label: String::new(),
            branch: None,
            lsp: None,
            todos: None,
            theme_name: "dark".to_string(),
            scrollback: scrollback::ScrollbackRenderer::new(),
            focused_scrollback: scrollback::ScrollbackRenderer::new(),
            teams: team::AgentTranscripts::new(),
            workflow_view: None,
            wf_view_agents: None,
            show_todos: true,
            agent_team,
            agent_control_notice: None,
            bus: EventBus::new(),
            system_prompt: String::new(),
            sidebar_open: false,
            sidebar: widgets::SelectList::new(),
            focused_agent: None,
            sidebar_rows: None,
            sidebar_rect: None,
        }
    }

    fn stop_target(&self) -> Option<String> {
        if self.workflow_view.is_some() {
            return None;
        }
        let id = if self.pending_query.as_ref().is_none_or(|query| {
            query.other_text.is_none() && matches!(query.purpose, QueryPurpose::Tool(_))
        }) {
            self.focused_agent.clone()
        } else {
            None
        };
        id.filter(|id| !id.is_empty() && id != "root")
    }

    fn control_notice(&self, id: &str) -> Option<&str> {
        self.agent_control_notice
            .as_ref()
            .filter(|(target, _)| target == id)
            .map(|(_, message)| message.as_str())
    }

    fn stick_to_bottom(&mut self) {
        self.scrollback.stick_to_bottom();
    }

    fn notify(&mut self, text: impl Into<String>) {
        self.view.push_notice(text.into());
        self.stick_to_bottom();
    }

    fn refresh_menu(&mut self) {
        let text = self.input.text();
        if text.starts_with('/') && !text.contains(' ') {
            let mut all: Vec<(String, String)> = COMMANDS
                .iter()
                .map(|(c, d)| (c.to_string(), d.to_string()))
                .collect();
            for cc in &self.custom_commands {
                all.push((format!("/{}", cc.name), cc.description.clone()));
            }
            self.menu = all
                .into_iter()
                .filter(|(c, _)| c.starts_with(text))
                .collect();
            self.menu_sel = self.menu_sel.min(self.menu.len().saturating_sub(1));
        } else {
            self.menu.clear();
            self.menu_sel = 0;
        }
        self.refresh_file_menu();
    }

    fn apply_bg(&mut self, outcome: BgOutcome, agent: &Arc<Mutex<Agent>>) {
        match outcome {
            BgOutcome::ProviderSwitched(Ok((id, provider))) => {
                let model = provider.model().to_string();
                if let Ok(mut a) = agent.try_lock() {
                    a.set_provider(provider);
                    self.context_usage = Some(a.context_usage());
                } else {
                    self.notify("busy — try switching again");
                    return;
                }
                self.provider_id = id;
                self.model_label = model;
                if let Err(e) =
                    bob_core::core::config::set_default_model(&self.provider_id, &self.model_label)
                {
                    self.notify(format!("switched, but couldn't save default: {e}"));
                }
                let cur = self.reasoning;
                self.open_reasoning_picker(cur);
                self.stick_to_bottom();
            }
            BgOutcome::ProviderSwitched(Err(e)) => {
                self.view.push_notice(format!("error: {}", e));
                self.stick_to_bottom();
            }
            BgOutcome::ModelList(Ok(models)) => {
                let options: Vec<String> = if models.is_empty() {
                    if self.provider_id == "openai" {
                        bob_core::providers::RESPONSES_MODELS
                            .iter()
                            .map(|s| s.to_string())
                            .collect()
                    } else {
                        vec![]
                    }
                } else {
                    models
                };
                if options.is_empty() {
                    self.view
                        .push_notice("provider returned no models; use `/models <spec>`".into());
                    self.stick_to_bottom();
                } else {
                    self.open_model_picker(options);
                }
            }
            BgOutcome::ModelList(Err(e)) => {
                self.view
                    .push_notice(format!("error listing models: {}", e));
                self.stick_to_bottom();
            }
            BgOutcome::FileList(files) => {
                self.all_files = files;
                self.files_loading = false;
                // The query may have changed while candidates were loading.
                self.refresh_file_menu();
            }
            BgOutcome::Compacted { cell, result } => self.finish_compaction(cell, result),
            BgOutcome::CompactionPanicked { cell } => {
                self.view.finish_compaction(
                    cell,
                    Err("compaction panicked; history may be incomplete".into()),
                );
                self.compaction_completion = None;
                let _ = self.turn_done_tx.send(TurnCompletion::panicked());
            }
            BgOutcome::SessionSaveFailed(e) => {
                self.view
                    .push_notice(format!("warning: couldn't save session: {e}"));
            }
        }
    }

    fn finish_compaction(&mut self, cell: usize, result: Result<(bool, usize, usize), String>) {
        self.view.finish_compaction(cell, result);
        self.stick_to_bottom();
        self.compaction_completion = Some(cell);
        self.settle_compaction();
    }

    fn settle_compaction(&mut self) {
        if let Some(cell) = self.compaction_completion {
            if !matches!(
                self.view.cells.get(cell),
                Some(Cell::Compaction { done: false, .. })
            ) {
                self.compaction_completion = None;
                let _ = self.turn_done_tx.send(TurnCompletion::default());
            }
        }
    }

    fn refresh_file_menu(&mut self) {
        match self.input.at_token() {
            Some((start, query)) => {
                self.file_at = Some(start);
                if self.all_files.is_empty() && !self.files_loading {
                    self.files_loading = true;
                    let bg_tx = self.bg_tx.clone();
                    spawn_bg(&bg_tx, async move {
                        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                        BgOutcome::FileList(files::gather_files(&cwd).await)
                    });
                }
                self.file_menu = files::fuzzy_rank(&self.all_files, &query, 8)
                    .into_iter()
                    .map(|s| s.to_string())
                    .collect();
                self.file_sel = self.file_sel.min(self.file_menu.len().saturating_sub(1));
            }
            None => {
                self.file_menu.clear();
                self.file_sel = 0;
                self.file_at = None;
            }
        }
    }

    fn accept_file(&mut self) {
        if let (Some(start), Some(path)) =
            (self.file_at, self.file_menu.get(self.file_sel).cloned())
        {
            self.input.replace_at_token(start, &path);
            self.file_menu.clear();
            self.file_at = None;
            self.file_sel = 0;
        }
    }

    /// Handle emacs/readline editing keys. Returns true if consumed.
    fn handle_emacs(&mut self, code: KeyCode, mods: KeyModifiers) -> bool {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);

        if ctrl {
            match code {
                KeyCode::Char('a') => self.input.move_home(),
                KeyCode::Char('e') => self.input.move_end(),
                KeyCode::Char('b') => self.input.move_left(),
                KeyCode::Char('f') => self.input.move_right(),
                KeyCode::Char('p') => self.input.history_prev(),
                KeyCode::Char('n') => self.input.history_next(),
                KeyCode::Char('d') => self.input.delete_right(),
                KeyCode::Char('h') => self.input.delete_left(),
                KeyCode::Char('k') => self.input.kill_to_end(),
                KeyCode::Char('u') => self.input.kill_to_start(),
                KeyCode::Char('w') => self.input.kill_word_left(),
                KeyCode::Char('t') => self.input.transpose(),
                // Ctrl+J: universal newline (Shift+Enter isn't distinguishable
                // in many terminals). Some terminals send Enter as Ctrl+J too.
                KeyCode::Char('j') => self.input.insert_newline(),
                KeyCode::Left => self.input.move_word_left(),
                KeyCode::Right => self.input.move_word_right(),
                _ => return false,
            }
            return true;
        }

        if alt {
            match code {
                KeyCode::Char('b') => self.input.move_word_left(),
                KeyCode::Char('f') => self.input.move_word_right(),
                KeyCode::Char('d') => self.input.kill_word_right(),
                KeyCode::Backspace => self.input.kill_word_left(),
                _ => return false,
            }
            return true;
        }

        false
    }

    fn handle_query_key(&mut self, code: KeyCode) -> KeyOutcome {
        let Some(q) = &mut self.pending_query else {
            return KeyOutcome::None;
        };
        let n = q.query.options.len() + if q.query.allow_other { 1 } else { 0 };
        if matches!(code, KeyCode::PageUp | KeyCode::PageDown) {
            q.inspection.page(code == KeyCode::PageDown);
            return KeyOutcome::None;
        }

        if let Some(buf) = &mut q.other_text {
            match code {
                KeyCode::Enter => {
                    let answer = buf.clone();
                    return self.answer_query(answer);
                }
                KeyCode::Esc => {
                    q.other_text = None;
                    q.inspection.offset = None;
                }
                KeyCode::Backspace => {
                    buf.pop();
                    q.inspection.offset = None;
                }
                KeyCode::Char(c) => {
                    buf.push(c);
                    q.inspection.offset = None;
                }
                _ => {}
            }
            return KeyOutcome::None;
        }

        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                q.list.up();
                q.inspection.offset = None;
                self.preview_theme_if_picking();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                q.list.down(n);
                q.inspection.offset = None;
                self.preview_theme_if_picking();
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < n {
                    return self.pick_query_option(idx);
                }
            }
            KeyCode::Enter => {
                let idx = q.list.selected;
                return self.pick_query_option(idx);
            }
            KeyCode::Esc => {
                if let Some(q) = self.pending_query.take() {
                    match q.purpose {
                        QueryPurpose::Tool(resp) => {
                            let _ = resp.send(None);
                        }
                        QueryPurpose::ThemePicker { original } => {
                            theme::set_theme(theme::Theme::by_name(&original));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        KeyOutcome::None
    }

    fn pick_query_option(&mut self, idx: usize) -> KeyOutcome {
        let Some(q) = &mut self.pending_query else {
            return KeyOutcome::None;
        };
        if idx < q.query.options.len() {
            let answer = q.query.options[idx].clone();
            self.answer_query(answer)
        } else {
            q.other_text = Some(String::new());
            q.inspection.offset = None;
            KeyOutcome::None
        }
    }

    fn answer_query(&mut self, answer: String) -> KeyOutcome {
        let Some(q) = self.pending_query.take() else {
            return KeyOutcome::None;
        };
        match q.purpose {
            QueryPurpose::Tool(resp) => {
                let is_plan = q.query.title == "Ready to code?";
                let approved = answer.starts_with("Yes");
                if is_plan && approved && self.permissions.mode() == Mode::Plan {
                    self.permissions.set_mode(Mode::Normal);
                    self.notify("plan approved · mode: normal");
                }
                let _ = resp.send(Some(answer));
                KeyOutcome::None
            }
            QueryPurpose::ModelPicker => {
                // The picker label is "id\t(N ctx)"; the spec is just the id.
                let id = answer.split('\t').next().unwrap_or(&answer).to_string();
                KeyOutcome::SwitchModel(id)
            }
            QueryPurpose::ReasoningPicker => KeyOutcome::SetReasoning(answer),
            QueryPurpose::ThemePicker { .. } => {
                self.theme_name = answer.clone();
                theme::set_theme(theme::Theme::by_name(&answer));
                if let Ok(cwd) = std::env::current_dir() {
                    let _ = bob_core::core::config::set_theme_in_project(&cwd, &answer);
                }
                KeyOutcome::None
            }
        }
    }

    fn preview_theme_if_picking(&mut self) {
        let Some(q) = &self.pending_query else { return };
        if !matches!(q.purpose, QueryPurpose::ThemePicker { .. }) {
            return;
        }
        if let Some(name) = q.query.options.get(q.list.selected) {
            theme::set_theme(theme::Theme::by_name(name));
        }
    }

    fn open_theme_picker(&mut self, current: &str) {
        let options: Vec<String> = theme::Theme::NAMES.iter().map(|s| s.to_string()).collect();
        let selected = options.iter().position(|n| n == current).unwrap_or(0);
        self.enqueue_query(PendingQuery {
            query: bob_core::tools::registry::UserQuery {
                title: "Theme (↑↓ to preview, Enter to keep)".to_string(),
                detail: String::new(),
                options,
                allow_other: false,
            },
            list: widgets::SelectList {
                selected,
                scroll: 0,
            },
            other_text: None,
            inspection: Default::default(),
            purpose: QueryPurpose::ThemePicker {
                original: current.to_string(),
            },
        });
    }

    fn open_reasoning_picker(&mut self, current: bob_core::core::types::ReasoningEffort) {
        let options: Vec<String> = ["off", "low", "medium", "high", "max"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let selected = options
            .iter()
            .position(|label| label == current.label())
            .unwrap_or(0);
        self.enqueue_query(PendingQuery {
            query: bob_core::tools::registry::UserQuery {
                title: format!("Reasoning effort (current: {})", current.label()),
                detail: String::new(),
                options,
                allow_other: false,
            },
            list: widgets::SelectList {
                selected,
                scroll: 0,
            },
            other_text: None,
            inspection: Default::default(),
            purpose: QueryPurpose::ReasoningPicker,
        });
    }

    fn click_scrollback(&mut self, _col: u16, row: u16) -> bool {
        if self.active_scrollback().click_exploration(row) {
            return true;
        }
        let Some((cell_idx, offset)) = self.active_scrollback().hit_test_offset(row) else {
            return false;
        };
        if let Some(id) = self.focused_agent.as_deref() {
            return self.teams.toggle_tool(id, cell_idx);
        }
        if let Some(cell) = self.view.cells.get(cell_idx) {
            if let view::Cell::Workflow { id, .. } = cell {
                if let Some(agent_id) = cell.workflow_agent_at(offset) {
                    let id = agent_id.to_string();
                    return self.open_sidebar_on(&id);
                }
                let run_id = id.clone();
                self.open_workflow_view(run_id);
                return true;
            }
        }
        self.view.toggle_tool_expanded(cell_idx)
    }

    fn open_workflow_view(&mut self, run_id: String) {
        self.workflow_view = Some(team::WorkflowView {
            run_id,
            ..Default::default()
        });
    }

    fn open_sidebar_on(&mut self, agent_id: &str) -> bool {
        let order = self.teams.display_order();
        let Some(idx) = order.iter().position(|id| id == agent_id) else {
            return false;
        };
        self.workflow_view = None;
        self.wf_view_agents = None;
        self.sidebar_open = true;
        self.sidebar.selected = idx + 1;
        self.set_focused_agent(Some(agent_id.to_string()));
        true
    }

    fn handle_workflow_view_key(&mut self, code: KeyCode) -> KeyOutcome {
        use draw::{workflow_rows, WfRow};
        // Rebuild the current row list so the cursor clamps to what's visible.
        let rows = self
            .workflow_view
            .as_ref()
            .and_then(|vw| {
                self.view
                    .workflow_by_id(&vw.run_id)
                    .map(|(_, phases, _)| workflow_rows(phases, &vw.collapsed))
                    .map(|r| (vw, r))
            })
            .map(|(_, r)| r);
        let Some(rows) = rows else {
            return KeyOutcome::None;
        };
        let n = rows.len();
        let Some(vw) = self.workflow_view.as_mut() else {
            return KeyOutcome::None;
        };
        vw.list.clamp(n);
        match code {
            KeyCode::Esc => {
                self.workflow_view = None;
                self.wf_view_agents = None;
            }
            KeyCode::Up | KeyCode::Char('k') => vw.list.up(),
            KeyCode::Down | KeyCode::Char('j') => vw.list.down(n),
            KeyCode::PageUp => vw.list.page_up(10),
            KeyCode::PageDown => vw.list.page_down(10, n),
            KeyCode::Enter | KeyCode::Char(' ') => match rows.get(vw.list.selected) {
                Some(WfRow::Phase(pi)) => {
                    if vw.collapsed.contains(pi) {
                        vw.collapsed.remove(pi);
                    } else {
                        vw.collapsed.insert(*pi);
                    }
                }
                Some(WfRow::Agent(pi, ai)) => {
                    let agent_id = self
                        .view
                        .workflow_by_id(&vw.run_id)
                        .map(|(_, phases, _)| phases[*pi].agents[*ai].agent_id.clone());
                    if let Some(id) = agent_id {
                        self.open_sidebar_on(&id);
                    }
                }
                None => {}
            },
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(WfRow::Phase(pi)) = rows.get(vw.list.selected) {
                    vw.collapsed.remove(pi);
                }
            }
            KeyCode::Left | KeyCode::Char('h') => match rows.get(vw.list.selected) {
                Some(WfRow::Phase(pi)) | Some(WfRow::Agent(pi, _)) => {
                    vw.collapsed.insert(*pi);
                }
                None => {}
            },
            _ => {}
        }
        KeyOutcome::None
    }

    fn click_workflow_view(&mut self, _col: u16, row: u16) {
        let Some(agent_id) = self
            .wf_view_agents
            .as_ref()
            .and_then(|rows| rows.iter().find(|(r, _)| *r == row).map(|(_, s)| s.clone()))
        else {
            return;
        };
        self.open_sidebar_on(&agent_id);
    }

    fn apply_agent_event(&mut self, event: &AgentEvent) {
        if let AgentEvent::ContextWarning {
            agent_id,
            used_tokens,
            context_window,
            ..
        } = event
        {
            if agent_id == "root" {
                self.context_usage = Some((*used_tokens, *context_window));
            }
        }
        self.teams.apply(event, self.focused_agent.as_deref());
        let order = self.teams.display_order();
        self.sync_sidebar_selection(&order);
        self.view.apply(event);
        self.settle_compaction();
    }

    fn sync_sidebar_selection(&mut self, order: &[String]) {
        self.sidebar.selected = self
            .focused_agent
            .as_ref()
            .and_then(|id| order.iter().position(|r| r == id))
            .map(|idx| idx + 1)
            .unwrap_or(0);
    }

    fn apply_sidebar_focus(&mut self) {
        if self.sidebar.selected == 0 {
            self.set_focused_agent(None);
            return;
        }
        let order = self.teams.display_order();
        let id = order.get(self.sidebar.selected - 1).cloned();
        self.set_focused_agent(id);
    }

    fn active_scrollback(&mut self) -> &mut scrollback::ScrollbackRenderer {
        if self.focused_agent.is_some() {
            &mut self.focused_scrollback
        } else {
            &mut self.scrollback
        }
    }

    fn set_focused_agent(&mut self, id: Option<String>) {
        if self.focused_agent != id {
            self.focused_scrollback = scrollback::ScrollbackRenderer::new();
        }
        if let Some(id) = id.as_deref() {
            self.teams.mark_read(id);
        }
        self.focused_agent = id;
    }

    fn send_agent_message(&mut self, id: &str, text: &str) {
        if self.agent_team.send(id, "user", text) {
            self.bus.emit(AgentEvent::UserMessageQueued {
                to: id.into(),
                text: text.into(),
            });
            self.agent_control_notice = None;
            self.focused_scrollback.stick_to_bottom();
        } else {
            let message = format!("{id} is not reachable");
            self.notify(message.clone());
            self.agent_control_notice = Some((id.to_string(), message));
        }
    }

    fn sidebar_contains(&self, col: u16, row: u16) -> bool {
        self.sidebar_rect
            .is_some_and(|r| col >= r.x && col < r.right() && row >= r.y && row < r.bottom())
    }

    fn click_sidebar(&mut self, col: u16, row: u16) {
        if !self.sidebar_contains(col, row) {
            return;
        }
        let Some(id) = self
            .sidebar_rows
            .as_ref()
            .and_then(|rows| rows.iter().find(|(r, _)| *r == row).map(|(_, s)| s.clone()))
        else {
            return;
        };
        if id.is_empty() {
            self.sidebar.selected = 0;
            self.set_focused_agent(None);
        } else if let Some(idx) = self.teams.display_order().iter().position(|r| *r == id) {
            self.sidebar.selected = idx + 1;
            self.set_focused_agent(Some(id));
        }
    }

    fn on_terminal_event(&mut self, event: CtEvent, dirty: &mut bool) -> KeyOutcome {
        match event {
            CtEvent::Key(key) if key.kind != KeyEventKind::Release => {
                *dirty = true;
                return self.on_key(key.code, key.modifiers);
            }
            CtEvent::Paste(text) => {
                self.paste_active(&text);
                *dirty = true;
            }
            CtEvent::Resize(_, _) => {
                // Ratatui resizes on the next draw; discard stale click coordinates now.
                self.sidebar_rect = None;
                self.sidebar_rows = None;
                self.wf_view_agents = None;
                *dirty = true;
            }
            CtEvent::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => {
                    self.active_scrollback().scroll_up(1);
                    *dirty = true;
                }
                MouseEventKind::ScrollDown => {
                    self.active_scrollback().scroll_down(1);
                    *dirty = true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if self.workflow_view.is_some() {
                        self.click_workflow_view(m.column, m.row);
                    } else if self.sidebar_open && self.sidebar_contains(m.column, m.row) {
                        self.click_sidebar(m.column, m.row);
                    } else if !self.click_scrollback(m.column, m.row) {
                        self.active_scrollback().stick_to_bottom();
                    }
                    *dirty = true;
                }
                _ => {}
            },
            _ => {}
        }
        KeyOutcome::None
    }

    fn refresh_job_panel(&mut self) -> bool {
        let running: Vec<_> = draw::shell_jobs(&self.jobs)
            .into_iter()
            .map(|(id, _, _, _)| id)
            .collect();
        if running == self.running_shell_jobs {
            return false;
        }
        self.running_shell_jobs = running;
        true
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyOutcome {
        self.clear_closed_prompts();
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return KeyOutcome::Quit;
        }
        if code == KeyCode::Esc
            && self.active_prompt() == prompts::ActivePrompt::Composer
            && (!self.menu.is_empty() || !self.file_menu.is_empty())
        {
            self.menu.clear();
            self.file_menu.clear();
            self.file_at = None;
            return KeyOutcome::None;
        }
        if code == KeyCode::Char('x') && mods.contains(KeyModifiers::CONTROL) {
            if let Some(id) = self.stop_target() {
                let message = match self.agent_team.stop("root", &id) {
                    Ok(count) => format!("cancellation requested ({count})"),
                    Err(error) => error,
                };
                self.notify(format!("{id}: {message}"));
                self.agent_control_notice = Some((id, message));
            }
            return KeyOutcome::None;
        }
        if code == KeyCode::BackTab {
            let next = self.permissions.mode().next();
            self.permissions.set_mode(next);
            return KeyOutcome::None;
        }

        if code == KeyCode::Char('t') && mods.contains(KeyModifiers::CONTROL) {
            self.sidebar_open = !self.sidebar_open;
            self.workflow_view = None;
            self.wf_view_agents = None;
            self.sidebar_rect = None;
            self.sidebar_rows = None;
            return KeyOutcome::None;
        }
        if code == KeyCode::Char('g') && mods.contains(KeyModifiers::CONTROL) {
            return KeyOutcome::None;
        }
        // Sidebar navigation must not consume composer or modal input.
        if self.sidebar_open
            && self.workflow_view.is_none()
            && self.pending_query.is_none()
            && self.perm_queue.is_empty()
        {
            self.sync_sidebar_selection(&self.teams.display_order());
            match code {
                KeyCode::Up if self.input.text().is_empty() => {
                    self.sidebar.up();
                    self.apply_sidebar_focus();
                    return KeyOutcome::None;
                }
                KeyCode::Down if self.input.text().is_empty() => {
                    let n = 1 + self.teams.display_order().len();
                    self.sidebar.down(n);
                    self.apply_sidebar_focus();
                    return KeyOutcome::None;
                }
                KeyCode::Esc if self.focused_agent.is_some() => {
                    self.set_focused_agent(None);
                    self.sidebar.selected = 0;
                    return KeyOutcome::None;
                }
                _ => {}
            }
        }
        if self.workflow_view.is_some() {
            return self.handle_workflow_view_key(code);
        }

        if self.active_prompt() == prompts::ActivePrompt::Query {
            let outcome = self.handle_query_key(code);
            self.advance_queries();
            return outcome;
        }

        if let Some(p) = self.perm_queue.front_mut() {
            let n = p.options.len();
            match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    p.list.up();
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    p.list.down(n);
                }
                KeyCode::Char(c @ '1'..='9') => {
                    let idx = (c as usize) - ('1' as usize);
                    if idx < n {
                        let p = self.perm_queue.pop_front().unwrap();
                        let _ = p.resp.send(Some(idx));
                    }
                }
                KeyCode::Enter => {
                    let sel = p.list.selected;
                    let p = self.perm_queue.pop_front().unwrap();
                    let _ = p.resp.send(Some(sel));
                }
                KeyCode::Esc => {
                    let _ = self.perm_queue.pop_front().unwrap().resp.send(None);
                }
                _ => {}
            }
            return KeyOutcome::None;
        }

        match (code, mods) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) => return KeyOutcome::Quit,
            (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
                self.copy_last();
                return KeyOutcome::None;
            }
            (KeyCode::Char('b'), KeyModifiers::CONTROL) => {
                self.detach_current_turn();
                return KeyOutcome::None;
            }
            (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                self.show_todos = !self.show_todos;
                return KeyOutcome::None;
            }
            (KeyCode::Esc, _) => {
                if self.focused_agent.is_some() {
                    self.set_focused_agent(None);
                    self.sidebar.selected = 0;
                } else if self.running {
                    self.interrupt();
                }
                return KeyOutcome::None;
            }
            (KeyCode::PageUp, _) => {
                self.active_scrollback().scroll_up(10);
                return KeyOutcome::None;
            }
            (KeyCode::PageDown, _) => {
                self.active_scrollback().scroll_down(10);
                return KeyOutcome::None;
            }
            _ => {}
        }

        if self.handle_emacs(code, mods) {
            self.refresh_menu();
            return KeyOutcome::None;
        }

        if !self.file_menu.is_empty() {
            match code {
                KeyCode::Up => {
                    if self.file_sel > 0 {
                        self.file_sel -= 1;
                    }
                    return KeyOutcome::None;
                }
                KeyCode::Down => {
                    if self.file_sel + 1 < self.file_menu.len() {
                        self.file_sel += 1;
                    }
                    return KeyOutcome::None;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.accept_file();
                    return KeyOutcome::None;
                }
                KeyCode::Esc => {
                    self.file_menu.clear();
                    self.file_at = None;
                    self.file_sel = 0;
                    return KeyOutcome::None;
                }
                _ => {}
            }
        }

        if !self.menu.is_empty() {
            match code {
                KeyCode::Up => {
                    if self.menu_sel > 0 {
                        self.menu_sel -= 1;
                    }
                    return KeyOutcome::None;
                }
                KeyCode::Down => {
                    if self.menu_sel + 1 < self.menu.len() {
                        self.menu_sel += 1;
                    }
                    return KeyOutcome::None;
                }
                KeyCode::Tab => {
                    let pick = self.menu[self.menu_sel].0.to_string();
                    self.input.set(&pick);
                    self.refresh_menu();
                    return KeyOutcome::None;
                }
                KeyCode::Enter if !mods.contains(KeyModifiers::SHIFT) => {
                    // Execute the highlighted completion, not the partially typed command.
                    let pick = self.menu[self.menu_sel].0.to_string();
                    self.input.set(&pick);
                    self.input.submit();
                    self.menu.clear();
                    if let Some(out) = self.handle_command(&pick) {
                        return out;
                    }
                    return KeyOutcome::None;
                }
                _ => {}
            }
        }

        match code {
            KeyCode::Enter => {
                if mods.contains(KeyModifiers::SHIFT) {
                    self.input.insert_newline();
                    return KeyOutcome::None;
                }
                let display = self.input.text().trim().to_string();
                if display.is_empty() {
                    return KeyOutcome::None;
                }
                let text = self.input.resolved_text().trim().to_string();
                self.input.submit();
                self.menu.clear();
                // Slash-commands are never steered/queued — run them inline.
                if let Some(out) = self.handle_command(&display) {
                    return out;
                }
                if mods.contains(KeyModifiers::ALT) && self.running && self.focused_agent.is_none()
                {
                    KeyOutcome::Steer(text)
                } else {
                    KeyOutcome::Submit(text)
                }
            }
            KeyCode::Up => {
                self.input.history_prev();
                KeyOutcome::None
            }
            KeyCode::Down => {
                self.input.history_next();
                KeyOutcome::None
            }
            KeyCode::Backspace if self.input.text().is_empty() && !self.queue.is_empty() => {
                if let Some(last) = self.queue.pop_back() {
                    self.input.set(&last);
                }
                KeyOutcome::None
            }
            _ => {
                self.input.on_key(code);
                self.refresh_menu();
                KeyOutcome::None
            }
        }
    }

    /// Handle in-session slash commands. Returns Some(outcome) if consumed.
    fn handle_command(&mut self, text: &str) -> Option<KeyOutcome> {
        let (cmd, arg) = match text.split_once(char::is_whitespace) {
            Some((c, a)) => (c, a.trim()),
            None => (text, ""),
        };
        match cmd {
            "/exit" => Some(KeyOutcome::Quit),
            "/clear" => Some(KeyOutcome::ClearContext),
            "/new" => Some(KeyOutcome::NewSession),
            "/yolo" => {
                let on = self.permissions.toggle_bypass();
                if on {
                    self.notify(
                        "YOLO on — permission prompts bypassed; explicit denials and plan mode still apply. /yolo to stop.",
                    );
                } else {
                    self.notify("YOLO off — permission prompts restored.");
                }
                Some(KeyOutcome::None)
            }
            "/copy" => {
                self.copy_last();
                Some(KeyOutcome::None)
            }
            "/usage" => {
                self.show_usage();
                Some(KeyOutcome::None)
            }
            "/jobs" => {
                self.show_jobs();
                Some(KeyOutcome::None)
            }
            "/mcp" => {
                self.show_mcp();
                Some(KeyOutcome::None)
            }
            "/lsp" => {
                self.show_lsp();
                Some(KeyOutcome::None)
            }
            "/model" => {
                if arg.is_empty() {
                    self.view.push_notice(format!(
                        "model: {} {}",
                        self.model_label,
                        self.reasoning.label()
                    ));
                    self.stick_to_bottom();
                    Some(KeyOutcome::None)
                } else {
                    Some(KeyOutcome::SwitchModel(arg.to_string()))
                }
            }
            "/reasoning" => {
                if arg.is_empty() {
                    Some(KeyOutcome::ListReasoning)
                } else {
                    Some(KeyOutcome::SetReasoning(arg.to_string()))
                }
            }
            "/models" => {
                if arg.is_empty() {
                    Some(KeyOutcome::ListModels)
                } else {
                    Some(KeyOutcome::SwitchModel(arg.to_string()))
                }
            }
            "/context" => Some(KeyOutcome::ShowContext),
            "/compact" => Some(KeyOutcome::Compact),
            "/memory" => {
                self.show_memory();
                Some(KeyOutcome::None)
            }
            "/init" => Some(KeyOutcome::Submit(INIT_PROMPT.to_string())),
            "/theme" => {
                if arg.is_empty() {
                    self.open_theme_picker(&self.theme_name.clone());
                } else {
                    self.theme_name = arg.to_string();
                    theme::set_theme(theme::Theme::by_name(arg));
                    if let Ok(cwd) = std::env::current_dir() {
                        let _ = bob_core::core::config::set_theme_in_project(&cwd, arg);
                    }
                }
                Some(KeyOutcome::None)
            }
            _ => {
                let name = cmd.strip_prefix('/').unwrap_or(cmd);
                if let Some(c) = self.custom_commands.iter().find(|c| c.name == name) {
                    return Some(KeyOutcome::Submit(c.expand(arg)));
                }
                None
            }
        }
    }

    fn show_memory(&mut self) {
        let mut shown = false;
        let project = std::env::current_dir().map(|c| c.join("AGENTS.md")).ok();
        if let Some(path) = project {
            if let Ok(text) = std::fs::read_to_string(&path) {
                let text = text.trim();
                if !text.is_empty() {
                    self.view
                        .push_notice(format!("memory · project ({})", path.display()));
                    self.view.push_notice(text.to_string());
                    shown = true;
                }
            }
        }
        if let Some(home) = dirs::home_dir() {
            let path = home.join(".bob").join("AGENTS.md");
            if let Ok(text) = std::fs::read_to_string(&path) {
                let text = text.trim();
                if !text.is_empty() {
                    self.view
                        .push_notice(format!("memory · global ({})", path.display()));
                    self.view.push_notice(text.to_string());
                    shown = true;
                }
            }
        }
        if !shown {
            self.view.push_notice(
                "no memories yet. bob saves durable preferences/conventions to AGENTS.md as you work.".to_string(),
            );
        }
        self.stick_to_bottom();
    }

    fn show_lsp(&mut self) {
        match &self.lsp {
            None => {
                self.view.push_notice(
                    "no language servers configured. Add one with: bob lsp add rust --ext rs -- rust-analyzer".to_string(),
                );
            }
            Some(lsp) => {
                use bob_core::lsp::Health;
                let statuses = lsp.statuses();
                if statuses.is_empty() {
                    self.view.push_notice("(no language servers)".to_string());
                } else {
                    self.view.push_notice("language servers".to_string());
                    for (name, health) in statuses {
                        let state = match health {
                            Health::Starting => "starting".to_string(),
                            Health::Indexing(Some(p)) => format!("indexing {p}%"),
                            Health::Indexing(None) => "indexing".to_string(),
                            Health::Ready => "ready".to_string(),
                            Health::Failed(reason) => format!("failed: {reason}"),
                        };
                        self.view.push_notice(format!("  {}: {}", name, state));
                    }
                }
            }
        }
        self.stick_to_bottom();
    }

    fn show_mcp(&mut self) {
        match bob_core::core::config::list_mcp_servers() {
            Ok(servers) if servers.is_empty() => {
                self.view.push_notice(
                    "no MCP servers configured. Add one with: bob mcp add <name> -- <command>"
                        .to_string(),
                );
            }
            Ok(servers) => {
                self.view.push_notice("MCP servers".to_string());
                for s in servers {
                    let args = if s.args.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", s.args.join(" "))
                    };
                    self.view
                        .push_notice(format!("  {}: {}{}", s.name, s.command, args));
                }
            }
            Err(e) => self
                .view
                .push_notice(format!("error reading MCP config: {}", e)),
        }
        self.stick_to_bottom();
    }

    fn show_jobs(&mut self) {
        use bob_core::tools::jobs::JobStatus;
        let jobs = self.jobs.list();
        if jobs.is_empty() {
            self.view.push_notice("(no background jobs)".to_string());
            return;
        }
        self.view.push_notice("background jobs".to_string());
        for (id, kind, desc, status) in jobs {
            let state = match status {
                JobStatus::Running => "running",
                JobStatus::Done => "done",
                JobStatus::Failed => "failed",
                JobStatus::Cancelled => "cancelled",
            };
            self.view
                .push_notice(format!("  {} [{}] {}: {}", id, state, kind, desc));
        }
        self.stick_to_bottom();
    }

    fn show_usage(&mut self) {
        let s = self.session_usage;
        let mut all = self.global_usage;
        all.add(&s);

        let row = |label: &str, u: &bob_core::core::types::Usage| -> String {
            format!(
                "  {:<9} {:>8} in   {:>8} out   {:>8} cached",
                label,
                fmt_tokens(u.total_input()),
                fmt_tokens(u.output_tokens),
                fmt_tokens(u.cache_read_input_tokens),
            )
        };

        self.view.push_notice("token usage".to_string());
        self.view.push_notice(row("session", &s));
        self.view.push_notice(row("all-time", &all));
        self.stick_to_bottom();
    }

    fn show_context(&mut self, used: usize, msgs: usize, window: usize) {
        let pct = used as f64 / window.max(1) as f64 * 100.0;
        let filled = ((pct.min(100.0) / 100.0) * 24.0).round() as usize;
        let bar: String = "█".repeat(filled) + &"░".repeat(24usize.saturating_sub(filled));
        self.view.push_notice("context".to_string());
        self.view.push_notice(format!(
            "  [{}] {:.0}%  (~{} / {} tokens, {} msgs)",
            bar,
            pct,
            fmt_tokens(used as u64),
            fmt_tokens(window as u64),
            msgs,
        ));
        self.view.push_notice(format!(
            "  auto-compaction enabled (at most {:.0}%, reserving room for output)",
            bob_core::agent::agent::COMPACT_THRESHOLD * 100.0,
        ));
        self.stick_to_bottom();
    }

    /// Detach tracks the turn but does not release the root agent's mutex.
    fn detach_current_turn(&mut self) {
        if !self.running || self.detached_job.is_some() {
            self.notify("nothing to detach");
            return;
        }
        let desc = self
            .current_prompt
            .clone()
            .unwrap_or_else(|| "turn".to_string());
        let id = self.jobs.next_id();
        self.jobs
            .register_tracking(id.clone(), "turn", truncate_mid(&desc, 60));
        self.detached_job = Some(id.clone());
        self.notify(format!("detached as {}", id));
    }

    /// Cancel cooperatively and discard queued prompts so they cannot restart work.
    fn interrupt(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.queue.clear();
        self.view.push_notice("interrupting…".to_string());
        self.notify("interrupting…");
        self.stick_to_bottom();
    }

    fn copy_last(&mut self) {
        let text = self.view.cells.iter().rev().find_map(|c| match c {
            view::Cell::Assistant { text, .. } => Some(text.clone()),
            _ => None,
        });
        match text {
            Some(text) => {
                clipboard::osc52_copy(&text);
                self.notify("copied last message");
            }
            None => self.notify("nothing to copy"),
        }
    }
}

/// Wrap by display columns, preserving span styles.
pub(super) fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    wrap_line_hanging(line, width, 0)
}

/// Prefix continuation rows with `indent` spaces.
pub(super) fn wrap_line_hanging(
    line: Line<'static>,
    width: usize,
    indent: usize,
) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= width {
        return vec![line];
    }
    use unicode_width::UnicodeWidthChar;
    let indent = indent.min(width.saturating_sub(1));
    let cont_lead = || Span::raw(" ".repeat(indent));
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;

    for span in line.spans {
        let style = span.style;
        let mut chunk = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if col + w > width && col > 0 {
                if !chunk.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                out.push(Line::from(std::mem::take(&mut cur)));
                if indent > 0 {
                    cur.push(cont_lead());
                }
                col = indent;
            }
            chunk.push(ch);
            col += w;
        }
        if !chunk.is_empty() {
            cur.push(Span::styled(chunk, style));
        }
    }
    if !cur.is_empty() {
        out.push(Line::from(cur));
    }
    out
}

pub(super) fn truncate_mid(s: &str, max: usize) -> String {
    if s.chars().count() <= max || max < 4 {
        return s.to_string();
    }
    let keep = max - 1;
    let head: String = s.chars().take(keep).collect();
    format!("{}...", head)
}

pub(super) fn indent_line(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("   ")];
    spans.extend(line.spans);
    Line::from(spans)
}

#[cfg(test)]
mod test_support;

#[cfg(all(test, unix))]
mod pty_tests {
    use bob_core::core::config::BobConfig;
    use bob_core::core::session::new_session;
    use bob_core::providers::mock::{MockProvider, MockReply};
    use std::fs::{self, File};
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const CHILD_FILTER: &str = "tui::pty_tests::child_fixture";
    const FIXTURE_ENV: &str = "BOB_PTY_TEST_FIXTURE";
    const STEP_TIMEOUT: Duration = Duration::from_secs(10);
    const MOCK_DELAY: Duration = Duration::from_secs(120);
    const IDLE_TEXT: &[u8] = b"send a message...";
    const CHILD_DONE: &[u8] = b"BOB_PTY_FIXTURE_RETURNED";

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("canonical repository root")
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let repo = repo_root();
            let target = repo.join("target");
            fs::create_dir_all(&target).expect("create repository target directory");
            let target = target.canonicalize().expect("canonical target directory");
            assert!(
                target.starts_with(&repo),
                "target must stay inside repository"
            );
            let root = target.join(format!("tui-pty-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&root).expect("create isolated PTY fixture");
            let fixture = Self(root);
            for name in [
                "work", "home", "appdata", "config", "cache", "data", "state", "runtime", "tmp",
                "bin",
            ] {
                fs::create_dir(fixture.0.join(name)).expect("create fixture directory");
            }
            fs::set_permissions(fixture.0.join("runtime"), fs::Permissions::from_mode(0o700))
                .expect("set XDG runtime permissions");
            fs::create_dir(fixture.0.join("work/.git")).expect("bound project discovery");
            fixture
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Pty {
        master: File,
        slave: File,
    }

    impl Pty {
        fn new() -> io::Result<Self> {
            let (mut master, mut slave) = (-1, -1);
            let mut size = libc::winsize {
                ws_row: 28,
                ws_col: 100,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            if unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            } == -1
            {
                return Err(io::Error::last_os_error());
            }
            // Own both descriptors immediately, including all subsequent error paths.
            let pty = Self {
                master: unsafe { File::from_raw_fd(master) },
                slave: unsafe { File::from_raw_fd(slave) },
            };
            for fd in [master, slave] {
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
                if flags == -1
                    || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
                {
                    return Err(io::Error::last_os_error());
                }
            }
            let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(io::Error::last_os_error());
            }
            let mut mode = pty.termios()?;
            mode.c_lflag |= libc::ICANON | libc::ECHO | libc::ISIG;
            mode.c_oflag |= libc::OPOST;
            if unsafe { libc::tcsetattr(slave, libc::TCSANOW, &mode) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(pty)
        }

        fn termios(&self) -> io::Result<libc::termios> {
            let mut mode = std::mem::MaybeUninit::uninit();
            if unsafe { libc::tcgetattr(self.slave.as_raw_fd(), mode.as_mut_ptr()) } == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { mode.assume_init() })
        }

        fn resize(&self, rows: u16, columns: u16) -> io::Result<()> {
            let mut size = libc::winsize {
                ws_row: rows,
                ws_col: columns,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            if unsafe { libc::ioctl(self.slave.as_raw_fd(), libc::TIOCSWINSZ as _, &mut size) }
                == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    struct RunningChild(Child);

    impl Drop for RunningChild {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    struct Driver {
        child: RunningChild,
        pty: Pty,
        output: Vec<u8>,
    }

    impl Driver {
        fn spawn(fixture: &Fixture) -> io::Result<Self> {
            let pty = Pty::new()?;
            let mut command = Command::new(std::env::current_exe()?);
            command
                .args([CHILD_FILTER, "--ignored", "--exact", "--nocapture"])
                .env_clear()
                .env(FIXTURE_ENV, &fixture.0)
                .env("HOME", fixture.0.join("home"))
                .env("APPDATA", fixture.0.join("appdata"))
                .env("XDG_CONFIG_HOME", fixture.0.join("config"))
                .env("XDG_CACHE_HOME", fixture.0.join("cache"))
                .env("XDG_DATA_HOME", fixture.0.join("data"))
                .env("XDG_STATE_HOME", fixture.0.join("state"))
                .env("XDG_RUNTIME_DIR", fixture.0.join("runtime"))
                .env("TMPDIR", fixture.0.join("tmp"))
                .env("PATH", fixture.0.join("bin"))
                .env("TERM", "xterm-256color")
                .env("LANG", "C")
                .current_dir(fixture.0.join("work"))
                .stdin(Stdio::from(pty.slave.try_clone()?))
                .stdout(Stdio::from(pty.slave.try_clone()?))
                .stderr(Stdio::from(pty.slave.try_clone()?));
            // Only async-signal-safe syscalls between fork and exec. A controlling
            // terminal makes crossterm's /dev/tty and SIGWINCH work on macOS and Linux.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            Ok(Self {
                child: RunningChild(command.spawn()?),
                pty,
                output: Vec::new(),
            })
        }

        fn drain(&mut self) {
            let mut bytes = [0; 8192];
            for _ in 0..64 {
                match self.pty.master.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(n) => self.output.extend_from_slice(&bytes[..n]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => panic!("read PTY: {error}"),
                }
                assert!(
                    self.output.len() <= 4 * 1024 * 1024,
                    "unexpected PTY output flood"
                );
            }
        }

        fn diagnostics(&self) -> String {
            let start = self.output.len().saturating_sub(8000);
            format!("{:?}", String::from_utf8_lossy(&self.output[start..]))
        }

        fn wait_for(&mut self, label: &str, mut ready: impl FnMut(&Self) -> bool) {
            let deadline = Instant::now() + STEP_TIMEOUT;
            loop {
                self.drain();
                assert!(
                    self.child
                        .0
                        .try_wait()
                        .expect("poll fixture child")
                        .is_none(),
                    "fixture exited before {label}; PTY: {}",
                    self.diagnostics()
                );
                if ready(self) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {label}; PTY: {}",
                    self.diagnostics()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        fn wait_text(&mut self, start: usize, text: &[u8], label: &str) {
            self.wait_for(label, |driver| {
                driver.output[start..]
                    .windows(text.len())
                    .any(|part| part == text)
            });
        }

        fn send(&mut self, mut bytes: &[u8]) {
            let deadline = Instant::now() + STEP_TIMEOUT;
            while !bytes.is_empty() {
                match self.pty.master.write(bytes) {
                    Ok(0) => panic!("PTY closed while sending input"),
                    Ok(n) => bytes = &bytes[n..],
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        self.drain();
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("write PTY: {error}"),
                }
                assert!(Instant::now() < deadline, "timed out writing PTY input");
            }
        }

        fn wait_exit(&mut self) -> ExitStatus {
            let deadline = Instant::now() + STEP_TIMEOUT;
            loop {
                self.drain();
                if let Some(status) = self.child.0.try_wait().expect("poll fixture exit") {
                    self.drain();
                    return status;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture exceeded quit deadline; PTY: {}",
                    self.diagnostics()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    fn real_pty_startup_resize_interrupt_quit_restores_termios() {
        let fixture = Fixture::new();
        let mut driver = Driver::spawn(&fixture).expect("launch real TUI in PTY");
        // The child can enter raw mode immediately after spawn; capture the baseline
        // in the child before run(), rather than racing it from the parent.
        driver.wait_text(0, IDLE_TEXT, "initial TUI prompt");
        driver.wait_text(0, b"\x1b[?2026l", "completed startup frame");
        let raw = driver.pty.termios().expect("read active terminal mode");
        assert_eq!(
            raw.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG),
            0,
            "TUI must enter raw mode"
        );

        driver.drain();
        let resized = driver.output.len();
        driver.pty.resize(34, 112).expect("resize PTY");
        // This checks a completed frame after the resize, not its pixel layout.
        driver.wait_text(resized, b"\x1b[?2026l", "completed frame after resize");
        driver.send(b"Harmless PTY smoke prompt. Do not use tools.\r");
        driver.wait_for("mock generation to start", |_| {
            fixture.0.join("provider-started").is_file()
        });
        driver.drain();
        let interrupted = driver.output.len();
        driver.send(b"\x1b");
        driver.wait_text(
            interrupted,
            b"interrupting",
            "Escape interrupt acknowledgement",
        );
        // Busy mode suppresses this placeholder. Its return before the 120-second
        // mock response proves the interrupted turn settled while the TUI stayed up.
        driver.wait_text(interrupted, IDLE_TEXT, "idle prompt after cancellation");
        driver.send(b"\x03");
        let status = driver.wait_exit();
        assert!(
            status.success(),
            "fixture failed: {status}; PTY: {}",
            driver.diagnostics()
        );
        assert!(
            driver
                .output
                .windows(CHILD_DONE.len())
                .any(|part| part == CHILD_DONE),
            "helper must actually run and return"
        );
        assert!(
            driver.output.windows(8).any(|part| part == b"\x1b[?1049l"),
            "TUI must leave alternate screen"
        );
        let restored = fs::read_to_string(fixture.0.join("termios-after"))
            .expect("fixture restored termios before PTY hangup");
        let expected =
            fs::read_to_string(fixture.0.join("termios-before")).expect("fixture baseline termios");
        assert_eq!(
            restored, expected,
            "terminal flags, control characters and speeds must be restored"
        );
    }

    fn termios_state(mode: &libc::termios) -> String {
        // Compare semantic fields, never struct padding (which differs by platform).
        format!(
            "{} {} {} {} {:?} {} {}",
            mode.c_iflag,
            mode.c_oflag,
            mode.c_cflag,
            mode.c_lflag,
            mode.c_cc,
            unsafe { libc::cfgetispeed(mode) },
            unsafe { libc::cfgetospeed(mode) },
        )
    }

    #[test]
    #[ignore = "launched only by the parent PTY smoke test in an isolated process"]
    fn child_fixture() {
        let root =
            PathBuf::from(std::env::var_os(FIXTURE_ENV).expect("PTY parent must supply isolation"));
        let root = root.canonicalize().expect("canonical fixture root");
        assert!(root.starts_with(repo_root().join("target")));
        assert_eq!(std::env::current_dir().unwrap(), root.join("work"));
        assert!(root.join("work/.git").is_dir());
        for name in [
            "HOME",
            "APPDATA",
            "XDG_CONFIG_HOME",
            "XDG_CACHE_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "XDG_RUNTIME_DIR",
            "TMPDIR",
            "PATH",
        ] {
            let path = PathBuf::from(std::env::var_os(name).expect("isolated child environment"));
            assert!(
                path.canonicalize().unwrap().starts_with(&root),
                "{name} must be isolated"
            );
        }
        let mut mode = std::mem::MaybeUninit::uninit();
        assert_eq!(unsafe { libc::tcgetattr(0, mode.as_mut_ptr()) }, 0);
        let mode = unsafe { mode.assume_init() };
        fs::write(root.join("termios-before"), termios_state(&mode))
            .expect("record baseline terminal mode");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("fixture Tokio runtime");
        runtime.block_on(async {
            let provider = Arc::new(
                MockProvider::new(vec![])
                    .with_default(MockReply::Text("delayed response must not complete".into()))
                    .with_delay(MOCK_DELAY),
            );
            let observed = provider.clone();
            let marker = root.join("provider-started");
            let observer = tokio::spawn(async move {
                while observed.call_count() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                fs::write(marker, b"started").expect("record mock generation start");
            });
            let config = BobConfig::default();
            assert!(config.lsp_servers.is_empty() && config.mcp_servers.is_empty());
            let cwd = root.join("work");
            let session = new_session(
                "mock:mock-model",
                "pty-smoke".into(),
                "0".into(),
                cwd.to_string_lossy().into_owned(),
            );
            super::run(
                config,
                provider.clone(),
                "mock:mock-model".into(),
                cwd,
                session,
            )
            .await
            .expect("real TUI run");
            observer.abort();
            assert_eq!(provider.call_count(), 1, "exactly one harmless generation");
        });
        drop(runtime);
        let mut restored = std::mem::MaybeUninit::uninit();
        assert_eq!(unsafe { libc::tcgetattr(0, restored.as_mut_ptr()) }, 0);
        fs::write(
            root.join("termios-after"),
            termios_state(&unsafe { restored.assume_init() }),
        )
        .expect("record restored terminal mode before PTY hangup");
        println!("{}", std::str::from_utf8(CHILD_DONE).unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::test_app;
    use super::*;

    #[test]
    fn authoritative_stop_summaries_are_not_repeated_as_notices() {
        for stop_reason in [
            RunStopReason::Completed,
            RunStopReason::TurnLimit,
            RunStopReason::OutputLimit,
            RunStopReason::ToolPanic,
            RunStopReason::Cancelled,
        ] {
            let completion = TurnCompletion::from_outcome(bob_core::agent::agent::RunOutcome {
                text: "authoritative handoff".into(),
                stop_reason,
                turns: 1,
            });
            assert_eq!(
                completion.error.is_some(),
                stop_reason == RunStopReason::Cancelled
            );
            assert_eq!(completion.stopped, stop_reason != RunStopReason::Completed);
            assert!(!completion.panicked);
        }
    }

    #[tokio::test]
    async fn root_panic_sends_completion_and_releases_agent_mutex() {
        use bob_core::agent::agent::{build_subagent, SubagentSpec};
        use bob_core::core::types::{Completion, GenerateOptions, StreamEvent};

        struct PanicProvider;
        #[async_trait]
        impl Provider for PanicProvider {
            fn name(&self) -> &str {
                "panic"
            }
            fn model(&self) -> &str {
                "panic-model"
            }
            async fn generate(&self, _: GenerateOptions) -> anyhow::Result<Completion> {
                panic!("unexpected generation")
            }
            async fn stream(
                &self,
                _: GenerateOptions,
            ) -> anyhow::Result<mpsc::UnboundedReceiver<StreamEvent>> {
                tokio::task::yield_now().await;
                panic!("provider worker panic")
            }
        }
        let agent = Arc::new(Mutex::new(build_subagent(SubagentSpec {
            provider: Arc::new(PanicProvider),
            tools: bob_core::tools::registry::ToolRegistry::new(None),
            bus: EventBus::new(),
            system: None,
            cwd: ".".into(),
            jobs: bob_core::tools::jobs::JobRegistry::new(),
            lsp: None,
            name: "root".into(),
            max_turns: 3,
            depth: 0,
            inbox: None,
            team: None,
            parent_cancel: None,
            hooks: Default::default(),
            memory: None,
        })));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let worker = spawn_turn(&agent, &tx, "go".into());
        let completion = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(completion.panicked && completion.stopped);
        assert!(completion.error.unwrap().contains("Exit and restart"));
        assert!(agent.try_lock().is_ok());
        worker.await.unwrap();
        assert!(rx.try_recv().is_err());
    }

    fn question(
        title: &str,
        allow_other: bool,
    ) -> (PendingQuery, oneshot::Receiver<Option<String>>) {
        let (resp, rx) = oneshot::channel();
        (
            PendingQuery {
                query: bob_core::tools::registry::UserQuery {
                    title: title.into(),
                    detail: String::new(),
                    options: vec!["Yes, proceed".into(), "No, revise".into()],
                    allow_other,
                },
                list: widgets::SelectList::new(),
                other_text: None,
                inspection: Default::default(),
                purpose: QueryPurpose::Tool(resp),
            },
            rx,
        )
    }

    #[test]
    fn terminal_key_kind_controls_editing_and_resize_invalidates_hit_maps() {
        let mut app = test_app();
        for (kind, expected, expected_dirty) in [
            (KeyEventKind::Release, "", false),
            (KeyEventKind::Press, "界", true),
            (KeyEventKind::Repeat, "界界", true),
        ] {
            let mut dirty = false;
            let key = crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('界'),
                KeyModifiers::NONE,
                kind,
            );
            assert!(matches!(
                app.on_terminal_event(CtEvent::Key(key), &mut dirty),
                KeyOutcome::None
            ));
            assert_eq!(app.input.text(), expected);
            assert_eq!(dirty, expected_dirty);
        }
        app.sidebar_rect = Some(Rect::new(10, 0, 10, 20));
        app.sidebar_rows = Some(vec![(2, "worker".into())]);
        app.wf_view_agents = Some(vec![(3, "worker".into())]);
        app.focused_agent = Some("worker".into());
        let mut dirty = false;
        app.on_terminal_event(CtEvent::Resize(80, 24), &mut dirty);
        assert!(dirty);
        assert!(app.sidebar_rect.is_none());
        assert!(app.sidebar_rows.is_none());
        assert!(app.wf_view_agents.is_none());
        assert_eq!(app.focused_agent.as_deref(), Some("worker"));
        assert_eq!(app.input.text(), "界界");
    }

    #[test]
    fn enter_resolves_paste_and_steers_only_a_running_root_turn() {
        for (running, focused, modifiers, steer) in [
            (false, None, KeyModifiers::NONE, false),
            (false, None, KeyModifiers::ALT, false),
            (true, None, KeyModifiers::NONE, false),
            (true, None, KeyModifiers::ALT, true),
            (true, Some("worker"), KeyModifiers::ALT, false),
        ] {
            let mut app = test_app();
            app.running = running;
            app.focused_agent = focused.map(String::from);
            app.input.paste("  first\n日本  ");
            let outcome = app.on_key(KeyCode::Enter, modifiers);
            match outcome {
                KeyOutcome::Steer(text) if steer => assert_eq!(text, "first\n日本"),
                KeyOutcome::Submit(text) if !steer => assert_eq!(text, "first\n日本"),
                _ => panic!("wrong outcome for running={running}, focused={focused:?}, modifiers={modifiers:?}"),
            }
            assert_eq!(app.input.text(), "");
            assert!(app.queue.is_empty(), "queueing belongs to the event loop");
        }
        let mut app = test_app();
        app.input.set("é");
        assert!(matches!(
            app.on_key(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyOutcome::None
        ));
        assert_eq!(app.input.text(), "é\n");
        app.input.set("  ");
        assert!(matches!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
            KeyOutcome::None
        ));
        assert_eq!(app.input.text(), "  ");
    }

    #[test]
    fn escape_dismisses_completion_before_interrupting_and_backspace_recalls_last_queue_item() {
        let mut app = test_app();
        app.running = true;
        app.input.set("/co");
        app.refresh_menu();
        assert!(!app.menu.is_empty());
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.menu.is_empty());
        assert!(!app.cancel.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(app.input.text(), "/co");
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.cancel.load(std::sync::atomic::Ordering::Relaxed));
        app.input.set("");
        app.queue.extend(["first".into(), "last 界".into()]);
        app.on_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "last 界");
        assert_eq!(
            app.queue.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["first"]
        );
        app.on_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.input.text(), "last ");
        assert_eq!(app.queue.len(), 1);
    }

    #[test]
    fn query_selection_replies_once_then_advances_without_editing_composer() {
        let mut app = test_app();
        app.input.set("draft");
        let (first, mut first_rx) = question("first", false);
        let (second, mut second_rx) = question("second", true);
        app.enqueue_query(first);
        app.enqueue_query(second);
        app.on_key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(app.pending_query.as_ref().unwrap().list.selected, 0);
        for _ in 0..3 {
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(app.pending_query.as_ref().unwrap().list.selected, 1);
        app.on_key(KeyCode::Char('9'), KeyModifiers::NONE);
        assert!(matches!(
            first_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(first_rx.try_recv().unwrap(), Some("No, revise".into()));
        assert_eq!(app.pending_query.as_ref().unwrap().query.title, "second");
        app.on_key(KeyCode::Char('3'), KeyModifiers::NONE);
        assert_eq!(
            app.pending_query.as_ref().unwrap().other_text.as_deref(),
            Some("")
        );
        for ch in ['é', '界'] {
            app.on_key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
        app.on_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(
            app.pending_query.as_ref().unwrap().other_text.as_deref(),
            Some("é")
        );
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.pending_query.as_ref().unwrap().other_text.is_none());
        assert!(matches!(
            second_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(second_rx.try_recv().unwrap(), None);
        assert!(app.pending_query.is_none());
        assert_eq!(app.input.text(), "draft");
    }

    #[test]
    fn permission_selection_answers_fifo_before_pending_questions() {
        let mut app = test_app();
        app.input.set("draft");
        let (query, mut query_rx) = question("question", false);
        app.enqueue_query(query);
        let mut receivers = Vec::new();
        for title in ["first permission", "second permission"] {
            let (resp, rx) = oneshot::channel();
            receivers.push(rx);
            app.perm_queue.push_back(PendingPerm {
                title: title.into(),
                detail: String::new(),
                preview: None,
                options: vec![
                    PermissionOption {
                        label: "allow".into(),
                        allow: true,
                        grant: None,
                    },
                    PermissionOption {
                        label: "deny".into(),
                        allow: false,
                        grant: None,
                    },
                ],
                list: widgets::SelectList::new(),
                resp,
            });
        }
        for _ in 0..3 {
            app.on_key(KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(app.perm_queue.front().unwrap().list.selected, 1);
        app.on_key(KeyCode::Char('9'), KeyModifiers::NONE);
        assert_eq!(app.perm_queue.len(), 2);
        app.on_key(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(receivers[0].try_recv().unwrap(), Some(1));
        assert_eq!(app.perm_queue.front().unwrap().title, "second permission");
        assert_eq!(app.perm_queue.front().unwrap().list.selected, 0);
        app.on_key(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(receivers[1].try_recv().unwrap(), None);
        assert!(app.perm_queue.is_empty());
        assert!(matches!(
            query_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(app.pending_query.as_ref().unwrap().list.selected, 0);
        app.on_key(KeyCode::Char('1'), KeyModifiers::NONE);
        assert_eq!(query_rx.try_recv().unwrap(), Some("Yes, proceed".into()));
        assert_eq!(app.input.text(), "draft");
    }

    #[test]
    fn plan_mode_exits_only_for_approved_plan_question() {
        for (title, key, expected) in [
            ("Ready to code?", '1', Mode::Normal),
            ("Ready to code?", '2', Mode::Plan),
            ("Continue?", '1', Mode::Plan),
        ] {
            let mut app = test_app();
            app.permissions.set_mode(Mode::Plan);
            let (query, mut rx) = question(title, false);
            app.enqueue_query(query);
            app.on_key(KeyCode::Char(key), KeyModifiers::NONE);
            assert_eq!(app.permissions.mode(), expected, "{title}, {key}");
            assert_eq!(
                rx.try_recv().unwrap(),
                Some(
                    if key == '1' {
                        "Yes, proceed"
                    } else {
                        "No, revise"
                    }
                    .into()
                )
            );
        }
    }

    #[test]
    fn model_choice_strips_metadata_and_ctrl_c_quits_without_answering() {
        let mut app = test_app();
        app.open_model_picker(vec!["openai:model\t128k ctx".into()]);
        assert!(matches!(
            app.on_key(KeyCode::Enter, KeyModifiers::NONE),
            KeyOutcome::SwitchModel(id) if id == "openai:model"
        ));
        assert!(app.pending_query.is_none());
        let (query, mut rx) = question("still pending", true);
        app.enqueue_query(query);
        assert!(matches!(
            app.on_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyOutcome::Quit
        ));
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert!(app.pending_query.is_some());
    }

    #[test]
    fn agent_completion_reorders_sidebar_without_changing_focused_identity() {
        let mut app = test_app();
        for id in ["first", "second"] {
            app.apply_agent_event(&AgentEvent::SubagentSpawn {
                agent_id: id.into(),
                parent_id: "root".into(),
                task: "review".into(),
                prompt: String::new(),
            });
        }
        app.set_focused_agent(Some("first".into()));
        app.apply_agent_event(&AgentEvent::SubagentDone {
            agent_id: "first".into(),
            failed: false,
            cancelled: false,
        });
        assert_eq!(app.teams.display_order(), vec!["second", "first"]);
        assert_eq!(app.focused_agent.as_deref(), Some("first"));
        assert_eq!(app.sidebar.selected, 2);
        assert_eq!(
            app.teams.get("first").unwrap().status,
            team::ThreadStatus::Done
        );
    }

    #[test]
    fn truncation_preserves_short_text_and_unicode_character_boundaries() {
        for (label, text, max, expected) in [
            ("under cap", "hi", 8, "hi"),
            ("no room for ellipsis", "abcdef", 3, "abcdef"),
            ("ascii head", "hello world", 8, "hello w..."),
            ("unicode head", "héllo wörld", 6, "héllo..."),
        ] {
            assert_eq!(truncate_mid(text, max), expected, "{label}");
        }
    }

    #[test]
    fn line_wrapping_uses_display_columns_and_preserves_zero_width_input() {
        for (label, text, width, expected) in [
            ("ascii", "aaaa", 2, vec!["aa", "aa"]),
            ("wide glyphs", "a界b", 2, vec!["a", "界", "b"]),
            ("combining mark", "e\u{301}xy", 2, vec!["e\u{301}x", "y"]),
            ("zero width", "abc", 0, vec!["abc"]),
        ] {
            let wrapped = wrap_line(Line::from(vec![Span::raw(text)]), width);
            let rows: Vec<String> = wrapped.iter().map(ToString::to_string).collect();
            assert_eq!(rows, expected, "{label}");
        }
    }
}
