//! The handshake seam: runs Noise-XK over the split relay WebSocket and hands
//! back the established [`Sealer`]/[`Opener`] halves plus the sink/stream, so the
//! host's writer task and read loop each own one crypto half without sharing a
//! lock. This is the single place transport meets crypto; host.rs and client.rs
//! stay free of key handling — they seal on send and open on receive.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use bob_protocol::Envelope;
use bob_secure::x25519::{PublicKey, StaticSecret};
use bob_secure::{Initiator, Opener, Responder, Sealer};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
pub type WsSink = SplitSink<Ws, WsMessage>;
pub type WsStream = SplitStream<Ws>;

/// The prologue binds external context into the handshake transcript, so a
/// captured handshake can't be replayed against a different relay/session.
pub fn prologue(relay: &str, session: &str) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(b"bob-remote/v1\0");
    p.extend_from_slice(relay.as_bytes());
    p.push(0);
    p.extend_from_slice(session.as_bytes());
    p
}

/// The result of a completed handshake: the two crypto halves (to hand to the
/// send/receive tasks), the peer's authenticated static key (for TOFU pairing),
/// and the 4-digit safety number to display on first pair.
pub struct Established {
    pub sealer: Sealer,
    pub opener: Opener,
    pub peer_static: PublicKey,
    pub safety_number: u16,
}

/// Run the **initiator** (controller) handshake. `peer_static` is the responder's
/// known static key (from the pairing QR the first time, storage thereafter).
pub async fn handshake_initiator(
    sink: &mut WsSink,
    stream: &mut WsStream,
    prologue: &[u8],
    static_secret: StaticSecret,
    ephemeral: StaticSecret,
    peer_static: PublicKey,
) -> Result<Established> {
    let mut init = Initiator::new(prologue, static_secret, ephemeral, peer_static);

    let m1 = init.write_message_1();
    send_handshake(sink, &m1).await?;
    let m2 = expect_handshake(stream).await?;
    init.read_message_2(&m2)
        .map_err(|_| anyhow!("handshake failed at message 2"))?;
    let (m3, completed) = init.finish();
    send_handshake(sink, &m3).await?;

    Ok(established(completed))
}

/// Run the **responder** (host) handshake. Learns + authenticates the initiator's
/// static key (returned for TOFU pairing).
pub async fn handshake_responder(
    sink: &mut WsSink,
    stream: &mut WsStream,
    prologue: &[u8],
    static_secret: StaticSecret,
    ephemeral: StaticSecret,
) -> Result<Established> {
    let mut resp = Responder::new(prologue, static_secret, ephemeral);

    let m1 = expect_handshake(stream).await?;
    resp.read_message_1(&m1)
        .map_err(|_| anyhow!("handshake failed at message 1"))?;
    let m2 = resp.write_message_2();
    send_handshake(sink, &m2).await?;
    let m3 = expect_handshake(stream).await?;
    let completed = resp
        .read_message_3(&m3)
        .map_err(|_| anyhow!("handshake failed at message 3"))?;

    Ok(established(completed))
}

fn established(completed: bob_secure::Completed) -> Established {
    let safety_number = bob_secure::short_auth_string(&completed.handshake_hash);
    let peer_static = completed.peer_static;
    let (sealer, opener) = completed.session.into_halves();
    Established {
        sealer,
        opener,
        peer_static,
        safety_number,
    }
}

// --- envelope helpers ------------------------------------------------------

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}
pub fn unb64(s: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(s)
        .context("invalid base64 envelope")
}

async fn send_handshake(sink: &mut WsSink, msg: &[u8]) -> Result<()> {
    let env = Envelope::Handshake { data: b64(msg) };
    sink.send(WsMessage::Text(serde_json::to_string(&env)?))
        .await?;
    Ok(())
}

async fn expect_handshake(stream: &mut WsStream) -> Result<Vec<u8>> {
    loop {
        match stream.next().await {
            Some(Ok(WsMessage::Text(t))) => {
                // Surface a relay rejection ({"error":...}) with its real reason.
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    if let Some(reason) = v.get("error").and_then(|m| m.as_str()) {
                        return Err(anyhow!("relay rejected the connection: {reason}"));
                    }
                }
                match serde_json::from_str::<Envelope>(&t).context("bad envelope")? {
                    Envelope::Handshake { data } => return unb64(&data),
                    Envelope::Sealed { .. } => {
                        return Err(anyhow!("expected handshake, got sealed frame"))
                    }
                }
            }
            Some(Ok(WsMessage::Close(_))) | None => return Err(anyhow!("connection closed")),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(e.into()),
        }
    }
}

/// Encode a sealed application frame for the wire.
pub fn seal_envelope(sealer: &mut Sealer, plaintext: &[u8]) -> Result<String> {
    let sealed = sealer.seal(plaintext).map_err(|e| anyhow!(e))?;
    let env = Envelope::Sealed { data: b64(&sealed) };
    Ok(serde_json::to_string(&env)?)
}

/// Decode + open a sealed application frame received from the wire. Returns the
/// plaintext bytes, or an error if it isn't a valid sealed frame.
pub fn open_envelope(opener: &mut Opener, text: &str) -> Result<Vec<u8>> {
    match serde_json::from_str::<Envelope>(text).context("bad envelope")? {
        Envelope::Sealed { data } => opener
            .open(&unb64(&data)?)
            .map_err(|_| anyhow!("frame failed to decrypt")),
        Envelope::Handshake { .. } => Err(anyhow!("unexpected handshake frame post-handshake")),
    }
}
