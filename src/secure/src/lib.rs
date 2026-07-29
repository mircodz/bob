//! bob-secure: the end-to-end encryption + device-pairing brain for bob remote
//! control. It is deliberately transport-agnostic and knows nothing about
//! WebSockets, bob-core, or the wire protocol's JSON — callers hand it bytes and
//! receive bytes. The iOS app mirrors this module 1:1 on Apple CryptoKit, so the
//! two interoperate against the shared test vectors.
//!
//! Layout:
//!   - [`session`]: the AEAD envelope. Once a handshake completes, a [`Session`]
//!     seals/opens application frames with directional keys + replay counters.
//!   - `handshake` (next): the Noise-XK handshake that establishes a `Session`.
//!   - `identity` / `admission` / `pairing` (next): long-term keys, the relay
//!     admission proof, and the QR pairing payload.
//!
//! Security note: we only ever *wire together* audited primitives (X25519,
//! HKDF-SHA256, ChaCha20-Poly1305, HMAC-SHA256). We never invent a primitive,
//! and the handshake transcribes the formally-analyzed Noise XK pattern.

pub mod admission;
pub mod handshake;
pub mod identity;
pub mod pairing;
pub mod session;

pub use handshake::{short_auth_string, Completed, HandshakeError, Initiator, Responder};
pub use identity::{Device, DeviceBook, Identity};
pub use pairing::Pairing;
pub use session::{Opener, Role, Sealer, Session, SessionError};

/// Re-exported X25519 key types, so callers wire handshakes without depending on
/// `x25519-dalek` directly (keeps the primitive choice centralized here).
pub mod x25519 {
    pub use x25519_dalek::{PublicKey, StaticSecret};
}
