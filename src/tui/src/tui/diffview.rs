//! Render a unified-diff text block (as emitted by the edit tools inside a
//! ```diff fence) into pretty ratatui lines: a sign + line-number gutter, the
//! content syntax-highlighted, and add/remove rows tinted green/red.

use super::highlight::highlight_line;
use super::theme::Palette;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Turn the body of a ```diff fence into styled lines. Lines are expected in the
/// `"± NNNN| text"` form emitted by `format_unified` (sign, right-aligned line
/// number, `|`, then the content). Bare lines are tolerated. `lang` selects the
/// syntax for highlighting the content (from the fence tag / file extension).
pub fn render_diff(body: &str, lang: &str) -> Vec<Line<'static>> {
    body.split('\n')
        .map(|raw| render_row(raw, lang))
        .collect()
}

fn render_row(raw: &str, lang: &str) -> Line<'static> {
    let (sign, num, content) = parse_row(raw);

    // Gap marker ("..."): dim, centered-ish.
    if content == "..." && sign == ' ' {
        return Line::from(Span::styled(
            "     ...".to_string(),
            Style::default().fg(Palette::FAINT),
        ));
    }

    let (sign_color, bg) = match sign {
        '+' => (Palette::DIFF_ADD, Some(Palette::DIFF_ADD_BG)),
        '-' => (Palette::DIFF_REMOVE, Some(Palette::DIFF_REMOVE_BG)),
        _ => (Palette::DIFF_GUTTER, None),
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    // Sign + line-number gutter.
    spans.push(Span::styled(
        format!("{} ", sign),
        Style::default().fg(sign_color).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!("{:>4} ", num),
        Style::default().fg(Palette::DIFF_GUTTER),
    ));

    // Content: syntax-highlighted, then tinted for add/remove via background.
    let mut content_spans = highlight_line(content, lang);
    if let Some(bg) = bg {
        for s in &mut content_spans {
            s.style = s.style.bg(bg);
        }
    }
    spans.extend(content_spans);
    Line::from(spans)
}

/// Parse a `"± NNNN| text"` row into (sign, line-number-string, content).
/// Falls back gracefully for rows that don't match the format.
fn parse_row(raw: &str) -> (char, String, &str) {
    let mut chars = raw.chars();
    let sign = match chars.next() {
        Some(c @ ('+' | '-' | ' ')) => c,
        _ => return (' ', String::new(), raw),
    };
    let rest = chars.as_str().trim_start_matches(' ');
    // Split "NNNN| content" on the first '|'.
    if let Some(bar) = rest.find('|') {
        let num = rest[..bar].trim().to_string();
        let content = rest[bar + 1..].strip_prefix(' ').unwrap_or(&rest[bar + 1..]);
        (sign, num, content)
    } else {
        (sign, String::new(), rest)
    }
}

/// A header line like "edited greet.py (+1 -1)".
pub fn diff_header(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Palette::DIM).add_modifier(Modifier::BOLD),
    ))
}
