//! Viewport-driven conversation rendering. Expensive cell content is prepared
//! once; only visited cells are laid out, with the two most recent widths kept.

use super::render::PreparedCell;
use super::theme::{self, Palette};
use super::view::Cell;
use super::widgets::SIDE_PAD;
use super::{shimmer_spans, wrap_line};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

const HANGING_INDENT: usize = 2;
const RIGHT_MARGIN: usize = 2;
const MAX_CACHED_CELLS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anchor {
    Start,
    Cell { index: usize, row: usize },
    Footer { row: usize },
}

#[derive(Clone)]
struct CellLayout {
    width: usize,
    lines: Arc<Vec<Line<'static>>>,
    source_rows: Arc<Vec<usize>>,
}

struct CachedCell {
    fingerprint: u64,
    validated_revision: u64,
    prepared: PreparedCell,
    layouts: VecDeque<CellLayout>,
    last_used: u64,
}

struct VisibleRow {
    anchor: Anchor,
    line: Line<'static>,
    owner: Option<(usize, usize)>,
}

struct RenderContext<'a> {
    cells: &'a [Cell],
    revision: u64,
    width: usize,
    footer: Vec<Line<'static>>,
}

/// `None` anchors to the bottom; a scrolled viewport anchors to an actual cell,
/// so appending output or toggling the sidebar does not jump to unrelated history.
#[derive(Default)]
pub struct ScrollbackRenderer {
    cache: HashMap<usize, CachedCell>,
    cache_limit: usize,
    generation: Option<u64>,
    clock: u64,
    previous_len: usize,
    anchor: Option<Anchor>,
    last_top: Option<Anchor>,
    pending_scroll: i64,
    rect: Option<Rect>,
    line_owner: Vec<Option<(usize, usize)>>,
    #[cfg(test)]
    fingerprints: usize,
    #[cfg(test)]
    preparations: usize,
    #[cfg(test)]
    layouts: usize,
    #[cfg(test)]
    peak_cached: usize,
}

impl ScrollbackRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn scroll_up(&mut self, n: usize) {
        self.pending_scroll = self
            .pending_scroll
            .saturating_sub(i64::try_from(n).unwrap_or(i64::MAX));
    }

    pub fn scroll_down(&mut self, n: usize) {
        self.pending_scroll = self
            .pending_scroll
            .saturating_add(i64::try_from(n).unwrap_or(i64::MAX));
        if self.anchor.is_none() {
            self.pending_scroll = self.pending_scroll.min(0);
        }
    }

    pub fn stick_to_bottom(&mut self) {
        self.anchor = None;
        self.pending_scroll = 0;
    }

    pub fn at_bottom(&self) -> bool {
        self.anchor.is_none() && self.pending_scroll >= 0
    }

    pub fn hit_test_offset(&self, row: u16) -> Option<(usize, usize)> {
        let rect = self.rect?;
        if row < rect.y || row >= rect.bottom() {
            return None;
        }
        self.line_owner
            .get((row - rect.y) as usize)
            .copied()
            .flatten()
    }

    fn layout(&mut self, ctx: &RenderContext<'_>, index: usize) -> CellLayout {
        self.clock = self.clock.wrapping_add(1);
        if self
            .cache
            .get(&index)
            .is_none_or(|entry| entry.validated_revision != ctx.revision)
        {
            let fingerprint = ctx.cells[index].fingerprint();
            #[cfg(test)]
            {
                self.fingerprints += 1;
            }
            if self
                .cache
                .get(&index)
                .is_none_or(|entry| entry.fingerprint != fingerprint)
            {
                self.cache.insert(
                    index,
                    CachedCell {
                        fingerprint,
                        validated_revision: ctx.revision,
                        prepared: PreparedCell::new(&ctx.cells[index]),
                        layouts: VecDeque::new(),
                        last_used: self.clock,
                    },
                );
                #[cfg(test)]
                {
                    self.preparations += 1;
                }
            }
        }
        {
            let entry = self.cache.get_mut(&index).unwrap();
            entry.validated_revision = ctx.revision;
            entry.last_used = self.clock;
        }
        #[cfg(test)]
        {
            self.peak_cached = self.peak_cached.max(self.cache.len());
        }
        self.trim_cache();
        let entry = self.cache.get_mut(&index).unwrap();
        if let Some(pos) = entry
            .layouts
            .iter()
            .position(|layout| layout.width == ctx.width)
        {
            if pos != 0 {
                let layout = entry.layouts.remove(pos).unwrap();
                entry.layouts.push_front(layout);
            }
            return entry.layouts[0].clone();
        }
        let is_user = matches!(&ctx.cells[index], Cell::User(_));
        let wrap_width = if is_user {
            ctx.width
        } else {
            ctx.width
                .saturating_sub(HANGING_INDENT + RIGHT_MARGIN)
                .max(1)
        };
        let mut rendered = Vec::new();
        entry
            .prepared
            .render(&ctx.cells[index], ctx.width, &mut rendered);
        let mut lines = Vec::new();
        let mut source_rows = Vec::new();
        for (source_row, line) in rendered.into_iter().enumerate() {
            for mut row in wrap_line(line, wrap_width) {
                if !is_user {
                    row.spans.insert(0, Span::raw(" ".repeat(HANGING_INDENT)));
                }
                lines.push(row);
                source_rows.push(source_row);
            }
        }
        let layout = CellLayout {
            width: ctx.width,
            lines: Arc::new(lines),
            source_rows: Arc::new(source_rows),
        };
        entry.layouts.push_front(layout.clone());
        entry.layouts.truncate(2);
        #[cfg(test)]
        {
            self.layouts += 1;
        }
        layout
    }

    fn first_from(&mut self, ctx: &RenderContext<'_>, start: usize) -> Anchor {
        for index in start..ctx.cells.len() {
            if ctx.cells[index].is_visible() && !self.layout(ctx, index).lines.is_empty() {
                return Anchor::Cell { index, row: 0 };
            }
        }
        Anchor::Footer { row: 0 }
    }

    fn last_before(&mut self, ctx: &RenderContext<'_>, end: usize) -> Anchor {
        for index in (0..end).rev() {
            if ctx.cells[index].is_visible() {
                let height = self.layout(ctx, index).lines.len();
                if height > 0 {
                    return Anchor::Cell {
                        index,
                        row: height - 1,
                    };
                }
            }
        }
        Anchor::Start
    }

    fn normalize(&mut self, ctx: &RenderContext<'_>, anchor: Anchor) -> Option<Anchor> {
        match anchor {
            Anchor::Cell { index, row } => {
                if index >= ctx.cells.len() {
                    return None;
                }
                if ctx.cells[index].is_visible() {
                    let height = self.layout(ctx, index).lines.len();
                    if height > 0 {
                        return Some(Anchor::Cell {
                            index,
                            row: row.min(height - 1),
                        });
                    }
                }
                Some(self.first_from(ctx, index + 1))
            }
            Anchor::Footer { row } => Some(Anchor::Footer {
                row: row.min(ctx.footer.len() - 1),
            }),
            Anchor::Start => Some(Anchor::Start),
        }
    }

    fn previous(&mut self, ctx: &RenderContext<'_>, anchor: Anchor) -> Option<Anchor> {
        match anchor {
            Anchor::Start => None,
            Anchor::Cell { index, row: 0 } => Some(self.last_before(ctx, index)),
            Anchor::Cell { index, row } => Some(Anchor::Cell {
                index,
                row: row - 1,
            }),
            Anchor::Footer { row: 0 } => Some(self.last_before(ctx, ctx.cells.len())),
            Anchor::Footer { row } => Some(Anchor::Footer { row: row - 1 }),
        }
    }

    fn next(&mut self, ctx: &RenderContext<'_>, anchor: Anchor) -> Option<Anchor> {
        match anchor {
            Anchor::Start => Some(self.first_from(ctx, 0)),
            Anchor::Cell { index, row } => {
                if row + 1 < self.layout(ctx, index).lines.len() {
                    Some(Anchor::Cell {
                        index,
                        row: row + 1,
                    })
                } else {
                    Some(self.first_from(ctx, index + 1))
                }
            }
            Anchor::Footer { row } => {
                (row + 1 < ctx.footer.len()).then_some(Anchor::Footer { row: row + 1 })
            }
        }
    }

    fn row(&mut self, ctx: &RenderContext<'_>, anchor: Anchor) -> VisibleRow {
        let (line, owner) = match anchor {
            Anchor::Start => (Line::from(""), None),
            Anchor::Cell { index, row } => {
                let layout = self.layout(ctx, index);
                (
                    layout.lines[row].clone(),
                    Some((index, layout.source_rows[row])),
                )
            }
            Anchor::Footer { row } => (ctx.footer[row].clone(), None),
        };
        VisibleRow {
            anchor,
            line,
            owner,
        }
    }

    fn bottom_window(&mut self, ctx: &RenderContext<'_>, height: usize) -> Vec<VisibleRow> {
        let mut rows = Vec::with_capacity(height);
        let mut anchor = Anchor::Footer {
            row: ctx.footer.len() - 1,
        };
        loop {
            rows.push(self.row(ctx, anchor));
            if rows.len() == height {
                break;
            }
            match self.previous(ctx, anchor) {
                Some(previous) => anchor = previous,
                None => break,
            }
        }
        rows.reverse();
        rows
    }

    fn forward_window(
        &mut self,
        ctx: &RenderContext<'_>,
        mut anchor: Anchor,
        height: usize,
    ) -> Vec<VisibleRow> {
        let mut rows = Vec::with_capacity(height);
        loop {
            rows.push(self.row(ctx, anchor));
            if rows.len() == height {
                break;
            }
            match self.next(ctx, anchor) {
                Some(next) => anchor = next,
                None => break,
            }
        }
        rows
    }

    fn trim_cache(&mut self) {
        while self.cache.len() > self.cache_limit {
            let oldest = self
                .cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| *index)
                .unwrap();
            self.cache.remove(&oldest);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        f: &mut ratatui::Frame,
        full: Rect,
        cells: &[Cell],
        revision: u64,
        working: bool,
        spinner: usize,
        turn_elapsed_secs: u64,
    ) {
        let area = Rect {
            x: full.x + SIDE_PAD,
            y: full.y,
            width: full.width.saturating_sub(SIDE_PAD * 2),
            height: full.height,
        };
        self.rect = Some(area);
        self.line_owner.clear();
        if area.width == 0 || area.height == 0 {
            return;
        }
        let generation = theme::generation();
        if self.generation != Some(generation) {
            self.cache.clear();
            self.generation = Some(generation);
        }
        if cells.len() < self.previous_len {
            self.cache.retain(|index, _| *index < cells.len());
        }
        self.previous_len = cells.len();
        let width = area.width as usize;
        let height = area.height as usize;
        self.cache_limit = MAX_CACHED_CELLS.max(height.saturating_mul(2));
        let mut footer = Vec::new();
        if working {
            let mut spans = vec![Span::styled(
                "  • ",
                Style::default().fg(Palette::RUNNING()),
            )];
            spans.extend(shimmer_spans("Working", spinner));
            spans.push(Span::styled(
                format!(" ({}s · esc to interrupt)", turn_elapsed_secs),
                Style::default().fg(Palette::FAINT()),
            ));
            footer.extend(wrap_line(Line::from(spans), width));
        }
        footer.push(Line::from(""));
        let ctx = RenderContext {
            cells,
            revision,
            width,
            footer,
        };
        let end = Anchor::Footer {
            row: ctx.footer.len() - 1,
        };
        let pending = std::mem::take(&mut self.pending_scroll);
        let mut anchor = self.anchor.and_then(|anchor| self.normalize(&ctx, anchor));
        if pending != 0 {
            if anchor.is_none() {
                anchor = self
                    .last_top
                    .and_then(|anchor| self.normalize(&ctx, anchor));
            }
            if anchor.is_none() {
                anchor = self
                    .bottom_window(&ctx, height)
                    .first()
                    .map(|row| row.anchor);
            }
            if let Some(mut position) = anchor {
                for _ in 0..pending.unsigned_abs() {
                    let next = if pending < 0 {
                        self.previous(&ctx, position)
                    } else {
                        self.next(&ctx, position)
                    };
                    match next {
                        Some(next) => position = next,
                        None => break,
                    }
                }
                anchor = Some(position);
            }
        }
        let mut rows = match anchor {
            Some(anchor) => self.forward_window(&ctx, anchor, height),
            None => self.bottom_window(&ctx, height),
        };
        let at_bottom = rows.last().is_some_and(|row| row.anchor == end);
        if at_bottom && rows.len() < height && anchor.is_some() {
            rows = self.bottom_window(&ctx, height);
        }
        self.last_top = rows.first().map(|row| row.anchor);
        self.anchor = if at_bottom { None } else { self.last_top };
        let mut lines = Vec::with_capacity(rows.len());
        for row in rows {
            self.line_owner.push(row.owner);
            lines.push(row.line);
        }
        f.render_widget(Paragraph::new(lines), area);
        if !self.at_bottom() {
            let label = " ↑ scrolled ";
            let hint_width = Span::raw(label).width().min(width) as u16;
            let hint = Rect {
                x: area.x + (area.width - hint_width) / 2,
                y: area.y,
                width: hint_width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default().fg(Palette::WARN()).bg(Palette::POPUP_BG()),
                )),
                hint,
            );
        }
        self.trim_cache();
    }
}

#[cfg(test)]
mod tests {
    use super::super::render;
    use super::super::view as view_types;
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use view_types::ToolStatus;

    #[allow(clippy::too_many_arguments)]
    fn render(
        renderer: &mut ScrollbackRenderer,
        terminal: &mut Terminal<TestBackend>,
        cells: &[Cell],
        revision: u64,
        width: u16,
        height: u16,
        working: bool,
        spinner: usize,
    ) {
        terminal
            .draw(|f| {
                renderer.render(
                    f,
                    Rect::new(0, 0, width, height),
                    cells,
                    revision,
                    working,
                    spinner,
                    1,
                )
            })
            .unwrap();
    }

    fn reference(cells: &[Cell], width: usize) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from("")];
        for cell in cells {
            let mut raw = Vec::new();
            render::render_cell(cell, width, &mut raw);
            let is_user = matches!(cell, Cell::User(_));
            let wrap_width = if is_user {
                width
            } else {
                width.saturating_sub(HANGING_INDENT + RIGHT_MARGIN).max(1)
            };
            for line in raw {
                for mut row in wrap_line(line, wrap_width) {
                    if !is_user {
                        row.spans.insert(0, Span::raw(" ".repeat(HANGING_INDENT)));
                    }
                    lines.push(row);
                }
            }
        }
        lines.push(Line::from(""));
        lines
    }

    fn history(count: usize) -> Vec<Cell> {
        (0..count).map(|i| if i % 20 == 0 {
            Cell::Assistant { text: format!("Message {i}\n\n```rust\nfn example_{i}() {{ println!(\"example\"); }}\n```"), open: false }
        } else {
            Cell::Notice(format!("history row {i}"))
        }).collect()
    }

    #[test]
    fn cold_open_and_toggles_only_prepare_visible_history() {
        let cells = history(5000);
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        render(&mut renderer, &mut terminal, &cells, 1, 120, 24, false, 0);
        assert!(renderer.preparations <= 24);
        render(&mut renderer, &mut terminal, &cells, 1, 76, 24, false, 0);
        let counts = (
            renderer.fingerprints,
            renderer.preparations,
            renderer.layouts,
        );
        for _ in 0..20 {
            render(&mut renderer, &mut terminal, &cells, 1, 120, 24, false, 0);
            render(&mut renderer, &mut terminal, &cells, 1, 76, 24, false, 0);
        }
        assert_eq!(
            (
                renderer.fingerprints,
                renderer.preparations,
                renderer.layouts
            ),
            counts
        );
        assert!(renderer.preparations <= 30);
    }

    #[test]
    fn animation_does_not_rehash_or_relayout_history() {
        let cells = history(5000);
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        render(&mut renderer, &mut terminal, &cells, 1, 100, 24, true, 0);
        let counts = (
            renderer.fingerprints,
            renderer.preparations,
            renderer.layouts,
        );
        for tick in 1..30 {
            render(&mut renderer, &mut terminal, &cells, 1, 100, 24, true, tick);
        }
        assert_eq!(
            (
                renderer.fingerprints,
                renderer.preparations,
                renderer.layouts
            ),
            counts
        );
    }

    #[test]
    fn initial_view_matches_full_history_reference() {
        let cells = vec![
            Cell::User("Please inspect this code\nand explain it".into()),
            Cell::Assistant {
                text: "Some **text**\n\n```rust\nfn main() {}\n```".into(),
                open: false,
            },
            Cell::Tool {
                id: "hidden".into(),
                name: "job_output".into(),
                input: serde_json::json!({}),
                status: ToolStatus::Ok,
                output: Some("invisible".repeat(1000)),
                expanded: true,
            },
            Cell::Notice("last line".into()),
        ];
        for width in [20, 60, 100] {
            for height in [1, 5, 30] {
                let mut actual = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut expected = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut renderer = ScrollbackRenderer::new();
                render(
                    &mut renderer,
                    &mut actual,
                    &cells,
                    1,
                    width,
                    height,
                    false,
                    0,
                );
                let full = reference(&cells, (width - SIDE_PAD * 2) as usize);
                let start = full.len().saturating_sub(height as usize);
                expected
                    .draw(|f| {
                        f.render_widget(
                            Paragraph::new(full[start..].to_vec()),
                            Rect::new(SIDE_PAD, 0, width - SIDE_PAD * 2, height),
                        )
                    })
                    .unwrap();
                assert_eq!(
                    actual.backend().buffer(),
                    expected.backend().buffer(),
                    "{width}x{height}"
                );
            }
        }
    }

    #[test]
    fn scroll_anchor_survives_appends_and_width_changes() {
        let mut cells: Vec<_> = (0..80).map(|i| Cell::Notice(format!("row {i}"))).collect();
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        render(&mut renderer, &mut terminal, &cells, 1, 120, 12, false, 0);
        renderer.scroll_up(20);
        render(&mut renderer, &mut terminal, &cells, 1, 120, 12, false, 0);
        let anchor = renderer.anchor;
        let owner = renderer.hit_test_offset(3);
        assert!(!renderer.at_bottom());
        cells.extend((80..100).map(|i| Cell::Notice(format!("row {i}"))));
        for width in [76, 120, 76, 120] {
            render(&mut renderer, &mut terminal, &cells, 2, width, 12, true, 1);
            assert_eq!(renderer.anchor, anchor);
            assert_eq!(renderer.hit_test_offset(3), owner);
        }
        renderer.scroll_down(10000);
        render(&mut renderer, &mut terminal, &cells, 2, 120, 12, false, 0);
        assert!(renderer.at_bottom());
    }

    #[test]
    fn hit_testing_tracks_offsets_and_hidden_cells() {
        let cells = vec![
            Cell::Tool {
                id: "poll".into(),
                name: "job_status".into(),
                input: serde_json::json!({}),
                status: ToolStatus::Ok,
                output: Some("hidden".into()),
                expanded: true,
            },
            Cell::Assistant {
                text: (0..20).map(|i| format!("line {i}\n")).collect(),
                open: false,
            },
        ];
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 5)).unwrap();
        render(&mut renderer, &mut terminal, &cells, 1, 80, 5, false, 0);
        let (index, offset) = renderer.hit_test_offset(0).unwrap();
        assert_eq!(index, 1);
        assert!(offset > 0);
        assert_eq!(renderer.hit_test_offset(1), Some((1, offset + 1)));
        assert_eq!(renderer.hit_test_offset(4), None);
        assert_eq!(renderer.preparations, 1);
    }

    #[test]
    fn visible_changes_and_clear_invalidate_correctly() {
        let mut cells = vec![Cell::Notice("before".into())];
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).unwrap();
        render(&mut renderer, &mut terminal, &cells, 1, 80, 10, false, 0);
        let count = renderer.preparations;
        cells[0] = Cell::Notice("after".into());
        render(&mut renderer, &mut terminal, &cells, 2, 80, 10, false, 0);
        assert_eq!(renderer.preparations, count + 1);
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
            .contains("after"));
        cells.clear();
        renderer.scroll_up(50);
        render(&mut renderer, &mut terminal, &cells, 3, 80, 10, false, 0);
        assert!(renderer.cache.is_empty());
        assert!(renderer.at_bottom());
        assert!(renderer.hit_test_offset(0).is_none());
    }

    #[test]
    fn scrolled_hint_is_centered_clamped_and_confined_to_the_conversation() {
        let cells: Vec<_> = (0..40).map(|i| Cell::Notice(format!("row {i}"))).collect();
        for viewport_width in [0, 1, 2, 7, 13, 40, 41, 70] {
            let mut renderer = ScrollbackRenderer::new();
            let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
            let full = Rect::new(3, 2, viewport_width, 5);
            terminal
                .draw(|f| renderer.render(f, full, &cells, 1, false, 0, 0))
                .unwrap();
            renderer.scroll_up(3);
            terminal
                .draw(|f| renderer.render(f, full, &cells, 1, false, 0, 0))
                .unwrap();
            let area_width = viewport_width.saturating_sub(SIDE_PAD * 2);
            let hint_width = area_width.min(12);
            let start = full.x + SIDE_PAD + (area_width - hint_width) / 2;
            let buffer = terminal.backend().buffer();
            for x in 0..100 {
                assert_eq!(
                    buffer[(x, full.y)].bg == Palette::POPUP_BG(),
                    hint_width > 0 && x >= start && x < start + hint_width
                );
            }
        }
    }

    #[test]
    fn wrapped_workflow_rows_keep_their_logical_click_targets() {
        use view_types::{WfAgent, WfPhase, WfStatus};
        let cells = vec![Cell::Workflow {
            id: "wf-test".into(),
            title: "a workflow title long enough to wrap across rows".into(),
            done: false,
            phases: vec![WfPhase {
                title: "phase".into(),
                index: 0,
                total: 1,
                agents: vec![WfAgent {
                    agent_id: "worker".into(),
                    label: "agent".into(),
                    status: WfStatus::Running,
                    tools: 0,
                    model: None,
                    tokens: 0,
                    duration_secs: None,
                    started_unix: 0,
                }],
            }],
        }];
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(32, 30)).unwrap();
        render(&mut renderer, &mut terminal, &cells, 1, 32, 30, false, 0);
        let mut header_rows = 0;
        let mut phase_rows = 0;
        let mut agent_rows = 0;
        for row in 0..30 {
            if let Some((index, source_row)) = renderer.hit_test_offset(row) {
                assert_eq!(index, 0);
                let target = cells[index].workflow_agent_at(source_row);
                match source_row {
                    0 => {
                        header_rows += 1;
                        assert!(target.is_none());
                    }
                    1 => {
                        phase_rows += 1;
                        assert!(target.is_none());
                    }
                    2 => {
                        agent_rows += 1;
                        assert_eq!(target, Some("worker"));
                    }
                    _ => assert!(target.is_none()),
                }
            }
        }
        assert!(header_rows > 1);
        assert!(phase_rows > 0 && agent_rows > 0);
    }

    #[test]
    fn large_scroll_jumps_keep_memory_bounded_during_traversal() {
        let cells: Vec<_> = (0..5000)
            .map(|i| Cell::Notice(format!("row {i}")))
            .collect();
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        render(&mut renderer, &mut terminal, &cells, 1, 80, 12, false, 0);
        let bottom = terminal.backend().buffer().clone();
        renderer.scroll_up(usize::MAX);
        render(&mut renderer, &mut terminal, &cells, 1, 80, 12, false, 0);
        assert_eq!(renderer.anchor, Some(Anchor::Start));
        assert!(renderer.cache.len() <= MAX_CACHED_CELLS);
        assert!(renderer.peak_cached <= MAX_CACHED_CELLS + 1);
        renderer.scroll_down(usize::MAX);
        render(&mut renderer, &mut terminal, &cells, 1, 80, 12, false, 0);
        assert!(renderer.at_bottom());
        assert!(renderer.peak_cached <= MAX_CACHED_CELLS + 1);
        assert_eq!(terminal.backend().buffer(), &bottom);
    }

    #[test]
    #[ignore = "manual timing comparison, not a wall-clock CI assertion"]
    fn benchmark_large_history_sidebar_toggles() {
        let cells = history(5000);
        let start = std::time::Instant::now();
        for _ in 0..6 {
            std::hint::black_box(reference(&cells, 114));
            std::hint::black_box(reference(&cells, 70));
        }
        let legacy = start.elapsed();
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let start = std::time::Instant::now();
        render(&mut renderer, &mut terminal, &cells, 1, 120, 24, false, 0);
        render(&mut renderer, &mut terminal, &cells, 1, 76, 24, false, 0);
        let cold = start.elapsed();
        let prepared = renderer.preparations;
        let start = std::time::Instant::now();
        for _ in 0..6 {
            render(&mut renderer, &mut terminal, &cells, 1, 120, 24, false, 0);
            render(&mut renderer, &mut terminal, &cells, 1, 76, 24, false, 0);
        }
        println!("5000 cells, 12 toggles: full-history reference {legacy:?}; cold two-width viewport {cold:?}; warm viewport {:?}; prepared {prepared} cells", start.elapsed());
        assert_eq!(renderer.preparations, prepared);
    }
}
