//! GitHub Copilot auth. Two-token dance:
//!   1. GitHub OAuth token — obtained once via device flow, stored long-term.
//!   2. Copilot API token — short-lived (~30 min), fetched from the GitHub token
//!      on demand and cached until it nears expiry.
//! The Copilot chat API itself is OpenAI-compatible at api.githubcopilot.com.

use super::{
    poll_for_token, request_device_code, AuthStore, Credential, DeviceCode, DeviceFlowConfig,
};

/// The public GitHub OAuth client id used by the Copilot/VS Code integration.
const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

fn device_flow() -> DeviceFlowConfig {
    DeviceFlowConfig {
        client_id: GITHUB_CLIENT_ID.to_string(),
        scope: "read:user".to_string(),
        device_code_url: DEVICE_CODE_URL.to_string(),
        token_url: TOKEN_URL.to_string(),
    }
}

/// Begin the device-login flow: returns the code/URL to show the user.
pub async fn begin_login() -> anyhow::Result<DeviceCode> {
    request_device_code(&device_flow()).await
}

/// Finish login: poll until the user authorizes, then persist the GitHub token
/// under the "copilot" provider in the auth store.
pub async fn finish_login<F: FnMut()>(device: &DeviceCode, on_wait: F) -> anyhow::Result<()> {
    let github_token = poll_for_token(&device_flow(), device, on_wait).await?;
    let mut store = AuthStore::load();
    store.set(
        "copilot",
        Credential {
            token: github_token,
            refresh_token: None,
            extra: Default::default(),
        },
    );
    store.save()?;
    Ok(())
}

/// Is there a stored GitHub token for Copilot?
pub fn is_logged_in() -> bool {
    AuthStore::load()
        .get("copilot")
        .map(|c| !c.token.is_empty())
        .unwrap_or(false)
}

/// Exchange the stored GitHub token for a short-lived Copilot API token.
/// Returns (token, expires_at_unix_secs, api_base_url). The api base comes from
/// the response's `endpoints.api` — it differs for Enterprise accounts
/// (api.enterprise.githubcopilot.com) vs. individual (api.githubcopilot.com).
pub async fn fetch_copilot_token(github_token: &str) -> anyhow::Result<(String, u64, String)> {
    let client = reqwest::Client::new();
    let res = client
        .get(COPILOT_TOKEN_URL)
        .header("authorization", format!("token {}", github_token))
        .header("user-agent", "bob")
        .header("accept", "application/json")
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!(
            "copilot token exchange failed ({}); try `bob login copilot` again",
            res.status()
        );
    }
    let v: serde_json::Value = res.json().await?;
    let token = v["token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no copilot token in response"))?
        .to_string();
    let expires_at = v["expires_at"].as_u64().unwrap_or(0);
    let api_base = v["endpoints"]["api"]
        .as_str()
        .unwrap_or("https://api.githubcopilot.com")
        .to_string();
    Ok((token, expires_at, api_base))
}

/// Load the stored GitHub token (the long-lived one).
pub fn github_token() -> Option<String> {
    AuthStore::load().get("copilot").map(|c| c.token.clone())
}
