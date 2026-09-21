use crossterm::event::KeyCode;

#[derive(Default)]
pub struct Input {
    buf: String,
    /// Cursor position as a char index.
    cursor: usize,
    history: Vec<String>,
    /// None while editing a fresh line.
    hist_idx: Option<usize>,
    /// Stashed in-progress line while browsing history.
    stash: String,
    /// Placeholder id → full text, expanded on submit.
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

    /// Return rows and cursor (row, display column), computed in one wrapping pass.
    pub fn wrapped(&self, width: usize) -> (Vec<String>, usize, usize) {
        use unicode_width::UnicodeWidthChar;
        let width = width.max(1);
        let chars: Vec<char> = self.buf.chars().collect();
        let mut rows: Vec<String> = vec![String::new()];
        let mut col = 0usize;
        let mut cur_row = 0usize;
        let mut cur_col = 0usize;
        let mut cursor_set = false;

        for (i, &ch) in chars.iter().enumerate() {
            let w = if ch == '\n' {
                0
            } else {
                ch.width().unwrap_or(0)
            };
            // Wrap before locating the cursor so it follows the glyph onto the new row.
            if ch != '\n' && col + w > width && col > 0 {
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
                col += w;
            }
        }

        if !cursor_set {
            if col >= width {
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

    /// Return the `@` char index and query up to the cursor, bounded by whitespace.
    pub fn at_token(&self) -> Option<(usize, String)> {
        let chars: Vec<char> = self.buf.chars().collect();
        if self.cursor > chars.len() {
            return None;
        }
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
    pub fn kill_to_end(&mut self) {
        let end = self.len_chars();
        self.delete_range(self.cursor, end);
    }
    pub fn kill_to_start(&mut self) {
        self.delete_range(0, self.cursor);
    }
    pub fn kill_word_left(&mut self) {
        let from = self.prev_word_boundary();
        self.delete_range(from, self.cursor);
    }
    pub fn kill_word_right(&mut self) {
        let to = self.next_word_boundary();
        self.delete_range(self.cursor, to);
    }
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
            // Conhost and legacy terminals may encode Backspace as BS or DEL.
            // Other controls must not enter the display buffer.
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
                self.hist_idx = None;
                let stash = self.stash.clone();
                self.set(&stash);
            }
        }
    }
}

fn placeholder(id: usize, content: &str) -> String {
    let lines = content.split('\n').count();
    format!("[Pasted text #{} +{} lines]", id, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_edits_preserve_suffix_and_character_cursor() {
        let mut input = Input::new();
        input.set("a界é🦀z");
        input.move_left();
        input.move_left();
        input.on_key(KeyCode::Char('λ'));
        assert_eq!((input.text(), input.cursor), ("a界éλ🦀z", 4));
        input.on_key(KeyCode::Backspace);
        assert_eq!((input.text(), input.cursor), ("a界é🦀z", 3));
        input.on_key(KeyCode::Delete);
        assert_eq!((input.text(), input.cursor), ("a界éz", 3));
        input.delete_left();
        input.delete_right();
        assert_eq!((input.text(), input.cursor), ("a界", 2));
        input.move_right();
        input.delete_right();
        assert_eq!((input.text(), input.cursor), ("a界", 2));
        input.move_home();
        input.move_left();
        input.delete_left();
        assert_eq!((input.text(), input.cursor), ("a界", 0));
    }

    #[test]
    fn word_motion_and_kills_respect_unicode_whitespace_and_keep_suffix() {
        let mut input = Input::new();
        input.set("éclair\u{2003}日本\tfin");
        input.move_word_left();
        assert_eq!(input.cursor, 10);
        input.kill_word_left();
        assert_eq!((input.text(), input.cursor), ("éclair\u{2003}fin", 7));
        input.move_home();
        input.move_word_right();
        assert_eq!(input.cursor, 6);
        input.kill_word_right();
        assert_eq!((input.text(), input.cursor), ("éclair", 6));
        input.move_left();
        input.kill_to_start();
        assert_eq!((input.text(), input.cursor), ("r", 0));
        input.kill_to_end();
        assert_eq!((input.text(), input.cursor), ("", 0));
    }

    #[test]
    fn transpose_handles_interior_end_and_short_buffers() {
        for (text, cursor, expected, expected_cursor) in [
            ("", 0, "", 0),
            ("界", 1, "界", 1),
            ("é界z", 0, "é界z", 0),
            ("é界z", 1, "界éz", 2),
            ("é界z", 3, "éz界", 3),
        ] {
            let mut input = Input::new();
            input.set(text);
            input.cursor = cursor;
            input.transpose();
            assert_eq!((input.text(), input.cursor), (expected, expected_cursor));
        }
    }

    #[test]
    fn control_key_encodings_delete_without_inserting_controls() {
        for key in [
            KeyCode::Backspace,
            KeyCode::Char('\u{8}'),
            KeyCode::Char('\u{7f}'),
        ] {
            let mut input = Input::new();
            input.set("é界x");
            input.move_left();
            input.on_key(key);
            assert_eq!((input.text(), input.cursor), ("éx", 1));
            for ch in ['\0', '\n', '\r', '\t', '\u{1b}', '\u{9d}'] {
                input.on_key(KeyCode::Char(ch));
                assert_eq!((input.text(), input.cursor), ("éx", 1));
            }
        }
    }

    #[test]
    fn completion_uses_character_offsets_and_preserves_text_after_cursor() {
        let mut input = Input::new();
        input.set("界 @srcé suffix");
        input.move_home();
        for _ in 0..7 {
            input.move_right();
        }
        assert_eq!(input.at_token(), Some((2, "srcé".into())));
        input.replace_at_token(2, "日本.rs");
        assert_eq!((input.text(), input.cursor), ("界 @日本.rs suffix", 8));
        input.move_right();
        assert_eq!(input.at_token(), None);
        input.move_home();
        assert_eq!(input.at_token(), None);
    }

    #[test]
    fn paste_threshold_counts_characters_and_expands_multiple_markers_in_place() {
        let mut input = Input::new();
        let short = "界".repeat(120);
        input.paste(&short);
        assert_eq!(input.text(), short);
        input.set("before  after");
        input.move_home();
        for _ in 0..7 {
            input.move_right();
        }
        let long = "é".repeat(121);
        input.paste(&long);
        input.paste("日本\n語\n");
        assert_eq!(
            input.text(),
            "before [Pasted text #1 +1 lines][Pasted text #2 +3 lines] after"
        );
        assert_eq!(
            input.resolved_text(),
            format!("before {long}日本\n語\n after")
        );
        input.submit();
        assert_eq!(input.text(), "");
        assert_eq!(input.cursor, 0);
        assert!(input.pastes.is_empty());
        input.paste("a\rb");
        assert_eq!(input.resolved_text(), "ab");
    }

    #[test]
    fn history_deduplicates_adjacent_entries_and_restores_unicode_draft() {
        let mut input = Input::new();
        for text in ["  first  ", "first", " \n ", "日本"] {
            input.set(text);
            input.submit();
        }
        assert_eq!(input.history, vec!["first", "日本"]);
        input.set("draft é");
        for expected in ["日本", "first", "first"] {
            input.history_prev();
            assert_eq!(input.text(), expected);
            assert_eq!(input.cursor, expected.chars().count());
        }
        for expected in ["日本", "draft é", "draft é"] {
            input.history_next();
            assert_eq!(input.text(), expected);
            assert_eq!(input.cursor, expected.chars().count());
        }
        input.history_prev();
        input.on_key(KeyCode::Char('!'));
        input.history_next();
        assert_eq!(input.text(), "日本!");
        assert_eq!(input.hist_idx, None);
    }

    #[test]
    fn wrapping_tracks_end_cursor_across_line_and_display_boundaries() {
        for (label, text, width, expected_rows, expected_cursor) in [
            ("short line", "hello", 20, vec!["hello"], (0, 5)),
            ("hard newline", "ab\ncd", 20, vec!["ab", "cd"], (1, 2)),
            (
                "exact soft wrap",
                "abcdef",
                3,
                vec!["abc", "def", ""],
                (2, 0),
            ),
            ("trailing newline", "a\n", 20, vec!["a", ""], (1, 0)),
            (
                "wide glyph wraps before edge",
                "a界b",
                2,
                vec!["a", "界", "b"],
                (2, 1),
            ),
            (
                "combining mark has no columns",
                "e\u{301}x",
                2,
                vec!["e\u{301}x", ""],
                (1, 0),
            ),
            ("empty input", "", 3, vec![""], (0, 0)),
            (
                "zero width clamps to one",
                "ab",
                0,
                vec!["a", "b", ""],
                (2, 0),
            ),
        ] {
            let mut input = Input::new();
            input.set(text);
            let (rows, row, column) = input.wrapped(width);
            assert_eq!(rows, expected_rows, "{label}: rows");
            assert_eq!((row, column), expected_cursor, "{label}: cursor");
        }
    }

    #[test]
    fn wrapping_tracks_interior_cursor_at_character_not_byte_offsets() {
        for (label, text, width, cursor, expected_rows, expected_cursor) in [
            (
                "inside soft row",
                "abcdefg",
                3,
                4,
                vec!["abc", "def", "g"],
                (1, 1),
            ),
            (
                "at soft boundary",
                "abcdef",
                3,
                3,
                vec!["abc", "def"],
                (1, 0),
            ),
            (
                "before wide glyph",
                "a界b",
                2,
                1,
                vec!["a", "界", "b"],
                (1, 0),
            ),
            (
                "after wide glyph",
                "a界b",
                2,
                2,
                vec!["a", "界", "b"],
                (2, 0),
            ),
            (
                "after combining mark",
                "e\u{301}xy",
                2,
                2,
                vec!["e\u{301}x", "y"],
                (0, 1),
            ),
            (
                "before hard newline",
                "ab\ncd",
                2,
                2,
                vec!["ab", "cd"],
                (0, 2),
            ),
        ] {
            let mut input = Input::new();
            input.set(text);
            input.move_home();
            for _ in 0..cursor {
                input.move_right();
            }
            let (rows, row, column) = input.wrapped(width);
            assert_eq!(rows, expected_rows, "{label}: rows");
            assert_eq!((row, column), expected_cursor, "{label}: cursor");
        }
    }
}
