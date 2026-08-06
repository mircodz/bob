//! Wire protocol for bob remote control. JSON frames over a WebSocket, relayed
//! between a `bob-remote` host (drives a bob-core Agent) and a controller (the
//! iOS app, or the built-in `--test-client`).
//!
//! DTOs mirror bob-core types instead of deriving `Serialize` on core, keeping
//! the core crate free of transport concerns. `From<&_>` conversions live here.

use bob_core::core::events::AgentEvent;
use bob_core::core::permissions::{PermissionOption, PermissionRequest};
use bob_core::core::types::{Message, Usage};
use bob_core::tools::registry::UserQuery;
use serde::{Deserialize, Serialize};

/// The first frame each peer sends on the control WebSocket. It identifies the
/// role + session slot and carries the **admission proof** (an opaque base64
/// value derived from the pairing secret via `bob-secure`). The relay pairs a
/// `Host` with a `Controller` on the same `session` and admits the second peer
/// only if its proof byte-matches the first's — without ever learning the secret.
/// Everything after the Hello is an end-to-end-encrypted [`Sealed`] frame the
/// relay cannot read.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Hello {
    Host { session: String, admission: String },
    Controller { session: String, admission: String },
}

impl Hello {
    pub fn session(&self) -> &str {
        match self {
            Hello::Host { session, .. } | Hello::Controller { session, .. } => session,
        }
    }
    /// The base64 admission proof this peer presents.
    pub fn admission(&self) -> &str {
        match self {
            Hello::Host { admission, .. } | Hello::Controller { admission, .. } => admission,
        }
    }
    pub fn is_host(&self) -> bool {
        matches!(self, Hello::Host { .. })
    }
}

/// Everything sent *after* the [`Hello`], while and once the end-to-end channel
/// is established. During the handshake the peers exchange [`Envelope::Handshake`]
/// messages; afterwards every application frame travels as [`Envelope::Sealed`].
/// The relay forwards these opaque blobs verbatim and can read none of them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Envelope {
    /// One Noise-handshake message, base64-encoded.
    Handshake { data: String },
    /// A sealed application frame: base64 of `bob_secure::Session::seal(plaintext)`,
    /// where the plaintext is a JSON `HostFrame` or `ControlFrame`.
    Sealed { data: String },
}

// ---------------------------------------------------------------------------
// Host -> Controller
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostFrame {
    /// A streamed agent event (text delta, tool call/result, turn end, ...).
    Event(AgentEventDto),
    /// The agent needs the human to answer a question (ask_user / exit_plan).
    AskQuery { id: String, query: UserQueryDto },
    /// A tool call needs permission; controller replies with a chosen index.
    AskPermission {
        id: String,
        request: PermissionReqDto,
        options: Vec<PermissionOptDto>,
    },
    /// Full conversation history, pushed when a controller connects, when a
    /// session is loaded, or after a new session is started (empty).
    History {
        messages: Vec<Message>,
        /// The active conversation session id these messages belong to.
        #[serde(default)]
        session_id: String,
        /// Persisted subagent runs (the `task` tool's children with their tool
        /// calls), so the app can rehydrate subagent detail after a restart.
        #[serde(default)]
        subagent_runs: Vec<bob_core::core::session::SubagentRun>,
    },
    /// The list of stored conversation sessions (for the drawer).
    SessionList { sessions: Vec<SessionMeta> },
    /// Whether the agent is mid-turn.
    Status { busy: bool },
}

/// Lightweight metadata for one stored conversation session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    /// A human title — typically the first user message, truncated.
    pub title: String,
    pub updated_at: String,
    pub message_count: usize,
}

impl From<&bob_core::core::session::SessionSummary> for SessionMeta {
    fn from(s: &bob_core::core::session::SessionSummary) -> Self {
        SessionMeta {
            id: s.id.clone(),
            title: s.title.clone(),
            updated_at: s.updated_at.clone(),
            message_count: s.message_count,
        }
    }
}

// ---------------------------------------------------------------------------
// Controller -> Host
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlFrame {
    /// Start a new turn with this user prompt.
    Prompt { text: String },
    /// Interrupt the running turn.
    Cancel,
    /// Answer a prior `AskQuery` (None = dismissed).
    AnswerQuery { id: String, answer: Option<String> },
    /// Answer a prior `AskPermission` (index into options; None = deny).
    AnswerPermission { id: String, choice: Option<usize> },
    /// Change interaction mode: "normal" | "auto_accept" | "plan".
    SetMode { mode: String },
    /// Request the list of stored sessions (host replies with SessionList).
    ListSessions,
    /// Load a stored session by id (host replies with History).
    LoadSession { id: String },
    /// Start a fresh, empty session (host replies with empty History).
    NewSession,
}

// ---------------------------------------------------------------------------
// DTOs mirroring bob-core types
// ---------------------------------------------------------------------------

/// Serializable mirror of `AgentEvent`. `Message`/`Usage` are already Serialize
/// so we embed them directly.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEventDto {
    TurnStart {
        agent_id: String,
    },
    TextDelta {
        agent_id: String,
        text: String,
    },
    Message {
        agent_id: String,
        message: Message,
    },
    ToolCall {
        agent_id: String,
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        agent_id: String,
        tool_use_id: String,
        output: String,
        is_error: bool,
    },
    SubagentSpawn {
        parent_id: String,
        agent_id: String,
        task: String,
        #[serde(default)]
        prompt: String,
    },
    SubagentDone {
        agent_id: String,
        failed: bool,
    },
    AgentMessage {
        to: String,
        from: String,
        text: String,
    },
    Compaction {
        agent_id: String,
        before_tokens: usize,
        after_tokens: usize,
    },
    WorkflowPhase {
        workflow_id: String,
        title: String,
        index: usize,
        total: usize,
    },
    WorkflowLog {
        workflow_id: String,
        message: String,
    },
    ContextWarning {
        agent_id: String,
        used_tokens: usize,
        context_window: usize,
        pct: u8,
    },
    Completion {
        agent_id: String,
        model: String,
        usage: Usage,
    },
    TurnEnd {
        agent_id: String,
        usage: Usage,
    },
    Error {
        agent_id: String,
        message: String,
    },
}

impl From<&AgentEvent> for AgentEventDto {
    fn from(e: &AgentEvent) -> Self {
        match e {
            // The phone echoes the user's own submitted input locally, so a
            // UserPrompt is mapped to a user Message but filtered out before it's
            // sent (see host::is_remote_event) to avoid a double line.
            AgentEvent::UserPrompt { agent_id, text } => AgentEventDto::Message {
                agent_id: agent_id.clone(),
                message: Message::user_text(text.clone()),
            },
            AgentEvent::TurnStart { agent_id } => AgentEventDto::TurnStart {
                agent_id: agent_id.clone(),
            },
            AgentEvent::TextDelta { agent_id, text } => AgentEventDto::TextDelta {
                agent_id: agent_id.clone(),
                text: text.clone(),
            },
            AgentEvent::Message { agent_id, message } => AgentEventDto::Message {
                agent_id: agent_id.clone(),
                message: message.clone(),
            },
            AgentEvent::ToolCall {
                agent_id,
                tool_use_id,
                name,
                input,
            } => AgentEventDto::ToolCall {
                agent_id: agent_id.clone(),
                tool_use_id: tool_use_id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            AgentEvent::ToolResult {
                agent_id,
                tool_use_id,
                output,
                is_error,
            } => AgentEventDto::ToolResult {
                agent_id: agent_id.clone(),
                tool_use_id: tool_use_id.clone(),
                output: output.clone(),
                is_error: *is_error,
            },
            AgentEvent::SubagentSpawn {
                parent_id,
                agent_id,
                task,
                prompt,
            } => AgentEventDto::SubagentSpawn {
                parent_id: parent_id.clone(),
                agent_id: agent_id.clone(),
                task: task.clone(),
                prompt: prompt.clone(),
            },
            AgentEvent::SubagentDone { agent_id, failed } => AgentEventDto::SubagentDone {
                agent_id: agent_id.clone(),
                failed: *failed,
            },
            AgentEvent::AgentMessage { to, from, text } => AgentEventDto::AgentMessage {
                to: to.clone(),
                from: from.clone(),
                text: text.clone(),
            },
            AgentEvent::Compaction {
                agent_id,
                before_tokens,
                after_tokens,
                ..
            } => AgentEventDto::Compaction {
                agent_id: agent_id.clone(),
                before_tokens: *before_tokens,
                after_tokens: *after_tokens,
            },
            AgentEvent::WorkflowPhase {
                workflow_id,
                title,
                index,
                total,
            } => AgentEventDto::WorkflowPhase {
                workflow_id: workflow_id.clone(),
                title: title.clone(),
                index: *index,
                total: *total,
            },
            AgentEvent::WorkflowLog {
                workflow_id,
                message,
            } => AgentEventDto::WorkflowLog {
                workflow_id: workflow_id.clone(),
                message: message.clone(),
            },
            AgentEvent::ContextWarning {
                agent_id,
                used_tokens,
                context_window,
                pct,
            } => AgentEventDto::ContextWarning {
                agent_id: agent_id.clone(),
                used_tokens: *used_tokens,
                context_window: *context_window,
                pct: *pct,
            },
            AgentEvent::Completion {
                agent_id,
                model,
                usage,
            } => AgentEventDto::Completion {
                agent_id: agent_id.clone(),
                model: model.clone(),
                usage: *usage,
            },
            AgentEvent::TurnEnd { agent_id, usage } => AgentEventDto::TurnEnd {
                agent_id: agent_id.clone(),
                usage: *usage,
            },
            AgentEvent::Error { agent_id, message } => AgentEventDto::Error {
                agent_id: agent_id.clone(),
                message: message.clone(),
            },
            // Only arises from replaying a log written by a newer bob; a live event
            // is never Unknown. Surface it as a benign error rather than dropping it.
            AgentEvent::Unknown => AgentEventDto::Error {
                agent_id: String::new(),
                message: "unknown event".to_string(),
            },
        }
    }
}

/// Mirror of `UserQuery` (ask_user / exit_plan).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserQueryDto {
    pub title: String,
    pub detail: String,
    pub options: Vec<String>,
    pub allow_other: bool,
}

impl From<&UserQuery> for UserQueryDto {
    fn from(q: &UserQuery) -> Self {
        UserQueryDto {
            title: q.title.clone(),
            detail: q.detail.clone(),
            options: q.options.clone(),
            allow_other: q.allow_other,
        }
    }
}

/// Mirror of `PermissionRequest` (only the display-relevant fields).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionReqDto {
    pub tool: String,
    pub input: serde_json::Value,
    pub cwd: String,
    pub preview: Option<String>,
}

impl From<&PermissionRequest> for PermissionReqDto {
    fn from(r: &PermissionRequest) -> Self {
        PermissionReqDto {
            tool: r.tool.clone(),
            input: r.input.clone(),
            cwd: r.cwd.clone(),
            preview: r.preview.clone(),
        }
    }
}

/// Mirror of `PermissionOption`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionOptDto {
    pub label: String,
    pub allow: bool,
}

impl From<&PermissionOption> for PermissionOptDto {
    fn from(o: &PermissionOption) -> Self {
        PermissionOptDto {
            label: o.label.clone(),
            allow: o.allow,
        }
    }
}
