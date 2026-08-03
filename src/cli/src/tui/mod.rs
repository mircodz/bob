//! The ratatui TUI: retained view model + async draw loop. Wires the same
//! bob-core the CLI uses, but renders events into a pretty scrollback with
//! markdown, syntax-highlighted code, diffs, and pretty tool cells.

mod clipboard;
mod diffview;
mod draw;
mod files;
mod highlight;
mod input;
mod markdown;
mod render;
mod scrollback;
mod team;
mod theme;
mod view;

use bob_core::agent::agent::Agent;
use bob_core::core::config::BobConfig;
use bob_core::core::events::{AgentEvent, EventBus};
use bob_core::core::permissions::{
    Asker, Decision, Mode, PermissionEngine, PermissionOption, PermissionRequest,
};
use bob_core::core::policies::{
    allow_bash_commands, allow_code_action_list, allow_read_only, allow_tools, deny_dangerous_bash,
    deny_tools,
};
use bob_core::core::session::{save_session, Session};
use bob_core::providers::create_provider;
use bob_core::providers::provider::Provider;

use async_trait::async_trait;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, BeginSynchronizedUpdate, EndSynchronizedUpdate,
    EnterAlternateScreen, LeaveAlternateScreen,
};
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
use view::ViewModel;

/// Build a "shimmer" version of `text`: a bright band sweeps across the glyphs
/// as a working indicator. `tick` advances the wave (one step per
/// animation frame). Each character's brightness is a function of its distance
/// from the moving crest, interpolated between a dim base and a bright peak.
pub(super) fn shimmer_spans(text: &str, tick: usize) -> Vec<Span<'static>> {
    // Base (dim) and peak (bright) endpoints of the gradient.
    const BASE: (f32, f32, f32) = (0x66 as f32, 0x66 as f32, 0x66 as f32);
    const PEAK: (f32, f32, f32) = (0xf0 as f32, 0xf0 as f32, 0xf0 as f32);
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(1) as f32;
    // The crest travels across the text and a bit beyond, then wraps, so there's
    // a brief "rest" before the next sweep. Period is a touch longer than n.
    let span = n + 6.0;
    let pos = (tick as f32 * 0.6) % span;
    // Width of the bright band, in characters.
    let width = 3.0_f32;
    chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let d = (i as f32 - pos).abs();
            // Smooth falloff: 1.0 at the crest, 0.0 past `width`.
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

/// Payload sent from the asker to the UI loop: what's being requested plus the
/// choices, and a channel to return the chosen index.
struct PermPrompt {
    title: String,
    detail: String,
    preview: Option<String>,
    options: Vec<PermissionOption>,
    resp: oneshot::Sender<Option<usize>>,
}

/// A pending permission ask surfaced to the UI, plus its live selection cursor.
struct PendingPerm {
    title: String,
    detail: String,
    preview: Option<String>,
    options: Vec<PermissionOption>,
    selected: usize,
    resp: oneshot::Sender<Option<usize>>,
}

/// Asker that hands the request to the UI loop and awaits the chosen option.
struct TuiAsker {
    tx: mpsc::UnboundedSender<PermPrompt>,
}

/// A user question posed by ask_user / exit_plan, sent to the UI loop.
struct QueryPrompt {
    query: bob_core::tools::registry::UserQuery,
    resp: oneshot::Sender<Option<String>>,
}

/// The UI-side asker for user questions (ask_user / exit_plan).
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

/// What a pending question is for — routes how the answer is handled.
enum QueryPurpose {
    /// From a tool (ask_user / exit_plan): answer goes back via the oneshot.
    Tool(oneshot::Sender<Option<String>>),
    /// UI-initiated model picker: the answer is a model spec to switch to.
    ModelPicker,
    /// Reasoning-effort picker (chained after the model picker): the answer is
    /// an effort label (off/low/medium/high/max).
    ReasoningPicker,
    /// Theme picker: moving the selection live-previews that theme; Enter
    /// persists it, Esc reverts to the theme active when the picker opened.
    ThemePicker { original: String },
}

/// A pending user question in the UI, with its live selection + optional
/// free-text ("Other…") entry buffer.
struct PendingQuery {
    query: bob_core::tools::registry::UserQuery,
    selected: usize,
    /// When the user picks "Other", this holds the typed answer (Some = typing).
    other_text: Option<String>,
    purpose: QueryPurpose,
}

#[async_trait]
impl Asker for TuiAsker {
    async fn ask(&self, req: &PermissionRequest, options: &[PermissionOption]) -> Option<usize> {
        let (title, detail) = if req.tool == "bash" {
            (
                "Run shell command?".to_string(),
                req.bash.as_ref().map(|b| b.raw.clone()).unwrap_or_default(),
            )
        } else {
            (
                format!("Allow {}?", req.tool),
                describe_input(&req.tool, &req.input),
            )
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        let prompt = PermPrompt {
            title,
            detail,
            preview: req.preview.clone(),
            options: options.to_vec(),
            resp: resp_tx,
        };
        if self.tx.send(prompt).is_err() {
            return None;
        }
        resp_rx.await.unwrap_or(None)
    }
}

/// A short human description of a non-bash tool's key argument.
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

/// Compact token count: 1234 → "1.2k", 2_000_000 → "2.0M".
fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Display path with $HOME collapsed to `~`.
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

/// Current git branch name, if `path` is inside a git repo. Best-effort.
fn git_branch(path: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Spawn a turn: lock the agent, run the prompt to completion, then signal done
/// with the error message (if any) so the UI can surface failures instead of
/// silently ending the turn.
fn spawn_turn(
    agent: &Arc<Mutex<Agent>>,
    done_tx: &mpsc::UnboundedSender<Option<String>>,
    text: String,
) {
    let agent = agent.clone();
    let done_tx = done_tx.clone();
    tokio::spawn(async move {
        let mut a = agent.lock().await;
        let err = match a.run(&text).await {
            Ok(_) => None,
            Err(e) => Some(e.to_string()),
        };
        let _ = done_tx.send(err);
    });
}

/// The result of a piece of work run off the event loop, applied back on the
/// loop thread via [`App::apply_bg`]. To offload any new expensive feature: add
/// a variant here, call [`spawn_bg`] with a future that produces it, and handle
/// it in `apply_bg`. No other plumbing — the select loop already drains these.
enum BgOutcome {
    /// A provider was (re)built for a model switch (slow: network/auth). Ok holds
    /// the resolved `provider_id` + the ready provider; Err is a message.
    ProviderSwitched(Result<(String, Arc<dyn Provider>), String>),
    /// A provider's model list came back for the `/models` picker.
    ModelList(Result<Vec<String>, String>),
    /// The `@file` candidate list finished gathering (git ls-files / walk).
    FileList(Vec<String>),
}

/// Run `fut` off the event loop and deliver its [`BgOutcome`] back to the UI.
/// This is the single seam for keeping the render/input loop responsive: the
/// expensive work happens on a Tokio worker, the cheap UI update happens on the
/// loop when the outcome arrives.
fn spawn_bg<F>(tx: &mpsc::UnboundedSender<BgOutcome>, fut: F)
where
    F: std::future::Future<Output = BgOutcome> + Send + 'static,
{
    let tx = tx.clone();
    tokio::spawn(async move {
        let _ = tx.send(fut.await);
    });
}

/// Entry point: assemble core with a TUI asker and run the loop.
pub async fn run(
    config: BobConfig,
    provider: Arc<dyn Provider>,
    provider_spec: String,
    cwd: PathBuf,
    mut session: Session,
) -> anyhow::Result<()> {
    // Select the color theme from config before anything renders.
    theme::set_theme(theme::Theme::by_name(
        config.theme.as_deref().unwrap_or("dark"),
    ));

    let bus = EventBus::new();

    // Bridge bus events → an async channel the UI loop drains. Subagent chatter
    // (tool calls/results/text from spawned `task_*` agents) is suppressed to
    // avoid noise; only the root agent's stream and subagent *spawn* notices
    // reach the UI. The task tool feeds each subagent's final result back to the
    // root as a normal tool result, so nothing important is lost.
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<AgentEvent>();
    {
        let evt_tx = evt_tx.clone();
        bus.on(Arc::new(move |e: &AgentEvent| {
            // Every event is forwarded to the UI channel; the apply site routes each
            // to the main transcript or a team-drawer thread by agent id.
            let _ = evt_tx.send(e.clone());
        }));
    }

    // Permission asker → UI channel.
    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<PermPrompt>();
    let asker = Arc::new(TuiAsker { tx: perm_tx });

    // User-question asker (ask_user / exit_plan) → UI channel.
    let (query_tx, mut query_rx) = mpsc::unbounded_channel::<QueryPrompt>();
    let user_asker = Arc::new(TuiUserAsker { tx: query_tx });

    let default_decision = match config.permissions.default.as_str() {
        "allow" => Decision::Allow,
        "deny" => Decision::Deny,
        _ => Decision::Ask,
    };
    let mut engine = PermissionEngine::new(default_decision, Some(asker));
    engine.add(allow_read_only());
    engine.add(allow_code_action_list());
    engine.add(deny_dangerous_bash());
    engine.add(allow_bash_commands(config.permissions.allow_bash.clone()));
    engine.add(allow_tools(config.permissions.allow.clone()));
    engine.add(deny_tools(config.permissions.deny.clone()));
    let permissions = Arc::new(engine);
    // Restore any "always allow" grants saved with this session.
    if !session.grants.is_empty() {
        permissions.import_grants(session.grants.clone());
    }

    // Shared background-job registry: the same instance is handed to the root
    // agent, the task tool, and the UI panel so all three see the same jobs.
    let jobs = bob_core::tools::jobs::JobRegistry::new();

    // Shared team roster for agent coordination (spawn_agent/send_message/
    // list_agents). Root + every spawned agent share this instance.
    let team = bob_core::agent::team::AgentRegistry::new();

    // Connect to any configured MCP servers (bob is the MCP *client*). Their
    // tools are namespaced `<server>.<tool>` and handed to root + subagents.
    let (mcp_tools, mcp_notices) = bob_core::mcp::connect_all(&config.mcp_servers).await;

    // Start any configured language servers in the background. This returns
    // immediately; servers initialize + index without blocking the session.
    let lsp = if config.lsp_servers.is_empty() {
        None
    } else {
        Some(bob_core::lsp::LspManager::start(&config.lsp_servers, &cwd))
    };

    // Compose the system prompt: the base bob prompt (or a config override) plus
    // a live environment block and any AGENTS.md/CLAUDE.md project context.
    let system_prompt =
        bob_core::agent::prompt::build_system_prompt(config.system.as_deref(), &cwd);

    // Build the fully-wired root agent (tools + coordination + team mailbox). The
    // same builder backs the remote host, so the two can't drift.
    let mut agent =
        bob_core::agent::assembly::build_root_agent(bob_core::agent::assembly::RootAgentParams {
            provider: provider.clone(),
            permissions: permissions.clone(),
            bus: bus.clone(),
            jobs: jobs.clone(),
            team: team.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            system_prompt: system_prompt.clone(),
            mcp_tools: mcp_tools.clone(),
            lsp: lsp.clone(),
            user_asker: user_asker.clone(),
            max_turns: config.max_turns,
        });
    if !session.messages.is_empty() {
        agent.load_history(session.messages.clone());
    }
    // Grab the cancel handle before moving the agent behind the mutex, so the UI
    // can interrupt a running turn without needing to lock the (busy) agent.
    let cancel = agent.cancel_handle();
    let todos = agent.todos();
    // Restore the persisted todo list so the panel survives a resume.
    if !session.todos.is_empty() {
        todos.set(session.todos.clone());
    }
    let agent = Arc::new(Mutex::new(agent));

    // --- terminal setup ---
    // Restore the terminal from a single place (RAII + panic hook) so a panic in
    // the draw path or any `?`-propagated error can't leave the user's shell in
    // raw mode / the alternate screen.
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    // A guard whose Drop restores the terminal, covering both the normal return
    // and any early `?`/unwind out of `run`.
    let _guard = TerminalGuard;
    // On panic, restore the terminal *before* the default hook prints the panic
    // message, so the backtrace is readable and the shell isn't wrecked.
    {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            default_hook(info);
        }));
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(
        jobs.clone(),
        permissions.clone(),
        cancel.clone(),
        team.clone(),
    );
    app.provider_id = provider_spec.split(':').next().unwrap_or("").to_string();
    app.model_label = provider.model().to_string();
    app.cwd_label = abbreviate_home(&cwd);
    app.branch = git_branch(&cwd);
    app.lsp = lsp.clone();
    app.todos = Some(todos);
    // Restore per-agent drawer transcripts from a resumed session.
    if !session.agent_threads.is_empty() {
        app.teams = team::AgentTranscripts::from_persisted(&session.agent_threads);
    }
    app.theme_name = config.theme.clone().unwrap_or_else(|| "dark".to_string());
    // Seed usage totals: this session's prior usage + the global ledger.
    app.session_usage = bob_core::core::usage::total_of(&session.usage);
    app.global_usage = bob_core::core::usage::global_total();
    if !session.messages.is_empty() {
        app.view.hydrate(&session.messages);
        app.view.push_notice(format!(
            "resumed session {} ({} msgs)",
            session.id,
            session.messages.len()
        ));
    }
    // Surface MCP connection results.
    for note in &mcp_notices {
        app.view.push_notice(note.clone());
    }

    let mut keys = EventStream::new();
    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    // Frame-rate–capped redraw. Input and agent events set `dirty`; the loop
    // repaints at most once per `FRAME`. A trackpad flick emits a burst of scroll
    // events arriving milliseconds apart — drawing once per event makes repaints
    // queue behind real time ("runoff"). Coalescing every event within a frame
    // window into ONE repaint keeps scrolling locked to your fingers.
    const FRAME: Duration = Duration::from_millis(16);
    draw_frame(&mut terminal, &mut app)?;
    let mut last_draw = tokio::time::Instant::now();
    let mut dirty = false;

    let result = 'outer: loop {
        if dirty && last_draw.elapsed() >= FRAME {
            draw_frame(&mut terminal, &mut app)?;
            last_draw = tokio::time::Instant::now();
            dirty = false;
        }
        // If a repaint is pending but the frame budget hasn't elapsed, wake exactly
        // at the deadline; otherwise sleep far out (real events/ticker still wake us).
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
                // Assume this event changes something; the arms that DON'T (a key
                // release, an unhandled event, a no-op mouse move) reset it. On
                // Windows every key press has a matching release and the mouse emits
                // a stream of move events — repainting on those forces a 60fps redraw
                // that shows as a flickering cursor, so only repaint on real changes.
                dirty = true;
                match maybe_key {
                    Some(Ok(CtEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                        match app.on_key(key.code, key.modifiers) {
                            KeyOutcome::Quit => break 'outer Ok(()),
                            KeyOutcome::Submit(text) => {
                                if app.running {
                                    // A turn is in flight — queue this as a pinned
                                    // chip above the input (NOT the transcript). It's
                                    // pushed to the transcript + dispatched when the
                                    // turn ends. Alt+Enter steers instead of queueing.
                                    app.queue.push_back(text);
                                } else {
                                    app.view.push_user(text.clone());
                                    app.stick_to_bottom();
                                    app.running = true;
                                    app.current_prompt = Some(text.clone());
                                    app.turn_started = Some(std::time::Instant::now());
                                    spawn_turn(&agent, &app.turn_done_tx, text);
                                }
                            }
                            KeyOutcome::Steer(text) => {
                                // Deliver into the root agent's inbox now, so the
                                // running turn folds it in at its next step (mid-turn
                                // course-correction). Shown in the transcript as a
                                // user line so there's a record of what was said.
                                if app.agent_team.send("root", "user", &text) {
                                    app.view.push_user(text);
                                    app.stick_to_bottom();
                                } else {
                                    app.toast = Some("couldn't steer — no running agent".into());
                                }
                            }
                            KeyOutcome::SwitchModel(spec) => {
                                if app.running {
                                    app.toast = Some("finish the current turn first".into());
                                } else {
                                    // A bare model id (no "provider:" prefix) uses
                                    // the current provider; a full spec switches both.
                                    let full = if spec.contains(':') {
                                        spec.clone()
                                    } else {
                                        format!("{}:{}", app.provider_id, spec)
                                    };
                                    // Building a provider does network/auth work —
                                    // run it off the loop so the UI stays live.
                                    app.toast = Some("switching model…".into());
                                    spawn_bg(&app.bg_tx, async move {
                                        let id = full.split(':').next().unwrap_or("").to_string();
                                        BgOutcome::ProviderSwitched(
                                            create_provider(&full)
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
                                // Never block the loop: if a turn holds the agent
                                // lock, apply on the next idle boundary via a toast.
                                if let Ok(mut a) = agent.try_lock() {
                                    a.set_reasoning(effort);
                                    app.reasoning = effort;
                                    app.view.push_event(format!(
                                        "Model changed to {} {}",
                                        app.model_label,
                                        effort.label()
                                    ));
                                    app.stick_to_bottom();
                                } else {
                                    app.toast = Some("busy — try again after this turn".into());
                                }
                            }
                            KeyOutcome::ListModels => {
                                // Fetch the current provider's models off the loop,
                                // then open the picker when the list arrives.
                                let prov = match agent.try_lock() {
                                    Ok(a) => a.provider(),
                                    Err(_) => {
                                        app.toast = Some("busy — try again after this turn".into());
                                        continue 'outer;
                                    }
                                };
                                app.toast = Some("fetching models…".into());
                                spawn_bg(&app.bg_tx, async move {
                                    BgOutcome::ModelList(
                                        prov.list_models().await.map_err(|e| e.to_string()),
                                    )
                                });
                            }
                            KeyOutcome::ShowContext => {
                                // Reading history needs the lock; skip cleanly if a
                                // turn holds it rather than freezing the UI.
                                if let Ok(a) = agent.try_lock() {
                                    let used = bob_core::agent::compaction::estimate_history_tokens(a.messages());
                                    let msgs = a.messages().len();
                                    drop(a);
                                    app.show_context(used, msgs);
                                } else {
                                    app.toast = Some("busy — try again after this turn".into());
                                }
                            }
                            KeyOutcome::None => {}
                        }
                    }
                    Some(Ok(CtEvent::Paste(text))) => app.input.paste(&text),
                    Some(Ok(CtEvent::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => {
                            // One line per wheel notch for a smooth, precise feel
                            // (batching coalesces a fast flick into one redraw).
                            if let Some(d) = app.team_drawer.as_mut() {
                                d.scroll = d.scroll.saturating_sub(1);
                            } else {
                                app.scrollback.scroll_up(1);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if let Some(d) = app.team_drawer.as_mut() {
                                d.scroll = d.scroll.saturating_add(1);
                            } else {
                                app.scrollback.scroll_down(1);
                            }
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            // In the drawer, a click on a roster row selects that
                            // agent. In the main view, a click on a tool cell
                            // expands/collapses its output; a click elsewhere sticks
                            // to the bottom.
                            if app.team_drawer.is_some() {
                                app.click_roster(m.column, m.row);
                            } else if !app.click_scrollback(m.column, m.row) {
                                app.stick_to_bottom();
                            }
                        }
                        MouseEventKind::Moved => {
                            // Hover highlight in the drawer. Only a CHANGE in the
                            // hovered row warrants a repaint (mouse-move fires a lot).
                            let changed = if app.team_drawer.is_some() {
                                app.hover_roster(m.column, m.row)
                            } else {
                                false
                            };
                            dirty = changed;
                        }
                        _ => {}
                    },
                    Some(Ok(_)) => dirty = false,
                    Some(Err(_)) | None => break 'outer Ok(()),
                }
            }
            Some(evt) = evt_rx.recv() => {
                dirty = true;
                // Intercept usage accounting before handing the event to the view.
                if let AgentEvent::Completion { agent_id, model, usage } = &evt {
                    let entry = bob_core::core::usage::UsageEntry {
                        ts: unix_now(),
                        session_id: session.id.clone(),
                        provider: provider.name().to_string(),
                        model: model.clone(),
                        agent_id: agent_id.clone(),
                        usage: *usage,
                    };
                    session.usage.push(entry.clone());
                    app.session_usage.add(usage);
                    let _ = bob_core::core::usage::append_global(&entry);
                }
                // Route to the right transcript: the main view always sees the
                // event (it updates the "• Spawned" cell for subagents and ignores
                // their prose), and the team store captures each subagent's full
                // thread for the drawer.
                let showing = app
                    .team_drawer
                    .as_ref()
                    .and_then(|d| app.teams.display_order().get(d.selected).cloned());
                app.teams.apply(&evt, showing.as_deref());
                app.view.apply(&evt);
                // Follow new output only when already pinned to the bottom. If the
                // user has scrolled up to read, don't yank them back down on every
                // streamed token.
                if app.scrollback.at_bottom() {
                    app.stick_to_bottom();
                }
            }
            Some(prompt) = perm_rx.recv() => {
                dirty = true;
                app.pending_perm = Some(PendingPerm {
                    title: prompt.title,
                    detail: prompt.detail,
                    preview: prompt.preview,
                    options: prompt.options,
                    selected: 0,
                    resp: prompt.resp,
                });
            }
            Some(q) = query_rx.recv() => {
                dirty = true;
                app.pending_query = Some(PendingQuery {
                    query: q.query,
                    selected: 0,
                    other_text: None,
                    purpose: QueryPurpose::Tool(q.resp),
                });
            }
            Some(err) = app.turn_done_rx.recv() => {
                dirty = true;
                // Surface a failed turn (provider error, etc.) instead of
                // silently ending.
                if let Some(msg) = err {
                    app.view.push_notice(format!("error: {}", msg));
                    app.stick_to_bottom();
                }
                // Persist after each completed turn (messages + session grants).
                let a = agent.lock().await;
                session.messages = a.messages().to_vec();
                session.grants = permissions.export_grants();
                if let Some(todos) = &app.todos {
                    session.todos = todos.items();
                }
                session.agent_threads = app.teams.to_persisted();
                session.updated_at = now_stamp();
                let _ = save_session(&session);
                drop(a);
                // If this turn had been detached (Ctrl+B), close out its job.
                if let Some(job_id) = app.detached_job.take() {
                    app.jobs.finish(&job_id, bob_core::tools::jobs::JobStatus::Done, "turn finished".to_string());
                }
                app.current_prompt = None;
                // Dispatch the next queued prompt, if any. Queued chips weren't in
                // the transcript, so push it as a user turn now that it's actually
                // being sent.
                match app.queue.pop_front() {
                    Some(next) => {
                        app.view.push_user(next.clone());
                        app.stick_to_bottom();
                        app.current_prompt = Some(next.clone());
                        app.turn_started = Some(std::time::Instant::now());
                        spawn_turn(&agent, &app.turn_done_tx, next);
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
                // Only the animated states (spinner/Working line, toast) need a
                // periodic repaint. When fully idle, leave `dirty` false so the loop
                // parks in sleep_until instead of repainting 10×/s for nothing.
                if app.running || app.view.busy || app.toast.is_some() {
                    app.spinner = app.spinner.wrapping_add(1);
                    dirty = true;
                }
                // Coordination wake: if idle but a spawned agent has reported back,
                // drive a fresh empty-prompt turn so the root folds the result into
                // history and acts on it — an idle agent is re-driven by a new turn
                // rather than blocking.
                if !app.running {
                    let pending = {
                        // try_lock: an in-flight turn holds this; we'll re-check on
                        // the next tick. Never block the render loop.
                        match agent.try_lock() {
                            Ok(mut a) => a.has_pending_coordination(),
                            Err(_) => false,
                        }
                    };
                    if pending {
                        app.running = true;
                        app.turn_started = Some(std::time::Instant::now());
                        spawn_turn(&agent, &app.turn_done_tx, String::new());
                        dirty = true;
                    }
                }
            }
        }
    };

    // Final save on exit: the per-turn save only runs when a turn completes, so a
    // quit (Quit/​/exit/Ctrl+C) or a stream error would otherwise drop everything
    // since the last completed turn. Snapshot the agent's full history + state now.
    {
        let a = agent.lock().await;
        session.messages = a.messages().to_vec();
        drop(a);
        session.grants = permissions.export_grants();
        if let Some(todos) = &app.todos {
            session.todos = todos.items();
        }
        session.agent_threads = app.teams.to_persisted();
        session.updated_at = now_stamp();
        let _ = save_session(&session);
    }

    // Terminal restore is handled by `_guard` (Drop) — covers the normal return,
    // the `?` error paths above, and panics (via the hook). Nothing to do here.
    result
}

/// Render one frame wrapped in a *synchronized update* (DEC private mode 2026).
/// The terminal buffers every byte between Begin/End and paints the frame
/// atomically, so partial frames and a flickering cursor are impossible — this is
/// what fixes the flicker on Windows Terminal/conhost and the same technique the
/// harness renderer uses. Terminals that don't support 2026 ignore the sequence,
/// so it's safe everywhere. `terminal.draw` already diffs cells, so combined with
/// this each frame is one atomic, minimal write.
fn draw_frame(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> std::io::Result<()> {
    let _ = execute!(std::io::stdout(), BeginSynchronizedUpdate);
    let res = terminal.draw(|f| app.draw(f));
    let _ = execute!(std::io::stdout(), EndSynchronizedUpdate);
    res.map(|_| ())
}

/// Best-effort restore of the terminal to its pre-TUI state. Safe to call more
/// than once (idempotent) and from a panic hook — every step ignores errors.
fn restore_terminal() {
    let mut stdout = std::io::stdout();
    let _ = execute!(
        stdout,
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture,
        crossterm::cursor::Show,
    );
    let _ = disable_raw_mode();
}

/// RAII guard: restores the terminal on Drop, so an early return or unwind out
/// of `run` can never leave the shell in raw mode / the alternate screen.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

enum KeyOutcome {
    None,
    Submit(String),
    /// Steer the RUNNING turn: deliver this text to the root agent's inbox now
    /// (Alt+Enter), instead of queueing it for after the turn.
    Steer(String),
    /// Switch the active provider/model to this spec (e.g. "anthropic:claude...").
    SwitchModel(String),
    /// Open the reasoning-effort picker.
    ListReasoning,
    /// Set the reasoning effort (label: off/low/medium/high/max).
    SetReasoning(String),
    /// Fetch the current provider's model list and open a picker.
    ListModels,
    /// Show context-window usage (needs the agent's history, handled in loop).
    ShowContext,
    Quit,
}

struct App {
    view: ViewModel,
    input: input::Input,
    spinner: usize,
    pending_perm: Option<PendingPerm>,
    /// Filtered slash-command menu, when the input starts with '/'.
    menu: Vec<(&'static str, &'static str)>,
    menu_sel: usize,
    /// `@file` completion: the full project file list (gathered lazily), the
    /// current filtered matches, and the selected index. `file_at` is the byte
    /// offset of the active `@` in the input buffer while completing.
    all_files: Vec<String>,
    /// True while a background `@file` gather is in flight (so we kick it off once).
    files_loading: bool,
    file_menu: Vec<String>,
    file_sel: usize,
    file_at: Option<usize>,
    toast: Option<String>,
    /// Prompts submitted while a turn is running, sent in order once it frees.
    queue: std::collections::VecDeque<String>,
    /// Whether a turn is currently executing (drives the queue + input hint).
    running: bool,
    /// If the running turn has been detached (Ctrl+B), the job id tracking it.
    detached_job: Option<String>,
    /// The prompt of the running turn (used as the job label on detach).
    current_prompt: Option<String>,
    /// Running token total for this session (sum of all completions).
    session_usage: bob_core::core::types::Usage,
    /// Grand total across all prior sessions, loaded from the global ledger.
    global_usage: bob_core::core::types::Usage,
    /// Shared background-job registry (for the pinned panel + Ctrl+B detach).
    jobs: bob_core::tools::jobs::JobRegistry,
    /// Permission engine handle (for mode switching via Shift+Tab).
    permissions: Arc<PermissionEngine>,
    /// Cancel flag for the running turn (set on Esc to interrupt).
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// A pending ask_user / exit_plan question.
    pending_query: Option<PendingQuery>,
    /// Human-readable current model spec (for /models and status).
    model_label: String,
    /// Current reasoning effort (shown next to the model, like "gpt-5.5 medium").
    reasoning: bob_core::core::types::ReasoningEffort,
    /// The config-registry provider id (e.g. "copilot", "openai", "anthropic").
    /// Distinct from the trait's name() — copilot and openai both report "openai".
    provider_id: String,
    turn_done_tx: mpsc::UnboundedSender<Option<String>>,
    turn_done_rx: mpsc::UnboundedReceiver<Option<String>>,
    /// Channel for background work (model switch/list, file gathering, …). The
    /// select loop drains `bg_rx` and applies each outcome via `apply_bg`.
    bg_tx: mpsc::UnboundedSender<BgOutcome>,
    bg_rx: mpsc::UnboundedReceiver<BgOutcome>,
    /// When the current turn started (for the "Working (Ns)" line).
    turn_started: Option<std::time::Instant>,
    /// Working directory (shown in the status line, abbreviated with ~).
    cwd_label: String,
    /// Current git branch, if any (refreshed lazily; shown in the status line).
    branch: Option<String>,
    /// Language servers for this project, for the status-line health indicator.
    lsp: Option<Arc<bob_core::lsp::LspManager>>,
    /// The agent's todo list, rendered as a sticky panel above the input.
    todos: Option<Arc<bob_core::tools::todo::TodoStore>>,
    /// Active theme name (for the /theme picker's current-selection + direct set).
    theme_name: String,
    /// The main transcript viewport: render caches, scroll position, click map.
    scrollback: scrollback::ScrollbackRenderer,
    /// Per-agent transcripts for the team drawer (fed by subagent events).
    teams: team::AgentTranscripts,
    /// Team drawer overlay state; `None` when closed.
    team_drawer: Option<team::TeamDrawer>,
    /// Screen rect of the drawer's agent roster (set each draw), so left-clicks can
    /// be hit-tested to select an agent. `None` when the drawer is closed.
    roster_rect: Option<Rect>,
    /// Whether the sticky todo panel is shown (toggled with Ctrl+L).
    show_todos: bool,
    /// Shared team roster, so the drawer can message agents (send_message path).
    agent_team: bob_core::agent::team::AgentRegistry,
}

const COMMANDS: &[(&str, &str)] = &[
    ("/copy", "copy the last message to the clipboard"),
    ("/clear", "clear the transcript"),
    ("/usage", "show token usage (session + all-time)"),
    ("/context", "show context-window usage"),
    ("/model", "show the current model"),
    ("/models", "list & switch models"),
    (
        "/reasoning",
        "set reasoning effort (off/low/medium/high/max)",
    ),
    ("/theme", "switch color theme (dark/light/terminal)"),
    ("/jobs", "list background jobs"),
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
            pending_perm: None,
            menu: Vec::new(),
            menu_sel: 0,
            all_files: Vec::new(),
            files_loading: false,
            file_menu: Vec::new(),
            file_sel: 0,
            file_at: None,
            toast: None,
            queue: std::collections::VecDeque::new(),
            running: false,
            detached_job: None,
            current_prompt: None,
            session_usage: bob_core::core::types::Usage::default(),
            global_usage: bob_core::core::types::Usage::default(),
            jobs,
            permissions,
            cancel,
            pending_query: None,
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
            teams: team::AgentTranscripts::new(),
            team_drawer: None,
            roster_rect: None,
            show_todos: true,
            agent_team,
        }
    }

    fn stick_to_bottom(&mut self) {
        self.scrollback.stick_to_bottom();
    }

    fn refresh_menu(&mut self) {
        let text = self.input.text();
        if text.starts_with('/') && !text.contains(' ') {
            self.menu = COMMANDS
                .iter()
                .copied()
                .filter(|(c, _)| c.starts_with(text))
                .collect();
            self.menu_sel = self.menu_sel.min(self.menu.len().saturating_sub(1));
        } else {
            self.menu.clear();
            self.menu_sel = 0;
        }
        self.refresh_file_menu();
    }

    /// Apply a completed [`BgOutcome`] on the event-loop thread. All the slow
    /// work already happened off-loop; here we only touch UI state (and the
    /// agent via `try_lock`, which never blocks — a switch is disallowed mid-turn
    /// anyway, so the lock is free).
    fn apply_bg(&mut self, outcome: BgOutcome, agent: &Arc<Mutex<Agent>>) {
        match outcome {
            BgOutcome::ProviderSwitched(Ok((id, provider))) => {
                let model = provider.model().to_string();
                if let Ok(mut a) = agent.try_lock() {
                    a.set_provider(provider);
                } else {
                    self.toast = Some("busy — try switching again".into());
                    return;
                }
                self.provider_id = id;
                self.model_label = model;
                self.toast = None;
                // Chain into the reasoning picker so model + effort are set together.
                let cur = self.reasoning;
                self.open_reasoning_picker(cur);
                self.stick_to_bottom();
            }
            BgOutcome::ProviderSwitched(Err(e)) => {
                self.toast = None;
                self.view.push_notice(format!("error: {}", e));
                self.stick_to_bottom();
            }
            BgOutcome::ModelList(Ok(models)) => {
                // Backends that don't list (ChatGPT/Codex Responses) fall back to
                // the known set.
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
                self.toast = None;
                if options.is_empty() {
                    self.view
                        .push_notice("provider returned no models; use `/models <spec>`".into());
                    self.stick_to_bottom();
                } else {
                    self.pending_query = Some(PendingQuery {
                        query: bob_core::tools::registry::UserQuery {
                            title: format!("Select a model (current: {})", self.model_label),
                            detail: String::new(),
                            options,
                            allow_other: true,
                        },
                        selected: 0,
                        other_text: None,
                        purpose: QueryPurpose::ModelPicker,
                    });
                }
            }
            BgOutcome::ModelList(Err(e)) => {
                self.toast = None;
                self.view
                    .push_notice(format!("error listing models: {}", e));
                self.stick_to_bottom();
            }
            BgOutcome::FileList(files) => {
                self.all_files = files;
                self.files_loading = false;
                // Re-filter now that candidates are available (the user may have
                // typed more since the gather started).
                self.refresh_file_menu();
            }
        }
    }

    /// Update the `@file` completion menu from the token under the cursor.
    fn refresh_file_menu(&mut self) {
        match self.input.at_token() {
            Some((start, query)) => {
                self.file_at = Some(start);
                // Gather the project file list lazily and OFF the keystroke path —
                // `git ls-files` / a filesystem walk can take real time on a big
                // repo. Kick it off once; `BgOutcome::FileList` fills `all_files`
                // and re-filters. Until then we just show whatever we have (empty).
                if self.all_files.is_empty() && !self.files_loading {
                    self.files_loading = true;
                    let bg_tx = self.bg_tx.clone();
                    spawn_bg(&bg_tx, async move {
                        // Blocking fs/subprocess work → a blocking-safe worker.
                        let files = tokio::task::spawn_blocking(|| {
                            let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                            files::gather_files(&cwd)
                        })
                        .await
                        .unwrap_or_default();
                        BgOutcome::FileList(files)
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

    /// Accept the selected `@file` completion, replacing the token in the input.
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

    /// Handle keys while a user question (ask_user / exit_plan) is open.
    fn handle_query_key(&mut self, code: KeyCode) -> KeyOutcome {
        let Some(q) = &mut self.pending_query else {
            return KeyOutcome::None;
        };
        let n = q.query.options.len() + if q.query.allow_other { 1 } else { 0 };

        // Free-text "Other" entry mode routes typing into the buffer.
        if let Some(buf) = &mut q.other_text {
            match code {
                KeyCode::Enter => {
                    let answer = buf.clone();
                    return self.answer_query(answer);
                }
                KeyCode::Esc => q.other_text = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return KeyOutcome::None;
        }

        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                if q.selected > 0 {
                    q.selected -= 1;
                }
                self.preview_theme_if_picking();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if q.selected + 1 < n {
                    q.selected += 1;
                }
                self.preview_theme_if_picking();
            }
            KeyCode::Char(c @ '1'..='9') => {
                let idx = (c as usize) - ('1' as usize);
                if idx < n {
                    return self.pick_query_option(idx);
                }
            }
            KeyCode::Enter => {
                let idx = q.selected;
                return self.pick_query_option(idx);
            }
            KeyCode::Esc => {
                if let Some(q) = self.pending_query.take() {
                    match q.purpose {
                        QueryPurpose::Tool(resp) => {
                            let _ = resp.send(None);
                        }
                        // Revert the live preview to the theme active on open.
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

    /// Select option `idx`; the "Other" row enters free-text mode.
    fn pick_query_option(&mut self, idx: usize) -> KeyOutcome {
        let Some(q) = &mut self.pending_query else {
            return KeyOutcome::None;
        };
        if idx < q.query.options.len() {
            let answer = q.query.options[idx].clone();
            self.answer_query(answer)
        } else {
            q.other_text = Some(String::new());
            KeyOutcome::None
        }
    }

    /// Finalize the pending query. Tool questions send the answer back over the
    /// oneshot; the model picker turns the answer into a SwitchModel outcome.
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
                    self.toast = Some("plan approved · mode: normal".into());
                }
                let _ = resp.send(Some(answer));
                KeyOutcome::None
            }
            QueryPurpose::ModelPicker => KeyOutcome::SwitchModel(answer),
            QueryPurpose::ReasoningPicker => KeyOutcome::SetReasoning(answer),
            QueryPurpose::ThemePicker { .. } => {
                // `answer` is the chosen theme name. It's already live (applied on
                // hover); persist it to the project config, silently.
                self.theme_name = answer.clone();
                theme::set_theme(theme::Theme::by_name(&answer));
                if let Ok(cwd) = std::env::current_dir() {
                    let _ = bob_core::core::config::set_theme_in_project(&cwd, &answer);
                }
                KeyOutcome::None
            }
        }
    }

    /// While the theme picker is open, apply the hovered theme so the whole UI
    /// previews it live. No-op for any other picker.
    fn preview_theme_if_picking(&mut self) {
        let Some(q) = &self.pending_query else { return };
        if !matches!(q.purpose, QueryPurpose::ThemePicker { .. }) {
            return;
        }
        if let Some(name) = q.query.options.get(q.selected) {
            theme::set_theme(theme::Theme::by_name(name));
        }
    }

    /// Open the theme picker: a list of theme names that live-previews on hover,
    /// persists on Enter, and reverts on Esc. Selection starts on the current
    /// theme so opening the picker doesn't change anything until you move.
    fn open_theme_picker(&mut self, current: &str) {
        let options: Vec<String> = theme::Theme::NAMES.iter().map(|s| s.to_string()).collect();
        let selected = options.iter().position(|n| n == current).unwrap_or(0);
        self.pending_query = Some(PendingQuery {
            query: bob_core::tools::registry::UserQuery {
                title: "Theme (↑↓ to preview, Enter to keep)".to_string(),
                detail: String::new(),
                options,
                allow_other: false,
            },
            selected,
            other_text: None,
            purpose: QueryPurpose::ThemePicker {
                original: current.to_string(),
            },
        });
    }

    /// Open the reasoning-effort picker (chained after a model switch, or via
    /// `/reasoning`).
    fn open_reasoning_picker(&mut self, current: bob_core::core::types::ReasoningEffort) {
        let options: Vec<String> = ["off", "low", "medium", "high", "max"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        self.pending_query = Some(PendingQuery {
            query: bob_core::tools::registry::UserQuery {
                title: format!("Reasoning effort (current: {})", current.label()),
                detail: String::new(),
                options,
                allow_other: false,
            },
            selected: 0,
            other_text: None,
            purpose: QueryPurpose::ReasoningPicker,
        });
    }

    /// Open the team drawer (if any agents exist) or close it if already open.
    fn toggle_team_drawer(&mut self) {
        if self.team_drawer.is_some() {
            self.team_drawer = None;
            return;
        }
        if self.teams.is_empty() {
            self.toast = Some("no agents yet".into());
            return;
        }
        if let Some(id) = self.teams.display_order().first().cloned() {
            self.teams.mark_read(&id);
        }
        self.team_drawer = Some(team::TeamDrawer::default());
    }

    /// Select a drawer agent by mouse click. `col`/`row` are terminal cells. The
    /// roster has one leading blank line, so agent `i` is at `rect.y + 1 + i`.
    fn click_roster(&mut self, col: u16, row: u16) {
        let Some(rect) = self.roster_rect else {
            return;
        };
        // Ignore clicks outside the roster column (e.g. on the transcript pane).
        if col < rect.x || col >= rect.x + rect.width {
            return;
        }
        if row <= rect.y {
            return; // header/blank line
        }
        let idx = (row - rect.y - 1) as usize;
        let order = self.teams.display_order();
        if idx >= order.len() {
            return;
        }
        if let Some(drawer) = self.team_drawer.as_mut() {
            if drawer.selected != idx {
                drawer.selected = idx;
                drawer.scroll = 0;
            }
        }
        if let Some(id) = order.get(idx) {
            self.teams.mark_read(id);
        }
    }

    /// Toggle a tool cell's expanded state when the scrollback is clicked at
    /// `row`. Maps the row → cell via the scrollback's hit-test; only `Cell::Tool`
    /// cells toggle. Returns true if something toggled (so the caller marks dirty).
    fn click_scrollback(&mut self, _col: u16, row: u16) -> bool {
        match self.scrollback.hit_test(row) {
            Some(cell_idx) => self.view.toggle_tool_expanded(cell_idx),
            None => false,
        }
    }

    /// `hovered` to the roster index under the cursor, or `None` when off-roster.
    /// Returns true if the hover state changed (so the caller can mark dirty).
    fn hover_roster(&mut self, col: u16, row: u16) -> bool {
        let new_hover = self.roster_rect.and_then(|rect| {
            if col < rect.x || col >= rect.x + rect.width || row <= rect.y {
                return None;
            }
            let idx = (row - rect.y - 1) as usize;
            (idx < self.teams.display_order().len()).then_some(idx)
        });
        if let Some(drawer) = self.team_drawer.as_mut() {
            if drawer.hovered != new_hover {
                drawer.hovered = new_hover;
                return true;
            }
        }
        false
    }

    /// Keys while the team drawer is open. ↑/↓ (or j/k) select an agent, PgUp/
    /// PgDn scroll the transcript, `i` compose a message, Esc/Ctrl+T close.
    fn handle_drawer_key(&mut self, code: KeyCode) -> KeyOutcome {
        let count = self.teams.len();
        // Compose mode: keys edit/submit the message to the selected agent.
        if self
            .team_drawer
            .as_ref()
            .is_some_and(|d| d.composing.is_some())
        {
            return self.handle_drawer_compose_key(code);
        }
        let Some(drawer) = self.team_drawer.as_mut() else {
            return KeyOutcome::None;
        };
        let mut selection_changed = false;
        match code {
            KeyCode::Esc => {
                self.team_drawer = None;
                return KeyOutcome::None;
            }
            KeyCode::Char('i') => {
                drawer.composing = Some(String::new());
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if drawer.selected > 0 {
                    drawer.selected -= 1;
                    drawer.scroll = 0;
                    selection_changed = true;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if drawer.selected + 1 < count {
                    drawer.selected += 1;
                    drawer.scroll = 0;
                    selection_changed = true;
                }
            }
            KeyCode::PageUp => {
                drawer.scroll = drawer.scroll.saturating_sub(5);
            }
            KeyCode::PageDown => {
                drawer.scroll = drawer.scroll.saturating_add(5);
            }
            _ => {}
        }
        if selection_changed {
            let sel = drawer.selected;
            if let Some(id) = self.teams.display_order().get(sel).cloned() {
                self.teams.mark_read(&id);
            }
        }
        KeyOutcome::None
    }

    /// Keys while composing a drawer message. Enter sends it into the selected
    /// agent's inbox (the send_message path); Esc cancels; chars/backspace edit.
    fn handle_drawer_compose_key(&mut self, code: KeyCode) -> KeyOutcome {
        match code {
            KeyCode::Esc => {
                if let Some(d) = self.team_drawer.as_mut() {
                    d.composing = None;
                }
            }
            KeyCode::Char(c) => {
                if let Some(buf) = self.team_drawer.as_mut().and_then(|d| d.composing.as_mut()) {
                    buf.push(c);
                }
            }
            KeyCode::Backspace => {
                if let Some(buf) = self.team_drawer.as_mut().and_then(|d| d.composing.as_mut()) {
                    buf.pop();
                }
            }
            KeyCode::Enter => {
                self.send_drawer_message();
            }
            _ => {}
        }
        KeyOutcome::None
    }

    /// Deliver the composed message into the selected agent's inbox, mirroring the
    /// `send_message` tool, and echo it into that agent's thread immediately.
    fn send_drawer_message(&mut self) {
        let Some(drawer) = self.team_drawer.as_mut() else {
            return;
        };
        let text = drawer.composing.take().unwrap_or_default();
        let text = text.trim().to_string();
        let sel = drawer.selected;
        if text.is_empty() {
            return;
        }
        let Some(id) = self.teams.display_order().get(sel).cloned() else {
            return;
        };
        // Route through the shared team registry (same path SendMessageTool uses),
        // so the agent is woken to process it by the coordination loop.
        if self.agent_team.send(&id, "user", &text) {
            self.teams.push_message(&id, "user", &text, Some(&id));
        } else {
            self.toast = Some(format!("{} is not reachable", id));
        }
    }

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyOutcome {
        // 0) Shift+Tab cycles the interaction mode (normal → auto-accept → plan).
        if code == KeyCode::BackTab {
            let next = self.permissions.mode().next();
            self.permissions.set_mode(next);
            return KeyOutcome::None;
        }

        // 0a) Ctrl+T toggles the team drawer (only meaningful once agents exist).
        if code == KeyCode::Char('t') && mods.contains(KeyModifiers::CONTROL) {
            self.toggle_team_drawer();
            return KeyOutcome::None;
        }
        // 0a') While the drawer is open it owns the keyboard.
        if self.team_drawer.is_some() {
            return self.handle_drawer_key(code);
        }

        // 0b) A pending user question (ask_user / exit_plan) takes priority.
        if self.pending_query.is_some() {
            return self.handle_query_key(code);
        }

        // 1) Permission prompt takes priority — a select, driven by arrows,
        //    digits (1-9), Enter (confirm), or Esc/Ctrl+C (deny).
        if let Some(p) = &mut self.pending_perm {
            let n = p.options.len();
            match code {
                KeyCode::Up | KeyCode::Char('k') => {
                    if p.selected > 0 {
                        p.selected -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if p.selected + 1 < n {
                        p.selected += 1;
                    }
                }
                KeyCode::Char(c @ '1'..='9') => {
                    let idx = (c as usize) - ('1' as usize);
                    if idx < n {
                        let p = self.pending_perm.take().unwrap();
                        let _ = p.resp.send(Some(idx));
                    }
                }
                KeyCode::Enter => {
                    let sel = p.selected;
                    let p = self.pending_perm.take().unwrap();
                    let _ = p.resp.send(Some(sel));
                }
                KeyCode::Esc => {
                    // Esc denies (send None → treated as deny).
                    let _ = self.pending_perm.take().unwrap().resp.send(None);
                }
                _ => {}
            }
            return KeyOutcome::None;
        }

        // 2) Global shortcuts.
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
                // Toggle the sticky todo (work items) panel.
                self.show_todos = !self.show_todos;
                return KeyOutcome::None;
            }
            (KeyCode::Esc, _) => {
                // Esc interrupts a running turn (cooperative). Only meaningful
                // while busy — otherwise it's ignored (prompts handle Esc above).
                if self.running {
                    self.interrupt();
                }
                return KeyOutcome::None;
            }
            (KeyCode::PageUp, _) => {
                self.scrollback.scroll_up(10);
                return KeyOutcome::None;
            }
            (KeyCode::PageDown, _) => {
                self.scrollback.scroll_down(10);
                return KeyOutcome::None;
            }
            _ => {}
        }

        // 2b) Emacs / readline editing bindings.
        if self.handle_emacs(code, mods) {
            self.refresh_menu();
            return KeyOutcome::None;
        }

        // 2c) `@file` completion menu navigation (takes precedence).
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

        // 3) Slash-command menu navigation.
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
                _ => {}
            }
        }

        // 4) Input editing.
        match code {
            KeyCode::Enter => {
                // Shift+Enter inserts a newline (multi-line prompts). Alt+Enter is
                // "steer": deliver the text to the running turn immediately; when
                // idle it falls through to a normal submit.
                if mods.contains(KeyModifiers::SHIFT) {
                    self.input.insert_newline();
                    return KeyOutcome::None;
                }
                let display = self.input.text().trim().to_string();
                if display.is_empty() {
                    return KeyOutcome::None;
                }
                // Resolve collapsed pastes into the real text before sending.
                let text = self.input.resolved_text().trim().to_string();
                self.input.submit();
                self.menu.clear();
                // Slash-commands are never steered/queued — run them inline.
                if let Some(out) = self.handle_command(&display) {
                    return out;
                }
                if mods.contains(KeyModifiers::ALT) && self.running {
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
                // Backspace on an empty prompt pops the LAST queued chip back into
                // the input, so you can edit or delete a message you queued.
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
            "/clear" => {
                self.view.clear();
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
                // Print the current model + effort (or switch, if given an arg).
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
                // Set reasoning effort directly, or open the picker.
                if arg.is_empty() {
                    Some(KeyOutcome::ListReasoning)
                } else {
                    Some(KeyOutcome::SetReasoning(arg.to_string()))
                }
            }
            "/models" => {
                // Open the live picker (or switch directly with an argument).
                if arg.is_empty() {
                    Some(KeyOutcome::ListModels)
                } else {
                    Some(KeyOutcome::SwitchModel(arg.to_string()))
                }
            }
            "/context" => Some(KeyOutcome::ShowContext),
            "/theme" => {
                if arg.is_empty() {
                    self.open_theme_picker(&self.theme_name.clone());
                } else {
                    // Direct switch: apply + persist silently.
                    self.theme_name = arg.to_string();
                    theme::set_theme(theme::Theme::by_name(arg));
                    if let Ok(cwd) = std::env::current_dir() {
                        let _ = bob_core::core::config::set_theme_in_project(&cwd, arg);
                    }
                }
                Some(KeyOutcome::None)
            }
            _ => None,
        }
    }

    /// Print the language servers + their live health as notice cells.
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

    /// Print the configured MCP servers as notice cells.
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

    /// Print the background-job list as notice cells.
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

    /// Print a token-usage summary (session + all-time) as notice cells.
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

    /// Print context-window usage: estimated tokens in the live history vs the
    /// window, with a simple bar. `used` is a chars/4 estimate from the agent.
    fn show_context(&mut self, used: usize, msgs: usize) {
        const WINDOW: usize = 200_000;
        const COMPACT_AT: f64 = 0.8; // matches the agent's compact threshold
        let pct = (used as f64 / WINDOW as f64 * 100.0).min(100.0);
        let filled = ((pct / 100.0) * 24.0).round() as usize;
        let bar: String = "█".repeat(filled) + &"░".repeat(24usize.saturating_sub(filled));
        self.view.push_notice("context".to_string());
        self.view.push_notice(format!(
            "  [{}] {:.0}%  (~{} / {} tokens, {} msgs)",
            bar,
            pct,
            fmt_tokens(used as u64),
            fmt_tokens(WINDOW as u64),
            msgs,
        ));
        self.view.push_notice(format!(
            "  auto-compacts at {:.0}% (~{} tokens)",
            COMPACT_AT * 100.0,
            fmt_tokens((WINDOW as f64 * COMPACT_AT) as u64),
        ));
        self.stick_to_bottom();
    }

    /// Ctrl+B: detach the in-flight turn into the background-job registry so it
    /// shows in the panel and the input is freed. The turn keeps running (its
    /// completion is still handled by the turn_done channel, which closes out the
    /// job). Note: because the root agent is single-locked, a *new* prompt still
    /// waits for the detached turn to release the agent — detach frees the UI,
    /// not true root-level concurrency.
    fn detach_current_turn(&mut self) {
        if !self.running || self.detached_job.is_some() {
            self.toast = Some("nothing to detach".into());
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
        self.toast = Some(format!("detached as {}", id));
    }

    /// Esc: cooperatively interrupt the running turn. Sets the cancel flag (the
    /// agent unwinds at its next safe point, keeping history valid) and clears
    /// any queued prompts so they don't fire after the interrupt.
    fn interrupt(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.queue.clear();
        self.view.push_notice("interrupting…".to_string());
        self.toast = Some("interrupting…".into());
        self.stick_to_bottom();
    }

    fn copy_last(&mut self) {
        // Copy the last assistant message to the clipboard via OSC 52 — we let the
        // terminal own the clipboard rather than talking to X11/Wayland directly
        // (arboard can block/hang inside the draw loop). Works over SSH too.
        let text = self.view.cells.iter().rev().find_map(|c| match c {
            view::Cell::Assistant { text, .. } => Some(text.clone()),
            _ => None,
        });
        match text {
            Some(text) => {
                clipboard::osc52_copy(&text);
                self.toast = Some("copied last message".into());
            }
            None => self.toast = Some("nothing to copy".into()),
        }
    }
}

/// Word-aware wrap of a styled Line into <= width display lines, splitting
/// spans at boundaries and preserving each span's style. Over-long single
/// tokens are hard-broken.
pub(super) fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= width {
        return vec![line];
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;

    for span in line.spans {
        let style = span.style;
        let mut chunk = String::new();
        for ch in span.content.chars() {
            if col >= width {
                if !chunk.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut chunk), style));
                }
                out.push(Line::from(std::mem::take(&mut cur)));
                col = 0;
            }
            chunk.push(ch);
            col += 1;
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

/// Indent a rendered line by 3 columns (for the permission preview block).
pub(super) fn indent_line(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("   ")];
    spans.extend(line.spans);
    Line::from(spans)
}
