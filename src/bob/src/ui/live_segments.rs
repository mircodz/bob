use bob_kernel::LiveAttempt;
use bob_ui::segmented::{
    render_markdown_chunk_continued, MarkdownChunk, MarkdownCursor, Sanitized, ScanState,
    SourceRow, TextCursor, MAX_MARKDOWN_CURSOR_BYTES, MAX_STYLED_BLOCK_BYTES,
};
use std::{collections::VecDeque, sync::Arc};

pub(super) const ROWS: usize = 64;
const TEXT_BYTES: usize = 8 * 1024;
const INPUT_BYTES: usize = 2048;
const WINDOW_BYTES: usize = 16 * 1024;
const CHECKPOINTS: usize = 32;
pub(super) const STEP_BYTES: usize = 8 * 1024;

#[derive(Clone)]
pub(super) struct Source {
    pub attempt: Arc<LiveAttempt>,
    pub reasoning: Option<usize>,
}
impl Source {
    pub fn text(&self) -> &str {
        self.reasoning.map_or(self.attempt.text.as_str(), |index| {
            self.attempt.reasoning[index].text.as_str()
        })
    }
}

enum Content {
    Live(Source),
    Complete(Arc<str>),
}
impl Content {
    fn text(&self) -> &str {
        match self {
            Self::Live(source) => source.text(),
            Self::Complete(text) => text,
        }
    }
    fn revision(&self) -> u64 {
        match self {
            Self::Live(source) => source.attempt.revision,
            Self::Complete(_) => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Target {
    #[default]
    Tail,
    Forward(usize, usize),
    Backward(usize, usize),
}

#[derive(Default)]
pub(super) struct Counters {
    pub compared_bytes: usize,
    pub scanned_bytes: usize,
    pub parsed_bytes: usize,
    pub builds: usize,
}

#[derive(Default)]
struct Window {
    rows: VecDeque<SourceRow>,
    bytes: usize,
}
impl Window {
    fn push(&mut self, mut row: SourceRow) {
        row.line.spans.shrink_to_fit();
        for span in &mut row.line.spans {
            if let std::borrow::Cow::Owned(text) = &mut span.content {
                text.shrink_to_fit();
            }
        }
        let bytes = row
            .line
            .spans
            .iter()
            .map(|span| span.content.len())
            .sum::<usize>();
        while !self.rows.is_empty() && (self.rows.len() == ROWS || self.bytes + bytes > TEXT_BYTES)
        {
            let old = self.rows.pop_front().unwrap();
            self.bytes -= old
                .line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>();
        }
        self.bytes += bytes;
        self.rows.push_back(row);
        while self.rows.len() > 1 && self.allocated_bytes() > WINDOW_BYTES {
            let old = self.rows.pop_front().unwrap();
            self.bytes -= old
                .line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>();
        }
    }
    fn would_evict(&self, row: &SourceRow, other: &Self) -> bool {
        let count = self.rows.len() + other.rows.len();
        let owned = self.allocated_bytes() + other.allocated_bytes()
            - (self.rows.capacity() + other.rows.capacity()) * std::mem::size_of::<SourceRow>();
        count != 0
            && (count >= ROWS
                || self.bytes
                    + other.bytes
                    + row
                        .line
                        .spans
                        .iter()
                        .map(|span| span.content.len())
                        .sum::<usize>()
                    > TEXT_BYTES
                || owned
                    + ROWS * std::mem::size_of::<SourceRow>()
                    + row.line.spans.capacity()
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
                    > WINDOW_BYTES)
    }
    fn retain_before(&mut self, offset: usize) {
        self.rows.retain(|row| row.source.end <= offset);
        self.bytes = self
            .rows
            .iter()
            .flat_map(|row| &row.line.spans)
            .map(|span| span.content.len())
            .sum();
    }
    fn allocated_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<SourceRow>()
            + self
                .rows
                .iter()
                .map(|row| {
                    row.line.spans.capacity() * std::mem::size_of::<ratatui::text::Span<'static>>()
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
                .sum::<usize>()
    }
}

struct Checkpoint {
    offset: usize,
    scan: ScanState,
    meaningful: bool,
    previous_blank: bool,
}
struct PendingText {
    page: Sanitized,
    scalar: usize,
    boundary: bool,
}
struct PendingMarkdown {
    cursor: Box<MarkdownCursor>,
    start: usize,
    end: usize,
    temporary: bool,
}

pub(super) struct Document {
    content: Content,
    incoming: Option<Content>,
    checked: usize,
    width: usize,
    target: Target,
    position: usize,
    scan: ScanState,
    block_start: usize,
    block_scan: ScanState,
    block: String,
    literal: Option<TextCursor>,
    pending_text: Option<PendingText>,
    pending_markdown: Option<PendingMarkdown>,
    tail_cursor: Option<TextCursor>,
    checkpoints: VecDeque<Checkpoint>,
    stable: Window,
    preview: Window,
    finished: bool,
    exhausted: bool,
    limited: bool,
    meaningful: bool,
    previous_blank: bool,
    block_ends_blank: bool,
    next: (usize, usize),
    pub counters: Counters,
}

impl Document {
    pub fn new(source: Source, width: usize, target: Target) -> Self {
        Self::from_content(Content::Live(source), width, target)
    }

    pub fn complete(text: &str, width: usize, target: Target) -> Self {
        Self::from_content(Content::Complete(Arc::from(text)), width, target)
    }

    fn from_content(content: Content, width: usize, target: Target) -> Self {
        Self {
            content,
            incoming: None,
            checked: 0,
            width,
            target,
            position: 0,
            scan: ScanState::default(),
            block_start: 0,
            block_scan: ScanState::default(),
            block: String::new(),
            literal: None,
            pending_text: None,
            pending_markdown: None,
            tail_cursor: None,
            checkpoints: VecDeque::new(),
            stable: Window::default(),
            preview: Window::default(),
            finished: false,
            exhausted: false,
            limited: false,
            meaningful: false,
            previous_blank: false,
            block_ends_blank: false,
            next: (0, 0),
            counters: Counters::default(),
        }
    }

    pub fn update(&mut self, source: Source, width: usize, target: Target) {
        if self.finished
            && self.incoming.is_none()
            && source.attempt.revision != self.content.revision()
        {
            self.incoming = Some(Content::Live(source));
            self.checked = 0;
        }
        if self.width != width || self.target != target {
            self.width = width;
            self.target = target;
            let offset = match target {
                Target::Tail => self
                    .preview
                    .rows
                    .back()
                    .or_else(|| self.stable.rows.back())
                    .map_or(self.content.text().len(), |row| row.source.start)
                    .saturating_sub(INPUT_BYTES),
                Target::Forward(offset, _) | Target::Backward(offset, _) => {
                    offset.saturating_sub(INPUT_BYTES)
                }
            };
            self.stable = Window::default();
            self.rewind(offset);
        }
    }

    fn rewind(&mut self, offset: usize) {
        let checkpoint = self
            .checkpoints
            .iter()
            .rev()
            .find(|checkpoint| checkpoint.offset <= offset);
        let (position, scan, meaningful, previous_blank) =
            checkpoint.map_or((0, ScanState::default(), false, false), |checkpoint| {
                (
                    checkpoint.offset,
                    checkpoint.scan.clone(),
                    checkpoint.meaningful,
                    checkpoint.previous_blank,
                )
            });
        self.position = position;
        self.scan = scan;
        self.block_start = position;
        self.next = (position, 0);
        self.block_scan = self.scan.clone();
        self.block.clear();
        self.literal = None;
        self.pending_text = None;
        self.pending_markdown = None;
        self.tail_cursor = None;
        self.stable.retain_before(position);
        self.meaningful = meaningful;
        self.previous_blank = previous_blank;
        self.block_ends_blank = previous_blank;
        self.preview = Window::default();
        self.checkpoints
            .retain(|checkpoint| checkpoint.offset <= position);
        self.finished = false;
        self.exhausted = false;
        self.limited = false;
    }

    fn validate(&mut self, budget: &mut usize) -> bool {
        let Some(incoming) = &self.incoming else {
            return true;
        };
        let old = self.content.text().as_bytes();
        let new = incoming.text().as_bytes();
        let limit = old.len().min(new.len());
        let end = limit.min(self.checked + *budget);
        let difference = old[self.checked..end]
            .iter()
            .zip(&new[self.checked..end])
            .position(|(old, new)| old != new)
            .map(|index| self.checked + index);
        let compared = difference.map_or(end, |offset| offset + 1) - self.checked;
        self.counters.compared_bytes += compared;
        *budget -= compared;
        self.checked = end;
        if difference.is_none() && end < limit {
            return false;
        }
        let prefix = difference.unwrap_or(limit);
        let append = prefix == old.len();
        let unchanged = append && old.len() == new.len();
        self.content = self.incoming.take().unwrap();
        if unchanged {
            return true;
        }
        if append {
            self.preview = Window::default();
            self.tail_cursor = None;
            if self
                .pending_markdown
                .as_ref()
                .is_some_and(|pending| pending.temporary)
            {
                self.pending_markdown = None;
            }
            self.finished =
                matches!(self.target, Target::Forward(..)) && self.stable.rows.len() >= ROWS;
            self.exhausted = false;
            self.limited = false;
        } else {
            self.rewind(prefix);
        }
        true
    }

    pub fn pending(&self) -> bool {
        self.incoming.is_some() || !self.finished
    }

    pub fn revision(&self) -> u64 {
        self.content.revision()
    }

    pub fn live_source(&self) -> Option<&Source> {
        match &self.content {
            Content::Live(source) => Some(source),
            Content::Complete(_) => None,
        }
    }

    pub fn meaningful(&self) -> bool {
        self.meaningful
    }

    pub fn next_anchor(&self) -> Option<(usize, usize)> {
        if self.exhausted {
            return None;
        }
        if self.finished && self.position == self.content.text().len() {
            if let Some(pending) = &self.pending_markdown {
                let page = pending.cursor.clone().page(1);
                if page.finished && page.rows.iter().all(|row| row.line.width() == 0) {
                    return None;
                }
            }
        }
        Some(self.next)
    }

    pub fn rows(&self) -> Vec<SourceRow> {
        let mut window = Window::default();
        for row in self.stable.rows.iter().chain(&self.preview.rows) {
            window.push(row.clone());
        }
        if self.exhausted && self.literal.is_none() {
            while window.rows.back().is_some_and(|row| row.line.width() == 0) {
                window.rows.pop_back();
            }
        }
        window.rows.into_iter().collect()
    }

    fn select(&mut self, row: SourceRow, temporary: bool, styled: bool) {
        if self.finished {
            self.limited = true;
            return;
        }
        let anchor = (row.source.start, row.ordinal);
        match self.target {
            Target::Forward(offset, ordinal)
                if row.source.end <= offset
                    || (styled && row.source.start == offset && row.ordinal_end <= ordinal) =>
            {
                return
            }
            Target::Backward(offset, ordinal) if anchor > (offset, ordinal) => {
                self.finished = true;
                self.limited = true;
                return;
            }
            _ => {}
        }
        if matches!(self.target, Target::Forward(..))
            && self.stable.would_evict(&row, &self.preview)
        {
            self.finished = true;
            self.limited = true;
            return;
        }
        self.next = if styled {
            (row.source.start, row.ordinal_end)
        } else {
            (row.source.end, 0)
        };
        if temporary {
            self.preview.push(row);
        } else {
            self.stable.push(row);
        }
        if matches!(self.target, Target::Forward(..))
            && self.stable.rows.len() + self.preview.rows.len() >= ROWS
        {
            self.finished = true;
        }
    }

    fn next_block(&mut self) {
        self.block_start = self.position;
        self.block_scan = self.scan.clone();
        self.block.clear();
        self.literal = None;
        self.previous_blank = self.block_ends_blank;
        if self.position == self.content.text().len() {
            self.finished = true;
            self.exhausted = !self.limited;
        }
        if self
            .checkpoints
            .back()
            .is_none_or(|checkpoint| self.position >= checkpoint.offset + INPUT_BYTES)
        {
            self.checkpoints.push_back(Checkpoint {
                offset: self.position,
                scan: self.scan.clone(),
                meaningful: self.meaningful,
                previous_blank: self.previous_blank,
            });
            while self.checkpoints.len() > CHECKPOINTS {
                self.checkpoints.pop_front();
            }
        }
    }

    fn literal_block(&mut self) {
        self.position = self.block_start;
        self.scan = self.block_scan.clone();
        self.block.clear();
        self.block_ends_blank = self.previous_blank;
        self.literal = Some(TextCursor::new(self.block_start, self.width));
        self.pending_text = None;
        self.pending_markdown = None;
        self.preview = Window::default();
    }

    fn markdown(&mut self, temporary: bool) {
        self.counters.parsed_bytes += self.block.len();
        match render_markdown_chunk_continued(
            &self.block,
            self.block_start,
            self.width,
            self.previous_blank,
            temporary,
        ) {
            MarkdownChunk::Styled(cursor)
                if cursor.allocated_bytes() <= MAX_MARKDOWN_CURSOR_BYTES =>
            {
                self.pending_markdown = Some(PendingMarkdown {
                    cursor,
                    start: self.block_start,
                    end: self.position,
                    temporary,
                });
            }
            _ => self.literal_block(),
        }
    }

    pub fn step(&mut self, budget: &mut usize) {
        if !self.pending() {
            return;
        }
        if !self.validate(budget) || *budget == 0 || self.finished {
            return;
        }
        self.counters.builds += 1;
        let mut rows_left = ROWS;
        let mut operations = 0;
        while !self.finished && rows_left > 0 && operations < 64 {
            operations += 1;
            let row_limit = if matches!(self.target, Target::Forward(..)) {
                rows_left.min(
                    ROWS.saturating_sub(self.stable.rows.len() + self.preview.rows.len())
                        .max(1),
                )
            } else {
                rows_left
            };
            if let Some(mut pending) = self.pending_markdown.take() {
                let page = pending.cursor.page(row_limit);
                rows_left -= page.rows.len();
                for mut row in page.rows {
                    row.source = pending.start..pending.end;
                    self.select(row, pending.temporary, true);
                }
                if page.finished {
                    if pending.temporary {
                        self.finished = true;
                        self.exhausted = !self.limited;
                    } else {
                        self.block_ends_blank = pending.cursor.ends_blank();
                        self.next_block();
                    }
                } else {
                    self.pending_markdown = Some(pending);
                }
                continue;
            }
            if let Some(mut pending) = self.pending_text.take() {
                let page = self.literal.as_mut().unwrap().push_sanitized(
                    &pending.page,
                    pending.scalar,
                    pending.boundary,
                    row_limit,
                );
                rows_left -= page.rows.len();
                pending.scalar = page.next_scalar;
                if let Some(last) = page.rows.last() {
                    self.block_ends_blank = last.line.width() == 0;
                }
                for row in page.rows {
                    self.select(row, false, false);
                }
                if page.finished {
                    self.next_block();
                } else if pending.scalar < pending.page.offsets.len() - 1 || pending.boundary {
                    self.pending_text = Some(pending);
                }
                continue;
            }
            if self.position == self.content.text().len() {
                if let Some(literal) = &self.literal {
                    let mut cursor = self.tail_cursor.take().unwrap_or_else(|| literal.clone());
                    let empty = self.scan.scan("", self.position);
                    let page = cursor.push_sanitized(&empty, 0, true, row_limit);
                    rows_left -= page.rows.len();
                    for row in page.rows {
                        self.select(row, true, false);
                    }
                    self.finished |= page.finished;
                    self.exhausted |= page.finished && !self.limited;
                    if !page.finished {
                        self.tail_cursor = Some(cursor);
                    }
                } else if self.block.is_empty() {
                    self.finished = true;
                    self.exhausted = true;
                } else {
                    self.markdown(true);
                }
                continue;
            }
            if *budget < 8 {
                break;
            }
            let text = self.content.text();
            let mut end = text.len().min(self.position + INPUT_BYTES.min(*budget));
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == self.position {
                break;
            }
            if let Some(newline) = text[self.position..end].find('\n') {
                end = self.position + newline + 1;
            }
            let page = self.scan.scan(&text[self.position..end], self.position);
            let scanned = page.consumed;
            *budget -= scanned;
            self.counters.scanned_bytes += scanned;
            self.position += page.consumed;
            self.meaningful |= page.text.chars().any(|c| !c.is_whitespace());
            let boundary =
                self.scan.at_block_boundary() && page.block_boundaries().next().is_some();
            if self.literal.is_some() {
                self.pending_text = Some(PendingText {
                    page,
                    scalar: 0,
                    boundary,
                });
            } else if self.block.len() + page.text.len() > MAX_STYLED_BLOCK_BYTES {
                self.literal_block();
            } else {
                self.block.push_str(&page.text);
                if boundary {
                    if self.block.trim().is_empty() {
                        self.next_block();
                    } else {
                        self.markdown(false);
                    }
                }
            }
        }
    }

    #[cfg(test)]
    pub fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.block.capacity()
            + self.stable.allocated_bytes()
            + self.preview.allocated_bytes()
            + self.checkpoints.capacity() * std::mem::size_of::<Checkpoint>()
            + self
                .pending_text
                .as_ref()
                .map_or(0, |pending| pending.page.allocated_bytes())
            + self
                .pending_markdown
                .as_ref()
                .map_or(0, |pending| pending.cursor.allocated_bytes())
            + self.literal.as_ref().map_or(0, TextCursor::allocated_bytes)
            + self
                .tail_cursor
                .as_ref()
                .map_or(0, TextCursor::allocated_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bob_kernel::{ModelActivity, OperationId, RunId, ThreadId};

    fn source(text: impl Into<String>, revision: u64) -> Source {
        Source {
            attempt: Arc::new(LiveAttempt {
                thread: ThreadId("root".into()),
                run: RunId("run".into()),
                operation: OperationId("live".into()),
                revision,
                activity: ModelActivity::Answering,
                text: text.into(),
                reasoning: Vec::new(),
                truncated: false,
                started_at: std::time::Instant::now(),
                stopping: false,
                settled_sequence: None,
                error: None,
            }),
            reasoning: None,
        }
    }

    fn finish(document: &mut Document) {
        for _ in 0..20000 {
            if !document.pending() {
                return;
            }
            let before = document.counters.scanned_bytes + document.counters.compared_bytes;
            let mut budget = STEP_BYTES;
            document.step(&mut budget);
            let work = document.counters.scanned_bytes + document.counters.compared_bytes - before;
            assert!(work <= STEP_BYTES, "unbounded source work: {work}");
            assert!(document.rows().len() <= ROWS);
            assert!(
                document.allocated_bytes() <= 224 * 1024,
                "document cache: {}",
                document.allocated_bytes()
            );
        }
        panic!("layout did not settle");
    }

    fn text(document: &Document) -> String {
        document
            .rows()
            .iter()
            .map(|row| row.line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn appended_segments_do_not_reparse_completed_prefixes() {
        let mut input = String::new();
        let mut document = Document::new(source("", 0), 80, Target::Tail);
        finish(&mut document);
        for revision in 1..=160 {
            input.push_str(&format!(
                "Paragraph {revision} with **stable** styling.\n\n"
            ));
            document.update(source(input.clone(), revision), 80, Target::Tail);
            finish(&mut document);
        }
        assert_eq!(document.counters.parsed_bytes, input.len());
        assert!(text(&document).contains("Paragraph 160"));
    }

    #[test]
    fn replaced_prefix_and_shortened_revision_invalidate_cached_rows() {
        let original = format!(
            "Private old prefix\n\n{}",
            "unchanged paragraph\n\n".repeat(180)
        );
        let mut document = Document::new(source(original, 1), 60, Target::Tail);
        finish(&mut document);
        document.update(
            source("Replacement 界 prefix\n\nFinal text", 2),
            60,
            Target::Tail,
        );
        finish(&mut document);
        let rendered = text(&document);
        assert!(rendered.contains("Replacement 界 prefix"));
        assert!(rendered.contains("Final text"));
        assert!(!rendered.contains("Private") && !rendered.contains("unchanged"));
        assert!(document
            .rows()
            .iter()
            .all(|row| row.source.end <= document.content.text().len()));
    }

    #[test]
    fn retained_window_is_independent_of_row_allocation_capacity() {
        let mut compact = Window::default();
        let mut oversized = Window::default();
        for offset in 0..ROWS * 2 {
            let row = SourceRow {
                source: offset..offset + 1,
                ordinal: 0,
                ordinal_end: 0,
                line: ratatui::text::Line::raw("x".to_owned()),
                grapheme_fragment: false,
            };
            compact.push(row.clone());
            let mut row = row;
            row.line.spans.reserve(128);
            row.line.spans[0].content.to_mut().reserve(1024);
            oversized.push(row);
            assert!(compact.allocated_bytes() <= WINDOW_BYTES);
            assert!(oversized.allocated_bytes() <= WINDOW_BYTES);
            assert_eq!(oversized.rows, compact.rows);
        }
        assert_eq!(compact.rows.len(), ROWS);
        assert_eq!(compact.rows.front().unwrap().source.start, ROWS);
        assert_eq!(compact.rows.back().unwrap().source.end, ROWS * 2);
    }

    #[test]
    fn literal_pages_preserve_graphemes_multibyte_and_osc_state() {
        let input = format!(
            "{}e\u{301}👩\u{200d}💻界\u{1b}]hidden{}\u{7}VISIBLE",
            "x".repeat(MAX_STYLED_BLOCK_BYTES * 2 - 1),
            "secret".repeat(900)
        );
        let mut document = Document::new(source(input.clone(), 1), 1, Target::Tail);
        finish(&mut document);
        let mut cursor = TextCursor::new(0, 1);
        let mut expected = Window::default();
        while !cursor.is_finished() {
            let page = cursor.push(&input.as_bytes()[cursor.position()..], true, ROWS);
            for row in page.rows {
                expected.push(row);
            }
        }
        assert_eq!(
            document.rows(),
            expected.rows.into_iter().collect::<Vec<_>>()
        );
        let rendered = text(&document);
        assert!(rendered.contains("e\u{301}") && rendered.contains("👩\u{200d}💻"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn appended_grapheme_and_split_osc_resume_without_finalizing_source() {
        let prefix = format!("{}e", "x".repeat(MAX_STYLED_BLOCK_BYTES * 2));
        let mut document = Document::new(source(prefix.clone(), 1), 1, Target::Tail);
        finish(&mut document);
        let next = format!("{prefix}\u{301}\u{1b}]private");
        document.update(source(next.clone(), 2), 1, Target::Tail);
        finish(&mut document);
        document.update(source(format!("{next} hidden\u{7}ok"), 3), 1, Target::Tail);
        finish(&mut document);
        let rendered = text(&document);
        assert!(rendered.contains("e\u{301}"));
        assert!(!rendered.contains("private") && !rendered.contains("hidden"));
        assert!(rendered.ends_with("o\nk"));
    }

    #[test]
    fn split_fence_keeps_interior_literal_until_real_boundary() {
        let first = format!(
            "```rust\n{}\n\n# inside code\n`",
            "x".repeat(MAX_STYLED_BLOCK_BYTES * 2)
        );
        let mut document = Document::new(source(first.clone(), 1), 80, Target::Tail);
        finish(&mut document);
        let complete = format!("{first}``\n\n**After fence**");
        document.update(source(complete, 2), 80, Target::Tail);
        finish(&mut document);
        let rendered = text(&document);
        assert!(rendered.contains("# inside code"));
        assert!(rendered.contains("After fence") && !rendered.contains("**After fence**"));
    }

    #[test]
    fn width_changes_keep_source_byte_and_markdown_ordinal_anchors() {
        let input =
            "**A paragraph with repeated words that wraps across multiple rows.**\n\n".repeat(80);
        let mut document = Document::new(source(input.clone(), 1), 50, Target::Tail);
        finish(&mut document);
        let anchor = document.rows()[10].clone();
        document.update(
            source(input.clone(), 1),
            17,
            Target::Forward(anchor.source.start, 0),
        );
        finish(&mut document);
        let rows = document.rows();
        assert_eq!(rows.first().unwrap().source.start, anchor.source.start);
        assert!(rows
            .windows(2)
            .all(|rows| (rows[0].source.start, rows[0].ordinal)
                < (rows[1].source.start, rows[1].ordinal)));
        assert!(rows
            .iter()
            .all(|row| input.is_char_boundary(row.source.start)
                && input.is_char_boundary(row.source.end)));
    }

    #[test]
    fn unchanged_revision_builds_nothing_and_changed_equal_text_reuses_layout() {
        let input = "A **stable** unfinished block";
        let mut document = Document::new(source(input, 1), 80, Target::Tail);
        finish(&mut document);
        let builds = document.counters.builds;
        let parsed = document.counters.parsed_bytes;
        for _ in 0..100 {
            document.update(source(input, 1), 80, Target::Tail);
            document.step(&mut STEP_BYTES.clone());
        }
        assert_eq!(document.counters.builds, builds);
        assert_eq!(document.counters.parsed_bytes, parsed);
        document.update(source(input, 2), 80, Target::Tail);
        finish(&mut document);
        assert_eq!(document.counters.builds, builds);
        assert_eq!(document.counters.parsed_bytes, parsed);
    }

    #[test]
    fn megabody_cache_and_each_continuation_are_bounded() {
        for input in ["x".repeat(512 * 1024), "*styled* content\n\n".repeat(20000)] {
            let mut document = Document::new(source(input, 1), 7, Target::Tail);
            finish(&mut document);
            assert!(document.checkpoints.len() <= CHECKPOINTS);
            assert!(document.stable.rows.len() <= ROWS && document.preview.rows.len() <= ROWS);
            assert!(document.allocated_bytes() <= 224 * 1024);
        }
    }

    #[test]
    fn forward_windows_keep_every_literal_byte_readable() {
        let input = "xyz界e\u{301}".repeat(4000);
        let source = source(input.clone(), 1);
        let mut document = Document::new(source.clone(), 23, Target::Forward(0, 0));
        let mut collected = String::new();
        let mut previous = (0, 0);
        for _ in 0..1000 {
            finish(&mut document);
            for row in document.rows() {
                collected.push_str(&row.line.to_string());
            }
            let Some(next) = document.next_anchor() else {
                assert_eq!(collected, input);
                return;
            };
            assert!(next > previous, "continuation did not advance: {next:?}");
            previous = next;
            document.update(source.clone(), 23, Target::Forward(next.0, next.1));
        }
        panic!("forward traversal did not finish");
    }

    #[test]
    fn changing_snapshots_do_not_starve_inflight_prefix_validation() {
        let initial = "x".repeat(STEP_BYTES * 32);
        let mut document = Document::new(source(initial.clone(), 1), 80, Target::Tail);
        finish(&mut document);
        document.update(source(format!("{initial} first"), 2), 80, Target::Tail);
        for _ in 0..32 {
            let mut budget = STEP_BYTES;
            document.step(&mut budget);
        }
        assert_eq!(document.revision(), 2);
        assert!(document.pending());
        document.update(source(format!("{initial} second"), 3), 80, Target::Tail);
        let mut budget = STEP_BYTES;
        document.step(&mut budget);
        assert!(
            text(&document).contains("first"),
            "an accepted layout must advance before validating the next snapshot"
        );
        document.update(source(format!("{initial} second"), 3), 80, Target::Tail);
        finish(&mut document);
        assert_eq!(document.revision(), 3);
        assert!(text(&document).contains("second"));
    }

    #[test]
    fn styled_wide_rows_keep_content_within_prepared_budget() {
        let input = format!("{}FINALMARKER", "**bold** *italic* ".repeat(200));
        let mut document = Document::new(source(input, 1), 4096, Target::Tail);
        finish(&mut document);
        assert!(text(&document).contains("FINALMARKER"));
        assert!(document
            .rows()
            .iter()
            .any(|row| row.line.spans.iter().any(|span| span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD))));
        assert!(document.allocated_bytes() <= 224 * 1024);
    }

    #[test]
    fn long_handoff_coordinates_keep_later_paragraph_visible() {
        let input: String = (0..60).map(|index| format!("Paragraph {index:02} with **stable** styling and enough words to wrap across several screen rows.\n\n")).collect();
        assert!(input.len() > 4096 && input.len() < bob_ui::segmented::MAX_INPUT_BYTES);
        let mut document = Document::new(source(input.clone(), 1), 35, Target::Tail);
        finish(&mut document);
        let anchor = document
            .rows()
            .into_iter()
            .find(|row| row.line.to_string().contains("Paragraph 55"))
            .unwrap();
        assert!(anchor.source.start > 0);
        let mut scan = ScanState::default();
        let mut complete = scan.scan(&input, 0);
        complete.set_complete(true);
        let durable = complete.layout(35, anchor.source.start, anchor.ordinal, ROWS, false);
        assert_eq!(
            durable.rows.first().unwrap().line.to_string(),
            anchor.line.to_string()
        );
        assert!(durable
            .rows
            .iter()
            .take(6)
            .any(|row| row.line.to_string().contains("Paragraph 55")));
    }

    #[test]
    fn final_complete_block_has_no_phantom_continuation() {
        let mut document = Document::new(
            source(format!("{}\n\n", "x".repeat(ROWS)), 1),
            1,
            Target::Forward(0, 0),
        );
        finish(&mut document);
        assert_eq!(
            document
                .rows()
                .iter()
                .map(|row| row.line.to_string())
                .collect::<String>(),
            "x".repeat(ROWS)
        );
        assert!(document.next_anchor().is_none());
    }

    #[test]
    fn completed_block_spacing_matches_whole_markdown() {
        let input = "First paragraph.\n\nSecond paragraph.\n\n# Heading\n\n```rust\nlet value = 2;\n```\n\nLast paragraph.";
        let mut document = Document::new(source(input, 1), 100, Target::Tail);
        finish(&mut document);
        let expected = bob_ui::markdown::render_markdown(input);
        assert_eq!(
            document
                .rows()
                .iter()
                .map(|row| row.line.to_string())
                .collect::<Vec<_>>(),
            expected.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
    }

    #[test]
    fn resizing_tail_keeps_visible_prefix_before_blank_suffix() {
        let source = source(format!("Visible prefix\n\n{}", " \n\n".repeat(4000)), 1);
        let mut document = Document::new(source.clone(), 80, Target::Tail);
        finish(&mut document);
        assert!(text(&document).contains("Visible prefix"));
        document.update(source, 40, Target::Tail);
        finish(&mut document);
        assert!(text(&document).contains("Visible prefix"));
    }

    #[test]
    fn checkpoint_restores_meaningful_prefix_after_width_change() {
        for prefix in ["Visible reasoning\n\n", "\n\n"] {
            let input = format!("{prefix}{}", " \n\n".repeat(4000));
            let source = source(input, 1);
            let mut document = Document::new(source.clone(), 80, Target::Tail);
            finish(&mut document);
            let meaningful = document.meaningful();
            assert_eq!(meaningful, prefix.starts_with("Visible"));
            document.update(
                source.clone(),
                40,
                Target::Forward(source.text().len().saturating_sub(100), 0),
            );
            assert!(document.position > 0);
            finish(&mut document);
            assert_eq!(document.meaningful(), meaningful);
        }
    }
}
