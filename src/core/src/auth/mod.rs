//! Provider authentication; credentials are keyed by provider in ~/.bob/auth.json.

pub mod anthropic;
pub mod copilot;
pub mod mcp;
pub mod openai;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[async_trait::async_trait]
pub trait AuthProvider: Send + Sync {
    /// Current bearer token, refreshing if needed.
    async fn token(&self) -> anyhow::Result<String>;
    /// Extra headers this backend requires (e.g. Copilot editor headers).
    fn extra_headers(&self) -> Vec<(String, String)> {
        vec![]
    }
}

pub struct StaticKey(pub String);

#[async_trait::async_trait]
impl AuthProvider for StaticKey {
    async fn token(&self) -> anyhow::Result<String> {
        Ok(self.0.clone())
    }
}

/// Reads the environment at request time, falling back when the variable is unset.
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

pub struct AnthropicOAuth;

#[async_trait::async_trait]
impl AuthProvider for AnthropicOAuth {
    async fn token(&self) -> anyhow::Result<String> {
        anthropic::access_token().await
    }
}

/// Explicit credentials override ambient environment and stored-login resolution.
#[derive(Clone, Debug)]
pub enum ProviderAuth {
    /// Sent as `x-api-key` for Anthropic, `Bearer` for OpenAI-compatible providers.
    ApiKey(String),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Credential {
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Provider-specific extra fields.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, String>,
}

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
            Err(_) => return AuthStore::default(),
        };
        match serde_json::from_str(&raw) {
            Ok(store) => store,
            Err(e) => {
                // Preserve corrupt credentials before a later save overwrites them.
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
        // Same-directory rename avoids exposing a partial write; no fsync.
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

    pub fn remove(&mut self, provider: &str) -> bool {
        self.providers.remove(provider).is_some()
    }

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

#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

/// Show `user_code` and `verification_uri` before calling `poll_for_token`.
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

/// Poll until authorization or expiry; call `on_wait` each poll.
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

/// PKCE S256 with a 32-byte random verifier.
pub fn pkce() -> Pkce {
    let mut seed = Vec::with_capacity(32);
    seed.extend_from_slice(uuid_bytes().as_slice());
    seed.extend_from_slice(uuid_bytes().as_slice());
    let verifier = base64_url(&seed);
    let challenge = pkce_challenge(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64_url(&Sha256::digest(verifier.as_bytes()))
}

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

/// Receive the authorization code on loopback, validating `state`.
pub async fn wait_for_callback(port: u16, expected_state: &str) -> anyhow::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let (mut sock, _) =
        tokio::time::timeout(std::time::Duration::from_secs(300), listener.accept())
            .await
            .map_err(|_| anyhow::anyhow!("login timed out waiting for browser redirect"))??;

    let mut buf = vec![0u8; 8192];
    let n = sock.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let result = callback_code(&req, expected_state);

    let body = "<html><body style='font-family:sans-serif'><h2>bob is authorized ✓</h2><p>You can close this tab and return to the terminal.</p></body></html>";
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    result
}

fn callback_code(req: &str, expected_state: &str) -> anyhow::Result<String> {
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

/// Returns raw token JSON for provider-specific fields.
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

/// Unix time in seconds; zero before the epoch.
pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Persist tokens and expiry; copy `extra_keys` from top-level response fields.
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
    let cred = credential_from_tokens(tokens, store.get(provider), extra_keys, now(), access);
    store.set(provider, cred);
    store.save()
}

fn credential_from_tokens(
    tokens: &serde_json::Value,
    previous: Option<&Credential>,
    extra_keys: &[&str],
    now: u64,
    access: String,
) -> Credential {
    // Omitted refresh tokens must survive for the next refresh.
    let refresh = tokens["refresh_token"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| previous.and_then(|c| c.refresh_token.clone()));
    let expires_in = tokens["expires_in"].as_u64().unwrap_or(3600);
    let expires_at = now + expires_in;

    let mut extra = std::collections::HashMap::new();
    extra.insert("expires_at".to_string(), expires_at.to_string());
    for key in extra_keys {
        if let Some(v) = tokens[*key].as_str() {
            extra.insert((*key).to_string(), v.to_string());
        }
    }

    Credential {
        token: access,
        refresh_token: refresh,
        extra,
    }
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                // Byte indexing avoids slicing inside malformed UTF-8 escapes.
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

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn base64url_vectors_cover_partial_groups_and_url_alphabet() {
        for (bytes, expected) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg"),
            (&b"fo"[..], "Zm8"),
            (&b"foo"[..], "Zm9v"),
            (&b"foobar"[..], "Zm9vYmFy"),
            (&[0xfb, 0xff, 0xff][..], "-___"),
        ] {
            assert_eq!(base64_url(bytes), expected);
        }
    }

    #[test]
    fn pkce_s256_matches_rfc7636_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let pair = pkce();
        assert!((43..=128).contains(&pair.verifier.len()));
        assert!(pair
            .verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_eq!(pair.challenge, pkce_challenge(&pair.verifier));
    }

    #[test]
    fn authorize_query_encodes_values_and_extra_parameter_names() {
        let cfg = AuthCodeConfig {
            client_id: "client&admin=true".into(),
            authorize_url: "https://example.invalid/authorize".into(),
            token_url: "https://example.invalid/token".into(),
            scope: "read write".into(),
            redirect_uri: "http://localhost:54545/callback?x=1&y=2".into(),
            callback_port: 54545,
            extra_authorize_params: vec![("hint&key".into(), "é +/#".into())],
        };
        let pair = Pkce {
            verifier: "must-not-appear-in-url".into(),
            challenge: "a-b_c".into(),
        };
        let url = authorize_url(&cfg, &pair, "s+&=");
        let parsed = reqwest::Url::parse(&url).unwrap();
        let mut pairs: Vec<_> = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        pairs.sort();
        let mut expected: Vec<_> = [
            ("response_type", "code"),
            ("client_id", "client&admin=true"),
            ("redirect_uri", "http://localhost:54545/callback?x=1&y=2"),
            ("scope", "read write"),
            ("state", "s+&="),
            ("code_challenge", "a-b_c"),
            ("code_challenge_method", "S256"),
            ("hint&key", "é +/#"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        expected.sort();
        assert_eq!(pairs, expected);
        assert_eq!(parsed.path(), "/authorize");
        assert!(parsed.fragment().is_none());
        assert!(!url.contains(&pair.verifier));
    }

    #[test]
    fn callback_requires_matching_decoded_state_and_code() {
        assert_eq!(
            callback_code(
                "GET /callback?code=a%2Bb%3D&state=s%2B%26%3D HTTP/1.1\r\nHost: localhost\r\n",
                "s+&="
            )
            .unwrap(),
            "a+b="
        );
        for (request, expected) in [
            ("GET /callback?code=x HTTP/1.1", "state mismatch"),
            (
                "GET /callback?code=x&state=wrong HTTP/1.1",
                "state mismatch",
            ),
            (
                "GET /callback?state=expected HTTP/1.1",
                "no authorization code",
            ),
            (
                "GET /callback?error=access_denied&state=expected HTTP/1.1",
                "authorization failed: access_denied",
            ),
            ("", "state mismatch"),
        ] {
            let error = callback_code(request, "expected").unwrap_err().to_string();
            assert!(error.contains(expected), "{request:?}: {error}");
        }
    }

    #[test]
    fn callback_decoding_preserves_malformed_unicode_and_distinguishes_plus() {
        for (input, expected) in [
            ("a+b%2Bc", "a b+c"),
            ("%C3%A9", "é"),
            ("%a€x", "%a€x"),
            ("%", "%"),
            ("%2", "%2"),
            ("%zz", "%zz"),
            ("%FF", "�"),
        ] {
            assert_eq!(urldecode(input), expected);
        }
    }

    #[test]
    fn refresh_response_preserves_or_rotates_token_and_records_expiry() {
        let previous = Credential {
            token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            extra: HashMap::new(),
        };
        for (response, expected_refresh, expected_expiry) in [
            (json!({}), Some("old-refresh"), "4600"),
            (
                json!({"refresh_token": null, "expires_in": 0}),
                Some("old-refresh"),
                "1000",
            ),
            (
                json!({"refresh_token": "rotated", "expires_in": 60}),
                Some("rotated"),
                "1060",
            ),
            (json!({"expires_in": "bad"}), Some("old-refresh"), "4600"),
        ] {
            let cred =
                credential_from_tokens(&response, Some(&previous), &[], 1000, "new-access".into());
            assert_eq!(cred.token, "new-access");
            assert_eq!(cred.refresh_token.as_deref(), expected_refresh);
            assert_eq!(cred.extra["expires_at"], expected_expiry);
        }
        let cred = credential_from_tokens(
            &json!({"id_token": "id", "ignored": "value"}),
            None,
            &["id_token"],
            1000,
            "access".into(),
        );
        assert!(cred.refresh_token.is_none());
        assert_eq!(
            cred.extra,
            HashMap::from([
                ("expires_at".into(), "4600".into()),
                ("id_token".into(), "id".into())
            ])
        );
    }

    #[test]
    fn refresh_conversion_copies_only_requested_string_metadata_without_mutating_inputs() {
        let previous = Credential {
            token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            extra: HashMap::from([("expires_at".into(), "900".into())]),
        };
        let response = json!({
            "refresh_token": "rotated",
            "expires_in": 45,
            "id_token": "new-id",
            "account_id": 17,
            "unrequested": "must-not-copy"
        });
        let old_previous = serde_json::to_value(&previous).unwrap();
        let old_response = response.clone();
        let cred = credential_from_tokens(
            &response,
            Some(&previous),
            &["id_token", "account_id", "absent"],
            1000,
            "new-access".into(),
        );
        assert_eq!(
            serde_json::to_value(&cred).unwrap(),
            json!({
                "token": "new-access",
                "refresh_token": "rotated",
                "extra": {"expires_at": "1045", "id_token": "new-id"}
            })
        );
        assert_eq!(serde_json::to_value(&previous).unwrap(), old_previous);
        assert_eq!(response, old_response);
    }

    #[test]
    fn callback_accepts_reordered_parameters_but_not_state_prefixes_or_header_injection() {
        assert_eq!(
            callback_code(
                "GET /callback?state=s%2B1&unused=x&code=a%26b%3Dc HTTP/1.1\r\nHost: localhost\r\n",
                "s+1",
            )
            .unwrap(),
            "a&b=c"
        );
        for request in [
            "GET /callback?state=expected-suffix&code=secret HTTP/1.1",
            "GET /callback?state=Expected&code=secret HTTP/1.1",
            "GET /callback?code=secret HTTP/1.1\r\nX-State: state=expected",
            "GET /callback HTTP/1.1\r\nX-Query: ?state=expected&code=secret",
        ] {
            let error = callback_code(request, "expected").unwrap_err().to_string();
            assert!(error.contains("state mismatch"), "{request:?}: {error}");
            assert!(!error.contains("secret"));
        }
        assert!(callback_code(
            "GET /callback?state=expected&code=secret&error=access_denied HTTP/1.1",
            "expected",
        )
        .is_err());
    }

    #[test]
    fn form_encoding_preserves_unreserved_bytes_and_escapes_delimiters_and_utf8() {
        for (raw, encoded) in [
            ("AZaz09-_.~", "AZaz09-_.~"),
            (" &=+/#?%", "%20%26%3D%2B%2F%23%3F%25"),
            ("é\n\0", "%C3%A9%0A%00"),
        ] {
            assert_eq!(urlencode(raw), encoded);
        }
    }

    #[test]
    fn credential_store_replacement_and_logout_are_provider_scoped() {
        let mut store = AuthStore::default();
        for (provider, token) in [("zeta", "z-token"), ("alpha", "a-token"), ("empty", "")] {
            store.set(
                provider,
                Credential {
                    token: token.into(),
                    ..Credential::default()
                },
            );
        }
        assert_eq!(store.logged_in(), ["alpha", "zeta"]);
        assert!(store.get("missing").is_none());
        let untouched = serde_json::to_value(store.get("zeta").unwrap()).unwrap();
        store.set(
            "alpha",
            Credential {
                token: "replacement".into(),
                refresh_token: Some("refresh".into()),
                extra: HashMap::from([("expires_at".into(), "2000".into())]),
            },
        );
        assert_eq!(store.providers.len(), 3);
        assert_eq!(store.get("alpha").unwrap().token, "replacement");
        assert_eq!(
            store.get("alpha").unwrap().refresh_token.as_deref(),
            Some("refresh")
        );
        assert_eq!(
            serde_json::to_value(store.get("zeta").unwrap()).unwrap(),
            untouched
        );
        assert!(store.remove("alpha"));
        assert!(!store.remove("alpha"));
        assert_eq!(store.logged_in(), ["zeta"]);
        assert_eq!(
            serde_json::to_value(store.get("zeta").unwrap()).unwrap(),
            untouched
        );
        assert!(store.get("empty").is_some());
    }

    #[test]
    fn credential_wire_format_accepts_legacy_minimal_entries_without_inventing_credentials() {
        let store: AuthStore = serde_json::from_value(json!({
            "providers": {"old": {"token": "access"}, "empty": {"token": ""}}
        }))
        .unwrap();
        let cred = store.get("old").unwrap();
        assert_eq!(cred.token, "access");
        assert!(cred.refresh_token.is_none());
        assert!(cred.extra.is_empty());
        assert_eq!(
            serde_json::to_value(cred).unwrap(),
            json!({"token": "access"})
        );
        assert_eq!(store.logged_in(), ["old"]);
        let empty: AuthStore = serde_json::from_value(json!({})).unwrap();
        assert!(empty.logged_in().is_empty());
        for malformed in [json!({}), json!({"token": null}), json!({"token": 42})] {
            assert!(serde_json::from_value::<Credential>(malformed).is_err());
        }
    }
}
