use crate::{ModelInfo, OperationId, Policy, Profile, SessionId, ThreadId};
use bob_types::Usage;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelCapabilities {
    pub max_input_tokens: Option<usize>,
    pub max_output_tokens: u32,
    pub preferred_output_tokens: u32,
}
impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            max_input_tokens: None,
            max_output_tokens: 16_384,
            preferred_output_tokens: 4096,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub model: ModelInfo,
    pub capabilities: ModelCapabilities,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelFailureKind {
    Fatal,
    Transient,
    ContextOverflow,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolCapabilities {
    pub mutates_state: bool,
    pub mutates_workspace: bool,
    pub executes_process: bool,
    pub external_access: bool,
    pub plan_safe: bool,
    pub requires_approval: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Normal,
    AutoAccept,
    Plan,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleDecision {
    Allow,
    #[default]
    Ask,
    RequireApproval,
    Deny,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRule {
    pub tool: String,
    pub decision: RuleDecision,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrant {
    pub tool: String,
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_command: Option<ShellCommandGrant>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellCommandGrant {
    pub dialect: String,
    pub program: String,
}
impl ShellCommandGrant {
    pub(crate) fn well_formed(&self) -> bool {
        self.dialect == "bash"
            && self.program.len() <= 64
            && self
                .program
                .as_bytes()
                .first()
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
            && self
                .program
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalOptions {
    pub operation: OperationId,
    pub thread: ThreadId,
    pub revision: i64,
    pub options: Vec<ApprovalOption>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalOption {
    pub label: String,
    pub approve: bool,
    pub grant: Option<PermissionGrant>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionPolicy {
    pub default: RuleDecision,
    pub mode: PermissionMode,
    pub rules: Vec<PermissionRule>,
    pub grants: Vec<PermissionGrant>,
    pub allow_bash: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionQuery {
    pub search: Option<String>,
    pub workspace: Option<String>,
    pub before: Option<i64>,
    pub limit: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub thread: ThreadId,
    pub ordinal: i64,
    pub workspace: String,
    pub model: String,
    pub preview: String,
    pub title: String,
    pub message_count: usize,
    pub updated_at: i64,
    pub human_previews: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionPage {
    pub sessions: Vec<SessionSummary>,
    pub next_before: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub title: String,
    pub title_revision: i64,
    pub thread: ThreadId,
    pub workspace: String,
    pub profile: Profile,
    pub profile_revision: i64,
    pub context_epoch: i64,
    pub policy: Policy,
    pub permissions: PermissionPolicy,
    pub policy_revision: i64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ContextBudget {
    pub context_window: usize,
    pub input_limit: usize,
    pub output_reserve: u32,
    pub estimated_input_tokens: usize,
    pub measured_input_tokens: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContextSnapshot {
    pub profile: Profile,
    pub thread: ThreadId,
    pub epoch: i64,
    pub revision: i64,
    pub profile_revision: i64,
    pub message_count: usize,
    pub checkpoint: Option<OperationId>,
    pub compact_pending: bool,
    pub budget: ContextBudget,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageSnapshot {
    pub requests: u64,
    pub compaction_requests: u64,
    pub usage: Usage,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HumanQuestion {
    pub operation: OperationId,
    pub revision: i64,
    pub question: String,
    pub detail: Option<String>,
    pub options: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanProposal {
    pub operation: OperationId,
    pub revision: i64,
    pub plan: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub active_form: Option<String>,
    pub status: TodoStatus,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadDetails {
    pub thread: ThreadId,
    pub task: String,
    pub depth: usize,
    pub launch_sequence: i64,
    pub workflow_operation: Option<OperationId>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub questions: Vec<HumanQuestion>,
    pub plans: Vec<PlanProposal>,
    pub todos: Vec<TodoItem>,
}
