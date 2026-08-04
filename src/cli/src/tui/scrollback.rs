//! The scrollback viewport: owns the render caches, scroll position, and the
//! click hit-test map for the main transcript. Grouping this state (previously a
//! sprawl of loose `App` fields with a tight mutual invariant) means the caches,
//! the flattened line list, and the click map are always rebuilt together and can
//! never fall out of sync.

use super::render;
use super::theme::{self, Palette};
use super::view::{self, ViewModel};
use super::{shimmer_spans, wrap_line};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::hash::{Hash, Hasher};

/// Horizontal inset applied to the whole scrollback (both sides), so content and
/// the full-width user band get even breathing room.
const SIDE_PAD: u16 = 2;
/// Columns reserved for a non-user cell's hanging indent (left) and the matching
/// right margin, so wrapped body text doesn't hug either edge.
const HANGING_INDENT: usize = 2;
const RIGHT_MARGIN: usize = 2;

#[derive(Default)]
pub struct ScrollbackRenderer {
    /// Per-cell cache: index → (cache key, fully wrapped+inset lines). A hit means
    /// the cell renders identically, so we skip markdown/highlight AND wrapping.
    render_cache: Vec<(u64, Vec<Line<'static>>)>,
    /// Flattened, display-ready lines for the WHOLE transcript. Rebuilt only when
    /// `display_sig` changes, so a plain scroll just re-windows this vec.
    display_cache: Vec<Line<'static>>,
    /// Signature of the inputs that produced `display_cache`.
    display_sig: u64,
    /// Parallel to `display_cache`: source cell index per line (None for the
    /// transient working line). Powers click→cell hit-testing.
    line_owner: Vec<Option<usize>>,
    /// Viewport rect + first visible line index from the last render, so a click
    /// row maps to a `line_owner` entry.
    rect: Option<Rect>,
    start: usize,
    /// `max_scroll` from the previous render, to keep the absolute scroll position
    /// fixed when content grows/shrinks without new bottom output.
    prev_max_scroll: usize,
    /// Distance from the bottom, in display lines. 0 = pinned to the latest output.
    scroll_up: usize,
}

impl ScrollbackRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scroll up by `n` lines (toward older content).
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_up = self.scroll_up.saturating_add(n);
    }

    /// Scroll down by `n` lines (toward the latest output).
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_up = self.scroll_up.saturating_sub(n);
    }

    /// Pin to the bottom (follow new output).
    pub fn stick_to_bottom(&mut self) {
        self.scroll_up = 0;
    }

    /// Whether the view is pinned to the bottom (following new output).
    pub fn at_bottom(&self) -> bool {
        self.scroll_up == 0
    }

    /// Map a click at screen `row` to the transcript cell index under it, if any.
    pub fn hit_test(&self, row: u16) -> Option<usize> {
        let rect = self.rect?;
        if row < rect.y || row >= rect.y + rect.height {
            return None;
        }
        let line_idx = self.start + (row - rect.y) as usize;
        self.line_owner.get(line_idx).copied().flatten()
    }

    /// Draw the transcript into `full`. `working` + `spinner` + `turn_elapsed_secs`
    /// drive the transient "Working" line while a turn runs.
    pub fn render(
        &mut self,
        f: &mut ratatui::Frame,
        full: Rect,
        view: &ViewModel,
        working: bool,
        spinner: usize,
        turn_elapsed_secs: u64,
    ) {
        // Inset horizontally so content + the full-width user band have even side
        // margins (not just a left indent).
        let area = Rect {
            x: full.x + SIDE_PAD,
            y: full.y,
            width: full.width.saturating_sub(SIDE_PAD * 2),
            height: full.height,
        };
        let width_full = area.width as usize;
        let width = area.width.max(1) as usize;
        let theme_gen = theme::generation();
        let cells = &view.cells;
        if self.render_cache.len() != cells.len() {
            self.render_cache.resize(cells.len(), (0, Vec::new()));
        }

        // Cheap rebuild trigger: the view's revision (bumped only on real cell
        // changes) + width/theme + the working-line tick. A pure scroll leaves all
        // of these unchanged, so we skip the rebuild — O(1) per scroll frame.
        let mut sig_hasher = std::collections::hash_map::DefaultHasher::new();
        view.revision.hash(&mut sig_hasher);
        width_full.hash(&mut sig_hasher);
        theme_gen.hash(&mut sig_hasher);
        working.hash(&mut sig_hasher);
        if working {
            spinner.hash(&mut sig_hasher);
            turn_elapsed_secs.hash(&mut sig_hasher);
        }
        let sig = sig_hasher.finish();

        let stale = self.display_cache.is_empty() && !cells.is_empty();
        if sig != self.display_sig || stale {
            self.rebuild(
                cells,
                width_full,
                width,
                theme_gen,
                working,
                spinner,
                turn_elapsed_secs,
            );
            self.display_sig = sig;
        }

        let viewport = area.height as usize;
        let total = self.display_cache.len();
        let max_scroll = total.saturating_sub(viewport);
        // Keep the ABSOLUTE position fixed across content changes that aren't new
        // bottom output (e.g. expanding a tool cell): shift scroll_up by the same
        // delta so `start` is unchanged. At the bottom (scroll_up == 0) we follow.
        if self.scroll_up > 0 {
            let delta = max_scroll as i64 - self.prev_max_scroll as i64;
            self.scroll_up = (self.scroll_up as i64 + delta).clamp(0, max_scroll as i64) as usize;
        }
        self.prev_max_scroll = max_scroll;
        self.scroll_up = self.scroll_up.min(max_scroll);
        let start = max_scroll.saturating_sub(self.scroll_up);
        self.rect = Some(area);
        self.start = start;

        let window: Vec<Line> = self
            .display_cache
            .iter()
            .skip(start)
            .take(viewport)
            .cloned()
            .collect();
        f.render_widget(Paragraph::new(window), area);

        // Scroll hint when not at the bottom.
        if self.scroll_up > 0 {
            let hint = Rect {
                x: area.x + area.width.saturating_sub(10),
                y: area.y,
                width: 10,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    " ↑ scrolled ",
                    Style::default().fg(Palette::WARN()).bg(Palette::POPUP_BG()),
                )),
                hint,
            );
        }
    }

    /// Rebuild `display_cache` + `line_owner` from the cells, reusing per-cell
    /// cached lines where the fingerprint is unchanged.
    #[allow(clippy::too_many_arguments)]
    fn rebuild(
        &mut self,
        cells: &[view::Cell],
        width_full: usize,
        width: usize,
        theme_gen: u64,
        working: bool,
        spinner: usize,
        turn_elapsed_secs: u64,
    ) {
        let mut flat: Vec<Line> = Vec::new();
        let mut owners: Vec<Option<usize>> = Vec::new();
        // A blank pad row at the very top so the first message has breathing room
        // against the top edge (visible when the transcript is short).
        flat.push(Line::from(""));
        owners.push(None);
        for (i, cell) in cells.iter().enumerate() {
            let is_user = matches!(cell, view::Cell::User(_));
            let key = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                cell.fingerprint().hash(&mut h);
                width_full.hash(&mut h);
                theme_gen.hash(&mut h);
                h.finish()
            };
            let slot = &mut self.render_cache[i];
            if slot.0 != key {
                let mut rendered = Vec::new();
                render::render_cell(cell, width_full, &mut rendered);
                let wrap_width = if is_user {
                    width
                } else {
                    width.saturating_sub(HANGING_INDENT + RIGHT_MARGIN).max(1)
                };
                let mut display = Vec::new();
                for l in rendered {
                    for mut wl in wrap_line(l, wrap_width) {
                        if !is_user {
                            wl.spans.insert(0, Span::raw(" ".repeat(HANGING_INDENT)));
                        }
                        display.push(wl);
                    }
                }
                *slot = (key, display);
            }
            for line in &slot.1 {
                flat.push(line.clone());
                owners.push(Some(i));
            }
        }

        // The transient "Working" line: a shimmering label + elapsed seconds.
        if working {
            let mut spans: Vec<Span> = vec![Span::styled(
                "  • ",
                Style::default().fg(Palette::RUNNING()),
            )];
            spans.extend(shimmer_spans("Working", spinner));
            spans.push(Span::styled(
                format!(" ({}s · esc to interrupt)", turn_elapsed_secs),
                Style::default().fg(Palette::FAINT()),
            ));
            for wl in wrap_line(Line::from(spans), width) {
                flat.push(wl);
                owners.push(None);
            }
        }

        // Always end with a blank pad row so the last line (a message tail, or the
        // "Working" indicator) never sits flush against the input box below.
        flat.push(Line::from(""));
        owners.push(None);

        self.display_cache = flat;
        self.line_owner = owners;
    }
}
