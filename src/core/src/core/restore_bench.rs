use super::events::AgentEvent;
use super::session::{context_epoch, new_session, root_history_from_events};
use super::store::{SessionStore, SqliteStore};
use super::types::{ContentBlock, Message, Role, ToolOutput};
use std::hint::black_box;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn tool_turn(index: usize) -> Vec<AgentEvent> {
    let id = format!("read-{index}");
    let input = serde_json::json!({"path": format!("src/module_{index}.rs")});
    vec![
        AgentEvent::UserPrompt {
            agent_id: "root".into(),
            text: format!("Inspect module {index}"),
        },
        AgentEvent::Message {
            agent_id: "root".into(),
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: id.clone(),
                    name: "read_file".into(),
                    input: input.clone(),
                }],
            },
        },
        AgentEvent::ToolCall {
            agent_id: "root".into(),
            tool_use_id: id.clone(),
            name: "read_file".into(),
            input,
        },
        AgentEvent::ToolResult {
            agent_id: "root".into(),
            tool_use_id: id,
            output: ToolOutput::Read {
                body: "    1 pub fn example() { println!(\"benchmark\"); }\n".repeat(40),
                loader_context: None,
            },
            is_error: false,
        },
        AgentEvent::Message {
            agent_id: "root".into(),
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: format!("Module {index} inspected."),
                }],
            },
        },
        AgentEvent::TurnEnd {
            agent_id: "root".into(),
            usage: Default::default(),
        },
    ]
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
#[ignore = "opt-in release restore baseline; no wall-clock threshold"]
fn benchmark_sqlite_restore() {
    let path = std::env::temp_dir().join(format!(
        "bob-restore-bench-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&path).unwrap();
    let db = path.join("sessions.db");
    let store = SqliteStore::open(&db).unwrap();
    for turns in [1000, 2000] {
        for scenario in ["healthy", "compacted", "reset", "snapshot-fallback"] {
            let id = format!("{scenario}-{turns}");
            let mut events: Vec<_> = (0..turns).flat_map(tool_turn).collect();
            if scenario == "reset" {
                events.insert(
                    turns / 2 * 6,
                    AgentEvent::ContextReset {
                        agent_id: "root".into(),
                        epoch: 1,
                    },
                );
            }
            let mut snapshot = new_session("benchmark", id.clone(), "1".into(), ".".into());
            snapshot.messages = root_history_from_events(&events);
            snapshot.context_epoch = context_epoch(&events);
            if scenario == "compacted" {
                events.push(AgentEvent::Compaction {
                    agent_id: "root".into(),
                    before_tokens: 100_000,
                    after_tokens: 100,
                    summary: "All modules inspected.".into(),
                    replaced_upto: snapshot.messages.len(),
                });
            }
            if scenario == "snapshot-fallback" {
                events.truncate(events.len() - 6);
            }
            store.save(&snapshot).unwrap();
            for event in &events {
                store.append_event(&id, event).unwrap();
            }
            store.flush().unwrap();
            let connection = rusqlite::Connection::open(&db).unwrap();
            let (rows, bytes): (usize, usize) = connection.query_row(
                "SELECT COUNT(*), SUM(length(CAST(data AS BLOB))) FROM events WHERE session_id = ?1",
                [&id], |row| Ok((row.get(0)?, row.get(1)?)),
            ).unwrap();
            let body_bytes: usize = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::ToolResult { output, .. } => Some(output.model_text().len()),
                    _ => None,
                })
                .sum();
            let mut totals = Vec::new();
            let mut stages: [Vec<Duration>; 5] = Default::default();
            for _ in 0..3 {
                let start = Instant::now();
                let mut loaded = store.load(black_box(&id)).unwrap().unwrap();
                let snapshot_read = start.elapsed();
                let stage = Instant::now();
                let history = store.history_for(&loaded).unwrap();
                let full_read = stage.elapsed();
                let stage = Instant::now();
                let working = store.working_history_for(&loaded).unwrap();
                let working_read = stage.elapsed();
                let stage = Instant::now();
                let archived = store.load_events(&id).unwrap().unwrap();
                loaded.context_epoch = loaded.context_epoch.max(context_epoch(&archived));
                loaded.messages = history.clone();
                let archive_read = stage.elapsed();
                let stage = Instant::now();
                let ui = store.load_events(&id).unwrap().unwrap();
                let rebuilt = root_history_from_events(&ui);
                let replay = context_epoch(&ui) >= loaded.context_epoch
                    && rebuilt.len() >= loaded.messages.len();
                black_box((
                    &loaded, &history, &working, &archived, &ui, &rebuilt, replay,
                ));
                let ui_read = stage.elapsed();
                totals.push(start.elapsed());
                for (samples, elapsed) in stages.iter_mut().zip([
                    snapshot_read,
                    full_read,
                    working_read,
                    archive_read,
                    ui_read,
                ]) {
                    samples.push(elapsed);
                }
                assert_eq!(history.len(), snapshot.messages.len());
                assert_eq!(
                    working.len(),
                    if scenario == "compacted" {
                        1
                    } else {
                        history.len()
                    }
                );
                assert_eq!(replay, scenario != "snapshot-fallback");
                let results = history.iter().flat_map(|m| &m.content).filter(|block| {
                    matches!(block, ContentBlock::ToolResult { content: ToolOutput::Read { body, .. }, .. } if !body.is_empty())
                }).count();
                assert_eq!(
                    results,
                    if scenario == "reset" {
                        turns / 2
                    } else {
                        turns
                    }
                );
            }
            println!("restore scenario={scenario} turns={turns} event_rows={rows} event_json_bytes={bytes} tool_body_bytes={body_bytes} history_messages={} median3={:?} stages(snapshot,history,working,archive+clone,ui+gate)={:?}",
                snapshot.messages.len(), median(totals), stages.map(median));
        }
    }
    drop(store);
    std::fs::remove_dir_all(path).unwrap();
}
