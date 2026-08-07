//! Shared TUI building blocks. bob's TUI is immediate-mode over ratatui — every
//! `draw_*` builds `Line`s and blits a `Paragraph`. Overlays (team drawer, workflow
//! view, agents sidebar, pickers) once hand-rolled the same primitives — insetting
//! a rect, a scrolling selection list, a faint vertical divider — and the copies
//! drifted. They live here once now.
//!
//! Deliberately a small set of HELPERS, not a widget framework: it stays
//! immediate-mode and composes with ratatui rather than wrapping it.

// `row_at` is the click-hit companion to `window` — kept as part of the SelectList
// API for symmetry (overlays currently hand-roll the screen-row→index map against
// their own recorded rects). Suppress the lone dead-code warning rather than split
// the type's vocabulary.
#![allow(dead_code)]

use super::theme::Palette;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// The shared horizontal layout budget, in columns. The whole scrollback is inset
/// by [`SIDE_PAD`] on each side; a floating "band" (a user message or the input
/// box) sits inside that with an extra [`BAND_MARGIN`], so its colored edge lands
/// at [`BAND_INSET`] (`SIDE_PAD + BAND_MARGIN`) columns from the screen edge. Every
/// panel stacked above the input aligns its left edge to `BAND_INSET` so the whole
/// column reads as one conversation. These live here (not per-file) so the input,
/// the transcript, and the overlays can't drift apart.
pub const SIDE_PAD: u16 = 2;
pub const BAND_MARGIN: u16 = 2;
pub const BAND_INSET: u16 = SIDE_PAD + BAND_MARGIN;

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

/// A scrolling single-selection list. Owns the cursor + scroll offset and the
/// window math so every overlay's roster/tree/picker shares one implementation.
///
/// Content-agnostic: the caller supplies `len` and builds each row's `Line` at
/// render time. `window()` returns the visible index range; `row_at()` maps a
/// click back to an index.
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

    #[test]
    fn clamp_leaves_in_range_selection() {
        let mut s = SelectList::new();
        s.selected = 3;
        s.clamp(10); // already in range → unchanged
        assert_eq!(s.selected, 3);
    }

    #[test]
    fn page_up_down_jump_and_clamp() {
        let mut s = SelectList::new();
        s.page_down(10, 100);
        assert_eq!(s.selected, 10);
        s.page_down(10, 100);
        assert_eq!(s.selected, 20);
        s.page_up(5);
        assert_eq!(s.selected, 15);
        // Clamp at the ends.
        s.page_down(1000, 100);
        assert_eq!(s.selected, 99);
        s.page_up(1000);
        assert_eq!(s.selected, 0);
        // Empty list → stays at 0, no panic.
        let mut e = SelectList::new();
        e.page_down(5, 0);
        assert_eq!(e.selected, 0);
    }

    #[test]
    fn window_edge_cases() {
        let mut s = SelectList::new();
        // Zero view height or empty list → empty range, scroll reset.
        s.scroll = 4;
        assert_eq!(s.window(10, 0), 0..0);
        assert_eq!(s.scroll, 0);
        s.scroll = 4;
        assert_eq!(s.window(0, 5), 0..0);
        assert_eq!(s.scroll, 0);
        // Fewer items than the viewport → no scroll, full range.
        s.selected = 2;
        assert_eq!(s.window(3, 10), 0..3);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn row_at_ignores_clicks_above_pane() {
        let mut s = SelectList::new();
        s.selected = 0;
        let range = s.window(10, 4); // scroll 0, range 0..4
                                     // A row above the pane top maps to nothing.
        assert_eq!(s.row_at(&range, 20, 19), None);
        // The pane's top row is the first index.
        assert_eq!(s.row_at(&range, 20, 20), Some(0));
    }

    #[test]
    fn divider_col_matches_height() {
        assert_eq!(
            divider_col(Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 3,
            })
            .len(),
            3
        );
        assert_eq!(
            divider_col(Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 0,
            })
            .len(),
            0
        );
    }
}
