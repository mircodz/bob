//! Anthropic subscription auth (Claude Pro/Max) via OAuth 2.0 + PKCE against
//! claude.ai. The resulting access token bills the user's subscription instead
//! of API credits. Tokens are short-lived and refreshed via the refresh token.
//!
//! NOTE: this uses the same public OAuth client id as Claude Code. Using it from
//! a third-party tool is a gray area w.r.t. Anthropic's terms; API keys are the
//! sanctioned path for custom tooling.

use super::{
    authorize_url, exchange_code, now, pkce, refresh_token, store_tokens, wait_for_callback,
    AuthCodeConfig, AuthStore, Pkce,
};

// Public client id used by Claude Code's OAuth flow.
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

/// Result of starting login: the URL to open + the in-flight PKCE/state to
/// complete with.
pub struct LoginHandle {
    pub url: String,
    pkce: Pkce,
    state: String,
}

/// Step 1: build the authorize URL. Caller opens it in a browser.
pub fn begin_login() -> LoginHandle {
    let cfg = config();
    let pkce = pkce();
    let state = super::base64_url(&std::process::id().to_le_bytes());
    let url = authorize_url(&cfg, &pkce, &state);
    LoginHandle { url, pkce, state }
}

/// Step 2: wait for the browser redirect, exchange the code, store tokens.
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

/// Return a valid access token, refreshing it if expired. None if not logged in.
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
    // Refresh.
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
