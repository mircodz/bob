#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    pub(super) fn negotiate(value: Option<&str>) -> anyhow::Result<Self> {
        match value.unwrap_or("utf-16") {
            "utf-8" => Ok(Self::Utf8),
            "utf-16" => Ok(Self::Utf16),
            "utf-32" => Ok(Self::Utf32),
            other => anyhow::bail!("unsupported LSP position encoding: {}", other),
        }
    }

    pub fn offset(self, text: &str, line: usize, character: usize) -> Option<usize> {
        let (start, body) = line_body(text, line)?;
        let mut units = 0;
        for (byte, ch) in body.char_indices() {
            if units == character {
                return Some(start + byte);
            }
            units += self.units(ch);
            if units > character {
                return None;
            }
        }
        (units == character).then_some(start + body.len())
    }

    pub fn from_column(self, text: &str, line: usize, column: usize) -> Option<usize> {
        let (_, body) = line_body(text, line)?;
        if column > body.chars().count() {
            return None;
        }
        Some(body.chars().take(column).map(|ch| self.units(ch)).sum())
    }

    pub fn to_column(self, text: &str, line: usize, character: usize) -> Option<usize> {
        let (start, _) = line_body(text, line)?;
        let end = self.offset(text, line, character)?;
        Some(text[start..end].chars().count())
    }

    fn units(self, ch: char) -> usize {
        match self {
            Self::Utf8 => ch.len_utf8(),
            Self::Utf16 => ch.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

fn line_body(text: &str, line: usize) -> Option<(usize, &str)> {
    let mut start = 0;
    for _ in 0..line {
        start += text[start..].find('\n')? + 1;
    }
    let tail = &text[start..];
    let body = match tail.find('\n') {
        Some(end) => tail[..end].strip_suffix('\r').unwrap_or(&tail[..end]),
        None => tail,
    };
    Some((start, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_defaults_to_utf16_and_rejects_unknown_encodings() {
        assert_eq!(
            PositionEncoding::negotiate(None).unwrap(),
            PositionEncoding::Utf16
        );
        for (name, encoding) in [
            ("utf-8", PositionEncoding::Utf8),
            ("utf-16", PositionEncoding::Utf16),
            ("utf-32", PositionEncoding::Utf32),
        ] {
            assert_eq!(PositionEncoding::negotiate(Some(name)).unwrap(), encoding);
        }
        for name in ["", "UTF-16", "utf-7"] {
            assert!(PositionEncoding::negotiate(Some(name))
                .unwrap_err()
                .to_string()
                .contains("unsupported LSP position encoding"));
        }
    }

    #[test]
    fn every_unicode_boundary_converts_between_columns_units_and_bytes() {
        let text = "prefix\r\naé😀e\u{301}中\n";
        let bytes = [8, 9, 11, 15, 16, 18, 21];
        for (encoding, units) in [
            (PositionEncoding::Utf8, [0, 1, 3, 7, 8, 10, 13]),
            (PositionEncoding::Utf16, [0, 1, 2, 4, 5, 6, 7]),
            (PositionEncoding::Utf32, [0, 1, 2, 3, 4, 5, 6]),
        ] {
            for (column, (&character, &byte)) in units.iter().zip(&bytes).enumerate() {
                assert_eq!(
                    encoding.from_column(text, 1, column),
                    Some(character),
                    "{encoding:?} column {column}"
                );
                assert_eq!(encoding.to_column(text, 1, character), Some(column));
                assert_eq!(encoding.offset(text, 1, character), Some(byte));
            }
            for character in 0..=units[6] + 1 {
                if !units.contains(&character) {
                    assert_eq!(encoding.offset(text, 1, character), None);
                    assert_eq!(encoding.to_column(text, 1, character), None);
                }
            }
            assert_eq!(encoding.from_column(text, 1, 7), None);
            assert_eq!(encoding.from_column(text, 1, usize::MAX), None);
            assert_eq!(encoding.to_column(text, 1, usize::MAX), None);
        }
    }

    #[test]
    fn empty_lines_eof_and_crlf_have_distinct_boundaries() {
        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            for (text, line, byte) in [
                ("", 0, 0),
                ("\r\n\n", 0, 0),
                ("\r\n\n", 1, 2),
                ("\r\n\n", 2, 3),
            ] {
                assert_eq!(encoding.offset(text, line, 0), Some(byte));
                assert_eq!(encoding.offset(text, line, 1), None);
                assert_eq!(encoding.from_column(text, line, 0), Some(0));
                assert_eq!(encoding.to_column(text, line, 0), Some(0));
            }
            for text in ["", "x"] {
                assert_eq!(encoding.offset(text, 1, 0), None);
                assert_eq!(encoding.from_column(text, 1, 0), None);
                assert_eq!(encoding.to_column(text, 1, 0), None);
            }
            assert_eq!(encoding.offset("x\r\n", 0, 1), Some(1));
            assert_eq!(encoding.offset("x\r\n", 0, 2), None);
        }
    }

    #[test]
    fn strict_non_bmp_and_crlf_positions() {
        let text = "a😀z\r\nβ\n";
        assert_eq!(PositionEncoding::Utf16.offset(text, 0, 1), Some(1));
        assert_eq!(PositionEncoding::Utf16.offset(text, 0, 2), None);
        assert_eq!(PositionEncoding::Utf16.offset(text, 0, 3), Some(5));
        assert_eq!(PositionEncoding::Utf16.offset(text, 0, 4), Some(6));
        assert_eq!(PositionEncoding::Utf16.offset(text, 0, 5), None);
        assert_eq!(PositionEncoding::Utf16.offset(text, 1, 1), Some(10));
        assert_eq!(PositionEncoding::Utf16.offset(text, 2, 0), Some(11));
        assert_eq!(PositionEncoding::Utf16.offset(text, 3, 0), None);
        assert_eq!(PositionEncoding::Utf8.offset(text, 0, 2), None);
        assert_eq!(PositionEncoding::Utf8.offset(text, 0, 5), Some(5));
        assert_eq!(PositionEncoding::Utf32.offset(text, 0, 2), Some(5));
        assert_eq!(PositionEncoding::Utf16.from_column(text, 0, 2), Some(3));
        assert_eq!(PositionEncoding::Utf16.to_column(text, 0, 3), Some(2));
    }
}
