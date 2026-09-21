use super::TestDir;
use crate::tools::file_tracker::FileTracker;
use crate::tools::registry::ToolContext;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) fn tool_context(cwd: &Path) -> ToolContext {
    ToolContext {
        cwd: cwd.to_string_lossy().into_owned(),
        files: Arc::new(FileTracker::new()),
        todos: Arc::new(crate::tools::todo::TodoStore::new()),
        jobs: crate::tools::jobs::JobRegistry::new(),
        user_asker: None,
        lsp: None,
        coord: None,
        permissions: None,
    }
}

pub(crate) struct FileFixture {
    pub dir: TestDir,
    pub ctx: ToolContext,
}

impl FileFixture {
    pub fn new() -> Self {
        let dir = TestDir::new();
        let ctx = tool_context(&dir);
        Self { dir, ctx }
    }

    pub fn write(&self, path: &str, content: &str) -> PathBuf {
        self.dir.write(path, content)
    }
}
