//! The Noise-XK handshake, transcribed from the Noise Protocol Framework spec
//! (rev 34) for the concrete ciphersuite `Noise_XK_25519_ChaChaPoly_SHA256`.
//!
//! We implement the spec's state machine by hand rather than pulling a Noise
//! library, because the iOS side has no trustworthy Noise implementation and must
//! mirror this exactly on CryptoKit. To keep that mirror faithful — and to keep
//! this auditable — the code follows the spec's structure verbatim:
//! `CipherState` → `SymmetricState` → `HandshakeState`, with each handshake
//! token (`e`, `es`, `s`, `se`, `ee`) applied in spec order and commented.
//!
//! ## XK, in one paragraph
//! Roles: **initiator** = phone/controller, **responder** = laptop/host. `XK`
//! means the responder's static public key is **K**nown to the initiator up front
//! (from the pairing QR the first time, from storage thereafter), while the
//! initiator's static is transmitted (**X**) encrypted during the handshake.
//! Three messages: `-> e, es` / `<- e, ee` / `-> s, se`. The result is mutual
//! authentication + forward secrecy (fresh ephemerals) + responder identity
//! hiding. `Split()` yields the two directional keys used by [`crate::Session`].
//!
//! ## Transcript binding
//! Everything exchanged is folded into the running hash `h`, seeded from the
//! protocol name and a caller-supplied `prologue` (we bind the relay URL + session
//! id there). If any byte is altered in flight, the two peers derive different
//! keys and the first encrypted handshake payload fails to authenticate — which
//! is what defeats downgrade / unknown-key-share / transcript tampering.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::session::{Role, Session};

/// The Noise protocol name for our fixed ciphersuite. Seeds the transcript hash.
const PROTOCOL_NAME: &[u8] = b"Noise_XK_25519_ChaChaPoly_SHA256";

/// Handshake failure. Coarse on purpose — callers shouldn't branch on *why*.
#[derive(Debug, PartialEq, Eq)]
pub enum HandshakeError {
    /// A handshake message failed to authenticate or was malformed. Usually means
    /// a wrong key, a tampered transcript, or an impostor.
    Failed,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "handshake failed")
    }
}
impl std::error::Error for HandshakeError {}

// ===========================================================================
// CipherState (spec §5.1) — an AEAD key + nonce counter used *within* the
// handshake to encrypt payloads/static keys as `h` accrues authentication.
// ===========================================================================

#[derive(Default)]
struct CipherState {
    key: Option<[u8; 32]>,
    nonce: u64,
}

impl CipherState {
    fn initialize_key(&mut self, key: Option<[u8; 32]>) {
        self.key = key;
        self.nonce = 0;
    }

    /// Noise nonce encoding: 4 zero bytes then the 64-bit counter little-endian.
    fn nonce_bytes(&self) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.nonce.to_le_bytes());
        n
    }

    /// `EncryptWithAd`: if keyed, AEAD-encrypt with `ad`; else passthrough.
    fn encrypt_with_ad(&mut self, ad: &[u8], plaintext: &[u8]) -> Vec<u8> {
        match self.key {
            None => plaintext.to_vec(),
            Some(k) => {
                let cipher = ChaCha20Poly1305::new(Key::from_slice(&k));
                let ct = cipher
                    .encrypt(
                        Nonce::from_slice(&self.nonce_bytes()),
                        Payload {
                            msg: plaintext,
                            aad: ad,
                        },
                    )
                    .expect("aead encrypt is infallible for valid key/nonce");
                self.nonce += 1;
                ct
            }
        }
    }

    /// `DecryptWithAd`: inverse of [`encrypt_with_ad`](Self::encrypt_with_ad).
    fn decrypt_with_ad(&mut self, ad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, HandshakeError> {
        match self.key {
            None => Ok(ciphertext.to_vec()),
            Some(k) => {
                let cipher = ChaCha20Poly1305::new(Key::from_slice(&k));
                let pt = cipher
                    .decrypt(
                        Nonce::from_slice(&self.nonce_bytes()),
                        Payload {
                            msg: ciphertext,
                            aad: ad,
                        },
                    )
                    .map_err(|_| HandshakeError::Failed)?;
                self.nonce += 1;
                Ok(pt)
            }
        }
    }
}

// ===========================================================================
// SymmetricState (spec §5.2) — the chaining key `ck`, transcript hash `h`, and
// the in-handshake CipherState. Mixes DH outputs and hashes every message.
// ===========================================================================

struct SymmetricState {
    ck: [u8; 32],
    h: [u8; 32],
    cipher: CipherState,
}

impl SymmetricState {
    /// `InitializeSymmetric(protocol_name)`.
    fn new() -> Self {
        // protocol_name <= 32 bytes → h = protocol_name zero-padded.
        let mut h = [0u8; 32];
        h[..PROTOCOL_NAME.len()].copy_from_slice(PROTOCOL_NAME);
        SymmetricState {
            ck: h,
            h,
            cipher: CipherState::default(),
        }
    }

    /// `MixHash(data)` → h = SHA256(h || data).
    fn mix_hash(&mut self, data: &[u8]) {
        let mut hasher = Sha256::new();
        hasher.update(self.h);
        hasher.update(data);
        self.h.copy_from_slice(&hasher.finalize());
    }

    /// `MixKey(input_key_material)` → (ck, temp_k) = HKDF(ck, ikm, 2); key cipher.
    fn mix_key(&mut self, ikm: &[u8]) {
        let (ck, k) = hkdf2(&self.ck, ikm);
        self.ck = ck;
        self.cipher.initialize_key(Some(k));
    }

    /// `EncryptAndHash(plaintext)` → ct = EncryptWithAd(h, pt); MixHash(ct).
    fn encrypt_and_hash(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let ct = self.cipher.encrypt_with_ad(&self.h, plaintext);
        self.mix_hash(&ct);
        ct
    }

    /// `DecryptAndHash(ciphertext)` → pt = DecryptWithAd(h, ct); MixHash(ct).
    fn decrypt_and_hash(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, HandshakeError> {
        let pt = self.cipher.decrypt_with_ad(&self.h, ciphertext)?;
        self.mix_hash(ciphertext);
        Ok(pt)
    }

    /// `Split()` → the two directional keys `(k1, k2)` for the transport phase.
    /// k1 = initiator→responder, k2 = responder→initiator (canonical order).
    fn split(&self) -> ([u8; 32], [u8; 32]) {
        hkdf2(&self.ck, &[])
    }
}

/// HKDF-SHA256 with two 32-byte outputs, per Noise's `HKDF(ck, ikm, 2)`.
/// `ck` is the salt; the two outputs are `T1` and `T2`.
fn hkdf2(ck: &[u8; 32], ikm: &[u8]) -> ([u8; 32], [u8; 32]) {
    let hk = Hkdf::<Sha256>::new(Some(ck), ikm);
    let mut okm = [0u8; 64];
    hk.expand(&[], &mut okm).expect("64 <= 255*32");
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    a.copy_from_slice(&okm[..32]);
    b.copy_from_slice(&okm[32..]);
    (a, b)
}

/// Raw X25519: returns the 32-byte shared secret between a secret and a public.
fn dh(secret: &StaticSecret, public: &PublicKey) -> [u8; 32] {
    secret.diffie_hellman(public).to_bytes()
}

// ===========================================================================
// HandshakeState (spec §5.3) — drives the XK message pattern.
// ===========================================================================

/// The initiator half of an XK handshake (bob's phone / controller). It already
/// knows the responder's static public key (`rs`), per the XK "K".
pub struct Initiator {
    sym: SymmetricState,
    s: StaticSecret,       // our static identity
    e: StaticSecret,       // our ephemeral (fresh)
    rs: PublicKey,         // responder static (known ahead)
    re: Option<PublicKey>, // responder ephemeral (learned in msg 2)
}

/// The responder half of an XK handshake (bob's laptop / host).
pub struct Responder {
    sym: SymmetricState,
    s: StaticSecret,       // our static identity
    e: StaticSecret,       // our ephemeral (fresh)
    rs: Option<PublicKey>, // initiator static (learned in msg 3)
    re: Option<PublicKey>, // initiator ephemeral (learned in msg 1)
}

/// A completed handshake: the transport [`Session`] plus the peer's authenticated
/// static public key (for TOFU pairing) and the derived handshake hash `h` (for
/// the short authentication string).
pub struct Completed {
    pub session: Session,
    pub peer_static: PublicKey,
    /// Final transcript hash — feed to [`short_auth_string`] for the SAS.
    pub handshake_hash: [u8; 32],
}

impl Initiator {
    /// Begin the handshake. `prologue` binds external context (we pass
    /// `"bob-remote/v1" || relay || session`). `static_secret` is our identity;
    /// `responder_static` is the peer key from the QR/store.
    ///
    /// Pre-message (XK): the responder's static is hashed into `h` on both sides
    /// before message 1, which is what "K"nown-responder means cryptographically.
    pub fn new(
        prologue: &[u8],
        static_secret: StaticSecret,
        ephemeral: StaticSecret,
        responder_static: PublicKey,
    ) -> Self {
        let mut sym = SymmetricState::new();
        sym.mix_hash(prologue);
        // Pre-message: responder static (rs) is known → MixHash(rs).
        sym.mix_hash(responder_static.as_bytes());
        Initiator {
            sym,
            s: static_secret,
            e: ephemeral,
            rs: responder_static,
            re: None,
        }
    }

    /// Produce message 1: `-> e, es` plus an (empty) encrypted payload.
    pub fn write_message_1(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        // token `e`: send our ephemeral public; MixHash(e.public).
        let epub = PublicKey::from(&self.e);
        out.extend_from_slice(epub.as_bytes());
        self.sym.mix_hash(epub.as_bytes());
        // token `es`: MixKey(DH(e, rs)).
        self.sym.mix_key(&dh(&self.e, &self.rs));
        // empty payload, encrypted+hashed (keyed after `es`).
        out.extend_from_slice(&self.sym.encrypt_and_hash(&[]));
        out
    }

    /// Consume message 2: `<- e, ee`. Returns nothing on success (state advances).
    pub fn read_message_2(&mut self, msg: &[u8]) -> Result<(), HandshakeError> {
        // token `e`: read responder ephemeral (32 bytes) then MixHash.
        let (re_bytes, rest) = split_pubkey(msg)?;
        let re = PublicKey::from(re_bytes);
        self.sym.mix_hash(re.as_bytes());
        self.re = Some(re);
        // token `ee`: MixKey(DH(e, re)).
        self.sym.mix_key(&dh(&self.e, &re));
        // trailing encrypted (empty) payload.
        self.sym.decrypt_and_hash(rest)?;
        Ok(())
    }

    /// Produce message 3 and finish: `-> s, se`. Returns the message-3 bytes the
    /// caller must transmit, together with the completed state (transport session,
    /// the responder's authenticated static key, and the transcript hash).
    pub fn finish(mut self) -> (Vec<u8>, Completed) {
        let spub = PublicKey::from(&self.s);
        let mut out = self.sym.encrypt_and_hash(spub.as_bytes());
        let re = self.re.expect("re set by read_message_2");
        self.sym.mix_key(&dh(&self.s, &re));
        out.extend_from_slice(&self.sym.encrypt_and_hash(&[]));

        let (k1, k2) = self.sym.split();
        let completed = Completed {
            session: Session::new(k1, k2, Role::Initiator),
            peer_static: self.rs,
            handshake_hash: self.sym.h,
        };
        (out, completed)
    }
}

impl Responder {
    /// Prepare to respond. `static_secret` is our identity (whose public the
    /// initiator already knows); `prologue` must match the initiator's exactly.
    pub fn new(prologue: &[u8], static_secret: StaticSecret, ephemeral: StaticSecret) -> Self {
        let mut sym = SymmetricState::new();
        sym.mix_hash(prologue);
        // Pre-message: our own static is the "known responder static".
        let spub = PublicKey::from(&static_secret);
        sym.mix_hash(spub.as_bytes());
        Responder {
            sym,
            s: static_secret,
            e: ephemeral,
            rs: None,
            re: None,
        }
    }

    /// Consume message 1: `-> e, es`.
    pub fn read_message_1(&mut self, msg: &[u8]) -> Result<(), HandshakeError> {
        // token `e`: read initiator ephemeral.
        let (re_bytes, rest) = split_pubkey(msg)?;
        let re = PublicKey::from(re_bytes);
        self.sym.mix_hash(re.as_bytes());
        self.re = Some(re);
        // token `es`: MixKey(DH(s, re)) — from the responder's side, `es` uses our
        // static and the initiator's ephemeral.
        self.sym.mix_key(&dh(&self.s, &re));
        self.sym.decrypt_and_hash(rest)?;
        Ok(())
    }

    /// Produce message 2: `<- e, ee`.
    pub fn write_message_2(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        // token `e`: our ephemeral public.
        let epub = PublicKey::from(&self.e);
        out.extend_from_slice(epub.as_bytes());
        self.sym.mix_hash(epub.as_bytes());
        // token `ee`: MixKey(DH(e, re)).
        let re = self.re.expect("re set by read_message_1");
        self.sym.mix_key(&dh(&self.e, &re));
        out.extend_from_slice(&self.sym.encrypt_and_hash(&[]));
        out
    }

    /// Consume message 3: `-> s, se`. Authenticates the initiator's static key and
    /// finishes, returning the transport session + the peer's static (for TOFU).
    pub fn read_message_3(mut self, msg: &[u8]) -> Result<Completed, HandshakeError> {
        // token `s`: decrypt the initiator's static public key.
        // The encrypted static is 32 bytes + 16-byte tag = 48 bytes.
        if msg.len() < 48 {
            return Err(HandshakeError::Failed);
        }
        let (enc_s, rest) = msg.split_at(48);
        let s_bytes = self.sym.decrypt_and_hash(enc_s)?;
        let rs = PublicKey::from(
            <[u8; 32]>::try_from(s_bytes.as_slice()).map_err(|_| HandshakeError::Failed)?,
        );
        self.rs = Some(rs);
        // token `se`: MixKey(DH(e, rs)) — responder side of `se`.
        self.sym.mix_key(&dh(&self.e, &rs));
        // trailing empty payload — authenticating the whole transcript.
        self.sym.decrypt_and_hash(rest)?;

        let (k1, k2) = self.sym.split();
        Ok(Completed {
            session: Session::new(k1, k2, Role::Responder),
            peer_static: rs,
            handshake_hash: self.sym.h,
        })
    }
}

/// Split a 32-byte X25519 public key off the front of a message.
fn split_pubkey(msg: &[u8]) -> Result<([u8; 32], &[u8]), HandshakeError> {
    if msg.len() < 32 {
        return Err(HandshakeError::Failed);
    }
    let (head, rest) = msg.split_at(32);
    Ok((head.try_into().expect("checked len 32"), rest))
}

/// Derive the 4-digit short authentication string from the final handshake hash.
/// Both peers display it after the first pairing so the user can confirm no
/// active MITM ("safety number"). Derived, not transmitted.
pub fn short_auth_string(handshake_hash: &[u8; 32]) -> u16 {
    let hk = Hkdf::<Sha256>::new(Some(handshake_hash), b"bob-remote/sas");
    let mut okm = [0u8; 2];
    hk.expand(&[], &mut okm).expect("2 <= 255*32");
    (u16::from_be_bytes(okm)) % 10_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    fn key() -> StaticSecret {
        StaticSecret::random_from_rng(OsRng)
    }

    /// Drive a full XK handshake between two peers and return both completions.
    fn run(prologue: &[u8], is: StaticSecret, rs: StaticSecret) -> (Completed, Completed) {
        let rs_pub = PublicKey::from(&rs);
        let mut initiator = Initiator::new(prologue, is, key(), rs_pub);
        let mut responder = Responder::new(prologue, rs, key());

        let m1 = initiator.write_message_1();
        responder.read_message_1(&m1).unwrap();
        let m2 = responder.write_message_2();
        initiator.read_message_2(&m2).unwrap();
        let (m3, ic) = initiator.finish();
        let rc = responder.read_message_3(&m3).unwrap();
        (ic, rc)
    }

    #[test]
    fn completes_and_derives_matching_keys() {
        let (mut ic, mut rc) = run(b"prologue", key(), key());
        // Transcript hashes must match on both sides.
        assert_eq!(ic.handshake_hash, rc.handshake_hash);
        // And the derived transport sessions must interoperate both ways.
        let sealed = ic.session.seal(b"ping").unwrap();
        assert_eq!(rc.session.open(&sealed).unwrap(), b"ping");
        let back = rc.session.seal(b"pong").unwrap();
        assert_eq!(ic.session.open(&back).unwrap(), b"pong");
    }

    #[test]
    fn each_side_learns_the_peer_static() {
        let is = key();
        let rs = key();
        let is_pub = PublicKey::from(&is);
        let rs_pub = PublicKey::from(&rs);
        let (ic, rc) = run(b"p", is, rs);
        // Initiator authenticated the responder's static (it knew it up front).
        assert_eq!(ic.peer_static.as_bytes(), rs_pub.as_bytes());
        // Responder learned + authenticated the initiator's static (for TOFU).
        assert_eq!(rc.peer_static.as_bytes(), is_pub.as_bytes());
    }

    #[test]
    fn sas_matches_and_is_four_digits() {
        let (ic, rc) = run(b"p", key(), key());
        let a = short_auth_string(&ic.handshake_hash);
        let b = short_auth_string(&rc.handshake_hash);
        assert_eq!(a, b);
        assert!(a < 10_000);
    }

    #[test]
    fn prologue_mismatch_breaks_handshake() {
        let is = key();
        let rs = key();
        let rs_pub = PublicKey::from(&rs);
        let mut initiator = Initiator::new(b"prologue-A", is, key(), rs_pub);
        let mut responder = Responder::new(b"prologue-B", rs, key());
        let m1 = initiator.write_message_1();
        // Responder's `h` differs (different prologue) → the encrypted payload in
        // message 1 fails to authenticate.
        assert_eq!(responder.read_message_1(&m1), Err(HandshakeError::Failed));
    }

    #[test]
    fn wrong_responder_static_breaks_handshake() {
        // Initiator believes the responder has key X, but the real responder holds
        // Y. `es` diverges → message 1 payload fails to open.
        let wrong = PublicKey::from(&key());
        let real = key();
        let mut initiator = Initiator::new(b"p", key(), key(), wrong);
        let mut responder = Responder::new(b"p", real, key());
        let m1 = initiator.write_message_1();
        assert_eq!(responder.read_message_1(&m1), Err(HandshakeError::Failed));
    }

    #[test]
    fn tampered_message_2_breaks_handshake() {
        let rs = key();
        let rs_pub = PublicKey::from(&rs);
        let mut initiator = Initiator::new(b"p", key(), key(), rs_pub);
        let mut responder = Responder::new(b"p", rs, key());
        let m1 = initiator.write_message_1();
        responder.read_message_1(&m1).unwrap();
        let mut m2 = responder.write_message_2();
        let last = m2.len() - 1;
        m2[last] ^= 0x01;
        assert_eq!(initiator.read_message_2(&m2), Err(HandshakeError::Failed));
    }

    #[test]
    fn tampered_static_in_message_3_breaks_handshake() {
        let rs = key();
        let rs_pub = PublicKey::from(&rs);
        let mut initiator = Initiator::new(b"p", key(), key(), rs_pub);
        let mut responder = Responder::new(b"p", rs, key());
        let m1 = initiator.write_message_1();
        responder.read_message_1(&m1).unwrap();
        let m2 = responder.write_message_2();
        initiator.read_message_2(&m2).unwrap();
        let (mut m3, _ic) = initiator.finish();
        m3[0] ^= 0x01; // corrupt the encrypted static
        assert!(responder.read_message_3(&m3).is_err());
    }
}
