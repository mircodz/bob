use super::visual::{Scheduler, Urgency, STREAM_FRAME};
use super::*;
use bob_kernel::{LiveAttempt, LiveSnapshot, ModelActivity, RunId, SessionId};
use ratatui::{backend::TestBackend, layout::Rect};
use std::sync::Arc;
use tokio::time::Instant;

fn thread() -> ThreadSummary {
    ThreadSummary {
        id: ThreadId("root".into()),
        session: SessionId("session".into()),
        parent_run: None,
        parent_thread: None,
        needs_approval: false,
        activity: None,
        name: "root".into(),
        model: "fixture".into(),
        state: ThreadState::Idle,
        run: None,
    }
}

fn view(sequence: i64) -> View {
    View {
        sequence,
        entry_sequence: 0,
        threads: vec![thread()],
        policies: vec![],
        entries: vec![],
        decisions: vec![],
        operations: vec![],
        error: None,
    }
}

fn app() -> App {
    let mut app = App::new(80);
    app.select(ThreadId("root".into()));
    app.reply(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(view(1)),
    });
    app
}

fn snapshot(revision: u64) -> Arc<LiveSnapshot> {
    Arc::new(LiveSnapshot {
        instance: "runtime".into(),
        revision,
        sequence: 1,
        attempts: vec![Arc::new(LiveAttempt {
            thread: ThreadId("root".into()),
            run: RunId("run".into()),
            operation: OperationId("operation".into()),
            revision,
            activity: ModelActivity::Answering,
            text: format!("stream {revision}"),
            reasoning: vec![],
            truncated: false,
            started_at: std::time::Instant::now(),
            stopping: false,
            settled_sequence: None,
            error: None,
        })],
        error: None,
    })
}

#[tokio::test]
async fn unchanged_housekeeping_has_zero_draws() {
    let mut app = app();
    let mut jobs = JoinSet::new();
    app.refresh_visual(&mut jobs);
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let now = Instant::now();
    let mut scheduler = Scheduler::new(now, app.animation_deadline(now));
    for tick in 1..=1000 {
        let now = now + Duration::from_millis(tick * 80);
        if app.refresh_visual(&mut jobs) {
            scheduler.invalidate(now, Urgency::Immediate);
        }
        if scheduler.due(now) {
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            scheduler.drawn(now, app.animation_deadline(now));
        }
    }
    assert_eq!(scheduler.draws, 0);
    assert_eq!(scheduler.deadline(), None);
}

#[test]
fn continuous_stream_is_bounded_and_never_postpones_its_deadline() {
    let start = Instant::now();
    let mut scheduler = Scheduler::new(start, None);
    for millis in 1..=1000 {
        let now = start + Duration::from_millis(millis);
        let previous = scheduler.deadline();
        scheduler.invalidate(now, Urgency::Stream);
        if let Some(previous) = previous {
            assert_eq!(scheduler.deadline(), Some(previous));
        }
        assert!(scheduler.deadline().unwrap() <= now + STREAM_FRAME);
        if scheduler.due(now) {
            scheduler.drawn(now, None);
        }
    }
    assert_eq!(scheduler.draws, 30);
    assert!(scheduler.coalesced > 900);
}

#[test]
fn immediate_input_or_error_bypasses_pending_stream_frame() {
    let start = Instant::now();
    let mut scheduler = Scheduler::new(start, None);
    scheduler.invalidate(start + Duration::from_millis(1), Urgency::Stream);
    let input = start + Duration::from_millis(2);
    assert!(!scheduler.due(input));
    scheduler.invalidate(input, Urgency::Immediate);
    assert!(scheduler.due(input));
    scheduler.drawn(input, None);
    assert_eq!(scheduler.deadline(), None);
    scheduler.invalidate(input, Urgency::Stream);
    assert_eq!(scheduler.deadline(), Some(input + STREAM_FRAME));
}

#[test]
fn animation_deadline_survives_housekeeping_and_stream_updates() {
    let start = Instant::now();
    let animation = start + Duration::from_millis(100);
    let mut scheduler = Scheduler::new(start, Some(animation));
    assert!(!scheduler.due(start + Duration::from_millis(80)));
    scheduler.invalidate(start + Duration::from_millis(90), Urgency::Stream);
    scheduler.drawn(start + Duration::from_millis(90), Some(animation));
    assert_eq!(scheduler.deadline(), Some(animation));
    assert!(scheduler.due(animation));
    scheduler.drawn(animation, None);
    assert_eq!(scheduler.deadline(), None);
}

#[test]
fn fallback_watermark_only_views_do_not_redraw_but_errors_and_sidebar_changes_do() {
    let mut app = app();
    for sequence in 2..=100 {
        assert!(!app.reply_visual(Reply::View {
            thread: ThreadId("root".into()),
            generation: 0,
            result: Ok(view(sequence)),
        }));
    }
    let mut changed = view(101);
    changed.threads[0].state = ThreadState::Failed;
    assert!(app.reply_visual(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(changed),
    }));
    let mut error = view(102);
    error.error = Some("kernel stopped".into());
    assert!(app.reply_visual(Reply::View {
        thread: ThreadId("root".into()),
        generation: 0,
        result: Ok(error),
    }));
}

#[test]
fn live_content_coalesces_but_new_errors_are_immediate() {
    let mut app = app();
    app.accept_live(snapshot(1));
    assert_eq!(app.live_urgency(&snapshot(2)), Some(Urgency::Stream));
    assert_eq!(app.live_urgency(&snapshot(1)), None);
    let mut watermark = (*snapshot(1)).clone();
    watermark.revision = 2;
    watermark.sequence = 20;
    assert_eq!(app.live_urgency(&watermark), None);
    let mut error = (*snapshot(2)).clone();
    Arc::make_mut(&mut error.attempts[0]).error = Some("stream failed".into());
    assert_eq!(app.live_urgency(&error), Some(Urgency::Immediate));
    app.focus = Focus::Inspector;
    assert_eq!(app.live_urgency(&snapshot(2)), None);
    error.error = Some("kernel failed".into());
    assert_eq!(app.live_urgency(&error), Some(Urgency::Immediate));
}

#[test]
fn animation_only_runs_for_visible_activity_and_receipts_expire_once() {
    let mut app = app();
    let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
    app.threads[0].state = ThreadState::Active;
    app.activity.observe(&app.threads);
    terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    let now = Instant::now();
    let animation = app.animation_deadline(now).unwrap();
    assert!(animation > now && animation <= now + Duration::from_millis(100));
    app.pane_mut().unwrap().viewport.height = 0;
    assert_eq!(app.animation_deadline(now), None);
    app.threads[0].state = ThreadState::Idle;
    app.pane_mut().unwrap().receipt = Some(("Sent".into(), now.into_std()));
    assert_eq!(
        app.animation_deadline(now),
        Some(now + Duration::from_secs(3))
    );
    assert_eq!(app.animation_deadline(now + Duration::from_secs(3)), None);
    app.size = Rect::new(0, 0, 10, 4);
    assert_eq!(app.animation_deadline(now), None);
}

#[tokio::test]
async fn scheduled_draw_wakes_without_input_or_housekeeping() {
    let mut events = futures::stream::pending::<io::Result<Event>>();
    let mut jobs = JoinSet::new();
    let mut receiver = None;
    let now = Instant::now();
    let mut tick = tokio::time::interval_at(now + Duration::from_secs(10), Duration::from_secs(10));
    let event = tokio::time::timeout(
        Duration::from_secs(1),
        next_event_or_draw(&mut events, &mut jobs, &mut tick, &mut receiver, Some(now)),
    )
    .await
    .unwrap();
    assert!(event.is_none());
}

#[tokio::test]
async fn scheduled_deadlines_preserve_fairness_between_ready_sources() {
    let mut events = futures::stream::repeat_with(|| {
        Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        )))
    });
    let mut jobs = JoinSet::new();
    jobs.spawn(async { Reply::Ready(Err("fixture".into())) });
    tokio::task::yield_now().await;
    let (sender, receiver) = tokio::sync::watch::channel(snapshot(0));
    let mut receiver = Some(receiver);
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut observed = [false; 5];
    for revision in 1..=512 {
        sender.send_replace(snapshot(revision));
        let overdue = Instant::now() - Duration::from_secs(1);
        tick.reset_at(overdue);
        let event = next_event_or_draw(
            &mut events,
            &mut jobs,
            &mut tick,
            &mut receiver,
            Some(overdue),
        )
        .await;
        match event {
            Some(NextEvent::Input(_)) => observed[0] = true,
            Some(NextEvent::Reply(_)) => observed[1] = true,
            Some(NextEvent::Tick) => observed[2] = true,
            Some(NextEvent::Live(_)) => observed[3] = true,
            None => observed[4] = true,
        }
        if observed.iter().all(|seen| *seen) {
            break;
        }
    }
    assert_eq!(observed, [true; 5]);
}

#[tokio::test]
async fn disconnected_watch_is_removed_and_housekeeping_continues() {
    let mut events = futures::stream::pending::<io::Result<Event>>();
    let mut jobs = JoinSet::new();
    let (sender, receiver) = tokio::sync::watch::channel(snapshot(1));
    let mut app = app();
    app.live_receiver = Some(receiver);
    drop(sender);
    let now = Instant::now();
    let mut tick = tokio::time::interval_at(now + Duration::from_secs(10), Duration::from_secs(10));
    let event = tokio::time::timeout(
        Duration::from_secs(1),
        next_event(&mut events, &mut jobs, &mut tick, &mut app.live_receiver),
    )
    .await
    .unwrap();
    assert!(matches!(event, NextEvent::Live(None)));
    app.disconnect_live();
    assert!(app.live_receiver.is_none());
    tick.reset_immediately();
    let event = tokio::time::timeout(
        Duration::from_secs(1),
        next_event(&mut events, &mut jobs, &mut tick, &mut app.live_receiver),
    )
    .await
    .unwrap();
    assert!(matches!(event, NextEvent::Tick));
}
