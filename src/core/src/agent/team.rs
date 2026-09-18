//! Agent coordination: the machinery that turns fire-and-forget subagents into an
//! addressable, message-passing *team*. The main agent can spawn named subagents
//! that run in the background, send messages into them mid-task, and receive
//! their results back — and subagents may spawn their own children (nesting is
//! bounded by [`MAX_SPAWN_DEPTH`], with a running-agent count capped by
//! [`MAX_TEAM_SIZE`]).
//!
//! This module is pure coordination: channels, handles, and a registry. It has no
//! knowledge of providers, tools, or the turn loop. The turn loop drains an
//! [`AgentInbox`] at each turn boundary (the same cooperative seam as cancel),
//! and the coordination tools route through an [`AgentRegistry`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};

/// A hard ceiling on the number of *currently-running* agents in a team, as a
/// runaway backstop. Finished agents don't count (their transcripts stay visible
/// in the drawer but they release their live slot), so a long session can keep
/// spawning without ever hitting a permanent cap.
pub const MAX_TEAM_SIZE: usize = 32;

/// Maximum spawn nesting depth (root's direct children = depth 1). Bounds runaway
/// recursive self-spawning; deep trees rarely help and cost context.
pub const MAX_SPAWN_DEPTH: usize = 3;

/// Prefix marking a message that was folded into an agent's history from another
/// agent's inbox (rather than typed by the user). Providers reject a `system`
/// role mid-conversation, so these must live as user-role text; this shared
/// marker lets the injector and any consumer (e.g. the UI, which hides them)
/// agree on exactly one format instead of duplicating a string literal.
pub const COORD_MESSAGE_PREFIX: &str = "[message from ";

/// Format an inbound coordination message for injection into history.
pub fn format_coord_message(from: &str, text: &str) -> String {
    format!("{COORD_MESSAGE_PREFIX}{from}]: {text}")
}

/// Whether `text` is a folded coordination message (see [`format_coord_message`]).
pub fn is_coord_message(text: &str) -> bool {
    text.starts_with(COORD_MESSAGE_PREFIX)
}

/// A message delivered between agents. `from` is the sender's name (e.g. "root"
/// or a subagent name), so the recipient can attribute and reply.
#[derive(Clone, Debug)]
pub struct AgentMessage {
    pub from: String,
    pub text: String,
}

/// The receiving end of an agent's mailbox. Owned by the `Agent`; the turn loop
/// drains it (non-blocking) at each turn boundary and folds any messages into
/// history so the model sees and can act on them.
pub struct AgentInbox {
    rx: mpsc::UnboundedReceiver<AgentMessage>,
    /// Messages pulled off the channel but not yet consumed (e.g. one peeked by
    /// `has_pending` to detect a wake, then returned by the next `drain`).
    pushback: std::collections::VecDeque<AgentMessage>,
}

impl AgentInbox {
    /// Drain all currently-queued messages without blocking. Returns them in
    /// arrival order (pushback first), empty if none are waiting.
    pub fn drain(&mut self) -> Vec<AgentMessage> {
        let mut out: Vec<AgentMessage> = self.pushback.drain(..).collect();
        while let Ok(msg) = self.rx.try_recv() {
            out.push(msg);
        }
        out
    }

    /// Whether any message is waiting (in pushback or the channel), without
    /// consuming it.
    pub fn has_pending(&mut self) -> bool {
        if !self.pushback.is_empty() {
            return true;
        }
        // Peek the channel by pulling one and stashing it in pushback.
        if let Ok(msg) = self.rx.try_recv() {
            self.pushback.push_back(msg);
            return true;
        }
        false
    }

    /// Await the next message. Returns `None` if every sender has been dropped    /// (the agent can no longer receive).
    pub async fn recv(&mut self) -> Option<AgentMessage> {
        if let Some(msg) = self.pushback.pop_front() {
            return Some(msg);
        }
        self.rx.recv().await
    }
}

/// A cloneable handle to one agent in the team: its identity, how to message it,
/// its spawn depth, and its live status. Stored in the [`AgentRegistry`].
#[derive(Clone)]
pub struct AgentHandle {
    pub name: String,
    pub depth: usize,
    /// The name of the agent that spawned this one ("root" for top-level agents,
    /// empty for the root itself). Used to scope coordination wakes to an agent's
    /// OWN children rather than the whole team.
    pub parent: String,
    tx: mpsc::UnboundedSender<AgentMessage>,
    status: Arc<Mutex<AgentStatus>>,
    stop_requested: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
}

/// Lifecycle of a team member, surfaced by `list_agents`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl AgentHandle {
    /// Send a message into this agent's inbox. Returns false if the agent is gone
    /// (its inbox was dropped), so callers can report a dead recipient.
    pub fn send(&self, msg: AgentMessage) -> bool {
        self.status() == AgentStatus::Running
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
            });
        }
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
    }
}

/// Create a fresh mailbox: the [`AgentInbox`] the agent owns, plus a sender used
/// to build its [`AgentHandle`]. Returned separately so the agent keeps the
/// receiver while the registry holds the handle.
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

/// The shared team roster. Every agent (root + subagents) holds an `Arc` to the
/// same registry, so any member can address any other by name. Registration and
/// lookup are the only operations; messaging goes through the returned handle.
#[derive(Clone, Default)]
pub struct AgentRegistry {
    agents: Arc<Mutex<HashMap<String, AgentHandle>>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        AgentRegistry::default()
    }

    /// Register a team member. Its `tx` (from [`mailbox`]) becomes the routing
    /// channel; returns the handle (also stored) so the spawner can track it.
    /// `parent` is the spawner's name (empty for the root) so wakes can be scoped
    /// to an agent's own children.
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
        let mut agents = self.agents.lock().unwrap();
        if agents.iter().any(|(id, handle)| {
            handle.status() == AgentStatus::Running
                && (id == &name || is_descendant(&agents, id, &name))
        }) {
            return Err(format!(
                "agent '{name}' or its descendants are still running"
            ));
        }
        let parent_stopping = agents
            .get(&parent)
            .is_some_and(AgentHandle::cancellation_requested);
        let handle = AgentHandle {
            name: name.clone(),
            depth,
            parent,
            tx,
            status: Arc::new(Mutex::new(AgentStatus::Running)),
            stop_requested: Arc::new(AtomicBool::new(parent_stopping)),
            stop_notify: Arc::new(Notify::new()),
        };
        agents.insert(name, handle.clone());
        Ok(handle)
    }

    /// Look up a member by name.
    pub fn get(&self, name: &str) -> Option<AgentHandle> {
        self.agents.lock().unwrap().get(name).cloned()
    }

    /// Route a message to `to`. Returns false if unknown or unreachable.
    pub fn send(&self, to: &str, from: &str, text: &str) -> bool {
        match self.get(to) {
            Some(h) => h.send(AgentMessage {
                from: from.to_string(),
                text: text.to_string(),
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

    /// A (name, depth, status) snapshot of the whole team, for `list_agents`.
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

    /// Whether a name is already taken by a STILL-RUNNING agent. A finished
    /// agent's name is free to reuse (its transcript stays visible in the drawer,
    /// but the live slot is released), so a long session can keep spawning.
    pub fn name_in_use(&self, name: &str) -> bool {
        let agents = self.agents.lock().unwrap();
        agents.iter().any(|(id, handle)| {
            handle.status() == AgentStatus::Running
                && (id == name || is_descendant(&agents, id, name))
        })
    }

    /// Whether `parent` has any still-running direct children. Coordination wakes
    /// are gated on this (an agent's OWN children), so an unrelated slow agent
    /// elsewhere in the team can't stall this agent from processing its replies.
    pub fn has_running_children(&self, parent: &str) -> bool {
        self.agents
            .lock()
            .unwrap()
            .values()
            .any(|h| h.parent == parent && h.status() == AgentStatus::Running)
    }

    /// Count of currently-running agents. The team-size cap counts only these, so
    /// finished agents remain in the roster (visible in the drawer) without
    /// consuming the live budget.
    pub fn active_len(&self) -> usize {
        self.agents
            .lock()
            .unwrap()
            .values()
            .filter(|h| h.status() == AgentStatus::Running)
            .count()
    }

    /// Whether a name is already taken by ANY agent (running or finished). Rarely
    /// needed directly — callers usually want [`name_in_use`](Self::name_in_use),
    /// which ignores finished agents so their names can be reused.
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
    use super::*;

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
        let late = team.register("late".into(), 3, "b".into(), tx);
        assert!(late.cancellation_requested());
        b.complete(false, true, None);
        late.complete(false, true, None);
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
        let run = tokio::spawn(async move {
            worker
                .run_until_stopped(
                    Arc::new(AtomicBool::new(false)),
                    std::future::pending::<()>(),
                )
                .await
        });
        tokio::task::yield_now().await;
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
        assert_eq!(inbox.drain()[0].text, "finished: result");
        assert!(!team.send("worker", "root", "late message"));
    }

    #[test]
    fn spawn_report_back_reaches_root_inbox() {
        // Simulates the real result-delivery loop: root is registered with its
        // OWN inbox; a spawned child reports back to "root"; the message must land
        // in root's inbox (the bug where root had no inbox → results were dropped).
        let team = AgentRegistry::new();
        let (mut root_inbox, root_tx) = mailbox();
        team.register("root".into(), 0, String::new(), root_tx);

        // Child registers, "runs", reports its result to root by name.
        let (_child_inbox, child_tx) = mailbox();
        team.register("reviewer".into(), 1, "root".into(), child_tx);
        assert!(team.send("root", "reviewer", "finished: all good"));

        // Root's inbox now has the child's report, and has_pending sees it.
        assert!(root_inbox.has_pending());
        let msgs = root_inbox.drain();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].from, "reviewer");
        assert_eq!(msgs[0].text, "finished: all good");
    }

    #[test]
    fn register_route_and_drain() {
        let reg = AgentRegistry::new();
        let (mut inbox, tx) = mailbox();
        reg.register("worker".into(), 1, "root".into(), tx);

        assert!(reg.send("worker", "root", "hello"));
        assert!(!reg.send("ghost", "root", "hi")); // unknown recipient

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
            })
            .unwrap();
        }
        let msgs = inbox.drain();
        assert_eq!(
            msgs.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["m0", "m1", "m2"]
        );
        // Draining again yields nothing (non-blocking).
        assert!(inbox.drain().is_empty());
    }

    #[test]
    fn roster_reports_status() {
        let reg = AgentRegistry::new();
        let (_inbox, tx) = mailbox();
        let h = reg.register("w".into(), 2, "root".into(), tx);
        assert_eq!(
            reg.roster(),
            vec![("w".to_string(), 2, AgentStatus::Running)]
        );
        h.set_status(AgentStatus::Done);
        assert_eq!(reg.roster()[0].2, AgentStatus::Done);
    }

    #[tokio::test]
    async fn recv_awaits_a_message() {
        let (mut inbox, tx) = mailbox();
        tokio::spawn(async move {
            tx.send(AgentMessage {
                from: "root".into(),
                text: "ping".into(),
            })
            .unwrap();
        });
        let msg = inbox.recv().await.unwrap();
        assert_eq!(msg.text, "ping");
    }

    #[test]
    fn dead_inbox_send_fails() {
        let reg = AgentRegistry::new();
        let (inbox, tx) = mailbox();
        reg.register("w".into(), 1, "root".into(), tx);
        drop(inbox); // agent gone
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

        // root's child finishes; the unrelated running one must NOT block root.
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

        // Finished → slot freed + name reusable, but still in the roster.
        h.set_status(AgentStatus::Done);
        assert_eq!(reg.active_len(), 0);
        assert!(!reg.name_in_use("w"));
        assert_eq!(reg.roster().len(), 1);
    }
}
