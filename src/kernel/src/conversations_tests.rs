use super::*;
use crate::model::{Command, CommandId, ModelInfo, OperationId, Receipt, RunId};
use crate::store::{
    self, artifact, entry, finish_tool_entry, submit, tests::fixture, EntryData, Store,
};
use crate::SessionCatalog;
use bob_types::{ContentBlock, Message, Role};
use std::collections::BTreeSet;

const TOKEN: &str = "00112233-4455-4677-8899-aabbccddeeff";

fn other(store: &mut Store, receipt: &Receipt, models: &[ModelInfo], workspace: &str) -> Receipt {
    let profile: String = store
        .db
        .query_row(
            "SELECT profile FROM threads WHERE id=?1",
            [&receipt.thread],
            |r| r.get(0),
        )
        .unwrap();
    let present_workspace: String = store
        .db
        .query_row(
            "SELECT workspace FROM sessions WHERE id=?1",
            [&receipt.session],
            |r| r.get(0),
        )
        .unwrap();
    let other = store
        .command(
            &Command::CreateSession {
                id: CommandId::new(),
                workspace: present_workspace,
                profile: store::decode(profile).unwrap(),
                policy: Default::default(),
            },
            models,
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE sessions SET workspace=?1 WHERE id=?2",
            params![workspace, other.session],
        )
        .unwrap();
    other
}
fn assistant(db: &Connection, thread: &ThreadId, text: &str) -> i64 {
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "PRIVATE_REASONING_MARKER".into(),
                signature: "PRIVATE_SIGNATURE_MARKER".into(),
            },
            ContentBlock::ReasoningItem {
                item: json!({"hidden":"PRIVATE_PROVIDER_MARKER"}),
            },
            ContentBlock::RedactedThinking {
                data: "PRIVATE_REDACTED_MARKER".into(),
            },
            ContentBlock::ToolUse {
                id: "hidden-call".into(),
                name: "PRIVATE_TOOL_NAME_MARKER".into(),
                input: json!({"hidden":"PRIVATE_TOOL_INPUT_MARKER"}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "hidden-call".into(),
                content: "PRIVATE_TOOL_BLOCK_MARKER".into(),
                is_error: None,
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
fn child(db: &Connection, root: &ThreadId) -> (RunId, ThreadId) {
    let run = RunId::new();
    db.execute(
        "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'completed')",
        params![run, root],
    )
    .unwrap();
    let child = ThreadId::new();
    db.execute("INSERT INTO threads(id,session,parent_run,name,profile) SELECT ?1,session,?2,'child',profile FROM threads WHERE id=?3", params![child,run,root]).unwrap();
    (run, child)
}
fn query(db: &Connection, caller: &ThreadId, tool: &str, input: Value) -> Value {
    let encoded = execute(db, caller, TOKEN, tool, &input).unwrap();
    assert!(encoded.len() <= input["limit_bytes"].as_u64().unwrap_or(MAX_OUTPUT as u64) as usize);
    serde_json::from_str(&encoded).unwrap()
}
fn search_all(db: &Connection, caller: &ThreadId, mut input: Value) -> Vec<Value> {
    let mut found = Vec::new();
    let mut previous = Value::Null;
    for _ in 0..100 {
        let page = query(db, caller, "search_conversation", input.clone());
        assert!(page["examined_entries"].as_u64().unwrap() <= MAX_CANDIDATES as u64);
        assert!(page["examined_bytes"].as_u64().unwrap() <= SCAN_BYTES as u64);
        found.extend(page["matches"].as_array().unwrap().iter().cloned());
        if page["scan_complete"] == true {
            assert!(page["next_cursor"].is_null());
            return found;
        }
        assert!(page["next_cursor"].is_string());
        assert_ne!(page["next_cursor"], previous);
        previous = page["next_cursor"].clone();
        input["cursor"] = previous.clone();
    }
    panic!("archive scan did not advance");
}

#[test]
fn full_text_tails_private_markers_and_current_session_exclusion() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(
        &mut store,
        &receipt,
        &models,
        "/missing/historical-workspace",
    );
    let text = format!("{} Καλημέρα tailneedle", "visible 雪🦀 ".repeat(1000));
    let sequence = assistant(&store.db, &old.thread, &text);
    assistant(
        &store.db,
        &receipt.thread,
        "tailneedle current session excluded",
    );
    let run = RunId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'completed')",
            params![run, old.thread],
        )
        .unwrap();
    let operation = OperationId::new();
    store.db.execute("INSERT INTO operations(id,run,thread,name,kind,call_id,input,state,prepared) VALUES(?1,?2,?3,'bash','tool',?1,?4,'done',?5)", params![operation,run,old.thread,r#"{"command":"PRIVATE_INPUT_MARKER"}"#,r#"{"hidden":"PRIVATE_PREPARED_MARKER"}"#]).unwrap();
    finish_tool_entry(&store.db, &operation, "PRIVATE_OUTPUT_MARKER", false).unwrap();
    let display = artifact(&store.db, "PRIVATE_DISPLAY_MARKER").unwrap();
    store
        .db
        .execute(
            "UPDATE operations SET display_result=?1 WHERE id=?2",
            params![display, operation],
        )
        .unwrap();
    artifact(&store.db, "PRIVATE_RAW_MARKER").unwrap();
    let hits = search_all(
        &store.db,
        &receipt.thread,
        json!({"query":"TAILNEEDLE καλημέρα"}),
    );
    assert_eq!(hits.len(), 1);
    let hit = &hits[0];
    assert_eq!(hit["sequence"], sequence);
    assert_eq!(hit["session_id"], json!(old.session));
    assert!(hit["snippet"].as_str().unwrap().contains("tailneedle"));
    let start = hit["snippet_offset"].as_u64().unwrap() as usize;
    assert!(start > 2048);
    assert!(text[start..].starts_with(hit["snippet"].as_str().unwrap()));
    assert_eq!(hit["preview_only"], false);
    assert_eq!(hit["source"], "message.text");
    for marker in [
        "PRIVATE_REASONING_MARKER",
        "PRIVATE_SIGNATURE_MARKER",
        "PRIVATE_PROVIDER_MARKER",
        "PRIVATE_REDACTED_MARKER",
        "PRIVATE_TOOL_NAME_MARKER",
        "PRIVATE_TOOL_INPUT_MARKER",
        "PRIVATE_TOOL_BLOCK_MARKER",
        "PRIVATE_INPUT_MARKER",
        "PRIVATE_PREPARED_MARKER",
        "PRIVATE_OUTPUT_MARKER",
        "PRIVATE_DISPLAY_MARKER",
        "PRIVATE_RAW_MARKER",
    ] {
        assert!(
            search_all(&store.db, &receipt.thread, json!({"query":marker})).is_empty(),
            "{marker}"
        );
    }
    let human = format!("{} human-tail-needle", "historical prompt 雪".repeat(2000));
    submit(
        &store.db,
        &CommandId::new(),
        &old.thread,
        "human",
        None,
        &human,
    )
    .unwrap();
    let human_sequence = store.db.last_insert_rowid();
    let human_hits = search_all(
        &store.db,
        &receipt.thread,
        json!({"query":"human-tail-needle"}),
    );
    assert_eq!(human_hits.len(), 1);
    assert_eq!(human_hits[0]["sequence"], human_sequence);
    assert_eq!(human_hits[0]["source"], "submissions.text");
    assert!(human_hits[0]["snippet_offset"].as_u64().unwrap() > 2048);
    let read = query(
        &store.db,
        &receipt.thread,
        "read_conversation",
        hit["read_ref"].clone(),
    );
    assert_eq!(read["session_id"], json!(old.session));
    assert!(!read.to_string().contains("PRIVATE_"));
    assert!(crate::history::execute(
        &store.db,
        &receipt.thread,
        &json!({"view":"entry","thread_id":old.thread,"sequence":sequence})
    )
    .is_err());
    assert!(!query(
        &store.db,
        &receipt.thread,
        "read_conversation",
        json!({"view":"transcript","session_id":receipt.session})
    )["entries"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn child_assignments_guidance_and_bidirectional_copies_have_safe_provenance() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/cross-workspace");
    let (run, child) = child(&store.db, &old.thread);
    submit(
        &store.db,
        &CommandId::new(),
        &child,
        "delegation",
        Some(&old.thread),
        "initial assignment recollection",
    )
    .unwrap();
    let initial = store.db.last_insert_rowid();
    let operation = OperationId::new();
    store.db.execute("INSERT INTO operations(id,run,thread,name,kind,input,state) VALUES(?1,?2,?3,'send_message','tool','{}','done')", params![operation,run,old.thread]).unwrap();
    let id = CommandId::new();
    let guidance = format!("{} guidance recollection", "detail 🦀".repeat(2000));
    submit(
        &store.db,
        &id,
        &child,
        "coordination",
        Some(&old.thread),
        &guidance,
    )
    .unwrap();
    let received = store.db.last_insert_rowid();
    store.db.execute("INSERT INTO coordination(id,source,destination,submission,operation,kind,text) VALUES(?1,?2,?3,?4,?5,'message','preview')", params![CommandId::new(),old.thread,child,id,operation]).unwrap();
    entry(
        &store.db,
        &old.thread,
        "coordination",
        "sent preview",
        EntryData {
            submission: Some(&id),
            operation: Some(&operation),
            ..Default::default()
        },
    )
    .unwrap();
    let sent = store.db.last_insert_rowid();
    for legacy in [false, true] {
        if legacy {
            store
                .db
                .execute("UPDATE entries SET submission=NULL WHERE seq=?1", [sent])
                .unwrap();
        }
        let hits = search_all(
            &store.db,
            &receipt.thread,
            json!({"query":"recollection","limit":1}),
        );
        assert_eq!(hits.iter().filter(|h| h["sequence"].is_i64()).count(), 2);
        assert!(hits.iter().any(|h| h["sequence"] == initial));
        let hit = hits.iter().find(|h| h["sequence"] == received).unwrap();
        assert_eq!(hit["thread_id"], json!(child));
        assert_eq!(hit["routing"]["sender_thread_id"], json!(old.thread));
        assert_eq!(hit["routing"]["recipient_thread_id"], json!(child));
        assert_eq!(hit["routing"]["submission_source"], "coordination");
        store
            .db
            .execute("UPDATE entries SET kind='notice' WHERE seq=?1", [received])
            .unwrap();
        let remaining = search_all(&store.db, &receipt.thread, json!({"query":"guidance"}));
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0]["sequence"], sent);
        assert_eq!(remaining[0]["routing"]["recipient_thread_id"], json!(child));
        store
            .db
            .execute(
                "UPDATE entries SET kind='coordination' WHERE seq=?1",
                [received],
            )
            .unwrap();
    }
    assert!(search_all(
        &store.db,
        &receipt.thread,
        json!({"query":"recollection","workspace":"/different"})
    )
    .is_empty());
    assert_eq!(
        search_all(
            &store.db,
            &receipt.thread,
            json!({"query":"guidance","workspace":"/archive/cross-workspace"})
        )
        .len(),
        1
    );
    let threads = query(
        &store.db,
        &receipt.thread,
        "read_conversation",
        json!({"view":"threads","session_id":old.session}),
    );
    assert_eq!(threads["threads"].as_array().unwrap().len(), 2);
    assert_eq!(threads["threads"][1]["parent_thread_id"], json!(old.thread));
}

#[test]
fn raw_candidate_budget_empty_partial_pages_and_append_snapshot() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/raw-bounds");
    for _ in 0..(MAX_CANDIDATES * 3) {
        entry(
            &store.db,
            &old.thread,
            "tool_result",
            "needle hidden tool marker",
            EntryData::default(),
        )
        .unwrap();
    }
    let original = assistant(&store.db, &old.thread, "needle originally retained");
    let first = query(
        &store.db,
        &receipt.thread,
        "search_conversation",
        json!({"query":"needle"}),
    );
    assert!(first["matches"].as_array().unwrap().is_empty());
    assert_eq!(first["scan_complete"], false);
    assert_eq!(first["examined_entries"], MAX_CANDIDATES);
    assistant(&store.db, &old.thread, "needle concurrent append");
    let hits = search_all(
        &store.db,
        &receipt.thread,
        json!({"query":"needle","cursor":first["next_cursor"]}),
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["sequence"], original);
    assert_eq!(
        search_all(&store.db, &receipt.thread, json!({"query":"needle"})).len(),
        2
    );
    let hidden = query(
        &store.db,
        &receipt.thread,
        "read_conversation",
        json!({"view":"transcript","session_id":old.session,"direction":"oldest_first"}),
    );
    assert!(hidden["entries"].as_array().unwrap().is_empty());
    assert_eq!(hidden["scan_complete"], false);
    assert!(hidden["next_cursor"].is_string());
}

#[test]
fn title_changes_are_fresh_and_title_only_sessions_are_searchable() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/titles");
    store.db.execute("INSERT INTO session_titles(session,thread,title,state) VALUES(?1,?2,'Historical Kusto Query','generated') ON CONFLICT(session) DO UPDATE SET title=excluded.title,state=excluded.state", params![old.session,old.thread]).unwrap();
    let hits = search_all(&store.db, &receipt.thread, json!({"query":"kusto"}));
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["kind"], "title");
    assert_eq!(hits[0]["source"], "session.title");
    assert!(hits[0]["sequence"].is_null());
    assert_eq!(hits[0]["read_ref"]["view"], "transcript");
    store
        .db
        .execute(
            "UPDATE session_titles SET title='Renamed Azure Discussion' WHERE session=?1",
            [&old.session],
        )
        .unwrap();
    assert!(search_all(&store.db, &receipt.thread, json!({"query":"kusto"})).is_empty());
    assert_eq!(
        search_all(&store.db, &receipt.thread, json!({"query":"azure"})).len(),
        1
    );
}

#[test]
fn cursor_query_scope_token_and_target_mismatches_are_rejected() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/cursors");
    let (_, child) = child(&store.db, &old.thread);
    for _ in 0..4 {
        assistant(&store.db, &old.thread, "cursor needle");
    }
    let first = query(
        &store.db,
        &receipt.thread,
        "search_conversation",
        json!({"query":"needle","limit":1}),
    );
    let cursor = &first["next_cursor"];
    for input in [
        json!({"query":"other","cursor":cursor}),
        json!({"query":"needle","workspace":"other","cursor":cursor}),
        json!({"query":"needle","cursor":"junk"}),
        json!({"query":"needle","cursor":"x".repeat(MAX_CURSOR+1)}),
    ] {
        assert!(execute(
            &store.db,
            &receipt.thread,
            TOKEN,
            "search_conversation",
            &input
        )
        .is_err());
    }
    assert!(execute(
        &store.db,
        &receipt.thread,
        &uuid::Uuid::new_v4().to_string(),
        "search_conversation",
        &json!({"query":"needle","cursor":cursor})
    )
    .is_err());
    assert!(execute(
        &store.db,
        &child,
        TOKEN,
        "search_conversation",
        &json!({"query":"needle","cursor":cursor})
    )
    .is_err());
    let first = query(
        &store.db,
        &receipt.thread,
        "read_conversation",
        json!({"view":"transcript","session_id":old.session,"limit":1}),
    );
    let cursor = &first["next_cursor"];
    for input in [
        json!({"view":"transcript","session_id":old.session,"thread_id":child,"cursor":cursor}),
        json!({"view":"transcript","session_id":old.session,"direction":"oldest_first","cursor":cursor}),
        json!({"view":"threads","session_id":old.session,"cursor":cursor}),
        json!({"view":"transcript","session_id":receipt.session,"cursor":cursor}),
    ] {
        assert!(execute(
            &store.db,
            &receipt.thread,
            TOKEN,
            "read_conversation",
            &input
        )
        .is_err());
    }
}

#[test]
fn missing_corrupt_oversized_and_preview_sources_are_reported() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/retention");
    entry(
        &store.db,
        &old.thread,
        "assistant",
        "previewneedle",
        EntryData::default(),
    )
    .unwrap();
    let preview = store.db.last_insert_rowid();
    let missing = assistant(&store.db, &old.thread, "missingneedle");
    store.db.execute("PRAGMA foreign_keys=OFF", []).unwrap();
    store
        .db
        .execute(
            "UPDATE entries SET model=?1 WHERE seq=?2",
            params![uuid::Uuid::new_v4().to_string(), missing],
        )
        .unwrap();
    store.db.execute("PRAGMA foreign_keys=ON", []).unwrap();
    let corrupt = assistant(&store.db, &old.thread, "corruptneedle");
    let corrupt_id: String = store
        .db
        .query_row("SELECT model FROM entries WHERE seq=?1", [corrupt], |r| {
            r.get(0)
        })
        .unwrap();
    store
        .db
        .execute(
            "UPDATE chunks SET data=?1 WHERE artifact=?2",
            params![b"not json".as_slice(), corrupt_id],
        )
        .unwrap();
    let oversized = assistant(&store.db, &old.thread, "oversizedneedle");
    store
        .db
        .execute(
            "UPDATE artifacts SET length=?1 WHERE id=(SELECT model FROM entries WHERE seq=?2)",
            params![projection::MAX_SOURCE_BYTES + 1, oversized],
        )
        .unwrap();
    let page = query(
        &store.db,
        &receipt.thread,
        "search_conversation",
        json!({"query":"needle"}),
    );
    assert_eq!(page["matches"].as_array().unwrap().len(), 4);
    for code in [
        "preview_only",
        "source_missing",
        "source_corrupt",
        "source_oversized",
    ] {
        assert!(
            page["limitations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["code"] == code),
            "{code}"
        );
    }
    for sequence in [preview, missing, corrupt, oversized] {
        let read = query(
            &store.db,
            &receipt.thread,
            "read_conversation",
            json!({"view":"entry","session_id":old.session,"sequence":sequence}),
        );
        assert_eq!(read["preview_only"], true);
        assert_eq!(read["retention"], "preview_only");
        assert!(read["original_complete"].is_null());
        assert!(!read["limitations"].as_array().unwrap().is_empty());
    }
    let no_match = query(
        &store.db,
        &receipt.thread,
        "search_conversation",
        json!({"query":"notretained"}),
    );
    assert_eq!(no_match["scan_complete"], true);
    assert!(!no_match["limitations"].as_array().unwrap().is_empty());
}

#[test]
fn byte_budget_and_encoded_output_budget_never_drop_hits() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/byte-bounds");
    let mut expected = BTreeSet::new();
    for _ in 0..3 {
        expected.insert(assistant(
            &store.db,
            &old.thread,
            &format!("{} budgetneedle", "a".repeat(3 * 1024 * 1024)),
        ));
    }
    for _ in 0..70 {
        expected.insert(assistant(
            &store.db,
            &old.thread,
            &format!("budgetneedle {}", "\u{0}\"\\雪".repeat(100)),
        ));
    }
    let first = query(
        &store.db,
        &receipt.thread,
        "search_conversation",
        json!({"query":"budgetneedle","limit":50}),
    );
    assert_eq!(first["scan_complete"], false);
    assert_eq!(first["matches"].as_array().unwrap().len(), 2);
    let all = search_all(
        &store.db,
        &receipt.thread,
        json!({"query":"budgetneedle","limit":50}),
    );
    let actual: BTreeSet<_> = all
        .iter()
        .map(|h| h["sequence"].as_i64().unwrap())
        .collect();
    assert_eq!(all.len(), expected.len());
    assert_eq!(actual, expected);
}

#[test]
fn unicode_entry_pages_relationships_and_around_context() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/unicode");
    let before = assistant(&store.db, &old.thread, "before match");
    let text = "雪🦀\n\"\\\u{0} λİ".repeat(3000);
    let sequence = assistant(&store.db, &old.thread, &text);
    let after = assistant(&store.db, &old.thread, "after match");
    let mut restored = String::new();
    loop {
        let page = query(
            &store.db,
            &receipt.thread,
            "read_conversation",
            json!({"view":"entry","session_id":old.session,"sequence":sequence,"offset":restored.len(),"limit_bytes":1024}),
        );
        restored.push_str(page["text"].as_str().unwrap());
        if page["next_offset"].is_null() {
            break;
        }
        assert_eq!(page["next_offset"], restored.len());
    }
    assert_eq!(restored, text);
    for input in [
        json!({"view":"entry","session_id":old.session,"sequence":sequence,"offset":1}),
        json!({"view":"entry","session_id":receipt.session,"sequence":sequence}),
        json!({"view":"entry","session_id":old.session,"thread_id":receipt.thread,"sequence":sequence}),
        json!({"view":"artifact","session_id":old.session,"artifact_id":CommandId::new()}),
        json!({"view":"tool_output","session_id":old.session,"operation_id":OperationId::new()}),
    ] {
        assert!(execute(
            &store.db,
            &receipt.thread,
            TOKEN,
            "read_conversation",
            &input
        )
        .is_err());
    }
    let context = query(
        &store.db,
        &receipt.thread,
        "read_conversation",
        json!({"view":"transcript","session_id":old.session,"direction":"oldest_first","around_sequence":sequence,"limit":3}),
    );
    assert_eq!(
        context["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["sequence"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        vec![before, sequence, after]
    );
    let (snippet, offset, _) = match_text(
        &format!("{}İstanbul Snow", "🦀".repeat(1000)),
        &["i\u{307}stanbul".into()],
        "i\u{307}stanbul",
    )
    .unwrap();
    assert!(offset > 2048);
    assert!(snippet.contains("İstanbul"));
}

#[test]
fn around_context_skips_tool_only_turns_and_reports_bounded_backtracking() {
    for tool_turns in [40, 300] {
        let (_dir, mut store, receipt, models) = fixture();
        let old = other(&mut store, &receipt, &models, "/archive/around-tools");
        let before = assistant(&store.db, &old.thread, "preceding visible context");
        for _ in 0..tool_turns {
            entry(
                &store.db,
                &old.thread,
                "tool_result",
                "PRIVATE_TOOL_LOG",
                EntryData::default(),
            )
            .unwrap();
            assistant(&store.db, &old.thread, "");
        }
        let found = assistant(&store.db, &old.thread, "matched visible passage");
        let after = assistant(&store.db, &old.thread, "following visible context");
        let page = query(
            &store.db,
            &receipt.thread,
            "read_conversation",
            json!({
                "view":"transcript", "session_id":old.session,
                "direction":"oldest_first", "around_sequence":found, "limit":5
            }),
        );
        let sequences = page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["sequence"].as_i64().unwrap())
            .collect::<Vec<_>>();
        if tool_turns == 40 {
            assert_eq!(sequences, vec![before, found, after]);
            assert!(page["limitations"].as_array().unwrap().is_empty());
        } else {
            assert_eq!(sequences, vec![found, after]);
            assert!(page["limitations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|note| note["code"] == "preceding_context_limited"));
        }
        assert!(page["examined_entries"].as_u64().unwrap() <= MAX_CANDIDATES as u64);
        assert!(!page.to_string().contains("PRIVATE_TOOL"));
    }
}

#[test]
fn catalog_reopen_compaction_and_missing_provider_do_not_mutate_target() {
    let (dir, mut store, receipt, models) = fixture();
    let old = other(
        &mut store,
        &receipt,
        &models,
        "/workspace/that/no/longer/exists",
    );
    let sequence = assistant(&store.db, &old.thread, "checkpoint retained needle");
    assistant(&store.db, &old.thread, "second retained needle");
    let snapshot =
        crate::context::snapshot(&store.db, &old.thread, &models[0], &Default::default()).unwrap();
    let tx = store.db.transaction().unwrap();
    crate::context::checkpoint(
        &tx,
        &old.thread,
        &OperationId::new(),
        snapshot.epoch,
        snapshot.revision,
        &[Message::user_text("compacted summary")],
    )
    .unwrap();
    tx.commit().unwrap();
    store
        .db
        .execute(
            "UPDATE threads SET held=1,profile='not even valid provider JSON' WHERE id=?1",
            [&old.thread],
        )
        .unwrap();
    let run = RunId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,'interrupted')",
            params![run, old.thread],
        )
        .unwrap();
    submit(
        &store.db,
        &CommandId::new(),
        &old.thread,
        "human",
        None,
        "queued historical work",
    )
    .unwrap();
    let catalog = SessionCatalog::open(&dir.0.join("lab.db")).unwrap();
    let baseline: i64 = store
        .db
        .query_row("SELECT total_changes()", [], |r| r.get(0))
        .unwrap();
    let first: Value = serde_json::from_str(
        &catalog
            .search_conversation(&receipt.thread, &json!({"query":"needle","limit":1}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first["matches"][0]["sequence"], sequence);
    let text: Value = serde_json::from_str(
        &catalog
            .read_conversation(
                &receipt.thread,
                &json!({"view":"entry","session_id":old.session,"sequence":sequence}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(text["text"], "checkpoint retained needle");
    assert_eq!(
        store
            .db
            .query_row("SELECT total_changes()", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        baseline
    );
    drop(catalog);
    let catalog = SessionCatalog::open(&dir.0.join("lab.db")).unwrap();
    assert!(catalog
        .search_conversation(
            &receipt.thread,
            &json!({"query":"needle","cursor":first["next_cursor"]})
        )
        .is_err());
    assert!(catalog
        .read_conversation(
            &receipt.thread,
            &json!({"view":"transcript","session_id":old.session})
        )
        .is_ok());
    let state: (String,i64,String) = store.db.query_row("SELECT r.state,t.held,t.profile FROM runs r JOIN threads t ON t.id=r.thread WHERE r.id=?1", [&run], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(
        state,
        (
            "interrupted".into(),
            1,
            "not even valid provider JSON".into()
        )
    );
    let queued: i64 = store
        .db
        .query_row(
            "SELECT COUNT(*) FROM submissions WHERE thread=?1 AND state='queued'",
            [&old.thread],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(queued, 1);
    drop(catalog);
    drop(store);
    let catalog = SessionCatalog::open(&dir.0.join("lab.db")).unwrap();
    let found: Value = serde_json::from_str(
        &catalog
            .search_conversation(&receipt.thread, &json!({"query":"checkpoint"}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(found["matches"][0]["sequence"], sequence);
}

#[test]
fn schemas_and_literal_and_page_local_phrase_preference() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/literal");
    let loose = assistant(&store.db, &old.thread, "alpha separated beta [x]");
    let phrase = assistant(&store.db, &old.thread, "ALPHA BETA [x]");
    let hits = search_all(&store.db, &receipt.thread, json!({"query":"alpha beta"}));
    assert_eq!(hits[0]["sequence"], phrase);
    assert_eq!(hits[1]["sequence"], loose);
    assert_eq!(
        search_all(&store.db, &receipt.thread, json!({"query":"[x]"})).len(),
        2
    );
    assert!(search_all(&store.db, &receipt.thread, json!({"query":"alpha absent"})).is_empty());
    let first = query(
        &store.db,
        &receipt.thread,
        "search_conversation",
        json!({"query":"alpha beta","limit":1}),
    );
    assert_eq!(first["matches"][0]["sequence"], loose);
    for tool in tools() {
        assert_eq!(tool.effect, EffectClass::Read);
        let schema = jsonschema::validator_for(&tool.spec.input_schema).unwrap();
        let input = if tool.spec.name == "search_conversation" {
            json!({"query":"needle"})
        } else {
            json!({"view":"entry","session_id":old.session,"sequence":loose})
        };
        assert!(schema.is_valid(&input));
        let mut bad = input;
        bad["db_path"] = json!("private.sqlite");
        assert!(!schema.is_valid(&bad));
        assert!(execute(&store.db, &receipt.thread, TOKEN, &tool.spec.name, &bad).is_err());
    }
}

#[test]
fn corrupt_chunk_size_is_rejected_before_fetching_blob() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/corrupt-chunk");
    let sequence = assistant(&store.db, &old.thread, "chunkneedle preview");
    let id: String = store
        .db
        .query_row("SELECT model FROM entries WHERE seq=?1", [sequence], |r| {
            r.get(0)
        })
        .unwrap();
    store
        .db
        .execute("UPDATE artifacts SET length=1 WHERE id=?1", [&id])
        .unwrap();
    store
        .db
        .execute(
            "UPDATE chunks SET data=zeroblob(?1) WHERE artifact=?2",
            params![projection::MAX_SOURCE_BYTES * 2, id],
        )
        .unwrap();
    let page = query(
        &store.db,
        &receipt.thread,
        "search_conversation",
        json!({"query":"chunkneedle"}),
    );
    assert_eq!(page["matches"][0]["preview_only"], true);
    assert!(page["limitations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["code"] == "source_corrupt"));
    assert!(page["examined_bytes"].as_u64().unwrap() < 1024);
}

#[test]
fn fallback_titles_do_not_search_unbounded_root_history() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/no-stored-title");
    store
        .db
        .execute(
            "DELETE FROM session_titles WHERE session=?1",
            [&old.session],
        )
        .unwrap();
    for _ in 0..300 {
        entry(
            &store.db,
            &old.thread,
            "notice",
            "no human title here",
            EntryData::default(),
        )
        .unwrap();
    }
    submit(
        &store.db,
        &CommandId::new(),
        &old.thread,
        "human",
        None,
        "latehumanneedle prompt",
    )
    .unwrap();
    let page = query(
        &store.db,
        &receipt.thread,
        "search_conversation",
        json!({"query":"latehumanneedle"}),
    );
    assert_eq!(page["scan_complete"], false);
    assert!(page["matches"].as_array().unwrap().is_empty());
    assert!(page["limitations"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l["code"] == "metadata_truncated"));
    let hits = search_all(
        &store.db,
        &receipt.thread,
        json!({"query":"latehumanneedle"}),
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["kind"], "submission");
}

#[test]
fn synthetic_scan_reports_bounded_work_and_measurement() {
    let (_dir, mut store, receipt, models) = fixture();
    let old = other(&mut store, &receipt, &models, "/archive/benchmark");
    let tx = store.db.transaction().unwrap();
    for _ in 0..1024 {
        assistant(
            &tx,
            &old.thread,
            "Representative retained conversation text with no requested term.",
        );
    }
    assistant(&tx, &old.thread, "syntheticbenchmarkneedle");
    tx.commit().unwrap();
    let started = std::time::Instant::now();
    let hits = search_all(
        &store.db,
        &receipt.thread,
        json!({"query":"syntheticbenchmarkneedle"}),
    );
    assert_eq!(hits.len(), 1);
    eprintln!(
        "bounded archive scan: 1025 visible messages, elapsed {:?}",
        started.elapsed()
    );
}
