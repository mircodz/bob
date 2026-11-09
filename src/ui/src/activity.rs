use crate::theme::Palette;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

pub fn shimmer_spans(text: &str, tick: usize) -> Vec<Span<'static>> {
    const BASE: (f32, f32, f32) = (0x66 as f32, 0x66 as f32, 0x66 as f32);
    const PEAK: (f32, f32, f32) = (0xf0 as f32, 0xf0 as f32, 0xf0 as f32);
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len().max(1) as f32;
    // Travel beyond the text for a pause between sweeps.
    let span = n + 6.0;
    let pos = (tick as f32 * 0.6) % span;
    let width = 3.0_f32;
    chars
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            let d = (i as f32 - pos).abs();
            let t = if d >= width { 0.0 } else { 1.0 - d / width };
            let t = t * t * (3.0 - 2.0 * t); // smoothstep
            let lerp = |a: f32, b: f32| (a + (b - a) * t) as u8;
            let color = Color::Rgb(
                lerp(BASE.0, PEAK.0),
                lerp(BASE.1, PEAK.1),
                lerp(BASE.2, PEAK.2),
            );
            Span::styled(c.to_string(), Style::default().fg(color))
        })
        .collect()
}

pub fn working_line(elapsed_secs: u64, tick: usize, hint: &str) -> Line<'static> {
    labeled_working_line("Working", elapsed_secs, tick, hint)
}

pub fn labeled_working_line(
    label: &str,
    elapsed_secs: u64,
    tick: usize,
    hint: &str,
) -> Line<'static> {
    let mut spans = vec![Span::styled("• ", Style::default().fg(Palette::RUNNING()))];
    spans.extend(shimmer_spans(label, tick));
    spans.push(Span::styled(
        format!(" ({elapsed_secs}s · {hint})"),
        Style::default().fg(Palette::FAINT()),
    ));
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_line_preserves_production_text_and_styles() {
        let line = working_line(125, 0, "Esc to interrupt");
        assert_eq!(line.to_string(), "• Working (125s · Esc to interrupt)");
        assert_eq!(line.spans.len(), 9);
        assert_eq!(
            line.spans[0],
            Span::styled("• ", Style::default().fg(Palette::RUNNING()))
        );
        assert_eq!(
            line.spans[8],
            Span::styled(
                " (125s · Esc to interrupt)",
                Style::default().fg(Palette::FAINT())
            )
        );
        for (span, value) in line.spans[1..8]
            .iter()
            .zip([240, 204, 137, 102, 102, 102, 102])
        {
            assert_eq!(
                span.style,
                Style::default().fg(Color::Rgb(value, value, value))
            );
        }
    }

    #[test]
    fn shimmer_sweeps_smoothly_then_pauses_and_repeats() {
        let colors = |tick| {
            shimmer_spans("Working", tick)
                .into_iter()
                .map(|span| span.style.fg.unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            colors(5),
            [102, 137, 204, 240, 204, 137, 102].map(|value| Color::Rgb(value, value, value))
        );
        assert_eq!(colors(15), vec![Color::Rgb(102, 102, 102); 7]);
        assert_eq!(colors(65), colors(0));
        assert_ne!(colors(1), colors(0));
        assert!(shimmer_spans("", 1).is_empty());
        assert_eq!(shimmer_spans("é中", 0).len(), 2);
    }

    #[test]
    fn working_line_keeps_full_hint_and_static_bullet() {
        let hint = "Ctrl+T agents · Esc to interrupt".repeat(10);
        let first = working_line(0, 0, &hint);
        let next = working_line(0, 5, &hint);
        assert_eq!(first.to_string(), format!("• Working (0s · {hint})"));
        assert_eq!(first.spans[0], next.spans[0]);
        assert_eq!(first.spans[8], next.spans[8]);
        assert_ne!(first.spans[1..8], next.spans[1..8]);
    }
}
