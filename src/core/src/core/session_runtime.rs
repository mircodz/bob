use super::events::{AgentEvent, EventBus, Subscription};
use super::session::Session;
use super::store::SessionStore;
use anyhow::Context;
use std::sync::{Arc, Mutex};

/// Reserve identifiers from the whole archive, including contexts cleared earlier.
pub fn reserve_runtime_ids(
    session: &Session,
    events: &[AgentEvent],
    jobs: &crate::tools::jobs::JobRegistry,
) {
    fn reserve_jobs(text: &str, jobs: &crate::tools::jobs::JobRegistry) {
        for word in text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_') {
            jobs.reserve_id(word);
        }
    }
    for message in &session.messages {
        if let Ok(text) = serde_json::to_string(message) {
            reserve_jobs(&text, jobs);
        }
    }
    for thread in &session.agent_threads {
        crate::tools::task::reserve_subagent_id(&thread.id);
    }
    for run in &session.subagent_runs {
        for child in &run.subagents {
            crate::tools::task::reserve_subagent_id(&child.id);
        }
    }
    for workflow in &session.workflows {
        crate::tools::workflow_tool::reserve_workflow_id(&workflow.id);
    }
    for event in events {
        match event {
            AgentEvent::ContextArchive {
                snapshot,
                stored_snapshot,
                ..
            } => {
                reserve_runtime_ids(snapshot, &[], jobs);
                if let Some(stored) = stored_snapshot {
                    reserve_runtime_ids(stored, &[], jobs);
                }
            }
            AgentEvent::SubagentSpawn { agent_id, .. } => {
                crate::tools::task::reserve_subagent_id(agent_id)
            }
            AgentEvent::WorkflowPhase { workflow_id, .. }
            | AgentEvent::WorkflowLog { workflow_id, .. } => {
                crate::tools::workflow_tool::reserve_workflow_id(workflow_id);
            }
            AgentEvent::ToolResult { .. }
            | AgentEvent::AgentMessage { .. }
            | AgentEvent::Message { .. } => {
                if let Ok(text) = serde_json::to_string(event) {
                    reserve_jobs(&text, jobs);
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct CheckpointToken {
    generation: u64,
    sequence: u64,
}

struct State {
    id: String,
    epoch: u64,
    generation: u64,
    issued: u64,
    saved: u64,
    error: Option<String>,
}

/// Serializes checkpoints and transitions, fencing delayed saves and listeners.
#[derive(Clone)]
pub struct SessionOwner {
    store: Arc<dyn SessionStore>,
    state: Arc<Mutex<State>>,
}

impl SessionOwner {
    pub fn new(store: Arc<dyn SessionStore>, session: &Session) -> Self {
        Self {
            store,
            state: Arc::new(Mutex::new(State {
                id: session.id.clone(),
                epoch: session.context_epoch,
                generation: 0,
                issued: 0,
                saved: 0,
                error: None,
            })),
        }
    }

    /// Bind a listener to this context. Rebind after `switch` or `reset`.
    pub fn subscribe(&self, bus: &EventBus) -> Subscription {
        let generation = self.state.lock().unwrap().generation;
        let owner = self.clone();
        bus.subscribe(Arc::new(move |event| {
            let mut state = owner.state.lock().unwrap();
            if state.generation != generation {
                return;
            }
            if let Err(error) = owner.store.append_event(&state.id, event) {
                state
                    .error
                    .get_or_insert_with(|| format!("append session event: {error:#}"));
            }
        }))
    }

    /// Capture before dispatching an asynchronous save, not inside its task.
    pub fn token(&self) -> CheckpointToken {
        let mut state = self.state.lock().unwrap();
        state.issued += 1;
        CheckpointToken {
            generation: state.generation,
            sequence: state.issued,
        }
    }

    /// Returns false for superseded snapshots. Run blocking storage off the runtime.
    pub fn checkpoint(&self, token: CheckpointToken, snapshot: Session) -> anyhow::Result<bool> {
        let mut state = self.state.lock().unwrap();
        if token.generation != state.generation || token.sequence <= state.saved {
            return Ok(false);
        }
        anyhow::ensure!(
            snapshot.id == state.id && snapshot.context_epoch == state.epoch,
            "checkpoint belongs to a different session context"
        );
        state.saved = token.sequence;
        let result = self.save_and_flush(&snapshot);
        Self::with_journal_error(result, &state)?;
        Ok(true)
    }

    /// Settle old agents/jobs first. Old subscriptions are invalidated even if retained.
    pub fn switch(&self, session: &Session) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        Self::with_journal_error(self.store.flush(), &state)?;
        self.save_and_flush(session)?;
        state.id = session.id.clone();
        state.epoch = session.context_epoch;
        state.generation += 1;
        state.saved = state.issued;
        state.error = None;
        Ok(())
    }

    /// Clear logical context while retaining the original append-only archive.
    /// The caller must settle producers first and rebind its subscription afterward,
    /// including when this returns a persistence error. Archival failures leave the
    /// context and epoch unchanged; later failures may occur after the reset advances.
    pub fn reset(&self, session: &mut Session) -> anyhow::Result<()> {
        let mut state = self.state.lock().unwrap();
        anyhow::ensure!(
            session.id == state.id,
            "reset belongs to a different session"
        );
        let epoch = state
            .epoch
            .checked_add(1)
            .context("context epoch exhausted")?;
        state.generation += 1;
        state.saved = state.issued;
        let stored_snapshot = self
            .store
            .load(&state.id)
            .context("load pre-reset session archive")?;
        self.store
            .append_event(
                &state.id,
                &AgentEvent::ContextArchive {
                    agent_id: "root".into(),
                    snapshot: Box::new(session.clone()),
                    stored_snapshot: stored_snapshot.map(Box::new),
                },
            )
            .context("append pre-reset session archive")?;
        Self::with_journal_error(
            self.store
                .flush()
                .context("flush pre-reset session archive"),
            &state,
        )?;
        state.epoch = epoch;
        session.context_epoch = state.epoch;
        session.messages.clear();
        session.todos.clear();
        session.subagent_runs.clear();
        session.agent_threads.clear();
        session.workflows.clear();
        let appended = self.store.append_event(
            &state.id,
            &AgentEvent::ContextReset {
                agent_id: "root".into(),
                epoch: state.epoch,
            },
        );
        if let Err(error) = appended {
            state
                .error
                .get_or_insert_with(|| format!("append context reset: {error:#}"));
        }
        let saved = self.save_and_flush(session);
        Self::with_journal_error(saved, &state)
    }

    pub fn flush(&self) -> anyhow::Result<()> {
        let state = self.state.lock().unwrap();
        Self::with_journal_error(self.store.flush(), &state)
    }

    fn save_and_flush(&self, session: &Session) -> anyhow::Result<()> {
        let saved = self
            .store
            .save(session)
            .with_context(|| format!("save session {}", session.id));
        let flushed = self
            .store
            .flush()
            .with_context(|| format!("flush session {}", session.id));
        match (saved, flushed) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(flush)) => {
                Err(error.context(format!("event flush also failed: {flush:#}")))
            }
        }
    }

    fn with_journal_error(result: anyhow::Result<()>, state: &State) -> anyhow::Result<()> {
        match (result, &state.error) {
            (Ok(()), None) => Ok(()),
            (Err(error), None) => Err(error),
            (Ok(()), Some(error)) => Err(anyhow::anyhow!(error.clone())),
            (Err(error), Some(journal)) => Err(error.context(journal.clone())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::core::session::SessionSummary;
    use crate::core::store::{MemoryStore, SqliteStore};
    use crate::core::types::Message;
    use crate::test_support::TestDir;
    use std::time::Duration;
    use tokio::sync::Notify;

    const LIMIT: Duration = Duration::from_secs(10);

    fn store(dir: &TestDir, sqlite: bool) -> Arc<dyn SessionStore> {
        if sqlite {
            Arc::new(SqliteStore::open(dir.join("sessions.db")).unwrap())
        } else {
            Arc::new(MemoryStore::new())
        }
    }

    fn reopen(
        dir: &TestDir,
        backing: Arc<dyn SessionStore>,
        sqlite: bool,
    ) -> Arc<dyn SessionStore> {
        if sqlite {
            drop(backing);
            store(dir, true)
        } else {
            backing
        }
    }

    fn message_texts(messages: &[Message]) -> Vec<String> {
        messages.iter().map(Message::text).collect()
    }

    struct ReleaseOnDrop(std::sync::mpsc::Sender<()>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    struct SaveGate {
        entered: Arc<Notify>,
        release: std::sync::mpsc::Receiver<()>,
    }

    struct ControlledStore {
        inner: Arc<MemoryStore>,
        failure: Mutex<Option<(&'static str, usize)>>,
        calls: Mutex<Vec<&'static str>>,
        save_gate: Mutex<Option<SaveGate>>,
    }

    impl ControlledStore {
        fn new(inner: Arc<MemoryStore>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                failure: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
                save_gate: Mutex::new(None),
            })
        }

        fn fail(&self, operation: &'static str, after: usize) {
            *self.failure.lock().unwrap() = Some((operation, after));
        }

        fn check(&self, operation: &'static str) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(operation);
            if let Some((failed_operation, remaining)) = self.failure.lock().unwrap().as_mut() {
                if *failed_operation == operation {
                    anyhow::ensure!(*remaining > 0, "injected {operation} failure");
                    *remaining -= 1;
                }
            }
            Ok(())
        }

        fn block_save(&self) -> (Arc<Notify>, ReleaseOnDrop) {
            let (tx, rx) = std::sync::mpsc::channel();
            let entered = Arc::new(Notify::new());
            *self.save_gate.lock().unwrap() = Some(SaveGate {
                entered: entered.clone(),
                release: rx,
            });
            (entered, ReleaseOnDrop(tx))
        }
    }

    impl SessionStore for ControlledStore {
        fn save(&self, session: &Session) -> anyhow::Result<()> {
            self.check("save")?;
            let gate = self.save_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.recv_timeout(LIMIT)?;
            }
            self.inner.save(session)
        }

        fn load(&self, id: &str) -> anyhow::Result<Option<Session>> {
            self.check("load")?;
            self.inner.load(id)
        }

        fn latest_in(&self, cwd: &str) -> anyhow::Result<Option<Session>> {
            self.inner.latest_in(cwd)
        }

        fn list(&self) -> anyhow::Result<Vec<SessionSummary>> {
            self.inner.list()
        }

        fn append_event(&self, id: &str, event: &AgentEvent) -> anyhow::Result<()> {
            self.check("append")?;
            self.inner.append_event(id, event)
        }

        fn flush(&self) -> anyhow::Result<()> {
            self.check("flush")?;
            self.inner.flush()
        }

        fn load_events(&self, id: &str) -> anyhow::Result<Option<Vec<AgentEvent>>> {
            self.inner.load_events(id)
        }
    }

    mod reset_recovery {
        use super::*;

        #[test]
        fn invalid_reset_is_rejected_before_storage_or_snapshot_changes() {
            for exhausted in [false, true] {
                let store = ControlledStore::new(Arc::new(MemoryStore::new()));
                let mut session = crate::core::session::new_session(
                    "mock",
                    "owned".into(),
                    "0".into(),
                    ".".into(),
                );
                session.messages.push(Message::user_text("preserve"));
                if exhausted {
                    session.context_epoch = u64::MAX;
                }
                let owner = SessionOwner::new(store.clone(), &session);
                if !exhausted {
                    session.id = "other".into();
                }
                let original = serde_json::to_value(&session).unwrap();
                let error = owner.reset(&mut session).unwrap_err();
                assert!(error.to_string().contains(if exhausted {
                    "context epoch exhausted"
                } else {
                    "different session"
                }));
                assert_eq!(serde_json::to_value(session).unwrap(), original);
                assert!(store.calls.lock().unwrap().is_empty());
            }
        }

        #[test]
        fn reset_archives_legacy_and_incomplete_blobs_before_replacing_them() {
            for sqlite in [false, true] {
                for incomplete in [false, true] {
                    let dir = TestDir::new();
                    let store = store(&dir, sqlite);
                    let mut session = crate::core::session::new_session(
                        "mock",
                        "archive".into(),
                        "0".into(),
                        ".".into(),
                    );
                    session.messages = vec![
                        Message::user_text("blob-only first"),
                        Message::user_text("blob-only second"),
                    ];
                    let original = session.clone();
                    store.save(&session).unwrap();
                    if incomplete {
                        store
                            .append_event(
                                &session.id,
                                &AgentEvent::UserPrompt {
                                    agent_id: "root".into(),
                                    text: "partial log".into(),
                                },
                            )
                            .unwrap();
                        store.flush().unwrap();
                    }
                    session
                        .messages
                        .push(Message::user_text("not checkpointed yet"));
                    let snapshot = session.clone();
                    let owner = SessionOwner::new(store.clone(), &session);
                    owner.reset(&mut session).unwrap();
                    drop(owner);
                    let store = reopen(&dir, store, sqlite);
                    let current = store.load(&session.id).unwrap().unwrap();
                    assert_eq!(current.context_epoch, 1);
                    assert!(current.messages.is_empty());
                    assert!(store.history_for(&current).unwrap().is_empty());
                    let events = store.load_events(&session.id).unwrap().unwrap();
                    let archive_index = events
                        .iter()
                        .position(|event| matches!(event, AgentEvent::ContextArchive { .. }))
                        .unwrap();
                    match &events[archive_index] {
                        AgentEvent::ContextArchive {
                            snapshot: archived,
                            stored_snapshot: Some(stored),
                            ..
                        } => {
                            assert_eq!(
                                serde_json::to_value(archived).unwrap(),
                                serde_json::to_value(snapshot).unwrap()
                            );
                            assert_eq!(
                                serde_json::to_value(stored).unwrap(),
                                serde_json::to_value(original).unwrap()
                            );
                        }
                        other => panic!("missing archive snapshots: {other:?}"),
                    }
                    assert!(matches!(
                        events[archive_index + 1],
                        AgentEvent::ContextReset { epoch: 1, .. }
                    ));
                }
            }
        }

        #[test]
        fn reset_errors_preserve_original_history_and_flush_archive_before_save() {
            for (operation, after, archive_durable) in [
                ("load", 0, false),
                ("append", 0, false),
                ("flush", 0, false),
                ("append", 1, true),
                ("save", 0, true),
                ("flush", 1, true),
            ] {
                let backing = Arc::new(MemoryStore::new());
                let mut session = crate::core::session::new_session(
                    "mock",
                    "failure".into(),
                    "0".into(),
                    ".".into(),
                );
                session
                    .messages
                    .push(Message::user_text("must survive reset failure"));
                backing.save(&session).unwrap();
                let original = serde_json::to_value(&session).unwrap();
                let store = ControlledStore::new(backing.clone());
                let owner = SessionOwner::new(store.clone(), &session);
                store.fail(operation, after);
                assert!(owner.reset(&mut session).is_err(), "{operation}/{after}");
                backing.flush().unwrap();
                let calls = store.calls.lock().unwrap().clone();
                if archive_durable {
                    let events = backing.load_events(&session.id).unwrap().unwrap();
                    assert!(events.iter().any(|event| matches!(event, AgentEvent::ContextArchive { stored_snapshot: Some(stored), .. }
                        if serde_json::to_value(stored).unwrap() == original)));
                    let flushed = calls
                        .iter()
                        .position(|operation| *operation == "flush")
                        .unwrap();
                    let saved = calls
                        .iter()
                        .position(|operation| *operation == "save")
                        .unwrap();
                    assert!(flushed < saved);
                } else {
                    assert_eq!(serde_json::to_value(&session).unwrap(), original);
                    assert_eq!(
                        serde_json::to_value(backing.load(&session.id).unwrap().unwrap()).unwrap(),
                        original
                    );
                    assert!(!calls.contains(&"save"));
                }
            }
        }

        #[test]
        fn context_reset_fences_stale_saves_and_keeps_the_event_archive() {
            use crate::core::events::EventBus;
            for sqlite in [false, true] {
                let dir = TestDir::new();
                let store = store(&dir, sqlite);
                let mut session = crate::core::session::new_session(
                    "mock",
                    "reset".into(),
                    "0".into(),
                    dir.0.to_string_lossy().into(),
                );
                let owner = SessionOwner::new(store.clone(), &session);
                let bus = EventBus::new();
                let old_subscription = owner.subscribe(&bus);
                for text in ["old one", "old two"] {
                    bus.emit(AgentEvent::UserPrompt {
                        agent_id: "root".into(),
                        text: text.into(),
                    });
                    session.messages.push(Message::user_text(text));
                }
                owner.checkpoint(owner.token(), session.clone()).unwrap();
                let stale = session.clone();
                let delayed = owner.token();
                owner.reset(&mut session).unwrap();
                assert!(!owner.checkpoint(delayed, stale.clone()).unwrap());
                store.save(&stale).unwrap();
                bus.emit(AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "late old listener".into(),
                });
                drop(old_subscription);
                let _current = owner.subscribe(&bus);
                bus.emit(AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "new context".into(),
                });
                session.messages.push(Message::user_text("new context"));
                owner.checkpoint(owner.token(), session.clone()).unwrap();
                let events = store.load_events(&session.id).unwrap().unwrap();
                assert_eq!(events.len(), 5);
                assert!(matches!(events[2], AgentEvent::ContextArchive { .. }));
                assert!(matches!(
                    events[3],
                    AgentEvent::ContextReset { epoch: 1, .. }
                ));
                assert_eq!(store.load(&session.id).unwrap().unwrap().context_epoch, 1);
                assert_eq!(
                    message_texts(&store.history_for(&session).unwrap()),
                    ["new context"]
                );
                assert_eq!(
                    message_texts(&store.working_history_for(&session).unwrap()),
                    ["new context"]
                );
                assert_eq!(
                    message_texts(&store.history_for(&stale).unwrap()),
                    ["new context"]
                );
                bus.emit(AgentEvent::Compaction {
                    agent_id: "root".into(),
                    before_tokens: 100,
                    after_tokens: 10,
                    summary: "new summary".into(),
                    replaced_upto: 1,
                });
                owner.flush().unwrap();
                for snapshot in [&session, &stale] {
                    assert_eq!(
                        message_texts(&store.working_history_for(snapshot).unwrap()),
                        ["[conversation summary]\nnew summary"]
                    );
                    assert_eq!(
                        message_texts(&store.history_for(snapshot).unwrap()),
                        ["new context"]
                    );
                }
            }
        }
    }

    mod checkpoint_ownership {
        use super::*;

        #[test]
        fn session_switch_retargets_events_and_rejects_delayed_old_snapshots() {
            use crate::core::events::EventBus;
            for sqlite in [false, true] {
                let dir = TestDir::new();
                let store = store(&dir, sqlite);
                let mut old =
                    crate::core::session::new_session("mock", "old".into(), "0".into(), ".".into());
                let new =
                    crate::core::session::new_session("mock", "new".into(), "1".into(), ".".into());
                let owner = SessionOwner::new(store.clone(), &old);
                let old_bus = EventBus::new();
                let _old_subscription = owner.subscribe(&old_bus);
                old_bus.emit(AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "old".into(),
                });
                old.messages.push(Message::user_text("old"));
                owner.checkpoint(owner.token(), old.clone()).unwrap();
                let pending = owner.token();
                owner.switch(&new).unwrap();
                old_bus.emit(AgentEvent::AgentMessage {
                    to: "root".into(),
                    from: "late child".into(),
                    text: "must not leak".into(),
                });
                let new_bus = EventBus::new();
                let _new_subscription = owner.subscribe(&new_bus);
                new_bus.emit(AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "new".into(),
                });
                assert!(!owner.checkpoint(pending, old).unwrap());
                owner.flush().unwrap();
                for id in ["old", "new"] {
                    assert_eq!(
                        serde_json::to_value(store.load_events(id).unwrap().unwrap()).unwrap(),
                        serde_json::to_value([AgentEvent::UserPrompt {
                            agent_id: "root".into(),
                            text: id.into(),
                        }])
                        .unwrap()
                    );
                }
                assert_eq!(message_texts(&store.history_for(&new).unwrap()), ["new"]);
            }
        }

        #[tokio::test]
        async fn reset_waits_for_inflight_checkpoint_and_supersedes_queued_ones() {
            let store = ControlledStore::new(Arc::new(MemoryStore::new()));
            let mut session =
                crate::core::session::new_session("mock", "race".into(), "0".into(), ".".into());
            session.messages.push(Message::user_text("stale"));
            let owner = SessionOwner::new(store.clone(), &session);
            let token = owner.token();
            let queued = owner.token();
            let (entered, release) = store.block_save();
            let saving = owner.clone();
            let snapshot = session.clone();
            let save = tokio::task::spawn_blocking(move || saving.checkpoint(token, snapshot));
            tokio::time::timeout(LIMIT, entered.notified())
                .await
                .unwrap();
            let resetting = owner.clone();
            let reset = tokio::task::spawn_blocking(move || {
                resetting.reset(&mut session)?;
                Ok::<_, anyhow::Error>(session)
            });
            drop(release);
            save.await.unwrap().unwrap();
            let reset_session = reset.await.unwrap().unwrap();
            assert!(!owner.checkpoint(queued, reset_session).unwrap());
            let saved = store.load("race").unwrap().unwrap();
            assert_eq!(saved.context_epoch, 1);
            assert!(saved.messages.is_empty());
            assert!(store.history_for(&saved).unwrap().is_empty());
        }

        #[test]
        fn invalid_snapshot_context_does_not_consume_token_or_touch_store() {
            let store = ControlledStore::new(Arc::new(MemoryStore::new()));
            let session =
                crate::core::session::new_session("mock", "owned".into(), "0".into(), ".".into());
            let owner = SessionOwner::new(store.clone(), &session);
            let token = owner.token();
            for wrong_epoch in [false, true] {
                let mut wrong = session.clone();
                if wrong_epoch {
                    wrong.context_epoch += 1;
                } else {
                    wrong.id = "other".into();
                }
                let error = owner.checkpoint(token, wrong).unwrap_err();
                assert!(error.to_string().contains("different session context"));
                assert!(store.calls.lock().unwrap().is_empty());
            }
            assert!(owner.clone().checkpoint(token, session.clone()).unwrap());
            assert_eq!(*store.calls.lock().unwrap(), ["save", "flush"]);
            store.calls.lock().unwrap().clear();
            assert!(!owner.checkpoint(token, session.clone()).unwrap());
            assert!(store.calls.lock().unwrap().is_empty());
            assert_eq!(
                serde_json::to_value(store.inner.load("owned").unwrap().unwrap()).unwrap(),
                serde_json::to_value(session).unwrap()
            );
        }

        #[test]
        fn checkpoint_flushes_after_save_failure_and_new_token_can_recover() {
            for operation in ["save", "flush"] {
                let store = ControlledStore::new(Arc::new(MemoryStore::new()));
                let mut session =
                    crate::core::session::new_session("mock", "s".into(), "0".into(), ".".into());
                session.messages.push(Message::user_text("keep this"));
                let owner = SessionOwner::new(store.clone(), &session);
                store.fail(operation, 0);
                let error = owner
                    .checkpoint(owner.token(), session.clone())
                    .unwrap_err();
                assert!(format!("{error:#}").contains(&format!("injected {operation} failure")));
                assert_eq!(*store.calls.lock().unwrap(), ["save", "flush"]);
                if operation == "save" {
                    assert!(store.inner.load("s").unwrap().is_none());
                }
                *store.failure.lock().unwrap() = None;
                assert!(owner.checkpoint(owner.token(), session.clone()).unwrap());
                assert_eq!(
                    serde_json::to_value(store.inner.load("s").unwrap().unwrap()).unwrap(),
                    serde_json::to_value(&session).unwrap()
                );
            }
        }

        #[test]
        fn failed_switch_keeps_old_listener_and_checkpoint_ownership() {
            for (operation, after, calls) in [
                ("flush", 0, vec!["flush"]),
                ("save", 0, vec!["flush", "save", "flush"]),
                ("flush", 1, vec!["flush", "save", "flush"]),
            ] {
                let store = ControlledStore::new(Arc::new(MemoryStore::new()));
                let old =
                    crate::core::session::new_session("mock", "old".into(), "0".into(), ".".into());
                let new =
                    crate::core::session::new_session("mock", "new".into(), "1".into(), ".".into());
                let owner = SessionOwner::new(store.clone(), &old);
                let token = owner.token();
                let bus = EventBus::new();
                let _subscription = owner.subscribe(&bus);
                store.fail(operation, after);
                let error = owner.switch(&new).unwrap_err();
                assert!(format!("{error:#}").contains(&format!("injected {operation} failure")));
                assert_eq!(*store.calls.lock().unwrap(), calls);
                *store.failure.lock().unwrap() = None;
                let event = AgentEvent::UserPrompt {
                    agent_id: "root".into(),
                    text: "still old".into(),
                };
                bus.emit(event.clone());
                assert!(owner.checkpoint(token, old).unwrap());
                assert_eq!(
                    serde_json::to_value(store.inner.load_events("old").unwrap().unwrap()).unwrap(),
                    serde_json::to_value([event]).unwrap()
                );
                assert!(store.inner.load_events("new").unwrap().is_none());
            }
        }

        #[test]
        fn journal_failure_remains_visible_after_later_successful_appends() {
            let store = ControlledStore::new(Arc::new(MemoryStore::new()));
            let session =
                crate::core::session::new_session("mock", "s".into(), "0".into(), ".".into());
            let owner = SessionOwner::new(store.clone(), &session);
            let bus = EventBus::new();
            let _subscription = owner.subscribe(&bus);
            store.fail("append", 0);
            bus.emit(AgentEvent::UserPrompt {
                agent_id: "root".into(),
                text: "lost".into(),
            });
            *store.failure.lock().unwrap() = None;
            let good = AgentEvent::UserPrompt {
                agent_id: "root".into(),
                text: "persisted".into(),
            };
            bus.emit(good.clone());
            for result in [
                owner.flush(),
                owner.checkpoint(owner.token(), session).map(|_| ()),
            ] {
                let error = result.unwrap_err();
                assert!(
                    format!("{error:#}").contains("append session event: injected append failure")
                );
            }
            assert_eq!(
                serde_json::to_value(store.inner.load_events("s").unwrap().unwrap()).unwrap(),
                serde_json::to_value([good]).unwrap()
            );
        }

        #[test]
        fn checkpoint_tokens_reject_out_of_order_equal_length_snapshots() {
            for sqlite in [false, true] {
                let dir = TestDir::new();
                let store = store(&dir, sqlite);
                let mut session = crate::core::session::new_session(
                    "mock",
                    "order".into(),
                    "0".into(),
                    ".".into(),
                );
                let owner = SessionOwner::new(store.clone(), &session);
                let old = owner.token();
                let current = owner.token();
                session.messages.push(Message::user_text("current"));
                assert!(owner.checkpoint(current, session.clone()).unwrap());
                session.messages[0] = Message::user_text("obsolete");
                assert!(!owner.checkpoint(old, session).unwrap());
                assert_eq!(
                    store.load("order").unwrap().unwrap().messages[0].text(),
                    "current"
                );
            }
        }
    }
}
