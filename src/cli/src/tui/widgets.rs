use ratatui::layout::Rect;
use ratatui::text::Line;

/// Shared insets in display columns; panels and input align at `BAND_INSET`.
pub const SIDE_PAD: u16 = 2;
pub const BAND_MARGIN: u16 = 2;
pub const BAND_INSET: u16 = SIDE_PAD + BAND_MARGIN;

pub fn inset(area: Rect, n: u16) -> Rect {
    inset_xy(area, n, 0)
}

pub fn inset_xy(area: Rect, x: u16, y: u16) -> Rect {
    Rect {
        x: area.x + x,
        y: area.y + y,
        width: area.width.saturating_sub(x * 2),
        height: area.height.saturating_sub(y * 2),
    }
}

#[derive(Default)]
pub struct SelectList {
    /// Selected row index (clamped to `0..len`).
    pub selected: usize,
    /// First visible row (scroll offset), kept so the selection stays on-screen.
    pub scroll: usize,
}

impl SelectList {
    pub fn new() -> Self {
        SelectList::default()
    }

    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self, len: usize) {
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    pub fn page_up(&mut self, n: usize) {
        self.selected = self.selected.saturating_sub(n);
    }

    pub fn page_down(&mut self, n: usize, len: usize) {
        self.selected = (self.selected + n).min(len.saturating_sub(1));
    }

    /// Clamp the cursor into `0..len` (call when the backing list may have shrunk).
    pub fn clamp(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    /// Keep the selection visible; call before building the visible rows.
    pub fn window(&mut self, len: usize, view_h: usize) -> std::ops::Range<usize> {
        if view_h == 0 || len == 0 {
            self.scroll = 0;
            return 0..0;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + view_h {
            self.scroll = self.selected + 1 - view_h;
        }
        let max_scroll = len.saturating_sub(view_h);
        self.scroll = self.scroll.min(max_scroll);
        let end = (self.scroll + view_h).min(len);
        self.scroll..end
    }
}

pub fn keep_visible(scroll: usize, selected: usize, height: usize, len: usize) -> usize {
    if height == 0 {
        return selected;
    }
    let scroll = scroll.min(selected);
    let scroll = if selected >= scroll.saturating_add(height) {
        selected + 1 - height
    } else {
        scroll
    };
    scroll.min(len.saturating_sub(height))
}

pub fn fit_text(text: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if text.width() <= width {
        return text.to_string();
    }
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let columns = ch.width().unwrap_or(0);
        if used + columns > width.saturating_sub(1) {
            break;
        }
        result.push(ch);
        used += columns;
    }
    if width > 0 {
        result.push('…');
    }
    result
}

pub fn prompt_window(lines: Vec<Line<'static>>, height: usize) -> Vec<Line<'static>> {
    if lines.len() <= height {
        return lines;
    }
    let selected = lines
        .iter()
        .position(|line| {
            line.spans
                .first()
                .is_some_and(|span| span.content.contains('❯') || span.content == "› ")
        })
        .unwrap_or(0);
    if height < 3 {
        return lines.into_iter().skip(selected).take(height).collect();
    }
    let start = selected.saturating_sub(height - 3).max(1);
    let mut visible = vec![lines[0].clone()];
    visible.extend(lines.iter().skip(start).take(height - 2).cloned());
    visible.push(lines.last().unwrap().clone());
    visible
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_text_keeps_exact_fits_and_budgets_ellipsis_in_display_columns() {
        for (text, width, expected) in [
            ("hello", 0, ""),
            ("hello", 1, "…"),
            ("hello", 5, "hello"),
            ("a界z", 3, "a…"),
            ("界界z", 4, "界…"),
            ("e\u{301}xy", 2, "e\u{301}…"),
        ] {
            assert_eq!(fit_text(text, width), expected, "{text:?}, width={width}");
        }
    }

    #[test]
    fn prompt_window_preserves_title_selection_and_footer_in_order() {
        let lines: Vec<Line> = ["title", "one", "two", " ❯ three", "four", "hint"]
            .into_iter()
            .map(Line::from)
            .collect();
        for (height, expected) in [
            (0, vec![]),
            (1, vec![" ❯ three"]),
            (2, vec![" ❯ three", "four"]),
            (3, vec!["title", " ❯ three", "hint"]),
            (4, vec!["title", "two", " ❯ three", "hint"]),
            (6, vec!["title", "one", "two", " ❯ three", "four", "hint"]),
        ] {
            assert_eq!(
                prompt_window(lines.clone(), height)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                expected
            );
        }
        let free_text = vec![Line::from("title"), Line::from("› "), Line::from("hint")];
        assert_eq!(prompt_window(free_text, 1)[0].to_string(), "› ");
    }

    #[test]
    fn selection_navigation_and_shrink_clamp_at_both_ends() {
        let mut list = SelectList::new();
        for (label, down, expected) in [
            ("first down", true, 1),
            ("last row", true, 2),
            ("down clamps", true, 2),
            ("up one", false, 1),
            ("first row", false, 0),
            ("up clamps", false, 0),
        ] {
            if down {
                list.down(3);
            } else {
                list.up();
            }
            assert_eq!(list.selected, expected, "{label}");
        }
        for (label, down, step, len, expected) in [
            ("page down", true, 10, 100, 10),
            ("second page", true, 10, 100, 20),
            ("page up", false, 5, 100, 15),
            ("page past end", true, 1000, 100, 99),
            ("page past start", false, 1000, 100, 0),
            ("empty list", true, 5, 0, 0),
        ] {
            if down {
                list.page_down(step, len);
            } else {
                list.page_up(step);
            }
            assert_eq!(list.selected, expected, "{label}");
        }
        for (label, selected, len, expected) in [
            ("shrunken list", 8, 3, 2),
            ("emptied list", 2, 0, 0),
            ("in range", 3, 10, 3),
        ] {
            list.selected = selected;
            list.clamp(len);
            assert_eq!(list.selected, expected, "{label}");
        }
    }

    #[test]
    fn selection_window_tracks_edges_and_resets_empty_viewports() {
        for (label, selected, scroll, len, height, expected) in [
            ("follow last row", 9, 0, 10, 3, 7..10),
            ("return to first row", 0, 7, 10, 3, 0..3),
            ("zero viewport", 0, 4, 10, 0, 0..0),
            ("empty list", 0, 4, 0, 5, 0..0),
            ("short list", 2, 4, 3, 10, 0..3),
        ] {
            let mut list = SelectList { selected, scroll };
            assert_eq!(list.window(len, height), expected, "{label}: range");
            assert_eq!(list.scroll, expected.start, "{label}: scroll");
        }
    }
}
