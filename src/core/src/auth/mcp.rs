//! MCP OAuth + PKCE; server credentials are isolated under `mcp:<name>`.

use super::{
    authorize_url, exchange_code, now, pkce, refresh_token, store_tokens, wait_for_callback,
    AuthCodeConfig, AuthStore, Pkce,
};
use crate::core::config::McpOAuthConfig;
use serde_json::{json, Value};

fn store_key(server: &str) -> String {
    format!("mcp:{}", server)
}

const MCP_CALLBACK_PORT: u16 = 54546;

/// Discover resource/server metadata (RFC 9728/8414) and register a client (RFC 7591).
pub async fn discover(server: &str, url: &str) -> anyhow::Result<McpOAuthConfig> {
    let client = reqwest::Client::new();

    let probe = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()
        .await?;

    // Fall back to the resource's well-known URL per RFC 9728 §3.1.
    let prm_url = probe
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_resource_metadata)
        .unwrap_or_else(|| well_known(url, "oauth-protected-resource"));

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

/// Insert `.well-known/<kind>` before the resource path (RFC 8414 §3).
fn well_known(base: &str, kind: &str) -> String {
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

pub struct LoginHandle {
    pub url: String,
    pkce: Pkce,
    state: String,
    cfg: AuthCodeConfig,
    server: String,
}

/// Open the returned URL in a browser before finishing login.
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

pub async fn finish_login(handle: LoginHandle) -> anyhow::Result<()> {
    let code = wait_for_callback(handle.cfg.callback_port, &handle.state).await?;
    let tokens = exchange_code(&handle.cfg, &handle.pkce, &code).await?;
    store_tokens(&store_key(&handle.server), &tokens, &[])?;
    Ok(())
}

/// Static bearer tokens have no expiry and are never refreshed.
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

/// Bearer token for servers without OAuth configuration.
pub fn stored_token(server: &str) -> Option<String> {
    AuthStore::load()
        .get(&store_key(server))
        .map(|c| c.token.clone())
        .filter(|t| !t.is_empty())
}

/// Refresh expired tokens through the server's token endpoint.
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

    // Allow 60 seconds for clock skew.
    let expires_at: u64 = cred
        .extra
        .get("expires_at")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if !cred.token.is_empty() && now() + 60 < expires_at {
        return Ok(cred.token);
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_keys_keep_servers_separate_from_provider_logins() {
        assert_eq!(store_key("github"), "mcp:github");
        assert_eq!(store_key("anthropic"), "mcp:anthropic");
        assert_ne!(store_key("github"), store_key("github-enterprise"));
        assert_ne!(store_key("anthropic"), "anthropic");
    }

    #[test]
    fn quoted_resource_metadata_is_extracted_without_neighboring_parameters() {
        for header in [
            "Bearer resource_metadata=\"https://example.invalid/metadata\"",
            "Bearer realm=\"service\", resource_metadata= \"https://example.invalid/metadata\", error=\"invalid_token\"",
        ] {
            assert_eq!(parse_resource_metadata(header).as_deref(), Some("https://example.invalid/metadata"));
        }
        for header in [
            "",
            "Bearer",
            "Bearer realm=\"service\"",
            "Bearer resource_metadata",
        ] {
            assert_eq!(parse_resource_metadata(header), None, "{header}");
        }
    }

    #[test]
    fn well_known_metadata_keeps_authority_and_inserts_before_resource_path() {
        for (base, expected) in [
            (
                "https://example.invalid",
                "https://example.invalid/.well-known/oauth-protected-resource",
            ),
            (
                "https://example.invalid/",
                "https://example.invalid/.well-known/oauth-protected-resource",
            ),
            (
                "https://example.invalid/mcp/team/",
                "https://example.invalid/.well-known/oauth-protected-resource/mcp/team",
            ),
            (
                "http://localhost:1234/mcp",
                "http://localhost:1234/.well-known/oauth-protected-resource/mcp",
            ),
            (
                "http://[::1]:1234/mcp",
                "http://[::1]:1234/.well-known/oauth-protected-resource/mcp",
            ),
        ] {
            assert_eq!(well_known(base, "oauth-protected-resource"), expected);
        }
        assert_eq!(
            well_known(
                "https://example.invalid/tenant",
                "oauth-authorization-server"
            ),
            "https://example.invalid/.well-known/oauth-authorization-server/tenant"
        );
    }

    #[test]
    fn oauth_config_preserves_server_settings_and_uses_its_callback_port() {
        let oauth = McpOAuthConfig {
            authorize_url: "https://example.invalid/authorize".into(),
            token_url: "https://example.invalid/token".into(),
            client_id: "server-client".into(),
            scope: "read write".into(),
            callback_port: 12345,
        };
        let cfg = auth_config(&oauth);
        assert_eq!(cfg.client_id, "server-client");
        assert_eq!(cfg.authorize_url, "https://example.invalid/authorize");
        assert_eq!(cfg.token_url, "https://example.invalid/token");
        assert_eq!(cfg.scope, "read write");
        assert_eq!(cfg.redirect_uri, "http://localhost:12345/callback");
        assert_eq!(cfg.callback_port, 12345);
        assert!(cfg.extra_authorize_params.is_empty());
    }
}
