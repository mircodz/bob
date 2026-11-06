//! Bounded layout primitives for a caller-owned source/viewport cache.
//!
//! A text checkpoint is the complete cloned cursor, not just its byte position.
//! Construct a fresh cursor only at a known clean boundary (normally byte zero).
//! An arbitrary tail cannot be safely sanitized without a preceding checkpoint:
//! an OSC/DCS string may have started arbitrarily far back. Index such prefixes
//! incrementally, displaying a placeholder rather than guessing their state.

use std::collections::VecDeque;
use std::ops::Range;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

#[cfg(test)]
use super::markdown::render_markdown;
use super::markdown::render_markdown_continued;
use super::sanitize::safe_display_text;
use super::theme::Palette;

pub const MAX_INPUT_BYTES: usize = 16 * 1024;
pub const MAX_PAGE_ROWS: usize = 256;
pub const MAX_PAGE_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_ROW_BYTES: usize = 4096;
pub const MAX_ROW_SPANS: usize = 128;
pub const MAX_GRAPHEME_BYTES: usize = 256;
pub const MAX_WIDTH: usize = 4096;
pub const MAX_STYLED_BLOCK_BYTES: usize = MAX_INPUT_BYTES;
pub const MAX_MARKDOWN_CURSOR_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceRow {
    /// Original byte extent, including intervening stripped controls. Markdown
    /// rows instead share their complete block's conservative source extent.
    pub source: Range<usize>,
    /// Width-independent rendered byte start within a styled source block.
    /// Hard line breaks count as one byte; literal rows use zero.
    pub ordinal: usize,
    pub ordinal_end: usize,
    pub line: Line<'static>,
    /// An overlong grapheme was split to enforce the bounded-memory contract.
    /// Every scalar is retained, but this row is not an atomic grapheme layout.
    pub grapheme_fragment: bool,
}

#[derive(Debug)]
pub struct TextPage {
    /// Bytes accepted from this call's input; resend only the unconsumed suffix.
    /// Accepted bytes can still be buffered in the returned cursor checkpoint.
    pub consumed: usize,
    pub rows: Vec<SourceRow>,
    pub finished: bool,
}

/// Resumable literal layout. Width is clamped to 1..=MAX_WIDTH. Wrapping is
/// grapheme-based, not word-based; tabs advance to a four-column stop, capped
/// to the viewport width. A grapheme wider than the viewport remains intact.
///
/// State is bounded independently of source length: a partial UTF-8 scalar,
/// sanitizer state, one <=256-byte grapheme, one <=4096-byte row, and at most
/// two ready rows. Invalid UTF-8 becomes U+FFFD. Incomplete UTF-8 is retained
/// until more input or finalization. Oversized graphemes are losslessly split
/// into scalar-boundary fragments and explicitly marked on affected rows.
#[derive(Clone, Debug)]
pub struct TextCursor {
    position: usize,
    utf8: [u8; 4],
    utf8_len: usize,
    utf8_start: usize,
    sanitizer: Sanitizer,
    layout: WrapState,
    ended: bool,
}

impl TextCursor {
    pub fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.layout.allocated_bytes()
    }

    pub fn new(source_start: usize, width: usize) -> Self {
        Self {
            position: source_start,
            utf8: [0; 4],
            utf8_len: 0,
            utf8_start: source_start,
            sanitizer: Sanitizer::default(),
            layout: WrapState::new(source_start, width),
            ended: false,
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn is_finished(&self) -> bool {
        self.ended && self.layout.ready.is_empty()
    }

    /// Consume at most MAX_INPUT_BYTES and emit at most min(row_limit, 256)
    /// rows with at most 64 KiB of owned text. A zero row limit is a no-op.
    /// `final_chunk` means this slice reaches the actual end of the source,
    /// not the end of a storage page. If the slice is only partly consumed,
    /// repeat with its suffix and the same final flag. Drain final buffered
    /// rows using an empty slice. Do not append after finalization.
    pub fn push(&mut self, input: &[u8], final_chunk: bool, row_limit: usize) -> TextPage {
        let mut rows = Vec::new();
        let mut text_bytes = 0;
        let mut consumed = 0;
        let row_limit = row_limit.min(MAX_PAGE_ROWS);
        let input_limit = input.len().min(MAX_INPUT_BYTES);
        while rows.len() < row_limit {
            if !drain_rows(&mut self.layout, &mut rows, &mut text_bytes, row_limit) {
                break;
            }
            if self.ended {
                break;
            }
            if consumed == input_limit {
                if final_chunk && consumed == input.len() {
                    if self.utf8_len != 0 {
                        self.accept('\u{fffd}', self.utf8_start..self.position);
                        self.utf8_len = 0;
                        continue;
                    }
                    self.layout.finish(self.position);
                    self.ended = true;
                    continue;
                }
                break;
            }
            let byte = input[consumed];
            if self.utf8_len != 0 && byte & 0xc0 != 0x80 {
                self.accept('\u{fffd}', self.utf8_start..self.position);
                self.utf8_len = 0;
                continue;
            }
            if self.utf8_len == 0 {
                self.utf8_start = self.position;
            }
            self.utf8[self.utf8_len] = byte;
            self.utf8_len += 1;
            consumed += 1;
            self.position += 1;
            match std::str::from_utf8(&self.utf8[..self.utf8_len]) {
                Ok(text) => {
                    let c = text.chars().next().expect("one decoded scalar");
                    self.accept(c, self.utf8_start..self.position);
                    self.utf8_len = 0;
                }
                Err(error) if error.error_len().is_some() => {
                    self.accept('\u{fffd}', self.utf8_start..self.position);
                    self.utf8_len = 0;
                }
                Err(_) => {}
            }
        }
        TextPage {
            consumed,
            rows,
            finished: self.is_finished(),
        }
    }

    fn accept(&mut self, c: char, source: Range<usize>) {
        if let Some(c) = self.sanitizer.accept(c) {
            self.layout
                .accept(c, source, Style::default().fg(Palette::TEXT()));
        }
    }
}

/// Width-independent sanitizer checkpoint. Unlike TextCursor, this contains
/// no wrap state. Input chunks must be valid UTF-8 and contiguous in the source.
#[derive(Clone, Debug, Default)]
pub struct ScanState {
    sanitizer: Sanitizer,
    markdown: MarkdownBoundary,
    previous_blank: bool,
}

#[derive(Clone, Debug)]
struct MarkdownBoundary {
    at_boundary: bool,
    fence: Option<(char, usize)>,
    indent: usize,
    marker: Option<char>,
    run: usize,
    after_run: bool,
    nonspace_tail: bool,
    blank: bool,
}

impl Default for MarkdownBoundary {
    fn default() -> Self {
        Self {
            at_boundary: true,
            fence: None,
            indent: 0,
            marker: None,
            run: 0,
            after_run: false,
            nonspace_tail: false,
            blank: true,
        }
    }
}

impl MarkdownBoundary {
    fn accept(&mut self, c: char) -> bool {
        if c == '\n' {
            let mut boundary = false;
            if let Some((marker, length)) = self.fence {
                if self.indent <= 3
                    && self.marker == Some(marker)
                    && self.run >= length
                    && !self.nonspace_tail
                {
                    self.fence = None;
                    boundary = true;
                }
            } else if self.indent <= 3 && self.run >= 3 {
                self.fence = self.marker.map(|marker| (marker, self.run));
            } else if self.blank {
                boundary = true;
            }
            self.at_boundary = boundary;
            self.indent = 0;
            self.marker = None;
            self.run = 0;
            self.after_run = false;
            self.nonspace_tail = false;
            self.blank = true;
            return boundary;
        }
        self.at_boundary = false;
        if self.blank && matches!(c, ' ' | '\t') {
            self.indent = (self.indent + if c == '\t' { 4 } else { 1 }).min(4);
            return false;
        }
        if self.blank {
            self.marker = matches!(c, '`' | '~').then_some(c);
            self.blank = false;
        }
        if !self.after_run && self.marker == Some(c) {
            self.run += 1;
        } else {
            self.after_run = true;
            self.nonspace_tail |= !c.is_whitespace();
        }
        false
    }
}

#[derive(Clone, Debug)]
pub struct Sanitized {
    pub text: String,
    /// Original byte start for each emitted scalar, plus a terminal sentinel
    /// equal to source.end. Thus stripped control gaps belong to the preceding
    /// scalar's extent. An empty result contains only the terminal sentinel.
    pub offsets: Vec<usize>,
    pub source: Range<usize>,
    pub consumed: usize,
    starts_block: bool,
    complete: bool,
    boundaries: Vec<usize>,
    boundary_sources: Vec<usize>,
    previous_blank: bool,
}

impl ScanState {
    pub fn finish_layout_page(&mut self, page: &Sanitized) {
        self.previous_blank = page.visit_rows(MAX_WIDTH, |_, _| true);
    }

    /// Accept <=16 KiB, rounding down to a UTF-8 boundary. Output text is no
    /// larger than accepted input; the map has at most 16385 usize elements.
    /// The caller must retain this state even when all text was suppressed.
    pub fn at_block_boundary(&self) -> bool {
        self.markdown.at_boundary
    }

    pub fn scan(&mut self, text: &str, start: usize) -> Sanitized {
        let mut consumed = text.len().min(MAX_INPUT_BYTES);
        while !text.is_char_boundary(consumed) {
            consumed -= 1;
        }
        let mut clean = String::with_capacity(consumed);
        let mut offsets = Vec::with_capacity(consumed + 1);
        let starts_block = self.markdown.at_boundary;
        let mut boundaries = Vec::new();
        let mut boundary_sources = Vec::new();
        for (byte, c) in text[..consumed].char_indices() {
            if let Some(c) = self.sanitizer.accept(c) {
                offsets.push(start + byte);
                clean.push(c);
                if self.markdown.accept(c) {
                    boundaries.push(clean.len());
                    boundary_sources.push(start + byte + c.len_utf8());
                }
            }
        }
        offsets.push(start + consumed);
        Sanitized {
            text: clean,
            offsets,
            source: start..start + consumed,
            consumed,
            starts_block,
            complete: false,
            boundaries,
            boundary_sources,
            previous_blank: self.previous_blank,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub rows: Vec<SourceRow>,
    /// Resume with this source-block anchor and rendered byte ordinal. Generated
    /// Markdown rows can outnumber original bytes, so both fields are needed.
    pub next_offset: usize,
    pub next_ordinal: usize,
}

impl Layout {
    pub fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.rows.capacity() * std::mem::size_of::<SourceRow>()
            + self.rows.iter().map(allocated_row_bytes).sum::<usize>()
    }
}

impl Sanitized {
    pub fn block_boundaries(&self) -> impl Iterator<Item = usize> + '_ {
        self.boundaries.iter().copied()
    }

    pub fn last_block_source_boundary(&self) -> Option<usize> {
        self.boundary_sources.last().copied()
    }

    pub fn starts_block(&self) -> bool {
        self.starts_block
    }

    /// Mark only a page ending at the actual source EOF as complete. Storage
    /// page boundaries are not Markdown block boundaries.
    pub fn set_complete(&mut self, complete: bool) {
        self.complete = complete;
    }

    pub fn source_end(&self) -> usize {
        self.source.end
    }

    pub fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.text.capacity()
            + (self.offsets.capacity()
                + self.boundaries.capacity()
                + self.boundary_sources.capacity())
                * std::mem::size_of::<usize>()
    }

    /// Select a bounded window without allocating every wrapped row. Styled
    /// completed chunks use (source start, rendered byte ordinal) anchors; literal rows use
    /// their exact source extent and ordinal zero. Tail selection includes its
    /// target; use usize::MAX as its ordinal to include the whole target block.
    ///
    /// Completed interior blocks retain normal Markdown. Continuing blocks and
    /// pathological renderer inputs remain literal. Raw pages cutting through
    /// a grapheme require caller lookahead or TextCursor::push_sanitized; this
    /// local-layout convenience cannot infer the missing neighboring scalars.
    pub fn layout(
        &self,
        width: usize,
        offset: usize,
        ordinal: usize,
        max_rows: usize,
        tail: bool,
    ) -> Layout {
        let max_rows = max_rows.min(MAX_PAGE_ROWS);
        if max_rows == 0 {
            return Layout {
                rows: Vec::new(),
                next_offset: offset,
                next_ordinal: ordinal,
            };
        }
        let mut selected: VecDeque<(SourceRow, (usize, usize))> = VecDeque::new();
        let mut text_bytes = 0;
        let mut exhausted = true;
        self.visit_rows(width, |row, styled| {
            if tail {
                if (row.source.start, row.ordinal) > (offset, ordinal) {
                    exhausted = false;
                    return false;
                }
            } else if row.source.end <= offset
                || (styled && row.source.start == offset && row.ordinal_end <= ordinal)
            {
                return true;
            }
            let bytes = row_bytes(&row);
            if tail {
                while selected.len() >= max_rows || text_bytes + bytes > MAX_PAGE_TEXT_BYTES {
                    let (old, _) = selected.pop_front().expect("bounded selected rows");
                    text_bytes -= row_bytes(&old);
                }
            } else if selected.len() == max_rows || text_bytes + bytes > MAX_PAGE_TEXT_BYTES {
                exhausted = false;
                return false;
            }
            let next = if styled {
                (row.source.start, row.ordinal_end)
            } else {
                (row.source.end, 0)
            };
            text_bytes += bytes;
            selected.push_back((row, next));
            true
        });
        let (next_offset, next_ordinal) = if exhausted {
            (self.source.end, 0)
        } else {
            selected
                .back()
                .map(|(_, next)| *next)
                .unwrap_or((self.source.end, 0))
        };
        Layout {
            rows: selected.into_iter().map(|(row, _)| row).collect(),
            next_offset,
            next_ordinal,
        }
    }

    fn visit_rows(&self, width: usize, mut visit: impl FnMut(SourceRow, bool) -> bool) -> bool {
        let mut scalar = 0;
        let mut start = 0;
        let mut raw_start = self.source.start;
        let mut at_boundary = self.starts_block;
        let mut previous_blank = self.previous_blank;
        for (end, raw_end, boundary) in self
            .boundaries
            .iter()
            .copied()
            .zip(self.boundary_sources.iter().copied())
            .map(|(end, raw)| (end, raw, true))
            .chain(std::iter::once((self.text.len(), self.source.end, false)))
        {
            if start == end {
                continue;
            }
            let part = &self.text[start..end];
            let scalar_end = scalar + part.chars().count();
            let styled =
                at_boundary && (boundary || self.complete) && part.len() <= MAX_STYLED_BLOCK_BYTES;
            if styled {
                let final_chunk = self.complete && self.text[end..].trim().is_empty();
                if let MarkdownChunk::Styled(mut cursor) = render_markdown_chunk_continued(
                    part,
                    raw_start,
                    width,
                    previous_blank,
                    final_chunk,
                ) {
                    if cursor.allocated_bytes() <= MAX_MARKDOWN_CURSOR_BYTES {
                        previous_blank = cursor.ends_blank();
                        loop {
                            let page = cursor.page(MAX_PAGE_ROWS);
                            for mut row in page.rows {
                                row.source = raw_start..raw_end;
                                if !visit(row, true) {
                                    return previous_blank;
                                }
                            }
                            if page.finished {
                                break;
                            }
                        }
                        scalar = scalar_end;
                        start = end;
                        raw_start = raw_end;
                        at_boundary = boundary;
                        continue;
                    }
                }
            }
            let mut wrap = WrapState::new(raw_start, width);
            for (index, c) in part.chars().enumerate() {
                let index = scalar + index;
                wrap.accept(
                    c,
                    self.offsets[index]..self.offsets[index + 1].min(raw_end),
                    Style::default().fg(Palette::TEXT()),
                );
                while let Some(row) = wrap.ready.pop_front() {
                    previous_blank = row.line.width() == 0;
                    if !visit(row, false) {
                        return previous_blank;
                    }
                }
            }
            wrap.finish(raw_end);
            while let Some(row) = wrap.ready.pop_front() {
                previous_blank = row.line.width() == 0;
                if !visit(row, false) {
                    return previous_blank;
                }
            }
            scalar = scalar_end;
            start = end;
            raw_start = raw_end;
            at_boundary = boundary;
        }
        previous_blank
    }
}

fn allocated_line_bytes(line: &Line<'static>) -> usize {
    line.spans.capacity() * std::mem::size_of::<Span<'static>>()
        + line
            .spans
            .iter()
            .map(|span| match &span.content {
                std::borrow::Cow::Owned(text) => text.capacity(),
                std::borrow::Cow::Borrowed(_) => 0,
            })
            .sum::<usize>()
}

fn allocated_row_bytes(row: &SourceRow) -> usize {
    allocated_line_bytes(&row.line)
}

#[derive(Debug)]
pub struct Prepared {
    pub rows: Vec<SourceRow>,
    /// Resume this Sanitized chunk at this scalar index, not at a UTF-8 byte.
    pub next_scalar: usize,
    /// Original byte position corresponding to next_scalar. This is ingestion
    /// progress; layout may still retain earlier text. Keep the cursor too.
    pub next_source: usize,
    pub finished: bool,
}

impl TextCursor {
    /// Lay out already-sanitized chunks without repeating their control scan.
    /// Do not mix this with push on the same cursor. Retain the cursor across
    /// chunks for grapheme continuity; `scalar_offset` is zero for a new chunk,
    /// otherwise the preceding result's next_scalar. Finalization means the
    /// actual source end, not a storage-page end. ScanState checkpoints survive
    /// width changes, but layout checkpoints must be recreated for the width.
    pub fn push_sanitized(
        &mut self,
        input: &Sanitized,
        scalar_offset: usize,
        final_chunk: bool,
        row_limit: usize,
    ) -> Prepared {
        let mut rows = Vec::new();
        let mut text_bytes = 0;
        let mut next_scalar = scalar_offset;
        let mut chars = input.text.chars().skip(scalar_offset);
        let row_limit = row_limit.min(MAX_PAGE_ROWS);
        while rows.len() < row_limit {
            if !drain_rows(&mut self.layout, &mut rows, &mut text_bytes, row_limit) {
                break;
            }
            if self.ended {
                break;
            }
            if let Some(c) = chars.next() {
                let start = input.offsets[next_scalar];
                next_scalar += 1;
                self.position = input.offsets[next_scalar];
                self.layout.accept(
                    c,
                    start..self.position,
                    Style::default().fg(Palette::TEXT()),
                );
            } else {
                self.position = input.source.end;
                if final_chunk {
                    self.layout.finish(self.position);
                    self.ended = true;
                    continue;
                }
                break;
            }
        }
        Prepared {
            rows,
            next_scalar,
            next_source: input.offsets[next_scalar],
            finished: self.is_finished(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
enum Sanitizer {
    #[default]
    Text,
    Escape,
    Intermediate,
    Csi,
    String {
        osc: bool,
        escape: bool,
    },
}

impl Sanitizer {
    fn accept(&mut self, c: char) -> Option<char> {
        if let Self::String { osc, escape } = *self {
            *self = if matches!(c, '\u{9c}' | '\u{18}' | '\u{1a}')
                || (osc && c == '\u{7}')
                || (escape && c == '\\')
            {
                Self::Text
            } else {
                Self::String {
                    osc,
                    escape: c == '\u{1b}',
                }
            };
            return None;
        }
        match c {
            '\u{1b}' => *self = Self::Escape,
            '\u{9b}' => *self = Self::Csi,
            '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                *self = Self::String {
                    osc: c == '\u{9d}',
                    escape: false,
                };
            }
            '\u{18}' | '\u{1a}' | '\u{9c}' => *self = Self::Text,
            '\n' | '\t' => return Some(c),
            c if c.is_control() => {}
            _ => match *self {
                Self::Text => return Some(c),
                Self::Escape => {
                    *self = match c {
                        '[' => Self::Csi,
                        ']' | 'P' | 'X' | '^' | '_' => Self::String {
                            osc: c == ']',
                            escape: false,
                        },
                        '\u{20}'..='\u{2f}' => Self::Intermediate,
                        '\u{30}'..='\u{7e}' => Self::Text,
                        _ => {
                            *self = Self::Text;
                            return Some(c);
                        }
                    };
                }
                Self::Intermediate => match c {
                    '\u{20}'..='\u{2f}' => {}
                    '\u{30}'..='\u{7e}' => *self = Self::Text,
                    _ => {
                        *self = Self::Text;
                        return Some(c);
                    }
                },
                Self::Csi => match c {
                    '\u{20}'..='\u{3f}' => {}
                    '\u{40}'..='\u{7e}' => *self = Self::Text,
                    _ => {
                        *self = Self::Text;
                        return Some(c);
                    }
                },
                Self::String { .. } => unreachable!(),
            },
        }
        None
    }
}

#[derive(Clone, Debug)]
struct WrapState {
    width: usize,
    row: Line<'static>,
    row_start: usize,
    row_bytes: usize,
    row_cells: usize,
    row_fragment: bool,
    cluster: String,
    cluster_source: Range<usize>,
    cluster_style: Style,
    cluster_fragment: bool,
    ready: VecDeque<SourceRow>,
}

impl WrapState {
    fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + allocated_line_bytes(&self.row)
            + self.cluster.capacity()
            + self.ready.capacity() * std::mem::size_of::<SourceRow>()
            + self.ready.iter().map(allocated_row_bytes).sum::<usize>()
    }

    fn new(source_start: usize, width: usize) -> Self {
        Self {
            width: width.clamp(1, MAX_WIDTH),
            row: Line::default(),
            row_start: source_start,
            row_bytes: 0,
            row_cells: 0,
            row_fragment: false,
            cluster: String::new(),
            cluster_source: source_start..source_start,
            cluster_style: Style::default(),
            cluster_fragment: false,
            ready: VecDeque::new(),
        }
    }

    fn accept(&mut self, c: char, source: Range<usize>, style: Style) {
        if c == '\n' {
            self.commit_cluster();
            self.emit(source.end);
            return;
        }
        if c == '\t' {
            self.commit_cluster();
            self.cluster = " ".repeat((4 - self.row_cells % 4).min(self.width));
            self.cluster_source = source;
            self.cluster_style = style;
            self.commit_cluster();
            return;
        }
        if self.cluster.len() + c.len_utf8() > MAX_GRAPHEME_BYTES {
            self.cluster_fragment = true;
            self.commit_cluster();
            self.cluster_fragment = true;
        }
        let old_len = self.cluster.len();
        self.cluster.push(c);
        let first_len = {
            let span = Span::raw(self.cluster.as_str());
            let mut graphemes = span.styled_graphemes(Style::default());
            graphemes.next().expect("nonempty cluster").symbol.len()
        };
        if first_len < self.cluster.len() {
            self.cluster.truncate(old_len);
            self.commit_cluster();
            self.cluster.push(c);
            self.cluster_source = source;
            self.cluster_style = style;
        } else if old_len == 0 {
            self.cluster_source = source;
            self.cluster_style = style;
        } else {
            self.cluster_source.end = source.end;
        }
    }

    fn commit_cluster(&mut self) {
        if self.cluster.is_empty() {
            return;
        }
        let cells = UnicodeWidthStr::width(self.cluster.as_str());
        if self.row_bytes != 0
            && (self.row_cells + cells > self.width
                || self.row_bytes + self.cluster.len() > MAX_ROW_BYTES
                || (self.row.spans.len() >= MAX_ROW_SPANS
                    && self
                        .row
                        .spans
                        .last()
                        .is_some_and(|span| span.style != self.cluster_style)))
        {
            self.emit(self.cluster_source.start);
        }
        self.row_bytes += self.cluster.len();
        self.row_cells += cells;
        self.row_fragment |= self.cluster_fragment;
        if let Some(last) = self
            .row
            .spans
            .last_mut()
            .filter(|s| s.style == self.cluster_style)
        {
            last.content.to_mut().push_str(&self.cluster);
            self.cluster.clear();
        } else {
            self.row.spans.push(Span::styled(
                std::mem::take(&mut self.cluster),
                self.cluster_style,
            ));
        }
        self.cluster_fragment = false;
    }

    fn emit(&mut self, source_end: usize) {
        self.ready.push_back(SourceRow {
            source: self.row_start..source_end,
            ordinal: 0,
            ordinal_end: 0,
            line: std::mem::take(&mut self.row),
            grapheme_fragment: self.row_fragment,
        });
        self.row_start = source_end;
        self.row_bytes = 0;
        self.row_cells = 0;
        self.row_fragment = false;
    }

    fn finish(&mut self, source_end: usize) {
        self.commit_cluster();
        if self.row_bytes != 0 {
            self.emit(source_end);
        }
    }
}

fn row_bytes(row: &SourceRow) -> usize {
    row.line.spans.iter().map(|span| span.content.len()).sum()
}

fn drain_rows(
    layout: &mut WrapState,
    rows: &mut Vec<SourceRow>,
    text_bytes: &mut usize,
    row_limit: usize,
) -> bool {
    while let Some(row) = layout.ready.front() {
        let bytes = row_bytes(row);
        if rows.len() == row_limit || *text_bytes + bytes > MAX_PAGE_TEXT_BYTES {
            return false;
        }
        *text_bytes += bytes;
        rows.push(layout.ready.pop_front().expect("ready row"));
    }
    rows.len() < row_limit
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiteralReason {
    InputTooLarge,
    LongLine,
    ComplexBlock,
    OutputExpansion,
    IncompleteControl,
}

#[derive(Debug)]
pub enum MarkdownChunk {
    Styled(Box<MarkdownCursor>),
    /// Send the original source through TextCursor, retaining checkpoints and
    /// this literal decision until the actual enclosing block ends.
    Literal(LiteralReason),
}

#[derive(Debug)]
pub struct MarkdownPage {
    pub rows: Vec<SourceRow>,
    pub finished: bool,
}

/// Holds only a bounded rendered block, never the retained assistant body.
/// Page wrapping is lazy, so a narrow viewport cannot allocate all wrapped
/// rows at once. Styles use the existing shared Markdown/highlight renderer.
/// Graphemes spanning differently styled spans take their first scalar's style.
#[derive(Clone, Debug)]
pub struct MarkdownCursor {
    source: Range<usize>,
    lines: VecDeque<Line<'static>>,
    span: usize,
    byte: usize,
    position: usize,
    ends_blank: bool,
    layout: WrapState,
}

impl MarkdownCursor {
    pub fn ends_blank(&self) -> bool {
        self.ends_blank
    }

    pub fn allocated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.layout.allocated_bytes()
            + self.lines.capacity() * std::mem::size_of::<Line<'static>>()
            + self.lines.iter().map(allocated_line_bytes).sum::<usize>()
    }

    pub fn is_finished(&self) -> bool {
        self.lines.is_empty() && self.layout.ready.is_empty()
    }

    pub fn page(&mut self, row_limit: usize) -> MarkdownPage {
        let mut rows = Vec::new();
        let mut text_bytes = 0;
        let row_limit = row_limit.min(MAX_PAGE_ROWS);
        while rows.len() < row_limit {
            if !drain_rows(&mut self.layout, &mut rows, &mut text_bytes, row_limit) {
                break;
            }
            let Some(line) = self.lines.front() else {
                break;
            };
            if let Some(span) = line.spans.get(self.span) {
                if let Some(c) = span.content[self.byte..].chars().next() {
                    let style = line.style.patch(span.style);
                    self.byte += c.len_utf8();
                    let start = self.position;
                    self.position += c.len_utf8();
                    self.layout.accept(c, start..self.position, style);
                } else {
                    self.span += 1;
                    self.byte = 0;
                }
            } else {
                let start = self.position;
                self.position += 1;
                self.layout
                    .accept('\n', start..self.position, Style::default());
                self.lines.pop_front();
                self.span = 0;
                self.byte = 0;
            }
        }
        for row in &mut rows {
            row.ordinal = row.source.start;
            row.ordinal_end = row.source.end;
            row.source = self.source.clone();
        }
        MarkdownPage {
            rows,
            finished: self.is_finished(),
        }
    }
}

/// Render a caller-confirmed complete Markdown block at a known clean control
/// boundary. This does not discover block boundaries: do not split fences,
/// tables, lists or reference definitions at arbitrary storage-page offsets.
/// Incomplete/oversized enclosing blocks must remain literal through their end.
///
/// Both preflight parsing and the ordinary renderer receive at most 16 KiB.
/// Depth, event count and table expansion are checked before rendering, avoiding
/// quadratic table padding and deep-prefix expansion. Returned styled output
/// is capped at 64 KiB text, 2048 spans and 1024 unwrapped lines. Those bounds
/// are independent of total source length; they exclude the highlighter's
/// existing process-wide syntax/theme cache. Wrapped pages have TextPage caps.
pub fn render_markdown_chunk(source: &str, source_start: usize, width: usize) -> MarkdownChunk {
    render_markdown_chunk_continued(source, source_start, width, false, true)
}

pub fn render_markdown_chunk_continued(
    source: &str,
    source_start: usize,
    width: usize,
    previous_blank: bool,
    final_chunk: bool,
) -> MarkdownChunk {
    if source.len() > MAX_INPUT_BYTES {
        return MarkdownChunk::Literal(LiteralReason::InputTooLarge);
    }
    let mut sanitizer = Sanitizer::default();
    for c in source.chars() {
        sanitizer.accept(c);
    }
    if !matches!(sanitizer, Sanitizer::Text) {
        return MarkdownChunk::Literal(LiteralReason::IncompleteControl);
    }
    let clean = safe_display_text(source);
    if let Err(reason) = markdown_preflight(&clean) {
        return MarkdownChunk::Literal(reason);
    }
    let mut lines = render_markdown_continued(&clean, previous_blank, final_chunk);
    for line in &mut lines {
        for span in &mut line.spans {
            if let std::borrow::Cow::Owned(text) = &mut span.content {
                text.shrink_to_fit();
            }
        }
        line.spans.shrink_to_fit();
    }
    lines.shrink_to_fit();
    let ends_blank = lines
        .last()
        .map_or(previous_blank, |line| line.width() == 0);
    let text_bytes: usize = lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|s| s.content.len())
        .sum();
    let spans: usize = lines.iter().map(|line| line.spans.len()).sum();
    if text_bytes > MAX_PAGE_TEXT_BYTES || spans > 2048 || lines.len() > 1024 {
        return MarkdownChunk::Literal(LiteralReason::OutputExpansion);
    }
    let cursor = MarkdownCursor {
        source: source_start..source_start + source.len(),
        lines: lines.into(),
        span: 0,
        byte: 0,
        position: 0,
        ends_blank,
        layout: WrapState::new(0, width),
    };
    if cursor.allocated_bytes() > MAX_MARKDOWN_CURSOR_BYTES {
        MarkdownChunk::Literal(LiteralReason::OutputExpansion)
    } else {
        MarkdownChunk::Styled(Box::new(cursor))
    }
}

fn markdown_preflight(source: &str) -> Result<(), LiteralReason> {
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let mut depth = 0usize;
    let mut max_depth = 0usize;
    let mut events = 0usize;
    let mut extra_bytes = 256usize;
    let mut table = false;
    let mut code = false;
    let mut code_line_bytes = 0;
    let mut widths = [0usize; 32];
    let mut columns = 0usize;
    let mut column = 0usize;
    let mut cell_chars = 0usize;
    let mut cells = 0usize;
    let mut table_rows = 0usize;
    for event in Parser::new_ext(source, opts) {
        events += 1;
        if events > 2048 {
            return Err(LiteralReason::ComplexBlock);
        }
        match &event {
            Event::Start(_) => {
                depth += 1;
                max_depth = max_depth.max(depth);
                if depth > 16 {
                    return Err(LiteralReason::ComplexBlock);
                }
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        match event {
            Event::Start(Tag::CodeBlock(_)) => {
                code = true;
                code_line_bytes = 0;
            }
            Event::End(TagEnd::CodeBlock) => code = false,
            Event::Text(text) if code => {
                for part in text.split_inclusive('\n') {
                    code_line_bytes += part.trim_end_matches('\n').len();
                    if code_line_bytes > 2048 {
                        return Err(LiteralReason::LongLine);
                    }
                    if part.ends_with('\n') {
                        code_line_bytes = 0;
                    }
                }
            }
            Event::Start(Tag::Table(alignments)) => {
                if alignments.len() > widths.len() {
                    return Err(LiteralReason::ComplexBlock);
                }
                table = true;
                columns = alignments.len();
                widths.fill(0);
                cells = 0;
                table_rows = 0;
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => {
                column = 0;
                table_rows += 1;
            }
            Event::Start(Tag::TableCell) => cell_chars = 0,
            Event::Text(text) | Event::Code(text) if table => cell_chars += text.chars().count(),
            Event::SoftBreak | Event::HardBreak if table => cell_chars += 1,
            Event::End(TagEnd::TableCell) => {
                if column >= columns || cells >= 256 {
                    return Err(LiteralReason::ComplexBlock);
                }
                widths[column] = widths[column].max(cell_chars);
                column += 1;
                cells += 1;
            }
            Event::End(TagEnd::Table) => {
                let row_width = widths[..columns].iter().sum::<usize>() + columns * 3 + 1;
                extra_bytes += row_width * 4 * (table_rows + 3);
                table = false;
            }
            Event::Start(Tag::Item) => extra_bytes += 64,
            Event::Rule => extra_bytes += 144,
            _ => {}
        }
        if extra_bytes > MAX_PAGE_TEXT_BYTES {
            return Err(LiteralReason::OutputExpansion);
        }
    }
    let source_lines = source.bytes().filter(|&b| b == b'\n').count() + 1;
    let upper_bytes = source.len() * 2 + source_lines * (16 + 6 * max_depth) + extra_bytes;
    if upper_bytes > MAX_PAGE_TEXT_BYTES {
        return Err(LiteralReason::OutputExpansion);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(
        cursor: &mut TextCursor,
        mut input: &[u8],
        final_chunk: bool,
        rows: usize,
    ) -> Vec<SourceRow> {
        let mut output = Vec::new();
        loop {
            let page = cursor.push(input, final_chunk, rows);
            assert!(page.consumed <= MAX_INPUT_BYTES);
            assert!(page.rows.len() <= rows.min(MAX_PAGE_ROWS));
            assert!(page.rows.iter().map(row_bytes).sum::<usize>() <= MAX_PAGE_TEXT_BYTES);
            assert!(page.rows.iter().all(|row| row_bytes(row) <= MAX_ROW_BYTES));
            let progress = page.consumed != 0 || !page.rows.is_empty();
            input = &input[page.consumed..];
            output.extend(page.rows);
            if input.is_empty() && (!final_chunk || page.finished) {
                return output;
            }
            assert!(progress, "cursor must progress or finish");
        }
    }

    fn rendered(source: &str, width: usize, chunk: usize, rows: usize) -> Vec<SourceRow> {
        let mut cursor = TextCursor::new(0, width);
        let mut output = Vec::new();
        for input in source.as_bytes().chunks(chunk) {
            output.extend(drain(&mut cursor, input, false, rows));
        }
        output.extend(drain(&mut cursor, &[], true, rows));
        output
    }

    fn text(rows: &[SourceRow]) -> String {
        rows.iter().map(|row| row.line.to_string()).collect()
    }

    #[test]
    fn byte_chunks_preserve_utf8_graphemes_and_source_ranges() {
        let source = "e\u{301}👩🏽\u{200d}💻🇺🇸क्\u{200d}ष界xyz";
        let baseline = rendered(source, 2, source.len(), 256);
        assert_eq!(text(&baseline), source);
        for chunk in 1..=source.len() {
            assert_eq!(rendered(source, 2, chunk, 1), baseline, "chunk {chunk}");
        }
        let mut cursor = TextCursor::new(41, 3);
        let rows = drain(&mut cursor, "ab界cd".as_bytes(), true, 1);
        assert_eq!(
            rows.iter()
                .map(|row| row.line.to_string())
                .collect::<Vec<_>>(),
            ["ab", "界c", "d"]
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.source.clone())
                .collect::<Vec<_>>(),
            [41..43, 43..47, 47..48]
        );
    }

    #[test]
    fn split_controls_match_shared_sanitizer_without_leaking_payloads() {
        let source = concat!(
            "a\u{1b}[31m界\u{1b}[0m",
            "\u{1b}]52;c;secret\u{1b}\\",
            "\u{9d}title\u{7}",
            "\u{1b}Pprivate\u{9c}",
            "\u{1b}^hidden\u{18}",
            "\u{1b}_hidden\u{1a}",
            "\u{1b}(Bx\u{9b}1my",
            "\u{1b}[31\nmb\r\u{7}",
            "\u{1b}]unfinished"
        );
        let clean = safe_display_text(source).replace('\n', "");
        let baseline = rendered(source, 3, source.len(), 256);
        assert_eq!(text(&baseline), clean);
        for chunk in 1..=source.len() {
            assert_eq!(rendered(source, 3, chunk, 1), baseline, "chunk {chunk}");
        }
    }

    #[test]
    fn cloned_checkpoint_preserves_pending_utf8_control_and_wrap_state() {
        for prefix in [b"abc\x1b]title\x1b".as_slice(), b"abc\xf0\x9f".as_slice()] {
            let mut cursor = TextCursor::new(80, 2);
            let _ = drain(&mut cursor, prefix, false, 1);
            let mut restored = cursor.clone();
            let suffix = b"\x92\xbb\\\x07end";
            assert_eq!(
                drain(&mut cursor, suffix, true, 1),
                drain(&mut restored, suffix, true, 1)
            );
            assert_eq!(cursor.position(), 80 + prefix.len() + suffix.len());
        }
    }

    #[test]
    fn oversized_combining_cluster_is_bounded_lossless_and_marked() {
        let source = format!("a{}z", "\u{301}".repeat(20000));
        let rows = rendered(&source, 1, 17, 1);
        assert_eq!(text(&rows), source);
        assert!(rows.iter().any(|row| row.grapheme_fragment));
        assert!(rows.iter().all(|row| row_bytes(row) <= MAX_ROW_BYTES));
    }

    #[test]
    fn giant_line_is_complete_with_bounded_pages_at_width_one() {
        let source = "界a".repeat(20000);
        let rows = rendered(&source, 1, MAX_INPUT_BYTES + 31, 7);
        assert_eq!(text(&rows), source);
        assert_eq!(rows.len(), 40000);
        assert_eq!(rows.last().unwrap().source.end, source.len());
    }

    #[test]
    fn finalization_handles_invalid_utf8_newlines_tabs_and_zero_budget() {
        let mut cursor = TextCursor::new(0, 0);
        let empty = cursor.push(b"abc", true, 0);
        assert_eq!(empty.consumed, 0);
        assert!(empty.rows.is_empty());
        assert!(!empty.finished);
        let rows = drain(&mut cursor, b"a\t\nb\xe2", true, 1);
        assert_eq!(
            rows.iter()
                .map(|row| row.line.to_string())
                .collect::<Vec<_>>(),
            ["a", " ", "b", "\u{fffd}"]
        );
        assert!(cursor.push(&[], true, 1).finished);
        let mut cursor = TextCursor::new(0, 10);
        let rows = drain(&mut cursor, b"\n\nx\n", true, 1);
        assert_eq!(
            rows.iter()
                .map(|row| row.line.to_string())
                .collect::<Vec<_>>(),
            ["", "", "x"]
        );
        let mut cursor = TextCursor::new(0, 10);
        assert!(drain(&mut cursor, b"\x1b]hidden", true, 1).is_empty());
        assert!(cursor.is_finished());
    }

    #[test]
    fn scan_checkpoints_are_width_independent_and_map_original_bytes() {
        let mut scan = ScanState::default();
        let first = scan.scan("a\u{1b}]payload", 50);
        let mut checkpoint = scan.clone();
        let second = scan.scan("still hidden\u{1b}\\界z", first.source_end());
        let replay = checkpoint.scan("still hidden\u{1b}\\界z", first.source_end());
        assert_eq!(second.text, "界z");
        assert_eq!(second.text, replay.text);
        assert_eq!(second.offsets, replay.offsets);
        assert_eq!(first.offsets, [50, first.source_end()]);
        assert_eq!(second.offsets.len(), second.text.chars().count() + 1);
        for width in [1, 2, 80] {
            assert_eq!(
                text(
                    &second
                        .layout(width, second.source.start, 0, 256, false)
                        .rows
                ),
                "界z"
            );
        }
    }

    #[test]
    fn scan_rounds_input_cap_down_without_dropping_utf8_or_growing_map() {
        let source = format!("{}界last", "x".repeat(MAX_INPUT_BYTES - 1));
        let mut scan = ScanState::default();
        let first = scan.scan(&source, 0);
        assert_eq!(first.consumed, MAX_INPUT_BYTES - 1);
        let second = scan.scan(&source[first.consumed..], first.source_end());
        assert_eq!(format!("{}{}", first.text, second.text), source);
        assert!(first.offsets.capacity() <= MAX_INPUT_BYTES + 1);
        assert!(
            first.allocated_bytes()
                <= std::mem::size_of::<Sanitized>()
                    + MAX_INPUT_BYTES
                    + (MAX_INPUT_BYTES + 1) * std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn sanitized_cursor_preserves_graphemes_across_pages_and_row_limits() {
        let source = "a e\u{301}👩\u{200d}💻 end";
        let mut scanner = ScanState::default();
        let mut cursor = TextCursor::new(0, 2);
        let mut rows = Vec::new();
        for (start, c) in source.char_indices() {
            let input = scanner.scan(&c.to_string(), start);
            let mut scalar = 0;
            loop {
                let page = cursor.push_sanitized(&input, scalar, false, 1);
                scalar = page.next_scalar;
                rows.extend(page.rows);
                if scalar == input.offsets.len() - 1 {
                    break;
                }
            }
        }
        let empty = scanner.scan("", source.len());
        loop {
            let page = cursor.push_sanitized(&empty, 0, true, 1);
            rows.extend(page.rows);
            if page.finished {
                break;
            }
        }
        assert_eq!(rows, rendered(source, 2, 1, 1));
    }

    #[test]
    fn local_layout_windows_resume_without_duplicating_or_skipping_rows() {
        let source = "ab界de\nfghijk";
        let clean = ScanState::default().scan(source, 100);
        let all = clean.layout(3, 100, 0, 256, false);
        let mut offset = 100;
        let mut rows = Vec::new();
        loop {
            let page = clean.layout(3, offset, 0, 1, false);
            if page.rows.is_empty() {
                break;
            }
            assert!(page.next_offset > offset);
            offset = page.next_offset;
            rows.extend(page.rows);
        }
        assert_eq!(rows, all.rows);
        let tail = clean.layout(3, clean.source_end(), usize::MAX, 2, true);
        assert_eq!(tail.rows, all.rows[all.rows.len() - 2..]);
        assert_eq!(tail.next_offset, clean.source_end());
        let interior = clean.layout(3, 103, 0, 1, false);
        assert_eq!(interior.rows[0], all.rows[1]);
    }

    #[test]
    fn width_one_tail_layout_retains_only_the_bounded_window() {
        let source = "x".repeat(MAX_INPUT_BYTES);
        let clean = ScanState::default().scan(&source, 0);
        let tail = clean.layout(1, source.len(), usize::MAX, 7, true);
        assert_eq!(tail.rows.len(), 7);
        assert_eq!(tail.rows[0].source.start, source.len() - 7);
        assert_eq!(tail.next_offset, source.len());
        assert!(tail.allocated_bytes() < 64 * 1024);
        assert!(clean
            .layout(1, source.len(), usize::MAX, 0, true)
            .rows
            .is_empty());
    }

    fn styles(lines: &[Line<'_>]) -> Vec<Vec<(char, Style)>> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .flat_map(|span| {
                        let style = line.style.patch(span.style);
                        span.content.chars().map(move |c| (c, style))
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn completed_markdown_blocks_match_shared_text_and_styles() {
        for source in [
            "# Heading\n\n**bold *both* bold** and `code`\n\n- first\n- second",
            "> quoted *text*\n\n[link](https://example.invalid) ~~gone~~",
            "```rust\nlet café = 1;\n```",
            "```diff file.rs\n+let x = 1;\n-old\n```",
            "| Name | Value |\n| --- | ---: |\n| café | 界 |",
            "**safe&#27;[31mred&#27;]payload&#7;end**",
        ] {
            let MarkdownChunk::Styled(mut cursor) = render_markdown_chunk(source, 300, 1024) else {
                panic!("ordinary completed block should be styled: {source}");
            };
            let mut lines = Vec::new();
            loop {
                let page = cursor.page(1);
                for row in page.rows {
                    assert_eq!(row.source, 300..300 + source.len());
                    lines.push(row.line);
                }
                if page.finished {
                    break;
                }
            }
            assert_eq!(styles(&lines), styles(&render_markdown(source)), "{source}");
        }
    }

    #[test]
    fn markdown_width_one_paginates_generated_rows_without_source_fiction() {
        let MarkdownChunk::Styled(mut cursor) = render_markdown_chunk("---", 50, 1) else {
            panic!("bounded rule should be styled");
        };
        assert!(cursor.page(0).rows.is_empty());
        let mut rows = Vec::new();
        loop {
            let page = cursor.page(3);
            assert!(page.rows.len() <= 3);
            assert!(page.rows.iter().all(|row| row.source == (50..53)));
            rows.extend(page.rows);
            if page.finished {
                break;
            }
        }
        assert_eq!(text(&rows), "─".repeat(48));
        assert!(rows.len() >= 48);
    }

    #[test]
    fn integrated_markdown_ordinals_resume_generated_rows_and_keep_styles() {
        let source = "# Heading\n\n---\n\n**bold** and *italic*";
        let mut clean = ScanState::default().scan(source, 70);
        clean.set_complete(true);
        let mut next = (70, 0);
        let mut rows = Vec::new();
        loop {
            let page = clean.layout(1, next.0, next.1, 3, false);
            if page.rows.is_empty() {
                assert_eq!(page.next_offset, clean.source_end());
                break;
            }
            let following = (page.next_offset, page.next_ordinal);
            assert!(following > next);
            next = following;
            rows.extend(page.rows);
        }
        assert!(rows.len() > source.len());
        assert!(rows
            .windows(2)
            .all(|rows| rows[0].source.start < rows[1].source.start
                || rows[0].ordinal_end == rows[1].ordinal));
        let tail = clean.layout(1, clean.source_end(), usize::MAX, 5, true);
        assert_eq!(tail.rows, rows[rows.len() - 5..]);
        let all = clean.layout(1024, 70, 0, 256, false);
        let lines: Vec<_> = all.rows.into_iter().map(|row| row.line).collect();
        assert_eq!(styles(&lines), styles(&render_markdown(source)));
    }

    #[test]
    fn complete_blocks_preserve_shared_markdown_spacing_and_source_anchors() {
        let source = "**first paragraph**\n\n# Heading\n\nsecond *paragraph*\n\n```text\ncode\n```\n\n- one\n- two\n\n---\n\nlast";
        let mut clean = ScanState::default().scan(source, 0);
        clean.set_complete(true);
        let rows = clean.layout(1024, 0, 0, MAX_PAGE_ROWS, false).rows;
        let lines: Vec<_> = rows.iter().map(|row| row.line.clone()).collect();
        assert_eq!(styles(&lines), styles(&render_markdown(source)));
        let second = rows
            .iter()
            .find(|row| row.line.to_string().contains("second paragraph"))
            .unwrap();
        assert_eq!(second.source.start, source.find("second").unwrap());
        assert_eq!(second.ordinal, 0);
        let mut scan = ScanState::default();
        let mut offset = 0;
        let mut paged = Vec::new();
        for end in clean
            .boundary_sources
            .iter()
            .copied()
            .chain(std::iter::once(source.len()))
        {
            if end == offset {
                continue;
            }
            let mut page = scan.scan(&source[offset..end], offset);
            page.set_complete(end == source.len());
            scan.finish_layout_page(&page);
            paged.extend(page.layout(1024, offset, 0, MAX_PAGE_ROWS, false).rows);
            offset = end;
        }
        assert_eq!(paged, rows);
    }

    #[test]
    fn wide_alternating_styles_keep_all_text_with_bounded_row_spans() {
        let source = "**bold** *italic* ".repeat(150);
        let MarkdownChunk::Styled(mut cursor) = render_markdown_chunk(&source, 0, MAX_WIDTH) else {
            panic!("bounded alternating styles should remain Markdown");
        };
        let mut lines = Vec::new();
        loop {
            let page = cursor.page(7);
            for row in page.rows {
                assert!(row.line.spans.len() <= MAX_ROW_SPANS);
                assert!(allocated_row_bytes(&row) < 16 * 1024);
                lines.push(row.line);
            }
            if page.finished {
                break;
            }
        }
        assert!(lines.len() > 1);
        assert_eq!(
            styles(&lines).into_iter().flatten().collect::<Vec<_>>(),
            styles(&render_markdown(&source))
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn ordinary_multi_kibibyte_code_and_paragraphs_keep_markdown() {
        for bytes in [3 * 1024, 8 * 1024] {
            let code = format!(
                "```rust\n{}\n```",
                "let value = compute(input); // ordinary highlighted Rust code\n"
                    .repeat(bytes / 60)
            );
            let paragraph = format!("**{}**", "ordinary words ".repeat(bytes / 15).trim_end());
            for source in [code, paragraph] {
                assert!(source.len() > 2048 && source.len() <= MAX_STYLED_BLOCK_BYTES);
                let expected = render_markdown(&source);
                let MarkdownChunk::Styled(mut cursor) =
                    render_markdown_chunk(&source, 0, MAX_WIDTH)
                else {
                    panic!("ordinary {}-byte Markdown was made literal", source.len());
                };
                assert!(cursor.allocated_bytes() <= MAX_MARKDOWN_CURSOR_BYTES);
                let mut lines = Vec::new();
                loop {
                    let page = cursor.page(7);
                    assert!(page.rows.len() <= 7);
                    lines.extend(page.rows.into_iter().map(|row| row.line));
                    if page.finished {
                        break;
                    }
                }
                assert_eq!(
                    styles(&lines).into_iter().flatten().collect::<Vec<_>>(),
                    styles(&expected).into_iter().flatten().collect::<Vec<_>>()
                );
                let mut clean = ScanState::default().scan(&source, 0);
                clean.set_complete(true);
                let layout = clean.layout(MAX_WIDTH, 0, 0, MAX_PAGE_ROWS, false);
                let actual: Vec<_> = layout.rows.into_iter().map(|row| row.line).collect();
                assert_eq!(
                    styles(&actual).into_iter().flatten().collect::<Vec<_>>(),
                    styles(&expected).into_iter().flatten().collect::<Vec<_>>()
                );
            }
        }
    }

    #[test]
    fn completed_markdown_trims_repeated_trailing_blank_blocks() {
        for source in [
            "**first**\n\n\n",
            "# Heading\n\n\n\n",
            "```text\ncode\n```\n\n\n",
        ] {
            let mut clean = ScanState::default().scan(source, 0);
            clean.set_complete(true);
            let lines: Vec<_> = clean
                .layout(1024, 0, 0, MAX_PAGE_ROWS, false)
                .rows
                .into_iter()
                .map(|row| row.line)
                .collect();
            assert_eq!(styles(&lines), styles(&render_markdown(source)));
        }
    }

    #[test]
    fn styled_resize_keeps_the_same_visible_content_not_the_same_wrapped_row() {
        let source = format!(
            "**{}**",
            (0..100)
                .map(|i| format!("word{i:03}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        assert!(render_markdown(&source)
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)));
        let mut clean = ScanState::default().scan(&source, 70);
        clean.set_complete(true);
        let wide = clean.layout(48, 70, 0, MAX_PAGE_ROWS, false);
        let anchor = &wide.rows[8];
        let marker = anchor.line.to_string()[..7].to_string();
        for width in [13, 31, 81] {
            let resized = clean.layout(width, anchor.source.start, anchor.ordinal, 16, false);
            let first = &resized.rows[0];
            assert!(first.ordinal <= anchor.ordinal && first.ordinal_end > anchor.ordinal);
            assert!(
                text(&resized.rows).contains(&marker),
                "width={width}: {}",
                text(&resized.rows)
            );
            let visible = first.line.to_string();
            let delta = anchor.ordinal - first.ordinal;
            assert!(
                visible[delta..].starts_with(&marker[..marker.len().min(visible.len() - delta)])
            );
            assert!(first.line.spans.iter().any(|span| span
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)));
        }
    }

    #[test]
    fn generated_markdown_rows_keep_byte_anchors_when_resized() {
        let mut clean = ScanState::default().scan("---", 50);
        clean.set_complete(true);
        let narrow = clean.layout(1, 50, 0, MAX_PAGE_ROWS, false);
        let anchor = &narrow.rows[20];
        let wide = clean.layout(9, 50, anchor.ordinal, 4, false);
        assert_eq!(
            wide.rows[0].ordinal,
            narrow.rows[1].ordinal + 18 * "─".len()
        );
        assert!(wide.rows[0].ordinal_end > anchor.ordinal);
        assert_eq!(wide.rows[0].line.to_string(), "─".repeat(9));
    }

    #[test]
    fn fence_continuation_does_not_style_hidden_interior_boundaries() {
        let mut scan = ScanState::default();
        let first = scan.scan("```rust\nlet value = 1;\n\n", 0);
        let second = scan.scan("**not bold**\n\n```\n\n**bold**\n\n", first.source_end());
        let first_rows = first.layout(80, 0, 0, 256, false).rows;
        assert_eq!(text(&first_rows), "```rustlet value = 1;");
        let second_rows = second.layout(80, second.source.start, 0, 256, false).rows;
        assert!(text(&second_rows).starts_with("**not bold**```"));
        let bold = second_rows
            .iter()
            .flat_map(|row| &row.line.spans)
            .find(|span| span.content == "bold")
            .expect("completed following paragraph");
        assert!(bold
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD));
    }

    #[test]
    fn pathological_markdown_falls_back_before_unbounded_layout() {
        for source in [
            "x".repeat(MAX_INPUT_BYTES + 1),
            format!("```rust\n{}\n```", "x".repeat(2049)),
            format!("{}quoted", "> ".repeat(20)),
            format!(
                "| {} | y |\n|---|---|\n{}",
                "x".repeat(1500),
                "|a|b|\n".repeat(100)
            ),
            "safe\u{1b}]unterminated".to_string(),
        ] {
            assert!(matches!(
                render_markdown_chunk(&source, 0, 1),
                MarkdownChunk::Literal(_)
            ));
            assert_eq!(
                text(&rendered(&source, 1, 31, 3)),
                safe_display_text(&source).replace('\n', "")
            );
        }
    }
}
