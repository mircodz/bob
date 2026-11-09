use bob_kernel::LiveAttempt;
use bob_ui::segmented::{
    render_markdown_chunk_continued, MarkdownChunk, MarkdownCursor, Sanitized, ScanState,
    SourceRow, TextCursor, MAX_MARKDOWN_CURSOR_BYTES, MAX_STYLED_BLOCK_BYTES,
};
use std::{collections::VecDeque, sync::Arc};

/// Per-step work limits only: layout stays incremental so one frame never
/// parses a whole long reply, but every row is kept and nothing is paged out.
const ROWS: usize = 512;
const INPUT_BYTES: usize = 2048;
const CHECKPOINTS: usize = 32;
pub(super) const STEP_BYTES: usize = 32 * 1024;

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
}
impl Window {
    fn push(&mut self, mut row: SourceRow) {
        row.line.spans.shrink_to_fit();
        for span in &mut row.line.spans {
            if let std::borrow::Cow::Owned(text) = &mut span.content {
                text.shrink_to_fit();
            }
        }
        self.rows.push_back(row);
    }
    fn retain_before(&mut self, offset: usize) {
        self.rows.retain(|row| row.source.end <= offset);
    }
}

struct Checkpoint {
    offset: usize,
    scan: ScanState,
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
    previous_blank: bool,
    block_ends_blank: bool,
    pub counters: Counters,
}

impl Document {
    pub fn new(source: Source, width: usize) -> Self {
        Self::from_content(Content::Live(source), width)
    }

    pub fn complete(text: &str, width: usize) -> Self {
        Self::from_content(Content::Complete(Arc::from(text)), width)
    }

    fn from_content(content: Content, width: usize) -> Self {
        Self {
            content,
            incoming: None,
            checked: 0,
            width,
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
            previous_blank: false,
            block_ends_blank: false,
            counters: Counters::default(),
        }
    }

    pub fn update(&mut self, source: Source, width: usize) {
        if self.finished
            && self.incoming.is_none()
            && source.attempt.revision != self.content.revision()
        {
            self.incoming = Some(Content::Live(source));
            self.checked = 0;
        }
        if self.width != width {
            self.width = width;
            self.stable = Window::default();
            self.rewind(0);
        }
    }

    fn rewind(&mut self, offset: usize) {
        let checkpoint = self
            .checkpoints
            .iter()
            .rev()
            .find(|checkpoint| checkpoint.offset <= offset);
        let (position, scan, previous_blank) =
            checkpoint.map_or((0, ScanState::default(), false), |checkpoint| {
                (
                    checkpoint.offset,
                    checkpoint.scan.clone(),
                    checkpoint.previous_blank,
                )
            });
        self.position = position;
        self.scan = scan;
        self.block_start = position;
        self.block_scan = self.scan.clone();
        self.block.clear();
        self.literal = None;
        self.pending_text = None;
        self.pending_markdown = None;
        self.tail_cursor = None;
        self.stable.retain_before(position);
        self.previous_blank = previous_blank;
        self.block_ends_blank = previous_blank;
        self.preview = Window::default();
        self.checkpoints
            .retain(|checkpoint| checkpoint.offset <= position);
        self.finished = false;
        self.exhausted = false;
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
            self.finished = false;
            self.exhausted = false;
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

    pub fn rows(&self) -> Vec<SourceRow> {
        let mut rows: Vec<_> = self
            .stable
            .rows
            .iter()
            .chain(&self.preview.rows)
            .cloned()
            .collect();
        if self.exhausted && self.literal.is_none() {
            while rows.last().is_some_and(|row| row.line.width() == 0) {
                rows.pop();
            }
        }
        rows
    }

    fn select(&mut self, row: SourceRow, temporary: bool) {
        if temporary {
            self.preview.push(row);
        } else {
            self.stable.push(row);
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
            self.exhausted = true;
        }
        if self
            .checkpoints
            .back()
            .is_none_or(|checkpoint| self.position >= checkpoint.offset + INPUT_BYTES)
        {
            self.checkpoints.push_back(Checkpoint {
                offset: self.position,
                scan: self.scan.clone(),
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
            if let Some(mut pending) = self.pending_markdown.take() {
                let page = pending.cursor.page(rows_left);
                rows_left -= page.rows.len();
                for mut row in page.rows {
                    row.source = pending.start..pending.end;
                    self.select(row, pending.temporary);
                }
                if page.finished {
                    if pending.temporary {
                        self.finished = true;
                        self.exhausted = true;
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
                    rows_left,
                );
                rows_left -= page.rows.len();
                pending.scalar = page.next_scalar;
                if let Some(last) = page.rows.last() {
                    self.block_ends_blank = last.line.width() == 0;
                }
                for row in page.rows {
                    self.select(row, false);
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
                    let page = cursor.push_sanitized(&empty, 0, true, rows_left);
                    rows_left -= page.rows.len();
                    for row in page.rows {
                        self.select(row, true);
                    }
                    self.finished |= page.finished;
                    self.exhausted |= page.finished;
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
                compacting: false,
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
        let mut document = Document::new(source("", 0), 80);
        finish(&mut document);
        for revision in 1..=160 {
            input.push_str(&format!(
                "Paragraph {revision} with **stable** styling.\n\n"
            ));
            document.update(source(input.clone(), revision), 80);
            finish(&mut document);
        }
        assert_eq!(document.counters.parsed_bytes, input.len());
        assert!(text(&document).contains("Paragraph 1 "));
        assert!(text(&document).contains("Paragraph 160"));
    }

    #[test]
    fn replaced_prefix_and_shortened_revision_invalidate_cached_rows() {
        let original = format!(
            "Private old prefix\n\n{}",
            "unchanged paragraph\n\n".repeat(180)
        );
        let mut document = Document::new(source(original, 1), 60);
        finish(&mut document);
        document.update(source("Replacement 界 prefix\n\nFinal text", 2), 60);
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
    fn literal_pages_preserve_graphemes_multibyte_and_osc_state() {
        let input = format!(
            "{}e\u{301}👩\u{200d}💻界\u{1b}]hidden{}\u{7}VISIBLE",
            "x".repeat(MAX_STYLED_BLOCK_BYTES * 2 - 1),
            "secret".repeat(900)
        );
        let mut document = Document::new(source(input.clone(), 1), 1);
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
        let mut document = Document::new(source(prefix.clone(), 1), 1);
        finish(&mut document);
        let next = format!("{prefix}\u{301}\u{1b}]private");
        document.update(source(next.clone(), 2), 1);
        finish(&mut document);
        document.update(source(format!("{next} hidden\u{7}ok"), 3), 1);
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
        let mut document = Document::new(source(first.clone(), 1), 80);
        finish(&mut document);
        let complete = format!("{first}``\n\n**After fence**");
        document.update(source(complete, 2), 80);
        finish(&mut document);
        let rendered = text(&document);
        assert!(rendered.contains("# inside code"));
        assert!(rendered.contains("After fence") && !rendered.contains("**After fence**"));
    }

    #[test]
    fn width_changes_relayout_every_row() {
        let input =
            "**A paragraph with repeated words that wraps across multiple rows.**\n\n".repeat(80);
        let mut document = Document::new(source(input.clone(), 1), 50);
        finish(&mut document);
        let wide = document.rows().len();
        document.update(source(input.clone(), 1), 17);
        finish(&mut document);
        let rows = document.rows();
        assert!(rows.len() > wide);
        assert_eq!(rows.first().unwrap().source.start, 0);
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
        let mut document = Document::new(source(input, 1), 80);
        finish(&mut document);
        let builds = document.counters.builds;
        let parsed = document.counters.parsed_bytes;
        for _ in 0..100 {
            document.update(source(input, 1), 80);
            document.step(&mut STEP_BYTES.clone());
        }
        assert_eq!(document.counters.builds, builds);
        assert_eq!(document.counters.parsed_bytes, parsed);
        document.update(source(input, 2), 80);
        finish(&mut document);
        assert_eq!(document.counters.builds, builds);
        assert_eq!(document.counters.parsed_bytes, parsed);
    }

    #[test]
    fn megabody_keeps_every_row_and_bounded_checkpoints() {
        let literal = "x".repeat(512 * 1024);
        let mut document = Document::new(source(literal.clone(), 1), 7);
        finish(&mut document);
        assert!(document.checkpoints.len() <= CHECKPOINTS);
        assert_eq!(
            document
                .rows()
                .iter()
                .map(|row| row.line.to_string())
                .collect::<String>(),
            literal
        );
        let mut document = Document::new(source("*styled* content\n\n".repeat(20000), 1), 80);
        finish(&mut document);
        assert!(document.checkpoints.len() <= CHECKPOINTS);
        assert_eq!(
            document
                .rows()
                .iter()
                .filter(|row| row.line.to_string().trim() == "styled content")
                .count(),
            20000
        );
    }

    #[test]
    fn literal_text_renders_every_byte_in_order() {
        let input = "xyz界e\u{301}".repeat(4000);
        let mut document = Document::new(source(input.clone(), 1), 23);
        finish(&mut document);
        let collected: String = document
            .rows()
            .iter()
            .map(|row| row.line.to_string())
            .collect();
        assert_eq!(collected, input);
    }

    #[test]
    fn changing_snapshots_do_not_starve_inflight_prefix_validation() {
        let initial = "x".repeat(STEP_BYTES * 32);
        let mut document = Document::new(source(initial.clone(), 1), 80);
        finish(&mut document);
        document.update(source(format!("{initial} first"), 2), 80);
        for _ in 0..32 {
            let mut budget = STEP_BYTES;
            document.step(&mut budget);
        }
        assert_eq!(document.revision(), 2);
        assert!(document.pending());
        document.update(source(format!("{initial} second"), 3), 80);
        let mut budget = STEP_BYTES;
        document.step(&mut budget);
        assert!(
            text(&document).contains("first"),
            "an accepted layout must advance before validating the next snapshot"
        );
        document.update(source(format!("{initial} second"), 3), 80);
        finish(&mut document);
        assert_eq!(document.revision(), 3);
        assert!(text(&document).contains("second"));
    }

    #[test]
    fn styled_wide_rows_keep_content_within_prepared_budget() {
        let input = format!("{}FINALMARKER", "**bold** *italic* ".repeat(200));
        let mut document = Document::new(source(input, 1), 4096);
        finish(&mut document);
        assert!(text(&document).contains("FINALMARKER"));
        assert!(document
            .rows()
            .iter()
            .any(|row| row.line.spans.iter().any(|span| span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD))));
    }

    #[test]
    fn long_handoff_coordinates_keep_later_paragraph_visible() {
        let input: String = (0..60).map(|index| format!("Paragraph {index:02} with **stable** styling and enough words to wrap across several screen rows.\n\n")).collect();
        assert!(input.len() > 4096 && input.len() < bob_ui::segmented::MAX_INPUT_BYTES);
        let mut document = Document::new(source(input.clone(), 1), 35);
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
    fn final_complete_block_keeps_every_row() {
        let mut document = Document::new(source(format!("{}\n\n", "x".repeat(ROWS * 2)), 1), 1);
        finish(&mut document);
        assert_eq!(
            document
                .rows()
                .iter()
                .map(|row| row.line.to_string())
                .collect::<String>(),
            "x".repeat(ROWS * 2)
        );
    }

    #[test]
    fn completed_block_spacing_matches_whole_markdown() {
        let input = "First paragraph.\n\nSecond paragraph.\n\n# Heading\n\n```rust\nlet value = 2;\n```\n\nLast paragraph.";
        let mut document = Document::new(source(input, 1), 100);
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
    fn resizing_keeps_visible_prefix_before_blank_suffix() {
        let source = source(format!("Visible prefix\n\n{}", " \n\n".repeat(4000)), 1);
        let mut document = Document::new(source.clone(), 80);
        finish(&mut document);
        assert!(text(&document).contains("Visible prefix"));
        document.update(source, 40);
        finish(&mut document);
        assert!(text(&document).contains("Visible prefix"));
    }
}
