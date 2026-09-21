//! Streamable HTTP uses JSON-RPC POSTs with JSON or SSE replies.
//! Echo the server's `Mcp-Session-Id` on subsequent messages.

use super::Transport;
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
                .timeout(std::time::Duration::from_secs(30))
                .build()?,
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
        })
    }

    /// OAuth takes precedence over stored static tokens; refresh errors propagate.
    async fn token(&self) -> anyhow::Result<Option<String>> {
        match &self.oauth {
            Some(oauth) => Ok(Some(
                crate::auth::mcp::access_token(&self.server, oauth).await?,
            )),
            None => Ok(crate::auth::mcp::stored_token(&self.server)),
        }
    }

    async fn post(&self, msg: &Value) -> anyhow::Result<reqwest::Response> {
        let mut req = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(sid) = self.session_id.lock().unwrap().clone() {
            req = req.header("mcp-session-id", sid);
        }
        if let Some(tok) = self.token().await? {
            req = req.header("authorization", format!("Bearer {}", tok));
        }
        let resp = req.json(msg).send().await?;
        Ok(resp)
    }
}

/// Match the request ID in either JSON or SSE; do not wait for SSE EOF after a reply.
async fn read_reply(resp: reqwest::Response, id: i64) -> anyhow::Result<Value> {
    let is_sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);

    if !is_sse {
        let v: Value = resp.json().await?;
        anyhow::ensure!(
            is_reply(&v, id),
            "MCP HTTP response did not match request {}",
            id
        );
        return Ok(v);
    }

    let mut found: Option<Value> = None;
    crate::providers::sse::parse_sse_control(
        resp,
        |event| {
            if let crate::providers::sse::SseEvent::Json(event) = event {
                if is_reply(&event, id) {
                    found = Some(event);
                    return std::ops::ControlFlow::Break(());
                }
            }
            std::ops::ControlFlow::Continue(())
        },
        std::time::Duration::from_secs(30),
    )
    .await?;
    found.ok_or_else(|| anyhow::anyhow!("MCP HTTP stream ended without a reply"))
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
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("MCP HTTP {} error: {} {}", self.server, status, body);
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
                anyhow::bail!("MCP error: {}", err);
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
