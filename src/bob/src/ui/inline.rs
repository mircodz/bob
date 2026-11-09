use super::{render::Row, state::*, style::*, *};
use bob_kernel::{Entry, TextPage};
use bob_ui::segmented::SourceRow;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    rc::Rc,
    time::Instant,
};

/// Saved replies longer than the 2,048-character entry preview are fetched in
/// full in the background, page by page, and then rendered whole.
pub(super) const PAGE_BYTES: usize = 64 * 1024;
const MAX_FLIGHTS: usize = 4;

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

struct Failure {
    message: String,
    retry_at: Instant,
}

#[derive(Default)]
struct Body {
    text: String,
    complete: bool,
    failure: Option<Failure>,
    layout: Option<Layout>,
}

struct Layout {
    width: u16,
    theme: u64,
    rows: Vec<SourceRow>,
}

#[derive(Default)]
pub(super) struct Counters {
    pub requests: usize,
    pub layouts: usize,
    pub stale_replies: usize,
    pub max_flights: usize,
}

#[derive(Default)]
pub(super) struct Cache {
    bodies: HashMap<Source, Body>,
    pending: HashMap<Source, (usize, u64)>,
    wanted: VecDeque<(Source, u64)>,
    serial: u64,
    pub counters: Counters,
}
pub(super) type Shared = Rc<RefCell<Cache>>;

#[derive(Default)]
pub(super) struct Inline {
    pub cache: Shared,
    revision: u64,
    pub epoch: u64,
    pub error: Option<String>,
}
impl Inline {
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn reset(&mut self) {
        self.error = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.invalidate();
    }
}

impl Cache {
    fn want(&mut self, source: Source, epoch: u64) {
        let settled = self.bodies.get(&source).is_some_and(|body| {
            body.complete
                || body
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.retry_at > Instant::now())
        });
        if settled || self.pending.contains_key(&source) {
            return;
        }
        self.wanted.retain(|(candidate, _)| candidate != &source);
        self.wanted.push_front((source, epoch));
    }
    pub fn clear(&mut self) {
        self.bodies.clear();
        self.pending.clear();
        self.wanted.clear();
    }
    /// Drops the loaded replies of a conversation that is no longer kept.
    pub fn forget(&mut self, thread: &ThreadId) {
        self.bodies.retain(|source, _| &source.thread != thread);
        self.wanted.retain(|(source, _)| &source.thread != thread);
    }
    fn accept(
        &mut self,
        source: &Source,
        offset: usize,
        token: u64,
        result: std::result::Result<TextPage, String>,
    ) -> bool {
        if self.pending.get(source) != Some(&(offset, token)) {
            self.counters.stale_replies += 1;
            return false;
        }
        self.pending.remove(source);
        let Some(body) = self.bodies.get_mut(source) else {
            self.counters.stale_replies += 1;
            return false;
        };
        let message = match result {
            Ok(page) => {
                let end = offset + page.text.len();
                if offset == body.text.len()
                    && end <= page.total_bytes
                    && page
                        .next_offset
                        .map_or(end == page.total_bytes, |next| next == end && next > offset)
                {
                    body.text.push_str(&page.text);
                    body.complete = page.next_offset.is_none();
                    body.failure = None;
                    return true;
                }
                "Retained reply returned an inconsistent text range".to_owned()
            }
            Err(message) => message,
        };
        body.failure = Some(Failure {
            message,
            retry_at: Instant::now() + Duration::from_secs(2),
        });
        true
    }
}

pub(super) fn long(entry: &Entry) -> bool {
    entry.kind == EntryKind::Assistant && entry.is_preview
}

/// Whether a saved reply can replace its streamed text: short replies arrive
/// whole, long ones once their full text has loaded.
pub(super) fn ready(pane: &Pane, entry: &Entry) -> bool {
    !long(entry)
        || pane
            .inline
            .cache
            .borrow()
            .bodies
            .get(&Source::entry(entry))
            .is_some_and(|body| body.complete)
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
            .map(|body| body.text.len())
    });
    let anchor = pane.viewport.anchor.as_mut().unwrap();
    anchor.identity = AnchorId::Body(sequence);
    if let Some(total) = total {
        anchor.offset = anchor.offset.min(total.saturating_sub(1));
    }
    pane.inline.invalidate();
    pane.rendered = None;
}

/// Rows for a long saved reply: its full text once loaded, its preview until
/// then. Returns None while the streamed text of the same reply is still on
/// screen, so the reply is never shown twice during the handoff.
pub(super) fn rows(pane: &Pane, entry: &Entry, width: u16) -> Option<Vec<Row>> {
    let mut guard = pane.inline.cache.borrow_mut();
    let cache = &mut *guard;
    let source = Source::entry(entry);
    let body = cache.bodies.entry(source.clone()).or_default();
    if body.complete {
        let theme = bob_ui::theme::generation();
        if body
            .layout
            .as_ref()
            .is_none_or(|layout| (layout.width, layout.theme) != (width, theme))
        {
            cache.counters.layouts += 1;
            body.layout = Some(Layout {
                width,
                theme,
                rows: super::live::complete_layout(&body.text, width),
            });
        }
        let layout = body.layout.as_ref().unwrap();
        return Some(super::live::body_rows(
            entry.sequence,
            layout.rows.iter().cloned(),
        ));
    }
    let failure = body.failure.as_ref().map(|failure| failure.message.clone());
    cache.want(source, pane.inline.epoch);
    if entry
        .operation
        .as_ref()
        .is_some_and(|operation| pane.live_segments.shows(operation))
    {
        return None;
    }
    let mut rows = super::live::complete_rows(entry, width);
    if let Some(error) = failure {
        rows.push(Row {
            anchor: Anchor {
                identity: AnchorId::Body(entry.sequence),
                line: usize::MAX,
                offset: entry.text.len(),
            },
            line: Line::styled(
                format!("Full reply unavailable: {}", safe(&error)),
                text().fg(Palette::ERROR()),
            ),
            band: false,
            hit: HitAction::Entry(entry.sequence),
        });
    }
    Some(rows)
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
        let mut cache = shared.borrow_mut();
        let mut queued = cache.wanted.len();
        while cache.pending.len() < MAX_FLIGHTS && queued > 0 {
            queued -= 1;
            let Some((source, epoch)) = cache.wanted.pop_front() else {
                break;
            };
            if self
                .panes
                .get(&source.thread)
                .is_none_or(|pane| pane.inline.epoch != epoch)
                || cache.pending.contains_key(&source)
            {
                continue;
            }
            if source.thread != selected {
                cache.wanted.push_back((source, epoch));
                continue;
            }
            let Some(offset) = cache
                .bodies
                .get(&source)
                .filter(|body| {
                    !body.complete
                        && body
                            .failure
                            .as_ref()
                            .is_none_or(|failure| failure.retry_at <= Instant::now())
                })
                .map(|body| body.text.len())
            else {
                continue;
            };
            cache.serial = cache.serial.wrapping_add(1);
            let token = cache.serial;
            cache.pending.insert(source.clone(), (offset, token));
            cache.counters.requests += 1;
            cache.counters.max_flights = cache.counters.max_flights.max(cache.pending.len());
            let handle = handle.clone();
            let runtime = self.runtime_epoch;
            jobs.spawn(async move {
                let result = handle
                    .entry_text(source.thread.clone(), source.sequence, offset, PAGE_BYTES)
                    .await
                    .map_err(|error| format!("{error:#}"));
                Reply::Inline(PageReply {
                    source,
                    offset,
                    token,
                    epoch,
                    runtime,
                    result,
                })
            });
        }
    }
    pub(super) fn accept_inline(&mut self, reply: PageReply) {
        let mut cache = self.inline_cache.borrow_mut();
        if self.runtime_epoch != reply.runtime {
            if cache.pending.get(&reply.source) == Some(&(reply.offset, reply.token)) {
                cache.pending.remove(&reply.source);
            }
            cache.counters.stale_replies += 1;
            return;
        }
        let error = reply.result.as_ref().err().cloned();
        if !cache.accept(&reply.source, reply.offset, reply.token, reply.result) {
            return;
        }
        let (complete, failed) = cache
            .bodies
            .get(&reply.source)
            .map_or((false, true), |body| {
                (body.complete, body.failure.is_some())
            });
        if !complete && !failed {
            let epoch = self
                .panes
                .get(&reply.source.thread)
                .map_or(reply.epoch, |pane| pane.inline.epoch);
            cache.want(reply.source.clone(), epoch);
        }
        drop(cache);
        if let Some(pane) = self.panes.get_mut(&reply.source.thread) {
            if pane.inline.epoch == reply.epoch {
                pane.inline.error = error;
            }
            if complete || failed {
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

    fn entry(text: &str) -> Entry {
        Entry {
            sequence: 1,
            thread: ThreadId("fixture".into()),
            kind: EntryKind::Assistant,
            text: text.into(),
            artifact: Some(ArtifactId("immutable".into())),
            operation: Some(OperationId("reply".into())),
            tool: None,
            sender: None,
            parent_guidance: false,
            submission: None,
            is_preview: true,
            visible_text_bytes: None,
            thinking: None,
        }
    }

    fn preview(text: &str) -> String {
        text.chars().take(2048).collect()
    }

    /// Delivers `text` page by page, the way the runtime serves it.
    fn publish(cache: &mut Cache, source: &Source, text: &str) {
        cache.bodies.entry(source.clone()).or_default();
        let mut offset = 0;
        for token in 1.. {
            let mut end = (offset + PAGE_BYTES).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            cache.pending.insert(source.clone(), (offset, token));
            assert!(cache.accept(
                source,
                offset,
                token,
                Ok(TextPage {
                    text: text[offset..end].into(),
                    total_bytes: text.len(),
                    next_offset: (end < text.len()).then_some(end),
                })
            ));
            if end == text.len() {
                return;
            }
            offset = end;
        }
    }

    fn shown(rows: &[Row]) -> String {
        rows.iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn long_reply_shows_its_preview_until_the_full_text_loads() {
        let full = format!("{}FULL_TAIL", "Opening paragraph.\n\n".repeat(200));
        let entry = entry(&preview(&full));
        let pane = Pane::default();
        let before = shown(&rows(&pane, &entry, 80).unwrap());
        assert!(before.contains("Opening paragraph."));
        assert!(!before.contains("FULL_TAIL") && !before.contains("Loading"));
        assert_eq!(pane.inline.cache.borrow().wanted.len(), 1);
        publish(
            &mut pane.inline.cache.borrow_mut(),
            &Source::entry(&entry),
            &full,
        );
        let after = shown(&rows(&pane, &entry, 80).unwrap());
        assert_eq!(after.matches("Opening paragraph.").count(), 200);
        assert!(after.contains("FULL_TAIL"));
        assert!(
            !after.contains("Earlier") && !after.contains("Later") && !after.contains("Loading")
        );
    }

    #[test]
    fn full_text_spanning_many_pages_renders_every_line_once() {
        let full: String = (0..20_000)
            .map(|index| format!("line {index:05}\n\n"))
            .collect();
        assert!(full.len() > PAGE_BYTES * 3);
        let entry = entry(&preview(&full));
        let pane = Pane::default();
        publish(
            &mut pane.inline.cache.borrow_mut(),
            &Source::entry(&entry),
            &full,
        );
        let text = shown(&rows(&pane, &entry, 80).unwrap());
        for index in [0, 1, 9_999, 19_999] {
            assert_eq!(text.matches(&format!("line {index:05}")).count(), 1);
        }
        assert_eq!(text.matches("line ").count(), 20_000);
    }

    #[test]
    fn full_text_is_laid_out_once_per_width() {
        let full = "Paragraph with **styled** text.\n\n".repeat(400);
        let entry = entry(&preview(&full));
        let pane = Pane::default();
        publish(
            &mut pane.inline.cache.borrow_mut(),
            &Source::entry(&entry),
            &full,
        );
        let wide = rows(&pane, &entry, 100).unwrap();
        for _ in 0..10 {
            rows(&pane, &entry, 100);
        }
        assert_eq!(pane.inline.cache.borrow().counters.layouts, 1);
        let narrow = rows(&pane, &entry, 20).unwrap();
        assert!(narrow.len() > wide.len());
        assert_eq!(pane.inline.cache.borrow().counters.layouts, 2);
    }

    #[test]
    fn terminal_escape_payloads_stay_hidden_across_pages() {
        let full = format!(
            "before\u{1b}]52;c;{}\u{7}after",
            "hidden".repeat(PAGE_BYTES / 2)
        );
        let entry = entry(&preview(&full));
        let pane = Pane::default();
        publish(
            &mut pane.inline.cache.borrow_mut(),
            &Source::entry(&entry),
            &full,
        );
        let text = shown(&rows(&pane, &entry, 80).unwrap());
        assert!(text.contains("before") && text.contains("after"), "{text}");
        assert!(!text.contains("hidden"));
    }

    #[test]
    fn stale_reply_does_not_remove_new_request_or_record_failure() {
        let mut cache = Cache::default();
        let source = Source::entry(&entry("preview"));
        cache.bodies.insert(source.clone(), Body::default());
        cache.pending.insert(source.clone(), (0, 2));
        assert!(!cache.accept(&source, 0, 1, Err("stale failure".into())));
        assert_eq!(cache.pending.get(&source), Some(&(0, 2)));
        assert!(cache.bodies[&source].failure.is_none());
        assert_eq!(cache.counters.stale_replies, 1);
    }

    #[test]
    fn inconsistent_page_is_rejected_without_keeping_its_text() {
        let mut cache = Cache::default();
        let source = Source::entry(&entry("preview"));
        cache.bodies.insert(source.clone(), Body::default());
        cache.pending.insert(source.clone(), (0, 1));
        assert!(cache.accept(
            &source,
            0,
            1,
            Ok(TextPage {
                text: "abc".into(),
                total_bytes: 10,
                next_offset: None,
            })
        ));
        let body = &cache.bodies[&source];
        assert!(body.text.is_empty() && !body.complete);
        assert!(body.failure.is_some());
    }

    #[test]
    fn failed_load_keeps_the_preview_and_retries_after_a_pause() {
        let entry = entry("Preview text");
        let pane = Pane::default();
        rows(&pane, &entry, 80);
        let source = Source::entry(&entry);
        {
            let mut cache = pane.inline.cache.borrow_mut();
            assert_eq!(cache.wanted.len(), 1);
            cache.wanted.clear();
            cache.pending.insert(source.clone(), (0, 1));
            assert!(cache.accept(&source, 0, 1, Err("retained artifact missing".into())));
        }
        let text = shown(&rows(&pane, &entry, 80).unwrap());
        assert!(text.contains("Preview text"));
        assert!(text.contains("retained artifact missing"));
        assert!(pane.inline.cache.borrow().wanted.is_empty());
        pane.inline
            .cache
            .borrow_mut()
            .bodies
            .get_mut(&source)
            .unwrap()
            .failure
            .as_mut()
            .unwrap()
            .retry_at = Instant::now();
        rows(&pane, &entry, 80);
        assert_eq!(pane.inline.cache.borrow().wanted.len(), 1);
    }

    #[test]
    fn newest_reply_is_requested_first() {
        let pane = Pane::default();
        for sequence in 1..=3 {
            let mut entry = entry("preview");
            entry.sequence = sequence;
            rows(&pane, &entry, 80);
        }
        let cache = pane.inline.cache.borrow();
        let order: Vec<_> = cache
            .wanted
            .iter()
            .map(|(source, _)| source.sequence)
            .collect();
        assert_eq!(order, [3, 2, 1]);
    }

    #[test]
    fn accepted_page_survives_pane_epoch_change() {
        let mut app = App::new(100);
        let entry = entry("retained reply");
        app.select(entry.thread.clone());
        let source = Source::entry(&entry);
        {
            let mut cache = app.inline_cache.borrow_mut();
            cache.bodies.insert(source.clone(), Body::default());
            cache.want(source.clone(), 0);
            cache.want(source.clone(), 1);
            assert_eq!(cache.wanted.len(), 1);
            assert_eq!(cache.wanted[0].1, 1);
            cache.wanted.clear();
            cache.pending.insert(source.clone(), (0, 1));
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
        assert!(shown(&rows(app.pane().unwrap(), &entry, 80).unwrap()).contains("retained reply"));
    }

    #[test]
    fn forgetting_a_thread_drops_its_replies() {
        let pane = Pane::default();
        let entry = entry("preview");
        rows(&pane, &entry, 80);
        pane.inline.cache.borrow_mut().forget(&entry.thread);
        let cache = pane.inline.cache.borrow();
        assert!(cache.bodies.is_empty() && cache.wanted.is_empty());
    }
}
