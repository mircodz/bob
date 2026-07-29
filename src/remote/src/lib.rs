//! bob-remote: headless agent host + a debug test-client, reachable over the
//! relay. Exposed as a library so the main `bob` binary can drive it via
//! `bob remote`.

pub mod channel;
pub mod client;
pub mod host;
pub mod session;

/// The crypto inputs a host/controller needs to establish the E2E channel:
/// this device's long-term identity secret, a fresh ephemeral, the shared
/// pairing secret (seeds the relay admission proof), and a callback invoked once
/// the handshake completes (to display/verify the safety number and persist the
/// peer's static key for trust-on-first-use).
pub struct SecureParams {
    pub identity: bob_secure::x25519::StaticSecret,
    pub ephemeral: bob_secure::x25519::StaticSecret,
    pub pairing_secret: Vec<u8>,
    /// The peer's static key, required for the initiator (controller) side and
    /// unused by the responder (host, which learns it during the handshake).
    pub peer_static: Option<bob_secure::x25519::PublicKey>,
    pub on_established: Box<dyn Fn(&channel::Established) + Send>,
}
