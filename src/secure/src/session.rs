//! The AEAD envelope: a [`Session`] holds the two directional keys produced by a
//! completed handshake and seals/opens application frames with them.
//!
//! Design:
//!   - Two independent keys, one per direction (`send`/`recv`), so the two peers
//!     never reuse a key/nonce pair. Each peer swaps which is which via [`Role`].
//!   - Each direction has a monotonic 64-bit counter. The counter IS the nonce
//!     (zero-padded to 96 bits) AND is bound into the AEAD as associated data, so
//!     any reorder/replay/truncation is rejected on `open`.
//!   - ChaCha20-Poly1305: a nonce must never repeat under one key. The counter is
//!     strictly increasing and we refuse to wrap, which guarantees uniqueness.
//!
//! This type performs no I/O and has no knowledge of the transport.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

/// Which end of the connection this session belongs to. Determines which of the
/// two handshake-derived keys is used for sending vs receiving, so both peers
/// agree without extra negotiation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The connection initiator (the phone / controller in bob's topology).
    Initiator,
    /// The connection responder (the laptop / host).
    Responder,
}

/// Errors from sealing/opening. Kept coarse on purpose: a caller must not be able
/// to distinguish *why* an open failed (that would leak an oracle), so every
/// authentication/format failure collapses to [`SessionError::Decrypt`].
#[derive(Debug, PartialEq, Eq)]
pub enum SessionError {
    /// The frame failed authentication, was malformed, or arrived out of order.
    Decrypt,
    /// The 64-bit message counter is exhausted. Practically unreachable
    /// (2^64 messages); we refuse to wrap rather than risk nonce reuse.
    CounterExhausted,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Decrypt => write!(f, "frame failed to decrypt/authenticate"),
            SessionError::CounterExhausted => write!(f, "message counter exhausted"),
        }
    }
}

impl std::error::Error for SessionError {}

/// A live encrypted channel. Construct via [`Session::new`] with the two keys the
/// handshake's `Split()` produced (in a fixed order) plus this peer's [`Role`].
pub struct Session {
    /// Cipher for frames we send.
    send: ChaCha20Poly1305,
    /// Cipher for frames we receive.
    recv: ChaCha20Poly1305,
    /// Next nonce counter for sending (strictly increasing).
    send_ctr: u64,
    /// Next expected nonce counter for receiving (strictly increasing).
    recv_ctr: u64,
}

impl Session {
    /// Build a session from the handshake output.
    ///
    /// `k1`/`k2` are the two 32-byte keys from `Split()` in a canonical order
    /// (k1 = initiator→responder, k2 = responder→initiator). `role` selects which
    /// is "send" for this peer, so both ends line up automatically.
    pub fn new(k1: [u8; 32], k2: [u8; 32], role: Role) -> Self {
        let (send_key, recv_key) = match role {
            Role::Initiator => (k1, k2),
            Role::Responder => (k2, k1),
        };
        Session {
            send: ChaCha20Poly1305::new(Key::from_slice(&send_key)),
            recv: ChaCha20Poly1305::new(Key::from_slice(&recv_key)),
            send_ctr: 0,
            recv_ctr: 0,
        }
    }

    /// Split the session into independently-owned [`Sealer`] and [`Opener`]
    /// halves, so the sending and receiving sides can live on separate tasks
    /// without sharing a lock. Each half owns its own key + counter.
    pub fn into_halves(self) -> (Sealer, Opener) {
        (
            Sealer {
                cipher: self.send,
                ctr: self.send_ctr,
            },
            Opener {
                cipher: self.recv,
                ctr: self.recv_ctr,
            },
        )
    }

    /// Seal one application frame. Returns `counter(8 bytes) || ciphertext`, where
    /// the counter is transmitted so the peer can reconstruct the nonce. The
    /// counter is also the AEAD associated data, binding it to the ciphertext.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, SessionError> {
        let ctr = self.send_ctr;
        self.send_ctr = ctr.checked_add(1).ok_or(SessionError::CounterExhausted)?;

        let nonce_bytes = nonce_from_counter(ctr);
        let aad = ctr.to_be_bytes();
        let ct = self
            .send
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload {
                    msg: plaintext,
                    aad: &aad,
                },
            )
            .map_err(|_| SessionError::Decrypt)?;

        let mut out = Vec::with_capacity(8 + ct.len());
        out.extend_from_slice(&aad);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Open one frame produced by the peer's [`seal`](Self::seal). Enforces that
    /// the embedded counter matches the next expected value — so replays,
    /// reorders, and drops are rejected, not just tampering.
    pub fn open(&mut self, frame: &[u8]) -> Result<Vec<u8>, SessionError> {
        if frame.len() < 8 {
            return Err(SessionError::Decrypt);
        }
        let (ctr_bytes, ct) = frame.split_at(8);
        let ctr = u64::from_be_bytes(ctr_bytes.try_into().expect("checked len 8"));

        // Strict in-order delivery: the transport is a single ordered WebSocket,
        // so we require exactly the next counter. This rejects replay and reorder
        // without a sliding window.
        if ctr != self.recv_ctr {
            return Err(SessionError::Decrypt);
        }

        let nonce_bytes = nonce_from_counter(ctr);
        let aad = ctr.to_be_bytes();
        let pt = self
            .recv
            .decrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload { msg: ct, aad: &aad },
            )
            .map_err(|_| SessionError::Decrypt)?;

        // Only advance after successful authentication, so a forged frame can't
        // desync the counter.
        self.recv_ctr = ctr.checked_add(1).ok_or(SessionError::CounterExhausted)?;
        Ok(pt)
    }
}

/// Expand a 64-bit counter into a 96-bit ChaCha20-Poly1305 nonce: 4 zero bytes
/// followed by the big-endian counter. Unique per key because the counter never
/// repeats.
fn nonce_from_counter(ctr: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&ctr.to_be_bytes());
    n
}

/// The send half of a [`Session`], owned independently (e.g. by a writer task).
pub struct Sealer {
    cipher: ChaCha20Poly1305,
    ctr: u64,
}

impl Sealer {
    /// Seal one frame; see [`Session::seal`] for the wire format.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, SessionError> {
        let ctr = self.ctr;
        self.ctr = ctr.checked_add(1).ok_or(SessionError::CounterExhausted)?;
        let ct = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce_from_counter(ctr)),
                Payload {
                    msg: plaintext,
                    aad: &ctr.to_be_bytes(),
                },
            )
            .map_err(|_| SessionError::Decrypt)?;
        let mut out = Vec::with_capacity(8 + ct.len());
        out.extend_from_slice(&ctr.to_be_bytes());
        out.extend_from_slice(&ct);
        Ok(out)
    }
}

/// The receive half of a [`Session`], owned independently (e.g. by a read loop).
pub struct Opener {
    cipher: ChaCha20Poly1305,
    ctr: u64,
}

impl Opener {
    /// Open one frame; see [`Session::open`] for the ordering guarantees.
    pub fn open(&mut self, frame: &[u8]) -> Result<Vec<u8>, SessionError> {
        if frame.len() < 8 {
            return Err(SessionError::Decrypt);
        }
        let (ctr_bytes, ct) = frame.split_at(8);
        let ctr = u64::from_be_bytes(ctr_bytes.try_into().expect("checked len 8"));
        if ctr != self.ctr {
            return Err(SessionError::Decrypt);
        }
        let pt = self
            .cipher
            .decrypt(
                Nonce::from_slice(&nonce_from_counter(ctr)),
                Payload {
                    msg: ct,
                    aad: &ctr.to_be_bytes(),
                },
            )
            .map_err(|_| SessionError::Decrypt)?;
        self.ctr = ctr.checked_add(1).ok_or(SessionError::CounterExhausted)?;
        Ok(pt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Session, Session) {
        let k1 = [7u8; 32];
        let k2 = [42u8; 32];
        (
            Session::new(k1, k2, Role::Initiator),
            Session::new(k1, k2, Role::Responder),
        )
    }

    #[test]
    fn round_trip_both_directions() {
        let (mut init, mut resp) = pair();
        let a = init.seal(b"hello from initiator").unwrap();
        assert_eq!(resp.open(&a).unwrap(), b"hello from initiator");
        let b = resp.seal(b"hi back from responder").unwrap();
        assert_eq!(init.open(&b).unwrap(), b"hi back from responder");
    }

    #[test]
    fn ordered_stream() {
        let (mut init, mut resp) = pair();
        for i in 0..100u32 {
            let msg = format!("frame {i}");
            let sealed = init.seal(msg.as_bytes()).unwrap();
            assert_eq!(resp.open(&sealed).unwrap(), msg.as_bytes());
        }
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let (mut init, mut resp) = pair();
        let mut sealed = init.seal(b"secret").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01; // flip a bit in the tag
        assert_eq!(resp.open(&sealed), Err(SessionError::Decrypt));
    }

    #[test]
    fn replay_is_rejected() {
        let (mut init, mut resp) = pair();
        let f0 = init.seal(b"one").unwrap();
        let f1 = init.seal(b"two").unwrap();
        assert_eq!(resp.open(&f0).unwrap(), b"one");
        assert_eq!(resp.open(&f1).unwrap(), b"two");
        // Replaying f0 now (counter already consumed) must fail.
        assert_eq!(resp.open(&f0), Err(SessionError::Decrypt));
    }

    #[test]
    fn reorder_is_rejected() {
        let (mut init, mut resp) = pair();
        let f0 = init.seal(b"one").unwrap();
        let f1 = init.seal(b"two").unwrap();
        // Deliver out of order: f1 before f0.
        assert_eq!(resp.open(&f1), Err(SessionError::Decrypt));
        // The counter didn't advance on the failed open, so f0 still works.
        assert_eq!(resp.open(&f0).unwrap(), b"one");
    }

    #[test]
    fn wrong_key_cannot_open() {
        let mut sender = Session::new([1u8; 32], [2u8; 32], Role::Initiator);
        let mut wrong = Session::new([9u8; 32], [8u8; 32], Role::Responder);
        let sealed = sender.seal(b"nope").unwrap();
        assert_eq!(wrong.open(&sealed), Err(SessionError::Decrypt));
    }

    #[test]
    fn truncated_frame_is_rejected() {
        let (mut init, mut resp) = pair();
        let sealed = init.seal(b"data").unwrap();
        assert_eq!(resp.open(&sealed[..4]), Err(SessionError::Decrypt));
    }
}
