use anyhow::{bail, Context, Result};
use bob_kernel::{
    ArtifactId, Command, CommandId, EntryKind, Handle, OperationId, OperationState, Receipt,
    SessionPolicy, ThreadId, ThreadState, ThreadSummary, View,
};
#[cfg(test)]
use bob_kernel::{Policy, Profile, ThreadActivity};
#[cfg(test)]
use bob_types::ReasoningEffort;
use crossterm::{
    event::{
        EnableBracketedPaste, EnableMouseCapture, Event, EventStream, KeyCode, KeyEvent,
        KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{enable_raw_mode, EnterAlternateScreen},
};
use futures::{Stream, StreamExt};
#[cfg(test)]
use ratatui::style::Modifier;
use ratatui::{
    backend::CrosstermBackend,
    style::Style,
    text::Line,
    widgets::{Block, Paragraph, Wrap},
    Frame, Terminal,
};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::task::JoinSet;

pub(crate) type Options = bob::application::LaunchOptions;

mod activity;
mod approval;
#[cfg(test)]
mod bash_parity_tests;
mod commands;
mod completion;
mod composer;
mod control_render;
mod exploration;
mod inline;
#[cfg(test)]
mod inline_tests;
mod jobs;
mod live;
#[cfg(test)]
mod live_tests;
mod markdown;
#[cfg(test)]
mod output_parity_tests;
#[cfg(test)]
mod parity_tests;
#[cfg(test)]
mod regressions;
mod render;
#[cfg(test)]
mod runtime_tests;
mod state;
mod style;
mod title;
#[cfg(test)]
mod tool_parity_tests;
mod tool_render;
#[cfg(all(unix, test))]
mod unix_input_tests;
mod visibility;
mod visual;
#[cfg(test)]
mod visual_tests;
#[cfg(all(windows, test))]
mod windows_tests;
mod workflow;
use render::draw;
use state::*;

fn route(text: &str, thread: &ThreadSummary, policy: Option<&SessionPolicy>) -> Result<Action> {
    let trimmed = text.trim();
    let mut words = trimmed.split_whitespace();
    let verb = words.next().unwrap_or_default();
    let args: Vec<_> = words.collect();
    let id = CommandId::new();
    let command = match (verb, args.as_slice()) {
        ("/stop", []) => Command::StopThread { id, thread: thread.id.clone() },
        ("/resume", []) => Command::Resume { id, thread: thread.id.clone() },
        ("/approve" | "/deny", [operation, revision]) => Command::Decide {
            id, operation: OperationId((*operation).into()), revision: revision.parse().context("Revision must be an integer")?, approve: verb == "/approve",
        },
        ("/resolve", [operation, evidence @ ..]) if !evidence.is_empty() => Command::ResolveUnknown {
            id, operation: OperationId((*operation).into()), evidence: evidence.join(" "),
        },
        ("/artifact", [artifact]) => return Ok(Action::Artifact(ArtifactId((*artifact).into()), 0)),
        ("/artifact", [artifact, offset]) => return Ok(Action::Artifact(ArtifactId((*artifact).into()), offset.parse().context("Offset must be a nonnegative integer")?)),
        ("/yolo", []) => {
            let current = policy.context("Current session policy is unavailable; wait for a successful refresh")?;
            let mut policy = current.policy.clone();
            policy.yolo = !policy.yolo;
            Command::SetPolicy { id, session: thread.session.clone(), policy, expected_revision: current.revision }
        }
        (verb, _) if verb.starts_with('/') => bail!("Commands: /stop, /resume, /yolo, /approve <operation> <revision>, /deny <operation> <revision>, /resolve <operation> <evidence>, /artifact <id> [offset], /quit"),
        _ => Command::Submit { id, thread: thread.id.clone(), text: text.to_owned() },
    };
    Ok(Action::Command(command))
}

async fn initialize(
    options: Options,
    auto_titles: bool,
) -> Result<(bob::application::Application, View)> {
    let page_size = options.page_size;
    let application = bob::application::open_with_titles(options, auto_titles).await?;
    let result = application
        .handle
        .transcript(Some(application.session.thread.clone()), None, page_size)
        .await;
    match result {
        Ok(view) => Ok((application, view)),
        Err(error) => {
            let _ = application.handle.shutdown().await;
            Err(error)
        }
    }
}

pub(crate) async fn run(options: Options, terminal_titles: bool) -> Result<()> {
    let mut guard = TerminalGuard::activate_with_titles(terminal_titles)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::new(options.page_size);
    app.configure(
        options.workspace.clone(),
        options.model.clone().unwrap_or_default(),
    );
    let mut jobs = JoinSet::new();
    guard.set_title(terminal.backend_mut(), &app.terminal_title());
    terminal.draw(|frame| draw(frame, &mut app))?;
    app.refresh_inline(&mut jobs);
    jobs.spawn(async move {
        Reply::ApplicationReady(
            initialize(options, terminal_titles)
                .await
                .map_err(|error| format!("{error:#}")),
        )
    });
    let mut events = InputEvents {
        events: EventStream::new(),
        guard: &guard,
    };
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let now = tokio::time::Instant::now();
    let mut scheduler = visual::Scheduler::new(now, app.animation_deadline(now));
    let result: Result<()> = async {
        loop {
            guard.record_mode()?;
            let event = next_event_or_draw(
                &mut events, &mut jobs, &mut tick, &mut app.live_receiver, scheduler.deadline(),
            ).await;
            let mut urgency = None;
            match event {
                Some(NextEvent::Input(event)) => {
                    let Some(event) = event else { break; };
                    match event? {
                        Event::Key(key) if key.kind != KeyEventKind::Release => {
                            if app.key(key, &mut jobs) { break; }
                            urgency = Some(visual::Urgency::Immediate);
                        }
                        Event::Paste(text) => {
                            app.paste(&text);
                            urgency = Some(visual::Urgency::Immediate);
                        }
                        Event::Mouse(mouse) => {
                            if !matches!(mouse.kind, crossterm::event::MouseEventKind::Moved) {
                                app.mouse(mouse, &mut jobs);
                                urgency = Some(visual::Urgency::Immediate);
                            }
                        }
                        Event::Resize(_, _) => {
                            app.hits.clear();
                            urgency = Some(visual::Urgency::Immediate);
                        }
                        _ => {}
                    }
                }
                Some(NextEvent::Reply(reply)) => {
                    match reply {
                        Some(Ok(reply)) => {
                            let reply_urgency = visual::reply_urgency(&reply);
                            if app.reply_visual(*reply) {
                                urgency = Some(reply_urgency);
                            }
                            app.refresh_inline(&mut jobs);
                        }
                        Some(Err(error)) => {
                            app.status = format!("Background task failed: {error}; pending results are unknown. Restart to inspect.");
                            let notice = app.status.clone();
                            if let Some(pane) = app.pane_mut() { pane.notify_current(notice); }
                            app.reading = false;
                            urgency = Some(visual::Urgency::Immediate);
                        }
                        None => {}
                    }
                }
                Some(NextEvent::Tick) => {
                    if app.refresh_visual(&mut jobs) {
                        urgency = Some(visual::Urgency::Immediate);
                    }
                }
                Some(NextEvent::Live(snapshot)) => {
                    if let Some(snapshot) = snapshot {
                        urgency = app.live_urgency(&snapshot);
                        app.accept_live(snapshot);
                        if app.refresh_visual(&mut jobs) {
                            urgency = Some(visual::Urgency::Immediate);
                        }
                    } else {
                        app.disconnect_live();
                        app.refresh(&mut jobs);
                        urgency = Some(visual::Urgency::Immediate);
                    }
                }
                None => {}
            }
            guard.record_mode()?;
            let now = tokio::time::Instant::now();
            if let Some(urgency) = urgency {
                scheduler.invalidate(now, urgency);
            }
            if scheduler.due(now) {
                guard.set_title(terminal.backend_mut(), &app.terminal_title());
                let frame_started = tokio::time::Instant::now();
                let inline_revision = app.pane().map(|pane| pane.inline.revision());
                terminal.draw(|frame| draw(frame, &mut app))?;
                app.refresh_inline(&mut jobs);
                let now = tokio::time::Instant::now();
                scheduler.drawn(now, app.animation_deadline(frame_started));
                if inline_revision != app.pane().map(|pane| pane.inline.revision())
                    || app.pane().is_some_and(|pane| pane.live_segments.pending())
                {
                    scheduler.invalidate(now, visual::Urgency::Stream);
                }
            }
        }
        Ok(())
    }.await;
    jobs.abort_all();
    drop(events);
    drop(terminal);
    let restoration = guard.restore();
    drop(guard);
    let shutdown = if let Some(handle) = app.handle {
        handle.shutdown().await
    } else {
        Ok(())
    };
    result?;
    restoration?;
    shutdown?;
    if let Some(error) = app.startup_error {
        anyhow::bail!("{error}");
    }
    if let Some(session) = app.threads.first().map(|thread| &thread.session) {
        println!("Resume this session with: bob --resume {session}");
    }
    Ok(())
}

enum NextEvent {
    Input(Option<io::Result<Event>>),
    Reply(Option<Result<Box<Reply>, tokio::task::JoinError>>),
    Tick,
    Live(Option<std::sync::Arc<bob_kernel::LiveSnapshot>>),
}

async fn next_event(
    events: &mut (impl Stream<Item = io::Result<Event>> + Unpin),
    jobs: &mut JoinSet<Reply>,
    tick: &mut tokio::time::Interval,
    receiver: &mut Option<tokio::sync::watch::Receiver<std::sync::Arc<bob_kernel::LiveSnapshot>>>,
) -> NextEvent {
    tokio::select! {
        event = events.next() => NextEvent::Input(event),
        reply = jobs.join_next(), if !jobs.is_empty() => NextEvent::Reply(reply.map(|result| result.map(Box::new))),
        _ = tick.tick() => NextEvent::Tick,
        snapshot = live::changed(receiver) => NextEvent::Live(snapshot),
    }
}

async fn next_event_or_draw(
    events: &mut (impl Stream<Item = io::Result<Event>> + Unpin),
    jobs: &mut JoinSet<Reply>,
    tick: &mut tokio::time::Interval,
    receiver: &mut Option<tokio::sync::watch::Receiver<std::sync::Arc<bob_kernel::LiveSnapshot>>>,
    deadline: Option<tokio::time::Instant>,
) -> Option<NextEvent> {
    tokio::select! {
        event = next_event(events, jobs, tick, receiver) => Some(event),
        _ = visual::wait(deadline) => None,
    }
}

fn clipped(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        result.push_str(" … [preview truncated; inspect artifact]");
    }
    result
}

fn sanitize_buffer(buffer: &mut ratatui::buffer::Buffer) {
    for cell in &mut buffer.content {
        if cell.symbol().chars().any(char::is_control) {
            let clean: String = cell
                .symbol()
                .chars()
                .filter(|character| !character.is_control())
                .collect();
            cell.set_symbol(if clean.is_empty() { " " } else { &clean });
        }
    }
}

pub(crate) struct InputEvents<'a> {
    events: EventStream,
    guard: &'a TerminalGuard,
}

impl Stream for InputEvents<'_> {
    type Item = io::Result<Event>;

    fn poll_next(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if let Err(error) = this.guard.record_mode() {
            return Poll::Ready(Some(Err(error)));
        }
        let result = Pin::new(&mut this.events).poll_next(context);
        if result.is_pending() {
            if let Err(error) = this.guard.record_mode() {
                return Poll::Ready(Some(Err(error)));
            }
        }
        result
    }
}

pub(crate) struct TerminalGuard {
    active: bool,
    title: std::cell::RefCell<bob_ui::terminal::title::TerminalTitle>,
    #[cfg(windows)]
    modes: bob_ui::terminal::windows::SavedConsoleModes,
}

impl TerminalGuard {
    pub(crate) fn events(&self) -> InputEvents<'_> {
        InputEvents {
            events: EventStream::new(),
            guard: self,
        }
    }

    pub(crate) fn activate_with_titles(enabled: bool) -> io::Result<Self> {
        let mut guard = Self::activate()?;
        *guard.title.get_mut() = bob_ui::terminal::title::TerminalTitle::new(enabled);
        Ok(guard)
    }

    pub(crate) fn set_title(&self, output: &mut impl io::Write, title: &str) {
        let _ = self.title.borrow_mut().update(output, title);
    }

    pub(crate) fn activate() -> io::Result<Self> {
        #[cfg(windows)]
        let modes = bob_ui::terminal::windows::SavedConsoleModes::capture()?;
        let guard = Self {
            active: true,
            title: std::cell::RefCell::new(bob_ui::terminal::title::TerminalTitle::new(false)),
            #[cfg(windows)]
            modes,
        };
        enable_raw_mode()?;
        guard.record_mode()?;
        #[cfg(windows)]
        guard.modes.enable_ansi_output()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        Ok(guard)
    }

    fn record_mode(&self) -> io::Result<()> {
        #[cfg(windows)]
        self.modes.input.ensure_record_mode()?;
        Ok(())
    }

    pub(crate) fn restore(&mut self) -> io::Result<()> {
        let _ = self.title.get_mut().restore(&mut io::stdout());
        let terminal = if std::mem::take(&mut self.active) {
            bob_ui::terminal::restore_terminal()
        } else {
            Ok(())
        };
        #[cfg(windows)]
        let modes = self.modes.restore();
        #[cfg(not(windows))]
        let modes = Ok(());
        terminal.and(modes)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(all(windows, test))]
use bob_ui::terminal::windows::ConsoleInput;

#[cfg(test)]
mod tests {
    use super::*;
    use bob_kernel::{Entry, RunId, SessionId};
    use bob_ui::theme::Palette;
    use ratatui::{backend::TestBackend, buffer::Buffer, style::Color};

    #[test]
    fn pasted_windows_and_lone_carriage_returns_preserve_multiline_unicode() {
        let mut app = App::new(80);
        let status = app.status.clone();
        app.paste("é\r\n界\r🦀");
        assert_eq!(
            app.draft_mut().resolved_text().as_deref(),
            Some("é\n界\n🦀")
        );
        assert!(app.draft_mut().text.starts_with("[Pasted text #1"));
        assert_eq!(app.status, status);
    }

    fn rendered(app: &mut App) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(180, 40)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        assert!(buffer
            .content
            .iter()
            .all(|cell| !cell.symbol().chars().any(char::is_control)));
        buffer
    }

    fn screen_text(buffer: &Buffer) -> String {
        buffer
            .content
            .chunks(usize::from(buffer.area.width))
            .map(|row| {
                let mut text = String::new();
                let mut column = 0;
                while column < row.len() {
                    let symbol = row[column].symbol();
                    text.push_str(symbol);
                    column += Line::raw(symbol).width().max(1);
                }
                text
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn buffer_guard_removes_all_terminal_controls_without_changing_style_or_unicode() {
        let controls: String = ('\u{0}'..='\u{1f}').chain('\u{7f}'..='\u{9f}').collect();
        let unicode = "e\u{301}";
        let style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
        let mut terminal = Terminal::new(TestBackend::new(3, 1)).unwrap();
        terminal
            .draw(|frame| {
                let buffer = frame.buffer_mut();
                buffer[(0, 0)]
                    .set_symbol(&format!("{controls}{unicode}{controls}"))
                    .set_style(style);
                buffer[(1, 0)].set_symbol(&controls).set_style(style);
                buffer[(2, 0)].set_symbol("λ").set_style(style);
                sanitize_buffer(buffer);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), unicode);
        assert_eq!(buffer[(1, 0)].symbol(), " ");
        assert_eq!(buffer[(2, 0)].symbol(), "λ");
        for cell in &buffer.content {
            assert_eq!(cell.fg, Color::Red);
            assert!(cell.modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn rendered_untrusted_content_is_inert_and_source_content_is_unchanged() {
        let unicode = "café 界 e\u{301} 👩\u{200d}💻";
        for payload in [
            "\u{1b}[2J\u{1b}[H",
            "\u{1b}]52;c;YQ==\u{7}",
            "\u{1b}]0;title\u{1b}\\",
            "\u{9d}52;c;YQ==\u{9c}\u{9b}2J",
            "\u{0}\u{8}\u{b}\u{c}\u{7f}\u{85}\u{90}",
        ] {
            let text = format!("{unicode}{payload} end");
            let mut app = App::new(80);
            let mut selected = thread("root");
            selected.name = format!("名{payload}");
            selected.parent_thread = Some(ThreadId("parent".into()));
            selected.state = ThreadState::Active;
            selected.id = ThreadId(format!("id{payload}"));
            app.select(selected.id.clone());
            app.threads.push(selected.clone());
            app.sidebar = true;
            app.status = text.clone();
            let pane = app.pane_mut().unwrap();
            pane.draft.insert(&text);
            pane.notice = text.clone();
            pane.view = Some(View {
                policies: vec![],
                sequence: 1,
                entry_sequence: 1,
                threads: vec![],
                entries: vec![Entry {
                    sequence: 1,
                    sender: None,
                    parent_guidance: false,
                    submission: None,
                    thread: selected.id.clone(),
                    kind: EntryKind::Notice,
                    text: text.clone(),
                    artifact: Some(ArtifactId(format!("reference{payload}"))),
                    operation: None,
                    tool: None,
                    is_preview: false,
                    visible_text_bytes: None,
                    thinking: None,
                }],
                decisions: vec![],
                operations: vec![],
                error: Some(text.clone()),
            });
            let source_view = serde_json::to_value(&pane.view).unwrap();
            let buffer = rendered(&mut app);
            let screen = screen_text(&buffer);
            assert!(screen.contains(unicode), "{screen}");
            assert!(screen.contains("Kernel error"));
            assert!(screen.contains("Working"), "{screen}");
            assert!(buffer.content.iter().any(|cell| {
                cell.symbol() == "名"
                    && cell.bg == Palette::INPUT_BG()
                    && cell.modifier.contains(Modifier::BOLD)
            }));
            assert!(buffer
                .content
                .iter()
                .any(|cell| { cell.symbol() == "K" && cell.fg == Palette::ERROR() }));

            let artifact_id = ArtifactId(format!("artifact{payload}"));
            app.pane_mut().unwrap().inspector = Some(Inspector {
                target: InspectionTarget::Artifact(artifact_id.clone()),
                previous: Default::default(),
                title: format!("Artifact {artifact_id}"),
                text: text.clone(),
                offset: 0,
                next: None,
                total: text.len(),
                scroll: 0,
                pending: false,
            });
            app.focus = Focus::Inspector;
            let screen = screen_text(&rendered(&mut app));
            assert!(screen.contains("Artifact artifact"));
            assert!(screen.contains(unicode));
            let pane = app.pane().unwrap();
            assert_eq!(
                pane.inspector.as_ref().unwrap().title,
                format!("Artifact {artifact_id}")
            );
            assert_eq!(pane.inspector.as_ref().unwrap().text, text);
            assert_eq!(pane.draft.text, text);
            assert_eq!(pane.notice, text);
            assert_eq!(app.status, text);
            assert_eq!(app.threads[0].name, selected.name);
            assert_eq!(app.threads[0].state, selected.state);
            assert_eq!(app.selected.as_ref(), Some(&selected.id));
            assert_eq!(serde_json::to_value(&pane.view).unwrap(), source_view);
        }
    }

    #[test]
    fn initial_loading_frame_renders_an_editable_draft_without_a_provider() {
        let mut app = App::new(80);
        app.draft_mut().insert("draft 界é");
        app.draft_mut()
            .key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        app.draft_mut()
            .key(KeyEvent::new(KeyCode::Char('λ'), KeyModifiers::NONE));
        let buffer = rendered(&mut app);
        let screen = screen_text(&buffer);
        assert!(app.handle.is_none());
        assert!(app.selected.is_none());
        assert!(screen.contains("Initializing Copilot and kernel"));
        assert_eq!(buffer[(5, 37)].symbol(), "›");
        assert_eq!(buffer[(7, 37)].bg, Palette::INPUT_BG());
        assert_eq!(buffer[(7, 37)].modifier, Modifier::empty());
        assert!(screen.contains("draft 界λé"));
        assert_eq!(app.loading_draft.text, "draft 界λé");
        assert_eq!(app.loading_draft.cursor, "draft 界λ".len());
    }

    fn thread(name: &str) -> ThreadSummary {
        ThreadSummary {
            id: ThreadId(name.into()),
            session: SessionId("session".into()),
            parent_run: None,
            parent_thread: None,
            needs_approval: false,
            activity: Some(ThreadActivity::Model),
            name: name.into(),
            model: "test".into(),
            state: ThreadState::Active,
            run: Some(RunId(format!("run-{name}"))),
        }
    }

    #[test]
    fn commands_capture_selected_thread_and_run() {
        let child = thread("child");
        let Action::Command(Command::Submit { thread, text, .. }) =
            route("hello", &child, None).unwrap()
        else {
            panic!("submit expected")
        };
        assert_eq!(thread, child.id);
        assert_eq!(text, "hello");
        let Action::Command(Command::StopThread { thread, .. }) =
            route("/stop", &child, None).unwrap()
        else {
            panic!("stop expected")
        };
        assert_eq!(thread, child.id);
        assert!(route("/approve operation", &child, None).is_err());
        let Action::Command(Command::Decide {
            approve, revision, ..
        }) = route("/deny operation 7", &child, None).unwrap()
        else {
            panic!("decision expected")
        };
        assert!(!approve);
        assert_eq!(revision, 7);
    }

    #[test]
    fn yolo_preserves_plan_and_explicit_denials() {
        let selected = thread("selected");
        let policy = Policy {
            yolo: false,
            plan: true,
            denied_tools: ["bash".into()].into_iter().collect(),
        };
        let Action::Command(Command::SetPolicy {
            session,
            policy: next,
            ..
        }) = route(
            "/yolo",
            &selected,
            Some(&SessionPolicy {
                session: selected.session.clone(),
                policy: policy.clone(),
                revision: 3,
            }),
        )
        .unwrap()
        else {
            panic!("policy expected")
        };
        assert_eq!(session, selected.session);
        assert!(next.yolo);
        assert!(next.plan);
        assert_eq!(next.denied_tools, policy.denied_tools);
        assert!(route("/yolo", &selected, None).is_err());
    }

    #[test]
    fn delayed_receipt_never_clears_another_threads_draft() {
        let mut app = App::new(80);
        app.loading_draft.insert("loading draft");
        let first = thread("first");
        let second = thread("second");
        app.select(first.id.clone());
        app.draft_mut().insert("sent");
        app.select(second.id.clone());
        app.draft_mut().insert("second draft");
        let revision = app.panes[&first.id].draft.revision;
        app.panes.get_mut(&first.id).unwrap().pending = Some(1);
        app.reply(Reply::Command {
            focus_epoch: 0,
            revision,
            epoch: 1,
            inspection_epoch: 0,
            thread: first.id.clone(),
            submitted: "sent".into(),
            result: Ok(Outcome::Receipt(Receipt {
                session: first.session,
                thread: first.id.clone(),
                sequence: 1,
                detail: "accepted".into(),
            })),
        });
        assert_eq!(app.draft_mut().text, "second draft");
        assert!(app.panes[&first.id].draft.text.is_empty());
        assert_eq!(app.loading_draft.text, "loading draft");
    }

    #[test]
    fn editing_while_pending_preserves_the_new_draft() {
        let mut app = App::new(80);
        let selected = thread("selected");
        app.select(selected.id.clone());
        app.draft_mut().insert("new draft");
        app.pane_mut().unwrap().pending = Some(1);
        app.reply(Reply::Command {
            focus_epoch: 0,
            revision: 0,
            epoch: 1,
            inspection_epoch: 0,
            thread: selected.id.clone(),
            submitted: "old draft".into(),
            result: Ok(Outcome::Receipt(Receipt {
                session: selected.session,
                thread: selected.id,
                sequence: 1,
                detail: "accepted".into(),
            })),
        });
        assert_eq!(app.draft_mut().text, "new draft");
    }

    #[test]
    fn initialization_preserves_the_editable_draft_and_cursor() {
        let mut app = App::new(80);
        app.draft_mut().insert("draft while loading");
        app.draft_mut()
            .key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        let cursor = app.loading_draft.cursor;
        app.select_initial(ThreadId("root".into()));
        assert_eq!(app.draft_mut().text, "draft while loading");
        assert_eq!(app.draft_mut().cursor, cursor);
        assert!(app.loading_draft.text.is_empty());
    }

    #[test]
    fn stale_page_result_does_not_replace_current_page_state() {
        let mut app = App::new(80);
        let thread = ThreadId("root".into());
        app.select(thread.clone());
        app.pane_mut().unwrap().generation = 2;
        app.pane_mut().unwrap().notice = "current page".into();
        app.reply(Reply::View {
            thread,
            generation: 1,
            result: Err("stale failure".into()),
        });
        assert_eq!(app.pane().unwrap().notice, "current page");
    }

    #[test]
    fn unicode_editing_keeps_byte_boundaries() {
        let mut draft = Draft::default();
        draft.insert("a界é");
        draft.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        draft.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        draft.insert("λ");
        assert_eq!(draft.text, "aλé");
    }
}
