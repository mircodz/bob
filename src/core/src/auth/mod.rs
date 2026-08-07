//! Authentication framework. A pluggable way to log in to providers and store
//! credentials, so bob can talk to Copilot / Claude / ChatGPT without a proxy or
//! a hand-pasted key. Credentials live in ~/.bob/auth.json keyed by provider id.
//!
//! The framework is intentionally small: a `DeviceFlow` helper implements the
//! OAuth 2.0 device authorization grant (RFC 8628), which GitHub uses; other
//! providers can add their own flows (PKCE, etc.) later behind the same store.

pub mod anthropic;
pub mod copilot;
pub mod mcp;
pub mod openai;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// The auth seam: something that supplies a bearer token (refreshing short-lived
/// ones transparently) plus any extra headers a backend requires. Every provider
/// holds an `Arc<dyn AuthProvider>` instead of reaching into the auth modules
/// directly — so auth is injectable (tests, alternate credential sources) and no
/// single provider owns the notion of "how do I get a token".
///
/// This is the canonical home of what used to be `providers::openai::TokenSource`
/// (which now re-exports this), so all provider families share ONE trait.
#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync {
    /// The current bearer token, refreshing if needed (single-flight inside).
    async fn token(&self) -> anyhow::Result<String>;
    /// Extra headers this backend requires (e.g. Copilot editor headers).
    fn extra_headers(&self) -> Vec<(String, String)> {
        vec![]
    }
}

/// A fixed API key (OpenAI-compatible endpoints, proxies, local gateways).
pub struct StaticKey(pub String);

#[async_trait::async_trait]
impl AuthProvider for StaticKey {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

/// An API key read from an environment variable at request time (so a key set
/// after startup is still picked up). `fallback` is used when the var is unset.
pub struct EnvKey {
    pub var: String,
    pub fallback: String,
}

#[async_trait::async_trait]
impl AuthProvider for EnvKey {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(std::env::var(&self.var).unwrap_or_else(|_| self.fallback.clone()))
    }
}

/// The Anthropic subscription (Claude Pro/Max) OAuth token source — refreshes via
/// the stored refresh token. Lets the Anthropic provider go through the same
/// `AuthProvider` seam as the OpenAI family instead of calling `auth::anthropic`
/// inline.
pub struct AnthropicOAuth;

#[async_trait::async_trait]
impl AuthProvider for AnthropicOAuth {
    async fn token(&self) -> anyhow::Result<String> {
        anthropic::access_token().await
    }
}

/// One provider's stored credentials. Flexible bag so different auth schemes fit
/// (an OAuth token here, a refresh token there). `extra` holds provider-specific
/// fields (e.g. Copilot's cached short-lived token + its expiry).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Credential {
    /// The long-lived token from the login flow (e.g. a GitHub OAuth token).
    pub token: String,
    /// Optional refresh token (for schemes that use one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Provider-specific extra fields.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, String>,
}

/// The on-disk credential store: provider id → credential.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(default)]
    pub providers: HashMap<String, Credential>,
}

fn auth_path() -> PathBuf {
    let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join(".bob").join("auth.json")
}

impl AuthStore {
    pub fn load() -> AuthStore {
        let path = auth_path();
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            // Absent (or unreadable) → a legitimately empty store. This is the
            // normal first-run case; nothing to preserve.
            Err(_) => return AuthStore::default(),
        };
        match serde_json::from_str(&raw) {
            Ok(store) => store,
            Err(e) => {
                // The file EXISTS but doesn't parse — a partial/corrupt write, most
                // likely from an interrupted (non-atomic, historically) save. Do NOT
                // silently return an empty store: the next store_tokens()->save()
                // would overwrite this file and permanently destroy every provider's
                // tokens. Back the bad file up first so a later save can't clobber
                // the only copy, and so it can be recovered by hand.
                let backup = path.with_extension("json.corrupt");
                let _ = std::fs::rename(&path, &backup);
                eprintln!(
                    "bob: warning: {} was unreadable ({e}); backed it up to {} and \
                     started with an empty credential store. Re-run login if needed.",
                    path.display(),
                    backup.display()
                );
                AuthStore::default()
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = auth_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self)?;
        // Write atomically: serialize to a temp file in the same dir, fsync-free
        // rename over the target. A crash mid-write then leaves EITHER the old file
        // or the new one intact — never a half-written file that load() would treat
        // as corrupt and back away from.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &body)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &path)?;
        // Best-effort: restrict permissions (contains tokens).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    pub fn get(&self, provider: &str) -> Option<&Credential> {
        self.providers.get(provider)
    }

    pub fn set(&mut self, provider: &str, cred: Credential) {
        self.providers.insert(provider.to_string(), cred);
    }

    /// Remove a provider's stored credentials (logout). Returns true if present.
    pub fn remove(&mut self, provider: &str) -> bool {
        self.providers.remove(provider).is_some()
    }

    /// Provider ids we currently hold credentials for.
    pub fn logged_in(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .providers
            .iter()
            .filter(|(_, c)| !c.token.is_empty())
            .map(|(k, _)| k.clone())
            .collect();
        v.sort();
        v
    }
}

/// Parameters for an OAuth 2.0 device authorization grant (RFC 8628).
pub struct DeviceFlowConfig {
    pub client_id: String,
    pub scope: String,
    /// Endpoint that issues a device+user code.
    pub device_code_url: String,
    /// Endpoint that exchanges the device code for an access token.
    pub token_url: String,
}

/// What the device endpoint returns — shown to the user so they can authorize.
#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// Step 1: request a device + user code. The caller shows `user_code` and
/// `verification_uri` to the user, then calls `poll_for_token`.
pub async fn request_device_code(cfg: &DeviceFlowConfig) -> anyhow::Result<DeviceCode> {
    let client = reqwest::Client::new();
    let res = client
        .post(&cfg.device_code_url)
        .header("accept", "application/json")
        .header("user-agent", "bob")
        .form(&[
            ("client_id", cfg.client_id.as_str()),
            ("scope", cfg.scope.as_str()),
        ])
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("device code request failed: {}", res.status());
    }
    let v: serde_json::Value = res.json().await?;
    Ok(DeviceCode {
        device_code: v["device_code"].as_str().unwrap_or_default().to_string(),
        user_code: v["user_code"].as_str().unwrap_or_default().to_string(),
        verification_uri: v["verification_uri"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        interval: v["interval"].as_u64().unwrap_or(5),
        expires_in: v["expires_in"].as_u64().unwrap_or(900),
    })
}

/// Step 2: poll the token endpoint until the user authorizes (or it expires).
/// Returns the access token on success. `on_wait` is called each poll so the
/// caller can show a spinner / keep the UI alive.
pub async fn poll_for_token<F: FnMut()>(
    cfg: &DeviceFlowConfig,
    device: &DeviceCode,
    mut on_wait: F,
) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(device.expires_in);
    let mut interval = device.interval.max(1);

    loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("device authorization timed out; run login again");
        }
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        on_wait();

        let res = client
            .post(&cfg.token_url)
            .header("accept", "application/json")
            .header("user-agent", "bob")
            .form(&[
                ("client_id", cfg.client_id.as_str()),
                ("device_code", device.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?;
        let v: serde_json::Value = res.json().await?;

        if let Some(token) = v["access_token"].as_str() {
            return Ok(token.to_string());
        }
        match v["error"].as_str() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval += 5;
                continue;
            }
            Some("expired_token") => anyhow::bail!("device code expired; run login again"),
            Some("access_denied") => anyhow::bail!("authorization denied"),
            Some(other) => anyhow::bail!("device auth error: {}", other),
            None => continue,
        }
    }
}

/* ------------------------------------------------------------------ */
/* PKCE + authorization-code helpers, for subscription OAuth flows      */
/* (Claude Pro/Max via claude.ai, ChatGPT Plus/Pro via auth.openai.com) */
/* ------------------------------------------------------------------ */

/// URL-safe base64 without padding (RFC 4648 §5), used for PKCE.
pub fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
}

/// A PKCE code verifier + its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a PKCE pair. The verifier is high-entropy random; the challenge is
/// base64url(sha256(verifier)). Randomness comes from a fresh UUID pair (128
/// bits each) to avoid pulling in a separate RNG crate.
pub fn pkce() -> Pkce {
    use sha2::{Digest, Sha256};
    // 32 bytes of entropy from two UUIDs.
    let mut seed = Vec::with_capacity(32);
    seed.extend_from_slice(uuid_bytes().as_slice());
    seed.extend_from_slice(uuid_bytes().as_slice());
    let verifier = base64_url(&seed);
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = base64_url(&hasher.finalize());
    Pkce {
        verifier,
        challenge,
    }
}

/// 16 cryptographically-random bytes (a UUID's worth), for PKCE verifiers and
/// OAuth state. Uses the OS CSPRNG via `getrandom`.
fn uuid_bytes() -> [u8; 16] {
    let mut out = [0u8; 16];
    getrandom::getrandom(&mut out).expect("OS RNG unavailable");
    out
}

/// Parameters for an OAuth 2.0 authorization-code + PKCE flow.
pub struct AuthCodeConfig {
    pub client_id: String,
    pub authorize_url: String,
    pub token_url: String,
    pub scope: String,
    /// The redirect URI registered for the client (usually a localhost port).
    pub redirect_uri: String,
    /// The localhost port to listen on for the callback.
    pub callback_port: u16,
    /// Extra query params to add to the authorize URL (provider-specific).
    pub extra_authorize_params: Vec<(String, String)>,
}

/// Build the authorize URL the user opens in a browser.
pub fn authorize_url(cfg: &AuthCodeConfig, pkce: &Pkce, state: &str) -> String {
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        cfg.authorize_url,
        urlencode(&cfg.client_id),
        urlencode(&cfg.redirect_uri),
        urlencode(&cfg.scope),
        urlencode(state),
        urlencode(&pkce.challenge),
    );
    for (k, v) in &cfg.extra_authorize_params {
        url.push('&');
        url.push_str(&urlencode(k));
        url.push('=');
        url.push_str(&urlencode(v));
    }
    url
}

/// Listen on the localhost callback port for the OAuth redirect, returning the
/// `code` (and validating `state`). Serves a tiny "you can close this" page.
pub async fn wait_for_callback(port: u16, expected_state: &str) -> anyhow::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    // One connection carries the redirect.
    let (mut sock, _) =
        tokio::time::timeout(std::time::Duration::from_secs(300), listener.accept())
            .await
            .map_err(|_| anyhow::anyhow!("login timed out waiting for browser redirect"))??;

    let mut buf = vec![0u8; 8192];
    let n = sock.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    // First line: GET /callback?code=...&state=... HTTP/1.1
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("");
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params: HashMap<String, String> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_string(), urldecode(v)))
        .collect();

    let body = "<html><body style='font-family:sans-serif'><h2>bob is authorized ✓</h2><p>You can close this tab and return to the terminal.</p></body></html>";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = sock.write_all(resp.as_bytes()).await;

    if let Some(err) = params.get("error") {
        anyhow::bail!("authorization failed: {}", err);
    }
    if params.get("state").map(|s| s.as_str()) != Some(expected_state) {
        anyhow::bail!("state mismatch (possible CSRF); login aborted");
    }
    params
        .get("code")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no authorization code in callback"))
}

/// Exchange an authorization code (+ PKCE verifier) for tokens. Returns the raw
/// token JSON so provider modules can pull out access/refresh/expiry as needed.
pub async fn exchange_code(
    cfg: &AuthCodeConfig,
    pkce: &Pkce,
    code: &str,
) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let res = client
        .post(&cfg.token_url)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", cfg.client_id.as_str()),
            ("redirect_uri", cfg.redirect_uri.as_str()),
            ("code_verifier", pkce.verifier.as_str()),
        ])
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!(
            "token exchange failed ({}): {}",
            res.status(),
            res.text().await.unwrap_or_default()
        );
    }
    Ok(res.json().await?)
}

/// Refresh an access token using a refresh token. Returns the raw token JSON.
pub async fn refresh_token(
    token_url: &str,
    client_id: &str,
    refresh: &str,
) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let res = client
        .post(token_url)
        .header("accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client_id),
        ])
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("token refresh failed ({})", res.status());
    }
    Ok(res.json().await?)
}

pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// Current Unix time in seconds (0 on the impossible pre-epoch case).
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persist an OAuth token response under `provider`, recording an `expires_at`
/// so callers can refresh proactively. `extra_keys` names additional top-level
/// token fields to copy into the credential's `extra` map (e.g. "id_token").
pub(crate) fn store_tokens(
    provider: &str,
    tokens: &serde_json::Value,
    extra_keys: &[&str],
) -> anyhow::Result<()> {
    let access = tokens["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no access_token"))?
        .to_string();
    let mut store = AuthStore::load();
    // Preserve the existing refresh_token when the refresh response omits one.
    // Many OAuth servers (rotation setups, or providers that just don't re-send it)
    // return no refresh_token on a refresh; overwriting with None here would brick
    // the NEXT expiry ("no refresh token; log in again").
    let refresh = tokens["refresh_token"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| store.get(provider).and_then(|c| c.refresh_token.clone()));
    // These tokens are typically ~1h; store an expiry so we refresh proactively.
    let expires_in = tokens["expires_in"].as_u64().unwrap_or(3600);
    let expires_at = now() + expires_in;

    let mut extra = std::collections::HashMap::new();
    extra.insert("expires_at".to_string(), expires_at.to_string());
    for key in extra_keys {
        if let Some(v) = tokens[*key].as_str() {
            extra.insert((*key).to_string(), v.to_string());
        }
    }

    store.set(
        provider,
        Credential {
            token: access,
            refresh_token: refresh,
            extra,
        },
    );
    store.save()
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                // Read the two hex BYTES directly — `&s[i+1..i+3]` panics when the
                // `%` precedes a multi-byte char (slice off a char boundary).
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push(hi * 16 + lo);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// One hex digit (ASCII) → its value, or None if not a hex digit.
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
