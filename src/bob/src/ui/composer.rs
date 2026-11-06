use super::{
    state::Draft,
    style::{safe, wrap},
};
use ratatui::text::Line;
use std::sync::Arc;

pub(super) const INPUT_LIMIT: usize = 64 * 1024;
pub(super) const LIMIT_NOTICE: &str = "Input rejected: 64 KiB limit; existing draft preserved.";

pub(super) struct Layout {
    revision: u64,
    cursor: usize,
    width: usize,
    pub rows: Vec<Line<'static>>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

pub(super) fn layout(draft: &mut Draft, width: usize) -> Arc<Layout> {
    let width = width.max(1);
    if let Some(layout) = &draft.layout {
        if (layout.revision, layout.cursor, layout.width) == (draft.revision, draft.cursor, width) {
            return layout.clone();
        }
    }
    let clean_draft = safe(&draft.text);
    let clean_cursor = safe(&draft.text[..draft.cursor]).len();
    let mut rows = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_col = 0;
    let mut start = 0;
    for logical in clean_draft.split('\n') {
        let mut wrapped = wrap(&Line::raw(logical.to_owned()), width);
        if wrapped
            .last()
            .is_some_and(|(_, line)| line.width() == width)
        {
            wrapped.push((logical.len(), Line::default()));
        }
        for (i, (offset, line)) in wrapped.iter().enumerate() {
            let end = wrapped
                .get(i + 1)
                .map_or(logical.len() + 1, |(offset, _)| *offset);
            let local = clean_cursor.saturating_sub(start);
            if clean_cursor >= start && local >= *offset && local < end && local <= logical.len() {
                cursor_row = rows.len();
                cursor_col = unicode_width::UnicodeWidthStr::width(&logical[*offset..local]);
            }
            rows.push(line.clone());
        }
        start += logical.len() + 1;
    }
    let layout = Arc::new(Layout {
        revision: draft.revision,
        cursor: draft.cursor,
        width,
        rows,
        cursor_row,
        cursor_col,
    });
    draft.layout = Some(layout.clone());
    #[cfg(test)]
    {
        draft.layout_builds += 1;
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::state::{App, Focus};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use tokio::task::JoinSet;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn emacs_word_motion_and_deletion_preserve_unicode() {
        let mut draft = Draft::default();
        draft.insert("αβ café 界");
        draft.key(key(KeyCode::Left, KeyModifiers::CONTROL));
        assert_eq!(&draft.text[draft.cursor..], "界");
        draft.key(key(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(draft.text, "αβ 界");
        draft.key(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        draft.key(key(KeyCode::Char('d'), KeyModifiers::ALT));
        assert_eq!(draft.text, " 界");
        draft.key(key(KeyCode::Char('f'), KeyModifiers::CONTROL));
        draft.key(key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(draft.text, " ");
    }

    #[test]
    fn multiline_paste_is_compact_but_resolves_and_survives_history() {
        let mut draft = Draft::default();
        draft.insert("before ");
        draft.paste("α\nβ");
        assert_eq!(draft.text, "before [Pasted text #1 +2 lines]");
        assert_eq!(draft.resolved_text().as_deref(), Some("before α\nβ"));
        draft.clear();
        draft.insert("unfinished");
        draft.key(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(draft.resolved_text().as_deref(), Some("before α\nβ"));
        draft.key(key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(draft.text, "unfinished");
    }

    #[test]
    fn paste_limit_is_atomic_and_expansion_is_bounded() {
        let mut draft = Draft::default();
        draft.insert("keep");
        let revision = draft.revision;
        draft.paste(&"x".repeat(INPUT_LIMIT));
        assert_eq!(draft.text, "keep");
        assert_eq!(draft.revision, revision);
        assert!(draft.limit_reached);
        draft.clear();
        draft.paste(&"x".repeat(INPUT_LIMIT / 2 + 1));
        let marker = draft.text.clone();
        let revision = draft.revision;
        let cursor = draft.cursor;
        draft.insert(&marker);
        assert!(draft.limit_reached);
        assert_eq!(draft.text, marker);
        assert_eq!(draft.revision, revision);
        assert_eq!(draft.cursor, cursor);
        assert_eq!(draft.resolved_text(), Some("x".repeat(INPUT_LIMIT / 2 + 1)));
    }

    #[test]
    fn native_backspace_encodings_do_not_insert_control_characters() {
        let mut draft = Draft::default();
        draft.insert("界ab");
        draft.key(key(KeyCode::Char('\u{8}'), KeyModifiers::NONE));
        draft.key(key(KeyCode::Char('\u{7f}'), KeyModifiers::NONE));
        draft.key(key(KeyCode::Char('\u{1b}'), KeyModifiers::NONE));
        assert_eq!(draft.text, "界");
    }

    #[test]
    fn sidebar_toggle_keeps_composer_and_control_p_n_are_history() {
        let mut app = App::new(80);
        let mut jobs = JoinSet::new();
        app.draft_mut().insert("previous");
        app.draft_mut().clear();
        app.draft_mut().insert("draft");
        app.key(key(KeyCode::Char('t'), KeyModifiers::CONTROL), &mut jobs);
        assert!(app.sidebar);
        assert_eq!(app.focus, Focus::Composer);
        app.key(key(KeyCode::Char('P'), KeyModifiers::CONTROL), &mut jobs);
        assert_eq!(app.draft_mut().text, "previous");
        app.key(key(KeyCode::Char('n'), KeyModifiers::CONTROL), &mut jobs);
        assert_eq!(app.draft_mut().text, "draft");
    }

    #[test]
    fn selected_slash_enter_executes_selection_not_prefix() {
        let mut app = App::new(80);
        let mut jobs = JoinSet::new();
        app.draft_mut().insert("/e");
        app.refresh_completion();
        assert!(app.key(key(KeyCode::Enter, KeyModifiers::NONE), &mut jobs));
        assert_eq!(app.draft_mut().text, "/exit");
    }

    #[test]
    fn invalidated_permission_does_not_accept_slash_completion() {
        let mut app = App::new(80);
        let mut jobs = JoinSet::new();
        app.draft_mut().insert("/e");
        app.refresh_completion();
        app.focus = Focus::PermissionInvalidated;
        assert!(!app.key(key(KeyCode::Enter, KeyModifiers::NONE), &mut jobs));
        assert_eq!(app.draft_mut().text, "/e");
    }
}
