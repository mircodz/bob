use super::state::App;
use bob_kernel::{SessionId, SessionSnapshot};
use bob_ui::terminal::title::session_title;
use std::{collections::HashMap, path::Path};

#[derive(Default)]
pub(super) struct SessionTitles {
    active: Option<SessionId>,
    cached: HashMap<SessionId, CachedTitle>,
}

struct CachedTitle {
    key: (u64, i64),
    text: String,
}

impl SessionTitles {
    pub fn select(&mut self, session: SessionId) {
        self.active = Some(session);
    }

    pub fn accept(&mut self, session: &SessionSnapshot, key: (u64, i64)) {
        let key = (key.0, session.title_revision);
        if self
            .cached
            .get(&session.id)
            .is_some_and(|cached| cached.key >= key)
        {
            return;
        }
        let title = bob_ui::terminal::title::sanitize(&session.title);
        let text = if title.is_empty() {
            workspace_title(Path::new(&session.workspace))
        } else {
            session_title(&title)
        };
        self.cached
            .insert(session.id.clone(), CachedTitle { key, text });
    }

    fn current(&self, runtime: u64) -> Option<&str> {
        let cached = self.cached.get(self.active.as_ref()?)?;
        (cached.key.0 == runtime).then_some(cached.text.as_str())
    }
}

fn workspace_title(workspace: &Path) -> String {
    session_title(
        &workspace
            .file_name()
            .unwrap_or(workspace.as_os_str())
            .to_string_lossy(),
    )
}

impl App {
    pub fn terminal_title(&self) -> String {
        self.titles.current(self.runtime_epoch).map_or_else(
            || {
                self.workspace
                    .as_deref()
                    .map_or_else(|| "Bob".into(), workspace_title)
            },
            str::to_owned,
        )
    }
}
