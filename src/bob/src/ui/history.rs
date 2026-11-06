use super::super::exploration;
use super::{AnchorId, Pane, View};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Query {
    Replace(Option<i64>),
    Around(i64),
    Refresh(Option<i64>),
    Older(i64),
    Newer(i64),
}
impl Query {
    pub(super) fn before(self) -> Option<i64> {
        match self {
            Self::Replace(before) | Self::Refresh(before) => before,
            Self::Older(before) => Some(before),
            Self::Around(anchor) => anchor.checked_add(1),
            Self::Newer(_) => None,
        }
    }

    pub(super) async fn read(
        self,
        handle: &bob_kernel::Handle,
        thread: bob_kernel::ThreadId,
        limit: usize,
    ) -> anyhow::Result<View> {
        match self {
            Self::Newer(after) | Self::Around(after) => {
                let previous = handle
                    .transcript(Some(thread.clone()), after.checked_add(1), limit)
                    .await?;
                let mut next = handle.transcript_after(Some(thread), after, limit).await?;
                next.entries.splice(..0, previous.entries);
                Ok(next)
            }
            _ => handle.transcript(Some(thread), self.before(), limit).await,
        }
    }
}

#[derive(Default)]
pub(super) struct History {
    pub(super) pending: Option<Query>,
    pub(super) direction: Option<bool>,
    pub(super) older_exhausted: bool,
    pub(super) debt: isize,
}

impl Pane {
    pub(super) fn history_query(&self) -> Option<Query> {
        let view = self.view.as_ref()?;
        let height = self.viewport.height.max(1);
        // The whole conversation stays in memory: backfill older pages until exhausted.
        if !self.history.older_exhausted {
            return view
                .entries
                .first()
                .map(|entry| Query::Older(entry.sequence));
        }
        match self.history.direction {
            Some(false)
                if self.before.is_some()
                    && (self.viewport.top.saturating_add(height * 2)
                        >= self.viewport.rows.len()
                        || self.history.debt > 0) =>
            {
                view.entries
                    .last()
                    .map(|entry| Query::Newer(entry.sequence))
            }
            _ => None,
        }
    }

    pub(super) fn refresh_query(&self) -> Query {
        let Some(view) = &self.view else {
            return self
                .viewport
                .anchor
                .as_ref()
                .and_then(|anchor| anchor.identity.sequence())
                .map_or_else(|| Query::Replace(self.query_before()), Query::Around);
        };
        if self.before.is_none()
            && (self.viewport.anchor.is_none()
                || self
                    .viewport
                    .top
                    .saturating_add(self.viewport.height.max(1) * 2)
                    >= self.viewport.rows.len())
        {
            if let Some(last) = view.entries.last() {
                return Query::Newer(last.sequence);
            }
        }
        Query::Refresh(self.query_before())
    }

    pub(super) fn stitch_history(&mut self, view: &mut View, query: Query, limit: usize) {
        let count = match query {
            Query::Newer(after) | Query::Around(after) => view
                .entries
                .iter()
                .filter(|entry| entry.sequence > after)
                .count(),
            _ => view.entries.len(),
        };
        let old_groups: Vec<_> = self
            .view
            .as_ref()
            .map(|previous| {
                exploration::groups(&previous.entries)
                    .into_iter()
                    .map(|group| {
                        let first = group.entries[0].sequence;
                        let last = group.entries.last().unwrap().sequence;
                        let expanded = self.expanded.contains(group.first());
                        let anchored = self.viewport.anchor.as_ref().is_some_and(|anchor| {
                            anchor.identity == AnchorId::Entry(first)
                                && (anchor.line == 0 || !expanded)
                        });
                        (first, last, expanded, anchored)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut entries = self
            .view
            .as_mut()
            .map(|previous| std::mem::take(&mut previous.entries))
            .unwrap_or_default();
        match query {
            Query::Replace(before) => {
                entries.clear();
                self.history.older_exhausted = count < limit;
                self.before = before;
            }
            Query::Around(anchor) => {
                entries.clear();
                self.history.older_exhausted = view
                    .entries
                    .iter()
                    .filter(|entry| entry.sequence <= anchor)
                    .count()
                    < limit;
                self.before = if count < limit {
                    None
                } else {
                    view.entries
                        .last()
                        .and_then(|entry| entry.sequence.checked_add(1))
                };
            }
            Query::Older(_) => self.history.older_exhausted = count < limit,
            Query::Newer(_) => {
                self.before = if count < limit {
                    None
                } else {
                    view.entries
                        .last()
                        .and_then(|entry| entry.sequence.checked_add(1))
                };
                if self.before.is_some() && self.viewport.anchor.is_none() {
                    self.history.direction = Some(false);
                }
            }
            Query::Refresh(_) => {}
        }
        for incoming in std::mem::take(&mut view.entries) {
            match entries.binary_search_by_key(&incoming.sequence, |entry| entry.sequence) {
                Ok(index) => entries[index] = incoming,
                Err(index) => entries.insert(index, incoming),
            }
        }
        let root = entries.first().is_some_and(|entry| {
            view.threads
                .iter()
                .any(|thread| thread.id == entry.thread && thread.parent_thread.is_none())
        });
        let first = entries.first().map(|entry| entry.sequence);
        let last = entries.last().map(|entry| entry.sequence);
        // Retain query boundaries even when an entire fetched page is invisible.
        entries.retain(|entry| {
            let hidden = (root && !entry.visible_in_root())
                || super::super::visibility::hidden_tool(entry)
                || (entry.kind == bob_kernel::EntryKind::Assistant
                    && entry.text.trim().is_empty()
                    && !entry.is_preview);
            Some(entry.sequence) == first || Some(entry.sequence) == last || !hidden
        });
        for group in exploration::groups(&entries) {
            let first = group.entries[0].sequence;
            let last = group.entries.last().unwrap().sequence;
            for (old_first, old_last, expanded, anchored) in &old_groups {
                if first <= *old_last && last >= *old_first {
                    if *expanded {
                        self.expanded.insert(group.first().clone());
                    }
                    if *anchored {
                        if let Some(anchor) = self.viewport.anchor.as_mut() {
                            anchor.identity = AnchorId::Entry(first);
                            anchor.line = 0;
                        }
                    }
                }
            }
        }
        view.entries = entries;
        if matches!(
            (query, self.history.direction),
            (Query::Older(_), Some(true)) | (Query::Newer(_), Some(false))
        ) {
            let debt = std::mem::take(&mut self.history.debt);
            self.viewport.pending_scroll = self.viewport.pending_scroll.saturating_add(debt);
            self.viewport.pending_follow =
                self.before.is_none() && self.viewport.pending_scroll > 0;
        }
        if self.history.older_exhausted && self.viewport.pending_scroll < 0 && count == 0 {
            self.viewport.pending_scroll = 0;
        }
        self.pinned_before = self.viewport.anchor.as_ref().and_then(|anchor| {
            anchor.identity.sequence()?;
            view.entries.last()?.sequence.checked_add(1)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Anchor, App, Reply, ThreadId};
    use super::*;
    use bob_kernel::{Entry, EntryKind, OperationId};

    fn view(start: i64, end: i64) -> View {
        View {
            policies: Vec::new(),
            sequence: 10_000,
            entry_sequence: 9_999,
            threads: Vec::new(),
            entries: (start..end)
                .map(|sequence| Entry {
                    sequence,
                    sender: None,
                    parent_guidance: false,
                    submission: None,
                    thread: ThreadId("root".into()),
                    kind: EntryKind::Notice,
                    text: format!("entry {sequence}"),
                    artifact: None,
                    operation: None,
                    tool: None,
                    is_preview: false,
                    visible_text_bytes: None,
                })
                .collect(),
            decisions: Vec::new(),
            operations: Vec::new(),
            error: None,
        }
    }

    fn anchor(sequence: i64) -> Anchor {
        Anchor {
            identity: AnchorId::Entry(sequence),
            line: 0,
            offset: 0,
        }
    }

    fn layout(pane: &mut Pane, height: usize) {
        pane.viewport.rows = pane
            .view
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .map(|entry| anchor(entry.sequence))
            .collect();
        pane.viewport.position(height);
    }

    fn accept(pane: &mut Pane, mut view: View, query: Query, limit: usize) {
        pane.stitch_history(&mut view, query, limit);
        pane.view = Some(view);
    }

    #[test]
    fn ordinary_scroll_prefetches_older_without_replacing_or_jumping() {
        let mut pane = Pane {
            view: Some(view(100, 120)),
            ..Default::default()
        };
        pane.viewport.anchor = Some(anchor(105));
        layout(&mut pane, 4);
        pane.scroll(-3);
        assert_eq!(pane.history_query(), Some(Query::Older(100)));
        let previous = pane.viewport.anchor.clone();
        accept(&mut pane, view(80, 100), Query::Older(100), 20);
        layout(&mut pane, 4);
        assert_eq!(pane.viewport.anchor, previous);
        assert_eq!(pane.viewport.top, 22);
        assert_eq!(pane.view.as_ref().unwrap().entries.len(), 40);
        assert_eq!(pane.history_query(), Some(Query::Older(80)));
    }

    #[test]
    fn overscroll_moves_into_newly_loaded_rows() {
        let mut pane = Pane {
            view: Some(view(10, 20)),
            ..Default::default()
        };
        pane.viewport.anchor = Some(anchor(10));
        layout(&mut pane, 4);
        pane.scroll(-3);
        assert_eq!(pane.history.debt, -3);
        accept(&mut pane, view(0, 10), Query::Older(10), 10);
        layout(&mut pane, 4);
        assert_eq!(pane.viewport.anchor, Some(anchor(7)));
        assert_eq!(pane.viewport.top, 7);
        assert_eq!(pane.viewport.pending_scroll, 0);
    }

    #[test]
    fn newer_scroll_stitches_and_follows_only_at_real_tail() {
        let mut pane = Pane {
            view: Some(view(10, 20)),
            before: Some(20),
            ..Default::default()
        };
        pane.history.older_exhausted = true;
        pane.viewport.anchor = Some(anchor(16));
        layout(&mut pane, 4);
        pane.scroll(3);
        assert_eq!(pane.history_query(), Some(Query::Newer(19)));
        accept(&mut pane, view(10, 30), Query::Newer(19), 10);
        layout(&mut pane, 4);
        assert_eq!(pane.viewport.anchor, Some(anchor(19)));
        assert_eq!(pane.before, Some(30));
        pane.scroll(200);
        accept(&mut pane, view(20, 30), Query::Newer(29), 10);
        layout(&mut pane, 4);
        assert_eq!(pane.before, None);
        assert_eq!(pane.viewport.anchor, None);
        assert_eq!(pane.viewport.top, 16);
    }

    #[test]
    fn exhaustion_does_not_repeat_older_queries() {
        let mut pane = Pane {
            view: Some(view(1, 10)),
            ..Default::default()
        };
        pane.viewport.anchor = Some(anchor(1));
        layout(&mut pane, 4);
        pane.scroll(-3);
        accept(&mut pane, view(0, 0), Query::Older(1), 10);
        layout(&mut pane, 4);
        assert!(pane.history.older_exhausted);
        assert_eq!(pane.history_query(), None);
        assert_eq!(pane.viewport.pending_scroll, 0);
        assert_eq!(pane.viewport.anchor, Some(anchor(1)));
    }

    #[test]
    fn overlap_refresh_updates_entries_without_duplicates_or_losing_history() {
        let mut pane = Pane {
            view: Some(view(1, 10)),
            ..Default::default()
        };
        let mut updated = view(7, 13);
        updated.entries[0].text = "changed".into();
        accept(&mut pane, updated, Query::Newer(9), 10);
        let entries = &pane.view.as_ref().unwrap().entries;
        assert_eq!(entries.len(), 12);
        assert_eq!(entries[6].text, "changed");
        assert!(entries
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence));
    }

    #[test]
    fn long_bidirectional_walk_keeps_visible_anchor_and_full_history() {
        let mut pane = Pane {
            view: Some(view(1900, 2000)),
            ..Default::default()
        };
        for start in (0..19).rev().map(|page| page * 100) {
            let first = pane.view.as_ref().unwrap().entries[0].sequence;
            pane.viewport.anchor = Some(anchor(first + 10));
            layout(&mut pane, 20);
            let previous = pane.viewport.anchor.clone();
            accept(
                &mut pane,
                view(start, start + 100),
                Query::Older(first),
                100,
            );
            layout(&mut pane, 20);
            assert_eq!(pane.viewport.anchor, previous);
        }
        assert_eq!(pane.view.as_ref().unwrap().entries.len(), 2000);
        assert!(pane.before.is_none());
    }

    #[allow(dead_code)]
    fn _unused_newer_walk(mut pane: Pane) {
        for _ in 0..30 {
            let last = pane.view.as_ref().unwrap().entries.last().unwrap().sequence;
            if last == 1999 {
                break;
            }
            pane.viewport.anchor = Some(anchor(last - 30));
            layout(&mut pane, 20);
            let previous = pane.viewport.anchor.clone();
            accept(
                &mut pane,
                view(last + 1, (last + 101).min(2000)),
                Query::Newer(last),
                100,
            );
            layout(&mut pane, 20);
            assert_eq!(pane.viewport.anchor, previous);
        }
        assert_eq!(
            pane.view.as_ref().unwrap().entries.last().unwrap().sequence,
            1999
        );
        assert!(!pane.history.older_exhausted);
    }

    #[test]
    fn evicted_pane_requests_surrounding_anchor_not_old_window_tail() {
        let mut pane = Pane::default();
        pane.viewport.anchor = Some(anchor(123));
        pane.before = Some(900);
        pane.pinned_before = Some(900);
        assert_eq!(pane.refresh_query(), Query::Around(123));
        accept(&mut pane, view(24, 224), Query::Around(123), 100);
        layout(&mut pane, 20);
        assert_eq!(pane.viewport.anchor, Some(anchor(123)));
        assert_eq!(pane.viewport.top, 99);
        assert_eq!(pane.before, Some(224));
    }

    #[test]
    fn reversing_direction_does_not_apply_old_scroll_debt_to_new_page() {
        let mut pane = Pane {
            view: Some(view(10, 30)),
            ..Default::default()
        };
        pane.viewport.anchor = Some(anchor(10));
        layout(&mut pane, 4);
        pane.scroll(-3);
        pane.scroll(3);
        let previous = pane.viewport.anchor.clone();
        accept(&mut pane, view(0, 10), Query::Older(10), 10);
        layout(&mut pane, 4);
        assert_eq!(pane.viewport.anchor, previous);
        assert_eq!(pane.viewport.pending_scroll, 0);
    }

    #[test]
    fn stale_generation_and_runtime_replies_cannot_replace_current_window() {
        let mut app = App::new(10);
        let thread = ThreadId("root".into());
        app.select(thread.clone());
        let pane = app.pane_mut().unwrap();
        pane.generation = 4;
        pane.view = Some(view(10, 20));
        pane.history.pending = Some(Query::Older(10));
        app.reading = true;
        app.reading_request = Some((thread.clone(), 4));
        app.reply(Reply::View {
            thread: thread.clone(),
            generation: 3,
            result: Ok(view(0, 10)),
        });
        assert!(app.reading);
        assert_eq!(
            app.pane().unwrap().view.as_ref().unwrap().entries[0].sequence,
            10
        );
        app.runtime_epoch = 2;
        app.reply(Reply::Runtime {
            epoch: 1,
            reply: Box::new(Reply::View {
                thread,
                generation: 4,
                result: Ok(view(0, 10)),
            }),
        });
        assert!(app.reading);
        assert_eq!(
            app.pane().unwrap().view.as_ref().unwrap().entries[0].sequence,
            10
        );
    }

    #[test]
    fn invisible_pages_keep_query_progress_without_growing_retained_content() {
        let mut pane = Pane {
            view: Some(view(2000, 2001)),
            ..Default::default()
        };
        pane.viewport.anchor = Some(anchor(2000));
        layout(&mut pane, 4);
        pane.history.direction = Some(true);
        for start in (0..20).rev().map(|page| page * 100) {
            let first = pane.view.as_ref().unwrap().entries[0].sequence;
            let mut hidden = view(start, start + 100);
            for entry in &mut hidden.entries {
                entry.kind = EntryKind::Assistant;
                entry.text.clear();
            }
            accept(&mut pane, hidden, Query::Older(first), 100);
            assert_eq!(pane.history_query(), Some(Query::Older(start)));
            assert!(pane.view.as_ref().unwrap().entries.len() <= 3);
            assert_eq!(pane.viewport.anchor, Some(anchor(2000)));
        }
        accept(&mut pane, view(-1, 0), Query::Older(0), 100);
        assert!(pane
            .view
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.sequence == -1));
        assert!(pane
            .view
            .as_ref()
            .unwrap()
            .entries
            .iter()
            .any(|entry| entry.sequence == 2000));
    }

    #[test]
    fn pending_submission_pages_remain_valid_pagination_boundaries() {
        let mut pane = Pane::default();
        for start in [100, 0] {
            let mut page = view(start, start + 100);
            for entry in &mut page.entries {
                entry.kind = EntryKind::Submission;
                entry.submission = Some(bob_kernel::EntrySubmission {
                    id: bob_kernel::CommandId(format!("input-{}", entry.sequence)),
                    state: bob_kernel::SubmissionState::Pending,
                    ordinal: entry.sequence,
                    sequence: page.sequence,
                });
            }
            let query = if start == 100 {
                Query::Replace(None)
            } else {
                Query::Older(100)
            };
            accept(&mut pane, page, query, 100);
            pane.reconcile_submissions();
            assert!(crate::ui::render::transcript(&pane, false, 120).is_empty());
        }
        assert_eq!(
            pane.view
                .as_ref()
                .unwrap()
                .entries
                .first()
                .unwrap()
                .sequence,
            0
        );
        assert_eq!(
            pane.view.as_ref().unwrap().entries.last().unwrap().sequence,
            199
        );
        assert_eq!(pane.view.as_ref().unwrap().entries.len(), 200);
        pane.history.direction = Some(true);
        assert_eq!(pane.history_query(), Some(Query::Older(0)));
    }

    #[test]
    fn hidden_instruction_pages_keep_pagination_boundaries() {
        hidden_tool_pages_keep_pagination_boundaries("project_instructions");
    }

    #[test]
    fn hidden_todo_pages_keep_pagination_boundaries() {
        hidden_tool_pages_keep_pagination_boundaries("todo_write");
    }

    fn hidden_tool_pages_keep_pagination_boundaries(name: &str) {
        let mut pane = Pane {
            view: Some(view(2000, 2001)),
            ..Default::default()
        };
        pane.viewport.anchor = Some(anchor(2000));
        layout(&mut pane, 4);
        pane.history.direction = Some(true);
        for start in (0..20).rev().map(|page| page * 100) {
            let first = pane.view.as_ref().unwrap().entries[0].sequence;
            let mut hidden = tools(start, start + 100);
            for entry in &mut hidden.entries {
                let tool = entry.tool.as_mut().unwrap();
                tool.name = name.into();
                tool.presentation =
                    (name == "project_instructions").then(|| bob_kernel::ToolResultPresentation {
                        summary: Some("1 convention sources".into()),
                        body_preview: None,
                        exit_code: None,
                    });
            }
            accept(&mut pane, hidden, Query::Older(first), 100);
            assert_eq!(pane.history_query(), Some(Query::Older(start)));
            assert!(pane.view.as_ref().unwrap().entries.len() <= 3);
            assert_eq!(pane.viewport.anchor, Some(anchor(2000)));
            for root in [true, false] {
                assert!(crate::ui::render::transcript(&pane, root, 120)
                    .iter()
                    .all(
                        |row| !row.line.to_string().contains(if name == "todo_write" {
                            "Plan"
                        } else {
                            name
                        })
                    ));
            }
        }
    }

    fn tools(start: i64, end: i64) -> View {
        let mut view = view(start, end);
        for entry in &mut view.entries {
            let operation = OperationId(format!("tool-{}", entry.sequence));
            entry.kind = EntryKind::ToolCall;
            entry.operation = Some(operation.clone());
            entry.tool = Some(bob_kernel::ToolSummary {
                id: operation,
                name: "read_file".into(),
                state: bob_kernel::OperationState::Done,
                input_preview: serde_json::json!({"path": "file.rs"}),
                result: None,
                presentation: None,
            });
        }
        view
    }

    #[test]
    fn exploration_group_seam_preserves_summary_anchor_and_expansion() {
        let mut pane = Pane {
            view: Some(tools(100, 110)),
            ..Default::default()
        };
        pane.expanded.insert(OperationId("tool-100".into()));
        pane.viewport.anchor = Some(anchor(100));
        accept(&mut pane, tools(90, 100), Query::Older(100), 10);
        assert_eq!(pane.viewport.anchor, Some(anchor(90)));
        assert!(pane.expanded.contains(&OperationId("tool-90".into())));
    }

    #[test]
    fn inactive_pane_cache_remains_bounded_and_retains_only_anchor_for_evictions() {
        let mut app = App::new(100);
        for index in 0..12 {
            app.select(ThreadId(format!("thread-{index}")));
            let pane = app.pane_mut().unwrap();
            pane.view = Some(view(0, 600));
            pane.viewport.anchor = Some(anchor(10));
            layout(pane, 20);
        }
        assert_eq!(
            app.panes
                .values()
                .filter(|pane| pane.view.is_some())
                .count(),
            8
        );
        for pane in app.panes.values().filter(|pane| pane.view.is_none()) {
            assert_eq!(pane.viewport.anchor, Some(anchor(10)));
            assert!(pane.viewport.rows.is_empty());
            assert!(pane.history.pending.is_none());
            assert_eq!(pane.refresh_query(), Query::Around(10));
        }
    }

    #[test]
    fn stitched_final_response_preserves_live_identity_anchor() {
        let mut app = App::new(10);
        let thread = ThreadId("root".into());
        let operation = OperationId("live".into());
        app.select(thread.clone());
        let pane = app.pane_mut().unwrap();
        pane.view = Some(view(1, 10));
        pane.history.pending = Some(Query::Newer(9));
        pane.viewport.anchor = Some(Anchor {
            identity: AnchorId::Live {
                instance: "runtime".into(),
                operation: operation.clone(),
            },
            line: 2,
            offset: 4,
        });
        let mut final_view = view(10, 11);
        final_view.entries[0].kind = EntryKind::Assistant;
        final_view.entries[0].operation = Some(operation.clone());
        app.reply(Reply::View {
            thread,
            generation: 0,
            result: Ok(final_view),
        });
        let pane = app.pane().unwrap();
        assert_eq!(
            pane.viewport.anchor,
            Some(Anchor {
                identity: AnchorId::Body(10),
                line: 2,
                offset: 4
            })
        );
        assert!(pane.committed_operations.contains(&operation));
        assert_eq!(pane.view.as_ref().unwrap().entries.len(), 10);
    }
}
