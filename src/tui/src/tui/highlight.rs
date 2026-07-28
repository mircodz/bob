//! Syntax highlighting for fenced code blocks via syntect. Loads the default
//! syntax + theme sets once (lazily) and maps syntect styles to ratatui spans.

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

/// Highlight `code` (already newline-split by the caller is fine too) in the
/// given language, returning one ratatui Line per source line. Falls back to
/// plain, uncolored lines when the language isn't recognized.
pub fn highlight(code: &str, lang: &str) -> Vec<Line<'static>> {
    let hl = &*HIGHLIGHTER;
    let theme = &hl.themes.themes["base16-eighties.dark"];

    let syntax = lang_syntax(&hl.syntaxes, lang);
    let syntax = match syntax {
        Some(s) => s,
        None => return plain(code),
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
    // Drop a trailing empty line produced by a final newline.
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
    // `lang` may be a bare token ("rust"), an extension ("rs"), or a full path
    // ("src/main.rs") — try token first, then the trailing extension.
    if let Some(s) = syntaxes.find_syntax_by_token(lang) {
        return Some(s);
    }
    if let Some(s) = syntaxes.find_syntax_by_extension(lang) {
        return Some(s);
    }
    let ext = lang.rsplit(['/', '.']).next().unwrap_or(lang);
    syntaxes.find_syntax_by_extension(ext)
}

/// Highlight a single line of `code` in `lang`, returning styled spans (no
/// trailing newline). Used by the diff renderer, which owns its own gutters.
pub fn highlight_line(code: &str, lang: &str) -> Vec<Span<'static>> {
    let hl = &*HIGHLIGHTER;
    let theme = &hl.themes.themes["base16-eighties.dark"];
    let syntax = match lang_syntax(&hl.syntaxes, lang) {
        Some(s) => s,
        None => {
            return vec![Span::styled(
                code.to_string(),
                Style::default().fg(super::theme::Palette::CODE_DEFAULT),
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
            Style::default().fg(super::theme::Palette::CODE_DEFAULT),
        )],
    }
}

fn plain(code: &str) -> Vec<Line<'static>> {
    code.split('\n')
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(super::theme::Palette::CODE_DEFAULT))))
        .collect()
}

fn syntect_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}
