//! OpenAI / ChatGPT subscription auth via the Codex **device-code** flow.
//! Reverse-engineered from the open-source Codex CLI
//! (codex-rs/login/src/device_code_auth.rs + server.rs). Three steps:
//!   1. POST {issuer}/api/accounts/deviceauth/usercode  {client_id}
//!        → { device_auth_id, user_code, interval }
//!      Show the user {issuer}/codex/device + the user_code.
//!   2. Poll POST {issuer}/api/accounts/deviceauth/token {device_auth_id, user_code}
//!        (403/404 = pending) → { authorization_code, code_challenge, code_verifier }
//!   3. POST {issuer}/oauth/token  (form-encoded, grant_type=authorization_code,
//!      code, redirect_uri={issuer}/deviceauth/callback, client_id, code_verifier)
//!        → { id_token, access_token, refresh_token }
//! Refresh uses grant_type=refresh_token at the same /oauth/token endpoint.

use super::{AuthStore, Credential};

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";

/// The device+user code shown to the user, plus the id used to poll.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub verification_uri: String,
    pub user_code: String,
    pub device_auth_id: String,
    pub interval: u64,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("bob")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Step 1: request the user code.
pub async fn begin_login() -> anyhow::Result<DeviceCode> {
    let url = format!("{}/api/accounts/deviceauth/usercode", ISSUER);
    let res = client()
        .post(&url)
        .header("content-type", "application/json")
        .body(serde_json::json!({ "client_id": CLIENT_ID }).to_string())
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("device code request failed ({})", res.status());
    }
    let v: serde_json::Value = res.json().await?;
    // `interval` may come as a string; parse loosely.
    let interval = v["interval"]
        .as_u64()
        .or_else(|| v["interval"].as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(5);
    Ok(DeviceCode {
        verification_uri: format!("{}/codex/device", ISSUER),
        user_code: v["user_code"]
            .as_str()
            .or_else(|| v["usercode"].as_str())
            .unwrap_or_default()
            .to_string(),
        device_auth_id: v["device_auth_id"].as_str().unwrap_or_default().to_string(),
        interval,
    })
}

/// Step 2 + 3: poll until authorized, exchange for tokens, store them.
pub async fn finish_login<F: FnMut()>(device: &DeviceCode, mut on_wait: F) -> anyhow::Result<()> {
    let client = client();
    let poll_url = format!("{}/api/accounts/deviceauth/token", ISSUER);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15 * 60);

    // Poll for the authorization_code + PKCE codes.
    let (auth_code, code_verifier) = loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("device auth timed out after 15 minutes");
        }
        let res = client
            .post(&poll_url)
            .header("content-type", "application/json")
            .body(
                serde_json::json!({
                    "device_auth_id": device.device_auth_id,
                    "user_code": device.user_code,
                })
                .to_string(),
            )
            .send()
            .await?;
        let status = res.status();
        if status.is_success() {
            let v: serde_json::Value = res.json().await?;
            let code = v["authorization_code"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("no authorization_code"))?
                .to_string();
            let verifier = v["code_verifier"].as_str().unwrap_or_default().to_string();
            break (code, verifier);
        }
        // 403/404 mean "still pending" in this flow.
        if status.as_u16() == 403 || status.as_u16() == 404 {
            on_wait();
            tokio::time::sleep(std::time::Duration::from_secs(device.interval.max(1))).await;
            continue;
        }
        anyhow::bail!("device auth failed ({})", status);
    };

    // Exchange the authorization code for tokens (form-encoded).
    let redirect_uri = format!("{}/deviceauth/callback", ISSUER);
    let token_url = format!("{}/oauth/token", ISSUER);
    let form = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        urlencode(&auth_code),
        urlencode(&redirect_uri),
        urlencode(CLIENT_ID),
        urlencode(&code_verifier),
    );
    let res = client
        .post(&token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("token exchange failed ({}): {}", res.status(), res.text().await.unwrap_or_default());
    }
    let tokens: serde_json::Value = res.json().await?;
    store_tokens(&tokens)?;
    Ok(())
}

fn store_tokens(tokens: &serde_json::Value) -> anyhow::Result<()> {
    let access = tokens["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no access_token"))?
        .to_string();
    let refresh = tokens["refresh_token"].as_str().map(|s| s.to_string());
    // These tokens are typically ~1h; store an expiry so we refresh proactively.
    let expires_in = tokens["expires_in"].as_u64().unwrap_or(3600);
    let expires_at = now() + expires_in;

    let mut store = AuthStore::load();
    let mut extra = std::collections::HashMap::new();
    extra.insert("expires_at".to_string(), expires_at.to_string());
    if let Some(id) = tokens["id_token"].as_str() {
        extra.insert("id_token".to_string(), id.to_string());
    }
    store.set(
        "openai",
        Credential {
            token: access,
            refresh_token: refresh,
            extra,
        },
    );
    store.save()?;
    Ok(())
}

pub fn is_logged_in() -> bool {
    AuthStore::load()
        .get("openai")
        .map(|c| !c.token.is_empty())
        .unwrap_or(false)
}

/// Return a valid access token, refreshing via /oauth/token if expired.
pub async fn access_token() -> anyhow::Result<String> {
    let store = AuthStore::load();
    let cred = store
        .get("openai")
        .ok_or_else(|| anyhow::anyhow!("not logged in to OpenAI — run `bob login openai`"))?
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
    let token_url = format!("{}/oauth/token", ISSUER);
    let form = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        urlencode(&refresh),
        urlencode(CLIENT_ID),
    );
    let res = client()
        .post(&token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("token refresh failed ({}); run `bob login openai` again", res.status());
    }
    let tokens: serde_json::Value = res.json().await?;
    store_tokens(&tokens)?;
    Ok(tokens["access_token"].as_str().unwrap_or_default().to_string())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
