//! Viewport-driven conversation rendering. Expensive cell content is prepared
//! once; only visited cells are laid out, with the two most recent widths kept.

use super::exploration::Exploration;
use super::render::{with_left_lead, PreparedCell};
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
const EXPLORATION_INDENT: usize = 2;
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
    logical_rows: Arc<Vec<(usize, usize)>>,
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
    exploration: Exploration,
    cache: HashMap<usize, CachedCell>,
    cache_limit: usize,
    generation: Option<u64>,
    clock: u64,
    previous_len: usize,
    anchor: Option<Anchor>,
    last_top: Option<Anchor>,
    logical_top: Option<(usize, usize, usize)>,
    last_width: usize,
    interrupt_hint: &'static str,
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
        Self {
            interrupt_hint: "esc to interrupt",
            ..Self::default()
        }
    }

    pub fn set_interrupt_hint(&mut self, hint: &'static str) {
        self.interrupt_hint = hint;
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

    pub fn click_exploration(&mut self, row: u16) -> bool {
        let Some((index, offset)) = self.hit_test_offset(row) else {
            return false;
        };
        let Some(group) = self.exploration.group(index) else {
            return false;
        };
        let first = group.first;
        if index != first || offset != 0 {
            return true;
        }
        self.cache.retain(|index, _| {
            self.exploration
                .group(*index)
                .is_none_or(|group| group.first != first)
        });
        self.exploration.toggle(first);
        self.anchor = self.last_top;
        true
    }

    fn layout(&mut self, ctx: &RenderContext<'_>, index: usize) -> CellLayout {
        let group = self.exploration.group(index);
        self.clock = self.clock.wrapping_add(1);
        if self
            .cache
            .get(&index)
            .is_none_or(|entry| entry.validated_revision != ctx.revision)
        {
            let fingerprint = group.map_or_else(
                || ctx.cells[index].fingerprint(),
                |group| group.fingerprint(&ctx.cells[index], index),
            );
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
                        prepared: match group {
                            Some(group) if group.shows_tool(index) => {
                                PreparedCell::compact_tool(&ctx.cells[index])
                            }
                            Some(_) => PreparedCell::default(),
                            None => PreparedCell::new(&ctx.cells[index]),
                        },
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
        let is_plan = matches!(&ctx.cells[index], Cell::Plan(_));
        let wrap_width = if is_user {
            ctx.width
        } else {
            ctx.width
                .saturating_sub(HANGING_INDENT + RIGHT_MARGIN)
                .max(1)
        };
        let mut rendered = Vec::new();
        let mut tool_rows = 0..0;
        if let Some(group) = self.exploration.group(index) {
            if index == group.first {
                rendered.push(group.header());
            }
            if group.shows_tool(index) {
                let start = rendered.len();
                entry.prepared.render(
                    &ctx.cells[index],
                    ctx.width.saturating_sub(EXPLORATION_INDENT),
                    &mut rendered,
                );
                tool_rows = start..rendered.len();
            }
            if index == group.last_visible {
                rendered.push(Line::from(""));
            }
        } else {
            entry
                .prepared
                .render(&ctx.cells[index], ctx.width, &mut rendered);
        }
        let mut lines = Vec::new();
        let mut source_rows = Vec::new();
        let mut logical_rows = Vec::new();
        let mut user_offset = 0;
        for (source_row, mut line) in rendered.into_iter().enumerate() {
            let user_content = is_user
                .then(|| line.spans.get(2).map(|span| span.width()))
                .flatten();
            // Wrap plan content only after assigning its logical line, excluding the
            // repeated bar from content offsets so reflow keeps the same source position.
            let lead = if tool_rows.contains(&source_row) {
                Some(Span::raw(" ".repeat(EXPLORATION_INDENT)))
            } else {
                (is_plan && !line.spans.is_empty()).then(|| line.spans.remove(0))
            };
            let lead_width = lead.as_ref().map_or(0, Span::width);
            let rows = match lead {
                Some(lead) => with_left_lead(vec![line], lead, wrap_width),
                None => wrap_line(line, wrap_width),
            };
            let mut offset = 0;
            for mut row in rows {
                let logical = if user_content.is_some() {
                    (1, user_offset)
                } else if is_user {
                    (if source_row == 0 { 0 } else { usize::MAX }, 0)
                } else {
                    (source_row, offset)
                };
                offset += row.width().saturating_sub(lead_width);
                logical_rows.push(logical);
                if !is_user {
                    row.spans.insert(0, Span::raw(" ".repeat(HANGING_INDENT)));
                }
                lines.push(row);
                source_rows.push(source_row);
            }
            user_offset += user_content.unwrap_or(0);
        }
        let layout = CellLayout {
            width: ctx.width,
            lines: Arc::new(lines),
            source_rows: Arc::new(source_rows),
            logical_rows: Arc::new(logical_rows),
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
        let start = self
            .exploration
            .visible
            .partition_point(|index| *index < start);
        for position in start..self.exploration.visible.len() {
            let index = self.exploration.visible[position];
            if !self.layout(ctx, index).lines.is_empty() {
                return Anchor::Cell { index, row: 0 };
            }
        }
        Anchor::Footer { row: 0 }
    }

    fn last_before(&mut self, ctx: &RenderContext<'_>, end: usize) -> Anchor {
        let end = self
            .exploration
            .visible
            .partition_point(|index| *index < end);
        for position in (0..end).rev() {
            let index = self.exploration.visible[position];
            let height = self.layout(ctx, index).lines.len();
            if height > 0 {
                return Anchor::Cell {
                    index,
                    row: height - 1,
                };
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
                if self.exploration.visible.binary_search(&index).is_err() {
                    if let Some(group) = self.exploration.group(index) {
                        return Some(Anchor::Cell {
                            index: group.first,
                            row: 0,
                        });
                    }
                }
                if self.exploration.visible.binary_search(&index).is_ok() {
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
        self.exploration.refresh(cells, revision);
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
                format!(" ({}s · {})", turn_elapsed_secs, self.interrupt_hint),
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
        if self.last_width != width {
            if let Some((index, source, offset)) = self.logical_top {
                if index < cells.len() && self.anchor.is_some() {
                    let layout = self.layout(&ctx, index);
                    let row = layout
                        .logical_rows
                        .iter()
                        .enumerate()
                        .filter(|(_, (line, column))| *line == source && *column <= offset)
                        .map(|(row, _)| row)
                        .next_back()
                        .unwrap_or(0);
                    self.anchor = Some(Anchor::Cell { index, row });
                    self.last_top = self.anchor;
                }
            }
        }
        self.last_width = width;
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
        self.logical_top = match self.anchor {
            Some(Anchor::Cell { index, row }) => {
                let layout = self.layout(&ctx, index);
                let (source, offset) = layout.logical_rows[row];
                let offset = self
                    .logical_top
                    .filter(|(old_index, old_source, _)| {
                        pending == 0 && *old_index == index && *old_source == source
                    })
                    .map(|(_, _, offset)| offset)
                    .unwrap_or(offset);
                Some((index, source, offset))
            }
            _ => None,
        };
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

    fn exploration_tool(index: usize) -> Cell {
        Cell::Tool {
            id: format!("read-{index}"),
            name: "read_file".into(),
            input: serde_json::json!({"path": format!("src/file_{index}.rs")}),
            status: ToolStatus::Ok,
            output: Some("large hidden output\n".repeat(1000).into()),
            expanded: true,
        }
    }

    #[test]
    fn exploration_indentation_survives_wrapping_without_changing_source_rows() {
        let path = "src/解析/long_module_name.rs";
        let mut cells: Vec<_> = (0..3).map(exploration_tool).collect();
        for cell in &mut cells {
            if let Cell::Tool { input, .. } = cell {
                *input = serde_json::json!({"path": path});
            }
        }
        let mut renderer = ScrollbackRenderer::new();
        renderer.cache_limit = 3;
        renderer.exploration.refresh(&cells, 1);
        renderer.exploration.toggle(0);
        renderer.exploration.refresh(&cells, 1);
        for width in [24, 48, 100, 24] {
            let ctx = RenderContext {
                cells: &cells,
                revision: 1,
                width,
                footer: vec![],
            };
            for index in 0..3 {
                let layout = renderer.layout(&ctx, index);
                let source_row = usize::from(index == 0);
                let mut recovered = String::new();
                let mut offset = 0;
                for (row, line) in layout.lines.iter().enumerate() {
                    if layout.source_rows[row] != source_row {
                        continue;
                    }
                    assert_eq!(layout.logical_rows[row], (source_row, offset));
                    assert!(line.width() <= width - RIGHT_MARGIN);
                    recovered.push_str(line.to_string().strip_prefix("    ").unwrap());
                    offset += line.width() - HANGING_INDENT - EXPLORATION_INDENT;
                }
                assert_eq!(recovered, format!("• Read {path}"));
                if index == 0 {
                    assert!(layout.lines[0].to_string().starts_with("  • Explored"));
                }
            }
        }
    }

    #[test]
    fn exploration_is_lazy_and_preserves_source_anchors_as_groups_grow() {
        let mut cells: Vec<_> = (0..1000).map(exploration_tool).collect();
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        render(&mut renderer, &mut terminal, &cells, 1, 100, 12, false, 0);
        assert_eq!(renderer.preparations, 1);
        let summary_row = (0..12)
            .find(|row| renderer.hit_test_offset(*row) == Some((0, 0)))
            .unwrap();
        assert!(renderer.click_exploration(summary_row));
        render(&mut renderer, &mut terminal, &cells, 1, 100, 12, false, 0);
        assert_eq!(renderer.hit_test_offset(summary_row), Some((0, 0)));
        assert!(renderer.preparations < 15);
        renderer.scroll_down(35);
        render(&mut renderer, &mut terminal, &cells, 1, 100, 12, false, 0);
        let anchor = renderer.anchor;
        let owner = renderer.hit_test_offset(2);
        let preparations = renderer.preparations;
        for width in [60, 100, 60, 100] {
            cells.push(exploration_tool(cells.len()));
            render(
                &mut renderer,
                &mut terminal,
                &cells,
                cells.len() as u64,
                width,
                12,
                true,
                1,
            );
            assert_eq!(renderer.anchor, anchor);
            assert_eq!(renderer.hit_test_offset(2), owner);
            assert_eq!(renderer.preparations, preparations);
        }
        let before = (
            renderer.fingerprints,
            renderer.preparations,
            renderer.layouts,
        );
        for tick in 2..10 {
            render(
                &mut renderer,
                &mut terminal,
                &cells,
                cells.len() as u64,
                100,
                12,
                true,
                tick,
            );
        }
        assert_eq!(
            before,
            (
                renderer.fingerprints,
                renderer.preparations,
                renderer.layouts
            )
        );
        renderer.exploration.toggle(0);
        renderer.cache.clear();
        render(
            &mut renderer,
            &mut terminal,
            &cells,
            cells.len() as u64,
            100,
            12,
            false,
            0,
        );
        assert_eq!(renderer.exploration.visible, [0]);
        assert!(renderer.at_bottom());
        assert!(terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .contains("Explored 1004 files"));
    }

    #[test]
    fn exploration_replays_from_events_and_messages_without_changing_tool_data() {
        use bob_core::core::events::AgentEvent;
        use bob_core::core::types::{ContentBlock, Message, Role};
        let mut live = view_types::ViewModel::new();
        let mut events = Vec::new();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for (index, name) in ["read_file", "grep", "list_dir"].into_iter().enumerate() {
            let id = format!("call-{index}");
            let input = serde_json::json!({"path": "src", "pattern": "needle"});
            events.push(AgentEvent::ToolCall {
                agent_id: "root".into(),
                tool_use_id: id.clone(),
                name: name.into(),
                input: input.clone(),
            });
            calls.push(ContentBlock::ToolUse {
                id: id.clone(),
                name: name.into(),
                input,
            });
            results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: format!("payload-{index}").into(),
                is_error: Some(index == 1),
            });
        }
        for result in results.iter().rev() {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = result
            else {
                unreachable!()
            };
            events.push(AgentEvent::ToolResult {
                agent_id: "root".into(),
                tool_use_id: tool_use_id.clone(),
                output: content.clone(),
                is_error: is_error.unwrap(),
            });
        }
        for event in &events {
            live.apply(event);
        }
        let saved = serde_json::to_string(&events).unwrap();
        let mut replay = view_types::ViewModel::new();
        for event in serde_json::from_str::<Vec<AgentEvent>>(&saved).unwrap() {
            replay.apply(&event);
        }
        replay.reset_after_replay();
        let mut hydrated = view_types::ViewModel::new();
        hydrated.hydrate(
            &[
                Message {
                    role: Role::Assistant,
                    content: calls,
                },
                Message {
                    role: Role::Tool,
                    content: results,
                },
            ],
            &[],
        );
        let mut expected = None;
        for view in [&live, &replay, &hydrated] {
            let mut renderer = ScrollbackRenderer::new();
            let mut terminal = Terminal::new(TestBackend::new(90, 15)).unwrap();
            render(
                &mut renderer,
                &mut terminal,
                &view.cells,
                view.revision,
                90,
                15,
                false,
                0,
            );
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("Explored 1 file · 1 search · 1 directory · 1 failed"));
            assert!(text.contains("payload-1"));
            assert!(!text.contains("payload-0") && !text.contains("payload-2"));
            if let Some(expected) = &expected {
                assert_eq!(terminal.backend().buffer(), expected);
            } else {
                expected = Some(terminal.backend().buffer().clone());
            }
            for (index, cell) in view.cells.iter().enumerate() {
                assert!(
                    matches!(cell, Cell::Tool { output: Some(output), .. } if output.as_text() == Some(format!("payload-{index}").as_str()))
                );
            }
        }
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
                output: Some("invisible".repeat(1000).into()),
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
    fn offset_viewport_has_exact_text_styles_and_hit_rows() {
        let cells = vec![
            Cell::Notice("first".into()),
            Cell::Tool {
                id: "hidden".into(),
                name: "job_output".into(),
                input: serde_json::json!({}),
                status: ToolStatus::Error,
                output: Some("must not appear".into()),
                expanded: true,
            },
            Cell::Notice("error: bad".into()),
        ];
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(32, 10)).unwrap();
        terminal
            .draw(|frame| {
                renderer.render(frame, Rect::new(3, 2, 24, 6), &cells, 1, false, 0, 0);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..10 {
            let text: String = (0..32).map(|x| buffer[(x, y)].symbol()).collect();
            let expected = match y {
                3 => format!("{}first{}", " ".repeat(9), " ".repeat(18)),
                4 => format!("{}error: bad{}", " ".repeat(9), " ".repeat(13)),
                _ => " ".repeat(32),
            };
            assert_eq!(text, expected, "row {y}");
            assert_eq!(
                renderer.hit_test_offset(y),
                match y {
                    3 => Some((0, 0)),
                    4 => Some((2, 0)),
                    _ => None,
                }
            );
        }
        assert_eq!(buffer[(9, 3)].fg, Palette::DIM());
        assert_eq!(buffer[(9, 4)].fg, Palette::ERROR());
        assert_eq!(renderer.preparations, 2);
        assert!(renderer.at_bottom());

        for empty in [Rect::new(3, 2, 4, 6), Rect::new(3, 2, 24, 0)] {
            terminal
                .draw(|frame| renderer.render(frame, empty, &cells, 1, false, 0, 0))
                .unwrap();
            assert!((0..10).all(|row| renderer.hit_test_offset(row).is_none()));
            assert!(terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| cell.symbol() == " "));
            assert_eq!(renderer.preparations, 2);
        }
    }

    #[test]
    fn revision_reuses_unchanged_content_but_invalidates_both_cached_widths() {
        let mut cells = vec![Cell::Assistant {
            text: "before".into(),
            open: true,
        }];
        let mut renderer = ScrollbackRenderer::new();
        renderer.cache_limit = 2;
        let layout = |renderer: &mut ScrollbackRenderer, cells: &[Cell], revision, width| {
            renderer.layout(
                &RenderContext {
                    cells,
                    revision,
                    width,
                    footer: vec![],
                },
                0,
            )
        };
        let narrow = layout(&mut renderer, &cells, 1, 20);
        let wide = layout(&mut renderer, &cells, 1, 30);
        assert_eq!(
            narrow.lines.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["  before", "  "]
        );
        assert_eq!(
            wide.lines.iter().map(Line::to_string).collect::<Vec<_>>(),
            ["  before", "  "]
        );
        assert_eq!(
            (
                renderer.fingerprints,
                renderer.preparations,
                renderer.layouts
            ),
            (1, 1, 2)
        );
        if let Cell::Assistant { open, .. } = &mut cells[0] {
            *open = false;
        }
        assert!(Arc::ptr_eq(
            &narrow.lines,
            &layout(&mut renderer, &cells, 2, 20).lines
        ));
        assert!(Arc::ptr_eq(
            &wide.lines,
            &layout(&mut renderer, &cells, 2, 30).lines
        ));
        assert_eq!(
            (
                renderer.fingerprints,
                renderer.preparations,
                renderer.layouts
            ),
            (2, 1, 2)
        );
        if let Cell::Assistant { text, .. } = &mut cells[0] {
            *text = "**after**".into();
        }
        for (width, previous) in [(20, narrow), (30, wide)] {
            let changed = layout(&mut renderer, &cells, 3, width);
            assert!(!Arc::ptr_eq(&previous.lines, &changed.lines));
            assert_eq!(
                changed
                    .lines
                    .iter()
                    .map(Line::to_string)
                    .collect::<Vec<_>>(),
                ["  after", "  "]
            );
            let text = changed.lines[0]
                .spans
                .iter()
                .find(|span| span.content == "after")
                .unwrap();
            assert_eq!(text.style.fg, Some(Palette::TEXT()));
            assert!(text
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD));
        }
        assert_eq!(
            (
                renderer.fingerprints,
                renderer.preparations,
                renderer.layouts
            ),
            (3, 2, 4)
        );
    }

    #[test]
    fn shell_metadata_updates_invalidate_cached_output_with_unchanged_text() {
        use bob_core::core::events::AgentEvent;
        use bob_core::core::types::{ShellOutcome, ShellOutput, ShellStream};

        let mut view = view_types::ViewModel::new();
        view.apply(&AgentEvent::ToolCall {
            agent_id: "root".into(),
            tool_use_id: "shell".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "check"}),
        });
        let mut renderer = ScrollbackRenderer::new();
        renderer.cache_limit = 2;
        let mut previous = None;
        for (index, (code, outcome, reference, expected)) in [
            (0, ShellOutcome::Exited, None, None),
            (7, ShellOutcome::Exited, None, Some("exit 7")),
            (0, ShellOutcome::TimedOut, None, Some("timed out")),
            (
                0,
                ShellOutcome::TimedOut,
                Some("output_1"),
                Some("timed out"),
            ),
        ]
        .into_iter()
        .enumerate()
        {
            view.apply(&AgentEvent::ToolResult {
                agent_id: "root".into(),
                tool_use_id: "shell".into(),
                output: ShellOutput {
                    exit_code: Some(code),
                    outcome,
                    stdout: ShellStream {
                        text: "same text".into(),
                        total_bytes: 9,
                        truncated: reference.is_some(),
                        output_id: reference.map(str::to_owned),
                    },
                    ..Default::default()
                }
                .into(),
                is_error: false,
            });
            let layout = renderer.layout(
                &RenderContext {
                    cells: &view.cells,
                    revision: view.revision,
                    width: 100,
                    footer: vec![],
                },
                0,
            );
            if let Some(previous) = &previous {
                assert!(!Arc::ptr_eq(previous, &layout.lines));
            }
            let mut expected_rows = vec!["  • Bash check".to_owned()];
            if let Some(status) = expected {
                expected_rows.push(format!("      {status}"));
            }
            if let Some(reference) = reference {
                expected_rows.push(format!(
                    "      stdout truncated; retained output: {reference}"
                ));
            }
            expected_rows.extend(["    │ same text".to_owned(), "  ".to_owned()]);
            assert_eq!(
                layout.lines.iter().map(Line::to_string).collect::<Vec<_>>(),
                expected_rows
            );
            assert_eq!(renderer.preparations, index + 1);
            previous = Some(layout.lines);
        }
    }

    #[test]
    fn expanding_a_cached_read_reveals_the_latest_previously_hidden_output() {
        let mut view = view_types::ViewModel::new();
        view.cells.push(Cell::Tool {
            id: "read".into(),
            name: "read_file".into(),
            input: serde_json::json!({"path": "file.rs"}),
            status: ToolStatus::Ok,
            output: Some("old".into()),
            expanded: false,
        });
        let mut renderer = ScrollbackRenderer::new();
        renderer.cache_limit = 2;
        let layout = |renderer: &mut ScrollbackRenderer, view: &view_types::ViewModel| {
            renderer.layout(
                &RenderContext {
                    cells: &view.cells,
                    revision: view.revision,
                    width: 40,
                    footer: vec![],
                },
                0,
            )
        };
        let collapsed = layout(&mut renderer, &view);
        view.apply(&bob_core::core::events::AgentEvent::ToolResult {
            agent_id: "root".into(),
            tool_use_id: "read".into(),
            output: "latest\nsecond".into(),
            is_error: false,
        });
        let updated = layout(&mut renderer, &view);
        assert!(Arc::ptr_eq(&collapsed.lines, &updated.lines));
        assert_eq!(
            updated
                .lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>(),
            ["  • Read file.rs", "  "]
        );
        assert_eq!(renderer.preparations, 1);
        assert!(view.toggle_tool_expanded(0));
        let expanded = layout(&mut renderer, &view);
        assert_eq!(
            expanded
                .lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>(),
            ["  • Read file.rs", "      latest", "      second", "  ",]
        );
        assert_eq!(renderer.preparations, 2);
        assert!(view.toggle_tool_expanded(0));
        let collapsed = layout(&mut renderer, &view);
        assert_eq!(
            collapsed
                .lines
                .iter()
                .map(Line::to_string)
                .collect::<Vec<_>>(),
            ["  • Read file.rs", "  "]
        );
        assert_eq!(renderer.preparations, 3);
    }

    #[test]
    fn retry_progress_replaces_cached_rows_in_root_and_agent_views() {
        use super::super::team::AgentTranscripts;
        use bob_core::core::events::AgentEvent;

        let mut root = view_types::ViewModel::new();
        let mut teams = AgentTranscripts::new();
        let mut root_renderer = ScrollbackRenderer::new();
        let mut agent_renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).unwrap();
        for attempt in 1..=10 {
            for agent_id in ["root", "worker"] {
                let event = AgentEvent::StreamRetry {
                    agent_id: agent_id.into(),
                    attempt,
                    max_attempts: 10,
                };
                root.apply(&event);
                teams.apply(&event, Some("worker"));
            }
            let thread = teams.get("worker").unwrap();
            for (renderer, cells, revision) in [
                (&mut root_renderer, &root.cells, root.revision),
                (&mut agent_renderer, &thread.cells, thread.revision),
            ] {
                assert_eq!(cells.len(), 1);
                render(renderer, &mut terminal, cells, revision, 100, 10, true, 0);
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert_eq!(text.matches("connection hiccup").count(), 1);
                assert!(text.contains(&format!("retry {attempt} of 10")));
                assert_eq!(renderer.preparations, attempt as usize);
            }
        }
        for agent_id in ["root", "worker"] {
            let event = AgentEvent::TextDelta {
                agent_id: agent_id.into(),
                text: "reconnected".into(),
            };
            root.apply(&event);
            teams.apply(&event, Some("worker"));
        }
        let thread = teams.get("worker").unwrap();
        for (renderer, cells, revision) in [
            (&mut root_renderer, &root.cells, root.revision),
            (&mut agent_renderer, &thread.cells, thread.revision),
        ] {
            render(renderer, &mut terminal, cells, revision, 100, 10, true, 0);
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("reconnected"));
            assert!(!text.contains("connection hiccup"));
        }
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
                title: "phase title long enough to wrap across rows".into(),
                index: 0,
                total: 1,
                agents: vec![WfAgent {
                    agent_id: "worker".into(),
                    label: "agent label long enough to wrap across rows".into(),
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
                        if agent_rows == 0 {
                            let text = (0..32)
                                .map(|x| terminal.backend().buffer()[(x, row)].symbol())
                                .collect::<String>();
                            assert!(text.contains("• agent label"), "{text}");
                        }
                        agent_rows += 1;
                        assert_eq!(target, Some("worker"));
                    }
                    _ => assert!(target.is_none()),
                }
            }
        }
        assert!(header_rows > 1 && phase_rows > 1 && agent_rows > 1);
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

    #[test]
    fn resize_reanchors_to_logical_content_instead_of_wrapped_row_number() {
        let cells = vec![Cell::Assistant {
            text: (0..60)
                .map(|index| format!("logical line {index}: {}", "words ".repeat(15)))
                .collect::<Vec<_>>()
                .join("\n\n"),
            open: false,
        }];
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|frame| renderer.render(frame, Rect::new(0, 0, 100, 12), &cells, 1, false, 0, 0))
            .unwrap();
        renderer.scroll_up(25);
        terminal
            .draw(|frame| renderer.render(frame, Rect::new(0, 0, 100, 12), &cells, 1, false, 0, 0))
            .unwrap();
        let (index, source, offset) = renderer.logical_top.unwrap();
        terminal
            .draw(|frame| renderer.render(frame, Rect::new(0, 0, 45, 12), &cells, 1, false, 0, 0))
            .unwrap();
        let (new_index, new_source, new_offset) = renderer.logical_top.unwrap();
        assert_eq!((index, source), (new_index, new_source));
        assert_eq!(new_offset, offset);
        for width in [100, 45, 100, 45] {
            terminal
                .draw(|frame| {
                    renderer.render(frame, Rect::new(0, 0, width, 12), &cells, 1, false, 0, 0)
                })
                .unwrap();
            assert_eq!(renderer.logical_top, Some((index, source, offset)));
        }
        assert!(!renderer.at_bottom());
    }

    #[test]
    fn plan_reflow_preserves_later_paragraph_phrase_and_content_offset() {
        let paragraphs: Vec<String> = (0..30)
            .map(|paragraph| {
                (0..120)
                    .map(|word| format!("p{paragraph:02}w{word:03} "))
                    .collect()
            })
            .collect();
        let cells = vec![Cell::Plan(paragraphs.join("\n\n"))];
        let mut renderer = ScrollbackRenderer::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        let draw =
            |renderer: &mut ScrollbackRenderer, terminal: &mut Terminal<TestBackend>, width| {
                terminal
                    .draw(|frame| {
                        renderer.render(frame, Rect::new(0, 0, width, 12), &cells, 1, false, 0, 0)
                    })
                    .unwrap();
            };
        draw(&mut renderer, &mut terminal, 100);
        let layout = &renderer.cache[&0].layouts[0];
        let target_row = layout
            .lines
            .iter()
            .position(|line| line.to_string().contains("p12w000"))
            .unwrap()
            + 4;
        let content = |line: &Line<'_>| {
            line.spans
                .iter()
                .skip(2)
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let target_content = content(&layout.lines[target_row]);
        let target_offset = paragraphs[12].find(&target_content).unwrap();
        assert!(target_offset > 0);
        let phrase = &paragraphs[12][target_offset..target_offset + 24];
        let Some(Anchor::Cell {
            index: 0,
            row: bottom_row,
        }) = renderer.last_top
        else {
            panic!("expected initial viewport within the plan");
        };
        renderer.scroll_up(bottom_row - target_row);
        draw(&mut renderer, &mut terminal, 100);
        let original = renderer.logical_top.unwrap();
        assert_eq!(original.2, target_offset);

        let mut warm_counts = None;
        for width in [45, 100, 45, 100, 45, 100] {
            draw(&mut renderer, &mut terminal, width);
            assert_eq!(renderer.logical_top, Some(original));
            let Some(Anchor::Cell { index: 0, row }) = renderer.anchor else {
                panic!("expected scrolled viewport within the plan");
            };
            let layout = &renderer.cache[&0].layouts[0];
            let (source, column) = layout.logical_rows[row];
            assert_eq!(source, original.1);
            let top_content = content(&layout.lines[row]);
            let actual_column = paragraphs[12].find(&top_content).unwrap();
            assert_eq!(column, actual_column);
            assert!(actual_column <= target_offset);
            assert!(target_offset < actual_column + top_content.len());
            let visible_content = layout.lines[row..row + 2]
                .iter()
                .map(&content)
                .collect::<String>();
            assert!(visible_content[target_offset - actual_column..].starts_with(phrase));
            if width == 100 {
                assert_eq!(row, target_row);
                assert_eq!(top_content, target_content);
            }
            for visible_row in 0..12 {
                assert_eq!(layout.lines[row + visible_row].spans[1].content, " ▏ ");
                assert!(layout.lines[row + visible_row].width() <= (width - SIDE_PAD * 2) as usize);
                let bar_x = SIDE_PAD + HANGING_INDENT as u16 + 1;
                assert_eq!(
                    terminal.backend().buffer()[(bar_x, visible_row as u16)].symbol(),
                    "▏"
                );
            }
            let counts = (renderer.preparations, renderer.layouts);
            match warm_counts {
                None => warm_counts = Some(counts),
                Some(expected) => assert_eq!(counts, expected),
            }
        }
    }
}
