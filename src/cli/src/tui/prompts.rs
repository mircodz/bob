use super::{App, PendingQuery, QueryPurpose};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ActivePrompt {
    Workflow,
    Permission,
    Query,
    Composer,
}

#[derive(Default)]
pub(super) struct QueryScroll {
    pub(super) offset: Option<usize>,
    pub(super) follow_start: usize,
    pub(super) page_size: usize,
    pub(super) max_offset: usize,
}

impl QueryScroll {
    pub(super) fn page(&mut self, down: bool) {
        let start = self.offset.unwrap_or(self.follow_start);
        let step = self.page_size.saturating_sub(1).max(1);
        self.offset = Some(if down {
            start.saturating_add(step).min(self.max_offset)
        } else {
            start.saturating_sub(step)
        });
    }
}

impl App {
    pub(super) fn open_model_picker(&mut self, options: Vec<String>) {
        let selected = options
            .iter()
            .position(|label| label.split('\t').next() == Some(self.model_label.as_str()))
            .unwrap_or(0);
        self.enqueue_query(PendingQuery {
            query: bob_core::tools::registry::UserQuery {
                title: format!("Select a model (current: {})", self.model_label),
                detail: String::new(),
                options,
                allow_other: true,
            },
            list: super::widgets::SelectList {
                selected,
                scroll: 0,
            },
            other_text: None,
            inspection: Default::default(),
            purpose: QueryPurpose::ModelPicker,
        });
    }

    pub(super) fn active_prompt(&self) -> ActivePrompt {
        if self.workflow_view.is_some() {
            ActivePrompt::Workflow
        } else if !self.perm_queue.is_empty() {
            ActivePrompt::Permission
        } else if self.pending_query.is_some() {
            ActivePrompt::Query
        } else {
            ActivePrompt::Composer
        }
    }

    pub(super) fn enqueue_query(&mut self, query: PendingQuery) {
        if self.pending_query.is_none() {
            self.pending_query = Some(query);
        } else {
            self.query_queue.push_back(query);
        }
    }

    pub(super) fn advance_queries(&mut self) {
        if self.pending_query.is_none() {
            self.pending_query = self.query_queue.pop_front();
        }
    }

    pub(super) fn paste_active(&mut self, text: &str) {
        match self.active_prompt() {
            ActivePrompt::Query => {
                if let Some(q) = self.pending_query.as_mut() {
                    if let Some(buffer) = q.other_text.as_mut() {
                        buffer.extend(text.chars().filter(|ch| !ch.is_control() || *ch == '\n'));
                        q.inspection.offset = None;
                    }
                }
            }
            ActivePrompt::Composer => {
                self.input.paste(text);
                self.refresh_menu();
            }
            ActivePrompt::Permission | ActivePrompt::Workflow => {}
        }
    }

    pub(super) fn clear_closed_prompts(&mut self) -> bool {
        let before = (
            self.perm_queue.len(),
            self.query_queue.len(),
            self.pending_query.is_some(),
        );
        self.perm_queue.retain(|prompt| !prompt.resp.is_closed());
        let live =
            |q: &PendingQuery| !matches!(&q.purpose, QueryPurpose::Tool(resp) if resp.is_closed());
        self.query_queue.retain(live);
        if self.pending_query.as_ref().is_some_and(|q| !live(q)) {
            self.pending_query = None;
        }
        self.advance_queries();
        before
            != (
                self.perm_queue.len(),
                self.query_queue.len(),
                self.pending_query.is_some(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::{
        team::WorkflowView, test_support::test_app, widgets::SelectList, PendingPerm,
    };
    use bob_core::tools::registry::UserQuery;
    use tokio::sync::oneshot;

    fn tool_query(title: &str) -> (PendingQuery, oneshot::Receiver<Option<String>>) {
        let (tx, rx) = oneshot::channel();
        (
            PendingQuery {
                query: UserQuery {
                    title: title.into(),
                    detail: String::new(),
                    options: vec!["first".into(), "second".into()],
                    allow_other: true,
                },
                list: SelectList::new(),
                other_text: None,
                inspection: Default::default(),
                purpose: QueryPurpose::Tool(tx),
            },
            rx,
        )
    }

    fn permission() -> (PendingPerm, oneshot::Receiver<Option<usize>>) {
        let (resp, rx) = oneshot::channel();
        (
            PendingPerm {
                title: "permission".into(),
                detail: String::new(),
                preview: None,
                options: Vec::new(),
                list: SelectList::new(),
                resp,
            },
            rx,
        )
    }

    #[test]
    fn prompt_priority_routes_paste_without_corrupting_inactive_buffers() {
        let mut app = test_app();
        app.input.set("draft");
        assert_eq!(app.active_prompt(), ActivePrompt::Composer);
        let (mut query, _query_rx) = tool_query("question");
        query.other_text = Some("answer: ".into());
        app.enqueue_query(query);
        let (perm, _perm_rx) = permission();
        app.perm_queue.push_back(perm);
        app.workflow_view = Some(WorkflowView::default());
        assert_eq!(app.active_prompt(), ActivePrompt::Workflow);
        app.paste_active("ignored");
        app.workflow_view = None;
        assert_eq!(app.active_prompt(), ActivePrompt::Permission);
        app.paste_active("ignored");
        app.perm_queue.clear();
        assert_eq!(app.active_prompt(), ActivePrompt::Query);
        assert_eq!(
            app.pending_query.as_ref().unwrap().other_text.as_deref(),
            Some("answer: ")
        );
        app.paste_active("é\t界\r\n\0done");
        assert_eq!(
            app.pending_query.as_ref().unwrap().other_text.as_deref(),
            Some("answer: é界\ndone")
        );
        assert_eq!(app.input.text(), "draft");
        app.pending_query.as_mut().unwrap().other_text = None;
        app.paste_active("must not select an option");
        assert!(app.pending_query.as_ref().unwrap().other_text.is_none());
        assert_eq!(app.pending_query.as_ref().unwrap().list.selected, 0);
        app.pending_query = None;
        app.paste_active("界");
        assert_eq!(app.input.text(), "draft界");
    }

    #[test]
    fn queries_are_fifo_and_advance_never_replaces_a_live_prompt() {
        let mut app = test_app();
        let (first, _first_rx) = tool_query("one");
        let (second, _second_rx) = tool_query("two");
        let (third, _third_rx) = tool_query("three");
        app.enqueue_query(first);
        app.enqueue_query(second);
        app.enqueue_query(third);
        app.advance_queries();
        assert_eq!(app.pending_query.as_ref().unwrap().query.title, "one");
        assert_eq!(app.query_queue.len(), 2);
        for (title, remaining) in [("two", 1), ("three", 0)] {
            app.pending_query = None;
            app.advance_queries();
            assert_eq!(app.pending_query.as_ref().unwrap().query.title, title);
            assert_eq!(app.query_queue.len(), remaining);
        }
        app.pending_query = None;
        app.advance_queries();
        assert!(app.pending_query.is_none());
    }

    #[test]
    fn closed_channels_are_pruned_before_advancing_to_the_next_live_question() {
        let mut app = test_app();
        let (first, first_rx) = tool_query("closed active");
        let (second, second_rx) = tool_query("closed queued");
        let (third, _third_rx) = tool_query("live queued");
        app.enqueue_query(first);
        app.enqueue_query(second);
        app.enqueue_query(third);
        let (closed_perm, closed_perm_rx) = permission();
        let (live_perm, _live_perm_rx) = permission();
        app.perm_queue.extend([closed_perm, live_perm]);
        assert!(!app.clear_closed_prompts());
        drop((first_rx, second_rx, closed_perm_rx));
        assert!(app.clear_closed_prompts());
        assert_eq!(
            app.pending_query.as_ref().unwrap().query.title,
            "live queued"
        );
        assert!(app.query_queue.is_empty());
        assert_eq!(app.perm_queue.len(), 1);
        assert!(!app.clear_closed_prompts());
        app.pending_query = None;
        app.open_model_picker(vec!["model".into()]);
        assert!(
            !app.clear_closed_prompts(),
            "UI pickers have no channel to close"
        );
        assert!(matches!(
            app.pending_query.as_ref().unwrap().purpose,
            QueryPurpose::ModelPicker
        ));
    }

    #[test]
    fn model_picker_selects_exact_id_not_prefix_or_metadata() {
        for (current, expected) in [("model-large", 1), ("model", 2), ("missing", 0)] {
            let mut app = test_app();
            app.model_label = current.into();
            let options = vec![
                "model-small\t128k ctx".into(),
                "model-large\t1M ctx".into(),
                "model".into(),
            ];
            app.open_model_picker(options.clone());
            let query = app.pending_query.as_ref().unwrap();
            assert_eq!(query.list.selected, expected, "{current}");
            assert_eq!(query.list.scroll, 0);
            assert_eq!(query.query.options, options);
            assert!(query.query.allow_other);
            assert!(query.other_text.is_none());
        }
    }
}
