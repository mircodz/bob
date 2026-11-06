use crate::{ArtifactId, JobId, OperationId, SessionId, ThreadId};
use serde::{Deserialize, Serialize};

pub const WORKFLOW_MAX_ROWS: usize = 100;
pub const WORKFLOW_MAX_PAGE_BYTES: usize = 64 * 1024;
pub const WORKFLOW_MAX_DEFINITION_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowPageRequest {
    /// Opaque membership-snapshot cursor, bound to session, operation, view and container.
    pub cursor: Option<String>,
    pub limit: usize,
    /// Encoded response budget, 4096..=65536 bytes, including paging metadata.
    pub max_bytes: usize,
}
impl Default for WorkflowPageRequest {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: 50,
            max_bytes: 32 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowQuery {
    List {
        page: WorkflowPageRequest,
    },
    Overview {
        operation: OperationId,
        container: Option<String>,
        page: WorkflowPageRequest,
    },
    Node {
        operation: OperationId,
        path: String,
    },
    /// Retained completed passes only; the node's iteration identifies its current pass.
    Iterations {
        operation: OperationId,
        path: String,
        page: WorkflowPageRequest,
    },
    Agents {
        operation: OperationId,
        page: WorkflowPageRequest,
    },
    /// UTF-8 byte offset, returned unchanged as next_offset; encoded budget 1024..=65536.
    Text {
        operation: OperationId,
        source: WorkflowTextSource,
        offset: usize,
        max_bytes: usize,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkflowQueryResult {
    List(WorkflowPage<WorkflowSummary>),
    Overview(WorkflowOverview),
    Node(WorkflowNodeDetails),
    Iterations(WorkflowPage<WorkflowIteration>),
    Agents(WorkflowPage<WorkflowAgent>),
    Text(WorkflowTextPage),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowPage<T> {
    pub rows: Vec<T>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowState {
    Unadmitted,
    Approval,
    Denied,
    Pending,
    Future,
    Running,
    WaitingCapacity,
    WaitingPermission,
    WaitingHuman,
    Blocked,
    NotSelected,
    Skipped,
    Stopping,
    Cancelled,
    Completed,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowNodeKind {
    Agent,
    FanOut,
    Parallel,
    Loop,
    Conditional,
    Branch,
    Iteration,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowTermination {
    ConditionMet,
    MaxRounds,
    Dry,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowSummary {
    pub operation: OperationId,
    pub owner: ThreadId,
    pub job: Option<JobId>,
    pub title: String,
    pub state: WorkflowState,
    pub admitted: bool,
    pub max_concurrency: usize,
    pub executions: usize,
    pub active_agents: usize,
    pub result: Option<ArtifactId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowOverview {
    pub workflow: WorkflowSummary,
    pub container: Option<String>,
    pub nodes: WorkflowPage<WorkflowNode>,
    pub definition_available: bool,
    pub definition_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowNode {
    pub path: String,
    pub label: String,
    pub kind: WorkflowNodeKind,
    pub state: WorkflowState,
    pub instantiated: bool,
    pub expandable: bool,
    pub iteration: usize,
    pub child: Option<ThreadId>,
    pub result: Option<ArtifactId>,
    pub error_preview: Option<String>,
    pub termination: Option<WorkflowTermination>,
    pub selected_branch: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowNodeDetails {
    pub operation: OperationId,
    pub node: WorkflowNode,
    pub specification_preview: String,
    pub specification_truncated: bool,
    pub dependencies: Vec<WorkflowDependency>,
    pub dependencies_truncated: bool,
    pub blockers: Vec<WorkflowBlocker>,
    pub artifacts: Vec<WorkflowArtifactLink>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowDependency {
    pub path: String,
    pub state: WorkflowState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowBlocker {
    pub state: WorkflowState,
    pub path: Option<String>,
    pub thread: Option<ThreadId>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowIteration {
    pub path: String,
    pub iteration: usize,
    pub result: ArtifactId,
    pub termination: Option<WorkflowTermination>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowAgent {
    pub thread: ThreadId,
    pub name: String,
    pub state: WorkflowState,
    pub node: Option<String>,
    pub result: Option<ArtifactId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowArtifactKind {
    NodeResult,
    IterationResult,
    ChildReport,
    Aggregate,
    ToolOutput,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowArtifactLink {
    pub artifact: ArtifactId,
    pub kind: WorkflowArtifactKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkflowTextSource {
    Definition,
    NodeSpecification { path: String },
    Artifact { artifact: ArtifactId },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkflowTextPage {
    pub session: SessionId,
    pub operation: OperationId,
    pub source: WorkflowTextSource,
    pub offset: usize,
    pub text: String,
    pub next_offset: Option<usize>,
    pub total_bytes: usize,
}
