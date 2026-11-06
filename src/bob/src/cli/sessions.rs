use anyhow::{Context, Result};
use bob::application::SessionSelection;
use bob_kernel::{SessionCatalog, SessionId, SessionPage, SessionQuery, SessionSummary};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_segmentation::UnicodeSegmentation;

#[path = "session_render.rs"]
mod render;
#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

struct PickerItem {
    summary: SessionSummary,
    haystack: String,
}

impl PickerItem {
    fn new(summary: SessionSummary) -> Self {
        let haystack = format!(
            "{} {} {} {}",
            summary.title,
            summary.workspace,
            summary.id,
            summary.human_previews.join(" ")
        )
        .to_lowercase();
        Self { summary, haystack }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Scope {
    #[default]
    Local,
    All,
}

impl Scope {
    fn index(self) -> usize {
        match self {
            Self::Local => 0,
            Self::All => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Local => "Local",
            Self::All => "All",
        }
    }
}

#[derive(Default)]
struct SessionList {
    items: Vec<PickerItem>,
    loaded: bool,
    before: Option<i64>,
    error: Option<String>,
    generation: u64,
}

impl SessionList {
    fn accept(&mut self, generation: u64, result: Result<SessionPage>) {
        if generation != self.generation {
            return;
        }
        match result {
            Ok(page) => {
                self.items
                    .extend(page.sessions.into_iter().map(PickerItem::new));
                self.before = page.next_before;
                self.loaded = true;
                self.error = None;
            }
            Err(error) => {
                self.error = Some(format!("Cannot load sessions: {error:#}. Ctrl+R to retry."))
            }
        }
    }

    fn needs_page(&self, state: &PickerState, matches: usize) -> bool {
        self.error.is_none()
            && (!self.loaded
                || (self.before.is_some()
                    && (!state.query.is_empty()
                        || state.selected.saturating_add(state.page_size.max(1)) >= matches)))
    }

    fn reset(&mut self) {
        *self = Self {
            generation: self.generation + 1,
            ..Default::default()
        };
    }
}

async fn read_page(
    database: PathBuf,
    workspace: Option<String>,
    before: Option<i64>,
    limit: usize,
) -> Result<SessionPage> {
    tokio::task::spawn_blocking(move || {
        SessionCatalog::open(&database)?.sessions(SessionQuery {
            search: None,
            workspace,
            before,
            limit,
        })
    })
    .await
    .context("session catalog reader stopped")?
}

pub async fn pick_session(
    database: &Path,
    workspace: &Path,
    terminal_titles: bool,
) -> Result<SessionSelection> {
    if !database.try_exists()? {
        return Ok(SessionSelection::New);
    }
    let page = read_page(
        database.into(),
        Some(workspace.to_string_lossy().into_owned()),
        None,
        200,
    )
    .await?;
    if page.sessions.is_empty()
        && read_page(database.into(), None, None, 1)
            .await?
            .sessions
            .is_empty()
    {
        return Ok(SessionSelection::New);
    }
    let mut lists = [SessionList::default(), SessionList::default()];
    lists[0].accept(0, Ok(page));
    Ok(
        match run_picker(database, workspace, lists, terminal_titles).await? {
            Some(id) => SessionSelection::Id(id),
            None => SessionSelection::New,
        },
    )
}

#[derive(Default)]
struct PickerState {
    scope: Scope,
    query: String,
    selected: usize,
    scroll: usize,
    page_size: usize,
    error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Open(usize),
    Cancel,
    Refresh,
}

impl PickerState {
    fn filtered(&mut self, items: &[PickerItem]) -> Vec<usize> {
        let filtered: Vec<_> = (0..items.len())
            .filter(|&i| fuzzy_match(&self.query, &items[i].haystack))
            .collect();
        self.selected = self.selected.min(filtered.len().saturating_sub(1));
        self.scroll = self.scroll.min(self.selected);
        filtered
    }

    fn insert(&mut self, text: &str) {
        let text = render::single_line(text);
        for grapheme in text.graphemes(true) {
            if self.query.len() + grapheme.len() > 1024 {
                break;
            }
            self.query.push_str(grapheme);
        }
        self.selected = 0;
        self.scroll = 0;
        self.error = None;
    }

    fn choose(&mut self, item: &PickerItem) -> Option<SessionId> {
        match super::launch::validate_workspace(Path::new(&item.summary.workspace)) {
            Ok(_) => Some(item.summary.id.clone()),
            Err(error) => {
                self.error = Some(format!(
                    "Cannot resume: workspace unavailable. Restore its folder or select another session. {error:#}"
                ));
                None
            }
        }
    }

    fn key(&mut self, key: KeyEvent, filtered: &[usize]) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => return Some(Action::Cancel),
            (KeyCode::Char('c' | 'C'), m) if m.contains(KeyModifiers::CONTROL) => {
                return Some(Action::Cancel);
            }
            (KeyCode::Char('r' | 'R'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.selected = 0;
                self.scroll = 0;
                self.error = None;
                return Some(Action::Refresh);
            }
            (KeyCode::Tab | KeyCode::BackTab, _) => {
                self.scope = match self.scope {
                    Scope::Local => Scope::All,
                    Scope::All => Scope::Local,
                };
                self.selected = 0;
                self.scroll = 0;
            }
            (KeyCode::Enter, _) if self.page_size > 0 => {
                return filtered.get(self.selected).copied().map(Action::Open);
            }
            (KeyCode::Enter, _) => return None,
            (KeyCode::Up, _) => self.selected = self.selected.saturating_sub(1),
            (KeyCode::Down, _) => {
                self.selected = (self.selected + 1).min(filtered.len().saturating_sub(1));
            }
            (KeyCode::PageUp, _) => {
                self.selected = self.selected.saturating_sub(self.page_size.max(1))
            }
            (KeyCode::PageDown, _) => {
                self.selected = self
                    .selected
                    .saturating_add(self.page_size.max(1))
                    .min(filtered.len().saturating_sub(1));
            }
            (KeyCode::Home, _) => self.selected = 0,
            (KeyCode::End, _) => self.selected = filtered.len().saturating_sub(1),
            (KeyCode::Backspace, _) => {
                if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
                    self.query.truncate(index);
                }
                self.selected = 0;
                self.scroll = 0;
            }
            (KeyCode::Char('u' | 'U'), m) if m.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.selected = 0;
                self.scroll = 0;
            }
            (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
                self.insert(&c.to_string());
            }
            _ => return None,
        }
        self.error = None;
        None
    }
}

async fn run_picker(
    database: &Path,
    workspace: &Path,
    mut lists: [SessionList; 2],
    terminal_titles: bool,
) -> Result<Option<SessionId>> {
    use ratatui::backend::CrosstermBackend;
    use ratatui::Terminal;

    let mut restore = crate::ui::TerminalGuard::activate_with_titles(terminal_titles)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    restore.set_title(terminal.backend_mut(), "Bob · Resume session");
    let mut events = restore.events();
    let mut state = PickerState::default();
    let mut loads = tokio::task::JoinSet::new();
    let mut loading = None;
    let result = loop {
        let filtered = state.filtered(&lists[state.scope.index()].items);
        let list = &lists[state.scope.index()];
        if loads.is_empty() && list.needs_page(&state, filtered.len()) {
            let database = database.to_owned();
            let scope = state.scope;
            let workspace =
                (scope == Scope::Local).then(|| workspace.to_string_lossy().into_owned());
            let before = list.before;
            let generation = list.generation;
            loads.spawn(async move {
                (
                    scope,
                    generation,
                    read_page(database, workspace, before, 200).await,
                )
            });
            loading = Some(scope);
        }
        terminal.draw(|frame| {
            render::draw_picker(frame, &lists, &filtered, &mut state, workspace, loading)
        })?;
        tokio::select! {
            biased;
            event = events.next() => {
                let event = event.context("terminal input closed during session selection")??;
                match event {
                    Event::Key(key) => match state.key(key, &filtered) {
                        Some(Action::Cancel) => break None,
                        Some(Action::Open(index)) => {
                            if let Some(id) = state.choose(&lists[state.scope.index()].items[index]) {
                                break Some(id);
                            }
                        }
                        Some(Action::Refresh) => lists[state.scope.index()].reset(),
                        None => {}
                    },
                    Event::Paste(text) => state.insert(&text),
                    _ => {}
                }
            }
            Some(result) = loads.join_next(), if !loads.is_empty() => {
                let (scope, generation, page) = result.context("session picker loader stopped")?;
                lists[scope.index()].accept(generation, page);
                loading = None;
            }
        }
    };
    drop(loads);
    drop(events);
    drop(terminal);
    restore.restore()?;
    Ok(result)
}

fn time_ago(updated_at: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(updated_at.max(0) as u64);
    if updated_at <= 0 {
        "unknown age".into()
    } else if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

fn fuzzy_match(query: &str, hay: &str) -> bool {
    let mut q = query.chars().flat_map(char::to_lowercase).peekable();
    if q.peek().is_none() {
        return true;
    }
    for h in hay.chars().flat_map(char::to_lowercase) {
        if q.peek() == Some(&h) {
            q.next();
        }
    }
    q.peek().is_none()
}
