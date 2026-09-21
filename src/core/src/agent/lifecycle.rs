use crate::agent::team::{AgentHandle, AgentStatus};
use crate::core::events::{AgentEvent, EventBus};

/// A launched child must publish a terminal event even if its future is aborted
/// before its first poll. Create this guard before spawning and move it into the
/// child future; dropping an unfinished guard records cancellation (or panic).
pub(crate) struct SubagentLifecycle {
    bus: EventBus,
    agent_id: String,
    finished: bool,
    handle: Option<AgentHandle>,
    report_to: Option<AgentHandle>,
    team: Option<crate::agent::team::AgentRegistry>,
}

impl SubagentLifecycle {
    pub fn start(
        bus: EventBus,
        parent_id: String,
        agent_id: String,
        task: String,
        prompt: String,
    ) -> Self {
        bus.emit(AgentEvent::SubagentSpawn {
            parent_id,
            agent_id: agent_id.clone(),
            task,
            prompt,
        });
        Self {
            bus,
            agent_id,
            finished: false,
            handle: None,
            report_to: None,
            team: None,
        }
    }

    pub fn with_handle(mut self, handle: AgentHandle) -> Self {
        self.handle = Some(handle);
        self
    }

    pub fn with_team(mut self, team: crate::agent::team::AgentRegistry) -> Self {
        self.team = Some(team);
        self
    }

    pub fn with_report_to(mut self, parent: Option<AgentHandle>) -> Self {
        self.report_to = parent;
        self
    }

    pub fn finish(&mut self, failed: bool, cancelled: bool) -> AgentStatus {
        self.finish_with_output(
            failed,
            cancelled,
            if cancelled {
                "[cancelled]"
            } else if failed {
                "agent failed"
            } else {
                "agent finished"
            },
        )
    }

    pub fn finish_with_output(
        &mut self,
        failed: bool,
        cancelled: bool,
        output: &str,
    ) -> AgentStatus {
        if self.finished {
            return self
                .handle
                .as_ref()
                .map_or(AgentStatus::Done, AgentHandle::status);
        }
        let _publication = self.handle.as_ref().map(AgentHandle::begin_terminal_event);
        let status = if let Some(handle) = &self.handle {
            handle.complete(
                failed,
                cancelled,
                self.report_to.as_ref().map(|parent| (parent, output)),
            )
        } else if cancelled {
            AgentStatus::Cancelled
        } else if failed {
            AgentStatus::Failed
        } else {
            AgentStatus::Done
        };
        self.finished = true;
        if status != AgentStatus::Done {
            if let Some(team) = &self.team {
                team.stop_descendants(&self.agent_id);
            }
        }
        self.bus.emit(AgentEvent::SubagentDone {
            agent_id: self.agent_id.clone(),
            failed: status == AgentStatus::Failed,
            cancelled: status == AgentStatus::Cancelled,
        });
        status
    }
}

impl Drop for SubagentLifecycle {
    fn drop(&mut self) {
        if !self.finished {
            let failed = std::thread::panicking();
            self.finish(failed, !failed);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::lifecycle::SubagentLifecycle;
    use crate::agent::team::{mailbox, AgentRegistry, AgentStatus};
    use crate::core::events::{AgentEvent, EventBus};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    #[test]
    fn terminal_publication_blocks_settlement_and_name_reuse_without_roster_deadlock() {
        let team = AgentRegistry::new();
        let (_root_inbox, tx) = mailbox();
        team.register("root".into(), 0, String::new(), tx);
        let (_branch_inbox, tx) = mailbox();
        let branch = team.register("branch".into(), 1, "root".into(), tx);
        let (_worker_inbox, tx) = mailbox();
        let worker = team.register("worker".into(), 2, "branch".into(), tx);
        branch.complete(false, false, None);
        let bus = EventBus::new();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let observed_team = team.clone();
        let observed_worker = worker.clone();
        bus.on(Arc::new(move |event| {
            if matches!(event, AgentEvent::SubagentDone { .. }) {
                assert_eq!(observed_worker.status(), AgentStatus::Done);
                assert!(!observed_worker.is_settled());
                assert!(observed_team
                    .roster()
                    .iter()
                    .any(|(name, _, status)| { name == "worker" && *status == AgentStatus::Done }));
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap();
            }
        }));
        let mut lifecycle = SubagentLifecycle::start(
            bus,
            "branch".into(),
            "worker".into(),
            "review".into(),
            "review code".into(),
        )
        .with_handle(worker.clone());
        let (finished_tx, finished_rx) = mpsc::channel();
        let finish = std::thread::spawn(move || {
            lifecycle.finish(false, false);
            finished_tx.send(()).unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(worker.status(), AgentStatus::Done);
        assert!(!worker.is_settled());
        assert!(team.has_unsettled_descendants("root"));
        assert!(team.has_unsettled_descendants("branch"));
        assert!(!team.has_unsettled_descendants("worker"));
        assert!(team.has_running_children("branch"));
        assert_eq!(team.active_len(), 2);
        assert!(team.stop("root", "worker").is_err());
        assert!(!team.send("worker", "root", "too late"));
        for name in ["worker", "branch"] {
            assert!(team.name_in_use(name));
            let (_inbox, tx) = mailbox();
            assert!(team
                .try_register(name.into(), 1, "root".into(), tx)
                .is_err());
        }
        release_tx.send(()).unwrap();
        finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        finish.join().unwrap();
        assert!(worker.is_settled());
        assert!(!team.has_unsettled_descendants("root"));
        assert!(!team.has_running_children("branch"));
        assert_eq!(team.active_len(), 1);
        for name in ["worker", "branch"] {
            assert!(!team.name_in_use(name));
            let (_inbox, tx) = mailbox();
            team.try_register(name.into(), 1, "root".into(), tx)
                .unwrap();
        }
    }

    #[test]
    fn terminal_publication_guard_releases_on_callback_panic() {
        let team = AgentRegistry::new();
        let (_inbox, tx) = mailbox();
        let worker = team.register("worker".into(), 1, "root".into(), tx);
        let bus = EventBus::new();
        let observed = worker.clone();
        bus.on(Arc::new(move |event| {
            if matches!(event, AgentEvent::SubagentDone { .. }) {
                assert_eq!(observed.status(), AgentStatus::Failed);
                assert!(!observed.is_settled());
                panic!("fixture callback panic");
            }
        }));
        let mut lifecycle = SubagentLifecycle::start(
            bus,
            "root".into(),
            "worker".into(),
            "review".into(),
            "review code".into(),
        )
        .with_handle(worker.clone());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            lifecycle.finish(true, false);
        }))
        .is_err());
        assert!(worker.is_settled());
        assert!(!team.name_in_use("worker"));
        assert!(!team.has_unsettled_descendants("root"));
        drop(lifecycle);
    }

    #[test]
    fn terminal_event_is_emitted_once() {
        let bus = EventBus::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        bus.on(Arc::new(move |event| {
            captured.lock().unwrap().push(event.clone())
        }));
        let mut lifecycle = SubagentLifecycle::start(
            bus,
            "root".into(),
            "task_1".into(),
            "review".into(),
            "review code".into(),
        );
        lifecycle.finish(false, false);
        drop(lifecycle);
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AgentEvent::SubagentSpawn { .. }));
        assert!(matches!(
            events[1],
            AgentEvent::SubagentDone {
                failed: false,
                cancelled: false,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn abort_before_first_poll_still_finishes_the_agent() {
        let bus = EventBus::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        bus.on(Arc::new(move |event| {
            captured.lock().unwrap().push(event.clone())
        }));
        let lifecycle = SubagentLifecycle::start(
            bus,
            "root".into(),
            "task_1".into(),
            "review".into(),
            "review code".into(),
        );
        let handle = tokio::spawn(async move {
            let _lifecycle = lifecycle;
            std::future::pending::<()>().await;
        });
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[1],
            AgentEvent::SubagentDone {
                failed: false,
                cancelled: true,
                ..
            }
        ));
    }

    #[test]
    fn finish_publishes_one_consistent_terminal_state_and_scopes_descendant_stops() {
        use crate::agent::team::AgentMessageKind;

        for (failed, cancelled, stop_first, expected) in [
            (false, false, false, AgentStatus::Done),
            (true, false, false, AgentStatus::Failed),
            (false, true, false, AgentStatus::Cancelled),
            (true, true, false, AgentStatus::Cancelled),
            (false, false, true, AgentStatus::Cancelled),
        ] {
            let team = AgentRegistry::new();
            let (mut parent_inbox, tx) = mailbox();
            let parent = team.register("root".into(), 0, String::new(), tx);
            let (_inbox, tx) = mailbox();
            let worker = team.register("worker".into(), 1, "root".into(), tx);
            let (_inbox, tx) = mailbox();
            let child = team.register("child".into(), 2, "worker".into(), tx);
            let (_inbox, tx) = mailbox();
            let sibling = team.register("sibling".into(), 1, "root".into(), tx);
            let bus = EventBus::new();
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            bus.on(Arc::new(move |event| {
                captured.lock().unwrap().push(event.clone())
            }));
            let mut lifecycle = SubagentLifecycle::start(
                bus,
                "root".into(),
                "worker".into(),
                "review".into(),
                "review code".into(),
            )
            .with_handle(worker.clone())
            .with_report_to(Some(parent.clone()))
            .with_team(team.clone());
            if stop_first {
                team.stop("root", "worker").unwrap();
            }
            assert_eq!(
                lifecycle.finish_with_output(failed, cancelled, "partial findings"),
                expected
            );
            assert_eq!(
                lifecycle.finish_with_output(!failed, !cancelled, "duplicate"),
                expected
            );
            drop(lifecycle);
            assert_eq!(worker.status(), expected);
            assert!(worker.is_settled());
            assert_eq!(
                child.cancellation_requested(),
                expected != AgentStatus::Done
            );
            assert!(!sibling.cancellation_requested());
            assert!(!parent.cancellation_requested());
            let reports = parent_inbox.drain();
            assert_eq!(reports.len(), 1);
            assert_eq!(reports[0].from, "worker");
            assert_eq!(
                reports[0].kind,
                AgentMessageKind::Completion {
                    status: expected.clone()
                }
            );
            if expected != AgentStatus::Cancelled {
                assert_eq!(reports[0].text, "finished: partial findings");
            }
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 2);
            assert!(
                matches!(&events[0], AgentEvent::SubagentSpawn { parent_id, agent_id, task, prompt }
                if parent_id == "root" && agent_id == "worker" && task == "review" && prompt == "review code")
            );
            assert!(
                matches!(&events[1], AgentEvent::SubagentDone { agent_id, failed, cancelled }
                if agent_id == "worker" && *failed == (expected == AgentStatus::Failed)
                    && *cancelled == (expected == AgentStatus::Cancelled))
            );
        }
    }

    #[test]
    fn dropping_unfinished_guard_distinguishes_cancellation_from_unwinding() {
        for panicking in [false, true] {
            let bus = EventBus::new();
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            bus.on(Arc::new(move |event| {
                captured.lock().unwrap().push(event.clone())
            }));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = SubagentLifecycle::start(
                    bus,
                    "root".into(),
                    "worker".into(),
                    "review".into(),
                    "code".into(),
                );
                if panicking {
                    panic!("fixture work panic");
                }
            }));
            assert_eq!(result.is_err(), panicking);
            let events = events.lock().unwrap();
            assert_eq!(events.len(), 2);
            assert!(
                matches!(&events[1], AgentEvent::SubagentDone { agent_id, failed, cancelled }
                if agent_id == "worker" && *failed == panicking && *cancelled != panicking)
            );
        }
    }

    #[test]
    fn old_completion_events_default_to_not_cancelled() {
        let event: AgentEvent =
            serde_json::from_str(r#"{"kind":"SubagentDone","agent_id":"task_1","failed":false}"#)
                .unwrap();
        assert!(matches!(
            event,
            AgentEvent::SubagentDone {
                cancelled: false,
                ..
            }
        ));
    }
}
