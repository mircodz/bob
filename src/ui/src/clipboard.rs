//! Native clipboard transfer, or an explicitly unconfirmed terminal request.

use std::io::{self, Write};

#[cfg(windows)]
mod windows;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyOutcome {
    NativeCopied,
    TerminalRequestSent,
}

impl CopyOutcome {
    pub fn notice(self) -> &'static str {
        match self {
            Self::NativeCopied => "Copied last message to clipboard.",
            Self::TerminalRequestSent => {
                "Clipboard request sent to terminal; acceptance is unconfirmed."
            }
        }
    }
}

pub fn copy(text: &str) -> io::Result<CopyOutcome> {
    #[cfg(windows)]
    {
        windows::copy(text)
    }
    #[cfg(not(windows))]
    {
        osc52_copy(text)
    }
}

/// Explicit terminal/remote route; never used as a silent native fallback.
pub fn osc52_copy(text: &str) -> io::Result<CopyOutcome> {
    osc52_copy_to(text, &mut io::stdout().lock())
}

fn osc52_copy_to(text: &str, output: &mut impl Write) -> io::Result<CopyOutcome> {
    let b64 = base64_encode(text.as_bytes());
    write!(output, "\x1b]52;c;{b64}\x07")?;
    output.flush()?;
    Ok(CopyOutcome::TerminalRequestSent)
}

#[cfg(any(windows, test))]
fn unicode_text(text: &str) -> io::Result<Vec<u16>> {
    if text.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Clipboard text contains an interior NUL",
        ));
    }
    Ok(text.encode_utf16().chain(Some(0)).collect())
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_request_is_unconfirmed_and_propagates_write_and_flush_errors() {
        let mut bytes = Vec::new();
        assert_eq!(
            osc52_copy_to("é", &mut bytes).unwrap(),
            CopyOutcome::TerminalRequestSent
        );
        assert_eq!(bytes, b"\x1b]52;c;w6k=\x07");
        struct Failure(bool);
        impl Write for Failure {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.0 {
                    Err(io::Error::other("write"))
                } else {
                    Ok(bytes.len())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("flush"))
            }
        }
        assert_eq!(
            osc52_copy_to("text", &mut Failure(true))
                .unwrap_err()
                .to_string(),
            "write"
        );
        assert_eq!(
            osc52_copy_to("text", &mut Failure(false))
                .unwrap_err()
                .to_string(),
            "flush"
        );
    }

    #[test]
    fn native_text_rejects_nul_and_preserves_surrogate_pairs_and_empty_text() {
        assert!(unicode_text("before\0after").is_err());
        assert_eq!(unicode_text("").unwrap(), vec![0]);
        assert_eq!(
            unicode_text("é🦀").unwrap(),
            vec![0x00e9, 0xd83e, 0xdd80, 0]
        );
    }

    #[test]
    fn standard_base64_vectors_cover_padding_and_binary_bytes() {
        for (input, expected) in [
            (b"".as_slice(), ""),
            (b"f".as_slice(), "Zg=="),
            (b"fo".as_slice(), "Zm8="),
            (b"foo".as_slice(), "Zm9v"),
            (b"foob".as_slice(), "Zm9vYg=="),
            (b"fooba".as_slice(), "Zm9vYmE="),
            (b"foobar".as_slice(), "Zm9vYmFy"),
            (b"\x00\xfb\xff".as_slice(), "APv/"),
            (b"\xfb\xff\xff".as_slice(), "+///"),
            ("é".as_bytes(), "w6k="),
        ] {
            assert_eq!(base64_encode(input), expected, "{input:?}");
        }
    }
}
