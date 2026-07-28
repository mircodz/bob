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
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn execute(&self, input: Value, ctx: &ToolContext) -> String;
    /// Optionally produce a human-readable preview of what this call *would* do,
    /// computed WITHOUT side effects. Shown in the permission prompt before the
    /// user approves. Edit/write tools return a ```diff block; most return None.
    fn preview(&self, _input: &Value, _ctx: &ToolContext) -> Option<String> {
        None
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

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order
            .iter()
            .filter_map(|n| self.tools.get(n).map(|t| t.spec()))
            .collect()
    }

    pub async fn execute(&self, name: &str, input: Value, ctx: &ToolContext) -> String {
        let tool = match self.tools.get(name) {
            Some(t) => t.clone(),
            None => return format!("error: unknown tool \"{}\"", name),
        };

        // Gate every call through the permission engine, if configured.
        if let Some(perms) = &self.permissions {
            let bash = if name == "bash" {
                let cmd = input
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
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
                return format!("error: permission denied for tool \"{}\"", name);
            }
        }

        tool.execute(input, ctx).await
    }
}
