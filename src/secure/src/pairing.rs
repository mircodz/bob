//! The pairing payload: the compact bundle a laptop shows (as a QR / text block)
//! for a phone to scan on first pairing. It carries everything the phone needs to
//! locate and authenticate the laptop: where the relay is, which session slot to
//! join, the laptop's static public key (the XK "known responder static"), and a
//! one-time pairing secret that seeds the relay admission proof.
//!
//! Encoded as a single URL so it round-trips cleanly through a QR code and can be
//! typed/pasted in a pinch: `bobpair://v1?relay=..&session=..&key=..&secret=..`.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use x25519_dalek::PublicKey;

/// Everything a phone needs to pair with a specific laptop, once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    /// Relay WebSocket URL, e.g. `wss://relay.example/ws`.
    pub relay: String,
    /// Session slot id both peers join on the relay.
    pub session: String,
    /// The laptop's (responder's) static public key.
    pub static_key: PublicKey,
    /// One-time pairing secret: seeds the relay admission proof for this pairing.
    pub secret: Vec<u8>,
}

const SCHEME: &str = "bobpair";
const VERSION: &str = "v1";

impl Pairing {
    /// Encode as a `bobpair://v1?...` URL suitable for a QR code.
    pub fn to_url(&self) -> String {
        format!(
            "{SCHEME}://{VERSION}?relay={}&session={}&key={}&secret={}",
            urlencode(&self.relay),
            urlencode(&self.session),
            b64(self.static_key.as_bytes()),
            b64(&self.secret),
        )
    }

    /// Parse a `bobpair://v1?...` URL produced by [`to_url`](Self::to_url).
    pub fn from_url(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix(&format!("{SCHEME}://{VERSION}?"))
            .ok_or_else(|| anyhow!("not a bobpair {VERSION} url"))?;

        let mut relay = None;
        let mut session = None;
        let mut key = None;
        let mut secret = None;
        for pair in rest.split('&') {
            let (k, v) = pair
                .split_once('=')
                .ok_or_else(|| anyhow!("bad query pair"))?;
            match k {
                "relay" => relay = Some(urldecode(v)?),
                "session" => session = Some(urldecode(v)?),
                "key" => key = Some(unb64(v)?),
                "secret" => secret = Some(unb64(v)?),
                _ => {} // ignore unknown params for forward-compat
            }
        }

        let key = key.context("missing key")?;
        let arr: [u8; 32] = key
            .try_into()
            .map_err(|_| anyhow!("static key is not 32 bytes"))?;
        Ok(Pairing {
            relay: relay.context("missing relay")?,
            session: session.context("missing session")?,
            static_key: PublicKey::from(arr),
            secret: secret.context("missing secret")?,
        })
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}
fn unb64(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .context("invalid base64")
}

/// Minimal percent-encoding for the handful of characters that appear in a relay
/// URL or session id and would break query parsing.
fn urlencode(s: &str) -> String {
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

fn urldecode(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = s
                    .get(i + 1..i + 3)
                    .ok_or_else(|| anyhow!("truncated %-escape"))?;
                out.push(u8::from_str_radix(hex, 16).context("bad %-escape")?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).context("invalid utf8 in url")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Identity;

    #[test]
    fn url_round_trips() {
        let id = Identity::generate();
        let p = Pairing {
            relay: "wss://relay.example:8787/ws".into(),
            session: "72829823-eef6-4ed5".into(),
            static_key: id.public(),
            secret: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let url = p.to_url();
        let back = Pairing::from_url(&url).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn rejects_wrong_scheme() {
        assert!(Pairing::from_url("https://example.com").is_err());
    }

    #[test]
    fn tolerates_unknown_params() {
        let id = Identity::generate();
        let p = Pairing {
            relay: "ws://x/ws".into(),
            session: "s".into(),
            static_key: id.public(),
            secret: vec![9],
        };
        let mut url = p.to_url();
        url.push_str("&future=whatever");
        assert_eq!(Pairing::from_url(&url).unwrap(), p);
    }
}
