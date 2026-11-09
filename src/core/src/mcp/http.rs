//! Streamable HTTP uses JSON-RPC POSTs with JSON or SSE replies.
//! Echo the server's `Mcp-Session-Id` on subsequent messages.

use super::{Transport, MAX_FRAME_BYTES};
use crate::core::config::{McpOAuthConfig, McpServerConfig};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;

pub(crate) struct HttpTransport {
    url: String,
    server: String,
    oauth: Option<McpOAuthConfig>,
    client: reqwest::Client,
    session_id: Mutex<Option<String>>,
    next_id: AtomicI64,
}

impl HttpTransport {
    pub(crate) async fn connect(cfg: &McpServerConfig) -> anyhow::Result<HttpTransport> {
        let url = cfg
            .url
            .clone()
            .ok_or_else(|| anyhow::anyhow!("HTTP MCP server '{}' has no url", cfg.name))?;
        Ok(HttpTransport {
            url,
            server: cfg.name.clone(),
            oauth: cfg.oauth.clone(),
            // Bound the full response lifetime, including SSE streams that never close.
            client: reqwest::Client::builder()
                .user_agent(crate::USER_AGENT)
                .timeout(std::time::Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
        })
    }

    /// OAuth takes precedence over stored static tokens; refresh errors propagate.
    async fn token(&self) -> anyhow::Result<Option<String>> {
        match &self.oauth {
            Some(oauth) => Ok(Some(
                crate::auth::mcp::access_token(&self.server, oauth)
                    .await
                    .map_err(|_| anyhow::anyhow!("MCP authorization failed; check server login"))?,
            )),
            None => Ok(crate::auth::mcp::stored_token(&self.server)),
        }
    }

    async fn post(&self, msg: &Value) -> anyhow::Result<reqwest::Response> {
        let token = self.token().await?;
        let request = self.build_request(msg, token.as_deref())?;
        self.client
            .execute(request)
            .await
            .map_err(|_| anyhow::anyhow!("MCP HTTP request failed"))
    }

    fn build_request(&self, msg: &Value, token: Option<&str>) -> anyhow::Result<reqwest::Request> {
        let mut req = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(sid) = self.session_id.lock().unwrap().clone() {
            req = req.header("mcp-session-id", sid);
        }
        if let Some(token) = token {
            let mut authorization =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| anyhow::anyhow!("MCP authorization token is invalid"))?;
            authorization.set_sensitive(true);
            req = req.header("authorization", authorization);
        }
        let body = serde_json::to_vec(msg)?;
        anyhow::ensure!(
            body.len() <= MAX_FRAME_BYTES,
            "MCP request exceeds byte limit"
        );
        req.body(body)
            .build()
            .map_err(|_| anyhow::anyhow!("MCP HTTP request configuration is invalid"))
    }
}

/// Match the request ID in either JSON or SSE; do not wait for SSE EOF after a reply.
async fn read_reply(mut resp: reqwest::Response, id: i64) -> anyhow::Result<Value> {
    let is_sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);

    anyhow::ensure!(
        resp.content_length().unwrap_or(0) <= MAX_FRAME_BYTES as u64,
        "MCP HTTP response exceeds byte limit"
    );
    let mut decoder = ReplyDecoder::new(is_sse, id);
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("MCP HTTP response interrupted"))?
    {
        if let Some(reply) = decoder.push(&chunk)? {
            return Ok(reply);
        }
    }
    decoder.finish()
}

struct ReplyDecoder {
    sse: bool,
    id: i64,
    received: usize,
    buffer: Vec<u8>,
    event: Vec<u8>,
}

impl ReplyDecoder {
    fn new(sse: bool, id: i64) -> Self {
        Self {
            sse,
            id,
            received: 0,
            buffer: Vec::new(),
            event: Vec::new(),
        }
    }

    fn push(&mut self, chunk: &[u8]) -> anyhow::Result<Option<Value>> {
        anyhow::ensure!(
            chunk.len() <= MAX_FRAME_BYTES - self.received,
            "MCP HTTP response exceeds byte limit"
        );
        self.received += chunk.len();
        self.buffer.extend_from_slice(chunk);
        if !self.sse {
            return Ok(None);
        }
        let mut consumed = 0;
        for line in self.buffer.split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") {
                break;
            }
            consumed += line.len();
            if let Some(reply) = Self::line(&mut self.event, &line[..line.len() - 1], self.id)? {
                return Ok(Some(reply));
            }
        }
        self.buffer.drain(..consumed);
        Ok(None)
    }

    fn line(event: &mut Vec<u8>, line: &[u8], id: i64) -> anyhow::Result<Option<Value>> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if event.is_empty() {
                return Ok(None);
            }
            let parsed = serde_json::from_slice::<Value>(event)
                .map_err(|_| anyhow::anyhow!("MCP SSE event contains invalid JSON"))?;
            event.clear();
            return Ok(is_reply(&parsed, id).then_some(parsed));
        }
        if let Some(data) = line.strip_prefix(b"data:") {
            let data = data.strip_prefix(b" ").unwrap_or(data);
            if !event.is_empty() {
                event.push(b'\n');
            }
            event.extend_from_slice(data);
        }
        Ok(None)
    }

    fn finish(mut self) -> anyhow::Result<Value> {
        if !self.sse {
            let reply: Value = serde_json::from_slice(&self.buffer)
                .map_err(|_| anyhow::anyhow!("MCP HTTP response contains invalid JSON"))?;
            anyhow::ensure!(
                is_reply(&reply, self.id),
                "MCP HTTP response did not match request {}",
                self.id
            );
            return Ok(reply);
        }
        if let Some(reply) = Self::line(&mut self.event, &self.buffer, self.id)? {
            return Ok(reply);
        }
        Self::line(&mut self.event, b"", self.id)?
            .ok_or_else(|| anyhow::anyhow!("MCP HTTP stream ended without a reply"))
    }
}

fn is_reply(value: &Value, id: i64) -> bool {
    value["id"].as_i64() == Some(id)
        && value.get("method").is_none()
        && (value.get("result").is_some() || value.get("error").is_some())
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let msg = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params
            });

            let resp = self.post(&msg).await?;

            if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
                anyhow::bail!(
                    "MCP server '{}' requires authorization; run `bob mcp login {}`",
                    self.server,
                    self.server
                );
            }
            if !resp.status().is_success() {
                anyhow::bail!("MCP HTTP error: {}", resp.status());
            }

            if let Some(sid) = resp
                .headers()
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
            {
                *self.session_id.lock().unwrap() = Some(sid);
            }

            let reply = read_reply(resp, id).await?;
            if let Some(err) = reply.get("error") {
                anyhow::bail!(
                    "MCP JSON-RPC error (code {})",
                    err["code"].as_i64().unwrap_or(0)
                );
            }
            Ok(reply.get("result").cloned().unwrap_or(Value::Null))
        })
        .await
        .map_err(|_| anyhow::anyhow!("MCP request '{}' timed out", method))?
    }

    async fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), self.post(&msg)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::http::response;
    use std::time::Duration;

    #[tokio::test]
    async fn auth_and_session_headers_are_bound_without_network_or_secret_debug_output() {
        let config =
            serde_json::from_value(json!({"name":"fixture","url":"http://127.0.0.1:1/mcp"}))
                .unwrap();
        let transport = HttpTransport::connect(&config).await.unwrap();
        *transport.session_id.lock().unwrap() = Some("fixture-session".into());
        let request = transport
            .build_request(&json!({"id":1}), Some("fixture-secret"))
            .unwrap();
        assert_eq!(request.headers()["authorization"], "Bearer fixture-secret");
        assert!(request.headers()["authorization"].is_sensitive());
        assert_eq!(request.headers()["mcp-session-id"], "fixture-session");
        assert_eq!(
            request.headers()["accept"],
            "application/json, text/event-stream"
        );
        assert!(!format!("{request:?}").contains("fixture-secret"));
        let no_auth = transport.build_request(&json!({}), None).unwrap();
        assert!(!no_auth.headers().contains_key("authorization"));
        assert!(transport
            .build_request(&json!({}), Some("bad\nsecret"))
            .is_err());
    }

    #[test]
    fn decoder_handles_every_utf8_chunk_boundary_multiline_and_crlf() {
        let stream = ": keepalive\r\ndata: {\"id\":2,\"result\":null}\r\n\r\ndata: {\"id\":1,\r\ndata: \"result\":\"é😀\"}\r\n\r\n".as_bytes();
        for split in 0..stream.len() {
            let mut decoder = ReplyDecoder::new(true, 1);
            assert!(decoder.push(&stream[..split]).unwrap().is_none());
            assert_eq!(
                decoder.push(&stream[split..]).unwrap().unwrap()["result"],
                "é😀"
            );
        }
        let mut decoder = ReplyDecoder::new(false, 1);
        for byte in br#"{"id":1,"result":null}"# {
            assert!(decoder.push(&[*byte]).unwrap().is_none());
        }
        assert_eq!(decoder.finish().unwrap()["result"], Value::Null);
    }

    #[test]
    fn decoder_rejects_oversized_json_sse_frames_and_endless_notices() {
        for sse in [false, true] {
            let mut decoder = ReplyDecoder::new(sse, 1);
            decoder.push(&vec![b' '; MAX_FRAME_BYTES]).unwrap();
            assert!(decoder
                .push(b"x")
                .unwrap_err()
                .to_string()
                .contains("byte limit"));
        }
        let mut decoder = ReplyDecoder::new(true, 1);
        let comment = vec![b':'; 1023];
        let mut line = comment;
        line.push(b'\n');
        for _ in 0..MAX_FRAME_BYTES / line.len() {
            assert!(decoder.push(&line).unwrap().is_none());
        }
        assert!(decoder.push(&line).is_err());
    }

    #[test]
    fn decoder_eof_and_parse_errors_do_not_echo_body() {
        let mut decoder = ReplyDecoder::new(true, 1);
        decoder.push(b"data: {\"id\":1,\"result\":true}").unwrap();
        assert_eq!(decoder.finish().unwrap()["result"], true);
        for sse in [false, true] {
            let mut decoder = ReplyDecoder::new(sse, 1);
            let malformed = if sse {
                b"data: Bearer secret\n\n".as_slice()
            } else {
                b"Bearer secret".as_slice()
            };
            let error = match decoder.push(malformed) {
                Err(error) => error,
                Ok(_) => decoder.finish().unwrap_err(),
            };
            assert!(!error.to_string().contains("secret"));
        }
    }

    #[test]
    fn reply_matching_requires_exact_integer_id_and_a_response_member() {
        for result in [
            Value::Null,
            json!(false),
            json!(0),
            json!([]),
            json!({"text":"ok"}),
        ] {
            assert!(is_reply(
                &json!({"jsonrpc":"2.0","id":7,"result":result}),
                7
            ));
        }
        assert!(is_reply(
            &json!({"id":7,"error":{"code":-32601,"message":"unknown method"}}),
            7
        ));
        for value in [
            Value::Null,
            json!([]),
            json!({"id":7}),
            json!({"id":8,"result":null}),
            json!({"id":"7","result":null}),
            json!({"id":7.5,"result":null}),
            json!({"id":null,"result":null}),
            json!({"result":null}),
            json!({"id":7,"method":"server/request","result":null}),
            json!({"id":7,"method":"server/request","error":{"code":-1}}),
        ] {
            assert!(!is_reply(&value, 7), "unexpected reply: {value}");
        }
    }

    #[tokio::test]
    async fn matching_sse_reply_returns_while_stream_stays_open() {
        let body = "data: {\"id\":9,\"result\":\"wrong\"}\n\ndata: {\"result\":\"missing\"}\n\ndata: {\"method\":\"notice\"}\n\ndata: {\"id\":7,\"result\":\"right\"}\n\n";
        let (response, _release) = response("text/event-stream", body, true).await;
        let reply = tokio::time::timeout(Duration::from_secs(1), read_reply(response, 7))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["result"], "right");
    }

    #[tokio::test]
    async fn json_requires_exact_id_and_response_shape() {
        for body in [
            r#"{"id":8,"result":1}"#,
            r#"{"result":1}"#,
            r#"{"id":7,"method":"request"}"#,
        ] {
            let (response, _) = response("application/json", body, false).await;
            assert!(read_reply(response, 7).await.is_err());
        }
        let (response, _) =
            response("application/json", r#"{"id":7,"error":{"code":-1}}"#, false).await;
        assert!(read_reply(response, 7)
            .await
            .unwrap()
            .get("error")
            .is_some());
    }

    #[tokio::test]
    async fn sse_eof_without_matching_reply_is_error() {
        let (response, _) = response(
            "text/event-stream",
            "data: {\"id\":8,\"result\":1}\n\n",
            false,
        )
        .await;
        assert!(read_reply(response, 7)
            .await
            .unwrap_err()
            .to_string()
            .contains("without a reply"));
    }
}
