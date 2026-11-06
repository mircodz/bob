use bob_kernel::{OperationState, ThreadState};
pub(super) use bob_ui::theme::Palette;
use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub(super) use bob_ui::widgets::{BAND_INSET, BAND_MARGIN};

pub(super) fn text() -> Style {
    Style::default().fg(Palette::TEXT())
}
pub(super) fn bold() -> Style {
    text().add_modifier(Modifier::BOLD)
}
pub(super) fn inset(area: Rect, pad: u16) -> Rect {
    let pad = pad.min(area.width / 2);
    Rect::new(
        area.x + pad,
        area.y,
        area.width.saturating_sub(pad * 2),
        area.height,
    )
}
pub(super) fn state_color(state: &OperationState) -> Color {
    match state {
        OperationState::Done => Palette::OK(),
        OperationState::Failed => Palette::ERROR(),
        OperationState::Pending
        | OperationState::Preparing
        | OperationState::Ready
        | OperationState::Approval
        | OperationState::Started
        | OperationState::Waiting
        | OperationState::Unknown
        | OperationState::Interrupted => Palette::WARN(),
        OperationState::Cancelled | OperationState::Resolved => Palette::DIM(),
    }
}
pub(super) fn thread_color(state: &ThreadState) -> Color {
    match state {
        ThreadState::Completed => Palette::OK(),
        ThreadState::Failed => Palette::ERROR(),
        ThreadState::Active => Palette::RUNNING(),
        ThreadState::Held | ThreadState::Stopping | ThreadState::Interrupted => Palette::WARN(),
        ThreadState::Idle | ThreadState::Cancelled => Palette::FAINT(),
    }
}
pub(super) fn terminal_thread(state: &ThreadState) -> bool {
    matches!(
        state,
        ThreadState::Completed | ThreadState::Failed | ThreadState::Cancelled
    )
}

#[derive(Default)]
enum ControlState {
    #[default]
    Text,
    Escape,
    EscapeIntermediate,
    Csi,
    String {
        osc: bool,
        escape: bool,
    },
}

impl ControlState {
    fn visible(&mut self, c: char) -> bool {
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
            return false;
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
            '\n' | '\t' => return true,
            c if c.is_control() => {}
            _ => match self {
                Self::Text => return true,
                Self::Escape => {
                    *self = match c {
                        '[' => Self::Csi,
                        ']' | 'P' | 'X' | '^' | '_' => Self::String {
                            osc: c == ']',
                            escape: false,
                        },
                        '\u{20}'..='\u{2f}' => Self::EscapeIntermediate,
                        '\u{30}'..='\u{7e}' => Self::Text,
                        _ => {
                            *self = Self::Text;
                            return true;
                        }
                    };
                }
                Self::EscapeIntermediate => match c {
                    '\u{20}'..='\u{2f}' => {}
                    '\u{30}'..='\u{7e}' => *self = Self::Text,
                    _ => {
                        *self = Self::Text;
                        return true;
                    }
                },
                Self::Csi => match c {
                    '\u{20}'..='\u{3f}' => {}
                    '\u{40}'..='\u{7e}' => *self = Self::Text,
                    _ => {
                        *self = Self::Text;
                        return true;
                    }
                },
                Self::String { .. } => unreachable!(),
            },
        }
        false
    }
}

pub(super) fn safe(text: &str) -> String {
    let mut state = ControlState::default();
    let mut clean = String::with_capacity(text.len());
    for c in text.chars().filter(|&c| state.visible(c)) {
        if c == '\t' {
            clean.push_str("    ");
        } else {
            clean.push(c);
        }
    }
    clean
}

struct StyledChar {
    start: usize,
    source: usize,
    style: Style,
}

struct Grapheme {
    start: usize,
    end: usize,
    source: usize,
    width: usize,
    whitespace: bool,
    newline: bool,
}

pub(super) fn wrap(line: &Line<'_>, width: usize) -> Vec<(usize, Line<'static>)> {
    wrap_styled(line, width, false)
}

pub(super) fn prose_wrap(line: &Line<'_>, width: usize) -> Vec<(usize, Line<'static>)> {
    wrap_styled(line, width, true)
}

fn wrap_styled(line: &Line<'_>, width: usize, prose: bool) -> Vec<(usize, Line<'static>)> {
    let width = width.max(1);
    let mut clean = String::new();
    let mut chars = Vec::new();
    let mut state = ControlState::default();
    let mut source = 0;
    for span in &line.spans {
        for (offset, c) in span.content.char_indices() {
            if !state.visible(c) {
                continue;
            }
            for _ in 0..if c == '\t' { 4 } else { 1 } {
                chars.push(StyledChar {
                    start: clean.len(),
                    source: source + offset,
                    style: span.style,
                });
                clean.push(if c == '\t' { ' ' } else { c });
            }
        }
        source += span.content.len();
    }
    let graphemes: Vec<_> = clean
        .grapheme_indices(true)
        .map(|(start, text)| {
            let index = chars.partition_point(|c| c.start < start);
            Grapheme {
                start,
                end: start + text.len(),
                source: chars[index].source,
                width: text.width(),
                whitespace: text.chars().all(char::is_whitespace),
                newline: text == "\n",
            }
        })
        .collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut anchor = 0;
    loop {
        let mut end = start;
        let mut columns = 0;
        let mut word_seen = false;
        let mut boundary = None;
        while let Some(g) = graphemes.get(end) {
            if g.newline {
                break;
            }
            if prose
                && g.whitespace
                && word_seen
                && (end == start || !graphemes[end - 1].whitespace)
            {
                boundary = Some(end);
            }
            if columns + g.width > width && columns > 0 {
                break;
            }
            word_seen |= !g.whitespace;
            columns += g.width;
            end += 1;
        }
        let mut next = end;
        let newline = graphemes.get(end).is_some_and(|g| g.newline);
        if prose && end < graphemes.len() && !newline {
            if let Some(boundary) = boundary {
                end = boundary;
                next = boundary;
                while graphemes
                    .get(next)
                    .is_some_and(|g| g.whitespace && !g.newline)
                {
                    next += 1;
                }
            }
        }
        let byte_start = graphemes.get(start).map_or(clean.len(), |g| g.start);
        let byte_end = if end > start {
            graphemes[end - 1].end
        } else {
            byte_start
        };
        out.push((
            anchor,
            styled_slice(&clean, &chars, byte_start, byte_end, line.style),
        ));
        if next == graphemes.len() {
            break;
        }
        if graphemes[next].newline {
            anchor = graphemes[next].source + 1;
            start = next + 1;
        } else {
            anchor = graphemes[next].source;
            start = next;
        }
    }
    out
}

fn styled_slice(
    text: &str,
    chars: &[StyledChar],
    start: usize,
    end: usize,
    style: Style,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut index = chars.partition_point(|c| c.start < start);
    while index < chars.len() && chars[index].start < end {
        let part_start = chars[index].start;
        let part_style = chars[index].style;
        let mut part_end = part_start;
        for grapheme in text[part_start..end].graphemes(true) {
            let next = part_end + grapheme.len();
            index = chars.partition_point(|c| c.start < next);
            part_end = next;
            if index >= chars.len() || chars[index].start >= end || chars[index].style != part_style
            {
                break;
            }
        }
        spans.push(Span::styled(
            text[part_start..part_end].to_owned(),
            part_style,
        ));
    }
    Line::from(spans).style(style)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[(usize, Line<'_>)]) -> Vec<(usize, String)> {
        lines
            .iter()
            .map(|(offset, line)| (*offset, line.to_string()))
            .collect()
    }

    #[test]
    fn sanitizer_consumes_terminal_sequences_and_incomplete_tails() {
        for sequence in [
            "\x1b[31m",
            "\x1b[?25l",
            "\x1b[1;2H",
            "\x1b[2J",
            "\x1b]52;c;payload\x07",
            "\x1b]0;title\x1b\\",
            "\x1bPdata\nmore\x1b\\",
            "\x1bXdata\x1b\\",
            "\x1b^data\x1b\\",
            "\x1b_data\x1b\\",
            "\x1b(B",
            "\x1b7",
            "\x1bc",
            "\u{9b}31m",
            "\u{9d}payload\u{9c}",
            "\u{90}data\u{9c}",
            "\u{98}data\u{9c}",
            "\u{9e}data\u{9c}",
            "\u{9f}data\u{9c}",
            "\x1b]title\u{9c}",
            "\u{9d}title\x1b\\",
        ] {
            assert_eq!(safe(&format!("before{sequence}後\n\t")), "before後\n    ");
        }
        for tail in [
            "\x1b",
            "\x1b[",
            "\x1b[31;",
            "\x1b(",
            "\x1b]title",
            "\u{90}data\x1b",
        ] {
            assert_eq!(safe(&format!("before{tail}")), "before");
        }
        assert_eq!(safe("a\0\x07\x08\r\x7f\u{85}b\n\t"), "ab\n    ");
        assert_eq!(safe("a\x1b[31\x18b"), "ab");
        assert_eq!(safe("a\x1b[\x1b[31mb"), "ab");
        assert_eq!(safe("a\x1b[日本語"), "a日本語");
        assert_eq!(safe("a\x1b]hidden\x1b[2Jstill hidden\x07b"), "ab");
    }

    #[test]
    fn sanitization_carries_control_state_across_styled_spans() {
        let raw = "a\x1b]title\x1b\\界\x1b[31mb\x1bPtail";
        for split in raw.char_indices().map(|(offset, _)| offset) {
            let line = Line::from(vec![
                Span::styled(&raw[..split], bold()),
                Span::styled(&raw[split..], text()),
            ]);
            assert_eq!(rows(&wrap(&line, 80)), [(0, "a界b".to_owned())]);
        }
    }

    #[test]
    fn literal_wrap_preserves_whitespace_and_original_byte_anchors() {
        let line = Line::from(Span::raw(" a  b \n\tc\n\n"));
        assert_eq!(
            rows(&wrap(&line, 3)),
            [
                (0, " a "),
                (3, " b "),
                (7, "   "),
                (7, " c"),
                (10, ""),
                (11, "")
            ]
            .map(|(offset, text)| (offset, text.to_owned()))
        );
        let raw = "\x1b[31m界\tZ";
        assert_eq!(
            rows(&wrap(&Line::raw(raw), 2)),
            [(0, "界"), (8, "  "), (8, "  "), (9, "Z")]
                .map(|(offset, text)| (offset, text.to_owned()))
        );
    }

    #[test]
    fn graphemes_cross_styles_without_splitting_or_losing_styles() {
        let a = Style::default().fg(Palette::ACCENT());
        let b = bold();
        let c = Style::default().fg(Palette::WARN());
        let line = Line::from(vec![
            Span::styled("x👩", a),
            Span::styled("\u{200d}💻e", b),
            Span::styled("\u{301}界", c),
        ])
        .style(text());
        let wrapped = wrap(&line, 2);
        assert_eq!(
            rows(&wrapped),
            [(0, "x"), (1, "👩\u{200d}💻"), (12, "e\u{301}"), (15, "界")]
                .map(|(offset, text)| (offset, text.to_owned()))
        );
        assert_eq!(wrapped[1].1.spans, [Span::styled("👩\u{200d}💻", a)]);
        assert_eq!(wrapped[2].1.spans, [Span::styled("e\u{301}", b)]);
        assert!(wrapped.iter().all(|(_, row)| row.width() <= 2));
        assert!(wrapped.iter().all(|(_, row)| row.style == line.style));
    }

    #[test]
    fn prose_wrap_keeps_words_across_styles_and_unicode_source_anchors() {
        let a = Style::default().fg(Palette::ACCENT());
        let b = bold();
        let line = Line::from(vec![
            Span::raw("one "),
            Span::styled("co", a),
            Span::styled("lo", b),
            Span::styled("red", a),
            Span::raw(" 界e\u{301} tail"),
        ])
        .style(text());
        let wrapped = prose_wrap(&line, 9);
        assert_eq!(
            rows(&wrapped),
            [(0, "one"), (4, "colored"), (12, "界e\u{301} tail")]
                .map(|(offset, text)| (offset, text.to_owned()))
        );
        assert_eq!(
            wrapped[1].1.spans,
            [
                Span::styled("co", a),
                Span::styled("lo", b),
                Span::styled("red", a),
            ]
        );
        assert!(wrapped.iter().all(|(_, row)| row.style == line.style));
        assert_eq!(
            rows(&prose_wrap(&Line::raw("界👩\u{200d}💻e\u{301}Z"), 4)),
            [(0, "界👩\u{200d}💻"), (14, "e\u{301}Z")]
                .map(|(offset, text)| (offset, text.to_owned()))
        );
    }

    #[test]
    fn prose_preserves_interior_spacing_and_explicit_empty_lines() {
        let line = Line::from(Span::raw("  a  b\n\nc\n"));
        assert_eq!(
            rows(&prose_wrap(&line, 6)),
            [(0, "  a  b"), (7, ""), (8, "c"), (10, "")]
                .map(|(offset, text)| (offset, text.to_owned()))
        );
        assert_eq!(
            rows(&prose_wrap(&Line::raw("a   b"), 3)),
            [(0, "a"), (4, "b")].map(|(offset, text)| (offset, text.to_owned()))
        );
    }

    #[test]
    fn crlf_and_tab_source_anchors_survive_styled_span_boundaries() {
        let raw = "\x1b[31m界\r\n\tZ\r\n\r\n";
        for split in raw.char_indices().map(|(offset, _)| offset) {
            let line = Line::from(vec![
                Span::styled(&raw[..split], bold()),
                Span::styled(&raw[split..], text()),
            ]);
            assert_eq!(
                rows(&wrap(&line, 2)),
                [
                    (0, "界"),
                    (10, "  "),
                    (10, "  "),
                    (11, "Z"),
                    (14, ""),
                    (16, "")
                ]
                .map(|(offset, text)| (offset, text.to_owned())),
                "split at {split}"
            );
        }
        let raw = "  a  b\r\n\r\nc\r\n";
        for split in raw.char_indices().map(|(offset, _)| offset) {
            let line = Line::from(vec![
                Span::styled(&raw[..split], bold()),
                Span::styled(&raw[split..], text()),
            ]);
            assert_eq!(
                rows(&prose_wrap(&line, 6)),
                [(0, "  a  b"), (8, ""), (10, "c"), (13, "")]
                    .map(|(offset, text)| (offset, text.to_owned())),
                "split at {split}"
            );
        }
    }

    #[test]
    fn narrow_and_empty_layouts_make_progress() {
        for prose in [false, true] {
            assert_eq!(
                rows(&wrap_styled(&Line::default(), 0, prose)),
                [(0, String::new())]
            );
            assert_eq!(
                rows(&wrap_styled(&Line::raw("界e\u{301}"), 0, prose)),
                [(0, "界"), (3, "e\u{301}")].map(|(offset, text)| (offset, text.to_owned()))
            );
            assert_eq!(
                rows(&wrap_styled(&Line::raw("\u{301}"), 0, prose)),
                [(0, "\u{301}".to_owned())]
            );
        }
    }

    #[test]
    fn typed_states_distinguish_terminal_and_recoverable_threads() {
        assert_eq!(state_color(&OperationState::Done), Palette::OK());
        assert_eq!(state_color(&OperationState::Unknown), Palette::WARN());
        assert_eq!(thread_color(&ThreadState::Completed), Palette::OK());
        assert_eq!(thread_color(&ThreadState::Failed), Palette::ERROR());
        for state in [
            ThreadState::Completed,
            ThreadState::Failed,
            ThreadState::Cancelled,
        ] {
            assert!(terminal_thread(&state));
        }
        for state in [
            ThreadState::Idle,
            ThreadState::Held,
            ThreadState::Active,
            ThreadState::Stopping,
            ThreadState::Interrupted,
        ] {
            assert!(!terminal_thread(&state));
        }
    }
}
