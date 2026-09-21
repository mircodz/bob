use super::highlight::highlight_line;
use super::theme::Palette;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Rows use `"± NNNN| text"` from `format_unified`; bare lines are also accepted.
pub fn render_diff(body: &str, lang: &str) -> Vec<Line<'static>> {
    body.split('\n').map(|raw| render_row(raw, lang)).collect()
}

fn render_row(raw: &str, lang: &str) -> Line<'static> {
    let (sign, num, content) = parse_row(raw);

    if content == "..." && sign == ' ' {
        return Line::from(Span::styled(
            "     ...".to_string(),
            Style::default().fg(Palette::FAINT()),
        ));
    }

    let (sign_color, bg) = match sign {
        '+' => (Palette::DIFF_ADD(), Some(Palette::DIFF_ADD_BG())),
        '-' => (Palette::DIFF_REMOVE(), Some(Palette::DIFF_REMOVE_BG())),
        _ => (Palette::DIFF_GUTTER(), None),
    };

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(
        format!("{} ", sign),
        Style::default().fg(sign_color).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(
        format!("{:>4} ", num),
        Style::default().fg(Palette::DIFF_GUTTER()),
    ));

    let mut content_spans = highlight_line(content, lang);
    if let Some(bg) = bg {
        for s in &mut content_spans {
            s.style = s.style.bg(bg);
        }
    }
    spans.extend(content_spans);
    Line::from(spans)
}

fn parse_row(raw: &str) -> (char, String, &str) {
    let mut chars = raw.chars();
    let sign = match chars.next() {
        Some(c @ ('+' | '-' | ' ')) => c,
        _ => return (' ', String::new(), raw),
    };
    let rest = chars.as_str().trim_start_matches(' ');
    if let Some(bar) = rest.find('|') {
        let num = rest[..bar].trim().to_string();
        let content = rest[bar + 1..]
            .strip_prefix(' ')
            .unwrap_or(&rest[bar + 1..]);
        (sign, num, content)
    } else {
        (sign, String::new(), rest)
    }
}

pub fn diff_header(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default()
            .fg(Palette::DIM())
            .add_modifier(Modifier::BOLD),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_parser_preserves_indentation_unicode_and_embedded_bars() {
        for (raw, sign, number, content) in [
            ("+   12|   日本 | value", '+', "12", "  日本 | value"),
            ("-    7| old", '-', "7", "old"),
            ("     8|same", ' ', "8", "same"),
            ("+ bare addition", '+', "", "bare addition"),
            ("-", '-', "", ""),
            ("...", ' ', "", "..."),
            ("日本", ' ', "", "日本"),
            ("", ' ', "", ""),
        ] {
            assert_eq!(parse_row(raw), (sign, number.into(), content), "{raw:?}");
        }
    }

    #[test]
    fn diff_rows_keep_gutters_separate_from_content_backgrounds() {
        let rows = render_diff(
            "+   12| new\n-    7| old\n     8| same\n...",
            "unknown-language",
        );
        assert_eq!(
            rows.iter().map(ToString::to_string).collect::<Vec<_>>(),
            vec!["+   12 new", "-    7 old", "     8 same", "     ...",]
        );
        for (row, background) in rows[..3].iter().zip([
            Some(Palette::DIFF_ADD_BG()),
            Some(Palette::DIFF_REMOVE_BG()),
            None,
        ]) {
            assert_eq!(row.spans[0].style.bg, None);
            assert_eq!(row.spans[1].style.bg, None);
            assert!(row.spans[0].style.add_modifier.contains(Modifier::BOLD));
            assert!(row.spans[2..]
                .iter()
                .all(|span| span.style.bg == background));
        }
        assert_eq!(rows[3].spans.len(), 1);
        assert_eq!(rows[3].spans[0].style.fg, Some(Palette::FAINT()));
    }
}
