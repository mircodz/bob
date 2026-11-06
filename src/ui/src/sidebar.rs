use crate::theme::Palette;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub fn row(
    label: &str,
    depth: usize,
    color: Color,
    finished: bool,
    selected: bool,
    width: usize,
) -> Line<'static> {
    let bg = Style::default().bg(Palette::INPUT_BG());
    let depth = depth.min(width.saturating_sub(8) / 2);
    let lead = format!("  {}• ", "  ".repeat(depth));
    let budget = width.saturating_sub(lead.width());
    let clean = label.replace(['\n', '\r', '\t'], " ");
    let label = if clean.width() <= budget {
        clean
    } else {
        let mut short = String::new();
        let mut used = 0;
        for ch in clean.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > budget.saturating_sub(1) {
                break;
            }
            short.push(ch);
            used += w;
        }
        if budget > 0 {
            short.push('…');
        }
        short
    };
    let mut name = if finished {
        Style::default()
            .fg(Palette::DIM())
            .add_modifier(Modifier::CROSSED_OUT)
    } else {
        Style::default().fg(Palette::TEXT())
    };
    if selected {
        name = name.add_modifier(Modifier::BOLD);
    }
    let pad = width.saturating_sub(lead.width() + label.width());
    Line::from(vec![
        Span::styled(lead, bg.fg(color)),
        Span::styled(label, name.bg(Palette::INPUT_BG())),
        Span::styled(" ".repeat(pad), bg),
    ])
}
