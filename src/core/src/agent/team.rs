use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};

/// Settled agents release capacity but remain in the roster.
pub const MAX_TEAM_SIZE: usize = 32;

/// Root's direct children have depth 1.
pub const MAX_SPAWN_DEPTH: usize = 3;

/// Providers reject mid-conversation system messages; coordination uses marked user text.
pub const COORD_MESSAGE_PREFIX: &str = "[message from ";

pub fn format_coord_message(from: &str, text: &str) -> String {
    format!("{COORD_MESSAGE_PREFIX}{from}]: {text}")
}

pub fn is_coord_message(text: &str) -> bool {
    text.starts_with(COORD_MESSAGE_PREFIX)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentMessageKind {
    Actionable,
    Completion { status: AgentStatus },
}

#[derive(Clone, Debug)]
pub struct AgentMessage {
    pub from: String,
    pub text: String,
    pub kind: AgentMessageKind,
}

pub struct AgentInbox {
    rx: mpsc::UnboundedReceiver<AgentMessage>,
    /// Peeked messages, retained until consumed.
    pushback: std::collections::VecDeque<AgentMessage>,
}

impl AgentInbox {
    pub fn drain(&mut self) -> Vec<AgentMessage> {
        let mut out: Vec<AgentMessage> = self.pushback.drain(..).collect();
        while let Ok(msg) = self.rx.try_recv() {
            out.push(msg);
        }
        out
    }

    pub fn has_pending(&mut self) -> bool {
        if !self.pushback.is_empty() {
            return true;
        }
        if let Ok(msg) = self.rx.try_recv() {
            self.pushback.push_back(msg);
            return true;
        }
        false
    }

    pub fn has_actionable(&mut self) -> bool {
        while let Ok(message) = self.rx.try_recv() {
            self.pushback.push_back(message);
        }
        self.pushback
            .iter()
            .any(|message| message.kind == AgentMessageKind::Actionable)
    }

    /// Close successful child acceptance only when every accepted message was consumed.
    pub fn try_close(&mut self, handle: &AgentHandle) -> bool {
        let _status = handle.status.lock().unwrap();
        if self.has_pending() {
            return false;
        }
        handle.accepting.store(false, Ordering::Release);
        true
    }

    pub async fn recv(&mut self) -> Option<AgentMessage> {
        if let Some(msg) = self.pushback.pop_front() {
            return Some(msg);
        }
        self.rx.recv().await
    }
}

#[derive(Clone)]
pub struct AgentHandle {
    pub name: String,
    pub depth: usize,
    /// Spawner's name; empty for root. Scopes wakes to direct children.
    pub parent: String,
    tx: mpsc::UnboundedSender<AgentMessage>,
    status: Arc<Mutex<AgentStatus>>,
    accepting: Arc<AtomicBool>,
    terminal_event_pending: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
    capacity_changed: Arc<Notify>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

pub(crate) struct PendingTerminalEvent {
    pending: Arc<AtomicBool>,
    capacity_changed: Arc<Notify>,
}

impl Drop for PendingTerminalEvent {
    fn drop(&mut self) {
        self.pending.store(false, Ordering::Release);
        self.capacity_changed.notify_waiters();
    }
}

impl AgentHandle {
    pub(crate) fn begin_terminal_event(&self) -> PendingTerminalEvent {
        self.terminal_event_pending.store(true, Ordering::Release);
        PendingTerminalEvent {
            pending: self.terminal_event_pending.clone(),
            capacity_changed: self.capacity_changed.clone(),
        }
    }

    /// Whether execution and terminal event publication have both finished.
    pub fn is_settled(&self) -> bool {
        let status = self.status.lock().unwrap();
        *status != AgentStatus::Running && !self.terminal_event_pending.load(Ordering::Acquire)
    }

    pub fn send(&self, msg: AgentMessage) -> bool {
        let status = self.status.lock().unwrap();
        *status == AgentStatus::Running
            && self.accepting.load(Ordering::Acquire)
            && !self.cancellation_requested()
            && self.tx.send(msg).is_ok()
    }

    pub fn cancellation_requested(&self) -> bool {
        self.stop_requested.load(Ordering::Acquire)
    }

    fn request_stop(&self) -> bool {
        let status = self.status.lock().unwrap();
        if *status != AgentStatus::Running {
            return false;
        }
        let changed = !self.stop_requested.swap(true, Ordering::AcqRel);
        self.stop_notify.notify_waiters();
        changed
    }

    pub(crate) fn complete(
        &self,
        failed: bool,
        cancelled: bool,
        report: Option<(&AgentHandle, &str)>,
    ) -> AgentStatus {
        // Stop acceptance and terminal selection share this lock. For named
        // children, enqueue the report before a parent can observe terminal state.
        let mut status = self.status.lock().unwrap();
        if *status != AgentStatus::Running {
            return status.clone();
        }
        *status = if cancelled || self.cancellation_requested() {
            AgentStatus::Cancelled
        } else if failed {
            AgentStatus::Failed
        } else {
            AgentStatus::Done
        };
        if let Some((parent, output)) = report {
            let output = if *status == AgentStatus::Cancelled {
                "[cancelled]"
            } else {
                output
            };
            let _ = parent.tx.send(AgentMessage {
                from: self.name.clone(),
                text: format!("finished: {output}"),
                kind: AgentMessageKind::Completion {
                    status: status.clone(),
                },
            });
        }
        self.capacity_changed.notify_waiters();
        status.clone()
    }

    pub async fn run_until_stopped<F: std::future::Future>(
        &self,
        parent_cancel: Arc<AtomicBool>,
        work: F,
    ) -> Option<F::Output> {
        let stopped = async {
            loop {
                if self.cancellation_requested() || parent_cancel.load(Ordering::Relaxed) {
                    return;
                }
                tokio::select! {
                    _ = self.stop_notify.notified() => {},
                    _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {},
                }
            }
        };
        tokio::select! {
            biased;
            _ = stopped => None,
            result = work => Some(result),
        }
    }

    pub fn status(&self) -> AgentStatus {
        self.status.lock().unwrap().clone()
    }

    pub fn set_status(&self, status: AgentStatus) {
        *self.status.lock().unwrap() = status;
        self.capacity_changed.notify_waiters();
    }
}

/// The agent owns the receiver; the registry holds the sending handle.
pub fn mailbox() -> (AgentInbox, mpsc::UnboundedSender<AgentMessage>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        AgentInbox {
            rx,
            pushback: std::collections::VecDeque::new(),
        },
        tx,
    )
}

#[derive(Clone, Default)]
pub struct AgentRegistry {
    agents: Arc<Mutex<HashMap<String, AgentHandle>>>,
    admission_queue: Arc<tokio::sync::Mutex<()>>,
    capacity_changed: Arc<Notify>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        AgentRegistry::default()
    }

    pub fn register(
        &self,
        name: String,
        depth: usize,
        parent: String,
        tx: mpsc::UnboundedSender<AgentMessage>,
    ) -> AgentHandle {
        self.try_register(name, depth, parent, tx)
            .expect("agent name must be unused")
    }

    pub fn try_register(
        &self,
        name: String,
        depth: usize,
        parent: String,
        tx: mpsc::UnboundedSender<AgentMessage>,
    ) -> Result<AgentHandle, String> {
        self.try_register_batch(vec![(name, depth, parent, tx)])
            .map(|mut handles| handles.remove(0))
    }

    /// Queue a leaf launch without reserving a team slot. Cancellation returns `None`.
    /// The deadline includes time behind other waiters, bounding parent-held-slot deadlocks.
    pub async fn register_when_available(
        &self,
        name: String,
        depth: usize,
        parent: String,
        tx: mpsc::UnboundedSender<AgentMessage>,
        cancelled: impl Fn() -> bool,
        max_wait: std::time::Duration,
    ) -> Result<Option<AgentHandle>, String> {
        let admission = async {
            let _queue = self.admission_queue.lock().await;
            loop {
                let changed = self.capacity_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if cancelled() {
                    return Ok(None);
                }
                if let Some(mut handles) = self.register_batch_if_available(vec![(
                    name.clone(),
                    depth,
                    parent.clone(),
                    tx.clone(),
                )])? {
                    return Ok(handles.pop());
                }
                changed.await;
            }
        };
        let cancellation = async {
            loop {
                if cancelled() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        };
        tokio::select! {
            biased;
            _ = cancellation => Ok(None),
            result = tokio::time::timeout(max_wait, admission) => result.unwrap_or_else(|_| {
                Err(format!(
                    "timed out waiting for team capacity (maximum {MAX_TEAM_SIZE} agents); parent agents may be holding all slots"
                ))
            }),
        }
    }

    pub fn try_register_batch(
        &self,
        requests: Vec<(String, usize, String, mpsc::UnboundedSender<AgentMessage>)>,
    ) -> Result<Vec<AgentHandle>, String> {
        self.register_batch_if_available(requests)?
            .ok_or_else(|| format!("team is at the maximum of {MAX_TEAM_SIZE} running agents"))
    }

    fn register_batch_if_available(
        &self,
        requests: Vec<(String, usize, String, mpsc::UnboundedSender<AgentMessage>)>,
    ) -> Result<Option<Vec<AgentHandle>>, String> {
        let mut agents = self.agents.lock().unwrap();
        let mut names = std::collections::HashSet::new();
        for (name, depth, parent, _) in &requests {
            if *depth > MAX_SPAWN_DEPTH {
                return Err(format!(
                    "spawn nesting too deep (max {MAX_SPAWN_DEPTH} levels)"
                ));
            }
            if !names.insert(name)
                || agents.iter().any(|(id, handle)| {
                    !handle.is_settled() && (id == name || is_descendant(&agents, id, name))
                })
            {
                return Err(format!(
                    "agent '{name}' or its descendants are still running"
                ));
            }
            if agents.get(parent).is_some_and(|handle| {
                handle.cancellation_requested() || handle.status() != AgentStatus::Running
            }) {
                return Err(format!("parent agent '{parent}' is stopping or finished"));
            }
        }
        if agents
            .values()
            .filter(|handle| !handle.is_settled())
            .count()
            .saturating_add(requests.len())
            > MAX_TEAM_SIZE
        {
            return Ok(None);
        }
        let mut handles = Vec::with_capacity(requests.len());
        for (name, depth, parent, tx) in requests {
            let handle = AgentHandle {
                name: name.clone(),
                depth,
                parent,
                tx,
                status: Arc::new(Mutex::new(AgentStatus::Running)),
                accepting: Arc::new(AtomicBool::new(true)),
                terminal_event_pending: Arc::new(AtomicBool::new(false)),
                stop_requested: Arc::new(AtomicBool::new(false)),
                stop_notify: Arc::new(Notify::new()),
                capacity_changed: self.capacity_changed.clone(),
            };
            agents.insert(name, handle.clone());
            handles.push(handle);
        }
        Ok(Some(handles))
    }

    pub fn get(&self, name: &str) -> Option<AgentHandle> {
        self.agents.lock().unwrap().get(name).cloned()
    }

    pub fn send(&self, to: &str, from: &str, text: &str) -> bool {
        match self.get(to) {
            Some(h) => h.send(AgentMessage {
                from: from.to_string(),
                text: text.to_string(),
                kind: AgentMessageKind::Actionable,
            }),
            None => false,
        }
    }

    /// Request cancellation of a live agent and its descendants. The root can
    /// control any child; other agents can control only their own descendants.
    pub fn stop(&self, requester: &str, target: &str) -> Result<usize, String> {
        if target == "root" {
            return Err("use the main interrupt control to stop the root agent".into());
        }
        let agents = self.agents.lock().unwrap();
        let target_handle = agents
            .get(target)
            .ok_or_else(|| format!("no agent named '{target}'"))?;
        if requester != "root" && !is_descendant(&agents, target, requester) {
            return Err("agents can only stop their own descendants".into());
        }
        if target_handle.status() != AgentStatus::Running {
            return Err(format!("agent '{target}' is already finished"));
        }
        let mut stopped = 0;
        for (name, handle) in agents.iter() {
            if (name == target || is_descendant(&agents, name, target))
                && handle.status() == AgentStatus::Running
            {
                stopped += usize::from(handle.request_stop());
            }
        }
        Ok(stopped)
    }

    pub(crate) fn descendant_names(&self, parent: &str) -> Vec<String> {
        let agents = self.agents.lock().unwrap();
        agents
            .keys()
            .filter(|name| is_descendant(&agents, name, parent))
            .cloned()
            .collect()
    }

    pub fn stop_descendants(&self, parent: &str) -> usize {
        let agents = self.agents.lock().unwrap();
        agents
            .iter()
            .filter(|(name, handle)| is_descendant(&agents, name, parent) && handle.request_stop())
            .count()
    }

    pub fn roster(&self) -> Vec<(String, usize, AgentStatus)> {
        let mut v: Vec<_> = self
            .agents
            .lock()
            .unwrap()
            .values()
            .map(|h| (h.name.clone(), h.depth, h.status()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Whether a name or its descendants are still running or publishing their
    /// terminal events. Settled agents release their names for reuse.
    pub fn name_in_use(&self, name: &str) -> bool {
        let agents = self.agents.lock().unwrap();
        agents.iter().any(|(id, handle)| {
            !handle.is_settled() && (id == name || is_descendant(&agents, id, name))
        })
    }

    /// Whether any descendant is running or still publishing its terminal event.
    /// The parent itself is excluded.
    pub fn has_unsettled_descendants(&self, parent: &str) -> bool {
        let agents = self.agents.lock().unwrap();
        agents.iter().any(|(id, handle)| {
            id != parent && is_descendant(&agents, id, parent) && !handle.is_settled()
        })
    }

    /// Only direct children gate coordination wakes, not unrelated agents.
    pub fn has_running_children(&self, parent: &str) -> bool {
        self.agents
            .lock()
            .unwrap()
            .values()
            .any(|h| h.parent == parent && !h.is_settled())
    }

    /// Count of running or publication-pending agents. The team-size cap counts
    /// these until settled; finished agents remain visible without consuming the
    /// live budget.
    pub fn active_len(&self) -> usize {
        self.agents
            .lock()
            .unwrap()
            .values()
            .filter(|h| !h.is_settled())
            .count()
    }

    /// Includes settled agents, unlike `name_in_use`.
    pub fn contains(&self, name: &str) -> bool {
        self.agents.lock().unwrap().contains_key(name)
    }
}

fn is_descendant(agents: &HashMap<String, AgentHandle>, name: &str, ancestor: &str) -> bool {
    let mut current = name;
    for _ in 0..agents.len() {
        let Some(handle) = agents.get(current) else {
            break;
        };
        if handle.parent == ancestor {
            return true;
        }
        current = &handle.parent;
    }
    false
}

#[cfg(test)]
mod tests {
    mod registry {
        use crate::agent::team::*;

        #[test]
        fn steering_and_final_acceptance_share_a_boundary() {
            let team = AgentRegistry::new();
            let (mut inbox, tx) = mailbox();
            let handle = team.register("worker".into(), 1, "root".into(), tx);
            assert!(team.send("worker", "root", "finished: actually steering"));
            assert!(inbox.has_actionable());
            assert!(!inbox.try_close(&handle));
            assert_eq!(inbox.drain().len(), 1);
            assert!(inbox.try_close(&handle));
            assert!(!team.send("worker", "root", "late"));
        }

        #[test]
        fn accepted_steering_cannot_race_past_successful_close() {
            for _ in 0..32 {
                let team = AgentRegistry::new();
                let (mut inbox, tx) = mailbox();
                let handle = team.register("worker".into(), 1, "root".into(), tx);
                let barrier = Arc::new(std::sync::Barrier::new(2));
                let ready = barrier.clone();
                let finish = std::thread::spawn(move || {
                    ready.wait();
                    let closed = inbox.try_close(&handle);
                    (closed, inbox)
                });
                barrier.wait();
                let accepted = team.send("worker", "root", "steer");
                let (closed, mut inbox) = finish.join().unwrap();
                assert_ne!(accepted, closed);
                assert_eq!(inbox.drain().len(), usize::from(accepted));
            }
        }

        #[test]
        fn admission_is_atomic_and_shared_under_concurrent_launches() {
            let team = AgentRegistry::new();
            let threads: Vec<_> = (0..MAX_TEAM_SIZE * 2)
                .map(|i| {
                    let team = team.clone();
                    std::thread::spawn(move || {
                        let (_inbox, tx) = mailbox();
                        team.try_register(format!("w{i}"), 1, "root".into(), tx)
                            .is_ok()
                    })
                })
                .collect();
            assert_eq!(
                threads
                    .into_iter()
                    .map(|thread| thread.join().expect("admission thread panicked"))
                    .filter(|ok| *ok)
                    .count(),
                MAX_TEAM_SIZE
            );
            let fresh = AgentRegistry::new();
            let (_, a) = mailbox();
            let (_, b) = mailbox();
            assert!(fresh
                .try_register_batch(vec![
                    ("valid".into(), 1, "root".into(), a),
                    ("invalid".into(), MAX_SPAWN_DEPTH + 1, "root".into(), b),
                ])
                .is_err());
            assert!(fresh.roster().is_empty());
        }

        #[tokio::test]
        async fn queued_admission_is_fifo_and_waits_for_terminal_publication() {
            let team = AgentRegistry::new();
            let handles: Vec<_> = (0..MAX_TEAM_SIZE)
                .map(|index| {
                    let (_, tx) = mailbox();
                    team.register(format!("existing-{index}"), 1, "root".into(), tx)
                })
                .collect();
            let (_, tx) = mailbox();
            let first = team.register_when_available(
                "first".into(),
                1,
                "root".into(),
                tx,
                || false,
                std::time::Duration::from_secs(1),
            );
            tokio::pin!(first);
            assert!(futures::poll!(first.as_mut()).is_pending());
            let (_, tx) = mailbox();
            let second = team.register_when_available(
                "second".into(),
                1,
                "root".into(),
                tx,
                || false,
                std::time::Duration::from_secs(1),
            );
            tokio::pin!(second);
            assert!(futures::poll!(second.as_mut()).is_pending());
            let publication = handles[0].begin_terminal_event();
            handles[0].complete(false, false, None);
            assert!(futures::poll!(first.as_mut()).is_pending());
            drop(publication);
            let first = first.await.unwrap().unwrap();
            assert!(futures::poll!(second.as_mut()).is_pending());
            assert_eq!(team.active_len(), MAX_TEAM_SIZE);
            first.set_status(AgentStatus::Done);
            assert_eq!(second.await.unwrap().unwrap().name, "second");
            assert_eq!(team.active_len(), MAX_TEAM_SIZE);
        }

        #[tokio::test]
        async fn queued_admission_cancellation_and_deadline_release_the_queue() {
            let team = AgentRegistry::new();
            let mut handles = Vec::new();
            for index in 0..MAX_TEAM_SIZE {
                let (_, tx) = mailbox();
                handles.push(team.register(format!("parent-{index}"), 1, "root".into(), tx));
            }
            let cancel = AtomicBool::new(false);
            let (_, tx) = mailbox();
            let waiting = team.register_when_available(
                "waiting".into(),
                2,
                "parent-0".into(),
                tx,
                || cancel.load(Ordering::Relaxed),
                std::time::Duration::from_secs(1),
            );
            tokio::pin!(waiting);
            assert!(futures::poll!(waiting.as_mut()).is_pending());
            let queued_cancel = AtomicBool::new(false);
            let (_, tx) = mailbox();
            let queued = team.register_when_available(
                "queued".into(),
                2,
                "parent-1".into(),
                tx,
                || queued_cancel.load(Ordering::Relaxed),
                std::time::Duration::from_secs(1),
            );
            tokio::pin!(queued);
            assert!(futures::poll!(queued.as_mut()).is_pending());
            queued_cancel.store(true, Ordering::Relaxed);
            assert!(queued.await.unwrap().is_none());
            let (_, tx) = mailbox();
            let error = team
                .register_when_available(
                    "timed-out".into(),
                    2,
                    "parent-1".into(),
                    tx,
                    || false,
                    std::time::Duration::ZERO,
                )
                .await
                .err()
                .unwrap();
            assert!(error.contains("timed out waiting for team capacity"));
            cancel.store(true, Ordering::Relaxed);
            assert!(waiting.await.unwrap().is_none());
            let (_, tx) = mailbox();
            let error = team
                .register_when_available(
                    "deadlocked".into(),
                    2,
                    "parent-1".into(),
                    tx,
                    || false,
                    std::time::Duration::ZERO,
                )
                .await
                .err()
                .unwrap();
            assert!(error.contains("parent agents may be holding all slots"));
            handles[2].complete(false, false, None);
            let (_, tx) = mailbox();
            assert!(team
                .register_when_available(
                    "next".into(),
                    2,
                    "parent-1".into(),
                    tx,
                    || false,
                    std::time::Duration::from_secs(1),
                )
                .await
                .unwrap()
                .is_some());
            assert!(!team.contains("waiting"));
            assert!(!team.contains("queued"));
            assert!(!team.contains("timed-out"));
            assert!(!team.contains("deadlocked"));
        }

        #[test]
        fn direct_terminal_updates_are_settled_without_lifecycle_publication() {
            for status in [
                AgentStatus::Done,
                AgentStatus::Failed,
                AgentStatus::Cancelled,
            ] {
                let team = AgentRegistry::new();
                let (_inbox, tx) = mailbox();
                let handle = team.register("worker".into(), 1, "root".into(), tx);
                assert!(!handle.is_settled());
                assert!(team.has_unsettled_descendants("root"));
                assert!(!team.has_unsettled_descendants("worker"));
                handle.set_status(status);
                assert!(handle.is_settled());
                assert!(!team.has_unsettled_descendants("root"));
            }
            for (failed, cancelled) in [(false, false), (true, false), (false, true)] {
                let team = AgentRegistry::new();
                let (_inbox, tx) = mailbox();
                let handle = team.register("worker".into(), 1, "root".into(), tx);
                handle.complete(failed, cancelled, None);
                assert!(handle.is_settled());
            }
        }

        #[test]
        fn unsettled_descendants_exclude_parent_and_unrelated_agents() {
            let team = AgentRegistry::new();
            let (_inbox, tx) = mailbox();
            team.register("root".into(), 0, String::new(), tx);
            assert!(!team.has_unsettled_descendants("root"));
            let (_inbox, tx) = mailbox();
            let parent = team.register("parent".into(), 1, "root".into(), tx);
            let (_inbox, tx) = mailbox();
            let child = team.register("child".into(), 2, "parent".into(), tx);
            parent.complete(false, false, None);
            assert!(team.has_unsettled_descendants("root"));
            assert!(team.has_unsettled_descendants("parent"));
            assert!(!team.has_unsettled_descendants("child"));
            assert!(!team.has_unsettled_descendants("unrelated"));
            child.complete(false, false, None);
            assert!(!team.has_unsettled_descendants("root"));
        }

        #[test]
        fn stop_is_scoped_to_descendants_and_preserves_siblings() {
            let team = AgentRegistry::new();
            let mut handles = HashMap::new();
            for (name, parent) in [("root", ""), ("a", "root"), ("b", "a"), ("sibling", "root")] {
                let (_inbox, tx) = mailbox();
                handles.insert(name, team.register(name.into(), 1, parent.into(), tx));
            }
            assert!(team.stop("b", "a").is_err());
            assert!(team.stop("a", "sibling").is_err());
            assert!(team.stop("root", "root").is_err());
            assert!(team.stop("root", "missing").is_err());
            assert_eq!(team.stop("root", "a").unwrap(), 2);
            assert!(handles["a"].cancellation_requested());
            assert!(handles["b"].cancellation_requested());
            assert!(!handles["sibling"].cancellation_requested());
            assert!(!handles["root"].cancellation_requested());
            assert_eq!(team.stop("root", "a").unwrap(), 0);
            assert_eq!(
                handles["a"].complete(false, false, None),
                AgentStatus::Cancelled
            );
            assert!(team.stop("root", "a").is_err());
        }

        #[test]
        fn live_descendants_prevent_name_reuse_and_late_children_inherit_stop() {
            let team = AgentRegistry::new();
            let (_i, tx) = mailbox();
            let a = team.register("a".into(), 1, "root".into(), tx);
            let (_i, tx) = mailbox();
            let b = team.register("b".into(), 2, "a".into(), tx);
            a.set_status(AgentStatus::Done);
            assert!(team.name_in_use("a"));
            let (_i, tx) = mailbox();
            assert!(team
                .try_register("a".into(), 2, "sibling".into(), tx)
                .is_err());
            assert!(team.stop("sibling", "b").is_err());
            team.stop("root", "b").unwrap();
            let (_i, tx) = mailbox();
            assert!(team.try_register("late".into(), 3, "b".into(), tx).is_err());
            b.complete(false, true, None);
            assert!(!team.name_in_use("a"));
        }

        #[tokio::test]
        async fn stop_before_start_does_not_poll_work_and_stop_interrupts_pending_work() {
            let team = AgentRegistry::new();
            let (_i, tx) = mailbox();
            let handle = team.register("worker".into(), 1, "root".into(), tx);
            team.stop("root", "worker").unwrap();
            let result = handle
                .run_until_stopped(Arc::new(AtomicBool::new(false)), async {
                    panic!("stopped work must not start")
                })
                .await;
            assert!(result.is_none());
            let (_i, tx) = mailbox();
            let waiting = team.register("waiting".into(), 1, "root".into(), tx);
            let worker = waiting.clone();
            let started = Arc::new(Notify::new());
            let entered = started.clone();
            let run = tokio::spawn(async move {
                worker
                    .run_until_stopped(Arc::new(AtomicBool::new(false)), async move {
                        entered.notify_one();
                        std::future::pending::<()>().await
                    })
                    .await
            });
            tokio::time::timeout(std::time::Duration::from_secs(1), started.notified())
                .await
                .expect("work must be polled before stop");
            team.stop("root", "waiting").unwrap();
            assert!(tokio::time::timeout(std::time::Duration::from_secs(1), run)
                .await
                .unwrap()
                .unwrap()
                .is_none());
        }

        #[test]
        fn accepted_stop_cannot_race_into_a_successful_completion() {
            for _ in 0..64 {
                let team = AgentRegistry::new();
                let (_i, tx) = mailbox();
                let handle = team.register("worker".into(), 1, "root".into(), tx);
                let barrier = Arc::new(std::sync::Barrier::new(2));
                let child = handle.clone();
                let ready = barrier.clone();
                let finish = std::thread::spawn(move || {
                    ready.wait();
                    child.complete(false, false, None)
                });
                barrier.wait();
                let accepted = team.stop("root", "worker").is_ok_and(|count| count == 1);
                let status = finish.join().unwrap();
                if accepted {
                    assert_eq!(status, AgentStatus::Cancelled);
                }
            }
        }

        #[test]
        fn completion_queues_parent_result_before_releasing_terminal_state() {
            let team = AgentRegistry::new();
            let (mut inbox, tx) = mailbox();
            let parent = team.register("root".into(), 0, String::new(), tx);
            let (_i, tx) = mailbox();
            let child = team.register("worker".into(), 1, "root".into(), tx);
            assert_eq!(
                child.complete(false, false, Some((&parent, "result"))),
                AgentStatus::Done
            );
            assert!(!team.has_running_children("root"));
            assert!(!inbox.has_actionable());
            let messages = inbox.drain();
            assert_eq!(messages[0].text, "finished: result");
            assert_eq!(
                messages[0].kind,
                AgentMessageKind::Completion {
                    status: AgentStatus::Done
                }
            );
            assert!(!team.send("worker", "root", "late message"));
        }

        #[test]
        fn register_route_and_drain() {
            let reg = AgentRegistry::new();
            let (mut inbox, tx) = mailbox();
            reg.register("worker".into(), 1, "root".into(), tx);

            assert!(reg.send("worker", "root", "hello"));
            assert!(!reg.send("ghost", "root", "hi"));

            let msgs = inbox.drain();
            assert_eq!(msgs.len(), 1);
            assert_eq!(msgs[0].from, "root");
            assert_eq!(msgs[0].text, "hello");
        }

        #[test]
        fn drain_is_ordered_and_nonblocking() {
            let (mut inbox, tx) = mailbox();
            for i in 0..3 {
                tx.send(AgentMessage {
                    from: "root".into(),
                    text: format!("m{i}"),
                    kind: AgentMessageKind::Actionable,
                })
                .unwrap();
            }
            let msgs = inbox.drain();
            assert_eq!(
                msgs.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
                ["m0", "m1", "m2"]
            );
            assert!(inbox.drain().is_empty());
        }

        #[test]
        fn dead_inbox_send_fails() {
            let reg = AgentRegistry::new();
            let (inbox, tx) = mailbox();
            reg.register("w".into(), 1, "root".into(), tx);
            drop(inbox);
            assert!(!reg.send("w", "root", "anyone home?"));
        }

        #[test]
        fn wake_scopes_to_own_children_only() {
            let reg = AgentRegistry::new();
            let (_i1, t1) = mailbox();
            let mine = reg.register("mine".into(), 1, "root".into(), t1);
            let (_i2, t2) = mailbox();
            let other = reg.register("other".into(), 1, "someone".into(), t2);

            assert!(reg.has_running_children("root"));
            assert!(reg.has_running_children("someone"));

            mine.set_status(AgentStatus::Done);
            assert!(!reg.has_running_children("root"));
            assert!(reg.has_running_children("someone"));
            let _ = other;
        }

        #[test]
        fn active_len_and_name_reuse_ignore_finished() {
            let reg = AgentRegistry::new();
            let (_i, tx) = mailbox();
            let h = reg.register("w".into(), 1, "root".into(), tx);
            assert_eq!(reg.active_len(), 1);
            assert!(reg.name_in_use("w"));

            h.set_status(AgentStatus::Done);
            assert_eq!(reg.active_len(), 0);
            assert!(!reg.name_in_use("w"));
            assert_eq!(reg.roster().len(), 1);
        }
    }

    mod tools {
        use crate::agent::env::AgentEnv;
        use crate::agent::team::AgentRegistry;
        use crate::core::events::{AgentEvent, EventBus};
        use crate::providers::mock::MockProvider;
        use crate::tools::coordinate::*;
        use crate::tools::file_tracker::FileTracker;
        use crate::tools::jobs::JobRegistry;
        use crate::tools::registry::{CoordContext, ToolRegistry};
        use crate::tools::registry::{Tool, ToolContext};
        use crate::tools::todo::TodoStore;
        use serde_json::json;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;

        #[tokio::test]
        async fn unknown_agent_type_does_not_leave_a_running_child() {
            let team = AgentRegistry::new();
            let bus = EventBus::new();
            let events = Arc::new(AtomicUsize::new(0));
            let count = events.clone();
            bus.on(Arc::new(move |_: &AgentEvent| {
                count.fetch_add(1, Ordering::Relaxed);
            }));
            let provider = MockProvider::new(vec![]);
            let jobs = JobRegistry::new();
            let tool = SpawnAgentTool {
                deps: CoordDeps {
                    env: AgentEnv {
                        provider: Arc::new(provider.clone()),
                        subagent_tools: ToolRegistry::new(None),
                        bus,
                        cwd: ".".into(),
                        subagent_system: None,
                        jobs: jobs.clone(),
                        team: team.clone(),
                        lsp: None,
                        parent_cancel: Arc::new(AtomicBool::new(false)),
                        definitions: Default::default(),
                        hooks: Default::default(),
                        memory: None,
                    },
                    team: team.clone(),
                },
            };
            let ctx = ToolContext {
                cwd: ".".into(),
                files: Arc::new(FileTracker::new()),
                todos: Arc::new(TodoStore::new()),
                jobs,
                user_asker: None,
                lsp: None,
                coord: Some(CoordContext {
                    name: "root".into(),
                    depth: 0,
                    team: team.clone(),
                }),
                permissions: None,
            };
            let result = tool
                .execute(
                    json!({
                        "name": "reviewer",
                        "task": "review the code",
                        "subagent_type": "missing"
                    }),
                    &ctx,
                )
                .await;
            assert!(result
                .unwrap_err()
                .message
                .contains("unknown subagent type"));
            assert!(team.roster().is_empty());
            assert!(!team.has_running_children("root"));
            assert_eq!(events.load(Ordering::Relaxed), 0);
            assert!(ctx.jobs.list().is_empty());
            assert_eq!(provider.call_count(), 0);
        }
    }
}
