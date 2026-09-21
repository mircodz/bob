//! Anthropic subscription OAuth + PKCE using Claude Code's public client ID.
//! Third-party use may be restricted by Anthropic's terms; prefer API keys.

use super::{
    authorize_url, exchange_code, now, pkce, refresh_token, store_tokens, wait_for_callback,
    AuthCodeConfig, AuthStore, Pkce,
};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const CALLBACK_PORT: u16 = 54545;
const SCOPE: &str = "org:create_api_key user:profile user:inference";

fn config() -> AuthCodeConfig {
    AuthCodeConfig {
        client_id: CLIENT_ID.to_string(),
        authorize_url: AUTHORIZE_URL.to_string(),
        token_url: TOKEN_URL.to_string(),
        scope: SCOPE.to_string(),
        redirect_uri: format!("http://localhost:{}/callback", CALLBACK_PORT),
        callback_port: CALLBACK_PORT,
        extra_authorize_params: vec![],
    }
}

pub struct LoginHandle {
    pub url: String,
    pkce: Pkce,
    state: String,
}

/// Open the returned URL in a browser before finishing login.
pub fn begin_login() -> LoginHandle {
    let cfg = config();
    let pkce = pkce();
    let state = super::base64_url(&std::process::id().to_le_bytes());
    let url = authorize_url(&cfg, &pkce, &state);
    LoginHandle { url, pkce, state }
}

pub async fn finish_login(handle: LoginHandle) -> anyhow::Result<()> {
    let cfg = config();
    let code = wait_for_callback(cfg.callback_port, &handle.state).await?;
    let tokens = exchange_code(&cfg, &handle.pkce, &code).await?;
    store_tokens("anthropic", &tokens, &[])?;
    Ok(())
}

pub fn is_logged_in() -> bool {
    AuthStore::load()
        .get("anthropic")
        .map(|c| !c.token.is_empty())
        .unwrap_or(false)
}

/// Refresh expired tokens; error if not logged in.
pub async fn access_token() -> anyhow::Result<String> {
    let store = AuthStore::load();
    let cred = store
        .get("anthropic")
        .ok_or_else(|| anyhow::anyhow!("not logged in to Anthropic — run `bob login anthropic`"))?
        .clone();

    let expires_at: u64 = cred
        .extra
        .get("expires_at")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if expires_at > now() + 60 {
        return Ok(cred.token);
    }
    let refresh = cred
        .refresh_token
        .ok_or_else(|| anyhow::anyhow!("session expired and no refresh token; log in again"))?;
    let tokens = refresh_token(TOKEN_URL, CLIENT_ID, &refresh).await?;
    store_tokens("anthropic", &tokens, &[])?;
    Ok(tokens["access_token"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}
