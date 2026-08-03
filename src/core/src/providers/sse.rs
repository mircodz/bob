//! Minimal SSE line parser over a byte stream. Yields the JSON payload of each
//! `data:` line as a parsed `serde_json::Value`.

use futures::StreamExt;
use serde_json::Value;

/// Drive a reqwest byte stream, invoking `on_event` for each parsed SSE payload.
///
/// Bytes are buffered raw and only decoded as UTF-8 once a complete line has
/// arrived. Decoding per network chunk (as a naive parser does) corrupts any
/// multibyte character — or base64 field such as a thinking `signature` or an
/// encrypted reasoning blob — that happens to be split across a TCP chunk
/// boundary. Buffering to the newline first makes every decoded line whole.
pub async fn parse_sse<F>(resp: reqwest::Response, mut on_event: F) -> anyhow::Result<()>
where
    F: FnMut(Value),
{
    let mut stream = resp.bytes_stream();
    let mut buffer: Vec<u8> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.extend_from_slice(&chunk);

        // Drain every complete line (terminated by `\n`), leaving any partial
        // trailing line in the buffer for the next chunk.
        while let Some(nl) = buffer.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buffer.drain(..=nl).collect();
            // The whole line is present now, so lossy decoding can't split a
            // multibyte sequence — the only place `_lossy` would matter is truly
            // invalid UTF-8, which shouldn't occur in a JSON SSE payload.
            let line = String::from_utf8_lossy(&line_bytes);
            handle_line(line.trim_end_matches(['\r', '\n']), &mut on_event);
        }
    }

    // Flush a final line that arrived without a trailing newline.
    if !buffer.is_empty() {
        let line = String::from_utf8_lossy(&buffer);
        handle_line(line.trim_end_matches(['\r', '\n']), &mut on_event);
    }
    Ok(())
}

/// Parse one SSE line: accept `data:` with or without the conventional trailing
/// space, skip the `[DONE]` sentinel and non-data lines, and forward valid JSON.
fn handle_line<F>(line: &str, on_event: &mut F)
where
    F: FnMut(Value),
{
    let Some(rest) = line.strip_prefix("data:") else {
        return;
    };
    let payload = rest.strip_prefix(' ').unwrap_or(rest);
    if payload.is_empty() || payload == "[DONE]" {
        return;
    }
    if let Ok(value) = serde_json::from_str::<Value>(payload) {
        on_event(value);
    }
}
