use super::*;
use bob_kernel::{ModelActivity, OperationId, SessionId};

fn thread(name: &str) -> ThreadSummary {
    ThreadSummary {
        id: ThreadId(name.into()),
        session: SessionId("session".into()),
        parent_run: None,
        parent_thread: None,
        needs_approval: false,
        activity: Some(ThreadActivity::Model),
        name: name.into(),
        model: "fixture".into(),
        state: ThreadState::Active,
        run: Some(RunId(format!("run-{name}"))),
    }
}

fn attempt(thread: &ThreadSummary, started_at: Instant) -> LiveAttempt {
    LiveAttempt {
        thread: thread.id.clone(),
        run: thread.run.clone().unwrap(),
        operation: OperationId("model-one".into()),
        revision: 1,
        activity: ModelActivity::Waiting,
        text: String::new(),
        reasoning: vec![],
        truncated: false,
        started_at,
        stopping: false,
        settled_sequence: None,
        error: None,
    }
}

#[test]
fn ordinary_phases_use_the_exact_shared_working_line() {
    let now = Instant::now();
    let since = now - Duration::from_millis(125_999);
    let mut thread = thread("root");
    let mut attempt = attempt(&thread, now - Duration::from_secs(2));
    for activity in [
        None,
        Some(ThreadActivity::Model),
        Some(ThreadActivity::Tool {
            name: "bash".into(),
        }),
        Some(ThreadActivity::Agent),
    ] {
        thread.activity = activity;
        for model_activity in [
            ModelActivity::Waiting,
            ModelActivity::Thinking,
            ModelActivity::Answering,
            ModelActivity::ToolCall,
        ] {
            attempt.activity = model_activity;
            for millis in [0, 99, 100, 599, 1500] {
                assert_eq!(
                    line_at(
                        &thread,
                        Some(&attempt),
                        Some(since),
                        Duration::from_millis(millis),
                        "Esc to interrupt",
                        now,
                    ),
                    Some(working_line(
                        125,
                        (millis / 100) as usize,
                        "Esc to interrupt"
                    ))
                );
            }
        }
    }
}

#[test]
fn clock_survives_phases_state_changes_and_new_attempts() {
    let start = Instant::now();
    let mut clock = Clock::default();
    let mut thread = thread("root");
    clock.observe_at(std::slice::from_ref(&thread), start);
    for (index, (state, activity)) in [
        (ThreadState::Active, ThreadActivity::Model),
        (
            ThreadState::Active,
            ThreadActivity::Tool {
                name: "bash".into(),
            },
        ),
        (ThreadState::Active, ThreadActivity::Approval),
        (ThreadState::Held, ThreadActivity::Approval),
        (ThreadState::Active, ThreadActivity::Agent),
        (ThreadState::Stopping, ThreadActivity::Agent),
        (ThreadState::Active, ThreadActivity::Model),
    ]
    .into_iter()
    .enumerate()
    {
        thread.state = state;
        thread.activity = Some(activity);
        clock.observe_at(
            std::slice::from_ref(&thread),
            start + Duration::from_secs(index as u64 + 1),
        );
        assert_eq!(clock.since(&thread.id), Some(start));
    }
    let now = start + Duration::from_secs(30);
    for (index, started_at) in [start, start + Duration::from_secs(20)]
        .into_iter()
        .enumerate()
    {
        let mut attempt = attempt(&thread, started_at);
        attempt.operation = OperationId(format!("model-{index}"));
        assert_eq!(
            line_at(
                &thread,
                Some(&attempt),
                clock.since(&thread.id),
                Duration::ZERO,
                "Esc to interrupt",
                now,
            ),
            Some(working_line(30, 0, "Esc to interrupt"))
        );
    }
    thread.run = Some(RunId("new-run".into()));
    clock.observe_at(std::slice::from_ref(&thread), now);
    assert_eq!(clock.since(&thread.id), Some(now));
}

#[test]
fn clocks_are_thread_specific_and_remove_missing_threads() {
    let start = Instant::now();
    let root = thread("root");
    let mut child = thread("child");
    let mut clock = Clock::default();
    clock.observe_at(std::slice::from_ref(&root), start);
    let child_start = start + Duration::from_secs(10);
    clock.observe_at(&[root.clone(), child.clone()], child_start);
    assert_eq!(clock.since(&root.id), Some(start));
    assert_eq!(clock.since(&child.id), Some(child_start));
    child.run = Some(RunId("new-child-run".into()));
    let child_restart = start + Duration::from_secs(20);
    clock.observe_at(&[root.clone(), child.clone()], child_restart);
    assert_eq!(clock.since(&root.id), Some(start));
    assert_eq!(clock.since(&child.id), Some(child_restart));
    clock.observe_at(std::slice::from_ref(&child), child_restart);
    assert_eq!(clock.since(&root.id), None);
    clock.observe_at(&[], child_restart);
    assert_eq!(clock.since(&child.id), None);
}

#[test]
fn idle_and_terminal_states_clear_clocks_and_hide_activity() {
    let start = Instant::now();
    for state in [
        ThreadState::Idle,
        ThreadState::Interrupted,
        ThreadState::Completed,
        ThreadState::Failed,
        ThreadState::Cancelled,
    ] {
        let mut thread = thread("root");
        let mut clock = Clock::default();
        let mut attempt = attempt(&thread, start);
        attempt.stopping = true;
        clock.observe_at(std::slice::from_ref(&thread), start);
        thread.state = state;
        thread.needs_approval = true;
        let now = start + Duration::from_secs(10);
        clock.observe_at(std::slice::from_ref(&thread), now);
        assert_eq!(clock.since(&thread.id), None);
        assert!(line_at(
            &thread,
            Some(&attempt),
            Some(start),
            Duration::ZERO,
            "Esc to interrupt",
            now,
        )
        .is_none());
        thread.state = ThreadState::Active;
        clock.observe_at(std::slice::from_ref(&thread), now);
        assert_eq!(clock.since(&thread.id), Some(now));
    }
}

#[test]
fn active_join_without_a_run_keeps_observed_clock() {
    let start = Instant::now();
    let mut thread = thread("root");
    thread.run = None;
    thread.activity = Some(ThreadActivity::Agent);
    let mut clock = Clock::default();
    clock.observe_at(std::slice::from_ref(&thread), start);
    let now = start + Duration::from_secs(12);
    clock.observe_at(std::slice::from_ref(&thread), now);
    assert_eq!(clock.since(&thread.id), Some(start));
    assert_eq!(
        line_at(
            &thread,
            None,
            clock.since(&thread.id),
            Duration::ZERO,
            "Esc to interrupt",
            now,
        ),
        Some(working_line(12, 0, "Esc to interrupt"))
    );
    thread.run = Some(RunId("next-run".into()));
    clock.observe_at(std::slice::from_ref(&thread), now);
    assert_eq!(clock.since(&thread.id), Some(now));
}

#[test]
fn attachment_clock_is_observed_not_historic_attempt_age() {
    let now = Instant::now();
    let thread = thread("root");
    let attempt = attempt(&thread, now - Duration::from_secs(90));
    let mut clock = Clock::default();
    clock.observe_at(std::slice::from_ref(&thread), now);
    assert_eq!(
        line_at(
            &thread,
            Some(&attempt),
            clock.since(&thread.id),
            Duration::ZERO,
            "Esc to interrupt",
            now,
        ),
        Some(working_line(0, 0, "Esc to interrupt"))
    );
    assert_eq!(
        line_at(
            &thread,
            Some(&attempt),
            None,
            Duration::ZERO,
            "Esc to interrupt",
            now,
        ),
        Some(working_line(90, 0, "Esc to interrupt"))
    );
}

#[test]
fn stale_or_other_thread_attempts_cannot_supply_time_or_stopping_state() {
    let now = Instant::now();
    let thread = thread("root");
    let mut stale_run = attempt(&thread, now - Duration::from_secs(90));
    stale_run.run = RunId("old-run".into());
    let mut other_thread = attempt(&thread, now - Duration::from_secs(90));
    other_thread.thread = ThreadId("child".into());
    let mut settled = attempt(&thread, now - Duration::from_secs(90));
    settled.settled_sequence = Some(10);
    for mut attempt in [stale_run, other_thread, settled] {
        attempt.stopping = true;
        assert_eq!(
            line_at(
                &thread,
                Some(&attempt),
                None,
                Duration::ZERO,
                "Esc to interrupt",
                now,
            ),
            Some(working_line(0, 0, "Esc to interrupt"))
        );
    }
}

#[test]
fn stopping_and_approval_keep_truthful_compact_hints() {
    let now = Instant::now();
    let start = now - Duration::from_secs(65);
    let mut thread = thread("root");
    let mut attempt = attempt(&thread, now);
    attempt.stopping = true;
    let render = |thread: &ThreadSummary, attempt: Option<&LiveAttempt>| {
        line_at(
            thread,
            attempt,
            Some(start),
            Duration::from_millis(500),
            "Esc to interrupt",
            now,
        )
        .unwrap()
    };
    let stopping = render(&thread, Some(&attempt));
    assert_eq!(
        stopping.to_string(),
        "• Stopping (65s · waiting for interruption)"
    );
    assert_eq!(&stopping.spans[1..9], shimmer_spans("Stopping", 5));
    assert_eq!(stopping.spans[0].style.fg, Some(Palette::RUNNING()));
    assert_eq!(
        stopping.spans.last().unwrap().style.fg,
        Some(Palette::FAINT())
    );
    thread.state = ThreadState::Stopping;
    assert_eq!(render(&thread, None), stopping);
    thread.state = ThreadState::Active;
    thread.needs_approval = true;
    let approval = render(&thread, None);
    assert_eq!(
        approval.to_string(),
        "• Approval needed (65s · Tab to review · Esc denies)"
    );
    assert_eq!(approval.spans[0], stopping.spans[0]);
    assert_eq!(approval.spans[1].style.fg, Some(Color::Rgb(102, 102, 102)));
    assert_eq!(approval.spans[2].style.fg, Some(Palette::FAINT()));
    thread.needs_approval = false;
    thread.activity = Some(ThreadActivity::Approval);
    assert_eq!(render(&thread, None), approval);
    thread.state = ThreadState::Held;
    assert_eq!(render(&thread, None), approval);
}

#[test]
fn long_caller_hints_are_not_truncated() {
    let now = Instant::now();
    let hint = "Ctrl+T agents · Esc to interrupt".repeat(10);
    let line = line_at(&thread("root"), None, None, Duration::ZERO, &hint, now).unwrap();
    assert_eq!(line.to_string(), format!("• Working (0s · {hint})"));
}
