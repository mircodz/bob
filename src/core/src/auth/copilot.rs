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

/// One model's capability limits from the Copilot `/models` response. Fields are
/// optional because embedding models (and future entries) omit them.
#[derive(Debug, Clone, Default)]
pub struct ModelLimits {
    pub id: String,
    /// Total context window (input + output) the backend advertises.
    pub max_context_window_tokens: Option<usize>,
    /// The input-token budget — what actually bounds our compaction. This is the
    /// window MINUS the reserved output allowance, so it's the number to size the
    /// history against.
    pub max_prompt_tokens: Option<usize>,
    pub max_output_tokens: Option<usize>,
}

/// Fetch per-model capability limits from the Copilot `/models` endpoint using a
/// freshly-minted Copilot token. Best-effort: returns an empty vec on any
/// failure so callers can fall back to the static id-based heuristic.
pub async fn fetch_model_limits(copilot_token: &str, api_base: &str) -> Vec<ModelLimits> {
    let client = reqwest::Client::new();
    let res = client
        .get(format!("{}/models", api_base.trim_end_matches('/')))
        .header("authorization", format!("Bearer {}", copilot_token))
        .header("user-agent", "bob")
        .header("editor-version", "bob/1.0")
        .header("copilot-integration-id", "vscode-chat")
        .send()
        .await;
    let Ok(res) = res else {
        return Vec::new();
    };
    if !res.status().is_success() {
        return Vec::new();
    }
    let Ok(v) = res.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    v["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m["id"].as_str()?.to_string();
                    let limits = &m["capabilities"]["limits"];
                    Some(ModelLimits {
                        id,
                        max_context_window_tokens: limits["max_context_window_tokens"]
                            .as_u64()
                            .map(|n| n as usize),
                        max_prompt_tokens: limits["max_prompt_tokens"].as_u64().map(|n| n as usize),
                        max_output_tokens: limits["max_output_tokens"].as_u64().map(|n| n as usize),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
