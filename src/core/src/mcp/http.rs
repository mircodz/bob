//! Streamable-HTTP MCP transport: JSON-RPC 2.0 over HTTP POST. Each request is a
//! POST to the server URL; the response is either a single JSON object or an SSE
//! stream (`text/event-stream`) whose events carry the JSON-RPC reply. We track
//! the `Mcp-Session-Id` the server assigns on initialize and echo it back, and
//! attach the OAuth bearer token (refreshing it when it has expired).

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
            // Match the stdio transport's 30s ceiling: without a timeout a server
            // that accepts the POST but never emits its JSON-RPC reply (or holds an
            // SSE stream open) would hang the tool call — and the turn — forever, so
            // the tool_result is never appended and everything after it is lost.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            session_id: Mutex::new(None),
            next_id: AtomicI64::new(1),
        })
    }

    /// Fetch a valid bearer token if this server uses OAuth, else fall back to a
    /// stored static token (e.g. a PAT), else None.
    async fn token(&self) -> anyhow::Result<Option<String>> {
        match &self.oauth {
            Some(oauth) => Ok(Some(
                crate::auth::mcp::access_token(&self.server, oauth).await?,
            )),
            None => Ok(crate::auth::mcp::stored_token(&self.server)),
        }
    }

    /// POST a JSON-RPC message and return the raw HTTP response.
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

/// Extract the JSON-RPC reply matching `id` from an HTTP response that is either
/// a single JSON object or an SSE stream of events.
async fn read_reply(resp: reqwest::Response, id: i64) -> anyhow::Result<Value> {
    let is_sse = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);

    if !is_sse {
        let v: Value = resp.json().await?;
        return Ok(v);
    }

    // SSE: collect events, keep the one whose id matches (or the first with a
    // result/error if the server omits the id).
    let mut found: Option<Value> = None;
    crate::providers::sse::parse_sse(resp, |event| {
        if found.is_some() {
            return;
        }
        let matches_id = event.get("id").and_then(|v| v.as_i64()) == Some(id);
        let has_reply = event.get("result").is_some() || event.get("error").is_some();
        if matches_id || has_reply {
            found = Some(event);
        }
    })
    .await?;
    found.ok_or_else(|| anyhow::anyhow!("MCP HTTP stream ended without a reply"))
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
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

        // Capture the session id the server assigns (usually on initialize).
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
    }

    async fn notify(&self, method: &str, params: Value) {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        let _ = self.post(&msg).await;
    }
}
