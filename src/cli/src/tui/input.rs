//! A growing multi-line text input with emacs editing, prompt history, and
//! Claude-style collapsed paste placeholders.

use crossterm::event::KeyCode;

#[derive(Default)]
pub struct Input {
    buf: String,
    /// Cursor position as a char index.
    cursor: usize,
    history: Vec<String>,
    /// Cursor into history when browsing (None = editing a fresh line).
    hist_idx: Option<usize>,
    /// Stashed in-progress line while browsing history.
    stash: String,
    /// Large pastes are collapsed to a placeholder in `buf`; the full text is
    /// stored here keyed by id, and re-expanded at submit time.
    pastes: Vec<(usize, String)>,
    paste_seq: usize,
}

impl Input {
    pub fn new() -> Self {
        Input::default()
    }

    pub fn text(&self) -> &str {
        &self.buf
    }

    /// Wrap the buffer to `width` columns, returning the visual rows (content
    /// only, without any prompt prefix) and the cursor's (row, col) within them.
    /// Wrapping happens on hard newlines AND at the width boundary, char-based —
    /// crucially, the cursor is computed in the SAME pass, so the rendered text
    /// and the cursor can never disagree (the old split-brain wrap/cursor bug).
    pub fn wrapped(&self, width: usize) -> (Vec<String>, usize, usize) {
        let width = width.max(1);
        let chars: Vec<char> = self.buf.chars().collect();
        let mut rows: Vec<String> = vec![String::new()];
        let mut col = 0usize;
        let mut cur_row = 0usize;
        let mut cur_col = 0usize;
        let mut cursor_set = false;

        for (i, &ch) in chars.iter().enumerate() {
            // Soft-wrap BEFORE placing (or locating the cursor at) this char, so a
            // char at the boundary — and a cursor sitting before it — both land on
            // the fresh row rather than at an off-screen column == width.
            if ch != '\n' && col == width {
                rows.push(String::new());
                col = 0;
            }
            if i == self.cursor {
                cur_row = rows.len() - 1;
                cur_col = col;
                cursor_set = true;
            }
            if ch == '\n' {
                rows.push(String::new());
                col = 0;
            } else {
                rows.last_mut().unwrap().push(ch);
                col += 1;
            }
        }

        // Cursor at the very end of the buffer.
        if !cursor_set {
            if col == width {
                rows.push(String::new());
                cur_row = rows.len() - 1;
                cur_col = 0;
            } else {
                cur_row = rows.len() - 1;
                cur_col = col;
            }
        }
        (rows, cur_row, cur_col)
    }

    /// The text to actually send: buffer with paste placeholders expanded.
    pub fn resolved_text(&self) -> String {
        let mut out = self.buf.clone();
        for (id, content) in &self.pastes {
            let marker = placeholder(*id, content);
            if out.contains(&marker) {
                out = out.replace(&marker, content);
            }
        }
        out
    }

    /// If the cursor sits inside an `@…` token (an `@` with no whitespace
    /// between it and the cursor), return `(start_char, query)` where
    /// `start_char` is the char index of the `@` and `query` is the text after
    /// it up to the cursor. Used to drive `@file` completion.
    pub fn at_token(&self) -> Option<(usize, String)> {
        let chars: Vec<char> = self.buf.chars().collect();
        if self.cursor > chars.len() {
            return None;
        }
        // Scan left from the cursor for a whitespace boundary or an '@'.
        let mut i = self.cursor;
        while i > 0 {
            let c = chars[i - 1];
            if c == '@' {
                let query: String = chars[i..self.cursor].iter().collect();
                return Some((i - 1, query));
            }
            if c.is_whitespace() {
                return None;
            }
            i -= 1;
        }
        None
    }

    /// Replace the `@…` token starting at char index `start` (through the
    /// cursor) with `@replacement`, leaving the cursor just after it.
    pub fn replace_at_token(&mut self, start: usize, replacement: &str) {
        let chars: Vec<char> = self.buf.chars().collect();
        let before: String = chars[..start].iter().collect();
        let after: String = chars[self.cursor..].iter().collect();
        let inserted = format!("@{replacement}");
        self.buf = format!("{before}{inserted}{after}");
        self.cursor = start + inserted.chars().count();
    }

    pub fn set(&mut self, text: &str) {
        self.buf = text.to_string();
        self.cursor = self.buf.chars().count();
    }

    /// Handle a bracketed paste. Small single-line pastes go in inline; large or
    /// multi-line pastes are collapsed to a `[Pasted text #N +L lines]` marker.
    pub fn paste(&mut self, text: &str) {
        let lines = text.split('\n').count();
        if lines <= 1 && text.chars().count() <= 120 {
            for ch in text.chars() {
                if ch != '\r' {
                    self.insert(ch);
                }
            }
            return;
        }
        self.paste_seq += 1;
        let id = self.paste_seq;
        let content = text.to_string();
        let marker = placeholder(id, &content);
        self.pastes.push((id, content));
        for ch in marker.chars() {
            self.insert(ch);
        }
    }

    pub fn insert_newline(&mut self) {
        self.hist_idx = None;
        self.insert('\n');
    }

    fn insert(&mut self, ch: char) {
        let byte = self.byte_at(self.cursor);
        self.buf.insert(byte, ch);
        self.cursor += 1;
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.buf
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.buf.len())
    }

    fn len_chars(&self) -> usize {
        self.buf.chars().count()
    }

    // --- cursor movement ---
    pub fn move_home(&mut self) {
        self.cursor = 0;
    }
    pub fn move_end(&mut self) {
        self.cursor = self.len_chars();
    }
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }
    pub fn move_right(&mut self) {
        if self.cursor < self.len_chars() {
            self.cursor += 1;
        }
    }

    /// Index of the start of the word at/just before the cursor.
    fn prev_word_boundary(&self) -> usize {
        let chars: Vec<char> = self.buf.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// Index of the end of the word at/just after the cursor.
    fn next_word_boundary(&self) -> usize {
        let chars: Vec<char> = self.buf.chars().collect();
        let len = chars.len();
        let mut i = self.cursor;
        while i < len && chars[i].is_whitespace() {
            i += 1;
        }
        while i < len && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    pub fn move_word_left(&mut self) {
        self.cursor = self.prev_word_boundary();
    }
    pub fn move_word_right(&mut self) {
        self.cursor = self.next_word_boundary();
    }

    // --- deletion ---
    fn delete_range(&mut self, from: usize, to: usize) {
        let (a, b) = (from.min(to), from.max(to));
        let start = self.byte_at(a);
        let end = self.byte_at(b);
        self.buf.replace_range(start..end, "");
        if self.cursor > a {
            self.cursor = a + self.cursor.saturating_sub(b);
        }
        self.cursor = self.cursor.min(self.len_chars());
    }

    pub fn delete_left(&mut self) {
        if self.cursor > 0 {
            self.delete_range(self.cursor - 1, self.cursor);
        }
    }
    pub fn delete_right(&mut self) {
        if self.cursor < self.len_chars() {
            self.delete_range(self.cursor, self.cursor + 1);
        }
    }
    /// Ctrl+K: kill from cursor to end of line.
    pub fn kill_to_end(&mut self) {
        let end = self.len_chars();
        self.delete_range(self.cursor, end);
    }
    /// Ctrl+U: kill from start of line to cursor.
    pub fn kill_to_start(&mut self) {
        self.delete_range(0, self.cursor);
    }
    /// Ctrl+W / Alt+Backspace: kill the word before the cursor.
    pub fn kill_word_left(&mut self) {
        let from = self.prev_word_boundary();
        self.delete_range(from, self.cursor);
    }
    /// Alt+D: kill the word after the cursor.
    pub fn kill_word_right(&mut self) {
        let to = self.next_word_boundary();
        self.delete_range(self.cursor, to);
    }
    /// Ctrl+T: transpose the two characters around the cursor.
    pub fn transpose(&mut self) {
        let len = self.len_chars();
        if len < 2 {
            return;
        }
        // Emacs: at end of line, transpose the last two chars.
        let i = if self.cursor >= len {
            len - 1
        } else {
            self.cursor
        };
        if i == 0 {
            return;
        }
        let mut chars: Vec<char> = self.buf.chars().collect();
        chars.swap(i - 1, i);
        self.buf = chars.into_iter().collect();
        self.cursor = (i + 1).min(len);
    }

    /// Delete the character before the cursor (Backspace).
    fn backspace(&mut self) {
        if self.cursor > 0 {
            let start = self.byte_at(self.cursor - 1);
            let end = self.byte_at(self.cursor);
            self.buf.replace_range(start..end, "");
            self.cursor -= 1;
        }
    }

    pub fn on_key(&mut self, code: KeyCode) {
        match code {
            // Windows conhost (and terminals without the enhanced key protocol)
            // deliver Backspace/Delete as control chars (BS \u{8}, DEL \u{7f})
            // rather than KeyCode::Backspace/Delete. Route them to deletion and
            // drop any other control char so it can't be inserted as a blank/box
            // glyph (the "backspace prints spaces" bug).
            KeyCode::Char('\u{8}') | KeyCode::Char('\u{7f}') => self.backspace(),
            KeyCode::Char(ch) if ch.is_control() => {}
            KeyCode::Char(ch) => {
                self.hist_idx = None;
                self.insert(ch);
            }
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => {
                let len = self.buf.chars().count();
                if self.cursor < len {
                    let start = self.byte_at(self.cursor);
                    let end = self.byte_at(self.cursor + 1);
                    self.buf.replace_range(start..end, "");
                }
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                }
            }
            KeyCode::Right => {
                if self.cursor < self.buf.chars().count() {
                    self.cursor += 1;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.buf.chars().count(),
            _ => {}
        }
    }

    /// Commit the current line to history and clear.
    pub fn submit(&mut self) {
        let text = self.buf.trim().to_string();
        if !text.is_empty() && self.history.last() != Some(&text) {
            self.history.push(text);
        }
        self.buf.clear();
        self.cursor = 0;
        self.hist_idx = None;
        self.stash.clear();
        self.pastes.clear();
    }

    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.hist_idx {
            None => {
                self.stash = self.buf.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.hist_idx = Some(idx);
        self.set(&self.history[idx].clone());
    }

    pub fn history_next(&mut self) {
        match self.hist_idx {
            None => {}
            Some(i) if i + 1 < self.history.len() => {
                self.hist_idx = Some(i + 1);
                self.set(&self.history[i + 1].clone());
            }
            Some(_) => {
                // Past the end → restore the stashed in-progress line.
                self.hist_idx = None;
                let stash = self.stash.clone();
                self.set(&stash);
            }
        }
    }
}

/// The collapsed marker shown in the input for a large paste.
fn placeholder(id: usize, content: &str) -> String {
    let lines = content.split('\n').count();
    format!("[Pasted text #{} +{} lines]", id, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Input {
        let mut i = Input::new();
        i.set(s);
        i
    }

    #[test]
    fn short_line_no_wrap() {
        let i = typed("hello");
        let (rows, r, c) = i.wrapped(20);
        assert_eq!(rows, vec!["hello"]);
        assert_eq!((r, c), (0, 5)); // cursor at end
    }

    #[test]
    fn hard_newline_splits_rows() {
        let i = typed("ab\ncd");
        let (rows, r, c) = i.wrapped(20);
        assert_eq!(rows, vec!["ab", "cd"]);
        assert_eq!((r, c), (1, 2));
    }

    #[test]
    fn soft_wrap_at_width() {
        let i = typed("abcdef"); // width 3 → "abc" / "def"
        let (rows, r, c) = i.wrapped(3);
        // The cursor sits at the end of an exactly-full row, so a fresh empty row
        // is added for it to rest on (standard editor behavior).
        assert_eq!(rows, vec!["abc", "def", ""]);
        assert_eq!((r, c), (2, 0));
    }

    #[test]
    fn soft_wrap_cursor_not_at_end() {
        let mut i = typed("abcdefg"); // width 3 → abc/def/g
        i.move_home();
        for _ in 0..4 {
            i.move_right(); // cursor after 'd'
        }
        let (rows, r, c) = i.wrapped(3);
        assert_eq!(rows, vec!["abc", "def", "g"]);
        assert_eq!((r, c), (1, 1)); // after 'd' on row 1
    }

    #[test]
    fn cursor_mid_buffer_tracks_wrap() {
        let mut i = typed("abcdef");
        // Move cursor to just after 'c' (index 3): should be row 1 col 0 with a
        // width-3 wrap, since the boundary pushes to the next row.
        i.move_home();
        for _ in 0..3 {
            i.move_right();
        }
        let (_, r, c) = i.wrapped(3);
        assert_eq!((r, c), (1, 0));
    }

    #[test]
    fn trailing_newline_gives_empty_row() {
        let i = typed("a\n");
        let (rows, r, c) = i.wrapped(20);
        assert_eq!(rows, vec!["a", ""]);
        assert_eq!((r, c), (1, 0));
    }
}
