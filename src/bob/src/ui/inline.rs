use super::{render::Row, state::*, style::*, *};
use bob_kernel::{Entry, TextPage};
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, VecDeque},
    rc::Rc,
    sync::Arc,
    time::Instant,
};
use unicode_segmentation::UnicodeSegmentation;

pub(super) const PAGE_BYTES: usize = 16 * 1024;
pub(super) const ENTRY_GAP_LINE: usize = usize::MAX - 2;
const RAW_BUDGET: usize = 6 * 1024 * 1024;
const LAYOUT_BUDGET: usize = 8 * 1024 * 1024;
const MAX_FLIGHTS: usize = 4;
const MAX_SOURCES: usize = 16;
const MAX_ROWS: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(super) struct Source {
    pub thread: ThreadId,
    pub sequence: i64,
    artifact: Option<ArtifactId>,
}
impl Source {
    fn entry(entry: &Entry) -> Self {
        Self {
            thread: entry.thread.clone(),
            sequence: entry.sequence,
            artifact: entry.artifact.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct PageKey {
    source: Source,
    offset: usize,
}

struct RawPage {
    text: bob_ui::segmented::Sanitized,
    bytes: usize,
    touched: u64,
}

struct Body {
    total: Option<usize>,
    indexed: usize,
    complete: bool,
    target: usize,
    checkpoints: BTreeMap<usize, bob_ui::segmented::ScanState>,
    touched: u64,
}
impl Body {
    fn new(total: Option<usize>, touched: u64) -> Self {
        Self {
            total,
            indexed: 0,
            complete: false,
            target: 0,
            checkpoints: [(0, Default::default())].into_iter().collect(),
            touched,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct LayoutKey {
    page: PageKey,
    offset: usize,
    width: usize,
    tail: bool,
    theme: u64,
    ordinal: usize,
}
struct Prepared {
    rows: Arc<Vec<bob_ui::segmented::SourceRow>>,
    next: usize,
    next_ordinal: usize,
    bytes: usize,
    touched: u64,
}

struct Failure {
    message: String,
    retry_at: Instant,
}

#[derive(Default)]
pub(super) struct Counters {
    pub requests: usize,
    pub scanned_bytes: usize,
    pub layouts: usize,
    pub raw_bytes: usize,
    pub layout_bytes: usize,
    pub stale_replies: usize,
    pub max_flights: usize,
}

#[derive(Default)]
pub(super) struct Cache {
    bodies: HashMap<Source, Body>,
    raw: HashMap<PageKey, RawPage>,
    prepared: HashMap<LayoutKey, Prepared>,
    pending: HashMap<PageKey, u64>,
    failures: HashMap<PageKey, Failure>,
    wanted: VecDeque<(PageKey, u64)>,
    serial: u64,
    clock: u64,
    pub counters: Counters,
}
pub(super) type Shared = Rc<RefCell<Cache>>;

#[derive(Default)]
pub(super) struct Inline {
    pub cache: Shared,
    revision: u64,
    pub focus: Option<(i64, usize)>,
    pub ordinal: usize,
    pub backward: bool,
    pub entry_focus: Option<i64>,
    pub epoch: u64,
    pub error: Option<String>,
    width: u16,
}
impl Inline {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn seek(&mut self, sequence: i64, offset: usize, ordinal: usize, backward: bool) {
        self.focus = Some((sequence, offset));
        self.ordinal = ordinal;
        self.backward = backward;
        self.entry_focus = Some(sequence);
        self.cache
            .borrow_mut()
            .failures
            .retain(|key, _| key.source.sequence != sequence);
        self.invalidate();
    }
    pub fn reset(&mut self) {
        self.focus = None;
        self.error = None;
        self.entry_focus = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.invalidate();
    }
}

impl Cache {
    fn touch(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }
    fn source(&mut self, entry: &Entry) -> Source {
        let source = Source::entry(entry);
        let touched = self.touch();
        self.bodies
            .entry(source.clone())
            .or_insert_with(|| Body::new(entry.visible_text_bytes, touched))
            .touched = touched;
        while self.bodies.len() > MAX_SOURCES {
            let oldest = self
                .bodies
                .iter()
                .filter(|(candidate, _)| {
                    **candidate != source
                        && !self.pending.keys().any(|key| &key.source == *candidate)
                })
                .min_by_key(|(_, body)| body.touched)
                .map(|(source, _)| source.clone());
            let Some(oldest) = oldest else { break };
            self.bodies.remove(&oldest);
            self.failures.retain(|key, _| key.source != oldest);
            self.wanted.retain(|(key, _)| key.source != oldest);
        }
        source
    }
    fn want(&mut self, key: PageKey, epoch: u64) {
        if let Some((_, requested_epoch)) = self
            .wanted
            .iter_mut()
            .find(|(candidate, _)| candidate == &key)
        {
            *requested_epoch = epoch;
            return;
        }
        if self.pending.contains_key(&key)
            || self
                .failures
                .get(&key)
                .is_some_and(|failure| failure.retry_at > Instant::now())
        {
            return;
        }
        if self.wanted.len() == 16 {
            self.wanted.pop_front();
        }
        self.wanted.push_back((key, epoch));
    }
    fn trim(&mut self) {
        while self.counters.raw_bytes > RAW_BUDGET {
            let Some(key) = self
                .raw
                .iter()
                .min_by_key(|(_, page)| page.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(page) = self.raw.remove(&key) {
                self.counters.raw_bytes = self.counters.raw_bytes.saturating_sub(page.bytes);
            }
        }
        while self.counters.layout_bytes > LAYOUT_BUDGET {
            let Some(key) = self
                .prepared
                .iter()
                .min_by_key(|(_, page)| page.touched)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(page) = self.prepared.remove(&key) {
                self.counters.layout_bytes = self.counters.layout_bytes.saturating_sub(page.bytes);
            }
        }
    }
    fn layout(
        &mut self,
        key: &LayoutKey,
    ) -> Option<(Arc<Vec<bob_ui::segmented::SourceRow>>, usize, usize)> {
        let touched = self.touch();
        if !self.prepared.contains_key(key) {
            let raw = self.raw.get_mut(&key.page)?;
            raw.touched = touched;
            let layout = raw
                .text
                .layout(key.width, key.offset, key.ordinal, MAX_ROWS, key.tail);
            let bytes = layout.allocated_bytes() + std::mem::size_of::<Prepared>() + 256;
            self.counters.layouts += 1;
            self.counters.layout_bytes += bytes;
            self.prepared.insert(
                key.clone(),
                Prepared {
                    rows: Arc::new(layout.rows),
                    next: layout.next_offset,
                    next_ordinal: layout.next_ordinal,
                    bytes,
                    touched,
                },
            );
            self.trim();
        }
        let prepared = self.prepared.get_mut(key)?;
        prepared.touched = touched;
        Some((prepared.rows.clone(), prepared.next, prepared.next_ordinal))
    }
    pub fn clear(&mut self) {
        self.bodies.clear();
        self.raw.clear();
        self.prepared.clear();
        self.pending.clear();
        self.failures.clear();
        self.wanted.clear();
        self.counters.raw_bytes = 0;
        self.counters.layout_bytes = 0;
    }
    fn accept(
        &mut self,
        key: PageKey,
        token: u64,
        result: std::result::Result<TextPage, String>,
    ) -> bool {
        if self.pending.get(&key) != Some(&token) {
            self.counters.stale_replies += 1;
            return false;
        }
        self.pending.remove(&key);
        let touched = self.touch();
        let Some(body) = self.bodies.get_mut(&key.source) else {
            self.counters.stale_replies += 1;
            return false;
        };
        match result {
            Ok(mut page) => {
                let Some(mut scanner) = body.checkpoints.get(&key.offset).cloned() else {
                    self.counters.stale_replies += 1;
                    return false;
                };
                let mut end = key.offset + page.text.len();
                if page
                    .next_offset
                    .is_some_and(|next| next != end || next <= key.offset)
                    || end > page.total_bytes
                    || (page.next_offset.is_none() && end != page.total_bytes)
                {
                    self.failures.insert(
                        key,
                        Failure {
                            message: "Retained reply returned an inconsistent text range".into(),
                            retry_at: Instant::now() + Duration::from_secs(2),
                        },
                    );
                    return true;
                }
                let mut clean = scanner.scan(&page.text, key.offset);
                self.counters.scanned_bytes += page.text.len();
                if page.next_offset.is_some() {
                    let boundary = clean.last_block_source_boundary().or_else(|| {
                        let (byte, _) = clean.text.grapheme_indices(true).next_back()?;
                        let scalar = clean.text[..byte].chars().count();
                        Some(clean.offsets[scalar])
                    });
                    if let Some(boundary) = boundary.filter(|boundary| *boundary > key.offset) {
                        page.text.truncate(boundary - key.offset);
                        page.next_offset = Some(boundary);
                        end = boundary;
                        scanner = body.checkpoints[&key.offset].clone();
                        clean = scanner.scan(&page.text, key.offset);
                        self.counters.scanned_bytes += page.text.len();
                    }
                }
                clean.set_complete(page.next_offset.is_none());
                scanner.finish_layout_page(&clean);
                body.total = Some(page.total_bytes);
                if key.offset == body.indexed {
                    body.indexed = end;
                    body.complete = page.next_offset.is_none();
                    if let Some(next) = page.next_offset {
                        body.checkpoints.insert(next, scanner);
                    }
                }
                let bytes = clean.allocated_bytes() + std::mem::size_of::<RawPage>() + 256;
                let raw = RawPage {
                    text: clean,
                    bytes,
                    touched,
                };
                if let Some(old) = self.raw.insert(key.clone(), raw) {
                    self.counters.raw_bytes = self.counters.raw_bytes.saturating_sub(old.bytes);
                }
                self.counters.raw_bytes += bytes;
                self.failures.remove(&key);
                self.trim();
            }
            Err(message) => {
                self.failures.insert(
                    key,
                    Failure {
                        message,
                        retry_at: Instant::now() + Duration::from_secs(2),
                    },
                );
            }
        }
        true
    }
}

fn gap(
    sequence: i64,
    offset: usize,
    ordinal: usize,
    message: impl Into<String>,
    error: bool,
) -> Row {
    Row {
        anchor: Anchor {
            identity: AnchorId::BodyGap { sequence, ordinal },
            line: ordinal,
            offset,
        },
        line: Line::styled(
            message.into(),
            text().fg(if error {
                Palette::ERROR()
            } else {
                Palette::DIM()
            }),
        ),
        band: false,
        hit: HitAction::Inline(sequence, offset, ordinal),
    }
}

pub(super) fn long(entry: &Entry) -> bool {
    entry.kind == EntryKind::Assistant && entry.is_preview
}

pub(super) fn ready(pane: &Pane, entry: &Entry) -> bool {
    if !long(entry) {
        return true;
    }
    let cache = pane.inline.cache.borrow();
    let source = Source::entry(entry);
    let Some(body) = cache.bodies.get(&source) else {
        return false;
    };
    if body.total.is_none() || (!body.complete && body.indexed <= body.target) {
        return false;
    }
    let offset = body
        .checkpoints
        .range(..=body.target)
        .next_back()
        .map_or(0, |(offset, _)| *offset);
    cache.raw.contains_key(&PageKey { source, offset })
}

pub(super) fn resize(pane: &mut Pane, width: u16) {
    if pane.inline.width == width {
        return;
    }
    pane.inline.width = width;
    if pane
        .viewport
        .anchor
        .as_ref()
        .is_none_or(|anchor| !matches!(anchor.identity, AnchorId::BodyGap { .. }))
    {
        pane.inline.focus = None;
        pane.inline.entry_focus = None;
        pane.inline.backward = false;
    }
    pane.inline.invalidate();
}

pub(super) fn handoff(pane: &mut Pane) {
    let Some(anchor) = pane.viewport.anchor.as_ref() else {
        return;
    };
    let AnchorId::Live { operation, .. } = &anchor.identity else {
        return;
    };
    let Some(entry) = pane.view.as_ref().and_then(|view| {
        view.entries.iter().find(|entry| {
            entry.kind == EntryKind::Assistant && entry.operation.as_ref() == Some(operation)
        })
    }) else {
        return;
    };
    if !ready(pane, entry) {
        return;
    }
    let sequence = entry.sequence;
    let total = entry.visible_text_bytes.or_else(|| {
        pane.inline
            .cache
            .borrow()
            .bodies
            .get(&Source::entry(entry))
            .and_then(|body| body.total)
    });
    let anchor = pane.viewport.anchor.as_mut().unwrap();
    anchor.identity = AnchorId::Body(sequence);
    if let Some(total) = total {
        anchor.offset = anchor.offset.min(total.saturating_sub(1));
    }
    pane.inline.invalidate();
    pane.rendered = None;
}

pub(super) fn rows(pane: &Pane, entry: &Entry, width: u16) -> Vec<Row> {
    let mut cache = pane.inline.cache.borrow_mut();
    let source = cache.source(entry);
    let anchored = pane.viewport.anchor.as_ref().filter(|anchor| anchor.identity.sequence() == Some(entry.sequence)
        || matches!(&anchor.identity, AnchorId::Live { operation, .. } if entry.operation.as_ref() == Some(operation)));
    let focused = pane
        .inline
        .focus
        .filter(|(sequence, _)| *sequence == entry.sequence)
        .map(|(_, offset)| offset);
    let follow = pane.viewport.anchor.is_none() && focused.is_none();
    let body = &cache.bodies[&source];
    let target = focused
        .or_else(|| {
            anchored
                .filter(|anchor| {
                    matches!(
                        anchor.identity,
                        AnchorId::Body(_) | AnchorId::BodyGap { .. } | AnchorId::Live { .. }
                    )
                })
                .map(|anchor| anchor.offset)
        })
        .unwrap_or_else(|| {
            if follow {
                body.total.unwrap_or(0).saturating_sub(1)
            } else {
                0
            }
        })
        .min(body.total.unwrap_or(usize::MAX).saturating_sub(1));
    let tail = follow || (focused.is_some() && pane.inline.backward);
    let indexed = body.indexed;
    let total = body.total;
    let page_start = body
        .checkpoints
        .range(..=target)
        .next_back()
        .map_or(0, |(offset, _)| *offset);
    let needs_index = target >= indexed && !body.complete;
    cache.bodies.get_mut(&source).unwrap().target = target;
    let key = PageKey {
        source: source.clone(),
        offset: if needs_index { indexed } else { page_start },
    };
    if needs_index || !cache.raw.contains_key(&key) {
        let failure = cache
            .failures
            .get(&key)
            .map(|failure| failure.message.clone());
        cache.want(key, pane.inline.epoch);
        return vec![gap(
            entry.sequence,
            target,
            pane.inline.ordinal,
            failure.map_or_else(
                || {
                    if indexed == 0 {
                        "Loading reply…".into()
                    } else {
                        format!(
                            "Loading reply… {indexed}{} bytes indexed",
                            total.map_or(String::new(), |total| format!("/{total}"))
                        )
                    }
                },
                |error| {
                    format!(
                        "Reply unavailable: {} · scroll or click to retry",
                        safe(&error)
                    )
                },
            ),
            cache.failures.keys().any(|key| key.source == source),
        )];
    }
    let key = LayoutKey {
        page: key,
        offset: target,
        width: width.saturating_sub(BAND_INSET * 2).max(1) as usize,
        tail,
        theme: bob_ui::theme::generation(),
        ordinal: if tail && focused.is_none() {
            usize::MAX
        } else if focused.is_some() {
            pane.inline.ordinal
        } else {
            anchored.map_or(0, |anchor| anchor.line)
        },
    };
    let Some((prepared_rows, mut next, mut next_ordinal)) = cache.layout(&key) else {
        return vec![gap(
            entry.sequence,
            target,
            key.ordinal,
            "Loading reply layout…",
            false,
        )];
    };
    let mut selected_rows = Vec::new();
    let overlap = pane.viewport.height.clamp(24, 128);
    let previous = if follow && prepared_rows.len() < overlap && page_start > 0 {
        Some((page_start - 1, usize::MAX))
    } else if !follow && !tail && (target > 0 || key.ordinal > 0) {
        Some(if key.ordinal > 0 {
            (target, key.ordinal - 1)
        } else {
            (target.saturating_sub(1), usize::MAX)
        })
    } else {
        None
    };
    if let Some(previous) = previous {
        let offset = cache.bodies[&source]
            .checkpoints
            .range(..=previous.0)
            .next_back()
            .map_or(0, |(offset, _)| *offset);
        let previous_key = LayoutKey {
            page: PageKey {
                source: source.clone(),
                offset,
            },
            offset: previous.0,
            ordinal: previous.1,
            tail: true,
            ..key.clone()
        };
        if let Some((before, _, _)) = cache.layout(&previous_key) {
            let first = prepared_rows
                .first()
                .map(|row| (row.source.start, row.ordinal));
            selected_rows.extend(
                before
                    .iter()
                    .rev()
                    .take(if follow {
                        overlap - prepared_rows.len()
                    } else {
                        overlap
                    })
                    .rev()
                    .filter(|row| first.is_none_or(|first| (row.source.start, row.ordinal) < first))
                    .cloned(),
            );
        } else if follow {
            cache.want(previous_key.page, pane.inline.epoch);
        }
    }
    selected_rows.extend(prepared_rows.iter().cloned());
    if !follow && tail && !selected_rows.is_empty() && total.is_some_and(|total| next < total) {
        let offset = cache.bodies[&source]
            .checkpoints
            .range(..=next)
            .next_back()
            .map_or(0, |(offset, _)| *offset);
        let next_key = LayoutKey {
            page: PageKey {
                source: source.clone(),
                offset,
            },
            offset: next,
            ordinal: next_ordinal,
            tail: false,
            ..key.clone()
        };
        if let Some((after, after_next, after_ordinal)) = cache.layout(&next_key) {
            let last = selected_rows
                .last()
                .map(|row| (row.source.start, row.ordinal));
            selected_rows.extend(
                after
                    .iter()
                    .filter(|row| last.is_none_or(|last| (row.source.start, row.ordinal) > last))
                    .take(overlap)
                    .cloned(),
            );
            if let Some(last) = selected_rows.last() {
                let following = after.iter().find(|row| {
                    (row.source.start, row.ordinal) > (last.source.start, last.ordinal)
                });
                (next, next_ordinal) = following.map_or((after_next, after_ordinal), |row| {
                    (row.source.start, row.ordinal)
                });
            }
        }
    }
    let first = selected_rows
        .first()
        .map_or((page_start, 0), |row| (row.source.start, row.ordinal));
    let mut rows = Vec::with_capacity(selected_rows.len() + 2);
    if first != (0, 0) && (tail || !selected_rows.is_empty()) {
        let previous = if first.1 > 0 {
            (first.0, first.1 - 1)
        } else {
            (first.0.saturating_sub(1), usize::MAX)
        };
        rows.push(gap(
            entry.sequence,
            previous.0,
            previous.1,
            "Loading earlier reply text…",
            false,
        ));
    }
    rows.extend(selected_rows.iter().map(|row| Row {
        anchor: Anchor {
            identity: AnchorId::Body(entry.sequence),
            line: row.ordinal,
            offset: row.source.start,
        },
        line: row.line.clone(),
        band: false,
        hit: HitAction::Entry(entry.sequence),
    }));
    if total.is_none_or(|total| next < total) {
        rows.push(gap(
            entry.sequence,
            next,
            next_ordinal,
            "Loading later reply text…",
            false,
        ));
    }
    rows
}

pub(super) fn visible(pane: &mut Pane, top: usize, height: usize) -> bool {
    let candidate = pane
        .viewport
        .rows
        .iter()
        .skip(top)
        .take(height)
        .find(|anchor| {
            if let AnchorId::BodyGap { sequence, ordinal } = anchor.identity {
                if !pane.inline.backward
                    && pane.inline.focus.is_some_and(|(focused, offset)| {
                        focused == sequence
                            && (anchor.offset, ordinal) < (offset, pane.inline.ordinal)
                    })
                {
                    return false;
                }
                return !pane.viewport.anchor.as_ref().is_some_and(|current| {
                    current.identity == AnchorId::Body(sequence)
                        && (current.offset, current.line) > (anchor.offset, ordinal)
                });
            }
            anchor.line == ENTRY_GAP_LINE
        })
        .cloned();
    let Some(anchor) = candidate else {
        return false;
    };
    if pane.viewport.anchor.is_none() {
        return false;
    }
    let Some(sequence) = anchor.identity.sequence() else {
        return false;
    };
    if anchor.line == ENTRY_GAP_LINE {
        if pane.inline.entry_focus == Some(sequence) {
            return false;
        }
        pane.inline.entry_focus = Some(sequence);
    } else {
        let ordinal = match anchor.identity {
            AnchorId::BodyGap { ordinal, .. } => ordinal,
            _ => 0,
        };
        let next = Some((sequence, anchor.offset));
        if pane.inline.focus == next && pane.inline.ordinal == ordinal {
            return false;
        }
        pane.inline.focus = next;
        pane.inline.ordinal = ordinal;
        pane.inline.backward = ordinal == usize::MAX;
    }
    pane.inline.invalidate();
    pane.rendered = None;
    true
}

pub(super) fn scroll(pane: &mut Pane, delta: isize) -> bool {
    let top = pane.viewport.top;
    let next = top.saturating_add_signed(delta);
    let edge = if delta < 0 {
        pane.viewport
            .rows
            .iter()
            .enumerate()
            .take(top + 1)
            .skip(next)
            .rev()
            .find(|(_, anchor)| {
                matches!(anchor.identity, AnchorId::BodyGap { .. }) || anchor.line == ENTRY_GAP_LINE
            })
            .or_else(|| {
                (top == 0)
                    .then(|| {
                        pane.viewport
                            .rows
                            .iter()
                            .enumerate()
                            .take(pane.viewport.height.max(1))
                            .take_while(|(_, anchor)| !matches!(anchor.identity, AnchorId::Body(_)))
                            .find(|(_, anchor)| {
                                matches!(anchor.identity, AnchorId::BodyGap { .. })
                                    || anchor.line == ENTRY_GAP_LINE
                            })
                    })
                    .flatten()
            })
    } else {
        pane.viewport
            .rows
            .iter()
            .enumerate()
            .skip(top)
            .take(next.saturating_sub(top) + pane.viewport.height.max(1))
            .find(|(_, anchor)| {
                matches!(anchor.identity, AnchorId::BodyGap { .. }) || anchor.line == ENTRY_GAP_LINE
            })
    }
    .map(|(index, anchor)| (index, anchor.clone()));
    let Some((index, anchor)) = edge else {
        return false;
    };
    let Some(sequence) = anchor.identity.sequence() else {
        return false;
    };
    if anchor.line == ENTRY_GAP_LINE {
        pane.inline.entry_focus = Some(sequence);
        pane.inline.focus = Some((sequence, if delta < 0 { usize::MAX } else { 0 }));
        pane.inline.backward = delta < 0;
        pane.inline.ordinal = if delta < 0 { usize::MAX } else { 0 };
        pane.inline.invalidate();
    } else if let AnchorId::BodyGap { ordinal, .. } = anchor.identity {
        pane.inline
            .seek(sequence, anchor.offset, ordinal, delta < 0);
    }
    pane.viewport.anchor = if delta > 0 && next < index {
        pane.viewport.rows.get(next).cloned().or(Some(anchor))
    } else {
        Some(anchor)
    };
    pane.viewport.top = next.min(index);
    pane.rendered = None;
    true
}

pub(super) fn entry_window(pane: &Pane) -> Option<std::ops::Range<usize>> {
    let view = pane.view.as_ref()?;
    // Render the whole conversation; no "Earlier/Later conversation" gaps.
    Some(0..view.entries.len())
}

pub(super) fn entry_gap(entry: &Entry, older: bool) -> Row {
    Row {
        anchor: Anchor {
            identity: AnchorId::Entry(entry.sequence),
            line: ENTRY_GAP_LINE,
            offset: if older { usize::MAX } else { 0 },
        },
        line: Line::styled(
            if older {
                "Earlier conversation…"
            } else {
                "Later conversation…"
            },
            text().fg(Palette::DIM()),
        ),
        band: false,
        hit: HitAction::Inline(entry.sequence, if older { usize::MAX } else { 0 }, 0),
    }
}

impl App {
    pub(super) fn refresh_inline(&mut self, jobs: &mut JoinSet<Reply>) {
        let (Some(handle), Some(selected)) = (self.handle.clone(), self.selected.clone()) else {
            return;
        };
        let Some(pane) = self.panes.get(&selected) else {
            return;
        };
        let shared = pane.inline.cache.clone();
        let epoch = pane.inline.epoch;
        let mut cache = shared.borrow_mut();
        let mut queued = cache.wanted.len();
        while cache.pending.len() < MAX_FLIGHTS && queued > 0 {
            queued -= 1;
            let Some((key, requested_epoch)) = cache.wanted.pop_front() else {
                break;
            };
            if self
                .panes
                .get(&key.source.thread)
                .is_none_or(|pane| pane.inline.epoch != requested_epoch)
                || cache.pending.contains_key(&key)
            {
                continue;
            }
            if key.source.thread != selected {
                cache.wanted.push_back((key, requested_epoch));
                continue;
            }
            if cache
                .failures
                .get(&key)
                .is_some_and(|failure| failure.retry_at > Instant::now())
            {
                continue;
            }
            cache.serial = cache.serial.wrapping_add(1);
            let token = cache.serial;
            cache.pending.insert(key.clone(), token);
            cache.counters.requests += 1;
            cache.counters.max_flights = cache.counters.max_flights.max(cache.pending.len());
            let handle = handle.clone();
            let runtime = self.runtime_epoch;
            jobs.spawn(async move {
                let result = handle
                    .entry_text(
                        key.source.thread.clone(),
                        key.source.sequence,
                        key.offset,
                        PAGE_BYTES,
                    )
                    .await
                    .map_err(|error| format!("{error:#}"));
                Reply::Inline(PageReply {
                    source: key.source,
                    offset: key.offset,
                    token,
                    epoch,
                    runtime,
                    result,
                })
            });
        }
    }
    pub(super) fn accept_inline(&mut self, reply: PageReply) {
        let key = PageKey {
            source: reply.source.clone(),
            offset: reply.offset,
        };
        let mut cache = self.inline_cache.borrow_mut();
        if self.runtime_epoch != reply.runtime {
            if cache.pending.get(&key) == Some(&reply.token) {
                cache.pending.remove(&key);
            }
            cache.counters.stale_replies += 1;
            return;
        }
        let error = reply.result.as_ref().err().cloned();
        let accepted = cache.accept(key, reply.token, reply.result);
        let indexing = cache.bodies.get(&reply.source).and_then(|body| {
            (accepted
                && !body.complete
                && body.indexed <= body.target
                && body.indexed > reply.offset)
                .then_some(body.indexed)
        });
        if let Some(offset) = indexing {
            let epoch = self
                .panes
                .get(&reply.source.thread)
                .map_or(reply.epoch, |pane| pane.inline.epoch);
            cache.want(
                PageKey {
                    source: reply.source.clone(),
                    offset,
                },
                epoch,
            );
        }
        drop(cache);
        if accepted {
            if let Some(pane) = self.panes.get_mut(&reply.source.thread) {
                if pane.inline.epoch == reply.epoch {
                    pane.inline.error = error;
                }
                pane.inline.invalidate();
                pane.rendered = None;
            }
        }
    }
}

pub(super) struct PageReply {
    source: Source,
    offset: usize,
    token: u64,
    epoch: u64,
    runtime: u64,
    result: std::result::Result<TextPage, String>,
}
impl PageReply {
    pub(super) fn is_error(&self) -> bool {
        self.result.is_err()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        Entry {
            sequence: 1,
            thread: ThreadId("fixture".into()),
            kind: EntryKind::Assistant,
            text: "preview".into(),
            artifact: Some(ArtifactId("immutable".into())),
            operation: None,
            tool: None,
            sender: None,
            parent_guidance: false,
            submission: None,
            is_preview: true,
            visible_text_bytes: None,
        }
    }
    fn publish(cache: &mut Cache, source: &Source, text: &str, offset: usize, token: u64) -> usize {
        let mut end = (offset + PAGE_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1
        }
        let key = PageKey {
            source: source.clone(),
            offset,
        };
        cache.pending.insert(key.clone(), token);
        assert!(cache.accept(
            key,
            token,
            Ok(TextPage {
                text: text[offset..end].into(),
                total_bytes: text.len(),
                next_offset: (end < text.len()).then_some(end),
            })
        ));
        cache.bodies[source].indexed
    }

    fn loaded_pane(text: &str) -> (Pane, Entry) {
        let entry = entry();
        let pane = Pane::default();
        let mut cache = pane.inline.cache.borrow_mut();
        let source = cache.source(&entry);
        let mut offset = 0;
        while offset < text.len() {
            offset = publish(&mut cache, &source, text, offset, offset as u64 + 1);
        }
        drop(cache);
        (pane, entry)
    }

    #[test]
    fn backward_navigation_crosses_empty_sanitized_pages() {
        let text = format!(
            "BEFORE\n\n\u{1b}]52;c;{}\u{7}AFTER",
            "hidden".repeat(PAGE_BYTES)
        );
        let (mut pane, entry) = loaded_pane(&text);
        let mut displayed = rows(&pane, &entry, 80);
        assert!(displayed
            .iter()
            .any(|row| row.line.to_string().contains("AFTER")));
        let pages = pane.inline.cache.borrow().bodies[&Source::entry(&entry)]
            .checkpoints
            .len();
        let mut previous = usize::MAX;
        let mut reached_before = false;
        for _ in 0..pages + 2 {
            pane.viewport.rows = displayed.iter().map(|row| row.anchor.clone()).collect();
            pane.viewport.position(30);
            assert!(scroll(&mut pane, -30));
            let target = pane.inline.focus.unwrap().1;
            assert!(target < previous, "backward navigation revisited {target}");
            previous = target;
            displayed = rows(&pane, &entry, 80);
            let visible = displayed
                .iter()
                .map(|row| row.line.to_string())
                .collect::<String>();
            assert!(!visible.contains("hidden"));
            if visible.contains("BEFORE") {
                reached_before = true;
                break;
            }
        }
        assert!(reached_before, "backward navigation did not reach BEFORE");
        assert!(pane.inline.cache.borrow().counters.layouts <= (pages + 2) * 2);
    }

    #[test]
    fn forward_navigation_crosses_empty_sanitized_pages_without_bouncing_back() {
        let text = format!(
            "BEFORE\n\n\u{1b}]52;c;{}\u{7}AFTER",
            "hidden".repeat(PAGE_BYTES)
        );
        let (mut pane, entry) = loaded_pane(&text);
        pane.inline.seek(entry.sequence, 0, 0, false);
        pane.viewport.anchor = Some(Anchor {
            identity: AnchorId::Body(entry.sequence),
            offset: 0,
            line: 0,
        });
        let pages = pane.inline.cache.borrow().bodies[&Source::entry(&entry)]
            .checkpoints
            .len();
        let mut previous = 0;
        let mut reached_after = false;
        for _ in 0..pages + 2 {
            let displayed = rows(&pane, &entry, 80);
            let shown = displayed
                .iter()
                .map(|row| row.line.to_string())
                .collect::<String>();
            assert!(!shown.contains("hidden"));
            pane.viewport.rows = displayed.iter().map(|row| row.anchor.clone()).collect();
            pane.viewport.position(30);
            if shown.contains("AFTER") {
                assert!(!visible(&mut pane, 0, 30));
                reached_after = true;
                break;
            }
            assert!(visible(&mut pane, 0, 30));
            let target = pane.inline.focus.unwrap().1;
            assert!(target > previous, "forward navigation revisited {target}");
            assert!(!pane.inline.backward);
            previous = target;
        }
        assert!(reached_after);
    }

    #[test]
    fn queued_request_and_accepted_page_survive_pane_epoch_change() {
        let mut app = App::new(100);
        let entry = entry();
        app.select(entry.thread.clone());
        let source = app.inline_cache.borrow_mut().source(&entry);
        let key = PageKey {
            source: source.clone(),
            offset: 0,
        };
        {
            let mut cache = app.inline_cache.borrow_mut();
            cache.want(key.clone(), 0);
            cache.want(key.clone(), 1);
            assert_eq!(cache.wanted.len(), 1);
            assert_eq!(cache.wanted[0].1, 1);
            cache.wanted.clear();
            cache.pending.insert(key, 1);
        }
        app.pane_mut().unwrap().inline.reset();
        let revision = app.pane().unwrap().inline.revision();
        app.accept_inline(PageReply {
            source,
            offset: 0,
            token: 1,
            epoch: 0,
            runtime: app.runtime_epoch,
            result: Ok(TextPage {
                text: "retained reply".into(),
                total_bytes: 14,
                next_offset: None,
            }),
        });
        assert!(app.pane().unwrap().inline.revision() > revision);
        assert!(rows(app.pane().unwrap(), &entry, 80)
            .iter()
            .any(|row| row.line.to_string().contains("retained reply")));
    }

    #[test]
    fn upward_scroll_enters_earlier_gap_below_header_in_short_viewport() {
        for delta in [-1, -24] {
            let mut pane = Pane::default();
            pane.viewport.height = 24;
            pane.viewport.rows = vec![
                Anchor {
                    identity: AnchorId::Entry(1),
                    offset: 0,
                    line: 0,
                },
                gap(1, PAGE_BYTES - 1, usize::MAX, "earlier", false).anchor,
                Anchor {
                    identity: AnchorId::Body(1),
                    offset: PAGE_BYTES,
                    line: 0,
                },
            ];
            assert_eq!(pane.viewport.position(24), 0);
            assert!(pane.viewport.anchor.is_none());
            assert!(scroll(&mut pane, delta));
            assert_eq!(pane.inline.focus, Some((1, PAGE_BYTES - 1)));
            assert!(pane.inline.backward);
            assert!(pane.viewport.anchor.is_some());
        }
    }

    #[test]
    fn styled_inline_resize_retains_the_visible_reading_position() {
        let text = (0..100)
            .map(|i| format!("**MARKER{i:03}** {}\n\n", "word ".repeat(12)))
            .collect::<String>();
        let (mut pane, entry) = loaded_pane(&text);
        resize(&mut pane, 80);
        let original = rows(&pane, &entry, 80);
        let anchor = original
            .iter()
            .find(|row| row.line.to_string().contains("MARKER060"))
            .unwrap()
            .anchor
            .clone();
        pane.viewport.anchor = Some(anchor);
        for width in [35, 101, 57] {
            resize(&mut pane, width);
            let resized = rows(&pane, &entry, width);
            pane.viewport.rows = resized.iter().map(|row| row.anchor.clone()).collect();
            let top = pane.viewport.position(12);
            let visible = resized
                .iter()
                .skip(top)
                .take(12)
                .map(|row| row.line.to_string())
                .collect::<String>();
            assert!(
                resized[top].line.to_string().contains("MARKER060"),
                "width={width}: {visible}"
            );
            assert!(
                !visible.contains("MARKER059"),
                "reading position moved backward: {visible}"
            );
        }
    }

    #[test]
    fn cross_page_pathological_graphemes_are_lossless_and_bounded() {
        for text in [
            format!("PREFIXa{}SUFFIX", "\u{301}".repeat(PAGE_BYTES * 2)),
            format!("PREFIX{}SUFFIX", "👩\u{200d}".repeat(PAGE_BYTES)),
        ] {
            let mut cache = Cache::default();
            let source = cache.source(&entry());
            let mut offset = 0;
            let mut output = String::new();
            let mut fragmented = false;
            while offset < text.len() {
                let end = publish(&mut cache, &source, &text, offset, offset as u64 + 1);
                assert!(end > offset && end - offset <= PAGE_BYTES);
                let key = PageKey {
                    source: source.clone(),
                    offset,
                };
                let mut next = (offset, 0);
                loop {
                    let page = cache.raw[&key].text.layout(1, next.0, next.1, 7, false);
                    assert!(page.rows.len() <= 7);
                    for row in &page.rows {
                        let shown = row.line.to_string();
                        assert!(shown.len() <= bob_ui::segmented::MAX_ROW_BYTES);
                        fragmented |= row.grapheme_fragment;
                        output.push_str(&shown);
                    }
                    if page.next_offset == end {
                        break;
                    }
                    let following = (page.next_offset, page.next_ordinal);
                    assert!(following > next);
                    next = following;
                }
                assert!(cache.counters.raw_bytes <= RAW_BUDGET);
                offset = end;
            }
            assert_eq!(output, text);
            assert!(fragmented);
            assert!(cache.counters.scanned_bytes <= text.len() * 2);
        }
    }

    #[test]
    fn stale_reply_does_not_remove_new_request_or_publish_error() {
        let mut cache = Cache::default();
        let source = cache.source(&entry());
        let key = PageKey { source, offset: 0 };
        cache.pending.insert(key.clone(), 2);
        assert!(!cache.accept(key.clone(), 1, Err("stale failure".into())));
        assert_eq!(cache.pending.get(&key), Some(&2));
        assert!(cache.failures.is_empty());
        assert_eq!(cache.counters.stale_replies, 1);
    }

    #[test]
    fn raw_cache_and_checkpoint_walk_are_bounded_through_near_cap() {
        let text = format!("{}FINAL", "a界e\u{301}👩\u{200d}💻\n".repeat(170_000));
        assert!(text.len() > 1024 * 1024);
        let mut cache = Cache::default();
        let source = cache.source(&entry());
        let mut offset = 0;
        let mut reconstructed = String::new();
        while offset < text.len() {
            let end = publish(&mut cache, &source, &text, offset, offset as u64 + 1);
            let page = &cache.raw[&PageKey {
                source: source.clone(),
                offset,
            }];
            reconstructed.push_str(&page.text.text);
            assert!(end > offset && end - offset <= PAGE_BYTES);
            assert!(cache.counters.raw_bytes <= RAW_BUDGET);
            offset = end;
        }
        assert_eq!(reconstructed, text);
        assert!(cache.bodies[&source].complete);
        assert!(cache.bodies[&source].checkpoints.len() <= text.len() / (PAGE_BYTES - 256) + 2);
    }

    #[test]
    fn sanitization_state_crosses_multiple_pages_without_exposing_osc_payload() {
        let source_text = format!("before\u{1b}]52;c;{}\u{7}after", "hidden".repeat(9000));
        let mut cache = Cache::default();
        let source = cache.source(&entry());
        let mut offset = 0;
        let mut shown = String::new();
        while offset < source_text.len() {
            let end = publish(&mut cache, &source, &source_text, offset, offset as u64 + 1);
            shown.push_str(
                &cache.raw[&PageKey {
                    source: source.clone(),
                    offset,
                }]
                    .text
                    .text,
            );
            assert!(end > offset);
            offset = end;
        }
        assert_eq!(shown, "beforeafter");
    }

    #[test]
    fn cache_deduplicates_requests_and_retry_keeps_true_error_visible() {
        let mut cache = Cache::default();
        let source = cache.source(&entry());
        let key = PageKey { source, offset: 0 };
        cache.want(key.clone(), 1);
        cache.want(key.clone(), 1);
        assert_eq!(cache.wanted.len(), 1);
        cache.wanted.clear();
        cache.pending.insert(key.clone(), 2);
        assert!(cache.accept(key.clone(), 2, Err("retained artifact missing".into())));
        cache.want(key.clone(), 1);
        assert!(cache.wanted.is_empty());
        assert_eq!(cache.failures[&key].message, "retained artifact missing");
        cache.failures.get_mut(&key).unwrap().retry_at = Instant::now();
        cache.want(key, 1);
        assert_eq!(cache.wanted.len(), 1);
    }
}
