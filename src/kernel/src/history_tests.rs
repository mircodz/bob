use super::*;
use crate::model::{Command, CommandId, OperationId, RunId};
use crate::store::{
    artifact, encode, entry, finish_tool_entry, submit, tests::fixture, EntryData, Store,
};
use bob_types::ContentBlock;

pub(super) fn seed_run(db: &Connection, thread: &ThreadId) -> RunId {
    let run = RunId::new();
    db.execute(
        "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'completed')",
        params![run, thread],
    )
    .unwrap();
    run
}

pub(super) fn seed_tool(
    db: &Connection,
    thread: &ThreadId,
    run: &RunId,
    input: &Value,
) -> OperationId {
    let operation = OperationId::new();
    db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state,prepared) VALUES(?1,?2,?3,'read_file','tool',?1,?4,'done',?5)", params![operation,run,thread,encode(input).unwrap(),r#"{"private_prepared":"NEVER_EXPOSE_PREPARED"}"#]).unwrap();
    operation
}

pub(super) fn seed_child(db: &Connection, root: &ThreadId, run: &RunId) -> ThreadId {
    let child = ThreadId::new();
    db.execute("INSERT INTO threads(id,session,parent_run,name,profile) SELECT ?1,session,?2,'child',profile FROM threads WHERE id=?3", params![child,run,root]).unwrap();
    db.execute(
        "INSERT INTO agents(thread,role,blocking,state) VALUES(?1,'collaborator',0,'completed')",
        [&child],
    )
    .unwrap();
    child
}

fn read(db: &Connection, caller: &ThreadId, input: Value) -> Value {
    let budget = input["limit_bytes"]
        .as_u64()
        .unwrap_or(DEFAULT_BYTES as u64) as usize;
    let encoded = execute(db, caller, &input).unwrap();
    assert!(encoded.len() <= budget);
    serde_json::from_str(&encoded).unwrap()
}

fn reconstruct(db: &Connection, caller: &ThreadId, mut input: Value) -> String {
    let mut text = String::new();
    let mut offset = 0;
    loop {
        input["offset"] = json!(offset);
        let page = read(db, caller, input.clone());
        assert_eq!(page["offset"], offset);
        let part = page["text"].as_str().unwrap();
        text.push_str(part);
        match page["next_offset"].as_u64() {
            Some(next) => {
                assert_eq!(next as usize, offset + part.len());
                assert!(next as usize > offset);
                offset = next as usize;
            }
            None => {
                assert_eq!(page["total_bytes"], text.len());
                return text;
            }
        }
    }
}

fn assistant(db: &Connection, thread: &ThreadId, text: &str) -> i64 {
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "NEVER_EXPOSE_REASONING".into(),
                signature: "NEVER_EXPOSE_SIGNATURE".into(),
            },
            ContentBlock::RedactedThinking {
                data: "NEVER_EXPOSE_REDACTED".into(),
            },
            ContentBlock::ReasoningItem {
                item: json!({"private":"NEVER_EXPOSE_PROVIDER"}),
            },
            ContentBlock::Text { text: text.into() },
        ],
    };
    entry(
        db,
        thread,
        "assistant",
        text,
        EntryData {
            message: Some(&message),
            ..Default::default()
        },
    )
    .unwrap();
    db.last_insert_rowid()
}

#[test]
fn prior_child_reports_remain_readable_after_followup_and_reopen() {
    let (dir, mut store, receipt, _) = fixture();
    let owner = seed_run(&store.db, &receipt.thread);
    let child = seed_child(&store.db, &receipt.thread, &owner);
    let mut outcomes = Vec::new();
    for text in ["first retained report", "follow-up retained report"] {
        let run = RunId::new();
        store
            .db
            .execute(
                "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'active')",
                params![run, child],
            )
            .unwrap();
        store
            .db
            .execute(
                "UPDATE agents SET state='running',reported=0 WHERE thread=?1",
                [&child],
            )
            .unwrap();
        let tx = store.db.transaction().unwrap();
        crate::store::settle_run(&tx, &run, "completed", text).unwrap();
        let artifact: String = tx
            .query_row("SELECT outcome FROM runs WHERE id=?1", [&run], |row| {
                row.get(0)
            })
            .unwrap();
        tx.commit().unwrap();
        outcomes.push((artifact, text));
    }
    drop(store);
    let store = Store::open(&dir.0.join("lab.db")).unwrap();
    for (id, text) in outcomes {
        let page = read(
            &store.db,
            &receipt.thread,
            json!({"view":"artifact","thread_id":child,"artifact_id":id}),
        );
        assert_eq!(page["text"], text);
        assert!(execute(
            &store.db,
            &receipt.thread,
            &json!({"view":"artifact","artifact_id":id})
        )
        .is_err());
    }
}

#[test]
fn full_visible_entry_and_large_original_arguments_reconstruct_without_private_records() {
    let (_dir, store, receipt, _) = fixture();
    let text = "visible 雪🦀\n\"\\\u{0}".repeat(12_000);
    let sequence = assistant(&store.db, &receipt.thread, &text);
    assert_eq!(
        reconstruct(
            &store.db,
            &receipt.thread,
            json!({"view":"entry", "sequence":sequence, "limit_bytes":1024})
        ),
        text
    );
    let run = seed_run(&store.db, &receipt.thread);
    let arguments = json!({"content":"large original λ🦀".repeat(10_000)});
    let operation = seed_tool(&store.db, &receipt.thread, &run, &arguments);
    entry(
        &store.db,
        &receipt.thread,
        "tool_call",
        "preview",
        EntryData {
            operation: Some(&operation),
            ..Default::default()
        },
    )
    .unwrap();
    let call_sequence = store.db.last_insert_rowid();
    for _ in 0..1200 {
        entry(
            &store.db,
            &receipt.thread,
            "notice",
            "newer UI entry",
            EntryData::default(),
        )
        .unwrap();
    }
    let full = reconstruct(
        &store.db,
        &receipt.thread,
        json!({"view":"tool_arguments", "operation_id":operation, "limit_bytes":MAX_BYTES}),
    );
    assert!(full.len() > 65536);
    assert_eq!(serde_json::from_str::<Value>(&full).unwrap(), arguments);
    assert_eq!(
        reconstruct(
            &store.db,
            &receipt.thread,
            json!({"view":"entry", "sequence":call_sequence})
        ),
        full
    );
    assert!(!full.contains("NEVER_EXPOSE"));
    let model: String = store
        .db
        .query_row("SELECT model FROM entries WHERE seq=?1", [sequence], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(execute(
        &store.db,
        &receipt.thread,
        &json!({"view":"artifact", "artifact_id":model})
    )
    .is_err());
}

#[test]
fn model_facing_tool_result_and_chunked_unicode_pages_exclude_display_output() {
    let (_dir, store, receipt, _) = fixture();
    let run = seed_run(&store.db, &receipt.thread);
    let operation = seed_tool(&store.db, &receipt.thread, &run, &json!({"path":"safe"}));
    let output = format!("{}🦀{}", "a".repeat(65535), "\n\"\u{0}λ".repeat(30_000));
    finish_tool_entry(&store.db, &operation, &output, false).unwrap();
    let sequence = store.db.last_insert_rowid();
    let display = artifact(&store.db, "HUMAN_FORMATTED_NOT_MODEL_TEXT").unwrap();
    store
        .db
        .execute(
            "UPDATE operations SET display_result=?1 WHERE id=?2",
            params![display, operation],
        )
        .unwrap();
    let id: String = store
        .db
        .query_row(
            "SELECT result FROM operations WHERE id=?1",
            [&operation],
            |r| r.get(0),
        )
        .unwrap();
    for request in [
        json!({"view":"tool_output", "operation_id":operation}),
        json!({"view":"entry", "sequence":sequence}),
        json!({"view":"artifact", "artifact_id":id}),
    ] {
        assert_eq!(
            reconstruct(&store.db, &receipt.thread, request.clone()),
            output
        );
        let mut request = request;
        request["offset"] = json!(65536);
        assert!(execute(&store.db, &receipt.thread, &request).is_err());
        request["offset"] = json!(output.len());
        let eof = read(&store.db, &receipt.thread, request.clone());
        assert_eq!(eof["text"], "");
        assert!(eof["next_offset"].is_null());
        request["offset"] = json!(output.len() + 1);
        assert!(execute(&store.db, &receipt.thread, &request).is_err());
    }
    assert!(execute(
        &store.db,
        &receipt.thread,
        &json!({"view":"artifact", "artifact_id":display})
    )
    .is_err());
    let page = read(
        &store.db,
        &receipt.thread,
        json!({"view":"artifact", "artifact_id":id}),
    );
    assert_eq!(page["source"], "operations.result");
    assert_eq!(page["preview_only"], false);
    assert!(page["original_complete"].is_null());
}

#[test]
fn transcript_cursors_freeze_initial_head_and_bind_target_and_direction() {
    let (_dir, store, receipt, _) = fixture();
    let run = seed_run(&store.db, &receipt.thread);
    let child = seed_child(&store.db, &receipt.thread, &run);
    for i in 0..9 {
        assistant(&store.db, &receipt.thread, &format!("entry {i}"));
    }
    for direction in ["oldest_first", "newest_first"] {
        let mut request =
            json!({"view":"transcript", "direction":direction, "limit":2, "limit_bytes":1024});
        let first = read(&store.db, &receipt.thread, request.clone());
        let head = first["initial_head"].as_i64().unwrap();
        let mut page = first.clone();
        let mut sequences = Vec::new();
        loop {
            sequences.extend(
                page["entries"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["sequence"].as_i64().unwrap()),
            );
            if page["next_cursor"].is_null() {
                break;
            }
            assistant(
                &store.db,
                &receipt.thread,
                "history reads keep adding entries",
            );
            request["cursor"] = page["next_cursor"].clone();
            page = read(&store.db, &receipt.thread, request.clone());
            assert_eq!(page["initial_head"], head);
        }
        assert!(sequences.iter().all(|seq| *seq <= head));
        assert_eq!(sequences.len(), head as usize);
        assert!(sequences
            .windows(2)
            .all(|pair| if direction == "oldest_first" {
                pair[0] < pair[1]
            } else {
                pair[0] > pair[1]
            }));
        request["cursor"] = first["next_cursor"].clone();
        request["thread_id"] = json!(child);
        assert!(execute(&store.db, &receipt.thread, &request).is_err());
        request.as_object_mut().unwrap().remove("thread_id");
        request["direction"] = json!(if direction == "oldest_first" {
            "newest_first"
        } else {
            "oldest_first"
        });
        assert!(execute(&store.db, &receipt.thread, &request).is_err());
    }
}

#[test]
fn history_validates_variants_parameters_and_encoded_response_budget() {
    let (_dir, store, receipt, _) = fixture();
    let sequence = assistant(&store.db, &receipt.thread, &"\u{0}\"\\雪".repeat(10_000));
    for invalid in [
        json!({}),
        json!({"view":"unknown"}),
        json!({"view":"entry"}),
        json!({"view":"entry", "sequence":0}),
        json!({"view":"entry", "sequence":sequence, "offset":-1}),
        json!({"view":"entry", "sequence":sequence, "offset":1.5}),
        json!({"view":"transcript", "limit":0}),
        json!({"view":"transcript", "limit":101}),
        json!({"view":"transcript", "direction":"invalid"}),
        json!({"view":"transcript", "cursor":"garbage"}),
        json!({"view":"transcript", "limit_bytes":0}),
        json!({"view":"transcript", "limit_bytes":MAX_BYTES+1}),
        json!({"view":"transcript", "operation_id":OperationId::new()}),
        json!({"view":"tool_arguments", "operation_id":"not-a-uuid"}),
        json!({"view":"artifact", "artifact_id":"../private"}),
        json!({"view":"entry", "sequence":sequence, "prepared":true}),
        json!({"view":"transcript", "thread_id":"root"}),
    ] {
        assert!(
            execute(&store.db, &receipt.thread, &invalid).is_err(),
            "{invalid}"
        );
    }
    for limit in [MIN_BYTES, DEFAULT_BYTES, MAX_BYTES] {
        let page = read(
            &store.db,
            &receipt.thread,
            json!({"view":"entry", "sequence":sequence, "limit_bytes":limit}),
        );
        assert!(!page["text"].as_str().unwrap().is_empty());
        assert!(page["next_offset"].is_number());
        read(
            &store.db,
            &receipt.thread,
            json!({"view":"transcript", "limit_bytes":limit}),
        );
    }
    let descriptor = tools().pop().unwrap();
    assert_eq!(descriptor.effect, EffectClass::Read);
    let schema = jsonschema::validator_for(&descriptor.spec.input_schema).unwrap();
    assert!(schema.is_valid(&json!({"view":"transcript"})));
    assert!(execute(&store.db, &receipt.thread, &json!({"view":"entry"})).is_err());
    assert!(descriptor.spec.input_schema.get("oneOf").is_none());
    assert!(!schema
        .is_valid(&json!({"view":"artifact", "artifact_id":ArtifactId::new(), "prepared":true})));
}

#[test]
fn same_session_includes_root_but_every_cross_session_id_route_is_rejected() {
    let (_dir, mut store, receipt, models) = fixture();
    let run = seed_run(&store.db, &receipt.thread);
    let child = seed_child(&store.db, &receipt.thread, &run);
    let root_entry = assistant(
        &store.db,
        &receipt.thread,
        "root context is explicitly readable",
    );
    assert_eq!(
        read(
            &store.db,
            &child,
            json!({"view":"entry", "thread_id":receipt.thread, "sequence":root_entry})
        )["text"],
        "root context is explicitly readable"
    );
    let profile: String = store
        .db
        .query_row(
            "SELECT profile FROM threads WHERE id=?1",
            [&receipt.thread],
            |r| r.get(0),
        )
        .unwrap();
    let other = store
        .command(
            &Command::CreateSession {
                id: CommandId::new(),
                workspace: _dir.0.to_string_lossy().into_owned(),
                profile: store::decode(profile).unwrap(),
                policy: Default::default(),
            },
            &models,
        )
        .unwrap();
    assert_ne!(other.session, receipt.session);
    let other_run = seed_run(&store.db, &other.thread);
    let other_op = seed_tool(
        &store.db,
        &other.thread,
        &other_run,
        &json!({"secret":"OTHER_SESSION"}),
    );
    finish_tool_entry(&store.db, &other_op, "OTHER_SESSION", false).unwrap();
    let other_entry = store.db.last_insert_rowid();
    let other_artifact: String = store
        .db
        .query_row(
            "SELECT result FROM operations WHERE id=?1",
            [&other_op],
            |r| r.get(0),
        )
        .unwrap();
    let requests = [
        json!({"view":"transcript"}),
        json!({"view":"entry", "sequence":other_entry}),
        json!({"view":"tool_arguments", "operation_id":other_op}),
        json!({"view":"tool_output", "operation_id":other_op}),
        json!({"view":"artifact", "artifact_id":other_artifact}),
    ];
    for mut request in requests {
        if request["view"] != "transcript" {
            assert!(execute(&store.db, &receipt.thread, &request).is_err());
            request["thread_id"] = json!(child);
            assert!(execute(&store.db, &receipt.thread, &request).is_err());
        }
        request["thread_id"] = json!(other.thread);
        assert!(execute(&store.db, &receipt.thread, &request).is_err());
    }
    store
        .db
        .execute(
            "UPDATE entries SET operation=?1 WHERE seq=?2",
            params![other_op, root_entry],
        )
        .unwrap();
    assert!(execute(
        &store.db,
        &receipt.thread,
        &json!({"view":"entry", "sequence":root_entry})
    )
    .is_err());
    let unattached = artifact(&store.db, "PRIVATE_REQUEST_SYSTEM_PROVIDER_RECORD").unwrap();
    let model = OperationId::new();
    store.db.execute("INSERT INTO operations(id,run,thread,name,kind,input,state,result) VALUES(?1,?2,?3,'model','model','{}','done',?4)", params![model,run,receipt.thread,unattached]).unwrap();
    for request in [
        json!({"view":"artifact", "artifact_id":unattached}),
        json!({"view":"tool_arguments", "operation_id":model}),
        json!({"view":"tool_output", "operation_id":model}),
    ] {
        assert!(execute(&store.db, &receipt.thread, &request).is_err());
    }
}

#[test]
fn full_human_and_bidirectional_coordination_sources_preserve_routing() {
    let (_dir, store, receipt, _) = fixture();
    let run = seed_run(&store.db, &receipt.thread);
    let child = seed_child(&store.db, &receipt.thread, &run);
    let text = "full coordination 🦀".repeat(8000);
    let operation = seed_tool(&store.db, &receipt.thread, &run, &json!({"message":text}));
    let submission = CommandId::new();
    submit(
        &store.db,
        &submission,
        &child,
        "coordination",
        Some(&receipt.thread),
        &text,
    )
    .unwrap();
    let received = store.db.last_insert_rowid();
    store.db.execute("INSERT INTO coordination(id,source,destination,submission,operation,kind,text) VALUES(?1,?2,?3,?4,?5,'message','preview')", params![CommandId::new(),receipt.thread,child,submission,operation]).unwrap();
    entry(
        &store.db,
        &receipt.thread,
        "coordination",
        "Sent preview",
        EntryData {
            submission: Some(&submission),
            operation: Some(&operation),
            ..Default::default()
        },
    )
    .unwrap();
    let sent = store.db.last_insert_rowid();
    for (thread, sequence) in [(&receipt.thread, sent), (&child, received)] {
        let request = json!({"view":"entry", "thread_id":thread, "sequence":sequence});
        assert_eq!(
            reconstruct(&store.db, &receipt.thread, request.clone()),
            text
        );
        let page = read(&store.db, &receipt.thread, request);
        assert_eq!(page["source"], "submissions.text");
        assert_eq!(page["preview_only"], false);
        assert_eq!(page["routing"]["sender_thread_id"], json!(receipt.thread));
        assert_eq!(page["routing"]["recipient_thread_id"], json!(child));
    }
    store
        .db
        .execute("UPDATE entries SET submission=NULL WHERE seq=?1", [sent])
        .unwrap();
    assert_eq!(
        reconstruct(
            &store.db,
            &receipt.thread,
            json!({"view":"entry", "sequence":sent})
        ),
        text
    );
    let human = "full human text λ".repeat(5000);
    submit(
        &store.db,
        &CommandId::new(),
        &receipt.thread,
        "human",
        None,
        &human,
    )
    .unwrap();
    let human_seq = store.db.last_insert_rowid();
    assert_eq!(
        reconstruct(
            &store.db,
            &receipt.thread,
            json!({"view":"entry", "sequence":human_seq})
        ),
        human
    );
    entry(
        &store.db,
        &receipt.thread,
        "coordination",
        &text,
        EntryData::default(),
    )
    .unwrap();
    let preview = read(
        &store.db,
        &receipt.thread,
        json!({"view":"entry", "sequence":store.db.last_insert_rowid()}),
    );
    assert_eq!(preview["preview_only"], true);
    assert_eq!(preview["retention"], "preview_only");
    assert!(preview["original_complete"].is_null());
    let message = Message::user_text(&text);
    entry(
        &store.db,
        &receipt.thread,
        "coordination",
        "old preview",
        EntryData {
            message: Some(&message),
            ..Default::default()
        },
    )
    .unwrap();
    let retained = store.db.last_insert_rowid();
    assert_eq!(
        reconstruct(
            &store.db,
            &receipt.thread,
            json!({"view":"entry", "sequence":retained})
        ),
        text
    );
}

#[test]
fn omitted_large_child_report_is_retrievable_only_through_its_target() {
    let (_dir, store, receipt, _) = fixture();
    let run = seed_run(&store.db, &receipt.thread);
    let child = seed_child(&store.db, &receipt.thread, &run);
    let report = "oversized child report 🦀\n".repeat(30_000);
    let outcome = artifact(&store.db, &report).unwrap();
    let operation = seed_tool(&store.db, &receipt.thread, &run, &json!({}));
    store
        .db
        .execute(
            "UPDATE agents SET outcome=?1,operation=?2 WHERE thread=?3",
            params![outcome, operation, child],
        )
        .unwrap();
    let (_, aggregate) = crate::delegation::reports(&store.db, &operation).unwrap();
    assert_eq!(aggregate["agents"][0]["output_omitted"], true);
    assert_eq!(aggregate["agents"][0]["result_artifact"], json!(outcome));
    let request = json!({"view":"artifact", "thread_id":child, "artifact_id":outcome});
    assert_eq!(reconstruct(&store.db, &receipt.thread, request), report);
    assert!(execute(
        &store.db,
        &receipt.thread,
        &json!({"view":"artifact", "artifact_id":outcome})
    )
    .is_err());
}

#[test]
fn retained_entries_survive_checkpoint_and_reopen() {
    let (dir, mut store, receipt, models) = fixture();
    let text = "Retained before compaction";
    let sequence = assistant(&store.db, &receipt.thread, text);
    let before =
        crate::context::snapshot(&store.db, &receipt.thread, &models[0], &Default::default())
            .unwrap();
    let tx = store.db.transaction().unwrap();
    crate::context::checkpoint(
        &tx,
        &receipt.thread,
        &OperationId::new(),
        before.epoch,
        before.revision,
        &[Message::user_text("compacted summary")],
    )
    .unwrap();
    tx.commit().unwrap();
    drop(store);
    let store = Store::open(&dir.0.join("lab.db")).unwrap();
    assert_eq!(
        read(
            &store.db,
            &receipt.thread,
            json!({"view":"entry", "sequence":sequence})
        )["text"],
        text
    );
    assert_eq!(
        read(
            &store.db,
            &receipt.thread,
            json!({"view":"transcript", "direction":"oldest_first"})
        )["entries"][0]["sequence"],
        sequence
    );
}
