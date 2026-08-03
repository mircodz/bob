//! A tool is its provider-agnostic spec plus an execute function. The registry
//! gates every call through the permission engine before running it.

use crate::core::permissions::{parse_bash, PermissionEngine, PermissionRequest};
use crate::core::types::ToolSpec;
use crate::tools::file_tracker::FileTracker;
use crate::tools::jobs::JobRegistry;
use crate::tools::todo::TodoStore;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// A question the model poses to the user (via `ask_user` or `exit_plan`): a
/// title, an optional detail/body (Markdown), and labeled options. The user
/// picks one (or types their own answer).
#[derive(Clone, Debug)]
pub struct UserQuery {
    pub title: String,
    pub detail: String,
    pub options: Vec<String>,
    /// If true, the UI offers a free-text "Other…" choice.
    pub allow_other: bool,
}

/// Supplied by the UI: pose a question to the human, return the chosen answer
/// text (an option label, or free-text). None means the user dismissed it.
#[async_trait]
pub trait UserAsker: Send + Sync {
    async fn ask(&self, query: &UserQuery) -> Option<String>;
}

/// Shared per-session state handed to every tool call.
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: String,
    pub files: Arc<FileTracker>,
    pub todos: Arc<TodoStore>,
    /// Registry of background jobs (started via the `task` tool with
    /// background:true, polled via job_status/job_output).
    pub jobs: JobRegistry,
    /// UI hook for asking the user a question (ask_user / exit_plan). None in
    /// headless contexts — tools handle that by returning a note.
    pub user_asker: Option<Arc<dyn UserAsker>>,
    /// Language servers for this project (None if no lsp_servers configured).
    /// The `lsp` tool routes files to the right server via this manager.
    pub lsp: Option<Arc<crate::lsp::LspManager>>,
    /// Coordination context: this agent's name, spawn depth, and the shared team
    /// roster, so the coordination tools (spawn_agent / send_message /
    /// list_agents) know who is calling and can address other agents. None when
    /// the agent is not part of a team (the simple `task` path).
    pub coord: Option<CoordContext>,
}

/// The calling agent's coordination context, threaded to the coordination tools.
#[derive(Clone)]
pub struct CoordContext {
    /// This agent's own name in the team.
    pub name: String,
    /// This agent's spawn depth (root = 0). Children are `depth + 1`.
    pub depth: usize,
    /// The shared team roster.
    pub team: crate::agent::team::AgentRegistry,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult;
    /// Optionally produce a human-readable preview of what this call *would* do,
    /// computed WITHOUT side effects. Shown in the permission prompt before the
    /// user approves. Edit/write tools return a ```diff block; most return None.
    fn preview(&self, _input: &Value, _ctx: &ToolContext) -> Option<String> {
        None
    }
}

/// A tool's outcome: `Ok` carries the success text shown to the model; `Err`
/// carries a typed failure. This replaces the old `"error:"` string convention —
/// the agent loop derives `is_error` from the variant, not by sniffing text, so a
/// `read_file` on a file that literally contains "error:" is no longer misread as
/// a failure.
pub type ToolResult = Result<String, ToolError>;

/// The category of a tool failure. Lets the model and UI branch on *why* a call
/// failed (retry a path, fix an argument, ask the user) instead of parsing prose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolErrorKind {
    /// A referenced file/symbol/agent doesn't exist.
    NotFound,
    /// The call's arguments were missing or malformed.
    InvalidInput,
    /// The permission engine (or the OS) denied the action.
    PermissionDenied,
    /// The operation timed out.
    Timeout,
    /// A required facility isn't available here (no LSP, no UI, no team).
    Unavailable,
    /// A catch-all runtime failure (a command exited non-zero, IO error, etc.).
    Failed,
}

impl ToolErrorKind {
    /// Stable snake_case slug, used as the wire prefix the model sees.
    pub fn slug(self) -> &'static str {
        match self {
            ToolErrorKind::NotFound => "not_found",
            ToolErrorKind::InvalidInput => "invalid_input",
            ToolErrorKind::PermissionDenied => "permission_denied",
            ToolErrorKind::Timeout => "timeout",
            ToolErrorKind::Unavailable => "unavailable",
            ToolErrorKind::Failed => "failed",
        }
    }
}

/// A typed tool failure: a category plus a human-readable message.
#[derive(Clone, Debug)]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
}

impl ToolError {
    pub fn new(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        ToolError {
            kind,
            message: message.into(),
        }
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::NotFound, message)
    }
    pub fn invalid_input(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::InvalidInput, message)
    }
    pub fn permission_denied(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::PermissionDenied, message)
    }
    pub fn timeout(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::Timeout, message)
    }
    pub fn unavailable(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::Unavailable, message)
    }
    pub fn failed(message: impl Into<String>) -> Self {
        ToolError::new(ToolErrorKind::Failed, message)
    }

    /// The text the model sees in a `tool_result` block: `"<kind>: <message>"`.
    /// The prefix gives the model a machine-stable category to reason about.
    pub fn wire(&self) -> String {
        format!("{}: {}", self.kind.slug(), self.message)
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.wire())
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => ToolError::not_found(e.to_string()),
            ErrorKind::PermissionDenied => ToolError::permission_denied(e.to_string()),
            _ => ToolError::failed(e.to_string()),
        }
    }
}

#[derive(Clone, Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
    permissions: Option<Arc<PermissionEngine>>,
}

impl ToolRegistry {
    pub fn new(permissions: Option<Arc<PermissionEngine>>) -> Self {
        ToolRegistry {
            tools: HashMap::new(),
            order: Vec::new(),
            permissions,
        }
    }

    pub fn add(&mut self, tool: Arc<dyn Tool>) -> &mut Self {
        let name = tool.spec().name;
        if !self.tools.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.tools.insert(name, tool);
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// A new registry containing only the read-only tools from this one — reads,
    /// searches, navigation, web. Used to build the `explore` subagent, which must
    /// not mutate the workspace. Unknown/mutating tools (edit, write, bash, etc.)
    /// are dropped, so an explore agent physically cannot change anything.
    pub fn read_only_subset(&self) -> ToolRegistry {
        const READ_ONLY: &[&str] = &[
            "read_file",
            "list_dir",
            "glob",
            "grep",
            "lsp",
            "web_fetch",
            "web_search",
        ];
        let mut out = ToolRegistry::new(self.permissions.clone());
        for name in &self.order {
            if READ_ONLY.contains(&name.as_str()) {
                if let Some(t) = self.tools.get(name) {
                    out.add(t.clone());
                }
            }
        }
        out
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order
            .iter()
            .filter_map(|n| self.tools.get(n).map(|t| t.spec()))
            .collect()
    }

    pub async fn execute(&self, name: &str, input: Value, ctx: &ToolContext) -> ToolResult {
        let tool = match self.tools.get(name) {
            Some(t) => t.clone(),
            None => return Err(ToolError::not_found(format!("unknown tool \"{}\"", name))),
        };

        // Gate every call through the permission engine, if configured.
        if let Some(perms) = &self.permissions {
            let bash = if name == "bash" {
                let cmd = input.get("command").and_then(|c| c.as_str()).unwrap_or("");
                Some(parse_bash(cmd))
            } else {
                None
            };
            // Compute a side-effect-free preview (e.g. the diff for an edit) so
            // the prompt can show what will happen before the user approves.
            let preview = tool.preview(&input, ctx);
            let req = PermissionRequest {
                tool: name.to_string(),
                input: input.clone(),
                cwd: ctx.cwd.clone(),
                bash,
                preview,
            };
            if !perms.check(&req).await {
                return Err(ToolError::permission_denied(format!(
                    "permission denied for tool \"{}\"",
                    name
                )));
            }
        }

        tool.execute(input, ctx).await
    }
}
