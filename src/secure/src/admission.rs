//! The relay admission proof — what replaces the shared token.
//!
//! End-to-end encryption stops the relay from reading or forging traffic, but the
//! relay still must decide *who may occupy a session slot*, or a stranger could
//! squat/flood it (a denial-of-service, though they still couldn't decrypt).
//!
//! For a **public, multi-tenant relay** the relay cannot know each pairing's
//! secret. So admission is *match-based*: both peers derive the same opaque proof
//! from their shared pairing secret, and the relay simply records the first
//! party's proof for a session slot and requires the second to present a
//! byte-identical one. The relay compares two opaque blobs in constant time — it
//! never learns the secret, and a squatter without the secret cannot produce a
//! matching proof. Knowing the proof is also useless for reading the E2E channel
//! (that key comes from the X25519 handshake, entirely separate).

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Derive the admission proof for a session from the pairing secret. Both peers
/// compute the same value; the relay records the first party's and requires the
/// second to present a byte-identical one (compared in constant time).
pub fn prove(secret: &[u8], session: &str) -> Vec<u8> {
    let mut m = <HmacSha256 as Mac>::new_from_slice(secret).expect("HMAC accepts any key length");
    m.update(b"bob-remote/admission/v1");
    m.update(session.as_bytes());
    m.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"pairing-secret";

    #[test]
    fn same_secret_and_session_match() {
        assert_eq!(prove(SECRET, "sess"), prove(SECRET, "sess"));
    }

    #[test]
    fn wrong_secret_does_not_match() {
        assert_ne!(prove(SECRET, "sess"), prove(b"other", "sess"));
    }

    #[test]
    fn different_session_does_not_match() {
        assert_ne!(prove(SECRET, "sess"), prove(SECRET, "other"));
    }
}
