//! Markdown → ratatui Lines pre-renderer. Mirrors the TS markdown_render pivot:
//! parse the whole block with pulldown-cmark, emit styled `Line`s once, rather
//! than using a live widget. Code fences go through syntect; ```diff fences go
//! through the diff renderer.

use super::diffview::render_diff;
use super::highlight::highlight;
use super::theme::Palette;
use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Render a markdown string into styled lines.
pub fn render_markdown(md: &str) -> Vec<Line<'static>> {
    let mut r = Renderer::default();
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    let parser = Parser::new_ext(md, opts);
    for event in parser {
        r.event(event);
    }
    r.finish();
    r.lines
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    /// Spans accumulated for the current line.
    cur: Vec<Span<'static>>,
    /// Active inline style stack (bold/italic/etc combine).
    style: Style,
    style_stack: Vec<Style>,
    /// List nesting: each entry is Some(next_number) for ordered, None for bullet.
    list_stack: Vec<Option<u64>>,
    blockquote_depth: usize,
    // Code fence state.
    in_code: bool,
    code_lang: String,
    code_buf: String,
    // Table state.
    in_table: bool,
    aligns: Vec<Alignment>,
    table_rows: Vec<Vec<String>>,
    cur_row: Vec<String>,
    cell_buf: String,
    in_header: bool,
}

impl Renderer {
    fn push_span(&mut self, text: impl Into<String>, style: Style) {
        // Default any span without an explicit foreground to the theme's body
        // text color, so plain text is never left to inherit the terminal's own
        // foreground (which would be unreadable on a forced light/dark bg).
        let style = if style.fg.is_none() {
            style.fg(Palette::TEXT())
        } else {
            style
        };
        self.cur.push(Span::styled(text.into(), style));
    }

    /// Commit the current spans as a line (prefixing blockquote bars).
    fn flush_line(&mut self) {
        let mut spans = Vec::new();
        for _ in 0..self.blockquote_depth {
            spans.push(Span::styled("▏ ", Style::default().fg(Palette::BLOCKQUOTE_BAR())));
        }
        spans.append(&mut self.cur);
        self.lines.push(Line::from(spans));
    }

    fn blank(&mut self) {
        // Avoid stacking multiple blank lines.
        if matches!(self.lines.last(), Some(l) if l.width() == 0) {
            return;
        }
        self.lines.push(Line::from(""));
    }

    fn text(&mut self, t: &str) {
        if self.in_code {
            self.code_buf.push_str(t);
        } else if self.in_table {
            self.cell_buf.push_str(t);
        } else {
            self.push_span(t.to_string(), self.style);
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => self.text(&t),
            Event::Code(t) => {
                let s = Style::default().fg(Palette::INLINE_CODE());
                if self.in_table {
                    self.cell_buf.push_str(&t);
                } else {
                    self.push_span(t.to_string(), s);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if self.in_table {
                    self.cell_buf.push(' ');
                } else {
                    self.flush_line();
                }
            }
            Event::Rule => {
                self.blank();
                self.push_span("─".repeat(48), Style::default().fg(Palette::RULE()));
                self.flush_line();
                self.blank();
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                self.blank();
                let hashes = "#".repeat(heading_num(level));
                self.push_span(
                    format!("{} ", hashes),
                    Style::default().fg(Palette::FAINT()),
                );
                self.style = Style::default()
                    .fg(Palette::HEADING())
                    .add_modifier(Modifier::BOLD);
            }
            Tag::BlockQuote(_) => {
                self.blockquote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.blank();
                self.in_code = true;
                self.code_buf.clear();
                // Keep the full info string ("diff", "rust", or "diff src/x.rs")
                // so the diff renderer can pick a syntax from the tagged path.
                self.code_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(info) => info.to_string(),
                    _ => String::new(),
                };
            }
            Tag::List(start) => {
                self.list_stack.push(start);
            }
            Tag::Item => {
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{}. ", n);
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.push_span(indent, Style::default());
                self.push_span(marker, Style::default().fg(Palette::LIST_MARKER()));
            }
            Tag::Emphasis => {
                self.style_stack.push(self.style);
                self.style = self.style.add_modifier(Modifier::ITALIC);
            }
            Tag::Strong => {
                self.style_stack.push(self.style);
                self.style = self.style.add_modifier(Modifier::BOLD);
            }
            Tag::Strikethrough => {
                self.style_stack.push(self.style);
                self.style = self.style.add_modifier(Modifier::CROSSED_OUT);
            }
            Tag::Link { .. } => {
                self.style_stack.push(self.style);
                self.style = self
                    .style
                    .fg(Palette::LINK())
                    .add_modifier(Modifier::UNDERLINED);
            }
            Tag::Table(aligns) => {
                self.in_table = true;
                self.aligns = aligns;
                self.table_rows.clear();
            }
            Tag::TableHead => {
                self.in_header = true;
            }
            Tag::TableRow => {
                self.cur_row.clear();
            }
            Tag::TableCell => {
                self.cell_buf.clear();
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_line();
                self.blank();
            }
            TagEnd::Heading(_) => {
                self.style = Style::default();
                self.flush_line();
                self.blank();
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.blank();
            }
            TagEnd::CodeBlock => {
                self.in_code = false;
                let code = std::mem::take(&mut self.code_buf);
                let code = code.strip_suffix('\n').unwrap_or(&code).to_string();
                // Split the info string into first token + remainder, e.g.
                // "diff src/x.rs" → ("diff", "src/x.rs"); "rust" → ("rust", "").
                let mut parts = self.code_lang.splitn(2, char::is_whitespace);
                let kind = parts.next().unwrap_or("");
                let arg = parts.next().unwrap_or("").trim();
                if kind == "diff" {
                    for l in render_diff(&code, arg) {
                        self.lines.push(l);
                    }
                } else {
                    for l in highlight(&code, kind) {
                        self.lines.push(l);
                    }
                }
                self.blank();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.blank();
                }
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Link => {
                if let Some(s) = self.style_stack.pop() {
                    self.style = s;
                }
            }
            TagEnd::Table => {
                self.render_table();
                self.in_table = false;
            }
            TagEnd::TableHead => {
                self.in_header = false;
                self.table_rows.push(std::mem::take(&mut self.cur_row));
            }
            TagEnd::TableRow => {
                self.table_rows.push(std::mem::take(&mut self.cur_row));
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.cell_buf);
                self.cur_row.push(cell);
            }
            _ => {}
        }
    }

    /// Draw the accumulated table with box borders and column alignment.
    fn render_table(&mut self) {
        let rows = std::mem::take(&mut self.table_rows);
        if rows.is_empty() {
            return;
        }
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut widths = vec![0usize; cols];
        for r in &rows {
            for (i, c) in r.iter().enumerate() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }

        let border = Style::default().fg(Palette::TABLE_BORDER());
        let sep = |l: &str, m: &str, r: &str, widths: &[usize]| -> Line<'static> {
            let mut s = String::from(l);
            for (i, w) in widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2));
                s.push_str(if i + 1 == widths.len() { r } else { m });
            }
            Line::from(Span::styled(s, border))
        };

        self.blank();
        self.lines.push(sep("┌", "┬", "┐", &widths));
        for (ri, row) in rows.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled("│", border));
            for (ci, w) in widths.iter().enumerate() {
                let cell = row.get(ci).cloned().unwrap_or_default();
                let pad = pad_cell(&cell, *w, self.aligns.get(ci).copied().unwrap_or(Alignment::None));
                let style = if ri == 0 {
                    Style::default().fg(Palette::HEADING()).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Palette::TEXT())
                };
                spans.push(Span::styled(format!(" {} ", pad), style));
                spans.push(Span::styled("│", border));
            }
            self.lines.push(Line::from(spans));
            if ri == 0 {
                self.lines.push(sep("├", "┼", "┤", &widths));
            }
        }
        self.lines.push(sep("└", "┴", "┘", &widths));
        self.blank();
    }

    fn finish(&mut self) {
        if !self.cur.is_empty() {
            self.flush_line();
        }
        // Trim a trailing blank line.
        while matches!(self.lines.last(), Some(l) if l.width() == 0) {
            self.lines.pop();
        }
    }
}

fn pad_cell(text: &str, width: usize, align: Alignment) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let pad = width - len;
    match align {
        Alignment::Right => format!("{}{}", " ".repeat(pad), text),
        Alignment::Center => {
            let left = pad / 2;
            let right = pad - left;
            format!("{}{}{}", " ".repeat(left), text, " ".repeat(right))
        }
        _ => format!("{}{}", text, " ".repeat(pad)),
    }
}

fn heading_num(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}
