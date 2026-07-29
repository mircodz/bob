//! The ratatui TUI: retained view model + async draw loop. Wires the same
//! bob-core the CLI uses, but renders events into a pretty scrollback with
//! markdown, syntax-highlighted code, diffs, and pretty tool cells.

mod diffview;
mod files;
mod highlight;
mod input;
mod markdown;
mod render;
mod theme;
mod view;

use bob_core::agent::agent::{Agent, AgentConfig};
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
use bob_core::tools::registry::ToolRegistry;
use bob_core::tools::task::TaskTool;

use async_trait::async_trait;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Terminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use theme::Palette;
use tokio::sync::{mpsc, oneshot, Mutex};
use view::ViewModel;

/// Build a "shimmer" version of `text`: a bright band sweeps across the glyphs
/// like Cortex's working indicator. `tick` advances the wave (one step per
/// animation frame). Each character's brightness is a function of its distance
/// from the moving crest, interpolated between a dim base and a bright peak.
fn shimmer_spans(text: &str, tick: usize) -> Vec<Span<'static>> {
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

/// Return true if an event should reach the UI. All root-agent events pass;
/// from subagents we let ToolCall and TurnEnd through (the view uses them to
/// update the spawn cell's tool count + done state) but drop their text/results
/// so the transcript stays clean.
fn is_ui_event(e: &AgentEvent) -> bool {
    match e {
        AgentEvent::SubagentSpawn { .. } => true,
        // A spawned agent finishing — updates its cell's status (green/red).
        AgentEvent::SubagentDone { .. } => true,
        // Inter-agent coordination chatter is internal — never surfaced in the
        // user transcript (it's used by the receiving agent, not shown to you).
        AgentEvent::AgentMessage { .. } => false,
        // ToolCall/TurnEnd pass from any agent: the view routes subagent ones to
        // update the spawn cell's count/done state.
        AgentEvent::ToolCall { .. } | AgentEvent::TurnEnd { .. } => true,
        // Completion events (usage accounting) pass from every agent.
        AgentEvent::Completion { .. } => true,
        AgentEvent::TurnStart { agent_id }
        | AgentEvent::TextDelta { agent_id, .. }
        | AgentEvent::Message { agent_id, .. }
        | AgentEvent::ToolResult { agent_id, .. }
        | AgentEvent::Compaction { agent_id, .. }
        | AgentEvent::Error { agent_id, .. } => agent_id == "root",
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
            if is_ui_event(e) {
                let _ = evt_tx.send(e.clone());
            }
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

    let mut subagent_tools = ToolRegistry::new(Some(permissions.clone()));
    for t in bob_core::tools::builtin_tools() {
        subagent_tools.add(t);
    }
    for t in &mcp_tools {
        subagent_tools.add(t.clone());
    }
    if let Some(lsp) = &lsp {
        subagent_tools.add(Arc::new(bob_core::tools::lsp::LspTool::new(lsp.clone())));
        subagent_tools.add(Arc::new(
            bob_core::tools::lsp_actions::RenameSymbolTool::new(lsp.clone()),
        ));
        subagent_tools.add(Arc::new(bob_core::tools::lsp_actions::CodeActionTool::new(
            lsp.clone(),
        )));
    }
    // Spawned agents can message + inspect the team (cycle-free tools). Nested
    // spawning (spawn_agent inside a child) is added to the root's tools only;
    // the depth cap governs how deep it can go.
    subagent_tools.add(Arc::new(bob_core::tools::coordinate::SendMessageTool {
        team: team.clone(),
    }));
    subagent_tools.add(Arc::new(bob_core::tools::coordinate::ListAgentsTool {
        team: team.clone(),
    }));
    // Compose the system prompt: the base bob prompt (or a config override) plus
    // a live environment block and any AGENTS.md/CLAUDE.md project context.
    let system_prompt =
        bob_core::agent::prompt::build_system_prompt(config.system.as_deref(), &cwd);

    let mut tools = ToolRegistry::new(Some(permissions.clone()));
    for t in bob_core::tools::builtin_tools() {
        tools.add(t);
    }
    for t in &mcp_tools {
        tools.add(t.clone());
    }
    if let Some(lsp) = &lsp {
        tools.add(Arc::new(bob_core::tools::lsp::LspTool::new(lsp.clone())));
        tools.add(Arc::new(
            bob_core::tools::lsp_actions::RenameSymbolTool::new(lsp.clone()),
        ));
        tools.add(Arc::new(bob_core::tools::lsp_actions::CodeActionTool::new(
            lsp.clone(),
        )));
    }
    tools.add(Arc::new(TaskTool {
        provider: provider.clone(),
        subagent_tools: subagent_tools.clone(),
        bus: bus.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        subagent_system: Some(system_prompt.clone()),
        jobs: jobs.clone(),
        lsp: lsp.clone(),
    }));

    // Agent-coordination tools: spawn_agent (build coordinated children from the
    // same deps as the task tool), send_message, list_agents. Available to the
    // root and to spawned agents (children get them via subagent_tools), so any
    // team member can coordinate. Depth is enforced at runtime by spawn_agent.
    {
        use bob_core::tools::coordinate::{
            CoordDeps, ListAgentsTool, SendMessageTool, SpawnAgentTool,
        };
        let deps = CoordDeps {
            provider: provider.clone(),
            subagent_tools: subagent_tools.clone(),
            bus: bus.clone(),
            cwd: cwd.to_string_lossy().to_string(),
            subagent_system: Some(system_prompt.clone()),
            jobs: jobs.clone(),
            lsp: lsp.clone(),
            team: team.clone(),
        };
        tools.add(Arc::new(SpawnAgentTool { deps: deps.clone() }));
        tools.add(Arc::new(SendMessageTool { team: team.clone() }));
        tools.add(Arc::new(ListAgentsTool { team: team.clone() }));
    }

    // The root is itself a team member so spawned agents can report back to it by
    // name ("root"). It needs a mailbox registered in the team and wired as its
    // inbox, or children's result messages would have nowhere to land.
    let (root_inbox, root_tx) = bob_core::agent::team::mailbox();
    team.register("root".to_string(), 0, root_tx);

    let mut agent = Agent::new(AgentConfig {
        provider: provider.clone(),
        tools,
        bus: bus.clone(),
        system: Some(system_prompt.clone()),
        cwd: cwd.to_string_lossy().to_string(),
        max_turns: config.max_turns.unwrap_or(20),
        id: Some("root".to_string()),
        context_window: 200_000,
        compact_threshold: 0.8,
        keep_recent: 6,
        jobs: jobs.clone(),
        user_asker: Some(user_asker.clone()),
        lsp: lsp.clone(),
        inbox: Some(root_inbox),
        team: Some(team.clone()),
        name: "root".to_string(),
        depth: 0,
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
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(jobs.clone(), permissions.clone(), cancel.clone());
    app.provider_id = provider_spec.split(':').next().unwrap_or("").to_string();
    app.model_label = provider.model().to_string();
    app.cwd_label = abbreviate_home(&cwd);
    app.branch = git_branch(&cwd);
    app.lsp = lsp.clone();
    app.todos = Some(todos);
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

    let result = 'outer: loop {
        terminal.draw(|f| app.draw(f))?;

        tokio::select! {
            maybe_key = keys.next() => {
                match maybe_key {
                    Some(Ok(CtEvent::Key(key))) if key.kind != KeyEventKind::Release => {
                        match app.on_key(key.code, key.modifiers) {
                            KeyOutcome::Quit => break 'outer Ok(()),
                            KeyOutcome::Submit(text) => {
                                app.view.push_user(text.clone());
                                app.stick_to_bottom();
                                if app.running {
                                    // A turn is in flight — queue this for later.
                                    app.queue.push_back(text);
                                } else {
                                    app.running = true;
                                    app.current_prompt = Some(text.clone());
                                    app.turn_started = Some(std::time::Instant::now());
                                    spawn_turn(&agent, &app.turn_done_tx, text);
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
                                    match create_provider(&full).await {
                                        Ok(p) => {
                                            let id = full.split(':').next().unwrap_or("").to_string();
                                            let model = p.model().to_string();
                                            agent.lock().await.set_provider(p);
                                            app.provider_id = id;
                                            app.model_label = model.clone();
                                            // Chain into the reasoning picker so the
                                            // user sets model + effort together.
                                            let cur = app.reasoning;
                                            app.open_reasoning_picker(cur);
                                        }
                                        Err(e) => {
                                            app.view.push_notice(format!("error: {}", e));
                                        }
                                    }
                                    app.stick_to_bottom();
                                }
                            }
                            KeyOutcome::ListReasoning => {
                                let cur = app.reasoning;
                                app.open_reasoning_picker(cur);
                            }
                            KeyOutcome::SetReasoning(label) => {
                                let effort = bob_core::core::types::ReasoningEffort::parse(&label)
                                    .unwrap_or(bob_core::core::types::ReasoningEffort::Off);
                                app.reasoning = effort;
                                agent.lock().await.set_reasoning(effort);
                                app.view.push_event(format!(
                                    "Model changed to {} {}",
                                    app.model_label,
                                    effort.label()
                                ));
                                app.stick_to_bottom();
                            }
                            KeyOutcome::ListModels => {
                                // Fetch the current provider's models and open a
                                // picker showing bare model ids. The switch step
                                // recombines with the current provider id.
                                let prov = agent.lock().await.provider();
                                app.toast = Some("fetching models…".into());
                                match prov.list_models().await {
                                    Ok(models) => {
                                        // Backends that don't list (ChatGPT/Codex
                                        // Responses) fall back to the known set.
                                        let options: Vec<String> = if models.is_empty() {
                                            if app.provider_id == "openai" {
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
                                            app.toast = None;
                                            app.view.push_notice(
                                                "provider returned no models; use `/models <spec>`".into(),
                                            );
                                            app.stick_to_bottom();
                                        } else {
                                            app.pending_query = Some(PendingQuery {
                                                query: bob_core::tools::registry::UserQuery {
                                                    title: format!("Select a model (current: {})", app.model_label),
                                                    detail: String::new(),
                                                    options,
                                                    allow_other: true,
                                                },
                                                selected: 0,
                                                other_text: None,
                                                purpose: QueryPurpose::ModelPicker,
                                            });
                                            app.toast = None;
                                        }
                                    }
                                    Err(e) => {
                                        app.toast = None;
                                        app.view.push_notice(format!("error listing models: {}", e));
                                        app.stick_to_bottom();
                                    }
                                }
                            }
                            KeyOutcome::ShowContext => {
                                let a = agent.lock().await;
                                let used = bob_core::agent::compaction::estimate_history_tokens(a.messages());
                                let msgs = a.messages().len();
                                drop(a);
                                app.show_context(used, msgs);
                            }
                            KeyOutcome::None => {}
                        }
                    }
                    Some(Ok(CtEvent::Paste(text))) => app.input.paste(&text),
                    Some(Ok(CtEvent::Mouse(m))) => match m.kind {
                        MouseEventKind::ScrollUp => {
                            app.scroll_up = app.scroll_up.saturating_add(3);
                        }
                        MouseEventKind::ScrollDown => {
                            app.scroll_up = app.scroll_up.saturating_sub(3);
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            // A plain click sticks back to the bottom; Shift+drag
                            // is handled natively by the terminal for selection.
                            app.stick_to_bottom();
                        }
                        _ => {}
                    },
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break 'outer Ok(()),
                }
            }
            Some(evt) = evt_rx.recv() => {
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
                app.view.apply(&evt);
                app.stick_to_bottom();
            }
            Some(prompt) = perm_rx.recv() => {
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
                app.pending_query = Some(PendingQuery {
                    query: q.query,
                    selected: 0,
                    other_text: None,
                    purpose: QueryPurpose::Tool(q.resp),
                });
            }
            Some(err) = app.turn_done_rx.recv() => {
                // Surface a failed turn (provider error, etc.) instead of
                // silently ending — this is what made failures look like a hang.
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
                session.updated_at = now_stamp();
                let _ = save_session(&session);
                drop(a);
                // If this turn had been detached (Ctrl+B), close out its job.
                if let Some(job_id) = app.detached_job.take() {
                    app.jobs.finish(&job_id, bob_core::tools::jobs::JobStatus::Done, "turn finished".to_string());
                }
                app.current_prompt = None;
                // Dispatch the next queued prompt, if any.
                match app.queue.pop_front() {
                    Some(next) => {
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
            _ = ticker.tick() => {
                app.spinner = app.spinner.wrapping_add(1);
                // Coordination wake: if idle but a spawned agent has reported back,
                // drive a fresh empty-prompt turn so the root folds the result into
                // history and acts on it — an idle agent is re-driven by a new turn
                // rather than blocking.
                if !app.running {
                    let pending = {
                        let mut a = agent.lock().await;
                        a.has_pending_coordination()
                    };
                    if pending {
                        app.running = true;
                        app.turn_started = Some(std::time::Instant::now());
                        spawn_turn(&agent, &app.turn_done_tx, String::new());
                    }
                }
            }
        }
    };

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

enum KeyOutcome {
    None,
    Submit(String),
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
    /// Lines scrolled up from the bottom (0 = stuck to bottom).
    scroll_up: u16,
    spinner: usize,
    pending_perm: Option<PendingPerm>,
    /// Filtered slash-command menu, when the input starts with '/'.
    menu: Vec<(&'static str, &'static str)>,
    menu_sel: usize,
    /// `@file` completion: the full project file list (gathered lazily), the
    /// current filtered matches, and the selected index. `file_at` is the byte
    /// offset of the active `@` in the input buffer while completing.
    all_files: Vec<String>,
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
    /// Per-cell render cache: index → (cache key, rendered lines). The key folds
    /// the cell fingerprint, group flag, width, and theme generation, so a hit
    /// means the cell renders identically and we can skip markdown/highlighting.
    render_cache: Vec<(u64, Vec<Line<'static>>)>,
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
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        App {
            view: ViewModel::new(),
            input: input::Input::new(),
            scroll_up: 0,
            spinner: 0,
            pending_perm: None,
            menu: Vec::new(),
            menu_sel: 0,
            all_files: Vec::new(),
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
            turn_started: None,
            cwd_label: String::new(),
            branch: None,
            lsp: None,
            todos: None,
            theme_name: "dark".to_string(),
            render_cache: Vec::new(),
        }
    }

    fn stick_to_bottom(&mut self) {
        self.scroll_up = 0;
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

    /// Update the `@file` completion menu from the token under the cursor.
    fn refresh_file_menu(&mut self) {
        match self.input.at_token() {
            Some((start, query)) => {
                // Lazily gather the project file list on first use.
                if self.all_files.is_empty() {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
                    self.all_files = files::gather_files(&cwd);
                }
                self.file_at = Some(start);
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

    fn on_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyOutcome {
        // 0) Shift+Tab cycles the interaction mode (normal → auto-accept → plan).
        if code == KeyCode::BackTab {
            let next = self.permissions.mode().next();
            self.permissions.set_mode(next);
            return KeyOutcome::None;
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
            (KeyCode::Esc, _) => {
                // Esc interrupts a running turn (cooperative). Only meaningful
                // while busy — otherwise it's ignored (prompts handle Esc above).
                if self.running {
                    self.interrupt();
                }
                return KeyOutcome::None;
            }
            (KeyCode::PageUp, _) => {
                self.scroll_up = self.scroll_up.saturating_add(10);
                return KeyOutcome::None;
            }
            (KeyCode::PageDown, _) => {
                self.scroll_up = self.scroll_up.saturating_sub(10);
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
                // Shift+Enter / Alt+Enter insert a newline (multi-line prompts).
                if mods.contains(KeyModifiers::SHIFT) || mods.contains(KeyModifiers::ALT) {
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
                if let Some(out) = self.handle_command(&display) {
                    return out;
                }
                KeyOutcome::Submit(text)
            }
            KeyCode::Up => {
                self.input.history_prev();
                KeyOutcome::None
            }
            KeyCode::Down => {
                self.input.history_next();
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
                self.view.cells.clear();
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

    /// Print a token-usage summary (session + all-time) as notice cells. A full
    /// heatmap/graph dashboard can replace this later, but the data is here now.
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
                osc52_copy(&text);
                self.toast = Some("copied last message".into());
            }
            None => self.toast = Some("nothing to copy".into()),
        }
    }

    /// Build the fully wrapped, prompt-prefixed display lines for the input box,
    /// given the usable text width. Used for BOTH the height calc and rendering
    /// so they never disagree (which is what clipped long lines).
    fn input_lines(&self, width: usize, busy: bool) -> Vec<Line<'static>> {
        let prompt = || {
            Span::styled(
                "› ",
                Style::default()
                    .fg(Palette::ACCENT())
                    .add_modifier(Modifier::BOLD),
            )
        };
        if self.input.text().is_empty() && !busy {
            return vec![Line::from(vec![
                prompt(),
                Span::styled(
                    "send a message...  (Ctrl+J or Shift+Enter for newline)",
                    Style::default().fg(Palette::FAINT()),
                ),
            ])];
        }
        let mut out: Vec<Line<'static>> = Vec::new();
        for (i, l) in self.input.display_lines().iter().enumerate() {
            let prefix = if i == 0 {
                prompt()
            } else {
                Span::styled("  ", Style::default())
            };
            let logical = Line::from(vec![
                prefix,
                Span::styled(l.to_string(), Style::default().fg(Palette::TEXT())),
            ]);
            // Wrap each logical line so long content (pasted code) never clips.
            for wl in wrap_line(logical, width.max(1)) {
                out.push(wl);
            }
        }
        out
    }

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let area = f.area();
        // Force the theme's base background across the whole screen so bob looks
        // identical regardless of the terminal's own background. Themes that want
        // to inherit the terminal use Color::Reset here (a no-op paint).
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::BG())),
            area,
        );
        // Codex-style input band: grows with the number of *wrapped* text lines
        // (1 pad row above + N text rows + 1 pad row below), capped so it can't
        // eat the whole screen.
        let text_width = area.width.saturating_sub(2) as usize;
        let wrapped = self
            .input_lines(text_width, self.running || self.view.busy)
            .len();
        let text_rows = (wrapped as u16).clamp(1, 10);
        let input_height = text_rows + 2;

        // The band above the input shows either a permission prompt or a user
        // question (they don't co-occur), sized to its content.
        let prompt_height = if self.pending_perm.is_some() {
            (self.permission_lines(area.width as usize).len() as u16 + 1).min(24)
        } else if self.pending_query.is_some() {
            (self.query_lines(area.width as usize).len() as u16 + 1).min(24)
        } else {
            0
        };

        // A pinned background-jobs panel sits just above the input when any
        // jobs exist (one row per job + a header).
        let job_rows = self.jobs.list();
        let jobs_height = if job_rows.is_empty() {
            0
        } else {
            (job_rows.len() as u16 + 1).min(8)
        };

        // A sticky todo panel sits just above the input while the list is
        // non-empty (one row per item + a header), capped so it can't dominate.
        let todo_items = self.todos.as_ref().map(|t| t.items()).unwrap_or_default();
        let todos_height = if todo_items.is_empty() {
            0
        } else {
            // header + one blank line of padding above and below.
            (todo_items.len() as u16 + 3).min(14)
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(prompt_height),
                Constraint::Length(todos_height),
                Constraint::Length(jobs_height),
                Constraint::Length(input_height),
                Constraint::Length(1), // status bar below the input
            ])
            .split(area);

        self.draw_scrollback(f, chunks[0]);
        if self.pending_perm.is_some() {
            self.draw_permission(f, chunks[1]);
        } else if self.pending_query.is_some() {
            self.draw_query(f, chunks[1]);
        }
        if todos_height > 0 {
            self.draw_todos(f, chunks[2], &todo_items);
        }
        if jobs_height > 0 {
            self.draw_jobs(f, chunks[3], &job_rows);
        }
        self.draw_input(f, chunks[4]);
        self.draw_status_bar(f, chunks[5]);

        if !self.menu.is_empty() {
            self.draw_menu(f, chunks[4]);
        }
        if !self.file_menu.is_empty() {
            self.draw_file_menu(f, chunks[4]);
        }
        if let Some(toast) = self.toast.clone() {
            self.draw_toast(f, area, &toast);
        }
    }

    /// Sticky todo checklist above the input: a header with the done/total count,
    /// then one row per item — ☐ pending (dim), ◐ in-progress (accent, bold), ✓
    /// done (green, struck-through-ish via dim).
    fn draw_todos(
        &self,
        f: &mut ratatui::Frame,
        area: Rect,
        items: &[bob_core::tools::todo::TodoItem],
    ) {
        use bob_core::tools::todo::TodoStatus;
        f.render_widget(Clear, area);
        let done = items
            .iter()
            .filter(|i| i.status == TodoStatus::Completed)
            .count();
        let in_progress = items
            .iter()
            .filter(|i| i.status == TodoStatus::InProgress)
            .count();
        let open = items.len() - done - in_progress;
        let header = format!(
            "{} task{} ({} done, {} in progress, {} open)",
            items.len(),
            if items.len() == 1 { "" } else { "s" },
            done,
            in_progress,
            open,
        );
        let mut lines: Vec<Line> = vec![
            Line::from(""),
            Line::from(Span::styled(
                format!("  {}", header),
                Style::default().fg(Palette::DIM()),
            )),
        ];
        for item in items
            .iter()
            .take(area.height.saturating_sub(3) as usize)
        {
            let (glyph, glyph_color, text_style) = match item.status {
                TodoStatus::Pending => {
                    ("[ ]", Palette::FAINT(), Style::default().fg(Palette::DIM()))
                }
                TodoStatus::InProgress => (
                    "[~]",
                    Palette::ACCENT(),
                    Style::default()
                        .fg(Palette::TEXT())
                        .add_modifier(Modifier::BOLD),
                ),
                TodoStatus::Completed => {
                    ("[x]", Palette::OK(), Style::default().fg(Palette::DIM()))
                }
            };
            let width = area.width.saturating_sub(8) as usize;
            let text: String = if item.label().chars().count() > width {
                format!(
                    "{}…",
                    item.label()
                        .chars()
                        .take(width.saturating_sub(1))
                        .collect::<String>()
                )
            } else {
                item.label().to_string()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", glyph), Style::default().fg(glyph_color)),
                Span::styled(text, text_style),
            ]));
        }
        lines.push(Line::from(""));
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            area,
        );
    }

    fn draw_jobs(
        &self,
        f: &mut ratatui::Frame,
        area: Rect,
        jobs: &[(String, String, String, bob_core::tools::jobs::JobStatus)],
    ) {
        use bob_core::tools::jobs::JobStatus;
        f.render_widget(Clear, area);
        let running = jobs
            .iter()
            .filter(|(_, _, _, s)| *s == JobStatus::Running)
            .count();
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            format!(" background jobs · {} running ", running),
            Style::default()
                .fg(Palette::ACCENT())
                .add_modifier(Modifier::BOLD),
        ))];
        for (id, kind, desc, status) in jobs.iter().take(area.height.saturating_sub(1) as usize) {
            let (glyph, color) = match status {
                JobStatus::Running => ("•", Palette::RUNNING()),
                JobStatus::Done => ("•", Palette::OK()),
                JobStatus::Failed => ("•", Palette::ERROR()),
                JobStatus::Cancelled => ("•", Palette::FAINT()),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {} ", glyph), Style::default().fg(color)),
                Span::styled(format!("{} ", id), Style::default().fg(Palette::DIM())),
                Span::styled(
                    format!("[{}] ", kind),
                    Style::default().fg(Palette::FAINT()),
                ),
                Span::styled(
                    truncate_mid(desc, area.width as usize / 2),
                    Style::default().fg(Palette::TEXT()),
                ),
            ]));
        }
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            area,
        );
    }

    fn draw_scrollback(&mut self, f: &mut ratatui::Frame, full: Rect) {
        // Content is inset with a small left/right margin, EXCEPT the user-message
        // band which spans full width (rendered against `full.width`). Non-user
        // cells get the inset via a left pad inside their lines... simplest: we
        // render at full width and inset all lines that aren't the user band.
        let area = full;

        let mut raw: Vec<Line> = Vec::new();
        // Parallel to `raw`: whether each line should get the 2-col non-user
        // inset (applied after wrapping so continuation lines stay aligned).
        let mut inset_flags: Vec<bool> = Vec::new();
        let width_full = full.width as usize;
        let theme_gen = theme::generation();
        let cells = &self.view.cells;
        // Keep the cache index-aligned with the cell list.
        if self.render_cache.len() != cells.len() {
            self.render_cache.resize(cells.len(), (0, Vec::new()));
        }
        for (i, cell) in cells.iter().enumerate() {
            let is_user = matches!(cell, view::Cell::User(_));

            // Cache key: content + width + theme generation. A hit means this cell
            // renders identically to last frame, so we reuse its Lines and skip
            // markdown/syntax-highlighting entirely.
            let key = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                cell.fingerprint().hash(&mut h);
                width_full.hash(&mut h);
                theme_gen.hash(&mut h);
                h.finish()
            };
            let slot = &mut self.render_cache[i];
            if slot.0 != key {
                let mut rendered = Vec::new();
                render::render_cell(cell, width_full, &mut rendered);
                *slot = (key, rendered);
            }
            for line in &slot.1 {
                raw.push(line.clone());
                inset_flags.push(!is_user);
            }
        }

        // While a turn runs, append a live "Working" line (Cortex-style): a
        // shimmering label with elapsed seconds and the interrupt hint. It's not
        // a stored cell — it's transient, regenerated each frame.
        if self.running || self.view.busy {
            let secs = self
                .turn_started
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let mut spans: Vec<Span> = vec![Span::styled(
                "  • ",
                Style::default().fg(Palette::RUNNING()),
            )];
            spans.extend(shimmer_spans("Working", self.spinner));
            spans.push(Span::styled(
                format!(" ({}s · esc to interrupt)", secs),
                Style::default().fg(Palette::FAINT()),
            ));
            raw.push(Line::from(spans));
            inset_flags.push(false); // already carries its own leading spaces
        }

        // Manually wrap into display lines so scroll math is exact and wide
        // content (tables/code) is clipped rather than reflowed mid-border.
        // Non-user lines get a 2-col hanging indent: we wrap at width-2 then
        // prefix EVERY wrapped line (including continuations) with the pad, so
        // wrapped text stays aligned under its first line.
        let width = area.width.max(1) as usize;
        let mut lines: Vec<Line> = Vec::new();
        for (idx, l) in raw.into_iter().enumerate() {
            let inset = inset_flags.get(idx).copied().unwrap_or(false);
            let wrap_width = if inset {
                width.saturating_sub(2).max(1)
            } else {
                width
            };
            for mut wl in wrap_line(l, wrap_width) {
                if inset {
                    wl.spans.insert(0, Span::raw("  "));
                }
                lines.push(wl);
            }
        }

        let viewport = area.height as usize;
        let total = lines.len();
        let max_scroll = total.saturating_sub(viewport);
        self.scroll_up = self.scroll_up.min(max_scroll as u16);
        let start = max_scroll.saturating_sub(self.scroll_up as usize);
        let window: Vec<Line> = lines.into_iter().skip(start).take(viewport).collect();

        f.render_widget(Paragraph::new(window), area);

        // Scroll hint when not at the bottom.
        if self.scroll_up > 0 {
            let hint = Rect {
                x: area.x + area.width.saturating_sub(10),
                y: area.y,
                width: 10,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    " ↑ scrolled ",
                    Style::default().fg(Palette::WARN()).bg(Palette::POPUP_BG()),
                )),
                hint,
            );
        }
    }

    /// A one-line status bar below the input: cwd · branch · mode. Colored,
    /// space for LSP status later.
    fn draw_status_bar(&self, f: &mut ratatui::Frame, area: Rect) {
        const PAD: u16 = 3;
        let sep = || Span::styled("  ", Style::default().fg(Palette::FAINT()));
        let mut spans: Vec<Span> = Vec::new();

        // cwd
        if !self.cwd_label.is_empty() {
            spans.push(Span::styled(
                self.cwd_label.clone(),
                Style::default().fg(Palette::ACCENT()),
            ));
        }
        // git branch
        if let Some(b) = &self.branch {
            spans.push(sep());
            spans.push(Span::styled(
                format!("\u{2387} {b}"), // ⎇ branch glyph
                Style::default().fg(Palette::LINK()),
            ));
        }
        // interaction mode (color-coded; normal is dim, others pop)
        let mode = self.permissions.mode();
        let (mode_text, mode_color) = match mode {
            Mode::Normal => ("normal", Palette::DIM()),
            Mode::AutoAccept => ("auto-accept", Palette::OK()),
            Mode::Plan => ("plan", Palette::WARN()),
        };
        spans.push(sep());
        spans.push(Span::styled(mode_text, Style::default().fg(mode_color)));

        // LSP health: one colored dot + name per configured server. Starting is
        // dim, Indexing amber (with % when known), Ready green, Failed red.
        if let Some(lsp) = &self.lsp {
            for (name, health) in lsp.statuses() {
                use bob_core::lsp::Health;
                let (glyph, label, color) = match health {
                    Health::Starting => ("\u{25CB}".to_string(), name.clone(), Palette::DIM()),
                    Health::Indexing(Some(p)) => (
                        "\u{25D0}".to_string(),
                        format!("{name} {p}%"),
                        Palette::WARN(),
                    ),
                    Health::Indexing(None) => {
                        ("\u{25D0}".to_string(), name.clone(), Palette::WARN())
                    }
                    Health::Ready => ("\u{25CF}".to_string(), name.clone(), Palette::OK()),
                    Health::Failed(_) => ("\u{25CF}".to_string(), name.clone(), Palette::ERROR()),
                };
                spans.push(sep());
                spans.push(Span::styled(
                    format!("{glyph} {label}"),
                    Style::default().fg(color),
                ));
            }
        }

        let bar = Rect {
            x: area.x + PAD,
            y: area.y,
            width: area.width.saturating_sub(PAD * 2),
            height: 1,
        };
        f.render_widget(Paragraph::new(Line::from(spans)), bar);
    }

    fn draw_input(&mut self, f: &mut ratatui::Frame, area: Rect) {
        // Full-width band with a lighter background; no border. One blank row of
        // padding above (with status) and below; the middle grows with lines.
        let bg = Block::default().style(Style::default().bg(Palette::INPUT_BG()));
        f.render_widget(bg, area);

        let busy = self.running || self.view.busy;
        // Status sits on the top padding row, right-aligned. The live "Working"
        // indicator lives in the transcript now; here we just note a non-default
        // mode and any queued prompts.
        // Mode now lives in the status bar below the input; the top row only
        // notes queued prompts while a turn is running.
        let status_line = if busy && !self.queue.is_empty() {
            Line::from(Span::styled(
                format!("{} queued ", self.queue.len()),
                Style::default().fg(Palette::DIM()),
            ))
        } else {
            Line::from("")
        };
        // Horizontal breathing room inside the input band.
        const PAD: u16 = 3;
        let status_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width.saturating_sub(PAD),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(status_line)
                .alignment(ratatui::layout::Alignment::Right)
                .style(Style::default().bg(Palette::INPUT_BG())),
            status_area,
        );

        // Text area: rows between the top and bottom pad rows, inset by PAD.
        let text_rows = area.height.saturating_sub(2).max(1);
        let text_area = Rect {
            x: area.x + PAD,
            y: area.y + 1,
            width: area.width.saturating_sub(PAD * 2),
            height: text_rows,
        };

        // Wrapped, prompt-prefixed lines (same builder as the height calc). Show
        // the tail if the content is taller than the visible rows.
        let all = self.input_lines(text_area.width as usize, busy);
        let visible: Vec<Line> = if all.len() > text_rows as usize {
            all[all.len() - text_rows as usize..].to_vec()
        } else {
            all
        };
        f.render_widget(
            Paragraph::new(visible).style(Style::default().bg(Palette::INPUT_BG())),
            text_area,
        );

        // Cursor at its row/col (accounting for the 2-col prompt/indent prefix).
        let (row, col) = self.input.cursor_row_col();
        let row = row.min(text_rows.saturating_sub(1) as usize) as u16;
        let cursor_x = text_area.x + 2 + col as u16;
        if cursor_x < text_area.x + text_area.width {
            f.set_cursor_position((cursor_x, text_area.y + row));
        }
    }

    fn draw_menu(&mut self, f: &mut ratatui::Frame, input_area: Rect) {
        // Borderless select, matching the permission prompt. Sits directly above
        // the input band; no box, no background fill.
        let h = self.menu.len() as u16;
        if h == 0 {
            return;
        }
        let width = input_area.width;
        let area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(h),
            width,
            height: h,
        };
        f.render_widget(Clear, area);
        // Fill with the theme popup background so no terminal-default color shows
        // through under a forced-background theme.
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::POPUP_BG())),
            area,
        );

        let lines: Vec<Line> = self
            .menu
            .iter()
            .enumerate()
            .map(|(i, (cmd, desc))| {
                let selected = i == self.menu_sel;
                let row_bg = if selected {
                    Palette::SELECTED_BG()
                } else {
                    Palette::POPUP_BG()
                };
                let marker = if selected { "❯" } else { " " };
                let cmd_style = if selected {
                    Style::default()
                        .fg(Palette::ACCENT())
                        .bg(row_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Palette::TEXT()).bg(row_bg)
                };
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", marker),
                        Style::default().fg(Palette::ACCENT()).bg(row_bg),
                    ),
                    Span::styled(format!("{:<8}", cmd), cmd_style),
                    Span::styled(
                        format!("  {}", desc),
                        Style::default().fg(Palette::FAINT()).bg(row_bg),
                    ),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::POPUP_BG())),
            area,
        );
    }

    /// The `@file` completion popup — same borderless style as the slash menu.
    fn draw_file_menu(&mut self, f: &mut ratatui::Frame, input_area: Rect) {
        let h = self.file_menu.len() as u16;
        if h == 0 {
            return;
        }
        let area = Rect {
            x: input_area.x,
            y: input_area.y.saturating_sub(h),
            width: input_area.width,
            height: h,
        };
        f.render_widget(Clear, area);
        f.render_widget(
            Block::default().style(Style::default().bg(Palette::POPUP_BG())),
            area,
        );

        let lines: Vec<Line> = self
            .file_menu
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let selected = i == self.file_sel;
                let row_bg = if selected {
                    Palette::SELECTED_BG()
                } else {
                    Palette::POPUP_BG()
                };
                let marker = if selected { "❯" } else { " " };
                let path_style = if selected {
                    Style::default()
                        .fg(Palette::ACCENT())
                        .bg(row_bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Palette::TEXT()).bg(row_bg)
                };
                Line::from(vec![
                    Span::styled(
                        format!(" {} ", marker),
                        Style::default().fg(Palette::ACCENT()).bg(row_bg),
                    ),
                    Span::styled(path.clone(), path_style),
                ])
            })
            .collect();
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::POPUP_BG())),
            area,
        );
    }

    /// Build all lines for the permission prompt: title, optional preview diff,
    /// numbered options, and the hint. Shared by height calc + render.
    fn permission_lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(p) = &self.pending_perm else {
            return vec![];
        };
        let mut lines: Vec<Line> = Vec::new();

        // Title line, e.g. "Allow write_file?" with the target dimmed after it.
        let mut title_spans = vec![Span::styled(
            p.title.clone(),
            Style::default()
                .fg(Palette::WARN())
                .add_modifier(Modifier::BOLD),
        )];
        if !p.detail.is_empty() {
            title_spans.push(Span::styled(
                format!(
                    "  {}",
                    truncate_mid(&p.detail, width.saturating_sub(p.title.len() + 2))
                ),
                Style::default().fg(Palette::DIM()),
            ));
        }
        lines.push(Line::from(title_spans));

        // Preview: render the ```diff / ```lang block the tool produced. Capped
        // so a huge edit doesn't push the options off-screen.
        if let Some(preview) = &p.preview {
            let rendered = render::render_markdown_like(preview);
            let cap = 14usize;
            for l in rendered.iter().take(cap) {
                lines.push(indent_line(l.clone()));
            }
            if rendered.len() > cap {
                lines.push(Line::from(Span::styled(
                    format!("   ... {} more diff lines", rendered.len() - cap),
                    Style::default().fg(Palette::FAINT()),
                )));
            }
            lines.push(Line::from(""));
        }

        for (i, opt) in p.options.iter().enumerate() {
            let selected = i == p.selected;
            let marker = if selected { "❯" } else { " " };
            let base = if opt.allow {
                if opt.grant.is_some() {
                    Palette::OK()
                } else {
                    Palette::TEXT()
                }
            } else {
                Palette::ERROR()
            };
            let label_style = if selected {
                Style::default().fg(base).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(base)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", marker),
                    Style::default().fg(Palette::WARN()),
                ),
                Span::styled(format!("{}. ", i + 1), Style::default().fg(Palette::DIM())),
                Span::styled(opt.label.clone(), label_style),
            ]));
        }
        lines.push(Line::from(Span::styled(
            "↑↓ move · 1-9 pick · enter confirm · esc deny",
            Style::default().fg(Palette::FAINT()),
        )));
        lines
    }

    fn draw_permission(&mut self, f: &mut ratatui::Frame, area: Rect) {
        if self.pending_perm.is_none() {
            return;
        }
        f.render_widget(Clear, area);
        let lines = self.permission_lines(area.width as usize);
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            area,
        );
    }

    /// Build the lines for a user question (ask_user / exit_plan): the question,
    /// optional Markdown detail (e.g. the plan), then a numbered select with an
    /// "Other…" row, or a free-text field when the user chose Other.
    fn query_lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(q) = &self.pending_query else {
            return vec![];
        };
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            q.query.title.clone(),
            Style::default()
                .fg(Palette::ACCENT())
                .add_modifier(Modifier::BOLD),
        )));
        if !q.query.detail.is_empty() {
            for l in render::render_markdown_like(&q.query.detail)
                .into_iter()
                .take(12)
            {
                lines.push(indent_line(l));
            }
        }
        lines.push(Line::from(""));

        // Free-text entry mode.
        if let Some(buf) = &q.other_text {
            lines.push(Line::from(vec![
                Span::styled(" > ", Style::default().fg(Palette::ACCENT())),
                Span::styled(buf.clone(), Style::default().fg(Palette::TEXT())),
                Span::styled("_", Style::default().fg(Palette::FAINT())),
            ]));
            lines.push(Line::from(Span::styled(
                "type your answer · enter to send · esc to go back",
                Style::default().fg(Palette::FAINT()),
            )));
            return lines;
        }

        // Build the full row list (options + optional "Other"), then window it
        // around the selection so a long list (e.g. 39 models) stays visible and
        // never hides the selected row behind the input.
        let n_opts = q.query.options.len();
        let total_rows = n_opts + if q.query.allow_other { 1 } else { 0 };
        let mut rows: Vec<(usize, String, bool)> = Vec::with_capacity(total_rows);
        for (i, opt) in q.query.options.iter().enumerate() {
            rows.push((i, opt.clone(), false));
        }
        if q.query.allow_other {
            rows.push((n_opts, "Other…".to_string(), true));
        }

        const VISIBLE: usize = 10;
        let start = if total_rows <= VISIBLE {
            0
        } else if q.selected < VISIBLE / 2 {
            0
        } else if q.selected >= total_rows - VISIBLE / 2 {
            total_rows - VISIBLE
        } else {
            q.selected - VISIBLE / 2
        };
        let end = (start + VISIBLE).min(total_rows);

        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("   ↑ {} more", start),
                Style::default().fg(Palette::FAINT()),
            )));
        }
        for (i, label, is_other) in &rows[start..end] {
            let selected = *i == q.selected;
            let marker = if selected { "❯" } else { " " };
            let base = if *is_other {
                Palette::DIM()
            } else {
                Palette::TEXT()
            };
            let style = if selected {
                Style::default().fg(base).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(base)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", marker),
                    Style::default().fg(Palette::ACCENT()),
                ),
                Span::styled(format!("{}. ", i + 1), Style::default().fg(Palette::DIM())),
                Span::styled(label.clone(), style),
            ]));
        }
        if end < total_rows {
            lines.push(Line::from(Span::styled(
                format!("   ↓ {} more", total_rows - end),
                Style::default().fg(Palette::FAINT()),
            )));
        }
        lines.push(Line::from(Span::styled(
            "↑↓ move · enter confirm · esc dismiss",
            Style::default().fg(Palette::FAINT()),
        )));
        let _ = width;
        lines
    }

    fn draw_query(&mut self, f: &mut ratatui::Frame, area: Rect) {
        if self.pending_query.is_none() {
            return;
        }
        f.render_widget(Clear, area);
        let lines = self.query_lines(area.width as usize);
        f.render_widget(
            Paragraph::new(lines).style(Style::default().bg(Palette::BG())),
            area,
        );
    }

    fn draw_toast(&mut self, f: &mut ratatui::Frame, area: Rect, text: &str) {
        let w = (text.len() as u16 + 4).clamp(10, area.width);
        let toast = Rect {
            x: area.x + area.width.saturating_sub(w).saturating_sub(1),
            y: area.y + 1,
            width: w,
            height: 1,
        };
        f.render_widget(Clear, toast);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(" {} ", text),
                Style::default()
                    .fg(Palette::TEXT())
                    .bg(Palette::SELECTED_BG()),
            )),
            toast,
        );
    }
}

/// Word-aware wrap of a styled Line into <= width display lines, splitting
/// spans at boundaries and preserving each span's style. Over-long single
/// tokens are hard-broken.
fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
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

fn truncate_mid(s: &str, max: usize) -> String {
    if s.chars().count() <= max || max < 4 {
        return s.to_string();
    }
    let keep = max - 1;
    let head: String = s.chars().take(keep).collect();
    format!("{}...", head)
}

/// Indent a rendered line by 3 columns (for the permission preview block).
fn indent_line(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::raw("   ")];
    spans.extend(line.spans);
    Line::from(spans)
}

/// Copy `text` to the system clipboard via the OSC 52 terminal escape. This
/// hands the clipboard to the terminal emulator (no X11/Wayland client, no
/// blocking), which also means it works over SSH. Silently no-ops if stdout
/// isn't writable.
fn osc52_copy(text: &str) {
    use std::io::Write;
    let b64 = base64_encode(text.as_bytes());
    let seq = format!("\x1b]52;c;{}\x07", b64);
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// Minimal standard base64 encoder (no external crate).
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
