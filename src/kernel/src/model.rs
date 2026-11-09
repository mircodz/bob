use crate::contracts::*;
use crate::live::ModelProgress;
use crate::presentation::*;
use async_trait::async_trait;
use bob_types::{Completion, Message, ReasoningEffort, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use tokio::sync::watch;

mod reasoning {
    use bob_types::ReasoningEffort;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(
        value: &ReasoningEffort,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(value.label())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<ReasoningEffort, D::Error> {
        let text = String::deserialize(deserializer)?;
        ReasoningEffort::parse(&text)
            .ok_or_else(|| serde::de::Error::custom("invalid reasoning effort"))
    }
}

macro_rules! ids {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl $name {
            pub fn new() -> Self { Self(uuid::Uuid::new_v4().to_string()) }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
        }
        impl rusqlite::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(self.0.as_str().into())
            }
        }
    )+};
}
ids!(
    SessionId,
    ThreadId,
    CommandId,
    RunId,
    OperationId,
    ArtifactId,
    JobId
);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub context_window: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub model: String,
    pub instructions: String,
    pub read_only: bool,
    pub max_requests: u32,
    #[serde(with = "reasoning")]
    pub reasoning: ReasoningEffort,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Policy {
    pub yolo: bool,
    pub plan: bool,
    pub denied_tools: BTreeSet<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Read,
    Write,
}

#[derive(Clone, Debug)]
pub struct ToolDescriptor {
    pub spec: ToolSpec,
    pub effect: EffectClass,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invocation {
    pub operation_id: OperationId,
    pub run_id: RunId,
    pub thread_id: ThreadId,
    pub workspace: String,
    pub name: String,
    pub input: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreparedTool {
    pub invocation: Invocation,
    pub specification: Value,
    pub preview: String,
    #[serde(default)]
    pub presentation: Option<ToolPresentation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
    pub interrupted: bool,
    #[serde(default)]
    pub presentation: Option<ToolResultPresentation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestSpec {
    pub model: ModelInfo,
    pub workspace: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    #[serde(with = "reasoning")]
    pub reasoning: ReasoningEffort,
    pub max_tokens: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreparedAuthorizationRequirement {
    EditFile { path: std::path::PathBuf },
    ToolPath { path: std::path::PathBuf },
}

#[async_trait]
pub trait Executor: Send + Sync + 'static {
    fn models(&self) -> Vec<ModelInfo>;
    fn tools(&self) -> Vec<ToolDescriptor>;
    fn model_capabilities(&self, _model: &str) -> ModelCapabilities {
        ModelCapabilities::default()
    }
    /// Total context input, including cache hits/writes exactly once; None if unknown.
    /// This measurement is independent of the raw billing receipt.
    fn measured_input_tokens(&self, _model: &str, _usage: &bob_types::Usage) -> Option<u64> {
        None
    }
    fn tool_capabilities(&self, name: &str) -> ToolCapabilities {
        let write = self
            .tools()
            .iter()
            .any(|tool| tool.spec.name == name && tool.effect == EffectClass::Write);
        ToolCapabilities {
            mutates_workspace: write,
            plan_safe: !write,
            ..Default::default()
        }
    }
    fn prepared_capabilities(&self, tool: &PreparedTool) -> anyhow::Result<ToolCapabilities> {
        Ok(self.tool_capabilities(&tool.invocation.name))
    }
    /// Return the immutable prepared primary pathname, not its ancestor or a newly resolved target.
    /// The kernel retains it with authorizations and uses it for policy and exact grant choices.
    fn prepared_authorization_target(
        &self,
        _tool: &PreparedTool,
    ) -> anyhow::Result<Option<std::path::PathBuf>> {
        Ok(None)
    }
    fn prepared_authorization_requirements(
        &self,
        _tool: &PreparedTool,
    ) -> anyhow::Result<Vec<PreparedAuthorizationRequirement>> {
        Ok(Vec::new())
    }
    fn prepared_shell_command(
        &self,
        _tool: &PreparedTool,
    ) -> anyhow::Result<Option<crate::ShellCommandGrant>> {
        Ok(None)
    }
    fn prepared_policy(
        &self,
        _tool: &PreparedTool,
        _permissions: &PermissionPolicy,
    ) -> anyhow::Result<Option<RuleDecision>> {
        Ok(None)
    }
    fn display_text(
        &self,
        _tool: &PreparedTool,
        _outcome: &ToolOutcome,
    ) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn classify_error(&self, _error: &anyhow::Error) -> ModelFailureKind {
        ModelFailureKind::Fatal
    }
    async fn system_context(&self, _workspace: &str, base: &str) -> anyhow::Result<String> {
        Ok(base.to_owned())
    }
    async fn generate(
        &self,
        request: RequestSpec,
        cancel: watch::Receiver<bool>,
        progress: ModelProgress,
    ) -> anyhow::Result<Completion>;
    async fn prepare(&self, invocation: Invocation) -> anyhow::Result<PreparedTool>;
    async fn prepare_with_cancel(
        &self,
        invocation: Invocation,
        _cancel: watch::Receiver<bool>,
    ) -> anyhow::Result<PreparedTool> {
        self.prepare(invocation).await
    }
    async fn execute_with_progress(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
        _progress: crate::ToolProgress,
    ) -> anyhow::Result<ToolOutcome> {
        self.execute(tool, cancel).await
    }
    async fn execute(
        &self,
        tool: PreparedTool,
        cancel: watch::Receiver<bool>,
    ) -> anyhow::Result<ToolOutcome>;
}

pub async fn cancelled(signal: &mut watch::Receiver<bool>) {
    while !*signal.borrow_and_update() {
        if signal.changed().await.is_err() {
            break;
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    RecallSubmission {
        id: CommandId,
        submission: CommandId,
    },
    Background {
        id: CommandId,
        thread: ThreadId,
    },
    StopJob {
        id: CommandId,
        job: JobId,
    },
    CreateSession {
        id: CommandId,
        workspace: String,
        profile: Profile,
        policy: Policy,
    },
    Submit {
        id: CommandId,
        thread: ThreadId,
        text: String,
    },
    StopThread {
        id: CommandId,
        thread: ThreadId,
    },
    Stop {
        id: CommandId,
        run: RunId,
    },
    Resume {
        id: CommandId,
        thread: ThreadId,
    },
    Decide {
        id: CommandId,
        operation: OperationId,
        revision: i64,
        approve: bool,
    },
    SetPolicy {
        id: CommandId,
        session: SessionId,
        policy: Policy,
        expected_revision: i64,
    },
    CreateSessionConfigured {
        id: CommandId,
        workspace: String,
        profile: Profile,
        policy: Policy,
        permissions: PermissionPolicy,
    },
    DecideWithGrant {
        id: CommandId,
        operation: OperationId,
        revision: i64,
        approve: bool,
        grant: Option<PermissionGrant>,
    },
    UpdateProfile {
        id: CommandId,
        thread: ThreadId,
        profile: Profile,
        expected_revision: i64,
    },
    ClearContext {
        id: CommandId,
        thread: ThreadId,
        expected_epoch: i64,
    },
    Compact {
        id: CommandId,
        thread: ThreadId,
        expected_revision: i64,
    },
    Steer {
        id: CommandId,
        thread: ThreadId,
        text: String,
    },
    SetPermissions {
        id: CommandId,
        session: SessionId,
        permissions: PermissionPolicy,
        expected_revision: i64,
    },
    AnswerQuestion {
        id: CommandId,
        operation: OperationId,
        revision: i64,
        answer: String,
    },
    DecidePlan {
        id: CommandId,
        operation: OperationId,
        revision: i64,
        approve: bool,
        feedback: Option<String>,
    },
    ResolveUnknown {
        id: CommandId,
        operation: OperationId,
        evidence: String,
    },
}
impl Command {
    pub fn id(&self) -> &CommandId {
        match self {
            Self::RecallSubmission { id, .. }
            | Self::Background { id, .. }
            | Self::StopJob { id, .. }
            | Self::CreateSession { id, .. }
            | Self::Submit { id, .. }
            | Self::StopThread { id, .. }
            | Self::Stop { id, .. }
            | Self::Resume { id, .. }
            | Self::Decide { id, .. }
            | Self::SetPolicy { id, .. }
            | Self::ResolveUnknown { id, .. }
            | Self::CreateSessionConfigured { id, .. }
            | Self::DecideWithGrant { id, .. }
            | Self::UpdateProfile { id, .. }
            | Self::ClearContext { id, .. }
            | Self::Compact { id, .. }
            | Self::Steer { id, .. }
            | Self::SetPermissions { id, .. }
            | Self::AnswerQuestion { id, .. }
            | Self::DecidePlan { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub session: SessionId,
    pub thread: ThreadId,
    pub sequence: i64,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub id: ThreadId,
    pub session: SessionId,
    pub parent_run: Option<RunId>,
    pub parent_thread: Option<ThreadId>,
    pub needs_approval: bool,
    pub activity: Option<ThreadActivity>,
    pub name: String,
    pub model: String,
    pub state: ThreadState,
    pub run: Option<RunId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntrySender {
    pub thread: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionState {
    Pending,
    Consumed,
    Recalled,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EntrySubmission {
    pub id: CommandId,
    pub state: SubmissionState,
    pub ordinal: i64,
    /// Changes snapshot revision, not the transcript entry sequence.
    pub sequence: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub sequence: i64,
    pub thread: ThreadId,
    pub kind: EntryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender: Option<EntrySender>,
    #[serde(default)]
    pub parent_guidance: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission: Option<EntrySubmission>,
    pub text: String,
    pub artifact: Option<ArtifactId>,
    pub operation: Option<OperationId>,
    pub tool: Option<ToolSummary>,
    pub is_preview: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_text_bytes: Option<usize>,
    /// Model reasoning shown above an assistant reply; display-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
}

impl Entry {
    pub fn visible_in_root(&self) -> bool {
        self.kind != EntryKind::Coordination
            && self.tool.as_ref().is_none_or(|tool| {
                !matches!(
                    tool.name.as_str(),
                    "send_message" | "wait_agent" | "wait_message" | "job_status" | "job_output"
                )
            })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSummary {
    pub id: OperationId,
    pub name: String,
    pub state: OperationState,
    pub input_preview: Value,
    pub result: Option<ArtifactId>,
    pub presentation: Option<ToolResultPresentation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TextPage {
    pub text: String,
    pub next_offset: Option<usize>,
    pub total_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub operation: OperationId,
    pub thread: ThreadId,
    pub name: String,
    pub revision: i64,
    pub preview: String,
    pub presentation: Option<ToolPresentation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationSummary {
    pub id: OperationId,
    pub run: RunId,
    pub name: String,
    pub state: OperationState,
    pub specification: Value,
    pub result: Option<ArtifactId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionPolicy {
    pub session: SessionId,
    pub policy: Policy,
    pub revision: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct View {
    pub policies: Vec<SessionPolicy>,
    pub sequence: i64,
    /// Global entry head in this snapshot, independent of the changes sequence and page bounds.
    pub entry_sequence: i64,
    pub threads: Vec<ThreadSummary>,
    pub entries: Vec<Entry>,
    pub decisions: Vec<Decision>,
    pub operations: Vec<OperationSummary>,
    pub error: Option<String>,
}
