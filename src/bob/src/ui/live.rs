use super::{
    clipped,
    render::Row,
    state::{Anchor, AnchorId, App, HitAction, Pane},
    style::*,
};
use bob_kernel::{LiveAttempt, LiveSnapshot, ThreadId};
use bob_ui::segmented::SourceRow;
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use std::sync::Arc;
use tokio::sync::watch;

#[path = "live_segments.rs"]
mod live_segments;
use live_segments::{Document, Source};

pub(super) async fn changed(
    receiver: &mut Option<watch::Receiver<Arc<LiveSnapshot>>>,
) -> Option<Arc<LiveSnapshot>> {
    match receiver {
        Some(receiver) => {
            receiver.changed().await.ok()?;
            Some(receiver.borrow_and_update().clone())
        }
        None => std::future::pending().await,
    }
}

impl App {
    pub fn accept_live(&mut self, snapshot: Arc<LiveSnapshot>) {
        if self.live_snapshot.as_ref().is_some_and(|previous| {
            previous.instance == snapshot.instance && previous.revision >= snapshot.revision
        }) {
            return;
        }
        if self
            .live_instance
            .as_ref()
            .is_some_and(|instance| instance != &snapshot.instance)
        {
            self.runtime_epoch += 1;
            self.focus_epoch += 1;
            self.sequence_floor = 0;
            self.reading = false;
            self.threads.clear();
            self.sidebar_scroll = 0;
            self.activity = Default::default();
            self.hits.clear();
            self.inline_cache.borrow_mut().clear();
            for pane in self.panes.values_mut() {
                pane.live_rendered = None;
                pane.live_segments = Segments::default();
                pane.inline.reset();
                pane.generation += 1;
                pane.inspection_epoch += 1;
                pane.action_epoch += 1;
                pane.inspector = None;
                pane.inspector_rendered = None;
                pane.pending = None;
                pane.decided = None;
                pane.expected_permission = None;
                pane.decision_confirmed = false;
                pane.decision_followup = false;
                pane.decision_refresh = false;
                pane.receipt = None;
                pane.clear_notices();
                pane.loaded_before = None;
                pane.force_refresh = true;
                pane.before = None;
                pane.pinned_before = None;
                pane.newer.clear();
                pane.viewport = Default::default();
                pane.expanded.clear();
                pane.full_inputs.clear();
                pane.pending_inputs.clear();
                pane.input_errors.clear();
                pane.expanded_outputs.clear();
                pane.pending_outputs.clear();
                pane.output_errors.clear();
                pane.content_key = None;
                pane.rendered = None;
                pane.view = None;
                pane.entry_sequence = 0;
                pane.committed_operations.clear();
                pane.permission = None;
                pane.invalidate_metadata();
                pane.context = None;
                pane.controls = None;
                pane.session = None;
                pane.jobs.clear();
                pane.queue.clear();
                pane.submissions.clear();
                pane.submission_revision += 1;
                pane.queue_sequence = 0;
                pane.question = None;
                pane.plan = None;
                pane.draft.layout = None;
            }
            self.loading_draft.layout = None;
            self.dismiss_picker();
            self.completion.items.clear();
            if matches!(
                self.focus,
                super::state::Focus::Permission | super::state::Focus::PermissionInvalidated
            ) {
                self.focus = super::state::Focus::PermissionInvalidated;
            } else if matches!(
                self.focus,
                super::state::Focus::Question
                    | super::state::Focus::Plan
                    | super::state::Focus::ControlInvalidated
            ) {
                self.focus = super::state::Focus::ControlInvalidated;
            } else if self.focus == super::state::Focus::Inspector {
                self.focus = super::state::Focus::Composer;
            }
        }
        if self
            .live_snapshot
            .as_ref()
            .map_or(snapshot.error.is_some(), |previous| {
                previous.error != snapshot.error
            })
        {
            for pane in self.panes.values_mut() {
                pane.generation += 1;
                pane.loaded_before = None;
                pane.force_refresh = true;
            }
        }
        self.live_instance = Some(snapshot.instance.clone());
        self.live_snapshot = Some(snapshot);
    }

    pub fn disconnect_live(&mut self) {
        self.live_receiver = None;
        // Retain the snapshot (including halt errors) and instance until replacement.
        for pane in self.panes.values_mut() {
            pane.loaded_before = None;
        }
        if let Some(pane) = self.pane_mut() {
            pane.notify_current("Live updates disconnected; refreshing durable state.");
        }
    }
}

#[derive(PartialEq, Eq)]
struct Key {
    instance: String,
    attempts: Vec<(bob_kernel::OperationId, u64)>,
    width: u16,
    theme: u64,
    thinking: Vec<bob_kernel::OperationId>,
}

pub(super) struct RenderCache {
    key: Key,
    pub rows: Vec<Row>,
}

#[derive(Default)]
pub(super) struct Segments {
    documents: Vec<Document>,
    retained: Vec<Arc<LiveAttempt>>,
}
impl Segments {
    /// Whether the live area draws this operation, standing in for its saved reply.
    pub(super) fn shows(&self, operation: &bob_kernel::OperationId) -> bool {
        self.retained
            .iter()
            .any(|attempt| &attempt.operation == operation)
    }
    pub(super) fn pending(&self) -> bool {
        self.documents.iter().any(|document| {
            document.pending()
                || document.live_source().is_some_and(|source| {
                    self.retained.iter().any(|attempt| {
                        attempt.operation == source.attempt.operation
                            && attempt.revision != document.revision()
                    })
                })
        })
    }
}

fn attempt_visible(pane: &Pane, thread: &ThreadId, attempt: &LiveAttempt) -> bool {
    if pane.before.is_some() || &attempt.thread != thread {
        return false;
    }
    if let Some(entry) = pane.view.as_ref().and_then(|view| {
        view.entries.iter().find(|entry| {
            entry.kind == bob_kernel::EntryKind::Assistant
                && entry.operation.as_ref() == Some(&attempt.operation)
        })
    }) {
        return !super::inline::ready(pane, entry);
    }
    let sequence = pane.view.as_ref().map_or(0, |view| view.sequence);
    !pane.committed_operations.contains(&attempt.operation)
        && attempt
            .settled_sequence
            .is_none_or(|settled| sequence < settled)
}

/// Whether this operation's live text stands in for a saved reply whose full
/// text is still loading, so the transcript leaves that reply out.
fn stands_in(pane: &Pane, operation: &bob_kernel::OperationId) -> bool {
    pane.view.as_ref().is_some_and(|view| {
        view.entries.iter().any(|entry| {
            entry.kind == bob_kernel::EntryKind::Assistant
                && entry.operation.as_ref() == Some(operation)
                && !super::inline::ready(pane, entry)
        })
    })
}

/// The transcript leaves out saved replies whose live text stands in for them,
/// so it must be rebuilt when that set changes.
fn stand_ins_changed(pane: &Pane, attempts: &[Arc<LiveAttempt>]) -> bool {
    let retained = &pane.live_segments.retained;
    let dropped = |from: &[Arc<LiveAttempt>], to: &[Arc<LiveAttempt>]| {
        from.iter().any(|attempt| {
            !to.iter().any(|other| other.operation == attempt.operation)
                && stands_in(pane, &attempt.operation)
        })
    };
    dropped(retained, attempts) || dropped(attempts, retained)
}

pub(super) fn rows(
    pane: &mut Pane,
    snapshot: Option<&LiveSnapshot>,
    thread: &ThreadId,
    width: u16,
) {
    let Some(snapshot) = snapshot else {
        if stand_ins_changed(pane, &[]) {
            pane.inline.invalidate();
        }
        pane.live_rendered = None;
        pane.live_segments = Segments::default();
        return;
    };
    if pane
        .live_rendered
        .as_ref()
        .is_some_and(|cache| cache.key.instance != snapshot.instance)
    {
        pane.live_segments = Segments::default();
    }
    let mut attempts: Vec<_> = snapshot
        .attempts
        .iter()
        .filter(|attempt| attempt_visible(pane, thread, attempt))
        .cloned()
        .collect();
    for attempt in &pane.live_segments.retained {
        if !attempts
            .iter()
            .any(|current| current.operation == attempt.operation)
            && !snapshot
                .attempts
                .iter()
                .any(|current| current.operation == attempt.operation)
            && attempt_visible(pane, thread, attempt)
        {
            attempts.push(attempt.clone());
        }
    }
    let key = Key {
        instance: snapshot.instance.clone(),
        attempts: attempts
            .iter()
            .map(|attempt| (attempt.operation.clone(), attempt.revision))
            .collect(),
        width,
        theme: bob_ui::theme::generation(),
        thinking: attempts
            .iter()
            .filter(|attempt| pane.expanded_live_thinking.contains(&attempt.operation))
            .map(|attempt| attempt.operation.clone())
            .collect(),
    };
    if pane
        .live_rendered
        .as_ref()
        .is_some_and(|cache| cache.key == key)
        && !pane.live_segments.pending()
    {
        return;
    }
    if pane
        .live_rendered
        .as_ref()
        .is_some_and(|cache| cache.key.theme != key.theme)
    {
        pane.live_segments.documents.clear();
    }
    if stand_ins_changed(pane, &attempts) {
        pane.inline.invalidate();
    }
    pane.live_segments.retained = attempts.clone();
    let mut sources = Vec::new();
    for attempt in attempts {
        if attempt
            .reasoning
            .iter()
            .any(|block| !block.text.trim().is_empty())
        {
            sources.push(Source {
                attempt: attempt.clone(),
                reasoning: Some(0),
            });
        }
        if !attempt.text.is_empty() || attempt.truncated || attempt.error.is_some() {
            sources.push(Source {
                attempt,
                reasoning: None,
            });
        }
    }
    pane.live_segments.documents.retain(|document| {
        document.live_source().is_some_and(|cached| {
            sources
                .iter()
                .any(|source| source.attempt.operation == cached.attempt.operation)
        })
    });
    let body_width = width.saturating_sub(BAND_INSET * 2).max(1) as usize;
    let mut budget = live_segments::STEP_BYTES;
    let mut rows = Vec::new();
    for source in &sources {
        if source.reasoning.is_some() {
            let expanded = pane
                .expanded_live_thinking
                .contains(&source.attempt.operation);
            thinking_rows(&mut rows, source, &snapshot.instance, expanded, width);
            continue;
        }
        let index = pane
            .live_segments
            .documents
            .iter()
            .position(|document| {
                document
                    .live_source()
                    .is_some_and(|cached| cached.attempt.operation == source.attempt.operation)
            })
            .unwrap_or_else(|| {
                pane.live_segments
                    .documents
                    .push(Document::new(source.clone(), body_width));
                pane.live_segments.documents.len() - 1
            });
        let document = &mut pane.live_segments.documents[index];
        document.update(source.clone(), body_width);
        document.step(&mut budget);
        let pending = document.pending();
        let prepared = document.rows();
        let identity = identity(source, &snapshot.instance);
        for row in &prepared {
            rows.push(Row {
                anchor: Anchor {
                    identity: identity.clone(),
                    line: row.ordinal,
                    offset: row.source.start,
                },
                line: row.line.clone(),
                band: false,
                hit: HitAction::Live,
            });
        }
        if pending {
            rows.push(Row {
                anchor: Anchor {
                    identity: identity.clone(),
                    line: usize::MAX - 2,
                    offset: source.text().len(),
                },
                line: Line::styled("Preparing live text…", text().fg(Palette::DIM())),
                band: false,
                hit: HitAction::Live,
            });
        }
        if source.attempt.truncated {
            rows.push(Row {
                anchor: Anchor {
                    identity: identity.clone(),
                    line: usize::MAX - 3,
                    offset: source.text().len(),
                },
                line: Line::styled(
                    "… live preview limited; final response will replace it",
                    text().fg(Palette::DIM()),
                ),
                band: false,
                hit: HitAction::Live,
            });
        }
        if let Some(error) = &source.attempt.error {
            rows.push(Row {
                anchor: Anchor {
                    identity: identity.clone(),
                    line: usize::MAX - 4,
                    offset: source.text().len(),
                },
                line: Line::styled(
                    format!("Stream interrupted: {}", safe(&clipped(error, 1024))),
                    text().fg(Palette::WARN()),
                ),
                band: false,
                hit: HitAction::Live,
            });
        }
        rows.push(Row {
            anchor: Anchor {
                identity,
                line: usize::MAX - 5,
                offset: source.text().len(),
            },
            line: Line::default(),
            band: false,
            hit: HitAction::Live,
        });
    }
    pane.live_rendered = Some(RenderCache { key, rows });
}

/// A saved reply laid out in full.
pub(super) fn complete_rows(entry: &bob_kernel::Entry, width: u16) -> Vec<Row> {
    body_rows(entry.sequence, complete_layout(&entry.text, width))
}

pub(super) fn complete_layout(text: &str, width: u16) -> Vec<SourceRow> {
    let mut document =
        Document::complete(text, width.saturating_sub(BAND_INSET * 2).max(1) as usize);
    while document.pending() {
        let mut budget = live_segments::STEP_BYTES;
        document.step(&mut budget);
    }
    document.rows()
}

pub(super) fn body_rows(sequence: i64, rows: impl IntoIterator<Item = SourceRow>) -> Vec<Row> {
    rows.into_iter()
        .map(|row| Row {
            anchor: Anchor {
                identity: AnchorId::Body(sequence),
                line: row.ordinal,
                offset: row.source.start,
            },
            line: row.line,
            band: false,
            hit: HitAction::Entry(sequence),
        })
        .collect()
}

fn thinking_rows(rows: &mut Vec<Row>, source: &Source, instance: &str, expanded: bool, width: u16) {
    let identity = identity(source, instance);
    let hit = HitAction::LiveThinking(source.attempt.operation.clone());
    let dim = text().fg(Palette::DIM());
    rows.push(Row {
        anchor: Anchor {
            identity: identity.clone(),
            line: usize::MAX - 1,
            offset: 0,
        },
        line: Line::from(Span::styled("Thinking", dim.add_modifier(Modifier::BOLD))),
        band: false,
        hit: hit.clone(),
    });
    let mut end = 0;
    if expanded {
        let reasoning = source
            .attempt
            .reasoning
            .iter()
            .map(|block| block.text.trim())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        let italic = dim.add_modifier(Modifier::ITALIC);
        let body_width = width.saturating_sub(BAND_INSET * 2 + 4).max(1) as usize;
        for line in safe(&reasoning).lines() {
            for (offset, mut content) in
                prose_wrap(&Line::styled(line.to_owned(), italic), body_width)
            {
                content.spans.insert(0, Span::raw("  "));
                rows.push(Row {
                    anchor: Anchor {
                        identity: identity.clone(),
                        line: 0,
                        offset: end + offset,
                    },
                    line: content,
                    band: false,
                    hit: hit.clone(),
                });
            }
            end += line.len() + 1;
        }
    }
    rows.push(Row {
        anchor: Anchor {
            identity,
            line: usize::MAX - 5,
            offset: end,
        },
        line: Line::default(),
        band: false,
        hit,
    });
}

fn identity(source: &Source, instance: &str) -> AnchorId {
    if source.reasoning.is_some() {
        AnchorId::Reasoning {
            instance: instance.to_owned(),
            operation: source.attempt.operation.clone(),
        }
    } else {
        AnchorId::Live {
            instance: instance.to_owned(),
            operation: source.attempt.operation.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(text: String) -> LiveSnapshot {
        LiveSnapshot {
            instance: "runtime".into(),
            revision: 1,
            sequence: 0,
            error: None,
            attempts: vec![Arc::new(LiveAttempt {
                thread: ThreadId("root".into()),
                run: bob_kernel::RunId("run".into()),
                operation: bob_kernel::OperationId("operation".into()),
                revision: 1,
                activity: bob_kernel::ModelActivity::Answering,
                text,
                reasoning: vec![bob_kernel::ReasoningPreview {
                    block_id: "summary".into(),
                    kind: bob_types::ReasoningKind::Summary,
                    text: "Local provider summary".into(),
                }],
                truncated: false,
                started_at: std::time::Instant::now(),
                compacting: false,
                stopping: false,
                settled_sequence: None,
                error: None,
            })],
        }
    }

    fn finish(pane: &mut Pane, snapshot: &LiveSnapshot, width: u16) {
        for _ in 0..20000 {
            rows(pane, Some(snapshot), &ThreadId("root".into()), width);
            if !pane.live_segments.pending() {
                return;
            }
        }
        panic!("live pane did not settle");
    }

    fn rendered(pane: &Pane) -> Vec<String> {
        pane.live_rendered
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .map(|row| row.line.to_string())
            .collect()
    }

    fn entry(text: String) -> bob_kernel::Entry {
        bob_kernel::Entry {
            sequence: 2,
            sender: None,
            parent_guidance: false,
            submission: None,
            thread: ThreadId("root".into()),
            kind: bob_kernel::EntryKind::Assistant,
            text,
            artifact: None,
            operation: Some(bob_kernel::OperationId("operation".into())),
            tool: None,
            is_preview: false,
            visible_text_bytes: None,
            thinking: None,
        }
    }

    fn view(entries: Vec<bob_kernel::Entry>) -> bob_kernel::View {
        bob_kernel::View {
            sequence: 2,
            entry_sequence: entries.last().map_or(0, |entry| entry.sequence),
            threads: Vec::new(),
            policies: Vec::new(),
            entries,
            decisions: Vec::new(),
            operations: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn short_durable_handoff_preserves_later_paragraph_visible_text() {
        let input: String = (0..20).map(|index| format!("Paragraph {index:02} preserves this later reading position with **emphasis** and more prose.\n\n")).collect();
        let snapshot = snapshot(input.clone());
        let mut app = App::new(80);
        app.select(ThreadId("root".into()));
        app.pane_mut().unwrap().view = Some(view(Vec::new()));
        app.accept_live(Arc::new(snapshot.clone()));
        finish(app.pane_mut().unwrap(), &snapshot, 40);
        let anchor = app
            .pane()
            .unwrap()
            .live_rendered
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .find(|row| {
                matches!(row.anchor.identity, AnchorId::Live { .. })
                    && row.line.to_string().contains("Paragraph 12")
            })
            .unwrap()
            .anchor
            .clone();
        assert!(anchor.offset > 0);
        app.pane_mut().unwrap().viewport.anchor = Some(anchor.clone());
        app.reply(super::super::Reply::View {
            thread: ThreadId("root".into()),
            generation: 0,
            result: Ok(view(vec![entry(format!(
                "{input}Authoritative final suffix."
            ))])),
        });
        let pane = app.pane_mut().unwrap();
        assert_eq!(
            pane.viewport.anchor.as_ref().unwrap().identity,
            AnchorId::Body(2)
        );
        assert_eq!(pane.viewport.anchor.as_ref().unwrap().offset, anchor.offset);
        let rendered = super::super::render::transcript(pane, true, 40);
        pane.viewport.rows = rendered.iter().map(|row| row.anchor.clone()).collect();
        let top = pane.viewport.position(6);
        let visible = rendered
            .iter()
            .skip(top)
            .take(6)
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            visible.contains("Paragraph 12"),
            "handoff moved the reading position: {visible}"
        );
        assert_eq!(pane.inline.cache.borrow().counters.requests, 0);
    }

    #[test]
    fn short_saved_replies_render_every_line_without_paging() {
        let entry = entry("x".repeat(3500));
        let rows = complete_rows(&entry, BAND_INSET * 2 + 1);
        let text: String = rows.iter().map(|row| row.line.to_string()).collect();
        assert_eq!(text, entry.text);
        assert!(rows
            .iter()
            .all(|row| matches!(row.anchor.identity, AnchorId::Body(_))));
    }

    #[test]
    fn unchanged_live_render_reuses_rows_and_builds_nothing() {
        let snapshot = snapshot("Visible **answer**".into());
        let mut pane = Pane::default();
        finish(&mut pane, &snapshot, 80);
        let builds: usize = pane
            .live_segments
            .documents
            .iter()
            .map(|document| document.counters.builds)
            .sum();
        let pointer = pane.live_rendered.as_ref().unwrap().rows.as_ptr();
        for _ in 0..100 {
            rows(&mut pane, Some(&snapshot), &ThreadId("root".into()), 80);
        }
        assert_eq!(pane.live_rendered.as_ref().unwrap().rows.as_ptr(), pointer);
        assert_eq!(
            pane.live_segments
                .documents
                .iter()
                .map(|document| document.counters.builds)
                .sum::<usize>(),
            builds
        );
    }

    #[test]
    fn long_live_answer_renders_in_full_without_paging_labels() {
        let body: String = (0..2000)
            .map(|index| format!("Line {index:04} with **styled** text.\n\n"))
            .collect();
        let snapshot = snapshot(body);
        let mut pane = Pane::default();
        finish(&mut pane, &snapshot, 60);
        let text = rendered(&pane);
        assert!(text.iter().any(|line| line.contains("Line 0000")));
        assert!(text.iter().any(|line| line.contains("Line 1999")));
        assert!(!text
            .iter()
            .any(|line| line.contains("Earlier") || line.contains("Later")));
    }

    #[test]
    fn streaming_long_thinking_after_many_steps_shows_no_earlier_label() {
        let mut pane = Pane::default();
        let attempts = (0..5)
            .map(|step| {
                let mut attempt = (*snapshot(format!("Answer {step}")).attempts[0]).clone();
                attempt.operation = bob_kernel::OperationId(format!("step-{step}"));
                attempt.reasoning[0].text = "Reasoning ".repeat(4000);
                if step == 4 {
                    attempt.text.clear();
                }
                Arc::new(attempt)
            })
            .collect();
        let snapshot = LiveSnapshot {
            attempts,
            ..snapshot(String::new())
        };
        finish(&mut pane, &snapshot, 80);
        let text = rendered(&pane);
        assert_eq!(
            text.iter().filter(|line| line.trim() == "Thinking").count(),
            5
        );
        assert!(text.iter().any(|line| line.contains("Answer 0")));
        assert_eq!(text.last().map(String::as_str), Some(""));
        assert!(!text
            .iter()
            .any(|line| line.contains("Earlier") || line.contains("Later")));
    }

    #[test]
    fn retired_live_text_survives_snapshot_removal_but_not_runtime_or_thread_change() {
        let mut snapshot = snapshot("Retired live answer".into());
        let mut pane = Pane::default();
        finish(&mut pane, &snapshot, 80);
        snapshot.attempts.clear();
        snapshot.revision += 1;
        finish(&mut pane, &snapshot, 80);
        assert!(pane
            .live_rendered
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .any(|row| row.line.to_string().contains("Retired live answer")));
        rows(&mut pane, Some(&snapshot), &ThreadId("child".into()), 80);
        assert!(pane.live_rendered.as_ref().unwrap().rows.is_empty());
        assert!(pane.live_segments.documents.is_empty());
        snapshot = self::snapshot("Other runtime answer".into());
        finish(&mut pane, &snapshot, 80);
        snapshot.instance = "replacement".into();
        snapshot.attempts.clear();
        rows(&mut pane, Some(&snapshot), &ThreadId("root".into()), 80);
        assert!(pane.live_rendered.as_ref().unwrap().rows.is_empty());
        assert!(pane.live_segments.documents.is_empty());
    }
}
