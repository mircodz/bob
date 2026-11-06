use super::*;
use bob_ui::widgets::{eligible_selection, scroll_offset, SelectionMove};

impl WorkflowUi {
    fn move_selection(&mut self, movement: SelectionMove) {
        let Some(cache) = &self.rendered else { return };
        let eligible: Vec<_> = cache
            .actions
            .iter()
            .enumerate()
            .filter_map(|(index, action)| (*action != Action::None).then_some(index))
            .collect();
        if let Some(selected) = eligible_selection(Some(self.selected), &eligible, movement) {
            self.selected = selected;
            if !matches!(self.route, Some(Route::Artifact { .. })) {
                self.top = bob_ui::widgets::keep_visible(
                    self.top,
                    selected,
                    self.height,
                    cache.actions.len(),
                );
            }
        }
        self.visual_changed();
    }

    pub(super) fn scroll(&mut self, delta: isize) {
        let Some(cache) = &self.rendered else { return };
        self.top = scroll_offset(self.top, delta, cache.scroll_len(), self.height);
        self.visual_changed();
    }

    pub(super) fn previous_page(&self) -> bool {
        let Some(previous) = self.stack.back() else {
            return false;
        };
        match (self.route.as_ref(), &previous.route) {
            (
                Some(Route::Workflow {
                    operation,
                    container,
                    cursor,
                }),
                Route::Workflow {
                    operation: old_operation,
                    container: old_container,
                    cursor: old_cursor,
                },
            ) => operation == old_operation && container == old_container && cursor != old_cursor,
            (
                Some(Route::Node {
                    operation,
                    path,
                    iterations: true,
                    cursor,
                }),
                Route::Node {
                    operation: old_operation,
                    path: old_path,
                    iterations: true,
                    cursor: old_cursor,
                },
            ) => operation == old_operation && path == old_path && cursor != old_cursor,
            (
                Some(Route::Artifact {
                    operation,
                    source,
                    offset,
                }),
                Route::Artifact {
                    operation: old_operation,
                    source: old_source,
                    offset: old_offset,
                },
            ) => operation == old_operation && source == old_source && offset != old_offset,
            _ => false,
        }
    }
}

impl App {
    pub(super) fn workflow_navigation_key(
        &mut self,
        code: KeyCode,
        jobs: &mut JoinSet<Reply>,
    ) -> bool {
        presentation::prepare(&mut self.workflows, None);
        let text_view = matches!(self.workflows.route, Some(Route::Artifact { .. }));
        let height = self.workflows.height.max(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') if text_view => self.workflows.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') if text_view => self.workflows.scroll(1),
            KeyCode::Up | KeyCode::Char('k') => {
                self.workflows.move_selection(SelectionMove::Previous)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.workflows.move_selection(SelectionMove::Next)
            }
            KeyCode::PageUp | KeyCode::PageDown => {
                let delta = if code == KeyCode::PageUp {
                    -(height as isize)
                } else {
                    height as isize
                };
                self.workflows.scroll(delta);
                if !text_view {
                    let top = self.workflows.top;
                    let cache = self.workflows.rendered.as_ref().unwrap();
                    let range = top..top.saturating_add(height);
                    let eligible: Vec<_> = cache
                        .actions
                        .iter()
                        .enumerate()
                        .filter_map(|(index, action)| {
                            (range.contains(&index) && *action != Action::None).then_some(index)
                        })
                        .collect();
                    if let Some(index) = eligible_selection(
                        Some(self.workflows.selected),
                        &eligible,
                        SelectionMove::Nearest,
                    ) {
                        self.workflows.selected = index;
                    }
                }
            }
            KeyCode::Home if text_view => self.workflows.scroll(isize::MIN),
            KeyCode::End if text_view => self.workflows.scroll(isize::MAX),
            KeyCode::Home => self.workflows.move_selection(SelectionMove::First),
            KeyCode::End => {
                self.workflows.move_selection(SelectionMove::Last);
                self.workflows.scroll(isize::MAX);
            }
            KeyCode::Left | KeyCode::Right if text_view => {
                self.workflows.move_selection(if code == KeyCode::Left {
                    SelectionMove::Previous
                } else {
                    SelectionMove::Next
                });
            }
            KeyCode::Left | KeyCode::Right => {
                let cache = self.workflows.rendered.as_ref().unwrap();
                let selected = self.workflows.selected;
                let path = cache.expansions.get(selected).cloned().flatten();
                let expanded = path
                    .as_ref()
                    .is_some_and(|path| self.workflows.expanded.contains(path));
                if (code == KeyCode::Right && !expanded) || (code == KeyCode::Left && expanded) {
                    if let Some(path) = path {
                        self.workflow_action(Action::Expand(path), jobs);
                        return true;
                    }
                }
                let next = if code == KeyCode::Right && expanded {
                    cache
                        .parents
                        .iter()
                        .enumerate()
                        .find_map(|(index, parent)| {
                            (*parent == Some(selected) && cache.actions[index] != Action::None)
                                .then_some(index)
                        })
                } else if code == KeyCode::Left {
                    cache.parents.get(selected).copied().flatten()
                } else {
                    None
                };
                if let Some(next) = next {
                    self.workflows.selected = next;
                    self.workflows.top = bob_ui::widgets::keep_visible(
                        self.workflows.top,
                        next,
                        height,
                        cache.actions.len(),
                    );
                    self.workflows.visual_changed();
                }
            }
            KeyCode::Enter => {
                let action = self
                    .workflows
                    .rendered
                    .as_ref()
                    .and_then(|cache| cache.actions.get(self.workflows.selected))
                    .cloned();
                if let Some(action) = action {
                    self.workflow_action(action, jobs);
                }
            }
            _ => return false,
        }
        true
    }
}
