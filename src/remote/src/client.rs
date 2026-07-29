//! A dumb terminal controller for exercising the relay + host end-to-end
//! before the iOS app exists. Reads stdin lines and sends them as prompts;
//! prints incoming HostFrames. Special lines: `/cancel`, `/y` (approve last
//! ask by choosing option 0 / first option), `/n` (dismiss/deny).

use base64::Engine as _;
use bob_protocol::{ControlFrame, Hello, HostFrame};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub async fn run(
    relay: String,
    session: String,
    secure: crate::SecureParams,
) -> anyhow::Result<()> {
    let peer_static = secure
        .peer_static
        .ok_or_else(|| anyhow::anyhow!("test-client needs the host's static key to pair"))?;

    let (ws, _) = tokio_tungstenite::connect_async(&relay).await?;
    let (mut sink, mut stream) = ws.split();

    // Present the admission proof, then run the initiator handshake.
    let admission = bob_secure::admission::prove(&secure.pairing_secret, &session);
    let hello = serde_json::to_string(&Hello::Controller {
        session: session.clone(),
        admission: base64::engine::general_purpose::STANDARD_NO_PAD.encode(&admission),
    })?;
    sink.send(WsMessage::Text(hello)).await?;

    let pro = crate::channel::prologue(&relay, &session);
    let est = crate::channel::handshake_initiator(
        &mut sink,
        &mut stream,
        &pro,
        secure.identity,
        secure.ephemeral,
        peer_static,
    )
    .await?;
    eprintln!(
        "[client] secure channel established · safety number {:04}",
        est.safety_number
    );

    // Share the sealer + sink so both the reader task and the stdin loop can send.
    let sealer = Arc::new(Mutex::new(est.sealer));
    let sink = Arc::new(Mutex::new(sink));
    let mut opener = est.opener;

    // Send helper: seal a frame and write it.
    async fn send_frame(
        sink: &Arc<Mutex<crate::channel::WsSink>>,
        sealer: &Arc<Mutex<bob_secure::Sealer>>,
        frame: &ControlFrame,
    ) -> anyhow::Result<()> {
        let plaintext = serde_json::to_vec(frame)?;
        let text = crate::channel::seal_envelope(&mut *sealer.lock().await, &plaintext)?;
        sink.lock().await.send(WsMessage::Text(text)).await?;
        Ok(())
    }

    // Pull current state (history + session list) now that we're paired.
    send_frame(&sink, &sealer, &ControlFrame::ListSessions).await?;
    eprintln!(
        "[client] connected, session '{}'. Type a prompt and hit enter.",
        session
    );
    eprintln!("[client] commands: /cancel, /y <id>, /n <id>");

    // Track the most recent ask id so /y and /n can answer it without typing it.
    // Kind: None = permission, Some(label) = query (label of option 0).
    let last_ask: Arc<Mutex<Option<(String, Option<String>)>>> = Arc::new(Mutex::new(None));

    // Reader task: incoming sealed frames -> opened HostFrames -> stdout.
    {
        let last_ask = last_ask.clone();
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                let text = match msg {
                    Ok(WsMessage::Text(t)) => t,
                    _ => continue,
                };
                let plaintext = match crate::channel::open_envelope(&mut opener, &text) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[client] {e}");
                        break;
                    }
                };
                match serde_json::from_slice::<HostFrame>(&plaintext) {
                    Ok(HostFrame::Event(e)) => {
                        println!("[event] {}", serde_json::to_string(&e).unwrap())
                    }
                    Ok(HostFrame::AskQuery { id, query }) => {
                        println!(
                            "[ask] {} — {} {:?}",
                            query.title, query.detail, query.options
                        );
                        println!("      reply with /y {id}  (option 0) or /n {id}");
                        let opt0 = query.options.first().cloned();
                        *last_ask.lock().await = Some((id, opt0));
                    }
                    Ok(HostFrame::AskPermission {
                        id,
                        request,
                        options,
                    }) => {
                        println!("[permission] {} on {}", request.tool, request.cwd);
                        for (i, o) in options.iter().enumerate() {
                            println!("      {i}: {}", o.label);
                        }
                        println!("      reply with /y {id} (option 0) or /n {id}");
                        *last_ask.lock().await = Some((id, None));
                    }
                    Ok(HostFrame::History {
                        messages,
                        session_id,
                        subagent_runs,
                    }) => {
                        println!(
                            "[history] session {session_id}: {} messages, {} subagent runs",
                            messages.len(),
                            subagent_runs.len()
                        );
                    }
                    Ok(HostFrame::SessionList { sessions }) => {
                        println!("[sessions] {} stored:", sessions.len());
                        for s in sessions {
                            println!("      {} · {} ({} msgs)", s.id, s.title, s.message_count);
                        }
                    }
                    Ok(HostFrame::Status { busy }) => println!("[status] busy={busy}"),
                    Err(e) => eprintln!("[client] decode error: {e}"),
                }
            }
            eprintln!("[client] relay closed");
        });
    }

    // stdin loop -> ControlFrames.
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let frame = if line == "/cancel" {
            ControlFrame::Cancel
        } else if let Some(rest) = line.strip_prefix("/y ") {
            answer(rest.trim(), true, &last_ask).await
        } else if let Some(rest) = line.strip_prefix("/n ") {
            answer(rest.trim(), false, &last_ask).await
        } else {
            ControlFrame::Prompt { text: line }
        };
        send_frame(&sink, &sealer, &frame).await?;
    }
    Ok(())
}

/// Build an AnswerQuery or AnswerPermission for the given id, based on the
/// tracked ask's kind. `approve` is true for /y, false for /n.
async fn answer(
    id: &str,
    approve: bool,
    last_ask: &Arc<Mutex<Option<(String, Option<String>)>>>,
) -> ControlFrame {
    // Look up the tracked ask; only act if the id matches what we're waiting on.
    let kind = last_ask
        .lock()
        .await
        .as_ref()
        .filter(|(aid, _)| aid == id)
        .map(|(_, k)| k.clone());
    match kind {
        // A query: /y answers with option 0's real label; /n dismisses.
        Some(Some(opt0)) => ControlFrame::AnswerQuery {
            id: id.to_string(),
            answer: if approve { Some(opt0) } else { None },
        },
        // A permission (or unknown/stale id → treat as permission, harmless).
        _ => ControlFrame::AnswerPermission {
            id: id.to_string(),
            choice: if approve { Some(0) } else { None },
        },
    }
}
