use super::{
    clipped,
    render::Row,
    state::{Anchor, AnchorId, App, HitAction, Pane},
    style::*,
};
use bob_kernel::{LiveAttempt, LiveSnapshot, ThreadId};
use ratatui::text::Line;
use std::sync::Arc;
use tokio::sync::watch;

#[path = "live_segments.rs"]
mod live_segments;
use live_segments::{Document, Source, Target};

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
    focus: Option<Focus>,
}

pub(super) struct RenderCache {
    key: Key,
    pub rows: Vec<Row>,
}

#[derive(Clone, PartialEq, Eq)]
struct SourceKey {
    operation: bob_kernel::OperationId,
    reasoning: Option<usize>,
}
impl SourceKey {
    fn of(source: &Source) -> Self {
        Self {
            operation: source.attempt.operation.clone(),
            reasoning: source.reasoning,
        }
    }
}
#[derive(Clone, PartialEq, Eq)]
struct Focus {
    source: SourceKey,
    target: Target,
}
struct Gap {
    anchor: Anchor,
    focus: Focus,
    backward: bool,
}
#[derive(Default)]
pub(super) struct Segments {
    documents: Vec<Document>,
    retained: Vec<Arc<LiveAttempt>>,
    focus: Option<Focus>,
    gaps: Vec<Gap>,
}
impl Segments {
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

pub(super) fn rows(
    pane: &mut Pane,
    snapshot: Option<&LiveSnapshot>,
    thread: &ThreadId,
    width: u16,
) {
    let Some(snapshot) = snapshot else {
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
    if pane.viewport.anchor.is_none() {
        pane.live_segments.focus = None;
    } else if pane.live_segments.focus.is_none()
        || pane
            .live_rendered
            .as_ref()
            .is_some_and(|cache| cache.key.width != width)
    {
        if let Some(anchor) = pane
            .viewport
            .anchor
            .as_ref()
            .filter(|anchor| anchor.line < usize::MAX - 5)
        {
            if let Some(source) = pane
                .live_segments
                .documents
                .iter()
                .filter_map(Document::live_source)
                .find(|source| {
                    identity(source, &snapshot.instance) == anchor.identity
                        && anchor.offset >= source_base(source)
                        && anchor.offset <= source_base(source) + source.text().len()
                })
            {
                pane.live_segments.focus = Some(Focus {
                    source: SourceKey::of(source),
                    target: Target::Forward(anchor.offset - source_base(source), anchor.line),
                });
            }
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
        focus: pane.live_segments.focus.clone(),
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
    pane.live_segments.retained = attempts.clone();
    let mut sources = Vec::new();
    for attempt in attempts {
        for (index, reasoning) in attempt.reasoning.iter().enumerate() {
            if !reasoning.text.is_empty() {
                sources.push(Source {
                    attempt: attempt.clone(),
                    reasoning: Some(index),
                });
            }
        }
        if !attempt.text.is_empty() || attempt.truncated || attempt.error.is_some() {
            sources.push(Source {
                attempt,
                reasoning: None,
            });
        }
    }
    let focus = pane.live_segments.focus.clone();
    let center = focus.as_ref().and_then(|focus| {
        sources
            .iter()
            .position(|source| SourceKey::of(source) == focus.source)
    });
    let start = center.map_or(sources.len().saturating_sub(2), |center| {
        if matches!(focus.as_ref().unwrap().target, Target::Backward(..)) {
            center.saturating_sub(1)
        } else {
            center
        }
    });
    let end = (start + 2).min(sources.len());
    let selected = &sources[start..end];
    pane.live_segments.documents.retain(|document| {
        document.live_source().is_some_and(|cached| {
            selected
                .iter()
                .any(|source| SourceKey::of(source) == SourceKey::of(cached))
        })
    });
    pane.live_segments.gaps.clear();
    let body_width = width.saturating_sub(BAND_INSET * 2).max(1) as usize;
    let mut budget = live_segments::STEP_BYTES;
    let mut rows = Vec::new();
    if start > 0 {
        let source = &sources[start - 1];
        add_gap(
            &mut rows,
            &mut pane.live_segments,
            source,
            &snapshot.instance,
            Target::Tail,
            true,
            "Earlier live text…",
        );
    }
    for source in selected {
        let source_key = SourceKey::of(source);
        let target = focus
            .as_ref()
            .filter(|focus| focus.source == source_key)
            .map_or(Target::Tail, |focus| focus.target);
        let index = pane
            .live_segments
            .documents
            .iter()
            .position(|document| {
                document
                    .live_source()
                    .is_some_and(|cached| SourceKey::of(cached) == source_key)
            })
            .unwrap_or_else(|| {
                pane.live_segments.documents.push(Document::new(
                    source.clone(),
                    body_width,
                    target,
                ));
                pane.live_segments.documents.len() - 1
            });
        let document = &mut pane.live_segments.documents[index];
        document.update(source.clone(), body_width, target);
        document.step(&mut budget);
        let pending = document.pending();
        if source.reasoning.is_some() && !document.meaningful() && !pending {
            continue;
        }
        let next_anchor = document.next_anchor();
        let prepared = document.rows();
        let identity = identity(source, &snapshot.instance);
        let base = source_base(source);
        if let Some(first) = prepared.first() {
            if (first.source.start, first.ordinal) != (0, 0) {
                let (offset, ordinal) = if first.ordinal > 0 {
                    (first.source.start, first.ordinal - 1)
                } else {
                    (first.source.start.saturating_sub(1), usize::MAX)
                };
                add_gap(
                    &mut rows,
                    &mut pane.live_segments,
                    source,
                    &snapshot.instance,
                    Target::Backward(offset, ordinal),
                    true,
                    "Earlier live text…",
                );
            }
        }
        if let Some(index) = source.reasoning {
            rows.push(Row {
                anchor: Anchor {
                    identity: identity.clone(),
                    line: usize::MAX - 1,
                    offset: base,
                },
                line: Line::styled(
                    match source.attempt.reasoning[index].kind {
                        bob_types::ReasoningKind::Summary => "Thinking summary",
                        bob_types::ReasoningKind::Thinking => "Thinking",
                    },
                    text().fg(Palette::DIM()),
                ),
                band: false,
                hit: HitAction::Live,
            });
        }
        for row in &prepared {
            let mut line = row.line.clone();
            if source.reasoning.is_some() {
                for span in &mut line.spans {
                    span.style = span.style.fg(Palette::DIM());
                }
            }
            rows.push(Row {
                anchor: Anchor {
                    identity: identity.clone(),
                    line: row.ordinal,
                    offset: base + row.source.start,
                },
                line,
                band: false,
                hit: HitAction::Live,
            });
        }
        if pending {
            rows.push(Row {
                anchor: Anchor {
                    identity: identity.clone(),
                    line: usize::MAX - 2,
                    offset: base + source.text().len(),
                },
                line: Line::styled("Preparing live text…", text().fg(Palette::DIM())),
                band: false,
                hit: HitAction::Live,
            });
        } else if !matches!(target, Target::Tail) {
            if let Some((offset, ordinal)) = next_anchor {
                if offset < source.text().len() {
                    add_gap(
                        &mut rows,
                        &mut pane.live_segments,
                        source,
                        &snapshot.instance,
                        Target::Forward(offset, ordinal),
                        false,
                        "Later live text…",
                    );
                }
            }
        }
        if source.reasoning.is_none() {
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
        }
        rows.push(Row {
            anchor: Anchor {
                identity,
                line: usize::MAX - 5,
                offset: base + source.text().len(),
            },
            line: Line::default(),
            band: false,
            hit: HitAction::Live,
        });
    }
    if end < sources.len() {
        add_gap(
            &mut rows,
            &mut pane.live_segments,
            &sources[end],
            &snapshot.instance,
            Target::Forward(0, 0),
            false,
            "Later live text…",
        );
    }
    if pane.live_segments.pending() {
        pane.inline.invalidate();
    }
    pane.live_rendered = Some(RenderCache { key, rows });
}

pub(super) fn complete_rows(pane: &Pane, entry: &bob_kernel::Entry, width: u16) -> Vec<Row> {
    let target = pane
        .inline
        .focus
        .filter(|(sequence, _)| *sequence == entry.sequence)
        .map(|(_, offset)| {
            if pane.inline.backward {
                Target::Backward(offset, pane.inline.ordinal)
            } else {
                Target::Forward(offset, pane.inline.ordinal)
            }
        })
        .or_else(|| {
            pane.viewport
                .anchor
                .as_ref()
                .filter(|anchor| anchor.identity == AnchorId::Body(entry.sequence))
                .map(|anchor| Target::Forward(anchor.offset, anchor.line))
        })
        .unwrap_or(Target::Tail);
    let mut document = Document::complete(
        &entry.text,
        width.saturating_sub(BAND_INSET * 2).max(1) as usize,
        target,
    );
    while document.pending() {
        let mut budget = live_segments::STEP_BYTES;
        document.step(&mut budget);
    }
    let mut prepared = document.rows();
    if prepared.is_empty() && document.meaningful() && target != Target::Tail {
        document = Document::complete(
            &entry.text,
            width.saturating_sub(BAND_INSET * 2).max(1) as usize,
            Target::Tail,
        );
        while document.pending() {
            let mut budget = live_segments::STEP_BYTES;
            document.step(&mut budget);
        }
        prepared = document.rows();
    }
    let mut rows = Vec::new();
    let gap = |offset, ordinal, label| Row {
        anchor: Anchor {
            identity: AnchorId::BodyGap {
                sequence: entry.sequence,
                ordinal,
            },
            line: ordinal,
            offset,
        },
        line: Line::styled(label, text().fg(Palette::DIM())),
        band: false,
        hit: HitAction::Inline(entry.sequence, offset, ordinal),
    };
    if let Some(first) = prepared.first() {
        if (first.source.start, first.ordinal) != (0, 0) {
            let (offset, ordinal) = if first.ordinal > 0 {
                (first.source.start, first.ordinal - 1)
            } else {
                (first.source.start.saturating_sub(1), usize::MAX)
            };
            rows.push(gap(offset, ordinal, "Earlier reply text…"));
        }
    }
    rows.extend(prepared.into_iter().map(|row| Row {
        anchor: Anchor {
            identity: AnchorId::Body(entry.sequence),
            line: row.ordinal,
            offset: row.source.start,
        },
        line: row.line,
        band: false,
        hit: HitAction::Entry(entry.sequence),
    }));
    if let Some((offset, ordinal)) = document.next_anchor() {
        if offset < entry.text.len() {
            rows.push(gap(offset, ordinal, "Later reply text…"));
        }
    }
    rows
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

fn source_base(source: &Source) -> usize {
    source.reasoning.map_or(0, |index| {
        source.attempt.reasoning[..index]
            .iter()
            .map(|reasoning| reasoning.text.len() + 1)
            .sum()
    })
}

fn add_gap(
    rows: &mut Vec<Row>,
    segments: &mut Segments,
    source: &Source,
    instance: &str,
    target: Target,
    backward: bool,
    label: &'static str,
) {
    let offset = match target {
        Target::Tail => source.text().len(),
        Target::Forward(offset, _) | Target::Backward(offset, _) => offset,
    };
    let anchor = Anchor {
        identity: identity(source, instance),
        line: usize::MAX,
        offset: source_base(source) + offset,
    };
    segments.gaps.push(Gap {
        anchor: anchor.clone(),
        focus: Focus {
            source: SourceKey::of(source),
            target,
        },
        backward,
    });
    rows.push(Row {
        anchor,
        line: Line::styled(label, text().fg(Palette::DIM())),
        band: false,
        hit: HitAction::Live,
    });
}

pub(super) fn scroll(pane: &mut Pane, delta: isize) -> bool {
    if delta == 0 {
        return false;
    }
    let top = pane.viewport.top;
    let next = top.saturating_add_signed(delta);
    let begin = if delta < 0 { next } else { top };
    let end = if delta < 0 {
        top + 1
    } else {
        next.saturating_add(pane.viewport.height.max(1))
    };
    let mut candidate = pane
        .viewport
        .rows
        .iter()
        .enumerate()
        .skip(begin)
        .take(end.saturating_sub(begin))
        .filter_map(|(index, anchor)| {
            pane.live_segments
                .gaps
                .iter()
                .find(|gap| gap.anchor == *anchor && gap.backward == (delta < 0))
                .map(|gap| (index, gap.anchor.clone(), gap.focus.clone()))
        });
    let candidate = if delta < 0 {
        candidate.next_back()
    } else {
        candidate.next()
    };
    let Some((index, anchor, focus)) = candidate else {
        return false;
    };
    pane.live_segments.focus = Some(focus);
    pane.viewport.anchor = Some(anchor);
    pane.viewport.top = next.min(index);
    pane.inline.invalidate();
    true
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
                stopping: false,
                settled_sequence: None,
                error: None,
            })],
        }
    }

    fn finish(pane: &mut Pane, snapshot: &LiveSnapshot, width: u16) {
        for _ in 0..20000 {
            rows(pane, Some(snapshot), &ThreadId("root".into()), width);
            let cache = pane.live_rendered.as_ref().unwrap();
            assert!(cache.rows.len() <= live_segments::ROWS * 2 + 16);
            let row_bytes = cache.rows.capacity() * std::mem::size_of::<Row>()
                + cache
                    .rows
                    .iter()
                    .map(|row| {
                        row.line.spans.capacity()
                            * std::mem::size_of::<ratatui::text::Span<'static>>()
                            + row
                                .line
                                .spans
                                .iter()
                                .map(|span| match &span.content {
                                    std::borrow::Cow::Owned(text) => text.capacity(),
                                    _ => 0,
                                })
                                .sum::<usize>()
                    })
                    .sum::<usize>();
            let bytes = row_bytes
                + pane
                    .live_segments
                    .documents
                    .iter()
                    .map(Document::allocated_bytes)
                    .sum::<usize>();
            assert!(bytes <= 512 * 1024, "live prepared cache: {bytes}");
            if !pane.live_segments.pending() {
                return;
            }
        }
        panic!("live pane did not settle");
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
    fn short_body_gaps_reach_every_byte_without_hydration() {
        let entry = entry("x".repeat(3500));
        let mut pane = Pane::default();
        pane.inline.seek(entry.sequence, 0, 0, false);
        let mut collected = String::new();
        for _ in 0..1000 {
            let rows = complete_rows(&pane, &entry, BAND_INSET * 2 + 1);
            assert!(rows.len() <= live_segments::ROWS + 2);
            for row in &rows {
                if matches!(row.anchor.identity, AnchorId::Body(_)) {
                    collected.push_str(&row.line.to_string());
                }
            }
            if let Some(row) = rows
                .last()
                .filter(|row| row.line.to_string() == "Later reply text…")
            {
                let AnchorId::BodyGap { ordinal, .. } = row.anchor.identity else {
                    panic!("missing continuation anchor")
                };
                pane.inline
                    .seek(entry.sequence, row.anchor.offset, ordinal, false);
            } else {
                assert_eq!(collected, entry.text);
                assert_eq!(pane.inline.cache.borrow().counters.requests, 0);
                return;
            }
        }
        panic!("short reply traversal did not finish");
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
    fn bounded_live_cache_keeps_large_answer_navigable() {
        let snapshot = snapshot("Long visible paragraph with **styled** text.\n\n".repeat(8000));
        let mut pane = Pane::default();
        finish(&mut pane, &snapshot, 60);
        let first = pane
            .live_rendered
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .find(|row| {
                matches!(row.anchor.identity, AnchorId::Live { .. })
                    && row.anchor.line < usize::MAX - 5
            })
            .unwrap()
            .anchor
            .clone();
        assert!(first.offset > 0);
        pane.viewport.rows = pane
            .live_rendered
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .map(|row| row.anchor.clone())
            .collect();
        pane.viewport.top = pane.viewport.rows.len().saturating_sub(12);
        pane.viewport.height = 12;
        assert!(scroll(&mut pane, -10000));
        finish(&mut pane, &snapshot, 60);
        assert!(pane.live_rendered.as_ref().unwrap().rows.iter().any(|row| {
            matches!(row.anchor.identity, AnchorId::Live { .. })
                && row.anchor.line < usize::MAX - 5
                && row.anchor.offset < first.offset
        }));
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
