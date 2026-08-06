//! The agent host. Builds a bob-core Agent via the shared
//! `agent::assembly::build_root_agent` (the same wiring the TUI uses), but
//! bridges the two seams over a WebSocket to the relay instead of a terminal:
//!   - EventBus listener  -> HostFrame::Event   (outbound)
//!   - RemoteUserAsker / RemoteAsker  <-> AskQuery / AskPermission round-trips
//!   - incoming ControlFrame::Prompt/Cancel/SetMode drive the agent

use std::collections::HashMap;
use std::sync::Arc;

use crate::session;

use async_trait::async_trait;
use base64::Engine as _;
use bob_core::core::config::load_config;
use bob_core::core::events::{AgentEvent, EventBus};
use bob_core::core::permissions::{
    Asker, Decision, Mode, PermissionEngine, PermissionOption, PermissionRequest,
};
use bob_core::core::policies::{
    allow_bash_commands, allow_code_action_list, allow_read_only, allow_tools, deny_tools,
    flag_dangerous_bash,
};
use bob_core::providers::create_provider;
use bob_core::tools::registry::{UserAsker, UserQuery};
use bob_protocol::{
    AgentEventDto, ControlFrame, Hello, HostFrame, PermissionOptDto, PermissionReqDto, UserQueryDto,
};
use futures::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Pending interaction requests keyed by id, so the matching answer frame can
/// resolve the parked oneshot.
type Pending<T> = Arc<Mutex<HashMap<String, oneshot::Sender<T>>>>;

/// Accumulates subagent tool activity for the current turn, so it can be
/// persisted into the session (subagent tools are not part of root history).
#[derive(Default)]
struct SubAccum {
    /// Completed runs this turn, in order.
    runs: Vec<bob_core::core::session::SubagentRun>,
    /// The run currently being built (bound to the latest `task` tool_use_id).
    current: Option<bob_core::core::session::SubagentRun>,
}

impl SubAccum {
    fn observe(&mut self, e: &AgentEvent) {
        use bob_core::core::session::{PersistedSubagent, PersistedTool, SubagentRun};
        match e {
            // A new `task` call starts a fresh run bound to its tool_use_id.
            AgentEvent::ToolCall {
                agent_id,
                tool_use_id,
                name,
                ..
            } if agent_id == "root" && name == "task" => {
                if let Some(done) = self.current.take() {
                    self.runs.push(done);
                }
                self.current = Some(SubagentRun {
                    task_use_id: tool_use_id.clone(),
                    subagents: Vec::new(),
                });
            }
            // A subagent spawned → add it to the current run.
            AgentEvent::SubagentSpawn { agent_id, task, .. } => {
                if let Some(run) = self.current.as_mut() {
                    run.subagents.push(PersistedSubagent {
                        id: agent_id.clone(),
                        task: task.clone(),
                        tools: Vec::new(),
                    });
                }
            }
            // A tool call inside a subagent → record it.
            AgentEvent::ToolCall {
                agent_id,
                tool_use_id,
                name,
                input,
            } if agent_id != "root" => {
                if let Some(sub) = self.find_sub(agent_id) {
                    sub.tools.push(PersistedTool {
                        id: tool_use_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        output: String::new(),
                        is_error: false,
                    });
                }
            }
            // A subagent tool result → attach output.
            AgentEvent::ToolResult {
                agent_id,
                tool_use_id,
                output,
                is_error,
            } if agent_id != "root" => {
                if let Some(sub) = self.find_sub(agent_id) {
                    if let Some(t) = sub.tools.iter_mut().find(|t| &t.id == tool_use_id) {
                        t.output = output.clone();
                        t.is_error = *is_error;
                    }
                }
            }
            _ => {}
        }
    }

    fn find_sub(
        &mut self,
        agent_id: &str,
    ) -> Option<&mut bob_core::core::session::PersistedSubagent> {
        self.current
            .as_mut()?
            .subagents
            .iter_mut()
            .find(|s| s.id == agent_id)
    }

    /// Finalize: fold the in-progress run and return all runs, resetting state.
    fn take_all(&mut self) -> Vec<bob_core::core::session::SubagentRun> {
        if let Some(done) = self.current.take() {
            self.runs.push(done);
        }
        std::mem::take(&mut self.runs)
    }
}

/// Queues a HostFrame for the writer task, which seals it into the E2E channel.
#[derive(Clone)]
struct Outbound(mpsc::UnboundedSender<HostFrame>);
impl Outbound {
    fn send(&self, frame: HostFrame) {
        if self.0.send(frame).is_err() {
            eprintln!("[host] OUT DROPPED (writer gone)");
        }
    }
}

/// UserAsker impl (ask_user / exit_plan) that round-trips over the relay.
struct RemoteUserAsker {
    out: Outbound,
    pending: Pending<Option<String>>,
}

#[async_trait]
impl UserAsker for RemoteUserAsker {
    async fn ask(&self, query: &UserQuery) -> Option<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        self.out.send(HostFrame::AskQuery {
            id,
            query: UserQueryDto::from(query),
        });
        rx.await.unwrap_or(None)
    }
}

/// Permission Asker impl that round-trips over the relay.
struct RemoteAsker {
    out: Outbound,
    pending: Pending<Option<usize>>,
}

#[async_trait]
impl Asker for RemoteAsker {
    async fn ask(&self, req: &PermissionRequest, options: &[PermissionOption]) -> Option<usize> {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), tx);
        self.out.send(HostFrame::AskPermission {
            id: id.clone(),
            request: PermissionReqDto::from(req),
            options: options.iter().map(PermissionOptDto::from).collect(),
        });
        rx.await.unwrap_or(None)
    }
}

pub async fn run(
    relay: String,
    session: String,
    secure: crate::SecureParams,
    provider_override: Option<String>,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = load_config(&cwd)?;
    let provider_spec = provider_override.unwrap_or_else(|| config.provider.clone());
    eprintln!("[host] cwd={} provider={}", cwd.display(), provider_spec);
    let provider = create_provider(&provider_spec).await?;
    eprintln!("[host] provider ready");

    // Connect to the relay, present the admission proof, then run the responder
    // side of the Noise-XK handshake. After this, every frame is sealed.
    let (ws, _) = tokio_tungstenite::connect_async(&relay).await?;
    let (mut ws_sink, mut ws_stream) = ws.split();
    let admission = bob_secure::admission::prove(&secure.pairing_secret, &session);
    let hello = serde_json::to_string(&Hello::Host {
        session: session.clone(),
        admission: base64::engine::general_purpose::STANDARD_NO_PAD.encode(&admission),
    })?;
    ws_sink.send(WsMessage::Text(hello)).await?;
    eprintln!("[host] connected to relay, session '{}'", session);

    let pro = crate::channel::prologue(&relay, &session);
    let est = crate::channel::handshake_responder(
        &mut ws_sink,
        &mut ws_stream,
        &pro,
        secure.identity,
        secure.ephemeral,
    )
    .await?;
    eprintln!(
        "[host] secure channel established · safety number {:04}",
        est.safety_number
    );
    // Authorize the peer: the callback does trust-on-first-use bookkeeping and
    // returns whether this device is allowed. An unauthorized key is rejected here,
    // before any agent is assembled — the E2E channel proves who they are, but only
    // a trusted identity gets a session.
    if !(secure.on_established)(&est) {
        eprintln!("[host] rejecting untrusted peer · closing connection");
        let _ = ws_sink.close().await;
        anyhow::bail!("peer not trusted");
    }
    let mut opener = est.opener;

    // Outbound queue: everything the host sends funnels here, gets sealed, and is
    // written to the relay by the writer task (which owns the Sealer).
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<HostFrame>();
    let mut sealer = est.sealer;
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let plaintext = match serde_json::to_vec(&frame) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[host] OUT SERIALIZE FAILED: {e}");
                    continue;
                }
            };
            match crate::channel::seal_envelope(&mut sealer, &plaintext) {
                Ok(text) => {
                    if ws_sink.send(WsMessage::Text(text)).await.is_err() {
                        break;
                    }
                }
                Err(e) => eprintln!("[host] SEAL FAILED: {e}"),
            }
        }
    });
    let out = Outbound(out_tx);

    // --- Assemble the agent (mirrors bob-cli run()) ---
    let bus = EventBus::new();
    // Live turn buffer: every event forwarded to the controller is ALSO kept
    // here for the duration of the in-flight turn, so a controller that
    // (re)connects mid-turn can be replayed the tokens it missed. Cleared at
    // turn start and again once the turn's messages are persisted. Uses a
    // std::sync::Mutex because the bus listener is a synchronous closure.
    let live_buffer: Arc<std::sync::Mutex<Vec<AgentEventDto>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    // Whether a turn is currently running (drives resync Status + notify seam).
    let busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Accumulates subagent tool activity for the in-flight turn so it can be
    // persisted (subagent tools aren't part of the root message history).
    // Keyed by task tool_use_id → agent_id ("task_N") → PersistedSubagent.
    // Since subagent events carry only "task_N" (not the task tool_use_id), we
    // bind them to the most recent `task` tool_call seen this turn.
    let sub_accum: Arc<std::sync::Mutex<SubAccum>> =
        Arc::new(std::sync::Mutex::new(SubAccum::default()));

    // Shadow-write every event to the append-only log, exactly as the TUI does, so
    // a remote-driven conversation builds the same event history a resumed TUI can
    // replay. The bus listener is synchronous and can't touch the async
    // `active_session` mutex, so the current session id is mirrored in a cheap
    // sync cell updated whenever the active session is swapped (load/new).
    let event_log = Arc::new(bob_core::core::session::EventLogWriter::spawn());
    let log_session_id: Arc<std::sync::Mutex<String>> =
        Arc::new(std::sync::Mutex::new(String::new()));
    {
        let event_log = event_log.clone();
        let log_session_id = log_session_id.clone();
        bus.on(Arc::new(move |e: &AgentEvent| {
            let id = log_session_id.lock().map(|g| g.clone()).unwrap_or_default();
            if !id.is_empty() {
                event_log.append(&id, e);
            }
        }));
    }
    {
        // Bridge every root-level event to a HostFrame::Event, and buffer it.
        let out = out.clone();
        let live_buffer = live_buffer.clone();
        let sub_accum = sub_accum.clone();
        bus.on(Arc::new(move |e: &AgentEvent| {
            if let Ok(mut acc) = sub_accum.lock() {
                acc.observe(e);
            }
            if is_remote_event(e) {
                let dto = AgentEventDto::from(e);
                if let Ok(mut buf) = live_buffer.lock() {
                    buf.push(dto.clone());
                }
                out.send(HostFrame::Event(dto));
            }
        }));
    }

    let query_pending: Pending<Option<String>> = Arc::new(Mutex::new(HashMap::new()));
    let perm_pending: Pending<Option<usize>> = Arc::new(Mutex::new(HashMap::new()));

    let user_asker = Arc::new(RemoteUserAsker {
        out: out.clone(),
        pending: query_pending.clone(),
    });
    let asker = Arc::new(RemoteAsker {
        out: out.clone(),
        pending: perm_pending.clone(),
    });

    let default_decision = match config.permissions.default.as_str() {
        "allow" => Decision::Allow,
        "deny" => Decision::Deny,
        _ => Decision::Ask,
    };
    let mut engine = PermissionEngine::new(default_decision, Some(asker));
    engine.add(allow_read_only());
    engine.add(allow_code_action_list());
    engine.add(flag_dangerous_bash());
    engine.add(allow_bash_commands(config.permissions.allow_bash.clone()));
    engine.add(allow_tools(config.permissions.allow.clone()));
    engine.add(deny_tools(config.permissions.deny.clone()));
    let permissions = Arc::new(engine);

    let jobs = bob_core::tools::jobs::JobRegistry::new();
    let team = bob_core::agent::team::AgentRegistry::new();
    let (mcp_tools, _notices) = bob_core::mcp::connect_all(&config.mcp_servers).await;

    // Start configured language servers in the background (non-blocking).
    let lsp = if config.lsp_servers.is_empty() {
        None
    } else {
        Some(bob_core::lsp::LspManager::start(&config.lsp_servers, &cwd))
    };

    let system_prompt =
        bob_core::agent::prompt::build_system_prompt(config.system.as_deref(), &cwd);

    // Build the fully-wired root agent (tools + coordination + team mailbox). The
    // same builder backs the TUI, so the two can't drift.
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

    // Resume the most recent conversation session (or start fresh), and load
    // its messages into the agent so a reconnecting controller sees history.
    let active_session = Arc::new(Mutex::new(session::latest_or_new(&provider_spec)));
    {
        let s = active_session.lock().await;
        // Point the event log at this session so bus events are appended under it.
        if let Ok(mut id) = log_session_id.lock() {
            *id = s.id.clone();
        }
        if !s.messages.is_empty() {
            let history = session::history_for(&s);
            agent.load_history(history.clone());
            eprintln!(
                "[host] resumed session {} ({} messages)",
                s.id,
                history.len()
            );
        }
    }
    let cancel = agent.cancel_handle();
    let agent = Arc::new(Mutex::new(agent));

    // Turn-completion channel: report per-turn errors + busy=false.
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<Option<String>>();
    {
        let out = out.clone();
        tokio::spawn(async move {
            while let Some(err) = done_rx.recv().await {
                if let Some(e) = err {
                    out.send(HostFrame::Event(AgentEventDto::Error {
                        agent_id: "root".into(),
                        message: e,
                    }));
                }
                out.send(HostFrame::Status { busy: false });
            }
        });
    }

    // Greet the controller with the current session's history and the session
    // list, so a freshly-connected app renders state immediately.
    {
        let s = active_session.lock().await;
        out.send(HostFrame::History {
            messages: session::history_for(&s),
            session_id: s.id.clone(),
            subagent_runs: s.subagent_runs.clone(),
        });
    }
    out.send(HostFrame::SessionList {
        sessions: session::list_all(),
    });

    // --- Inbound control loop ---
    while let Some(msg) = ws_stream.next().await {
        let text = match msg {
            Ok(WsMessage::Text(t)) => t,
            Ok(WsMessage::Close(_)) => {
                eprintln!("[host] relay closed the connection");
                break;
            }
            Err(e) => {
                eprintln!("[host] websocket error: {e}");
                break;
            }
            _ => continue,
        };
        // Every post-handshake frame is a sealed envelope; open it, then parse the
        // plaintext as a ControlFrame.
        let plaintext = match crate::channel::open_envelope(&mut opener, &text) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[host] {e}");
                break;
            }
        };
        let frame: ControlFrame = match serde_json::from_slice(&plaintext) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[host] bad control frame: {e}");
                continue;
            }
        };
        match frame {
            ControlFrame::Prompt { text } => {
                eprintln!(
                    "[host] prompt received ({} chars); starting turn",
                    text.len()
                );
                // New turn: reset the live buffer + subagent accumulator so
                // they only hold this turn's events, and mark busy. Clear any
                // leftover cancel from a PRIOR turn — otherwise a single Cancel
                // would poison every turn that follows it.
                cancel.store(false, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut buf) = live_buffer.lock() {
                    buf.clear();
                }
                if let Ok(mut acc) = sub_accum.lock() {
                    *acc = SubAccum::default();
                }
                busy.store(true, std::sync::atomic::Ordering::Relaxed);
                out.send(HostFrame::Status { busy: true });
                let agent = agent.clone();
                let done_tx = done_tx.clone();
                let active_session = active_session.clone();
                let out2 = out.clone();
                let live_buffer = live_buffer.clone();
                let busy = busy.clone();
                let sub_accum = sub_accum.clone();
                let cancel = cancel.clone();
                let event_log = event_log.clone();
                tokio::spawn(async move {
                    let mut a = agent.lock().await;
                    let result = a.run(&text).await;
                    match &result {
                        Ok(reply) => eprintln!("[host] turn finished ok ({} chars)", reply.len()),
                        Err(e) => eprintln!("[host] turn FAILED: {e:#}"),
                    }
                    // Coordination: if this turn spawned agents, keep the root
                    // alive until they all report back, driving an empty-prompt
                    // "wake" turn each time results are ready so the root folds them
                    // into history and acts on them. Without this, coordination is
                    // dead on remote (results reach root's inbox but nothing
                    // re-drives root). We hold the agent lock throughout, so a new
                    // Prompt queues behind this — intended: the turn isn't "done"
                    // until its team is. Bounded so a stuck child can't spin forever.
                    // A Cancel breaks the loop: the shared flag has already cascaded
                    // into every child, so they wind down and we stop re-waking.
                    let mut wakes = 0;
                    while wakes < 128
                        && !cancel.load(std::sync::atomic::Ordering::Relaxed)
                        && a.has_outstanding_coordination()
                    {
                        if a.has_pending_coordination() {
                            wakes += 1;
                            if let Err(e) = a.run("").await {
                                eprintln!("[host] wake turn FAILED: {e:#}");
                                break;
                            }
                        } else {
                            // Children still running; wait for one to report.
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                    }
                    // Collect this turn's subagent runs and merge into the
                    // session so subagent tool calls survive a restart.
                    let new_runs = sub_accum
                        .lock()
                        .map(|mut a| a.take_all())
                        .unwrap_or_default();
                    // Persist the updated history to the active session, then
                    // refresh the drawer (title/count may have changed).
                    let messages = a.messages().to_vec();
                    {
                        let mut s = active_session.lock().await;
                        s.subagent_runs.extend(new_runs);
                        session::persist(&mut s, messages);
                    }
                    // Make this turn's shadow-written events durable alongside the
                    // blob save above. flush() blocks on the writer draining, so do
                    // it off the async worker.
                    {
                        let event_log = event_log.clone();
                        let _ = tokio::task::spawn_blocking(move || event_log.flush()).await;
                    }
                    // The turn's events are now captured in persisted messages,
                    // so drop the live buffer and clear busy. (A reconnect that
                    // lands between persist and this clear self-heals on the next
                    // resync — History already reflects the finished turn.)
                    if let Ok(mut buf) = live_buffer.lock() {
                        buf.clear();
                    }
                    busy.store(false, std::sync::atomic::Ordering::Relaxed);
                    out2.send(HostFrame::SessionList {
                        sessions: session::list_all(),
                    });
                    let _ = done_tx.send(result.err().map(|e| format!("{e:#}")));
                });
            }
            ControlFrame::Cancel => {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            ControlFrame::AnswerQuery { id, answer } => {
                if let Some(tx) = query_pending.lock().await.remove(&id) {
                    let _ = tx.send(answer);
                }
            }
            ControlFrame::AnswerPermission { id, choice } => {
                if let Some(tx) = perm_pending.lock().await.remove(&id) {
                    let _ = tx.send(choice);
                }
            }
            ControlFrame::SetMode { mode } => {
                let m = match mode.as_str() {
                    "auto_accept" => Mode::AutoAccept,
                    "plan" => Mode::Plan,
                    _ => Mode::Normal,
                };
                permissions.set_mode(m);
            }
            ControlFrame::ListSessions => {
                // A controller sends this on connect/reconnect to resync. Reply
                // with, in order: the persisted history, a replay of any
                // in-flight turn's buffered events (tokens streamed while the
                // controller was away), the current busy state, and the session
                // list. This reconstructs the exact live state with no lost
                // tokens.
                {
                    let s = active_session.lock().await;
                    out.send(HostFrame::History {
                        messages: s.messages.clone(),
                        session_id: s.id.clone(),
                        subagent_runs: s.subagent_runs.clone(),
                    });
                }
                let replay: Vec<AgentEventDto> =
                    live_buffer.lock().map(|b| b.clone()).unwrap_or_default();
                if !replay.is_empty() {
                    eprintln!("[host] resync: replaying {} buffered events", replay.len());
                    for dto in replay {
                        out.send(HostFrame::Event(dto));
                    }
                }
                out.send(HostFrame::Status {
                    busy: busy.load(std::sync::atomic::Ordering::Relaxed),
                });
                out.send(HostFrame::SessionList {
                    sessions: session::list_all(),
                });
            }
            ControlFrame::LoadSession { id } => {
                match session::load(&id) {
                    Some(s) => {
                        // Reconstruct history from the event log (blob fallback),
                        // then swap the agent's history and the active session.
                        let history = session::history_for(&s);
                        {
                            let mut a = agent.lock().await;
                            a.load_history(history.clone());
                        }
                        // Drop buffered/accumulated events from the previous
                        // session so a resync can't replay them under this one.
                        if let Ok(mut buf) = live_buffer.lock() {
                            buf.clear();
                        }
                        if let Ok(mut acc) = sub_accum.lock() {
                            *acc = SubAccum::default();
                        }
                        let (messages, sid, runs) = {
                            let mut active = active_session.lock().await;
                            *active = s;
                            (
                                history,
                                active.id.clone(),
                                active.subagent_runs.clone(),
                            )
                        };
                        // Redirect the event log to the newly-active session.
                        if let Ok(mut id) = log_session_id.lock() {
                            *id = sid.clone();
                        }
                        eprintln!("[host] loaded session {sid} ({} messages)", messages.len());
                        out.send(HostFrame::History {
                            messages,
                            session_id: sid,
                            subagent_runs: runs,
                        });
                    }
                    None => eprintln!("[host] load: unknown session {id}"),
                }
            }
            ControlFrame::NewSession => {
                let fresh = session::fresh(&provider_spec);
                {
                    let mut a = agent.lock().await;
                    a.load_history(Vec::new());
                }
                // Switching sessions: drop any buffered/accumulated events from
                // the previous session so a later resync can't replay them here.
                if let Ok(mut buf) = live_buffer.lock() {
                    buf.clear();
                }
                if let Ok(mut acc) = sub_accum.lock() {
                    *acc = SubAccum::default();
                }
                let sid = fresh.id.clone();
                *active_session.lock().await = fresh;
                // Redirect the event log to the new session.
                if let Ok(mut id) = log_session_id.lock() {
                    *id = sid.clone();
                }
                eprintln!("[host] started new session {sid}");
                out.send(HostFrame::History {
                    messages: Vec::new(),
                    session_id: sid,
                    subagent_runs: Vec::new(),
                });
                out.send(HostFrame::SessionList {
                    sessions: session::list_all(),
                });
            }
        }
    }

    writer.abort();
    Ok(())
}

/// Which events to forward to the controller. All root events, plus subagent
/// progress: SubagentSpawn + ToolCall/ToolResult/TurnEnd/Completion from ANY
/// agent (so the subagent transcript shows each step's input and output).
/// Subagent TextDelta/Message stay root-only — they'd flood the chat.
fn is_remote_event(e: &AgentEvent) -> bool {
    match e {
        AgentEvent::SubagentSpawn { .. }
        | AgentEvent::SubagentDone { .. }
        | AgentEvent::ToolCall { .. }
        | AgentEvent::ToolResult { .. }
        | AgentEvent::TurnEnd { .. }
        | AgentEvent::WorkflowPhase { .. }
        | AgentEvent::WorkflowLog { .. }
        | AgentEvent::Completion { .. } => true,
        AgentEvent::TurnStart { agent_id }
        | AgentEvent::TextDelta { agent_id, .. }
        | AgentEvent::Message { agent_id, .. }
        | AgentEvent::Compaction { agent_id, .. }
        | AgentEvent::ContextWarning { agent_id, .. }
        | AgentEvent::Error { agent_id, .. } => agent_id == "root",
        // Inter-agent coordination chatter stays internal — never sent to the phone.
        AgentEvent::AgentMessage { .. } => false,
        // The phone echoes its own submitted input locally; forwarding would double it.
        AgentEvent::UserPrompt { .. } => false,
        // Only from replaying a newer-version log; nothing to forward live.
        AgentEvent::Unknown => false,
    }
}
