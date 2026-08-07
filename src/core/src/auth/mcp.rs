//! OAuth (authorization-code + PKCE) login for remote MCP servers. Mirrors the
//! Anthropic flow but is parameterized per server: the endpoints/client-id come
//! from the server's config, and tokens are stored under the key `mcp:<name>`
//! in the shared auth store so each remote server keeps its own credentials.

use super::{
    authorize_url, exchange_code, now, pkce, refresh_token, store_tokens, wait_for_callback,
    AuthCodeConfig, AuthStore, Pkce,
};
use crate::core::config::McpOAuthConfig;
use serde_json::{json, Value};

/// The auth-store key for a given MCP server's credentials.
fn store_key(server: &str) -> String {
    format!("mcp:{}", server)
}

/// The localhost port bob listens on for the OAuth redirect during MCP login.
const MCP_CALLBACK_PORT: u16 = 54546;

/// Discover a server's OAuth configuration by probing its URL and following the
/// MCP authorization spec: an unauthenticated request returns 401 with a
/// `WWW-Authenticate` header pointing at protected-resource metadata (RFC 9728),
/// which names the authorization server whose metadata (RFC 8414) gives the
/// authorize/token/registration endpoints. If no static client id is known we do
/// dynamic client registration (RFC 7591). Returns a ready-to-store config.
pub async fn discover(server: &str, url: &str) -> anyhow::Result<McpOAuthConfig> {
    let client = reqwest::Client::new();

    // 1. Probe: an unauthenticated JSON-RPC POST should 401 (or point us to
    //    metadata via WWW-Authenticate).
    let probe = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()
        .await?;

    // Find the protected-resource metadata URL: from WWW-Authenticate if present,
    // else the well-known path derived from the resource URL (RFC 9728 §3.1).
    let prm_url = probe
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_resource_metadata)
        .unwrap_or_else(|| well_known(url, "oauth-protected-resource"));

    // 2. Protected-resource metadata → authorization server(s).
    let prm: Value = client
        .get(&prm_url)
        .send()
        .await?
        .json()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to read protected-resource metadata at {}: {}",
                prm_url,
                e
            )
        })?;
    let as_base = prm["authorization_servers"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no authorization_servers in resource metadata"))?
        .to_string();

    // 3. Authorization-server metadata (RFC 8414).
    let asm_url = well_known(&as_base, "oauth-authorization-server");
    let asm: Value = client
        .get(&asm_url)
        .send()
        .await?
        .json()
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to read authorization-server metadata at {}: {}",
                asm_url,
                e
            )
        })?;
    let authorize_url = asm["authorization_endpoint"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no authorization_endpoint in AS metadata"))?
        .to_string();
    let token_url = asm["token_endpoint"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no token_endpoint in AS metadata"))?
        .to_string();
    let scope = prm["scopes_supported"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    // 4. Dynamic client registration (RFC 7591), if the AS advertises it.
    let redirect_uri = format!("http://localhost:{}/callback", MCP_CALLBACK_PORT);
    let client_id = if let Some(reg) = asm["registration_endpoint"].as_str() {
        let body = json!({
            "client_name": format!("bob ({})", server),
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "scope": scope,
        });
        let reg_resp = client.post(reg).json(&body).send().await?;
        if !reg_resp.status().is_success() {
            let s = reg_resp.status();
            let b = reg_resp.text().await.unwrap_or_default();
            anyhow::bail!("dynamic client registration failed ({}): {}", s, b);
        }
        let reg_json: Value = reg_resp.json().await?;
        reg_json["client_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("no client_id from registration"))?
            .to_string()
    } else {
        anyhow::bail!(
            "server '{}' does not support dynamic client registration; \
             pass --client-id (and --authorize-url/--token-url) manually",
            server
        );
    };

    Ok(McpOAuthConfig {
        authorize_url,
        token_url,
        client_id,
        scope,
        callback_port: MCP_CALLBACK_PORT,
    })
}

/// Parse `resource_metadata="URL"` out of a `WWW-Authenticate` header value.
fn parse_resource_metadata(header: &str) -> Option<String> {
    let idx = header.find("resource_metadata")?;
    let rest = &header[idx..];
    let eq = rest.find('=')?;
    let after = rest[eq + 1..].trim_start();
    let after = after.strip_prefix('"').unwrap_or(after);
    let end = after.find('"').unwrap_or(after.len());
    Some(after[..end].to_string())
}

/// Build a `.well-known` metadata URL from a base URL, inserting the well-known
/// segment between the authority and any path component (RFC 8414 §3).
fn well_known(base: &str, kind: &str) -> String {
    // Split scheme://authority from the path.
    let (scheme_authority, path) = match base.find("://") {
        Some(i) => {
            let after = &base[i + 3..];
            match after.find('/') {
                Some(p) => (&base[..i + 3 + p], &after[p..]),
                None => (base, ""),
            }
        }
        None => (base, ""),
    };
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        format!("{}/.well-known/{}", scheme_authority, kind)
    } else {
        format!("{}/.well-known/{}{}", scheme_authority, kind, path)
    }
}

fn auth_config(cfg: &McpOAuthConfig) -> AuthCodeConfig {
    AuthCodeConfig {
        client_id: cfg.client_id.clone(),
        authorize_url: cfg.authorize_url.clone(),
        token_url: cfg.token_url.clone(),
        scope: cfg.scope.clone(),
        redirect_uri: format!("http://localhost:{}/callback", cfg.callback_port),
        callback_port: cfg.callback_port,
        extra_authorize_params: vec![],
    }
}

/// The in-flight login: the URL to open in a browser plus the PKCE/state to
/// complete with once the redirect arrives.
pub struct LoginHandle {
    pub url: String,
    pkce: Pkce,
    state: String,
    cfg: AuthCodeConfig,
    server: String,
}

/// Step 1: build the authorize URL. Caller opens it in a browser.
pub fn begin_login(server: &str, oauth: &McpOAuthConfig) -> LoginHandle {
    let cfg = auth_config(oauth);
    let pkce = pkce();
    let state = super::base64_url(&std::process::id().to_le_bytes());
    let url = authorize_url(&cfg, &pkce, &state);
    LoginHandle {
        url,
        pkce,
        state,
        cfg,
        server: server.to_string(),
    }
}

/// Step 2: wait for the browser redirect, exchange the code, store tokens under
/// `mcp:<server>`.
pub async fn finish_login(handle: LoginHandle) -> anyhow::Result<()> {
    let code = wait_for_callback(handle.cfg.callback_port, &handle.state).await?;
    let tokens = exchange_code(&handle.cfg, &handle.pkce, &code).await?;
    store_tokens(&store_key(&handle.server), &tokens, &[])?;
    Ok(())
}

/// Store a static bearer token (e.g. a GitHub Personal Access Token) for a
/// server. It has no expiry and is never refreshed.
pub fn store_static_token(server: &str, token: &str) -> anyhow::Result<()> {
    let mut store = AuthStore::load();
    store.set(
        &store_key(server),
        super::Credential {
            token: token.to_string(),
            refresh_token: None,
            extra: std::collections::HashMap::new(),
        },
    );
    store.save()
}

/// The stored bearer token for a server, if any (used when the server has no
/// OAuth config — e.g. a Personal Access Token). Returns None if not present.
pub fn stored_token(server: &str) -> Option<String> {
    AuthStore::load()
        .get(&store_key(server))
        .map(|c| c.token.clone())
        .filter(|t| !t.is_empty())
}

/// Return a valid access token for the server, refreshing via the token endpoint
/// if the stored one has expired.
pub async fn access_token(server: &str, oauth: &McpOAuthConfig) -> anyhow::Result<String> {
    let key = store_key(server);
    let store = AuthStore::load();
    let cred = store
        .get(&key)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "not logged in to MCP server '{}'; run `bob mcp login {}`",
                server,
                server
            )
        })?
        .clone();

    // If we have a non-expired token, use it. 60s of slack for clock skew.
    let expires_at: u64 = cred
        .extra
        .get("expires_at")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if !cred.token.is_empty() && now() + 60 < expires_at {
        return Ok(cred.token);
    }

    // Otherwise refresh, if we can.
    let refresh = cred.refresh_token.ok_or_else(|| {
        anyhow::anyhow!(
            "session for MCP server '{}' expired and no refresh token; run `bob mcp login {}`",
            server,
            server
        )
    })?;
    let tokens = refresh_token(&oauth.token_url, &oauth.client_id, &refresh).await?;
    store_tokens(&key, &tokens, &[])?;
    Ok(tokens["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no access_token in refresh response"))?
        .to_string())
}
