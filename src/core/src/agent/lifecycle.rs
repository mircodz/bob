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
        }
    }

    pub fn with_handle(mut self, handle: AgentHandle) -> Self {
        self.handle = Some(handle);
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
    use super::*;
    use std::sync::{Arc, Mutex};

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
