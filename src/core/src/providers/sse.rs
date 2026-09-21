//! SSE framing shared by providers and HTTP integrations.

use futures::StreamExt;
use serde_json::Value;
use std::ops::ControlFlow;
use std::time::Duration;

#[derive(Debug, PartialEq)]
pub enum SseEvent {
    Json(Value),
    Done,
}

#[derive(Debug, PartialEq)]
pub enum SseEnd {
    Eof,
    Stopped,
}

pub async fn parse_sse<F>(resp: reqwest::Response, mut on_event: F) -> anyhow::Result<()>
where
    F: FnMut(Value),
{
    parse_sse_control(
        resp,
        |event| {
            match event {
                SseEvent::Json(value) => on_event(value),
                SseEvent::Done => return ControlFlow::Break(()),
            }
            ControlFlow::Continue(())
        },
        Duration::from_secs(120),
    )
    .await?;
    Ok(())
}

pub async fn parse_sse_control<F>(
    resp: reqwest::Response,
    mut on_event: F,
    idle: Duration,
) -> anyhow::Result<SseEnd>
where
    F: FnMut(SseEvent) -> ControlFlow<()>,
{
    let mut stream = resp.bytes_stream();
    let mut decoder = Decoder::default();
    loop {
        let chunk = tokio::time::timeout(idle, stream.next())
            .await
            .map_err(|_| anyhow::anyhow!("SSE idle deadline exceeded"))?;
        let Some(chunk) = chunk else {
            decoder.finish()?;
            return Ok(SseEnd::Eof);
        };
        if decoder.push(&chunk?, &mut on_event)?.is_break() {
            return Ok(SseEnd::Stopped);
        }
    }
}

#[derive(Default)]
struct Decoder {
    line: Vec<u8>,
    data: String,
    has_data: bool,
    after_cr: bool,
    started: bool,
}

impl Decoder {
    fn push(
        &mut self,
        bytes: &[u8],
        on_event: &mut impl FnMut(SseEvent) -> ControlFlow<()>,
    ) -> anyhow::Result<ControlFlow<()>> {
        for &byte in bytes {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.after_cr = byte == b'\r';
                if self.line(on_event)?.is_break() {
                    return Ok(ControlFlow::Break(()));
                }
            } else {
                self.line.push(byte);
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn line(
        &mut self,
        on_event: &mut impl FnMut(SseEvent) -> ControlFlow<()>,
    ) -> anyhow::Result<ControlFlow<()>> {
        let bytes = std::mem::take(&mut self.line);
        let mut line = std::str::from_utf8(&bytes)
            .map_err(|error| anyhow::anyhow!("invalid SSE UTF-8: {error}"))?;
        if !self.started {
            self.started = true;
            line = line.strip_prefix('\u{feff}').unwrap_or(line);
        }
        if line.is_empty() {
            if !self.has_data {
                return Ok(ControlFlow::Continue(()));
            }
            self.has_data = false;
            let mut data = std::mem::take(&mut self.data);
            data.pop();
            let event = if data == "[DONE]" {
                SseEvent::Done
            } else {
                SseEvent::Json(
                    serde_json::from_str(&data)
                        .map_err(|error| anyhow::anyhow!("malformed SSE JSON: {error}"))?,
                )
            };
            return Ok(on_event(event));
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        if field == "data" {
            self.has_data = true;
            self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
            self.data.push('\n');
        }
        Ok(ControlFlow::Continue(()))
    }

    fn finish(&self) -> anyhow::Result<()> {
        if self.has_data || !self.line.is_empty() {
            anyhow::bail!("SSE stream ended in an incomplete frame");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn framing_survives_every_byte_boundary() {
        let wire = "\u{feff}:keepalive\r\nevent: result\r\ndata: {\"text\":\r\ndata: \"日本語 😀\"}\r\n\r\ndata: [DONE]\r\n\r\n";
        for split in 0..=wire.len() {
            let mut decoder = Decoder::default();
            let mut events = Vec::new();
            let mut receive = |event| {
                events.push(event);
                ControlFlow::Continue(())
            };
            assert!(decoder
                .push(&wire.as_bytes()[..split], &mut receive)
                .unwrap()
                .is_continue());
            assert!(decoder
                .push(&wire.as_bytes()[split..], &mut receive)
                .unwrap()
                .is_continue());
            decoder.finish().unwrap();
            assert_eq!(
                events,
                vec![SseEvent::Json(json!({"text":"日本語 😀"})), SseEvent::Done]
            );
        }
    }

    #[test]
    fn carriage_return_and_multiple_frames() {
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        assert!(decoder
            .push(b"data:{\"a\":1}\r\rdata:{\"b\":2}\n\n", &mut |event| {
                events.push(event);
                ControlFlow::Continue(())
            })
            .unwrap()
            .is_continue());
        decoder.finish().unwrap();
        assert_eq!(
            events,
            vec![
                SseEvent::Json(json!({"a":1})),
                SseEvent::Json(json!({"b":2}))
            ]
        );
    }

    #[test]
    fn malformed_json_utf8_and_unterminated_frames_fail() {
        for bytes in [b"data: {broken}\n\n".as_slice(), b"data: \xff\n\n"] {
            assert!(Decoder::default()
                .push(bytes, &mut |_| ControlFlow::Continue(()))
                .is_err());
        }
        for bytes in [b"data: {}".as_slice(), b"data: {}\n"] {
            let mut decoder = Decoder::default();
            assert!(decoder
                .push(bytes, &mut |_| ControlFlow::Continue(()))
                .unwrap()
                .is_continue());
            assert!(decoder.finish().is_err());
        }
    }

    #[test]
    fn callback_break_does_not_parse_later_garbage() {
        let result = Decoder::default().push(b"data: {}\n\ndata: broken\n\n", &mut |_| {
            ControlFlow::Break(())
        });
        assert!(result.unwrap().is_break());
    }

    #[test]
    fn bytewise_frames_ignore_metadata_without_losing_data_or_unicode() {
        for newline in ["\n", "\r", "\r\n"] {
            let wire = [
                "\u{feff}: ping",
                "event: message",
                "id: 7",
                "retry: 100",
                "unknown: ignored",
                "",
                "data: {\"text\":",
                ": mid-frame comment",
                "data: \"é日本😀\",",
                "data: \"n\": 2}",
                "",
                "data: [DONE]",
                "",
                "",
            ]
            .join(newline);
            let mut decoder = Decoder::default();
            let mut events = Vec::new();
            for byte in wire.as_bytes() {
                assert!(decoder
                    .push(&[*byte], &mut |event| {
                        events.push(event);
                        ControlFlow::Continue(())
                    })
                    .unwrap()
                    .is_continue());
            }
            decoder.finish().unwrap();
            assert_eq!(
                events,
                vec![
                    SseEvent::Json(json!({"text":"é日本😀","n":2})),
                    SseEvent::Done,
                ],
                "newline {newline:?}"
            );
        }
    }

    #[test]
    fn incomplete_frames_are_not_dispatched_at_eof() {
        for wire in ["data: {}", "data: {}\n", "data: {\"text\":\"日本語\"}\r\n"] {
            for split in 0..=wire.len() {
                let mut decoder = Decoder::default();
                let mut events = Vec::new();
                let mut receive = |event| {
                    events.push(event);
                    ControlFlow::Continue(())
                };
                for chunk in [&wire.as_bytes()[..split], &wire.as_bytes()[split..]] {
                    assert!(decoder.push(chunk, &mut receive).unwrap().is_continue());
                }
                assert_eq!(
                    decoder.finish().unwrap_err().to_string(),
                    "SSE stream ended in an incomplete frame"
                );
                assert!(events.is_empty(), "{wire:?}, split {split}");
            }
        }
    }

    #[test]
    fn malformed_frame_fails_at_every_split_without_emitting_it() {
        for (wire, expected_error) in [
            (b"data: {broken}\n\n".as_slice(), "malformed SSE JSON"),
            (b"data: \xff\n\n".as_slice(), "invalid SSE UTF-8"),
            (b"data:\n\n".as_slice(), "malformed SSE JSON"),
            (b"data: {} trailing\n\n".as_slice(), "malformed SSE JSON"),
        ] {
            for split in 0..=wire.len() {
                let mut decoder = Decoder::default();
                let mut events = Vec::new();
                let mut receive = |event| {
                    events.push(event);
                    ControlFlow::Continue(())
                };
                let result = decoder
                    .push(&wire[..split], &mut receive)
                    .and_then(|_| decoder.push(&wire[split..], &mut receive));
                assert!(
                    result.unwrap_err().to_string().contains(expected_error),
                    "split {split}"
                );
                assert!(events.is_empty());
            }
        }
    }

    #[test]
    fn done_stops_before_a_second_terminal_or_malformed_frame() {
        let mut events = Vec::new();
        let result = Decoder::default()
            .push(
                b"data: {\"text\":\"ok\"}\n\ndata: [DONE]\n\ndata: [DONE]\n\ndata: broken\n\n",
                &mut |event| {
                    let stop = event == SseEvent::Done;
                    events.push(event);
                    if stop {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                },
            )
            .unwrap();
        assert!(result.is_break());
        assert_eq!(
            events,
            vec![SseEvent::Json(json!({"text":"ok"})), SseEvent::Done]
        );
    }
}
