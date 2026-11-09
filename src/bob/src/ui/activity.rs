use bob_kernel::{LiveAttempt, RunId, ThreadActivity, ThreadId, ThreadState, ThreadSummary};
use bob_ui::activity::{labeled_working_line, shimmer_spans, working_line};
use bob_ui::theme::Palette;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Clock {
    runs: HashMap<ThreadId, RunClock>,
}
struct RunClock {
    run: Option<RunId>,
    since: Instant,
}
impl Clock {
    pub fn observe(&mut self, threads: &[ThreadSummary]) {
        self.observe_at(threads, Instant::now());
    }

    fn observe_at(&mut self, threads: &[ThreadSummary], now: Instant) {
        self.runs
            .retain(|id, _| threads.iter().any(|thread| &thread.id == id));
        for thread in threads {
            if !matches!(
                thread.state,
                ThreadState::Active | ThreadState::Stopping | ThreadState::Held
            ) {
                self.runs.remove(&thread.id);
            } else if self
                .runs
                .get(&thread.id)
                .is_none_or(|clock| clock.run != thread.run)
            {
                self.runs.insert(
                    thread.id.clone(),
                    RunClock {
                        run: thread.run.clone(),
                        since: now,
                    },
                );
            }
        }
    }

    pub fn since(&self, thread: &ThreadId) -> Option<Instant> {
        self.runs.get(thread).map(|clock| clock.since)
    }
}

pub(super) fn line(
    thread: &ThreadSummary,
    attempt: Option<&LiveAttempt>,
    since: Option<Instant>,
    animation: Duration,
    hint: &str,
) -> Option<Line<'static>> {
    line_at(thread, attempt, since, animation, hint, Instant::now())
}

fn line_at(
    thread: &ThreadSummary,
    attempt: Option<&LiveAttempt>,
    since: Option<Instant>,
    animation: Duration,
    hint: &str,
    now: Instant,
) -> Option<Line<'static>> {
    if !matches!(
        thread.state,
        ThreadState::Active | ThreadState::Stopping | ThreadState::Held
    ) {
        return None;
    }
    let attempt = attempt.filter(|attempt| {
        attempt.thread == thread.id
            && thread.run.as_ref() == Some(&attempt.run)
            && attempt.settled_sequence.is_none()
            && matches!(thread.state, ThreadState::Active | ThreadState::Stopping)
    });
    let elapsed_secs = since
        .or_else(|| attempt.map(|attempt| attempt.started_at))
        .map_or(0, |start| now.duration_since(start).as_secs());
    let tick = (animation.as_millis() / 100) as usize;
    let (label, hint, animated) =
        if thread.state == ThreadState::Stopping || attempt.is_some_and(|a| a.stopping) {
            ("Stopping", "waiting for interruption", true)
        } else if thread.needs_approval || thread.activity == Some(ThreadActivity::Approval) {
            ("Approval needed", "Tab to review · Esc denies", false)
        } else if thread.state == ThreadState::Active && attempt.is_some_and(|a| a.compacting) {
            return Some(labeled_working_line(
                "Compacting context",
                elapsed_secs,
                tick,
                hint,
            ));
        } else if thread.state == ThreadState::Active {
            return Some(working_line(elapsed_secs, tick, hint));
        } else {
            return None;
        };
    let mut spans = vec![Span::styled("• ", Style::default().fg(Palette::RUNNING()))];
    if animated {
        spans.extend(shimmer_spans(label, tick));
    } else {
        spans.push(Span::styled(
            label,
            Style::default().fg(Color::Rgb(0x66, 0x66, 0x66)),
        ));
    }
    spans.push(Span::styled(
        format!(" ({elapsed_secs}s · {hint})"),
        Style::default().fg(Palette::FAINT()),
    ));
    Some(Line::from(spans))
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
