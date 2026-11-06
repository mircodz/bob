use super::{state::*, Reply};
use bob_kernel::{LiveSnapshot, ThreadActivity, ThreadState};
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    time::Duration,
};
use tokio::time::Instant;

pub(super) const STREAM_FRAME: Duration = Duration::from_millis(33);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Urgency {
    Immediate,
    Stream,
}

pub(super) struct Scheduler {
    last_draw: Instant,
    dirty: Option<Instant>,
    animation: Option<Instant>,
    #[cfg(test)]
    pub draws: usize,
    #[cfg(test)]
    pub coalesced: usize,
}

impl Scheduler {
    pub fn new(now: Instant, animation: Option<Instant>) -> Self {
        Self {
            last_draw: now,
            dirty: None,
            animation,
            #[cfg(test)]
            draws: 0,
            #[cfg(test)]
            coalesced: 0,
        }
    }

    pub fn invalidate(&mut self, now: Instant, urgency: Urgency) {
        let deadline = match urgency {
            Urgency::Immediate => now,
            Urgency::Stream => now.max(self.last_draw + STREAM_FRAME),
        };
        #[cfg(test)]
        if self.dirty.is_some() && urgency == Urgency::Stream {
            self.coalesced += 1;
        }
        self.dirty = Some(self.dirty.map_or(deadline, |old| old.min(deadline)));
    }

    pub fn deadline(&self) -> Option<Instant> {
        match (self.dirty, self.animation) {
            (Some(dirty), Some(animation)) => Some(dirty.min(animation)),
            (dirty, animation) => dirty.or(animation),
        }
    }

    pub fn due(&self, now: Instant) -> bool {
        self.deadline().is_some_and(|deadline| deadline <= now)
    }

    pub fn drawn(&mut self, now: Instant, animation: Option<Instant>) {
        self.last_draw = now;
        self.dirty = None;
        self.animation = animation;
        #[cfg(test)]
        {
            self.draws += 1;
        }
    }
}

pub(super) async fn wait(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

pub(super) fn refresh_reply(reply: &Reply) -> bool {
    match reply {
        Reply::View { .. } | Reply::Workflow(_) | Reply::Inline(_) => true,
        Reply::Runtime { reply, .. } => refresh_reply(reply),
        _ => false,
    }
}

pub(super) fn reply_urgency(reply: &Reply) -> Urgency {
    match reply {
        Reply::Inline(reply) if !reply.is_error() => Urgency::Stream,
        Reply::Runtime { reply, .. } => reply_urgency(reply),
        _ => Urgency::Immediate,
    }
}

impl App {
    pub(super) fn refresh_visual(&mut self, jobs: &mut tokio::task::JoinSet<Reply>) -> bool {
        let before = self.visual_stamp();
        self.refresh(jobs);
        self.visual_stamp() != before
    }

    pub(super) fn reply_visual(&mut self, reply: Reply) -> bool {
        let before = refresh_reply(&reply).then(|| self.visual_stamp());
        self.reply(reply);
        before.is_none_or(|before| self.visual_stamp() != before)
    }

    pub(super) fn visual_stamp(&self) -> u64 {
        let mut hash = DefaultHasher::new();
        self.runtime_epoch.hash(&mut hash);
        self.selected.hash(&mut hash);
        std::mem::discriminant(&self.focus).hash(&mut hash);
        self.workflows.visual_revision().hash(&mut hash);
        for thread in &self.threads {
            thread.id.hash(&mut hash);
            thread.parent_thread.hash(&mut hash);
            thread.name.hash(&mut hash);
            thread.model.hash(&mut hash);
            thread.state.hash(&mut hash);
            thread.needs_approval.hash(&mut hash);
            thread.run.hash(&mut hash);
            thread
                .activity
                .as_ref()
                .map(std::mem::discriminant)
                .hash(&mut hash);
            if let Some(ThreadActivity::Tool { name }) = &thread.activity {
                name.hash(&mut hash);
            }
        }
        if let Some(pane) = self.pane() {
            pane.content_key.hash(&mut hash);
            pane.inline.revision().hash(&mut hash);
            pane.submission_revision.hash(&mut hash);
            pane.before.hash(&mut hash);
            pane.notices.back().map(|notice| notice.id).hash(&mut hash);
            pane.notices.len().hash(&mut hash);
            pane.receipt.as_ref().map(|(_, time)| time).hash(&mut hash);
            pane.view.is_some().hash(&mut hash);
            if pane.before.is_none()
                && self.focus != Focus::Inspector
                && !self.workflows.inspecting()
            {
                if let (Some(snapshot), Some(thread)) = (&self.live_snapshot, &self.selected) {
                    for attempt in &snapshot.attempts {
                        if &attempt.thread == thread
                            && !pane.committed_operations.contains(&attempt.operation)
                            && attempt.settled_sequence.is_none_or(|settled| {
                                pane.view.as_ref().map_or(0, |view| view.sequence) < settled
                            })
                        {
                            attempt.operation.hash(&mut hash);
                            attempt.revision.hash(&mut hash);
                        }
                    }
                }
            }
            if let Some(view) = &pane.view {
                view.entries.len().hash(&mut hash);
                view.entries
                    .first()
                    .map(|entry| entry.sequence)
                    .hash(&mut hash);
                view.entries
                    .last()
                    .map(|entry| entry.sequence)
                    .hash(&mut hash);
                for policy in &view.policies {
                    policy.session.hash(&mut hash);
                    policy.revision.hash(&mut hash);
                }
            }
            if let Some(permission) = &pane.permission {
                permission.decision.operation.hash(&mut hash);
                permission.decision.revision.hash(&mut hash);
                permission.pending.hash(&mut hash);
                permission.options_pending.hash(&mut hash);
                permission.options_loaded.hash(&mut hash);
            }
            pane.question
                .as_ref()
                .map(|prompt| {
                    (
                        &prompt.question.operation,
                        prompt.question.revision,
                        prompt.pending,
                    )
                })
                .hash(&mut hash);
            pane.plan
                .as_ref()
                .map(|prompt| (&prompt.plan.operation, prompt.plan.revision, prompt.pending))
                .hash(&mut hash);
        }
        hash.finish()
    }

    pub(super) fn live_urgency(&self, snapshot: &LiveSnapshot) -> Option<Urgency> {
        let Some(previous) = &self.live_snapshot else {
            return Some(Urgency::Immediate);
        };
        if previous.instance != snapshot.instance {
            return Some(Urgency::Immediate);
        }
        if previous.revision >= snapshot.revision {
            return None;
        }
        if previous.error != snapshot.error {
            return Some(Urgency::Immediate);
        }
        let (Some(thread), Some(pane)) = (&self.selected, self.pane()) else {
            return None;
        };
        if pane.before.is_some() || self.focus == Focus::Inspector || self.workflows.inspecting() {
            return None;
        }
        let visible = |attempt: &&std::sync::Arc<bob_kernel::LiveAttempt>| {
            &attempt.thread == thread
                && !pane.committed_operations.contains(&attempt.operation)
                && attempt.settled_sequence.is_none_or(|settled| {
                    pane.view.as_ref().map_or(0, |view| view.sequence) < settled
                })
        };
        let current = snapshot.attempts.iter().filter(visible);
        let old = previous.attempts.iter().filter(visible);
        if current
            .clone()
            .map(|attempt| (&attempt.operation, attempt.revision))
            .eq(old.map(|attempt| (&attempt.operation, attempt.revision)))
        {
            return None;
        }
        if current.clone().any(|attempt| {
            attempt.error.is_some()
                && previous
                    .attempts
                    .iter()
                    .find(|old| old.operation == attempt.operation)
                    .is_none_or(|old| old.error != attempt.error)
        }) {
            Some(Urgency::Immediate)
        } else {
            Some(Urgency::Stream)
        }
    }

    pub(super) fn animation_deadline(&self, now: Instant) -> Option<Instant> {
        if self.size.width < 12
            || self.size.height < 5
            || self
                .live_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.error.is_some())
        {
            return None;
        }
        let pane = self.pane()?;
        if pane.draft.limit_reached {
            return None;
        }
        let thread = self
            .threads
            .iter()
            .find(|thread| Some(&thread.id) == self.selected.as_ref());
        let has_activity = pane.before.is_none()
            && self.focus != Focus::Inspector
            && !self.workflows.inspecting()
            && pane.permission.is_none()
            && pane.question.is_none()
            && pane.plan.is_none()
            && thread.is_some_and(|thread| {
                matches!(thread.state, ThreadState::Active | ThreadState::Stopping)
                    || (thread.state == ThreadState::Held
                        && (thread.needs_approval
                            || thread.activity == Some(ThreadActivity::Approval)))
            });
        if has_activity {
            let visible = pane
                .viewport
                .rows
                .iter()
                .skip(pane.viewport.top)
                .take(pane.viewport.height)
                .any(|anchor| anchor.identity == AnchorId::Activity && anchor.line == 0);
            if !visible {
                return None;
            }
            let thread = thread.unwrap();
            let approval = thread.state != ThreadState::Stopping
                && (thread.needs_approval || thread.activity == Some(ThreadActivity::Approval));
            let (origin, period) = if approval {
                (
                    self.activity.since(&thread.id).unwrap_or(self.started_at),
                    Duration::from_secs(1),
                )
            } else {
                (self.started_at, Duration::from_millis(100))
            };
            Some(next_boundary(now, origin.into(), period))
        } else {
            pane.receipt.as_ref().and_then(|(_, when)| {
                let expires = Instant::from_std(*when) + Duration::from_secs(3);
                (now < expires).then_some(expires)
            })
        }
    }
}

fn next_boundary(now: Instant, origin: Instant, period: Duration) -> Instant {
    let elapsed = now.saturating_duration_since(origin);
    let remainder = Duration::from_nanos((elapsed.as_nanos() % period.as_nanos()) as u64);
    now + period - remainder
}
