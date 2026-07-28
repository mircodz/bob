//! Minimal SSE line parser over a byte stream. Yields the JSON payload of each
//! `data:` line as a parsed `serde_json::Value`.

use futures::StreamExt;
use serde_json::Value;

/// Drive a reqwest byte stream, invoking `on_event` for each parsed SSE payload.
pub async fn parse_sse<F>(
    resp: reqwest::Response,
    mut on_event: F,
) -> anyhow::Result<()>
where
    F: FnMut(Value),
{
    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Split on blank lines between events.
        loop {
            let Some(idx) = buffer.find("\n\n") else { break };
            let raw = buffer[..idx].to_string();
            buffer.drain(..idx + 2);

            for line in raw.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                if payload == "[DONE]" {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(payload) {
                    on_event(value);
                }
            }
        }
    }
    Ok(())
}
