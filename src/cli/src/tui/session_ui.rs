use super::{scrollback, team, widgets, App};
use bob_core::tools::jobs::JobStatus;

impl App {
    pub(super) fn snapshot_session(&self, session: &mut bob_core::core::session::Session) {
        session.grants = self.permissions.export_grants();
        session.todos = self
            .todos
            .as_ref()
            .map(|todos| todos.items())
            .unwrap_or_default();
        session.agent_threads = self.teams.to_persisted();
        session.workflows = self.view.to_persisted_workflows();
        session.updated_at = super::now_stamp();
    }

    pub(super) fn apply_session_event(
        &mut self,
        session: &mut bob_core::core::session::Session,
        event: &bob_core::core::events::AgentEvent,
    ) {
        if let bob_core::core::events::AgentEvent::Completion {
            agent_id,
            model,
            usage,
        } = event
        {
            let entry = bob_core::core::usage::UsageEntry {
                ts: super::unix_now(),
                session_id: session.id.clone(),
                provider: self.provider_id.clone(),
                model: model.clone(),
                agent_id: agent_id.clone(),
                usage: *usage,
            };
            session.usage.push(entry.clone());
            self.session_usage.add(usage);
            tokio::task::spawn_blocking(move || {
                let _ = bob_core::core::usage::append_global(&entry);
            });
        }
        self.apply_agent_event(event);
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn transition_session(
        &mut self,
        fresh: bool,
        agent: &std::sync::Arc<tokio::sync::Mutex<bob_core::agent::agent::Agent>>,
        owner: &bob_core::core::session_runtime::SessionOwner,
        session: &mut bob_core::core::session::Session,
        subscription: &mut bob_core::core::events::Subscription,
        cwd: &std::path::Path,
    ) {
        if self.session_work_pending() {
            self.notify("finish or stop active agents and jobs first");
            return;
        }
        let Ok(mut agent) = agent.try_lock() else {
            self.notify("busy — try again after this turn");
            return;
        };
        if !self.jobs.is_settled() {
            self.notify("background cleanup is still settling — retry shortly");
            return;
        }
        session.messages = agent.messages().to_vec();
        self.snapshot_session(session);
        let token = owner.token();
        let owner_task = owner.clone();
        let mut snapshot = session.clone();
        let next = fresh.then(|| {
            bob_core::core::session::new_session(
                &self.provider_id,
                uuid::Uuid::new_v4().to_string(),
                super::now_stamp(),
                cwd.to_string_lossy().to_string(),
            )
        });
        let result = tokio::task::spawn_blocking(move || {
            let result = if let Some(next) = next {
                owner_task
                    .checkpoint(token, snapshot.clone())
                    .and_then(|_| owner_task.switch(&next))
                    .map(|_| {
                        snapshot = next;
                    })
            } else {
                owner_task.reset(&mut snapshot)
            };
            (snapshot, result)
        })
        .await;
        // Reset invalidates its listener even when persistence fails.
        *subscription = owner.subscribe(&self.bus);
        match result {
            Ok((snapshot, result)) => {
                match &result {
                    Err(error) if fresh || snapshot.context_epoch == session.context_epoch => {
                        self.notify(format!("session transition failed: {error}"));
                        return;
                    }
                    _ => {}
                }
                let _ = self.jobs.clear_settled();
                *session = snapshot;
                self.permissions.import_grants(session.grants.clone());
                agent.load_history(Vec::new());
                agent.discard_pending_coordination();
                let _ = self.jobs.take_finished();
                self.reset_session_ui();
                self.session_usage = bob_core::core::usage::total_of(&session.usage);
                if let Err(error) = result {
                    self.notify(format!("couldn't save cleared context: {error}"));
                }
                self.notify(if fresh {
                    format!("started a fresh session ({}).", session.id)
                } else {
                    "context cleared — the agent has forgotten the conversation.".into()
                });
            }
            Err(error) => self.notify(format!("session transition failed: {error}")),
        }
    }
    pub(super) fn session_work_pending(&self) -> bool {
        self.running
            || self.agent_team.has_unsettled_descendants("root")
            || self
                .jobs
                .list()
                .iter()
                .any(|(_, _, _, status)| *status == JobStatus::Running)
    }

    pub(super) fn reset_session_ui(&mut self) {
        self.view.clear();
        self.teams = team::AgentTranscripts::new();
        self.scrollback = scrollback::ScrollbackRenderer::new();
        self.focused_scrollback = scrollback::ScrollbackRenderer::new();
        self.focused_agent = None;
        self.sidebar = widgets::SelectList::new();
        self.sidebar_rows = None;
        self.workflow_view = None;
        self.wf_view_agents = None;
        self.agent_control_notice = None;
        self.queue.clear();
        self.perm_queue.clear();
        self.pending_query = None;
        self.query_queue.clear();
        self.compaction_completion = None;
        self.context_usage = None;
        self.input_scroll = 0;
        self.current_prompt = None;
        self.turn_started = None;
        self.detached_job = None;
        self.session_usage = Default::default();
        if let Some(todos) = &self.todos {
            todos.set(Vec::new());
        }
    }

    pub(super) fn focused_activity(&self) -> (bool, u64) {
        if let Some(id) = self.focused_agent.as_deref() {
            return self
                .teams
                .get(id)
                .map(|thread| {
                    (
                        thread.status == team::ThreadStatus::Running,
                        thread.started.elapsed().as_secs(),
                    )
                })
                .unwrap_or((false, 0));
        }
        (
            self.running || self.view.busy,
            self.turn_started
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::test_app;
    use bob_core::core::events::AgentEvent;
    use std::time::{Duration, Instant};

    #[test]
    fn reset_clears_session_state_and_drops_prompt_responders_but_keeps_preferences() {
        let mut app = test_app();
        app.view.push_notice("old transcript".into());
        app.teams
            .on_spawn("worker", "root", "old task", "old prompt");
        app.focused_agent = Some("worker".into());
        app.sidebar.selected = 1;
        app.sidebar.scroll = 1;
        app.sidebar_rows = Some(vec![(3, "worker".into())]);
        app.workflow_view = Some(team::WorkflowView::default());
        app.wf_view_agents = Some(vec![(3, "worker".into())]);
        app.agent_control_notice = Some(("worker".into(), "stopping".into()));
        app.queue.push_back("queued".into());
        app.compaction_completion = Some(1);
        app.context_usage = Some((900, 1000));
        app.input_scroll = 4;
        app.current_prompt = Some("old prompt".into());
        app.turn_started = Some(Instant::now());
        app.detached_job = Some("old job".into());
        app.input.set("unsent draft");
        app.model_label = "openai:model".into();
        app.sidebar_open = true;
        app.show_todos = false;
        app.open_model_picker(vec!["model".into()]);
        app.open_model_picker(vec!["another model".into()]);
        let (resp, mut rx) = tokio::sync::oneshot::channel();
        app.perm_queue.push_back(crate::tui::PendingPerm {
            title: "old permission".into(),
            detail: String::new(),
            preview: None,
            options: Vec::new(),
            list: widgets::SelectList::new(),
            resp,
        });

        app.reset_session_ui();

        assert!(app.view.cells.is_empty());
        assert!(app.teams.is_empty());
        assert!(app.focused_agent.is_none());
        assert_eq!((app.sidebar.selected, app.sidebar.scroll), (0, 0));
        assert!(app.sidebar_rows.is_none());
        assert!(app.workflow_view.is_none());
        assert!(app.wf_view_agents.is_none());
        assert!(app.agent_control_notice.is_none());
        assert!(app.queue.is_empty());
        assert!(app.perm_queue.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        assert!(app.pending_query.is_none());
        assert!(app.query_queue.is_empty());
        assert!(app.compaction_completion.is_none());
        assert!(app.context_usage.is_none());
        assert_eq!(app.input_scroll, 0);
        assert!(app.current_prompt.is_none());
        assert!(app.turn_started.is_none());
        assert!(app.detached_job.is_none());
        assert_eq!(app.input.text(), "unsent draft");
        assert_eq!(app.model_label, "openai:model");
        assert!(app.sidebar_open);
        assert!(!app.show_todos);
    }

    #[test]
    fn focused_activity_uses_selected_agent_instead_of_root_busy_state() {
        let mut app = test_app();
        assert_eq!(app.focused_activity(), (false, 0));
        app.running = true;
        app.turn_started = Some(Instant::now() - Duration::from_secs(10));
        let (running, elapsed) = app.focused_activity();
        assert!(running);
        assert!(elapsed >= 10);
        app.teams.on_spawn("worker", "root", "task", "");
        app.focused_agent = Some("worker".into());
        assert!(app.focused_activity().0);
        app.teams.on_done("worker", true);
        assert!(
            !app.focused_activity().0,
            "a failed child must not inherit root activity"
        );
        app.focused_agent = Some("missing".into());
        assert_eq!(app.focused_activity(), (false, 0));
        app.focused_agent = None;
        app.running = false;
        app.view.busy = true;
        assert!(app.focused_activity().0);
    }

    #[test]
    fn session_context_meter_ignores_child_warnings() {
        let mut app = test_app();
        let mut session = bob_core::core::session::new_session(
            "mock",
            "unit-test".into(),
            "0".into(),
            "/test".into(),
        );
        for (agent_id, used_tokens, expected) in [
            ("worker", 700, None),
            ("root", 850, Some((850, 1000))),
            ("worker", 950, Some((850, 1000))),
        ] {
            app.apply_session_event(
                &mut session,
                &AgentEvent::ContextWarning {
                    agent_id: agent_id.into(),
                    used_tokens,
                    context_window: 1000,
                    pct: 85,
                },
            );
            assert_eq!(app.context_usage, expected);
            assert!(session.usage.is_empty());
        }
    }
}
