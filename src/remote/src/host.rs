//! The agent host. Assembles a bob-core Agent exactly like bob-tui's `run()`,
//! but bridges the two seams over a WebSocket to the relay instead of a
//! terminal:
//!   - EventBus listener  -> HostFrame::Event   (outbound)
//!   - RemoteUserAsker / RemoteAsker  <-> AskQuery / AskPermission round-trips
//!   - incoming ControlFrame::Prompt/Cancel/SetMode drive the agent

use std::collections::HashMap;
use std::sync::Arc;

use crate::session;

use async_trait::async_trait;
use bob_core::agent::agent::{Agent, AgentConfig};
use bob_core::core::config::load_config;
use bob_core::core::events::{AgentEvent, EventBus};
use bob_core::core::permissions::{
    Asker, Decision, Mode, PermissionEngine, PermissionOption, PermissionRequest,
};
use bob_core::core::policies::{
    allow_bash_commands, allow_read_only, allow_tools, deny_dangerous_bash, deny_tools,
};
use bob_core::providers::create_provider;
use bob_core::tools::registry::{ToolRegistry, UserAsker, UserQuery};
use bob_core::tools::task::TaskTool;
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
            AgentEvent::ToolCall { agent_id, tool_use_id, name, .. }
                if agent_id == "root" && name == "task" =>
            {
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
            AgentEvent::ToolCall { agent_id, tool_use_id, name, input } if agent_id != "root" => {
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
            AgentEvent::ToolResult { agent_id, tool_use_id, output, is_error } if agent_id != "root" => {
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

/// Sends a HostFrame to the relay (serialized to a WS text frame).
#[derive(Clone)]
struct Outbound(mpsc::UnboundedSender<WsMessage>);
impl Outbound {
    fn send(&self, frame: HostFrame) {
        match serde_json::to_string(&frame) {
            Ok(json) => {
                if self.0.send(WsMessage::Text(json)).is_err() {
                    eprintln!("[host] OUT DROPPED (writer gone)");
                }
            }
            Err(e) => eprintln!("[host] OUT SERIALIZE FAILED: {e}"),
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
    token: String,
    provider_override: Option<String>,
) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = load_config(&cwd)?;
    let provider_spec = provider_override.unwrap_or_else(|| config.provider.clone());
    eprintln!("[host] cwd={} provider={}", cwd.display(), provider_spec);
    let provider = create_provider(&provider_spec).await?;
    eprintln!("[host] provider ready");

    // Connect to the relay and send Hello::Host.
    let (ws, _) = tokio_tungstenite::connect_async(&relay).await?;
    let (mut ws_sink, mut ws_stream) = ws.split();
    let hello = serde_json::to_string(&Hello::Host {
        session: session.clone(),
        token,
    })?;
    ws_sink.send(WsMessage::Text(hello)).await?;
    eprintln!("[host] connected to relay, session '{}'", session);

    // Outbound queue: everything the host sends to the relay funnels here.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMessage>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
    });
    let out = Outbound(out_tx);

    // --- Assemble the agent (mirrors bob-tui run()) ---
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
    engine.add(deny_dangerous_bash());
    engine.add(allow_bash_commands(config.permissions.allow_bash.clone()));
    engine.add(allow_tools(config.permissions.allow.clone()));
    engine.add(deny_tools(config.permissions.deny.clone()));
    let permissions = Arc::new(engine);

    let jobs = bob_core::tools::jobs::JobRegistry::new();
    let (mcp_tools, _notices) = bob_core::mcp::connect_all(&config.mcp_servers).await;

    let mut subagent_tools = ToolRegistry::new(Some(permissions.clone()));
    for t in bob_core::tools::builtin_tools() {
        subagent_tools.add(t);
    }
    for t in &mcp_tools {
        subagent_tools.add(t.clone());
    }

    let system_prompt =
        bob_core::agent::prompt::build_system_prompt(config.system.as_deref(), &cwd);

    let mut tools = ToolRegistry::new(Some(permissions.clone()));
    for t in bob_core::tools::builtin_tools() {
        tools.add(t);
    }
    for t in &mcp_tools {
        tools.add(t.clone());
    }
    tools.add(Arc::new(TaskTool {
        provider: provider.clone(),
        subagent_tools,
        bus: bus.clone(),
        cwd: cwd.to_string_lossy().to_string(),
        subagent_system: Some(system_prompt.clone()),
        jobs: jobs.clone(),
    }));

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
    });

    // Resume the most recent conversation session (or start fresh), and load
    // its messages into the agent so a reconnecting controller sees history.
    let active_session = Arc::new(Mutex::new(session::latest_or_new(&provider_spec)));
    {
        let s = active_session.lock().await;
        if !s.messages.is_empty() {
            agent.load_history(s.messages.clone());
            eprintln!(
                "[host] resumed session {} ({} messages)",
                s.id,
                s.messages.len()
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
            messages: s.messages.clone(),
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
        let frame: ControlFrame = match serde_json::from_str(&text) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[host] bad control frame: {e}");
                continue;
            }
        };
        match frame {
            ControlFrame::Prompt { text } => {
                eprintln!("[host] prompt received ({} chars); starting turn", text.len());
                // New turn: reset the live buffer + subagent accumulator so
                // they only hold this turn's events, and mark busy.
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
                tokio::spawn(async move {
                    let mut a = agent.lock().await;
                    let result = a.run(&text).await;
                    match &result {
                        Ok(reply) => eprintln!("[host] turn finished ok ({} chars)", reply.len()),
                        Err(e) => eprintln!("[host] turn FAILED: {e:#}"),
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
                let replay: Vec<AgentEventDto> = live_buffer
                    .lock()
                    .map(|b| b.clone())
                    .unwrap_or_default();
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
                        // Swap the agent's history and the active session.
                        {
                            let mut a = agent.lock().await;
                            a.load_history(s.messages.clone());
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
                            (active.messages.clone(), active.id.clone(),
                             active.subagent_runs.clone())
                        };
                        eprintln!("[host] loaded session {sid} ({} messages)", messages.len());
                        out.send(HostFrame::History {
                            messages, session_id: sid, subagent_runs: runs,
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
                eprintln!("[host] started new session {sid}");
                out.send(HostFrame::History {
                    messages: Vec::new(), session_id: sid, subagent_runs: Vec::new(),
                });
                out.send(HostFrame::SessionList { sessions: session::list_all() });
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
        | AgentEvent::ToolCall { .. }
        | AgentEvent::ToolResult { .. }
        | AgentEvent::TurnEnd { .. }
        | AgentEvent::Completion { .. } => true,
        AgentEvent::TurnStart { agent_id }
        | AgentEvent::TextDelta { agent_id, .. }
        | AgentEvent::Message { agent_id, .. }
        | AgentEvent::Compaction { agent_id, .. }
        | AgentEvent::Error { agent_id, .. } => agent_id == "root",
    }
}
