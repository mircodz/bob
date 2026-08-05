//! Shared TUI building blocks. bob's TUI is immediate-mode over ratatui — every
//! `draw_*` builds `Line`s and blits a `Paragraph`. Several overlays (team drawer,
//! workflow view, agents sidebar, the pickers) re-implemented the same handful of
//! primitives by hand: insetting a rect, a scrolling list with a selection cursor +
//! click hit-testing, a faint vertical divider, and right-aligning a metadata
//! column. Those hand-rolled copies drifted (inconsistent padding, off-by-one
//! scroll/pad math), so they live here once.
//!
//! This is deliberately a small set of HELPERS, not a widget framework — it stays
//! immediate-mode and composes with ratatui rather than wrapping it.

// Some helpers are the shared vocabulary for overlays and aren't all wired into a
// caller yet (they'll be used by the LSP/MCP sidebar sections + a future command
// palette). Keep the module free of per-item dead-code warnings.
#![allow(dead_code)]

use super::theme::Palette;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Shrink a rect by `n` columns on each side and 0 rows (a horizontal inset). The
/// most common breathing-room adjustment before rendering into a pane.
pub fn inset(area: Rect, n: u16) -> Rect {
    inset_xy(area, n, 0)
}

/// Shrink a rect by `x` columns on each side and `y` rows on top+bottom.
pub fn inset_xy(area: Rect, x: u16, y: u16) -> Rect {
    Rect {
        x: area.x + x,
        y: area.y + y,
        width: area.width.saturating_sub(x * 2),
        height: area.height.saturating_sub(y * 2),
    }
}

/// A faint ` │` vertical divider filling `area`'s height. Used between the columns
/// of a split overlay (team drawer, workflow view). Render into a 2-col-wide rect.
pub fn divider_col(area: Rect) -> Vec<Line<'static>> {
    (0..area.height)
        .map(|_| Line::from(Span::styled(" │", Style::default().fg(Palette::FAINT()))))
        .collect()
}

/// Right-align `meta` after `left_spans` within `width` display columns: returns
/// `left_spans` + a padding spacer + `meta`, so the metadata column lands flush
/// right. `left_w` is the display width already consumed by `left_spans` (the
/// caller knows it; spans don't carry a cheap width). A minimum 1-col gap is kept.
pub fn right_align(
    mut left_spans: Vec<Span<'static>>,
    left_w: usize,
    meta: Span<'static>,
    meta_w: usize,
    width: usize,
) -> Line<'static> {
    let pad = width.saturating_sub(left_w + meta_w).max(1);
    left_spans.push(Span::raw(" ".repeat(pad)));
    left_spans.push(meta);
    Line::from(left_spans)
}

/// A scrolling single-selection list. Owns the cursor + scroll offset and the
/// window math so every overlay's roster/tree/picker shares one implementation
/// (previously each hand-rolled ↑↓ clamping, scroll-to-keep-visible, and a
/// screen-row → index hit map, and each got a subtle case wrong).
///
/// The list is content-agnostic: the caller supplies `len` (how many rows) and,
/// at render time, a closure that builds each row's `Line`. `SelectList` returns
/// the visible slice + a row→index hit map for clicks.
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

    /// Move the cursor up one, clamped at 0.
    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the cursor down one, clamped at `len-1`.
    pub fn down(&mut self, len: usize) {
        if self.selected + 1 < len {
            self.selected += 1;
        }
    }

    /// Jump the cursor by `n` rows up.
    pub fn page_up(&mut self, n: usize) {
        self.selected = self.selected.saturating_sub(n);
    }

    /// Jump the cursor by `n` rows down, clamped.
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

    /// Recompute `scroll` so the selected row is visible within `view_h` rows, then
    /// return the range of row indices currently on screen: `scroll..scroll+view_h`
    /// clamped to `len`. Call once per render before building the visible rows.
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

    /// Given the on-screen range (from [`window`]) and the pane's top screen row,
    /// map a clicked screen `row` to the row index it represents, if any.
    pub fn row_at(&self, range: &std::ops::Range<usize>, pane_top: u16, row: u16) -> Option<usize> {
        if row < pane_top {
            return None;
        }
        let idx = self.scroll + (row - pane_top) as usize;
        if range.contains(&idx) {
            Some(idx)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inset_shrinks_symmetrically() {
        let r = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let i = inset(r, 2);
        assert_eq!((i.x, i.width), (2, 16));
        assert_eq!((i.y, i.height), (0, 10));
        let i2 = inset_xy(r, 1, 3);
        assert_eq!((i2.x, i2.width, i2.y, i2.height), (1, 18, 3, 4));
    }

    #[test]
    fn selectlist_nav_clamps() {
        let mut s = SelectList::new();
        s.down(3);
        s.down(3);
        s.down(3);
        assert_eq!(s.selected, 2); // clamped at len-1
        s.up();
        s.up();
        s.up();
        assert_eq!(s.selected, 0); // clamped at 0
    }

    #[test]
    fn window_keeps_selection_visible() {
        let mut s = SelectList::new();
        // 10 items, 3 visible. Select the last → scroll follows.
        s.selected = 9;
        let range = s.window(10, 3);
        assert_eq!(range, 7..10);
        assert_eq!(s.scroll, 7);
        // Select the first → scroll snaps back.
        s.selected = 0;
        let range = s.window(10, 3);
        assert_eq!(range, 0..3);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn row_at_maps_clicks_within_window() {
        let mut s = SelectList::new();
        s.selected = 5;
        let range = s.window(10, 4); // scroll=2, range 2..6
        assert_eq!(s.scroll, 2);
        // Pane top at screen row 20 → row 22 is index 4.
        assert_eq!(s.row_at(&range, 20, 22), Some(4));
        // A row past the window maps to nothing.
        assert_eq!(s.row_at(&range, 20, 40), None);
    }

    #[test]
    fn clamp_after_shrink() {
        let mut s = SelectList::new();
        s.selected = 8;
        s.clamp(3);
        assert_eq!(s.selected, 2);
        s.clamp(0);
        assert_eq!(s.selected, 0);
    }
}
