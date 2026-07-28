pub mod builtin;
pub mod diff;
pub mod edit;
pub mod file_tracker;
pub mod interact;
pub mod jobs;
pub mod registry;
pub mod search;
pub mod task;
pub mod todo;
pub mod todo_write;
pub mod web;

use crate::tools::builtin::{BashTool, ListDirTool, ReadFileTool, WriteFileTool};
use crate::tools::edit::{EditFileTool, MultiEditTool};
use crate::tools::interact::{AskUserTool, ExitPlanTool};
use crate::tools::jobs::{JobOutputTool, JobStatusTool};
use crate::tools::registry::Tool;
use crate::tools::search::{GlobTool, GrepTool};
use crate::tools::todo_write::TodoWriteTool;
use crate::tools::web::WebFetchTool;
use std::sync::Arc;

/// The full built-in tool set (not including `task`, which is wired separately).
pub fn builtin_tools() -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ReadFileTool),
        Arc::new(WriteFileTool),
        Arc::new(EditFileTool),
        Arc::new(MultiEditTool),
        Arc::new(ListDirTool),
        Arc::new(GlobTool),
        Arc::new(GrepTool),
        Arc::new(BashTool),
        Arc::new(WebFetchTool),
        Arc::new(TodoWriteTool),
        Arc::new(JobStatusTool),
        Arc::new(JobOutputTool),
        Arc::new(AskUserTool),
        Arc::new(ExitPlanTool),
    ]
}
