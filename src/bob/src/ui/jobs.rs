use super::style::{safe, Palette};
use bob_kernel::{JobKind, JobState, JobSummary, QueuedInput};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

impl super::Pane {
    pub(super) fn observe_submission(&mut self, submission: bob_kernel::EntrySubmission) {
        if self
            .submissions
            .get(&submission.id)
            .is_some_and(|old| old.sequence >= submission.sequence)
        {
            return;
        }
        let changed = self
            .submissions
            .get(&submission.id)
            .is_none_or(|old| old.state != submission.state);
        self.submissions.insert(submission.id.clone(), submission);
        if changed {
            self.submission_revision += 1;
        }
    }

    pub(super) fn accept_queue(&mut self, snapshot: bob_kernel::QueueSnapshot) {
        if snapshot.sequence < self.queue_sequence {
            return;
        }
        self.queue_sequence = snapshot.sequence;
        for submission in snapshot.submissions {
            self.observe_submission(submission);
        }
        self.queue = snapshot.pending;
        self.reconcile_submissions();
    }

    pub(super) fn reconcile_submissions(&mut self) {
        let submissions: Vec<_> = self
            .view
            .iter()
            .flat_map(|view| &view.entries)
            .filter_map(|entry| entry.submission.clone())
            .collect();
        for submission in submissions {
            self.observe_submission(submission);
        }
        self.queue.retain(|input| {
            self.submissions
                .get(&input.id)
                .is_none_or(|submission| submission.state == bob_kernel::SubmissionState::Pending)
        });
        self.tracked_submissions();
    }

    pub(super) fn submission_visible(&self, entry: &bob_kernel::Entry) -> bool {
        entry.submission.as_ref().is_none_or(|submission| {
            let latest = self
                .submissions
                .get(&submission.id)
                .filter(|latest| latest.sequence >= submission.sequence)
                .unwrap_or(submission);
            latest.state == bob_kernel::SubmissionState::Consumed
        })
    }

    pub(super) fn visible_queue(&self) -> Vec<QueuedInput> {
        let mut queue: Vec<_> = self
            .queue
            .iter()
            .filter(|input| {
                self.submissions.get(&input.id).is_none_or(|submission| {
                    submission.state == bob_kernel::SubmissionState::Pending
                })
            })
            .cloned()
            .collect();
        for entry in self.view.iter().flat_map(|view| &view.entries) {
            let Some(submission) = &entry.submission else {
                continue;
            };
            let latest = self
                .submissions
                .get(&submission.id)
                .filter(|latest| latest.sequence >= submission.sequence)
                .unwrap_or(submission);
            if latest.state == bob_kernel::SubmissionState::Pending
                && latest.sequence >= self.queue_sequence
                && !queue.iter().any(|input| input.id == submission.id)
            {
                queue.push(QueuedInput {
                    id: submission.id.clone(),
                    thread: entry.thread.clone(),
                    text: entry.text.clone(),
                });
            }
        }
        queue.sort_by_key(|input| {
            self.submissions
                .get(&input.id)
                .map(|submission| submission.ordinal)
                .or_else(|| {
                    self.view
                        .iter()
                        .flat_map(|view| &view.entries)
                        .filter_map(|entry| entry.submission.as_ref())
                        .find(|submission| submission.id == input.id)
                        .map(|submission| submission.ordinal)
                })
                .unwrap_or(i64::MAX)
        });
        queue
    }
}

pub(super) fn panel_lines(jobs: &[JobSummary], width: usize) -> Vec<Line<'static>> {
    let jobs: Vec<_> = jobs
        .iter()
        .filter(|job| {
            job.kind == JobKind::Shell
                && job.background
                && matches!(job.state, JobState::Running | JobState::Stopping)
        })
        .collect();
    if jobs.is_empty() || width == 0 {
        return Vec::new();
    }
    let running = jobs
        .iter()
        .filter(|job| job.state == JobState::Running)
        .count();
    let stopping = jobs.len() - running;
    let mut heading = format!("background shell commands · {running} running");
    if stopping > 0 {
        heading.push_str(&format!(" · {stopping} stopping"));
    }
    let mut lines = vec![header(heading, width)];
    for job in jobs {
        let color = if job.state == JobState::Stopping {
            Palette::WARN()
        } else {
            Palette::RUNNING()
        };
        let id = format!("{} ", job.id);
        let remaining = width.saturating_sub(2 + id.width() + "[bash] ".width());
        lines.push(clipped(
            vec![
                Span::styled("• ", Style::default().fg(color)),
                Span::styled(id, Style::default().fg(Palette::DIM())),
                Span::styled("[bash] ", Style::default().fg(Palette::FAINT())),
                Span::styled(
                    truncate_mid(&single_line(&job.title), remaining.min(width / 2)),
                    Style::default().fg(Palette::TEXT()),
                ),
            ],
            width,
        ));
    }
    lines
}

pub(super) fn queue_lines(queue: &[QueuedInput], width: usize) -> Vec<Line<'static>> {
    if queue.is_empty() || width == 0 {
        return Vec::new();
    }
    let mut lines = vec![header(
        format!("queued · {} · sent after this turn", queue.len()),
        width,
    )];
    for input in queue {
        lines.push(clipped(
            vec![
                Span::styled("› ", Style::default().fg(Palette::DIM())),
                Span::styled(
                    truncate_mid(&single_line(&input.text), width.saturating_sub(2)),
                    Style::default().fg(Palette::TEXT()),
                ),
            ],
            width,
        ));
    }
    lines
}

pub(super) fn notice(jobs: &[JobSummary]) -> String {
    if jobs.is_empty() {
        return "(no background jobs)".into();
    }
    let mut lines = vec!["background jobs".to_owned()];
    for job in jobs {
        let kind = match job.kind {
            JobKind::Shell => "bash",
            JobKind::Task => "task",
            JobKind::Workflow => "workflow",
            JobKind::Turn => "turn",
        };
        let state = match job.state {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Stopping => "stopping",
            JobState::Completed => "done",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
            JobState::Unknown => "unknown",
        };
        lines.push(format!(
            "  {} [{state}] {kind}: {}",
            job.id,
            single_line(&job.title)
        ));
    }
    lines.push("Use /jobs <job-id> to inspect output · /jobs stop <job-id> to stop".into());
    lines.join("\n")
}

fn single_line(text: &str) -> String {
    safe(&text.replace(['\r', '\n'], " "))
}

fn header(text: String, width: usize) -> Line<'static> {
    clipped(
        vec![Span::styled(
            text,
            Style::default()
                .fg(Palette::ACCENT())
                .add_modifier(Modifier::BOLD),
        )],
        width,
    )
}

fn clipped(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let mut remaining = width;
    let mut result = Vec::new();
    for span in spans {
        let mut end = 0;
        for grapheme in span.content.graphemes(true) {
            if grapheme.width() > remaining {
                break;
            }
            remaining -= grapheme.width();
            end += grapheme.len();
        }
        let truncated = end < span.content.len();
        if end > 0 {
            result.push(Span::styled(span.content[..end].to_owned(), span.style));
        }
        if truncated || remaining == 0 {
            break;
        }
    }
    Line::from(result)
}

fn truncate_mid(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let left_budget = (width - 1) / 2;
    let right_budget = width - 1 - left_budget;
    let mut left_end = 0;
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        if used + grapheme.width() > left_budget {
            break;
        }
        used += grapheme.width();
        left_end += grapheme.len();
    }
    let mut right_start = text.len();
    used = 0;
    for grapheme in text.graphemes(true).rev() {
        if used + grapheme.width() > right_budget {
            break;
        }
        used += grapheme.width();
        right_start -= grapheme.len();
    }
    format!("{}…{}", &text[..left_end], &text[right_start..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use bob_kernel::{CommandId, JobId, SessionId, ThreadId};

    fn job(id: &str, kind: JobKind, state: JobState) -> JobSummary {
        JobSummary {
            id: JobId(id.into()),
            session: SessionId::new(),
            thread: ThreadId::new(),
            operation: None,
            run: None,
            kind,
            state,
            title: "cargo check".into(),
            result: None,
            started_at: 0,
            background: true,
        }
    }

    fn queued(text: &str) -> QueuedInput {
        QueuedInput {
            id: CommandId::new(),
            thread: ThreadId::new(),
            text: text.into(),
        }
    }

    #[test]
    fn panel_shows_only_running_and_stopping_shells() {
        let mut jobs = vec![
            job("running", JobKind::Shell, JobState::Running),
            job("stopping", JobKind::Shell, JobState::Stopping),
        ];
        for kind in [JobKind::Task, JobKind::Workflow, JobKind::Turn] {
            jobs.push(job("hidden-agent", kind, JobState::Running));
        }
        for state in [
            JobState::Queued,
            JobState::Completed,
            JobState::Failed,
            JobState::Cancelled,
            JobState::Unknown,
        ] {
            jobs.push(job("hidden-shell", JobKind::Shell, state));
        }
        let mut foreground = job("foreground", JobKind::Shell, JobState::Running);
        foreground.background = false;
        jobs.push(foreground);
        let lines = panel_lines(&jobs, 100);
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0].to_string(),
            "background shell commands · 1 running · 1 stopping"
        );
        assert_eq!(lines[1].to_string(), "• running [bash] cargo check");
        assert_eq!(lines[2].to_string(), "• stopping [bash] cargo check");
        assert_eq!(lines[1].spans[0].style.fg, Some(Palette::RUNNING()));
        assert_eq!(lines[2].spans[0].style.fg, Some(Palette::WARN()));
        assert!(panel_lines(&jobs[2..], 100).is_empty());
        assert!(panel_lines(&[], 100).is_empty());
    }

    #[test]
    fn queue_preserves_order_and_production_header() {
        let queue = [queued("first\nmessage"), queued("second message")];
        let lines = queue_lines(&queue, 80);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].to_string(), "queued · 2 · sent after this turn");
        assert_eq!(lines[1].to_string(), "› first message");
        assert_eq!(lines[2].to_string(), "› second message");
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert!(queue_lines(&[], 80).is_empty());
    }

    #[test]
    fn narrow_panels_are_safe_single_rows_with_intact_graphemes() {
        let mut jobs = vec![job("job_1", JobKind::Shell, JobState::Running)];
        jobs[0].title = "\x1b[31m界e\u{301}👩\u{200d}💻\nlong command\x1b[0m".into();
        let queue = [queued(&jobs[0].title)];
        for width in 0..100 {
            for line in panel_lines(&jobs, width)
                .into_iter()
                .chain(queue_lines(&queue, width))
            {
                assert!(line.width() <= width, "width {width}: {line}");
                assert!(!line.to_string().contains(['\n', '\r', '\x1b']));
            }
        }
        assert!(panel_lines(&jobs, 0).is_empty());
        assert!(queue_lines(&queue, 0).is_empty());
        assert_eq!(
            truncate_mid("e\u{301}abcdef👩\u{200d}💻", 5),
            "e\u{301}a…👩\u{200d}💻"
        );
    }

    #[test]
    fn jobs_notice_includes_retained_and_non_shell_work() {
        let jobs = [
            job("job_1", JobKind::Shell, JobState::Running),
            job("task_1", JobKind::Task, JobState::Completed),
            job("workflow_1", JobKind::Workflow, JobState::Failed),
            job("turn_1", JobKind::Turn, JobState::Cancelled),
            job("job_2", JobKind::Shell, JobState::Queued),
            job("job_3", JobKind::Shell, JobState::Stopping),
            job("job_4", JobKind::Shell, JobState::Unknown),
        ];
        assert_eq!(notice(&[]), "(no background jobs)");
        assert_eq!(
            notice(&jobs),
            "background jobs\n  job_1 [running] bash: cargo check\n  task_1 [done] task: cargo check\n  workflow_1 [failed] workflow: cargo check\n  turn_1 [cancelled] turn: cargo check\n  job_2 [queued] bash: cargo check\n  job_3 [stopping] bash: cargo check\n  job_4 [unknown] bash: cargo check\nUse /jobs <job-id> to inspect output · /jobs stop <job-id> to stop"
        );
    }
}
