use super::*;
use bob_kernel::{Executor, ModelInfo, ModelProgress, ModelUpdate, Runtime};
use bob_types::{Completion, ContentBlock, Message, Role, StopReason, Usage};
use ratatui::backend::TestBackend;
use std::sync::Arc;

struct ReplyFixture {
    text: String,
    stream_same_text: bool,
    finish: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl Executor for ReplyFixture {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "inline-fixture".into(),
            context_window: 100_000,
        }]
    }
    fn tools(&self) -> Vec<bob_kernel::ToolDescriptor> {
        Vec::new()
    }
    async fn generate(
        &self,
        _request: bob_kernel::RequestSpec,
        mut cancel: tokio::sync::watch::Receiver<bool>,
        progress: ModelProgress,
    ) -> Result<Completion> {
        let streamed = if self.stream_same_text {
            self.text.clone()
        } else {
            format!("{}STREAM_ONLY_MARKER", "streamed text\n".repeat(200))
        };
        progress.emit(ModelUpdate::Text(streamed)).await?;
        tokio::select! {
            _ = self.finish.notified() => {},
            _ = bob_kernel::cancelled(&mut cancel) => bail!("fixture cancelled"),
        }
        Ok(Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: self.text.clone(),
                }],
            },
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }
    async fn prepare(
        &self,
        _invocation: bob_kernel::Invocation,
    ) -> Result<bob_kernel::PreparedTool> {
        bail!("no tools")
    }
    async fn execute(
        &self,
        _tool: bob_kernel::PreparedTool,
        _cancel: tokio::sync::watch::Receiver<bool>,
    ) -> Result<bob_kernel::ToolOutcome> {
        bail!("no tools")
    }
}

fn frame(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .chunks(width as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn hydrate(app: &mut App) -> String {
    let mut jobs = JoinSet::new();
    for _ in 0..1024 {
        let screen = frame(app, 100, 30);
        app.refresh_inline(&mut jobs);
        if jobs.is_empty() {
            if app
                .pane()
                .is_some_and(|pane| pane.rendered.is_none() || pane.live_segments.pending())
            {
                continue;
            }
            return screen;
        }
        while let Some(reply) = jobs.join_next().await {
            app.reply(reply.unwrap());
            app.refresh_inline(&mut jobs);
        }
    }
    panic!("bounded body hydration did not converge")
}

async fn fixture(text: String) -> (std::path::PathBuf, Arc<ReplyFixture>, App, ThreadId) {
    fixture_with_stream(text, false).await
}

async fn fixture_with_stream(
    text: String,
    stream_same_text: bool,
) -> (std::path::PathBuf, Arc<ReplyFixture>, App, ThreadId) {
    let database = std::env::temp_dir().join(format!("bob-inline-{}.sqlite", uuid::Uuid::new_v4()));
    let executor = Arc::new(ReplyFixture {
        text,
        stream_same_text,
        finish: tokio::sync::Notify::new(),
    });
    let handle = Runtime::open(&database, executor.clone()).await.unwrap();
    let receipt = handle
        .command(Command::CreateSession {
            id: CommandId::new(),
            workspace: std::env::temp_dir().to_string_lossy().into_owned(),
            profile: Profile {
                model: "inline-fixture".into(),
                instructions: "test".into(),
                read_only: true,
                max_requests: 2,
                reasoning: ReasoningEffort::Off,
            },
            policy: Policy::default(),
        })
        .await
        .unwrap();
    handle
        .command(Command::Submit {
            id: CommandId::new(),
            thread: receipt.thread.clone(),
            text: "produce retained response".into(),
        })
        .await
        .unwrap();
    let view = handle
        .transcript(Some(receipt.thread.clone()), None, 100)
        .await
        .unwrap();
    let mut app = App::new(100);
    app.reply(Reply::Ready(Ok((handle, view, receipt.thread.clone()))));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !app.live_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot
                .attempts
                .iter()
                .any(|attempt| attempt.text.contains("STREAM_ONLY_MARKER"))
        }) {
            let snapshot = live::changed(&mut app.live_receiver).await.unwrap();
            app.accept_live(snapshot);
        }
    })
    .await
    .unwrap();
    (database, executor, app, receipt.thread)
}

async fn settle(app: &mut App, executor: &ReplyFixture, thread: &ThreadId) {
    executor.finish.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = live::changed(&mut app.live_receiver).await.unwrap();
            let settled = snapshot
                .attempts
                .iter()
                .any(|attempt| attempt.settled_sequence.is_some());
            app.accept_live(snapshot);
            if settled {
                break;
            }
        }
    })
    .await
    .unwrap();
    let view = app
        .handle
        .as_ref()
        .unwrap()
        .transcript(Some(thread.clone()), None, 100)
        .await
        .unwrap();
    let generation = app.pane().unwrap().generation;
    app.reply(Reply::View {
        thread: thread.clone(),
        generation,
        result: Ok(view),
    });
}

#[tokio::test]
async fn retained_reply_tail_is_inline_after_settlement_and_reopen() {
    let text = format!(
        "BEGIN_RETAINED\n{}\nEND_RETAINED",
        "complete retained answer 界 e\u{301}\n".repeat(2300)
    );
    let (database, executor, mut app, thread) = fixture(text.clone()).await;
    assert!(hydrate(&mut app).await.contains("STREAM_ONLY_MARKER"));
    settle(&mut app, &executor, &thread).await;
    let entry = app
        .pane()
        .unwrap()
        .view
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .find(|entry| entry.kind == EntryKind::Assistant)
        .unwrap();
    assert!(entry.is_preview);
    assert!(entry.text.chars().count() <= 2048);
    assert!(!entry.text.contains("END_RETAINED"));
    let sequence = entry.sequence;
    let screen = hydrate(&mut app).await;
    assert!(screen.contains("END_RETAINED"), "{screen}");
    assert!(!screen.contains("STREAM_ONLY_MARKER"));
    assert!(!screen.contains("click to inspect retained content"));
    let requests = app.inline_cache.borrow().counters.requests;
    let layouts = app.inline_cache.borrow().counters.layouts;
    for _ in 0..10 {
        frame(&mut app, 100, 30);
    }
    assert_eq!(app.inline_cache.borrow().counters.requests, requests);
    assert_eq!(app.inline_cache.borrow().counters.layouts, layouts);
    assert!(app.inline_cache.borrow().counters.max_flights <= 4);
    assert!(app.inline_cache.borrow().counters.raw_bytes <= 8 * 1024 * 1024);
    assert!(app.inline_cache.borrow().counters.layout_bytes <= 16 * 1024 * 1024);
    app.pane_mut().unwrap().inline.seek(sequence, 0, 0, false);
    app.pane_mut().unwrap().viewport.anchor = Some(Anchor {
        identity: AnchorId::Body(sequence),
        line: 0,
        offset: 0,
    });
    app.pane_mut().unwrap().rendered = None;
    assert!(hydrate(&mut app).await.contains("BEGIN_RETAINED"));
    let anchor = app.pane().unwrap().viewport.anchor.clone();
    frame(&mut app, 70, 24);
    assert_eq!(app.pane().unwrap().viewport.anchor, anchor);
    app.handle.as_ref().unwrap().shutdown().await.unwrap();
    drop(app);
    let reopened = Runtime::open_paused(&database, executor).await.unwrap();
    let view = reopened
        .transcript(Some(thread.clone()), None, 100)
        .await
        .unwrap();
    let mut restored = App::new(100);
    restored.reply(Reply::Ready(Ok((reopened.clone(), view, thread))));
    assert!(hydrate(&mut restored).await.contains("END_RETAINED"));
    reopened.shutdown().await.unwrap();
    drop(restored);
    std::fs::remove_file(database).unwrap();
}

#[tokio::test]
async fn later_paragraph_reading_position_survives_live_settlement_and_resize() {
    let body = format!("{}STREAM_ONLY_MARKER", (0..200).map(|i| {
        format!("**PARA{i:03}** {}\nREAD{i:03} middle reading line\nend of paragraph {i:03}\n\n", "word ".repeat(12))
    }).collect::<String>());
    assert!(body.len() > inline::PAGE_BYTES);
    let (database, executor, mut app, thread) = fixture_with_stream(body, true).await;
    for _ in 0..1024 {
        if frame(&mut app, 100, 30).contains("STREAM_ONLY_MARKER") {
            break;
        }
    }
    let anchor = app
        .pane()
        .unwrap()
        .live_rendered
        .as_ref()
        .unwrap()
        .rows
        .iter()
        .find(|row| row.line.to_string().contains("READ190"))
        .expect("later paragraph should be retained in the bounded live tail")
        .anchor
        .clone();
    app.pane_mut().unwrap().viewport.anchor = Some(anchor);
    let mut before = String::new();
    for _ in 0..1024 {
        before = frame(&mut app, 100, 30);
        if before.contains("READ190") && !before.contains("STREAM_ONLY_MARKER") {
            break;
        }
    }
    assert!(before.contains("READ190"), "{before}");
    assert!(
        !before.contains("PARA190"),
        "anchor must show the middle, not the paragraph start: {before}"
    );
    settle(&mut app, &executor, &thread).await;
    let after = hydrate(&mut app).await;
    assert!(after.contains("READ190"), "{after}");
    assert!(
        !after.contains("PARA190"),
        "settlement moved to the paragraph start: {after}"
    );
    assert!(
        !after.contains("STREAM_ONLY_MARKER"),
        "settlement moved to EOF: {after}"
    );
    let resized = frame(&mut app, 62, 30);
    assert!(resized.contains("READ190"), "{resized}");
    assert!(
        !resized.contains("PARA190"),
        "resize lost the reading position: {resized}"
    );
    app.handle.as_ref().unwrap().shutdown().await.unwrap();
    drop(app);
    std::fs::remove_file(database).unwrap();
}

#[tokio::test]
async fn pane_switch_during_indexing_preserves_loading_successor() {
    let body = format!("{}SWITCHED_PANE_TAIL", "retained line\n".repeat(8000));
    let (database, executor, mut app, thread) = fixture(body).await;
    settle(&mut app, &executor, &thread).await;
    assert!(frame(&mut app, 100, 30).contains("Loading reply"));
    let mut jobs = JoinSet::new();
    app.refresh_inline(&mut jobs);
    app.reply(jobs.join_next().await.unwrap().unwrap());
    assert!(
        app.pane().unwrap().rendered.is_none(),
        "intermediate page left a cached Loading row"
    );
    assert!(frame(&mut app, 100, 30).contains("Loading reply"));
    let layouts = app.inline_cache.borrow().counters.layouts;
    app.select(ThreadId("other-pane".into()));
    app.refresh_inline(&mut jobs);
    assert!(jobs.is_empty());
    app.select(thread.clone());
    app.refresh_inline(&mut jobs);
    assert!(
        !jobs.is_empty(),
        "switching panes discarded the queued successor"
    );
    while let Some(reply) = jobs.join_next().await {
        app.reply(reply.unwrap());
        app.refresh_inline(&mut jobs);
    }
    let screen = hydrate(&mut app).await;
    assert!(screen.contains("SWITCHED_PANE_TAIL"), "{screen}");
    assert!(app.inline_cache.borrow().counters.layouts > layouts);
    app.handle.as_ref().unwrap().shutdown().await.unwrap();
    drop(app);
    std::fs::remove_file(database).unwrap();
}

#[tokio::test]
async fn complete_multibyte_reply_uses_retained_text_without_fetching_pages() {
    let text = format!("{}\nMULTIBYTE_COMPLETE_TAIL", "界".repeat(1800));
    assert!(text.len() > 4096);
    assert!(text.chars().count() < 2048);
    let (database, executor, mut app, thread) = fixture(text.clone()).await;
    settle(&mut app, &executor, &thread).await;
    let entry = app
        .pane()
        .unwrap()
        .view
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .find(|entry| entry.kind == EntryKind::Assistant)
        .unwrap();
    assert!(!entry.is_preview);
    assert_eq!(entry.text, text);
    let screen = hydrate(&mut app).await;
    assert!(screen.contains("MULTIBYTE_COMPLETE_TAIL"), "{screen}");
    assert_eq!(app.inline_cache.borrow().counters.requests, 0);
    assert!(app.pane().unwrap().inspector.is_none());
    app.handle.as_ref().unwrap().shutdown().await.unwrap();
    drop(app);
    std::fs::remove_file(database).unwrap();
}

#[tokio::test]
async fn inline_preview_boundaries_use_real_retained_projection() {
    for length in [2047usize, 2048, 2049, 16 * 1024, 64 * 1024, 256 * 1024] {
        let suffix = "INLINE_BOUNDARY_TAIL";
        let text = format!("{}{}", "x".repeat(length - suffix.len()), suffix);
        let (database, executor, mut app, thread) = fixture(text).await;
        settle(&mut app, &executor, &thread).await;
        let screen = hydrate(&mut app).await;
        let visible: String = screen.lines().map(str::trim).collect();
        assert!(visible.contains(suffix), "length={length}: {screen}");
        assert!(app.pane().unwrap().inspector.is_none());
        app.handle.as_ref().unwrap().shutdown().await.unwrap();
        drop(app);
        std::fs::remove_file(database).unwrap();
    }
}

#[tokio::test]
async fn ordinary_scroll_traverses_every_retained_marker_without_duplicate_source_rows() {
    let body = format!(
        "```text\n{}\n```",
        (0..2200)
            .map(|index| format!("retained[{index:04}] value\n"))
            .collect::<String>()
    );
    let (database, executor, mut app, thread) = fixture(body).await;
    settle(&mut app, &executor, &thread).await;
    hydrate(&mut app).await;
    for _ in 0..400 {
        app.pane_mut().unwrap().scroll(-100);
        hydrate(&mut app).await;
        let pane = app.pane().unwrap();
        if pane.viewport.top == 0
            && pane
                .viewport
                .rows
                .first()
                .is_none_or(|anchor| !matches!(anchor.identity, AnchorId::BodyGap { .. }))
        {
            break;
        }
    }
    let mut seen = std::collections::BTreeMap::new();
    let mut reached_tail = false;
    for _ in 0..3000 {
        hydrate(&mut app).await;
        let pane = app.pane().unwrap();
        if let Some(cache) = &pane.rendered {
            for row in cache
                .rows
                .iter()
                .skip(pane.viewport.top)
                .take(pane.viewport.height)
            {
                if matches!(row.anchor.identity, AnchorId::Body(_)) {
                    let key = (row.anchor.offset, row.anchor.line);
                    let value = row.line.to_string();
                    if let Some(previous) = seen.insert(key, value.clone()) {
                        assert_eq!(previous, value, "source row changed while scrolling");
                    }
                }
            }
        }
        if pane.viewport.anchor.is_none() {
            reached_tail = true;
            break;
        }
        app.pane_mut().unwrap().scroll(3);
    }
    assert!(reached_tail, "scrolling did not reach actual retained EOF");
    let complete = seen.into_values().collect::<String>();
    for index in 0..2200 {
        let marker = format!("retained[{index:04}]");
        assert_eq!(
            complete.matches(&marker).count(),
            1,
            "missing or duplicated {marker}"
        );
    }
    app.handle.as_ref().unwrap().shutdown().await.unwrap();
    drop(app);
    std::fs::remove_file(database).unwrap();
}

#[test]
fn body_source_and_markdown_ordinal_anchor_survives_relayout() {
    let mut viewport = Viewport::default();
    viewport.anchor = Some(Anchor {
        identity: AnchorId::Body(7),
        offset: 100,
        line: 4,
    });
    viewport.rows = (0..20)
        .map(|line| Anchor {
            identity: AnchorId::Body(7),
            offset: 100,
            line,
        })
        .collect();
    assert_eq!(viewport.position(5), 4);
    viewport.rows.insert(
        0,
        Anchor {
            identity: AnchorId::Body(7),
            offset: 0,
            line: 0,
        },
    );
    assert_eq!(viewport.position(5), 5);
    assert_eq!(viewport.anchor.as_ref().unwrap().line, 4);
}
