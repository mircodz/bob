//! End-to-end secure-channel test: stand up the real relay, run the Noise-XK
//! handshake between an initiator and responder through it, and assert that
//! sealed frames round-trip while the relay (which only sees ciphertext) forwards
//! them blind, and that both sides derive the same safety number. No LLM needed.

use base64::Engine as _;
use bob_protocol::Hello;
use bob_remote::channel::{
    handshake_initiator, handshake_responder, open_envelope, prologue, seal_envelope, WsSink,
    WsStream,
};
use bob_secure::{admission, Identity};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

async fn start_relay() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, bob_relay::router()).await.unwrap();
    });
    format!("ws://{addr}/ws")
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes)
}

/// Connect to the relay and send the role Hello with the admission proof.
async fn join(url: &str, hello: &Hello) -> (WsSink, WsStream) {
    let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    let (mut sink, stream) = ws.split();
    sink.send(WsMessage::Text(serde_json::to_string(hello).unwrap()))
        .await
        .unwrap();
    (sink, stream)
}

async fn next_text(stream: &mut WsStream) -> String {
    loop {
        if let Some(Ok(WsMessage::Text(t))) = stream.next().await {
            return t;
        }
    }
}

#[tokio::test]
async fn secure_channel_round_trips_through_relay() {
    let url = start_relay().await;
    let session = "sess-1";
    let secret = b"shared-pairing-secret";
    let proof = b64(&admission::prove(secret, session));

    // The host's identity is the "known responder static" the phone pairs against.
    let host_id = Identity::generate();
    let host_static = host_id.public();
    let host_secret = host_id.secret();

    // --- Responder (host): joins first, occupies the room. ---
    let host_url = url.clone();
    let host_proof = proof.clone();
    let host = tokio::spawn(async move {
        let (mut sink, mut stream) = join(
            &host_url,
            &Hello::Host {
                session: session.into(),
                admission: host_proof,
            },
        )
        .await;
        let pro = prologue(&host_url, session);
        let mut est = handshake_responder(
            &mut sink,
            &mut stream,
            &pro,
            host_secret,
            Identity::generate().secret(),
        )
        .await
        .unwrap();

        let opened = open_envelope(&mut est.opener, &next_text(&mut stream).await).unwrap();
        assert_eq!(opened, b"hello from phone");
        let reply = seal_envelope(&mut est.sealer, b"hello from host").unwrap();
        sink.send(WsMessage::Text(reply)).await.unwrap();
        (est.safety_number, est.peer_static)
    });

    // Let the responder register before the initiator joins.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // --- Initiator (phone). ---
    let phone_id = Identity::generate();
    let phone_static = phone_id.public();
    let (mut sink, mut stream) = join(
        &url,
        &Hello::Controller {
            session: session.into(),
            admission: proof,
        },
    )
    .await;
    let pro = prologue(&url, session);
    let mut est = handshake_initiator(
        &mut sink,
        &mut stream,
        &pro,
        phone_id.secret(),
        Identity::generate().secret(),
        host_static,
    )
    .await
    .unwrap();

    let msg = seal_envelope(&mut est.sealer, b"hello from phone").unwrap();
    sink.send(WsMessage::Text(msg)).await.unwrap();
    let opened = open_envelope(&mut est.opener, &next_text(&mut stream).await).unwrap();
    assert_eq!(opened, b"hello from host");

    let (host_sas, host_saw_peer) = host.await.unwrap();
    // Both sides derive the same safety number from the same transcript.
    assert_eq!(host_sas, est.safety_number);
    // The host authenticated the phone's real static key (for TOFU pairing).
    assert_eq!(host_saw_peer.as_bytes(), phone_static.as_bytes());
    // The phone authenticated the host's known static key.
    assert_eq!(est.peer_static.as_bytes(), host_static.as_bytes());
}
