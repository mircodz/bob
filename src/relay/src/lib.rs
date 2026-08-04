//! bob-relay: a dumb public WebSocket relay. Both a host and a controller
//! (iOS app / test-client) dial *out* to this server — solving the NAT problem
//! where the phone can't reach the dev machine directly.
//!
//! Each connection's first WS text frame is a `Hello` identifying role + session
//! + an opaque **admission proof**. The relay pairs one host with one controller
//! per `session`: it records the first peer's proof and admits the second only if
//! its proof byte-matches. It never learns the pairing secret, and every frame
//! after the Hello is end-to-end encrypted — the relay forwards opaque blobs and
//! cannot read them. No agent logic, no persistence.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use bob_protocol::Hello;
use dashmap::DashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// A peer that connects but never sends its `Hello` ties up a socket forever.
/// Bound the handshake so a stalled/hostile dialer is dropped.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest relayed frame. E2E payloads are small control/telemetry blobs; a frame
/// beyond this is either a bug or an attempt to OOM the relay, so we drop it.
const MAX_FRAME_BYTES: usize = 1 << 20; // 1 MiB

/// Bound on frames queued toward one peer's socket. A peer that stops reading (or
/// reads slowly) can't make us buffer unboundedly — past this we drop the peer
/// rather than the relay's memory.
const PEER_QUEUE_CAP: usize = 256;

/// A pairing room: at most one host and one controller. Each slot holds a sender
/// that pushes frames to that peer's socket writer task. `admission` records the
/// first-arriving peer's proof, which the second peer must match.
#[derive(Default)]
struct Room {
    host: Option<mpsc::Sender<Message>>,
    controller: Option<mpsc::Sender<Message>>,
    /// The admission proof the first peer presented for this session; the second
    /// peer must present a byte-identical one. `None` until the first peer joins.
    admission: Option<String>,
}

pub struct AppState {
    rooms: DashMap<String, Room>,
}

/// Build the axum router. The relay holds no secret — admission is match-based.
pub fn router() -> Router {
    let state = Arc::new(AppState {
        rooms: DashMap::new(),
    });
    Router::new()
        .route("/ws", get(ws_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    // First frame must be a Hello — within a bounded window, so a peer that
    // connects and never handshakes can't hold the socket (and its slot) forever.
    let hello: Hello = match tokio::time::timeout(HELLO_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(t)))) => match serde_json::from_str(&t) {
            Ok(h) => h,
            Err(e) => {
                let _ = socket
                    .send(Message::Text(format!("{{\"error\":\"bad hello: {e}\"}}")))
                    .await;
                return;
            }
        },
        _ => return,
    };

    let session = hello.session().to_string();
    let is_host = hello.is_host();
    let admission = hello.admission().to_string();

    // Channel from the peer's writer half to this socket. Bounded so a peer that
    // stops reading can't make us buffer frames without limit.
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(PEER_QUEUE_CAP);

    // Admission + registration, atomically under the room entry lock.
    //
    // Match-based admission: the first peer to join a session sets the room's
    // expected proof; every later peer must present a byte-identical one. The
    // relay never learns the pairing secret — it only compares opaque proofs. A
    // mismatch is rejected without disturbing the peer already in the room.
    //
    // If a peer already occupies our slot (e.g. a stale host lingering after a
    // restart) but the proof matches, we evict it — same session, so it's our own
    // reconnecting host, not an impostor. This preserves the "no split brain" fix.
    {
        let mut room = state.rooms.entry(session.clone()).or_default();
        match &room.admission {
            Some(expected) if !constant_time_eq(expected.as_bytes(), admission.as_bytes()) => {
                drop(room);
                let _ = socket
                    .send(Message::Text("{\"error\":\"bad admission\"}".into()))
                    .await;
                return;
            }
            None => room.admission = Some(admission.clone()),
            Some(_) => {} // matched — proceed
        }
        if is_host {
            if room.host.is_some() {
                eprintln!("[relay] evicting previous host in session '{}'", session);
            }
            room.host = Some(out_tx.clone());
        } else {
            if room.controller.is_some() {
                eprintln!(
                    "[relay] evicting previous controller in session '{}'",
                    session
                );
            }
            room.controller = Some(out_tx.clone());
        }
    }
    eprintln!(
        "[relay] {} joined session '{}'",
        if is_host { "host" } else { "controller" },
        session
    );

    // axum's WebSocket isn't Clone, so split into sink+stream.
    use futures::stream::StreamExt;
    use futures::SinkExt;
    let (mut sink, mut stream) = socket.split();

    // Task: push queued frames (from the peer) to our socket.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Read loop: forward each inbound frame to the peer's sender.
    while let Some(Ok(msg)) = stream.next().await {
        match msg {
            Message::Text(_) | Message::Binary(_) => {
                // Drop oversized frames: E2E payloads are small; anything past the
                // cap is a bug or an OOM attempt. Skip it, keep the connection.
                let size = match &msg {
                    Message::Text(t) => t.len(),
                    Message::Binary(b) => b.len(),
                    _ => 0,
                };
                if size > MAX_FRAME_BYTES {
                    eprintln!(
                        "[relay] dropping oversized frame ({} bytes) in session '{}'",
                        size, session
                    );
                    continue;
                }
                let peer = {
                    let room = state.rooms.get(&session);
                    room.and_then(|r| {
                        if is_host {
                            r.controller.clone()
                        } else {
                            r.host.clone()
                        }
                    })
                };
                if let Some(peer) = peer {
                    // `try_send` (not `send`) so a stalled peer applies backpressure
                    // without blocking THIS reader; a full queue means the peer is
                    // dead/too-slow, so we stop forwarding to it.
                    if let Err(mpsc::error::TrySendError::Closed(_)) = peer.try_send(msg) {
                        // peer gone — nothing to do; cleanup happens on its own task
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Cleanup: drop our slot, but ONLY if it still holds OUR sender. If we were
    // already evicted by a newer connection for this role (reconnect), the slot
    // now belongs to that peer — clearing it would kill the live connection (the
    // split-brain bug). `same_channel` identifies our own sender.
    {
        if let Some(mut room) = state.rooms.get_mut(&session) {
            let slot = if is_host {
                &mut room.host
            } else {
                &mut room.controller
            };
            if slot
                .as_ref()
                .map(|s| s.same_channel(&out_tx))
                .unwrap_or(false)
            {
                *slot = None;
            }
        }
        let empty = state
            .rooms
            .get(&session)
            .map(|r| r.host.is_none() && r.controller.is_none())
            .unwrap_or(true);
        if empty {
            state.rooms.remove(&session);
        }
    }
    writer.abort();
    eprintln!(
        "[relay] {} left session '{}'",
        if is_host { "host" } else { "controller" },
        session
    );
}

/// Length-safe, constant-time byte comparison for admission proofs, so the relay
/// leaks no timing signal about how much of a proof matched.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
