use crate::model::*;
use crate::presentation::*;
use crate::store::{artifact, encode, entry, finish_tool_entry, tests::fixture, EntryData, Store};
use bob_types::{ContentBlock, Message, Role};
use rusqlite::{params, types::Value as SqlValue};
use serde_json::{json, Value};

fn seed_submission(
    store: &Store,
    thread: &ThreadId,
    source: &str,
    sender: Option<&ThreadId>,
    text: &str,
) -> i64 {
    crate::store::submit(&store.db, &CommandId::new(), thread, source, sender, text).unwrap();
    store.db.last_insert_rowid()
}

#[test]
fn coordination_senders_project_in_both_directions_without_changing_content() {
    let (dir, store, receipt, _) = fixture();
    let run = seed_run(&store, &receipt.thread, "active");
    let child = seed_child(&store, &receipt.thread, &run);
    store
        .db
        .execute(
            "UPDATE threads SET name='coordinator' WHERE id=?1",
            [&receipt.thread],
        )
        .unwrap();
    let catalog = crate::SessionCatalog::open(&dir.0.join("lab.db")).unwrap();
    for (recipient, sender, name, text) in [
        (
            &child,
            &receipt.thread,
            "coordinator",
            "Please inspect the change",
        ),
        (&receipt.thread, &child, "reviewer", "Review complete"),
    ] {
        let id = CommandId::new();
        crate::store::submit(
            &store.db,
            &id,
            recipient,
            "coordination",
            Some(sender),
            text,
        )
        .unwrap();
        let sequence = store.db.last_insert_rowid();
        let recipient_run = seed_run(&store, recipient, "completed");
        crate::store::consume(&store.db, &id, &recipient_run).unwrap();
        let records = durable_records(&store);
        for page in [
            store.view(Some(recipient.clone()), None, 1, None).unwrap(),
            store
                .transcript(Some(recipient.clone()), None, 1, None)
                .unwrap(),
            catalog.view(Some(recipient.clone()), None, 1).unwrap(),
            catalog
                .transcript(Some(recipient.clone()), None, 1)
                .unwrap(),
            catalog
                .transcript_after(Some(recipient.clone()), sequence - 1, 1)
                .unwrap(),
        ] {
            assert_eq!(page.entries.len(), 1);
            let entry = &page.entries[0];
            assert_eq!(entry.sequence, sequence);
            assert_eq!(entry.kind, EntryKind::Coordination);
            assert_eq!(
                entry.sender,
                Some(EntrySender {
                    thread: sender.clone(),
                    name: Some(name.into()),
                })
            );
            assert_eq!(entry.text, text);
            assert!(!entry.visible_in_root());
            assert_eq!(inspect(&store, recipient, sequence, 73), text);
            let decoded: Entry = serde_json::from_value(json!(entry)).unwrap();
            assert_eq!(decoded.sender, entry.sender);
        }
        assert_eq!(durable_records(&store), records);
    }
}

#[test]
fn coordination_sender_projection_tolerates_missing_references_and_runtime_notices() {
    let (_dir, store, receipt, _) = fixture();
    seed_submission(
        &store,
        &receipt.thread,
        "coordination",
        None,
        "Runtime notice",
    );
    entry(
        &store.db,
        &receipt.thread,
        "coordination",
        "Outgoing notice",
        EntryData::default(),
    )
    .unwrap();
    entry(
        &store.db,
        &receipt.thread,
        "coordination",
        "Missing submission",
        EntryData {
            submission: Some(&CommandId::new()),
            ..Default::default()
        },
    )
    .unwrap();
    let missing = ThreadId::new();
    seed_submission(
        &store,
        &receipt.thread,
        "coordination",
        Some(&missing),
        "Unknown sender",
    );
    for source in ["human", "recovery"] {
        seed_submission(
            &store,
            &receipt.thread,
            source,
            Some(&receipt.thread),
            source,
        );
    }
    let page = store
        .transcript(Some(receipt.thread.clone()), None, 100, None)
        .unwrap();
    assert_eq!(page.entries.len(), 6);
    for (index, entry) in page.entries.iter().enumerate() {
        let value = json!(entry);
        if index == 3 {
            assert_eq!(
                entry.sender,
                Some(EntrySender {
                    thread: missing.clone(),
                    name: None
                })
            );
            assert_eq!(value["sender"], json!({"thread": missing}));
            let sender: EntrySender = serde_json::from_value(value["sender"].clone()).unwrap();
            assert_eq!(Some(sender), entry.sender);
        } else {
            assert_eq!(entry.sender, None);
            assert!(value.get("sender").is_none());
        }
        let mut without_sender = value;
        without_sender.as_object_mut().unwrap().remove("sender");
        assert!(serde_json::from_value::<Entry>(without_sender)
            .unwrap()
            .sender
            .is_none());
        assert_eq!(entry.visible_in_root(), index >= 4);
    }
}

#[test]
fn coordination_sender_names_are_bounded_and_independent_of_thread_window_and_paging() {
    let (dir, store, receipt, _) = fixture();
    let run = seed_run(&store, &receipt.thread, "completed");
    let child = seed_child(&store, &receipt.thread, &run);
    let name = "λ🦀".repeat(4096);
    store
        .db
        .execute(
            "UPDATE threads SET name=?1 WHERE id=?2",
            params![name, child],
        )
        .unwrap();
    store
        .db
        .execute(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1001)
         INSERT INTO threads(id,session,parent_run,name,profile)
         SELECT 'newer-'||x,session,?2,'newer',profile FROM n CROSS JOIN threads WHERE id=?1",
            params![receipt.thread, run],
        )
        .unwrap();
    let expected: Vec<_> = (0..11)
        .map(|index| {
            seed_submission(
                &store,
                &receipt.thread,
                "coordination",
                Some(&child),
                &format!("message {index}"),
            )
        })
        .collect();
    let records = durable_records(&store);
    let catalog = crate::SessionCatalog::open(&dir.0.join("lab.db")).unwrap();
    let expected_sender = Some(EntrySender {
        thread: child.clone(),
        name: Some(name.chars().take(256).collect()),
    });
    for ascending in [false, true] {
        let mut cursor = if ascending { 0 } else { i64::MAX };
        let mut sequences = Vec::new();
        loop {
            let page = if ascending {
                catalog
                    .transcript_after(Some(receipt.thread.clone()), cursor, 3)
                    .unwrap()
            } else {
                catalog
                    .transcript(Some(receipt.thread.clone()), Some(cursor), 3)
                    .unwrap()
            };
            assert!(page.threads.iter().all(|thread| thread.id != child));
            assert!(page.entries.len() <= 3);
            if page.entries.is_empty() {
                break;
            }
            for entry in &page.entries {
                assert_eq!(entry.sender, expected_sender);
                assert!(encode(&entry.sender).unwrap().len() < 2048);
            }
            cursor = if ascending {
                page.entries.last().unwrap().sequence
            } else {
                page.entries[0].sequence
            };
            sequences.extend(page.entries.iter().map(|entry| entry.sequence));
        }
        sequences.sort_unstable();
        assert_eq!(sequences, expected);
    }
    assert_eq!(durable_records(&store), records);
}

#[test]
fn coordination_sender_page_work_is_independent_of_history_size() {
    use rusqlite::StatementStatus;

    let mut previous_steps = None;
    for size in [100, 10_000] {
        let (_dir, store, receipt, _) = fixture();
        store
            .db
            .execute(
                "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<?2)
             INSERT INTO submissions(id,thread,source,sender,text,state)
             SELECT 'message-'||x,?1,'coordination',?1,'hello','queued' FROM n",
                params![receipt.thread, size],
            )
            .unwrap();
        store
            .db
            .execute(
                "INSERT INTO entries(thread,kind,text,submission)
             SELECT thread,'coordination',text,id FROM submissions",
                [],
            )
            .unwrap();
        let mut steps = Vec::new();
        for sql in [
            crate::store::PAGE_SQL.to_owned(),
            crate::store::PAGE_SQL.replace(
                "e.seq<?2 ORDER BY e.seq DESC",
                "e.seq>?2 ORDER BY e.seq ASC",
            ),
        ] {
            let mut statement = store.db.prepare(&sql).unwrap();
            let senders: Vec<String> = statement
                .query_map(params![receipt.thread, size / 2, 7], |row| row.get(11))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(senders, vec![receipt.thread.0.clone(); 7]);
            assert_eq!(statement.get_status(StatementStatus::FullscanStep), 0);
            assert_eq!(statement.get_status(StatementStatus::Sort), 0);
            steps.push(statement.get_status(StatementStatus::VmStep));
        }
        if let Some(previous) = previous_steps {
            assert_eq!(steps, previous);
        }
        previous_steps = Some(steps);
    }
}

#[test]
fn forward_transcript_returns_nearest_entries_and_empty_exhaustion() {
    let (_dir, store, receipt, _) = fixture();
    for n in 0..205 {
        entry(
            &store.db,
            &receipt.thread,
            "notice",
            &format!("row {n}"),
            EntryData::default(),
        )
        .unwrap();
    }
    let mut after = 0;
    let mut sequences = Vec::new();
    loop {
        let page = store
            .transcript_after(Some(receipt.thread.clone()), after, 37, None)
            .unwrap();
        assert!(page.operations.is_empty());
        assert!(page.entries.len() <= 37);
        if page.entries.is_empty() {
            break;
        }
        for entry in page.entries {
            assert!(entry.sequence > after);
            after = entry.sequence;
            sequences.push(after);
        }
    }
    assert_eq!(sequences.len(), 205);
    let query = crate::store::PAGE_SQL.replace(
        "e.seq<?2 ORDER BY e.seq DESC",
        "e.seq>?2 ORDER BY e.seq ASC",
    );
    let mut stmt = store
        .db
        .prepare(&format!("EXPLAIN QUERY PLAN {query}"))
        .unwrap();
    let plan: Vec<String> = stmt
        .query_map(params![receipt.thread, 0, 37], |r| r.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| line.contains("thread_entries") && line.contains("seq>?")),
        "{plan:?}"
    );
    assert!(
        !plan.iter().any(|line| line.contains("TEMP B-TREE")),
        "{plan:?}"
    );
}

#[test]
fn tool_output_pages_human_artifact_and_falls_back_without_mutation() {
    let (_dir, store, receipt, _) = fixture();
    let run = seed_run(&store, &receipt.thread, "active");
    let (operation, _) = seed_tool(
        &store,
        &receipt.thread,
        &run,
        "bash",
        "started",
        &json!({"command":"test"}),
    );
    finish_tool_entry(&store.db, &operation, "raw json", false).unwrap();
    assert_eq!(
        store.tool_output(&operation, 0, 100).unwrap().text,
        "raw json"
    );
    let human = "λ full output\n".repeat(10000);
    let display = artifact(&store.db, &human).unwrap();
    store
        .db
        .execute(
            "UPDATE operations SET display_result=?1 WHERE id=?2",
            params![display, operation],
        )
        .unwrap();
    let before = durable_records(&store);
    let mut offset = 0;
    let mut collected = String::new();
    loop {
        let page = store.tool_output(&operation, offset, 73).unwrap();
        assert!(page.text.len() <= 73);
        assert_eq!(page.total_bytes, human.len());
        collected.push_str(&page.text);
        if let Some(next) = page.next_offset {
            offset = next;
        } else {
            break;
        }
    }
    assert_eq!(collected, human);
    assert_eq!(before, durable_records(&store));
    assert!(store.tool_output(&operation, 1, 73).is_err());
}

fn seed_run(store: &Store, thread: &ThreadId, state: &str) -> RunId {
    let run = RunId::new();
    store
        .db
        .execute(
            "INSERT INTO runs(id,thread,state) VALUES(?1,?2,?3)",
            params![run, thread, state],
        )
        .unwrap();
    run
}

fn seed_child(store: &Store, parent: &ThreadId, run: &RunId) -> ThreadId {
    let child = ThreadId::new();
    store
        .db
        .execute(
            "INSERT INTO threads(id,session,parent_run,name,profile) SELECT ?1,session,?2,'reviewer',profile FROM threads WHERE id=?3",
            params![child, run, parent],
        )
        .unwrap();
    child
}

fn seed_tool(
    store: &Store,
    thread: &ThreadId,
    run: &RunId,
    name: &str,
    state: &str,
    input: &Value,
) -> (OperationId, i64) {
    let operation = OperationId::new();
    let input = encode(input).unwrap();
    store
        .db
        .execute(
            "INSERT INTO operations(id,run,thread,name,kind,call_id,input,state) VALUES(?1,?2,?3,?4,'tool',?1,?5,?6)",
            params![operation, run, thread, name, input, state],
        )
        .unwrap();
    entry(
        &store.db,
        thread,
        "tool_call",
        &format!("{name} {input}"),
        EntryData {
            operation: Some(&operation),
            ..Default::default()
        },
    )
    .unwrap();
    (operation, store.db.last_insert_rowid())
}

fn seed_decision(store: &Store, operation: &OperationId) {
    store
        .db
        .execute(
            "UPDATE sessions SET revision=7 WHERE id=(SELECT t.session FROM threads t JOIN operations o ON o.thread=t.id WHERE o.id=?1)",
            [operation],
        )
        .unwrap();
    store
        .db
        .execute(
            "UPDATE operations SET policy_revision=7 WHERE id=?1",
            [operation],
        )
        .unwrap();
    store
        .db
        .execute(
            "INSERT INTO decisions(operation,revision,preview,state) VALUES(?1,7,'Write src/main.rs','pending')",
            [operation],
        )
        .unwrap();
}

fn inspect(store: &Store, thread: &ThreadId, sequence: i64, limit: usize) -> String {
    let mut text = String::new();
    let mut offset = 0;
    loop {
        let page = store.entry_text(thread, sequence, offset, limit).unwrap();
        assert!(page.text.len() <= limit.clamp(4, 65536));
        text.push_str(&page.text);
        if let Some(next) = page.next_offset {
            assert!(next > offset);
            assert_eq!(next, text.len());
            assert!(next < page.total_bytes);
            offset = next;
        } else {
            assert_eq!(text.len(), page.total_bytes);
            return text;
        }
    }
}

fn durable_records(store: &Store) -> Vec<Vec<Vec<SqlValue>>> {
    [
        "sessions",
        "threads",
        "runs",
        "commands",
        "submissions",
        "artifacts",
        "chunks",
        "entries",
        "operations",
        "decisions",
        "changes",
        "sqlite_sequence",
    ]
    .into_iter()
    .map(|table| {
        let mut statement = store
            .db
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
            .unwrap();
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                (0..columns).map(|column| row.get(column)).collect()
            })
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    })
    .collect()
}

#[test]
fn old_pages_join_their_tools_independently_of_the_diagnostic_window() {
    let (_dir, store, receipt, _models) = fixture();
    let run = seed_run(&store, &receipt.thread, "active");
    let mut expected = Vec::new();
    for index in 0..107 {
        let name = if index % 2 == 0 {
            "read_file"
        } else {
            "edit_file"
        };
        let state = match index % 4 {
            0 => "done",
            1 => "failed",
            2 => "unknown",
            _ => "cancelled",
        };
        let input = json!({"path": format!("src/file-{index}.rs"), "offset": index + 1});
        let (operation, sequence) = seed_tool(&store, &receipt.thread, &run, name, state, &input);
        expected.push((operation, sequence, name, state, input));
    }

    let raw = store
        .view(Some(receipt.thread.clone()), None, 100, None)
        .unwrap();
    assert_eq!(raw.operations.len(), 100);
    assert!(expected[..7]
        .iter()
        .all(|(id, ..)| raw.operations.iter().all(|operation| operation.id != *id)));

    let mut before = None;
    let mut end = expected.len();
    while end > 0 {
        let page = store
            .transcript(Some(receipt.thread.clone()), before, 7, None)
            .unwrap();
        assert!(page.operations.is_empty());
        let start = end.saturating_sub(7);
        assert_eq!(page.entries.len(), end - start);
        for (entry, (id, sequence, name, state, input)) in
            page.entries.iter().zip(&expected[start..end])
        {
            assert_eq!(entry.sequence, *sequence);
            assert_eq!(entry.thread, receipt.thread);
            assert_eq!(entry.operation.as_ref(), Some(id));
            assert_eq!(entry.kind, EntryKind::ToolCall);
            assert!(!entry.is_preview);
            let tool = entry.tool.as_ref().unwrap();
            assert_eq!(&tool.id, id);
            assert_eq!(tool.name, *name);
            assert_eq!(tool.state.as_str(), *state);
            assert_eq!(tool.presentation, None);
            assert_eq!(&tool.input_preview, input);
        }
        before = Some(page.entries[0].sequence);
        end = start;
    }
    assert!(store
        .transcript(Some(receipt.thread.clone()), before, 7, None)
        .unwrap()
        .entries
        .is_empty());
    let minimum = store
        .transcript(Some(receipt.thread.clone()), None, 0, None)
        .unwrap();
    assert_eq!(minimum.entries.len(), 1);
    assert_eq!(minimum.entries[0].sequence, expected[106].1);
    let maximum = store
        .transcript(Some(receipt.thread.clone()), None, usize::MAX, None)
        .unwrap();
    assert_eq!(maximum.entries.len(), 100);
    assert_eq!(maximum.entries[0].sequence, expected[7].1);
    assert_eq!(maximum.entries[99].sequence, expected[106].1);
}

#[test]
fn large_old_and_new_inputs_have_bounded_previews_and_exact_inspection() {
    let (dir, store, receipt, _models) = fixture();
    let run = seed_run(&store, &receipt.thread, "active");
    let input = json!({
        "content": "λ🦀\n\"replacement\"".repeat(16_384),
        "path": "src/日本語/main.rs",
        "command": "cargo check",
    });
    let specification = json!({"original": "unchanged file content\n".repeat(65_536)});
    let mut retained = Vec::new();
    for age in ["old", "new"] {
        let (operation, sequence) = seed_tool(
            &store,
            &receipt.thread,
            &run,
            "write_file",
            "approval",
            &input,
        );
        let prepared = PreparedTool {
            invocation: Invocation {
                operation_id: operation.clone(),
                run_id: run.clone(),
                thread_id: receipt.thread.clone(),
                workspace: dir.0.to_string_lossy().into_owned(),
                name: "write_file".into(),
                input: input.clone(),
            },
            specification: specification.clone(),
            preview: "Write src/日本語/main.rs".into(),
            presentation: None,
        };
        let frozen = encode(&prepared).unwrap();
        store
            .db
            .execute(
                "UPDATE operations SET prepared=?1 WHERE id=?2",
                params![frozen, operation],
            )
            .unwrap();
        let raw = store
            .view(Some(receipt.thread.clone()), None, 1, None)
            .unwrap();
        assert_eq!(raw.operations[0].id, operation);
        assert_eq!(
            raw.operations[0].specification,
            json!({"preview_truncated":true})
        );
        retained.push((operation, sequence, frozen));
        if age == "old" {
            for index in 0..101 {
                seed_tool(
                    &store,
                    &receipt.thread,
                    &run,
                    "read_file",
                    "done",
                    &json!({"path": format!("src/{index}.rs")}),
                );
            }
        }
    }

    for (operation, sequence, frozen) in &retained {
        let page = store
            .transcript(Some(receipt.thread.clone()), Some(sequence + 1), 1, None)
            .unwrap();
        assert!(page.operations.is_empty());
        assert_eq!(page.entries[0].sequence, *sequence);
        assert!(page.entries[0].is_preview);
        let tool = page.entries[0].tool.as_ref().unwrap();
        assert_eq!(&tool.id, operation);
        assert_eq!(tool.input_preview["path"], input["path"]);
        assert_eq!(tool.input_preview["command"], input["command"]);
        assert_eq!(tool.input_preview["preview_truncated"], true);
        let encoded = encode(&tool.input_preview).unwrap();
        assert!(encoded.len() <= 4096);
        assert_eq!(
            serde_json::from_str::<Value>(&encoded).unwrap(),
            tool.input_preview
        );
        assert!(encode(&page).unwrap().len() < 32 * 1024);
        assert_eq!(
            inspect(&store, &receipt.thread, *sequence, 65536),
            format!("write_file · approval\nOperation: {operation}\n{frozen}")
        );
    }
    let raw = store
        .view(Some(receipt.thread.clone()), None, 1, None)
        .unwrap();
    assert!(raw
        .operations
        .iter()
        .all(|operation| operation.id != retained[0].0));
    assert_eq!(raw.operations[0].id, retained[1].0);
    assert_eq!(
        raw.operations[0].specification,
        json!({"preview_truncated":true})
    );
    assert!(encode(&raw).unwrap().len() < 256 * 1024);
    assert_eq!(
        inspect(&store, &receipt.thread, retained[1].1, 65536),
        format!(
            "write_file · approval\nOperation: {}\n{}",
            retained[1].0, retained[1].2
        )
    );
}

#[test]
fn entry_inspection_pages_readable_unicode_bodies_and_keeps_stable_targets() {
    let (_dir, mut store, receipt, models) = fixture();
    let user_text = "🦀 café 日本語\n\"quoted\" \\path\n".repeat(1_500);
    let text = user_text.repeat(2);
    store
        .command(
            &Command::Submit {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                text: user_text.clone(),
            },
            &models,
        )
        .unwrap();
    let user_sequence = store
        .transcript(Some(receipt.thread.clone()), None, 1, None)
        .unwrap()
        .entries[0]
        .sequence;
    let run = seed_run(&store, &receipt.thread, "active");
    let child = seed_child(&store, &receipt.thread, &run);
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Text { text: text.clone() },
            ContentBlock::Text {
                text: "終わり".into(),
            },
        ],
    };
    let assistant_text = format!("{text}終わり");
    let result = artifact(&store.db, &encode(&message).unwrap()).unwrap();
    entry(
        &store.db,
        &receipt.thread,
        "assistant",
        &assistant_text,
        EntryData {
            message: Some(&message),
            result: Some(&result),
            ..Default::default()
        },
    )
    .unwrap();
    let assistant_sequence = store.db.last_insert_rowid();
    let (operation, call_sequence) = seed_tool(
        &store,
        &receipt.thread,
        &run,
        "bash",
        "unknown",
        &json!({"command": "cargo check"}),
    );
    finish_tool_entry(&store.db, &operation, &text, true).unwrap();
    let tool_sequence = store.db.last_insert_rowid();
    let (cancelled, _) = seed_tool(
        &store,
        &receipt.thread,
        &run,
        "edit_file",
        "cancelled",
        &json!({"path": "src/main.rs"}),
    );
    entry(
        &store.db,
        &child,
        "notice",
        "child text",
        EntryData::default(),
    )
    .unwrap();
    let child_sequence = store.db.last_insert_rowid();

    for (sequence, expected) in [
        (user_sequence, user_text.as_str()),
        (assistant_sequence, assistant_text.as_str()),
        (tool_sequence, text.as_str()),
    ] {
        for limit in [4097, usize::MAX] {
            assert_eq!(inspect(&store, &receipt.thread, sequence, limit), expected);
        }
        let first = store.entry_text(&receipt.thread, sequence, 0, 0).unwrap();
        assert_eq!(first.text, "🦀");
        assert_eq!(first.next_offset, Some(4));
        assert_eq!(first.total_bytes, expected.len());
        let end = store
            .entry_text(&receipt.thread, sequence, expected.len(), 4)
            .unwrap();
        assert!(end.text.is_empty());
        assert_eq!(end.next_offset, None);
        assert_eq!(end.total_bytes, expected.len());
        for offset in [1, 2, 3, expected.len() + 1, usize::MAX] {
            assert!(store
                .entry_text(&receipt.thread, sequence, offset, 4)
                .is_err());
        }
        assert!(store.entry_text(&child, sequence, 0, 100).is_err());
    }
    assert_eq!(inspect(&store, &child, child_sequence, 4), "child text");
    assert!(store
        .entry_text(&receipt.thread, child_sequence, 0, 100)
        .is_err());
    assert!(store.entry_text(&receipt.thread, i64::MAX, 0, 100).is_err());
    assert_eq!(
        inspect(&store, &receipt.thread, call_sequence, 7),
        format!("bash · unknown\nOperation: {operation}\n{{\"command\":\"cargo check\"}}")
    );
    let page = store
        .transcript(Some(receipt.thread.clone()), None, 100, None)
        .unwrap();
    for sequence in [user_sequence, assistant_sequence, tool_sequence] {
        let entry = page
            .entries
            .iter()
            .find(|entry| entry.sequence == sequence)
            .unwrap();
        assert!(entry.is_preview);
        assert_eq!(entry.text.chars().count(), 2048);
    }
    let output = page
        .entries
        .iter()
        .find(|entry| entry.sequence == tool_sequence)
        .unwrap();
    assert_eq!(output.kind, EntryKind::ToolError);
    assert_eq!(output.tool.as_ref().unwrap().state, OperationState::Unknown);
    let cancelled_entry = page
        .entries
        .iter()
        .find(|entry| entry.operation.as_ref() == Some(&cancelled))
        .unwrap();
    assert_eq!(
        cancelled_entry.tool.as_ref().unwrap().state,
        OperationState::Cancelled
    );
}

#[test]
fn preview_flags_use_character_boundaries_and_full_text_remains_inspectable() {
    let (_dir, mut store, receipt, models) = fixture();
    for characters in [2047, 2048, 2049] {
        let text = "界".repeat(characters);
        store
            .command(
                &Command::Submit {
                    id: CommandId::new(),
                    thread: receipt.thread.clone(),
                    text: text.clone(),
                },
                &models,
            )
            .unwrap();
        let page = store
            .transcript(Some(receipt.thread.clone()), None, 1, None)
            .unwrap();
        let entry = &page.entries[0];
        assert_eq!(entry.text.chars().count(), characters.min(2048));
        assert_eq!(entry.is_preview, characters >= 2048);
        assert_eq!(inspect(&store, &receipt.thread, entry.sequence, 4096), text);
    }
}

#[test]
fn parent_ownership_and_actionable_approvals_follow_the_owning_run() {
    let (_dir, store, receipt, _models) = fixture();
    let old_parent_run = seed_run(&store, &receipt.thread, "completed");
    let child = seed_child(&store, &receipt.thread, &old_parent_run);
    let new_parent_run = seed_run(&store, &receipt.thread, "active");
    let child_run = seed_run(&store, &child, "active");
    let (obsolete, _) = seed_tool(
        &store,
        &receipt.thread,
        &old_parent_run,
        "write_file",
        "approval",
        &json!({"path": "src/main.rs"}),
    );
    seed_decision(&store, &obsolete);
    let (operation, _) = seed_tool(
        &store,
        &child,
        &child_run,
        "edit_file",
        "approval",
        &json!({"path": "src/main.rs"}),
    );
    seed_decision(&store, &operation);

    for state in ["active", "stopping", "interrupted", "completed", "failed"] {
        store
            .db
            .execute(
                "UPDATE runs SET state=?1 WHERE id=?2",
                params![state, child_run],
            )
            .unwrap();
        let page = store
            .transcript(Some(child.clone()), None, 100, None)
            .unwrap();
        let summary = page
            .threads
            .iter()
            .find(|thread| thread.id == child)
            .unwrap();
        assert_eq!(summary.parent_run.as_ref(), Some(&old_parent_run));
        assert_eq!(summary.parent_thread.as_ref(), Some(&receipt.thread));
        assert_eq!(summary.state.as_str(), state);
        assert_eq!(summary.needs_approval, state == "active");
        assert_eq!(page.decisions.len(), usize::from(state == "active"));
        if state == "active" {
            let decision = &page.decisions[0];
            assert_eq!(decision.operation, operation);
            assert_eq!(decision.thread, child);
            assert_eq!(decision.name, "edit_file");
            assert_eq!(decision.revision, 7);
            assert_eq!(decision.preview, "Write src/main.rs");
            assert_eq!(decision.presentation, None);
        }
        let parent = page
            .threads
            .iter()
            .find(|thread| thread.id == receipt.thread)
            .unwrap();
        assert_eq!(parent.run.as_ref(), Some(&new_parent_run));
        assert!(!parent.needs_approval);
        assert!(parent.parent_thread.is_none());
        let raw = store.view(Some(child.clone()), None, 100, None).unwrap();
        assert_eq!(raw.operations[0].id, operation);
        assert_eq!(raw.operations[0].state, OperationState::Approval);
        assert_eq!(raw.decisions.len(), page.decisions.len());
        let decision_state: String = store
            .db
            .query_row(
                "SELECT state FROM decisions WHERE operation=?1",
                [&operation],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(decision_state, "pending");
    }
    assert!(store
        .transcript(Some(receipt.thread.clone()), None, 100, None)
        .unwrap()
        .decisions
        .is_empty());
    store
        .db
        .execute("UPDATE runs SET state='active' WHERE id=?1", [&child_run])
        .unwrap();
    for (operation_state, decision_state) in [
        ("cancelled", "pending"),
        ("done", "pending"),
        ("approval", "approved"),
        ("approval", "cancelled"),
    ] {
        store
            .db
            .execute(
                "UPDATE operations SET state=?1 WHERE id=?2",
                params![operation_state, operation],
            )
            .unwrap();
        store
            .db
            .execute(
                "UPDATE decisions SET state=?1 WHERE operation=?2",
                params![decision_state, operation],
            )
            .unwrap();
        let page = store
            .transcript(Some(child.clone()), None, 100, None)
            .unwrap();
        assert!(page.decisions.is_empty());
        assert!(
            !page
                .threads
                .iter()
                .find(|thread| thread.id == child)
                .unwrap()
                .needs_approval
        );
        assert_eq!(
            page.entries[0].tool.as_ref().unwrap().state.as_str(),
            operation_state
        );
    }
}

#[test]
fn selected_thread_root_and_policy_survive_bounded_metadata_windows() {
    let (_dir, store, receipt, _models) = fixture();
    let run = seed_run(&store, &receipt.thread, "completed");
    let selected = seed_child(&store, &receipt.thread, &run);
    store.db.execute_batch("BEGIN").unwrap();
    for _ in 0..1100 {
        seed_child(&store, &receipt.thread, &run);
    }
    store.db.execute_batch("COMMIT").unwrap();
    let overview = store.transcript(None, None, 1, None).unwrap();
    assert!(overview.threads.iter().any(|t| t.id == receipt.thread));
    assert_eq!(overview.threads.len(), 1001);
    let page = store
        .transcript(Some(selected.clone()), None, 1, None)
        .unwrap();
    assert!(page.threads.iter().any(|t| t.id == selected));
    assert!(page.threads.iter().any(|t| t.id == receipt.thread));
    assert_eq!(page.threads.len(), 1002);

    store
        .db
        .execute(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1100)
         INSERT INTO sessions(id,workspace,policy) SELECT 'session-'||x,'/unused',?1 FROM n",
            [encode(&Policy::default()).unwrap()],
        )
        .unwrap();
    store.db.execute(
        "INSERT INTO threads(id,session,name,profile)
         SELECT 'root-'||s.id,s.id,'main',t.profile FROM sessions s JOIN threads t ON t.id=?1 WHERE s.id!=?2",
        params![receipt.thread, receipt.session],
    ).unwrap();
    let page = store
        .transcript(Some(selected.clone()), None, 1, None)
        .unwrap();
    assert!(page.threads.iter().any(|t| t.id == selected));
    assert!(page.threads.iter().any(|t| t.id == receipt.thread));
    assert_eq!(page.threads.len(), 1002);
    assert!(page.threads.iter().all(|t| t.session == receipt.session));
    assert_eq!(page.policies.len(), 1);
    assert_eq!(page.policies[0].session, receipt.session);
}

#[test]
fn historical_calls_expose_retained_results_outside_their_page() {
    let (dir, store, receipt, _) = fixture();
    let run = seed_run(&store, &receipt.thread, "completed");
    let (operation, sequence) = seed_tool(
        &store,
        &receipt.thread,
        &run,
        "bash",
        "failed",
        &json!({"command":"cargo check"}),
    );
    let body = "界 failure output\n".repeat(16_384);
    finish_tool_entry(&store.db, &operation, &body, true).unwrap();
    let result_sequence = store.db.last_insert_rowid();
    let presentation = ToolResultPresentation {
        summary: Some("Compilation failed: unresolved import".into()),
        body_preview: Some("src/main.rs:1: unresolved import".into()),
        exit_code: Some(101),
    };
    store
        .db
        .execute(
            "UPDATE operations SET presentation=?1 WHERE id=?2",
            params![encode(&presentation).unwrap(), operation],
        )
        .unwrap();
    for index in 0..101 {
        seed_tool(
            &store,
            &receipt.thread,
            &run,
            "read_file",
            "done",
            &json!({"path": format!("src/{index}.rs")}),
        );
    }
    drop(store);
    let store = Store::open(&dir.0.join("lab.db")).unwrap();
    let records = durable_records(&store);
    let raw = store
        .view(Some(receipt.thread.clone()), None, usize::MAX, None)
        .unwrap();
    assert_eq!(raw.operations.len(), 100);
    assert!(raw.operations.iter().all(|tool| tool.id != operation));
    let page = store
        .transcript(Some(receipt.thread.clone()), Some(sequence + 1), 1, None)
        .unwrap();
    assert_eq!(page.entries[0].kind, EntryKind::ToolCall);
    let tool = page.entries[0].tool.as_ref().unwrap();
    let artifact = tool.result.as_ref().unwrap();
    assert_eq!(tool.state, OperationState::Failed);
    assert_eq!(tool.presentation.as_ref(), Some(&presentation));
    assert_eq!(page.entries.len(), 1);
    assert!(page.operations.is_empty());
    let output_page = store
        .transcript(
            Some(receipt.thread.clone()),
            Some(result_sequence + 1),
            1,
            None,
        )
        .unwrap();
    assert_eq!(output_page.entries[0].kind, EntryKind::ToolError);
    assert_eq!(
        output_page.entries[0]
            .tool
            .as_ref()
            .unwrap()
            .presentation
            .as_ref(),
        Some(&presentation)
    );
    assert_eq!(
        store.artifact_page(artifact, 0, 16_384).unwrap(),
        body[..16_384]
    );
    assert_eq!(
        inspect(&store, &receipt.thread, result_sequence, 16_384),
        body
    );
    assert_eq!(durable_records(&store), records);
}

#[test]
fn approval_presentations_survive_reopen_without_exposing_adapter_private_metadata() {
    let (dir, store, receipt, models) = fixture();
    let run = seed_run(&store, &receipt.thread, "active");
    let mut expected = Vec::new();
    for (name, title, body) in [
        (
            "edit_file",
            "Edit",
            ToolBody::Edit {
                old_string: "let café = 1;".into(),
                new_string: "let café = 2;".into(),
            },
        ),
        (
            "write_file",
            "Create",
            ToolBody::Create {
                content: "fn main() {}\n".into(),
            },
        ),
        (
            "bash",
            "Run",
            ToolBody::Command {
                command: "cargo check --workspace".into(),
            },
        ),
        (
            "custom_tool",
            "Inspect",
            ToolBody::Text {
                text: "Review the selected target".into(),
            },
        ),
    ] {
        let input = json!({"path": "src/日本語/main.rs"});
        let (operation, sequence) =
            seed_tool(&store, &receipt.thread, &run, name, "approval", &input);
        seed_decision(&store, &operation);
        let presentation = ToolPresentation {
            title: title.into(),
            target: Some("src/日本語/main.rs".into()),
            body,
            details: vec!["Requires write permission".into()],
        };
        let prepared = PreparedTool {
            invocation: Invocation {
                operation_id: operation.clone(),
                run_id: run.clone(),
                thread_id: receipt.thread.clone(),
                workspace: dir.0.to_string_lossy().into_owned(),
                name: name.into(),
                input,
            },
            specification: json!({"presentation": {
                "title": "Adapter-private implementation detail",
                "target": null,
                "body": {"kind": "text", "text": "Not an approval preview"},
                "details": []
            }}),
            preview: format!("{title} src/日本語/main.rs"),
            presentation: Some(presentation.clone()),
        };
        store
            .db
            .execute(
                "UPDATE operations SET prepared=?1 WHERE id=?2",
                params![encode(&prepared).unwrap(), operation],
            )
            .unwrap();
        store
            .db
            .execute(
                "UPDATE decisions SET preview=?1 WHERE operation=?2",
                params![prepared.preview, operation],
            )
            .unwrap();
        expected.push((operation, sequence, name, prepared, presentation));
    }
    let before = store
        .transcript(Some(receipt.thread.clone()), None, 1, None)
        .unwrap();
    assert_eq!(before.entries.len(), 1);
    assert_eq!(before.decisions.len(), expected.len());
    drop(store);
    let mut store = Store::open(&dir.0.join("lab.db")).unwrap();
    let claim =
        crate::SessionClaim::acquire(store.path.as_deref().unwrap(), receipt.session.clone())
            .unwrap();
    store.adopt_claim(claim, true).unwrap();
    store
        .command(
            &Command::Resume {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
            },
            &models,
        )
        .unwrap();
    let records = durable_records(&store);
    let page = store
        .transcript(
            Some(receipt.thread.clone()),
            Some(expected[0].1 + 1),
            1,
            None,
        )
        .unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].sequence, expected[0].1);
    assert_eq!(json!(page.decisions), json!(before.decisions));
    for (decision, (operation, sequence, name, prepared, presentation)) in
        page.decisions.iter().zip(&expected)
    {
        assert_eq!(&decision.operation, operation);
        assert_eq!(decision.thread, receipt.thread);
        assert_eq!(decision.name, *name);
        assert_eq!(decision.revision, 7);
        assert_eq!(decision.preview, prepared.preview);
        assert_eq!(decision.presentation.as_ref(), Some(presentation));
        assert_eq!(
            inspect(&store, &receipt.thread, *sequence, 17),
            format!(
                "{name} · approval\nOperation: {operation}\n{}",
                encode(prepared).unwrap()
            )
        );
    }
    assert_eq!(durable_records(&store), records);
    let (operation, _, _, mut prepared, _) = expected.remove(0);
    prepared.presentation = None;
    store
        .db
        .execute(
            "UPDATE operations SET prepared=?1 WHERE id=?2",
            params![encode(&prepared).unwrap(), operation],
        )
        .unwrap();
    let page = store
        .transcript(Some(receipt.thread.clone()), None, 1, None)
        .unwrap();
    assert_eq!(page.decisions.len(), 4);
    assert_eq!(page.decisions[0].operation, operation);
    assert_eq!(page.decisions[0].presentation, None);
}

#[test]
fn approvals_require_pending_decisions_at_the_current_policy_revision() {
    let (_dir, store, receipt, _) = fixture();
    let run = seed_run(&store, &receipt.thread, "active");
    let (operation, _) = seed_tool(
        &store,
        &receipt.thread,
        &run,
        "edit_file",
        "approval",
        &json!({"path": "src/main.rs"}),
    );
    seed_decision(&store, &operation);
    for (decision_revision, prepared_revision, decision_state, visible) in [
        (7, 7, "pending", true),
        (6, 6, "pending", false),
        (6, 7, "pending", false),
        (7, 6, "pending", false),
        (8, 8, "pending", false),
        (7, 7, "approved", false),
        (7, 7, "denied", false),
        (7, 7, "cancelled", false),
        (7, 7, "pending", true),
    ] {
        store
            .db
            .execute(
                "UPDATE decisions SET revision=?1,state=?2 WHERE operation=?3",
                params![decision_revision, decision_state, operation],
            )
            .unwrap();
        store
            .db
            .execute(
                "UPDATE operations SET policy_revision=?1 WHERE id=?2",
                params![prepared_revision, operation],
            )
            .unwrap();
        let records = durable_records(&store);
        for page in [
            store
                .transcript(Some(receipt.thread.clone()), None, 1, None)
                .unwrap(),
            store
                .view(Some(receipt.thread.clone()), None, 1, None)
                .unwrap(),
        ] {
            assert_eq!(
                page.decisions.len(),
                usize::from(visible),
                "decision={decision_revision}/{decision_state}, prepared={prepared_revision}"
            );
            let summary = page
                .threads
                .iter()
                .find(|thread| thread.id == receipt.thread)
                .unwrap();
            assert_eq!(summary.needs_approval, visible);
            assert_eq!(
                page.entries[0].tool.as_ref().unwrap().state,
                OperationState::Approval
            );
            if visible {
                assert_eq!(page.decisions[0].operation, operation);
                assert_eq!(page.decisions[0].revision, 7);
            }
        }
        assert_eq!(durable_records(&store), records);
    }
}

#[test]
fn approval_metadata_is_bounded_independently_of_the_transcript_page() {
    let (_dir, store, receipt, _) = fixture();
    let run = seed_run(&store, &receipt.thread, "active");
    let mut operations = Vec::new();
    for index in 0..103 {
        let (operation, _) = seed_tool(
            &store,
            &receipt.thread,
            &run,
            "edit_file",
            "approval",
            &json!({"path": format!("src/{index}.rs")}),
        );
        seed_decision(&store, &operation);
        operations.push(operation);
    }
    let records = durable_records(&store);
    for limit in [0, 1, usize::MAX] {
        let page = store
            .transcript(Some(receipt.thread.clone()), None, limit, None)
            .unwrap();
        assert_eq!(page.entries.len(), limit.clamp(1, 100));
        assert_eq!(page.decisions.len(), 100);
        assert_eq!(
            page.decisions
                .iter()
                .map(|decision| &decision.operation)
                .collect::<Vec<_>>(),
            operations[..100].iter().collect::<Vec<_>>()
        );
        assert!(page.operations.is_empty());
    }
    assert_eq!(durable_records(&store), records);
}

#[test]
fn activity_comes_from_the_current_runs_next_live_operation_not_the_page() {
    let (_dir, store, receipt, _) = fixture();
    for (held, state) in [(0, ThreadState::Idle), (1, ThreadState::Held)] {
        store
            .db
            .execute(
                "UPDATE threads SET held=?1 WHERE id=?2",
                params![held, receipt.thread],
            )
            .unwrap();
        let page = store
            .transcript(Some(receipt.thread.clone()), None, 1, None)
            .unwrap();
        let summary = page
            .threads
            .iter()
            .find(|thread| thread.id == receipt.thread)
            .unwrap();
        assert_eq!(summary.state, state);
        assert_eq!(summary.activity, None);
        assert_eq!(summary.run, None);
        assert!(!summary.needs_approval);
    }
    store
        .db
        .execute("UPDATE threads SET held=0 WHERE id=?1", [&receipt.thread])
        .unwrap();
    let old_run = seed_run(&store, &receipt.thread, "completed");
    let (obsolete, old_sequence) = seed_tool(
        &store,
        &receipt.thread,
        &old_run,
        "write_file",
        "approval",
        &json!({"path": "old.rs"}),
    );
    seed_decision(&store, &obsolete);
    let run = seed_run(&store, &receipt.thread, "active");
    let (operation, _) = seed_tool(
        &store,
        &receipt.thread,
        &run,
        "completion",
        "pending",
        &json!({}),
    );
    seed_decision(&store, &operation);
    let (later, _) = seed_tool(
        &store,
        &receipt.thread,
        &run,
        "read_file",
        "done",
        &json!({"path": "new.rs"}),
    );
    for (kind, name, state, activity) in [
        (
            "model",
            "completion",
            "pending",
            Some(ThreadActivity::Model),
        ),
        (
            "model",
            "completion",
            "started",
            Some(ThreadActivity::Model),
        ),
        (
            "model",
            "completion",
            "waiting",
            Some(ThreadActivity::Agent),
        ),
        (
            "tool",
            "edit_file",
            "pending",
            Some(ThreadActivity::Tool {
                name: "edit_file".into(),
            }),
        ),
        (
            "tool",
            "edit_file",
            "preparing",
            Some(ThreadActivity::Tool {
                name: "edit_file".into(),
            }),
        ),
        (
            "tool",
            "edit_file",
            "ready",
            Some(ThreadActivity::Tool {
                name: "edit_file".into(),
            }),
        ),
        (
            "tool",
            "edit_file",
            "started",
            Some(ThreadActivity::Tool {
                name: "edit_file".into(),
            }),
        ),
        (
            "tool",
            "edit_file",
            "approval",
            Some(ThreadActivity::Approval),
        ),
        ("tool", "wait_agent", "waiting", Some(ThreadActivity::Agent)),
        (
            "tool",
            "wait_message",
            "waiting",
            Some(ThreadActivity::Agent),
        ),
        (
            "tool",
            "wait_agent",
            "ready",
            Some(ThreadActivity::Tool {
                name: "wait_agent".into(),
            }),
        ),
        ("tool", "read_file", "done", None),
        ("tool", "read_file", "failed", None),
        ("tool", "read_file", "cancelled", None),
        ("tool", "read_file", "unknown", None),
        ("tool", "read_file", "resolved", None),
        ("model", "completion", "interrupted", None),
    ] {
        store
            .db
            .execute(
                "UPDATE operations SET kind=?1,name=?2,state=?3 WHERE id=?4",
                params![kind, name, state, operation],
            )
            .unwrap();
        let records = durable_records(&store);
        for page in [
            store
                .transcript(
                    Some(receipt.thread.clone()),
                    Some(old_sequence + 1),
                    1,
                    None,
                )
                .unwrap(),
            store.transcript(None, None, 1, None).unwrap(),
            store
                .view(Some(receipt.thread.clone()), None, 1, None)
                .unwrap(),
        ] {
            let summary = page
                .threads
                .iter()
                .find(|thread| thread.id == receipt.thread)
                .unwrap();
            assert_eq!(summary.state, ThreadState::Active);
            assert_eq!(summary.run.as_ref(), Some(&run));
            assert_eq!(summary.activity, activity, "{kind}/{name}/{state}");
            assert_eq!(summary.needs_approval, state == "approval");
        }
        assert_eq!(durable_records(&store), records);
    }
    store
        .db
        .execute(
            "UPDATE operations SET kind='model',name='completion',state='started' WHERE id=?1",
            [&operation],
        )
        .unwrap();
    store
        .db
        .execute("UPDATE operations SET state='ready' WHERE id=?1", [&later])
        .unwrap();
    for (state, expected) in [
        ("started", ThreadActivity::Model),
        (
            "done",
            ThreadActivity::Tool {
                name: "read_file".into(),
            },
        ),
    ] {
        store
            .db
            .execute(
                "UPDATE operations SET state=?1 WHERE id=?2",
                params![state, operation],
            )
            .unwrap();
        let page = store
            .transcript(
                Some(receipt.thread.clone()),
                Some(old_sequence + 1),
                1,
                None,
            )
            .unwrap();
        assert_eq!(page.entries[0].sequence, old_sequence);
        let summary = page
            .threads
            .iter()
            .find(|thread| thread.id == receipt.thread)
            .unwrap();
        assert_eq!(summary.activity, Some(expected));
    }
    for state in [
        ThreadState::Stopping,
        ThreadState::Interrupted,
        ThreadState::Completed,
        ThreadState::Failed,
        ThreadState::Cancelled,
    ] {
        store
            .db
            .execute(
                "UPDATE runs SET state=?1 WHERE id=?2",
                params![state.as_str(), run],
            )
            .unwrap();
        let page = store
            .transcript(
                Some(receipt.thread.clone()),
                Some(old_sequence + 1),
                1,
                None,
            )
            .unwrap();
        let summary = page
            .threads
            .iter()
            .find(|thread| thread.id == receipt.thread)
            .unwrap();
        assert_eq!(summary.state, state);
        assert_eq!(summary.activity, None);
        assert!(!summary.needs_approval);
        assert!(page.decisions.is_empty());
    }
}

#[test]
fn queries_preserve_durable_records_and_the_existing_error_view() {
    let (dir, mut store, receipt, models) = fixture();
    store
        .command(
            &Command::Submit {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                text: "Inspect the interrupted write".into(),
            },
            &models,
        )
        .unwrap();
    let run = seed_run(&store, &receipt.thread, "active");
    let (operation, sequence) = seed_tool(
        &store,
        &receipt.thread,
        &run,
        "write_file",
        "approval",
        &json!({"path": "src/main.rs", "content": "fn main() {}"}),
    );
    seed_decision(&store, &operation);
    let approval = ToolPresentation {
        title: "Create".into(),
        target: Some("src/main.rs".into()),
        body: ToolBody::Create {
            content: "fn main() {}".into(),
        },
        details: Vec::new(),
    };
    let prepared = PreparedTool {
        invocation: Invocation {
            operation_id: operation.clone(),
            run_id: run.clone(),
            thread_id: receipt.thread.clone(),
            workspace: dir.0.to_string_lossy().into_owned(),
            name: "write_file".into(),
            input: json!({"path": "src/main.rs", "content": "fn main() {}"}),
        },
        specification: json!({}),
        preview: "Create src/main.rs".into(),
        presentation: Some(approval.clone()),
    };
    store
        .db
        .execute(
            "UPDATE operations SET prepared=?1 WHERE id=?2",
            params![encode(&prepared).unwrap(), operation],
        )
        .unwrap();
    let (unknown, _) = seed_tool(
        &store,
        &receipt.thread,
        &run,
        "bash",
        "unknown",
        &json!({"command": "cargo check"}),
    );
    finish_tool_entry(
        &store.db,
        &unknown,
        "Outcome unknown; inspect before retrying",
        true,
    )
    .unwrap();
    let records = durable_records(&store);
    let changes: i64 = store
        .db
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    let error = "The previous request failed; retained work is available";
    let raw = store
        .view(Some(receipt.thread.clone()), None, 100, Some(error.into()))
        .unwrap();
    let page = store
        .transcript(Some(receipt.thread.clone()), None, 100, Some(error.into()))
        .unwrap();
    assert_eq!(page.error.as_deref(), Some(error));
    assert_eq!(raw.error, page.error);
    assert_eq!(raw.sequence, page.sequence);
    assert_eq!(json!(raw.entries), json!(page.entries));
    assert_eq!(json!(raw.threads), json!(page.threads));
    assert_eq!(json!(raw.decisions), json!(page.decisions));
    assert_eq!(page.decisions.len(), 1);
    assert_eq!(page.decisions[0].presentation.as_ref(), Some(&approval));
    assert_eq!(
        page.threads
            .iter()
            .find(|thread| thread.id == receipt.thread)
            .unwrap()
            .activity,
        Some(ThreadActivity::Approval)
    );
    assert_eq!(json!(raw.policies), json!(page.policies));
    assert!(!raw.operations.is_empty());
    assert!(page.operations.is_empty());
    for entry in &page.entries {
        inspect(&store, &receipt.thread, entry.sequence, 17);
    }
    assert!(store
        .entry_text(&receipt.thread, sequence, usize::MAX, 4)
        .is_err());
    let overview = store
        .transcript(None, None, 100, Some(error.into()))
        .unwrap();
    assert_eq!(overview.error.as_deref(), Some(error));
    assert_eq!(overview.sequence, page.sequence);
    assert_eq!(json!(overview.threads), json!(page.threads));
    assert!(overview.entries.is_empty());
    assert!(overview.operations.is_empty());
    let after: i64 = store
        .db
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, changes);
    assert_eq!(durable_records(&store), records);
}
