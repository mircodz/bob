//! End-to-end forwarding test: bind the real relay router on an ephemeral port,
//! connect a fake host and controller in the same session, and assert frames
//! forward both ways. Also asserts a mismatched admission proof is rejected.
//! No LLM needed.

use bob_protocol::{ControlFrame, Hello, HostFrame};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Start the relay on an OS-assigned port; return its ws:// base URL. The relay
/// holds no secret — admission is match-based between the two peers.
async fn start_relay() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = bob_relay::router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("ws://{addr}/ws")
}

async fn ws_connect(
    url: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws
}

async fn send_json<T: serde::Serialize>(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    v: &T,
) {
    ws.send(WsMessage::Text(serde_json::to_string(v).unwrap()))
        .await
        .unwrap();
}

async fn next_text(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> String {
    loop {
        match ws.next().await {
            Some(Ok(WsMessage::Text(t))) => return t,
            Some(Ok(_)) => continue,
            other => panic!("expected text frame, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn forwards_control_and_host_frames() {
    let url = start_relay().await;

    // Both peers present the SAME admission proof (they share the pairing secret).
    let proof = "matching-proof";

    let mut host = ws_connect(&url).await;
    send_json(
        &mut host,
        &Hello::Host {
            session: "s1".into(),
            admission: proof.into(),
        },
    )
    .await;

    let mut ctrl = ws_connect(&url).await;
    send_json(
        &mut ctrl,
        &Hello::Controller {
            session: "s1".into(),
            admission: proof.into(),
        },
    )
    .await;

    // Controller -> Host: a prompt should arrive at the host verbatim.
    send_json(
        &mut ctrl,
        &ControlFrame::Prompt {
            text: "hello".into(),
        },
    )
    .await;
    let got = next_text(&mut host).await;
    match serde_json::from_str::<ControlFrame>(&got).unwrap() {
        ControlFrame::Prompt { text } => assert_eq!(text, "hello"),
        other => panic!("host got wrong frame: {other:?}"),
    }

    // Host -> Controller: a status frame should arrive at the controller.
    send_json(&mut host, &HostFrame::Status { busy: true }).await;
    let got = next_text(&mut ctrl).await;
    match serde_json::from_str::<HostFrame>(&got).unwrap() {
        HostFrame::Status { busy } => assert!(busy),
        other => panic!("controller got wrong frame: {other:?}"),
    }
}

#[tokio::test]
async fn rejects_mismatched_admission() {
    let url = start_relay().await;

    // First peer sets the room's expected proof.
    let mut host = ws_connect(&url).await;
    send_json(
        &mut host,
        &Hello::Host {
            session: "s1".into(),
            admission: "the-real-proof".into(),
        },
    )
    .await;

    // Second peer presents a different proof → rejected.
    let mut impostor = ws_connect(&url).await;
    send_json(
        &mut impostor,
        &Hello::Controller {
            session: "s1".into(),
            admission: "wrong-proof".into(),
        },
    )
    .await;
    let got = next_text(&mut impostor).await;
    assert!(
        got.contains("bad admission"),
        "expected rejection, got: {got}"
    );
}
