//! bob-relay: a dumb public WebSocket relay. Both a `bob-remote` host and a
//! controller (iOS app / test-client) dial *out* to this server — solving the
//! NAT problem where the phone can't reach the dev machine directly.
//!
//! Each connection's first WS text frame must be a `Hello` identifying role +
//! session + shared token. The relay validates the token, pairs one host with
//! one controller per `session`, then forwards every subsequent frame verbatim
//! between them. No agent logic, no persistence.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use bob_protocol::Hello;
use dashmap::DashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// A pairing room: at most one host and one controller. Each slot holds a
/// sender that pushes frames to that peer's socket writer task.
#[derive(Default)]
struct Room {
    host: Option<mpsc::UnboundedSender<Message>>,
    controller: Option<mpsc::UnboundedSender<Message>>,
}

pub struct AppState {
    token: String,
    rooms: DashMap<String, Room>,
}

/// Build the axum router with the given shared token. Exposed for tests.
pub fn router(token: String) -> Router {
    let state = Arc::new(AppState {
        token,
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
    // First frame must be a Hello.
    let hello: Hello = match socket.recv().await {
        Some(Ok(Message::Text(t))) => match serde_json::from_str(&t) {
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

    if hello.token() != state.token {
        let _ = socket
            .send(Message::Text("{\"error\":\"bad token\"}".into()))
            .await;
        return;
    }

    let session = hello.session().to_string();
    let is_host = hello.is_host();

    // Channel from the peer's writer half to this socket.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

    // Register this side in the room, grabbing the peer's sender if present.
    // If a peer already occupies this slot (e.g. a stale host lingering after a
    // restart), evict it: drop its sender so its writer task ends and it stops
    // receiving routed frames. This prevents the "two hosts, split brain" bug
    // where a prompt reaches one host while the controller watches the other.
    {
        let mut room = state.rooms.entry(session.clone()).or_default();
        if is_host {
            if room.host.is_some() {
                eprintln!("[relay] evicting previous host in session '{}'", session);
            }
            room.host = Some(out_tx.clone());
        } else {
            if room.controller.is_some() {
                eprintln!("[relay] evicting previous controller in session '{}'", session);
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
                    let _ = peer.send(msg);
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    // Cleanup: drop our slot; if the room is now empty, remove it.
    {
        if let Some(mut room) = state.rooms.get_mut(&session) {
            if is_host {
                room.host = None;
            } else {
                room.controller = None;
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
