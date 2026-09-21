//! Agent events decouple core execution from transcript rendering and replay.

use crate::core::types::{Message, ToolOutput, Usage};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// Persisted with a `kind` tag. Unknown fields are ignored; unknown variants
/// decode to [`AgentEvent::Unknown`] and replay as a no-op.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum AgentEvent {
    /// Emitted when a user prompt enters history; empty coordination wakes emit nothing.
    UserPrompt {
        agent_id: String,
        text: String,
    },
    /// Preserves pre-reset snapshots without contributing to active context replay.
    ContextArchive {
        agent_id: String,
        snapshot: Box<crate::core::session::Session>,
        stored_snapshot: Option<Box<crate::core::session::Session>>,
    },
    /// Starts a new logical context without deleting the archived event log.
    ContextReset {
        agent_id: String,
        epoch: u64,
    },
    TurnStart {
        agent_id: String,
    },
    TextDelta {
        agent_id: String,
        text: String,
    },
    Message {
        agent_id: String,
        message: Message,
    },
    ToolCall {
        agent_id: String,
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        agent_id: String,
        tool_use_id: String,
        output: ToolOutput,
        is_error: bool,
    },
    SubagentSpawn {
        parent_id: String,
        agent_id: String,
        task: String,
        /// Full delegated instructions, unlike the short `task` label.
        prompt: String,
    },
    SubagentDone {
        agent_id: String,
        failed: bool,
        #[serde(default)]
        cancelled: bool,
    },
    /// Accepted user delivery; display-only until consumed as an AgentMessage.
    UserMessageQueued {
        to: String,
        text: String,
    },
    AgentMessage {
        to: String,
        from: String,
        text: String,
    },
    Compaction {
        agent_id: String,
        before_tokens: usize,
        after_tokens: usize,
        /// Rebuilds compacted provider context on replay; empty in legacy events.
        #[serde(default)]
        summary: String,
        /// Replay replaces `working[..replaced_upto]` with the summary message.
        #[serde(default)]
        replaced_upto: usize,
    },
    WorkflowPhase {
        workflow_id: String,
        title: String,
        index: usize,
        total: usize,
    },
    WorkflowLog {
        workflow_id: String,
        message: String,
    },
    /// Emitted at most once per threshold per direction.
    ContextWarning {
        agent_id: String,
        used_tokens: usize,
        context_window: usize,
        /// The crossed threshold as a percentage (70, 85, 95).
        pct: u8,
    },
    /// A generation or compaction request will retry after a cancellable 10-second delay.
    StreamRetry {
        agent_id: String,
        /// The upcoming retry number (1-based), excluding the original request.
        attempt: u32,
        /// The maximum number of retries after the original request.
        max_attempts: u32,
    },
    /// Usage for one provider completion, not the entire turn.
    Completion {
        agent_id: String,
        model: String,
        usage: Usage,
    },
    TurnEnd {
        agent_id: String,
        usage: Usage,
    },
    Error {
        agent_id: String,
        message: String,
    },
    #[serde(other)]
    Unknown,
}

impl AgentEvent {
    /// The variant tag, stored in the log's `kind` column for cheap filtering
    /// and debugging without parsing the JSON payload.
    pub fn kind(&self) -> &'static str {
        match self {
            AgentEvent::UserPrompt { .. } => "UserPrompt",
            AgentEvent::ContextArchive { .. } => "ContextArchive",
            AgentEvent::ContextReset { .. } => "ContextReset",
            AgentEvent::TurnStart { .. } => "TurnStart",
            AgentEvent::TextDelta { .. } => "TextDelta",
            AgentEvent::Message { .. } => "Message",
            AgentEvent::ToolCall { .. } => "ToolCall",
            AgentEvent::ToolResult { .. } => "ToolResult",
            AgentEvent::SubagentSpawn { .. } => "SubagentSpawn",
            AgentEvent::SubagentDone { .. } => "SubagentDone",
            AgentEvent::UserMessageQueued { .. } => "UserMessageQueued",
            AgentEvent::AgentMessage { .. } => "AgentMessage",
            AgentEvent::Compaction { .. } => "Compaction",
            AgentEvent::WorkflowPhase { .. } => "WorkflowPhase",
            AgentEvent::WorkflowLog { .. } => "WorkflowLog",
            AgentEvent::ContextWarning { .. } => "ContextWarning",
            AgentEvent::StreamRetry { .. } => "StreamRetry",
            AgentEvent::Completion { .. } => "Completion",
            AgentEvent::TurnEnd { .. } => "TurnEnd",
            AgentEvent::Error { .. } => "Error",
            AgentEvent::Unknown => "Unknown",
        }
    }
}

pub type EventListener = Arc<dyn Fn(&AgentEvent) + Send + Sync>;

#[derive(Clone, Default)]
pub struct EventBus {
    listeners: Arc<Mutex<Vec<EventListener>>>,
    keyed: Arc<Mutex<Vec<(u64, EventListener)>>>,
    next_id: Arc<std::sync::atomic::AtomicU64>,
}

/// Dropping the subscription removes its listener from the bus.
pub struct Subscription {
    bus: EventBus,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus
            .keyed
            .lock()
            .unwrap()
            .retain(|(id, _)| *id != self.id);
    }
}

impl EventBus {
    pub fn new() -> Self {
        EventBus::default()
    }

    pub fn on(&self, listener: EventListener) {
        self.listeners.lock().unwrap().push(listener);
    }

    #[must_use]
    pub fn subscribe(&self, listener: EventListener) -> Subscription {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.keyed.lock().unwrap().push((id, listener));
        Subscription {
            bus: self.clone(),
            id,
        }
    }

    pub fn emit(&self, event: AgentEvent) {
        // Invoke outside the locks so listeners can reenter the bus without deadlocking.
        let listeners = {
            let guard = self.listeners.lock().unwrap();
            guard.clone()
        };
        for l in listeners.iter() {
            l(&event);
        }
        let keyed = {
            let guard = self.keyed.lock().unwrap();
            guard.clone()
        };
        for (_, l) in keyed.iter() {
            l(&event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentEvent, EventBus};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn completes_without_deadlock(scenario: impl FnOnce() + Send + 'static) {
        let (done, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            scenario();
            done.send(()).unwrap();
        });
        match completion.recv_timeout(Duration::from_secs(5)) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => worker.join().unwrap(),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("event callback deadlocked"),
        }
    }

    #[test]
    fn dropping_subscription_removes_only_its_listener_across_bus_clones() {
        let bus = EventBus::new();
        let counts = Arc::new(Mutex::new([0; 3]));
        let permanent = counts.clone();
        bus.on(Arc::new(move |_| permanent.lock().unwrap()[0] += 1));
        let first = counts.clone();
        let subscription = bus.subscribe(Arc::new(move |_| first.lock().unwrap()[1] += 1));
        let second = counts.clone();
        let _other = bus.subscribe(Arc::new(move |_| second.lock().unwrap()[2] += 1));
        bus.emit(AgentEvent::Unknown);
        drop(subscription);
        bus.clone().emit(AgentEvent::Unknown);
        assert_eq!(*counts.lock().unwrap(), [2, 1, 2]);
    }

    #[test]
    fn listeners_can_emit_reentrantly_without_holding_bus_locks() {
        completes_without_deadlock(|| {
            for removable in [false, true] {
                let bus = Arc::new(EventBus::new());
                let seen = Arc::new(Mutex::new(Vec::new()));
                let nested_bus = Arc::downgrade(&bus);
                let nested_seen = seen.clone();
                let listener: super::EventListener = Arc::new(move |event| {
                    nested_seen.lock().unwrap().push(event.kind());
                    if matches!(event, AgentEvent::TurnStart { .. }) {
                        nested_bus.upgrade().unwrap().emit(AgentEvent::Unknown);
                    }
                });
                let _subscription = if removable {
                    Some(bus.subscribe(listener))
                } else {
                    bus.on(listener);
                    None
                };
                bus.emit(AgentEvent::TurnStart {
                    agent_id: "root".into(),
                });
                assert_eq!(*seen.lock().unwrap(), ["TurnStart", "Unknown"]);
            }
        });
    }

    #[test]
    fn event_replay_accepts_old_fields_and_unknown_future_variants() {
        let done: AgentEvent = serde_json::from_value(json!({
            "kind": "SubagentDone", "agent_id": "child", "failed": false, "future_field": 1,
        }))
        .unwrap();
        assert!(
            matches!(done, AgentEvent::SubagentDone { agent_id, failed: false, cancelled: false } if agent_id == "child")
        );
        let compact: AgentEvent = serde_json::from_value(json!({
            "kind": "Compaction", "agent_id": "root", "before_tokens": 100, "after_tokens": 10,
        }))
        .unwrap();
        assert!(
            matches!(compact, AgentEvent::Compaction { agent_id, before_tokens: 100, after_tokens: 10, summary, replaced_upto: 0 } if agent_id == "root" && summary.is_empty())
        );
        let future: AgentEvent =
            serde_json::from_value(json!({"kind": "FutureEvent", "payload": [1]})).unwrap();
        assert!(matches!(future, AgentEvent::Unknown));
    }

    #[test]
    fn subscriptions_receive_future_payloads_once_in_order_and_release_captures() {
        let bus = EventBus::new();
        bus.emit(AgentEvent::TextDelta {
            agent_id: "old".into(),
            text: "not replayed".into(),
        });
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let captured = Arc::new(());
        let weak = Arc::downgrade(&captured);
        let subscription = bus.subscribe(Arc::new(move |event| {
            let _keep_alive = &captured;
            if let AgentEvent::TextDelta { agent_id, text } = event {
                observed
                    .lock()
                    .unwrap()
                    .push((agent_id.clone(), text.clone()));
            }
        }));
        let clone = bus.clone();
        drop(bus);
        assert!(weak.upgrade().is_some());
        clone.emit(AgentEvent::TextDelta {
            agent_id: "root".into(),
            text: "first".into(),
        });
        clone.emit(AgentEvent::TextDelta {
            agent_id: "child".into(),
            text: "second".into(),
        });
        drop(subscription);
        assert!(weak.upgrade().is_none());
        clone.emit(AgentEvent::TextDelta {
            agent_id: "root".into(),
            text: "unsubscribed".into(),
        });
        assert_eq!(
            *seen.lock().unwrap(),
            [
                ("root".to_string(), "first".to_string()),
                ("child".to_string(), "second".to_string()),
            ]
        );
    }

    #[test]
    fn permanent_listeners_live_until_last_bus_handle_is_dropped() {
        let bus = EventBus::new();
        let state = Arc::new(Mutex::new(0));
        let weak = Arc::downgrade(&state);
        bus.on(Arc::new(move |_| *state.lock().unwrap() += 1));
        let clone = bus.clone();
        drop(bus);
        clone.emit(AgentEvent::Unknown);
        assert_eq!(*weak.upgrade().unwrap().lock().unwrap(), 1);
        drop(clone);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn a_listener_can_unsubscribe_itself_during_delivery() {
        completes_without_deadlock(|| {
            let bus = EventBus::new();
            let slot = Arc::new(Mutex::new(None::<super::Subscription>));
            let callback_slot = slot.clone();
            let calls = Arc::new(Mutex::new(0));
            let callback_calls = calls.clone();
            *slot.lock().unwrap() = Some(bus.subscribe(Arc::new(move |_| {
                *callback_calls.lock().unwrap() += 1;
                drop(callback_slot.lock().unwrap().take());
            })));
            bus.emit(AgentEvent::Unknown);
            bus.emit(AgentEvent::Unknown);
            assert_eq!(*calls.lock().unwrap(), 1);
            assert!(slot.lock().unwrap().is_none());
        });
    }

    #[test]
    fn callbacks_can_register_listeners_for_later_emits() {
        completes_without_deadlock(|| {
            for removable in [false, true] {
                let bus = Arc::new(EventBus::new());
                let nested_bus = Arc::downgrade(&bus);
                let registered = Arc::new(Mutex::new(None::<super::Subscription>));
                let callback_registered = registered.clone();
                let seen = Arc::new(Mutex::new(Vec::new()));
                let callback_seen = seen.clone();
                let listener: super::EventListener = Arc::new(move |event| {
                    if matches!(event, AgentEvent::TurnStart { .. }) {
                        let observed = callback_seen.clone();
                        let listener: super::EventListener = Arc::new(move |event| {
                            observed.lock().unwrap().push(event.kind());
                        });
                        let bus = nested_bus.upgrade().unwrap();
                        if removable {
                            *callback_registered.lock().unwrap() = Some(bus.subscribe(listener));
                        } else {
                            bus.on(listener);
                        }
                    }
                });
                let _subscription = if removable {
                    Some(bus.subscribe(listener))
                } else {
                    bus.on(listener);
                    None
                };
                bus.emit(AgentEvent::TurnStart {
                    agent_id: "root".into(),
                });
                assert!(seen.lock().unwrap().is_empty());
                bus.emit(AgentEvent::Unknown);
                assert_eq!(*seen.lock().unwrap(), ["Unknown"]);
                drop(registered.lock().unwrap().take());
            }
        });
    }

    #[test]
    fn known_events_reject_missing_or_mistyped_required_fields() {
        for value in [
            json!({"kind": "TextDelta", "agent_id": "root"}),
            json!({"kind": "TextDelta", "agent_id": "root", "text": 42}),
            json!({"kind": "SubagentDone", "agent_id": "child", "failed": "false"}),
            json!({"agent_id": "root", "text": "missing kind"}),
        ] {
            assert!(
                serde_json::from_value::<AgentEvent>(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn replay_preserves_current_payloads_and_stable_kind_tags() {
        for (kind, value) in [
            (
                "SubagentDone",
                json!({"kind": "SubagentDone", "agent_id": "child", "failed": true, "cancelled": true}),
            ),
            (
                "Compaction",
                json!({"kind": "Compaction", "agent_id": "root", "before_tokens": 100, "after_tokens": 10, "summary": "kept context", "replaced_upto": 4}),
            ),
            (
                "Completion",
                json!({"kind": "Completion", "agent_id": "child", "model": "model-a", "usage": {"input_tokens": 2, "output_tokens": 3, "cache_creation_input_tokens": 5, "cache_read_input_tokens": 7}}),
            ),
            (
                "ToolCall",
                json!({"kind": "ToolCall", "agent_id": "root", "tool_use_id": "call-1", "name": "read_file", "input": {"path": "src/main.rs", "offset": 2}}),
            ),
            (
                "ContextReset",
                json!({"kind": "ContextReset", "agent_id": "root", "epoch": 3}),
            ),
            (
                "AgentMessage",
                json!({"kind": "AgentMessage", "to": "child", "from": "root", "text": "instructions"}),
            ),
        ] {
            let event: AgentEvent = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(event.kind(), kind);
            assert_eq!(serde_json::to_value(event).unwrap(), value);
        }
    }
}
