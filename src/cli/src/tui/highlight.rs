use super::render::safe_display_text;
use once_cell::sync::Lazy;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

struct Highlighter {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

static HIGHLIGHTER: Lazy<Highlighter> = Lazy::new(|| Highlighter {
    syntaxes: SyntaxSet::load_defaults_newlines(),
    themes: ThemeSet::load_defaults(),
});

pub fn highlight(code: &str, lang: &str) -> Vec<Line<'static>> {
    let code = safe_display_text(code);
    let hl = &*HIGHLIGHTER;
    let theme = &hl.themes.themes["base16-eighties.dark"];

    let syntax = lang_syntax(&hl.syntaxes, lang);
    let syntax = match syntax {
        Some(s) => s,
        None => return plain(&code),
    };

    let mut h = HighlightLines::new(syntax, theme);
    let mut out = Vec::new();
    for line in code.split('\n') {
        // syntect wants the trailing newline for correct state.
        let with_nl = format!("{}\n", line);
        match h.highlight_line(&with_nl, &hl.syntaxes) {
            Ok(ranges) => {
                let spans: Vec<Span<'static>> = ranges
                    .iter()
                    .map(|(style, text)| {
                        let mut s = Style::default().fg(syntect_color(style.foreground));
                        if style.font_style.contains(FontStyle::BOLD) {
                            s = s.add_modifier(ratatui::style::Modifier::BOLD);
                        }
                        if style.font_style.contains(FontStyle::ITALIC) {
                            s = s.add_modifier(ratatui::style::Modifier::ITALIC);
                        }
                        Span::styled(text.trim_end_matches('\n').to_string(), s)
                    })
                    .collect();
                out.push(Line::from(spans));
            }
            Err(_) => out.push(Line::from(line.to_string())),
        }
    }
    if matches!(out.last(), Some(l) if l.width() == 0) {
        out.pop();
    }
    out
}

fn lang_syntax<'a>(
    syntaxes: &'a SyntaxSet,
    lang: &str,
) -> Option<&'a syntect::parsing::SyntaxReference> {
    if lang.is_empty() {
        return None;
    }
    if let Some(s) = syntaxes.find_syntax_by_token(lang) {
        return Some(s);
    }
    if let Some(s) = syntaxes.find_syntax_by_extension(lang) {
        return Some(s);
    }
    let ext = lang.rsplit(['/', '.']).next().unwrap_or(lang);
    syntaxes.find_syntax_by_extension(ext)
}

pub fn highlight_line(code: &str, lang: &str) -> Vec<Span<'static>> {
    let code = safe_display_text(code);
    let hl = &*HIGHLIGHTER;
    let theme = &hl.themes.themes["base16-eighties.dark"];
    let syntax = match lang_syntax(&hl.syntaxes, lang) {
        Some(s) => s,
        None => {
            return vec![Span::styled(
                code.to_string(),
                Style::default().fg(super::theme::Palette::CODE_DEFAULT()),
            )]
        }
    };
    let mut h = HighlightLines::new(syntax, theme);
    let with_nl = format!("{}\n", code);
    match h.highlight_line(&with_nl, &hl.syntaxes) {
        Ok(ranges) => ranges
            .iter()
            .map(|(style, text)| {
                let mut s = Style::default().fg(syntect_color(style.foreground));
                if style.font_style.contains(FontStyle::BOLD) {
                    s = s.add_modifier(ratatui::style::Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    s = s.add_modifier(ratatui::style::Modifier::ITALIC);
                }
                Span::styled(text.trim_end_matches('\n').to_string(), s)
            })
            .collect(),
        Err(_) => vec![Span::styled(
            code.to_string(),
            Style::default().fg(super::theme::Palette::CODE_DEFAULT()),
        )],
    }
}

fn plain(code: &str) -> Vec<Line<'static>> {
    code.split('\n')
        .map(|l| {
            Line::from(Span::styled(
                l.to_string(),
                Style::default().fg(super::theme::Palette::CODE_DEFAULT()),
            ))
        })
        .collect()
}

fn syntect_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tokens_extensions_and_paths_share_syntax_and_preserve_source() {
        let code = "let café = \"日本語\";";
        let expected = highlight_line(code, "rust");
        assert_eq!(
            expected
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            code
        );
        for language in ["rs", "src/main.rs", "nested/path/main.rs"] {
            assert_eq!(highlight_line(code, language), expected, "{language}");
        }
        let plain = highlight_line(code, "not-a-real-language");
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0].content, code);
        assert_eq!(
            plain[0].style.fg,
            Some(super::super::theme::Palette::CODE_DEFAULT())
        );
        assert_ne!(
            expected, plain,
            "recognized syntax must actually add highlighting"
        );
    }

    #[test]
    fn highlighting_strips_controls_before_syntax_splitting_and_preserves_styles() {
        let clean = "let café = \"日本語\";";
        let raw = format!("\u{1b}[31m{clean}\u{1b}[0m\u{9d}52;c;payload\u{9c}");
        for lang in ["rust", "sh", "unknown-language"] {
            assert_eq!(highlight_line(&raw, lang), highlight_line(clean, lang));
            assert_eq!(highlight(&raw, lang), highlight(clean, lang));
            assert_eq!(
                highlight(&format!("{raw}\n\t{clean}"), lang),
                highlight(&format!("{clean}\n\t{clean}"), lang)
            );
        }
    }
}
