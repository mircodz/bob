use super::*;
use crate::store::{artifact, full_artifact, preview};

fn database() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    initialize(&db);
    db
}

fn initialize(db: &Connection) {
    db.execute_batch("PRAGMA foreign_keys=ON;
        CREATE TABLE artifacts(id TEXT PRIMARY KEY,length INTEGER NOT NULL);
        CREATE TABLE chunks(artifact TEXT NOT NULL REFERENCES artifacts(id),ordinal INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(artifact,ordinal)) WITHOUT ROWID;").unwrap();
    db.execute_batch(SCHEMA_SQL).unwrap();
}

fn assistant(text: impl Into<String>) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

fn source(db: &Connection, message: &Message) -> ArtifactId {
    artifact(db, &serde_json::to_string(message).unwrap()).unwrap()
}

fn retained(db: &mut Connection, message: &Message) -> (ArtifactId, ArtifactId) {
    let tx = db.transaction().unwrap();
    let source = source(&tx, message);
    let visible = retain(&tx, &source, message).unwrap();
    tx.commit().unwrap();
    (source, visible)
}

fn read_all(mut read: impl FnMut(usize, usize) -> anyhow::Result<TextPage>, size: usize) -> String {
    let mut result = String::new();
    let mut offset = 0;
    loop {
        let page = read(offset, size).unwrap();
        assert!(page.text.len() <= size.clamp(4, CHUNK));
        result.push_str(&page.text);
        match page.next_offset {
            Some(next) => {
                assert_eq!(next, offset + page.text.len());
                assert!(next > offset);
                assert!(next < page.total_bytes);
                offset = next;
            }
            None => {
                assert_eq!(result.len(), page.total_bytes);
                break;
            }
        }
    }
    result
}

#[test]
fn short_exact_preview_and_long_messages_have_exact_completeness() {
    let mut db = database();
    for count in [0, 1, 2047, 2048, 2049, 100_000] {
        for scalar in ["x", "界"] {
            let text = scalar.repeat(count);
            let message = assistant(&text);
            let (source, visible) = retained(&mut db, &message);
            assert_eq!(
                is_preview(&db, &source, &preview(&text)).unwrap(),
                Some(count > 2048)
            );
            assert_eq!(full_artifact(&db, &visible).unwrap(), text);
            assert_eq!(
                full_artifact(&db, &source).unwrap(),
                serde_json::to_string(&message).unwrap()
            );
            assert_eq!(
                read_all(
                    |offset, limit| Ok(page(&db, &source, offset, limit)?.unwrap()),
                    16 * 1024
                ),
                text
            );
        }
    }
}

#[test]
fn text_blocks_concatenate_without_reasoning_or_tool_payloads() {
    let mut db = database();
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::Thinking {
                thinking: "private thinking".into(),
                signature: "private signature".into(),
            },
            ContentBlock::Text {
                text: "first".into(),
            },
            ContentBlock::ToolUse {
                id: "call".into(),
                name: "secret tool".into(),
                input: serde_json::json!({"private": "payload"}),
            },
            ContentBlock::RedactedThinking {
                data: "private encrypted".into(),
            },
            ContentBlock::Text {
                text: "界last".into(),
            },
            ContentBlock::ReasoningItem {
                item: serde_json::json!({"private": "reasoning"}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "private tool result".to_owned().into(),
                is_error: None,
            },
        ],
    };
    let (new, _) = retained(&mut db, &message);
    let old = source(&db, &message);
    let mut cache = Cache::default();
    for id in [&new, &old] {
        assert_eq!(
            read_all(|offset, limit| cache.page(&db, id, offset, limit), 4),
            "first界last"
        );
    }
    assert_eq!(cache.stats().extractions, 1);
    assert!(full_artifact(&db, &new)
        .unwrap()
        .contains("private thinking"));
    assert_eq!(message.text(), "first界last");
}

#[test]
fn multibyte_text_crosses_storage_and_page_boundaries_without_loss() {
    let mut db = database();
    let text = format!(
        "{}🦀{}界{}e\u{301}tail",
        "a".repeat(16 * 1024 - 1),
        "b".repeat(3 * 16 * 1024 - 4),
        "🦀".repeat(20_000)
    );
    let message = assistant(&text);
    let (new, _) = retained(&mut db, &message);
    let old = source(&db, &message);
    let mut cache = Cache::default();
    for id in [&new, &old] {
        for limit in [4, 16 * 1024, CHUNK] {
            assert_eq!(
                read_all(|offset, size| cache.page(&db, id, offset, size), limit),
                text
            );
        }
    }
    assert_eq!(cache.stats().extractions, 1);
}

#[test]
fn boundary_errors_are_typed_and_invalid_offsets_do_not_retry() {
    let mut db = database();
    let message = assistant("x🦀界y");
    let (new, _) = retained(&mut db, &message);
    let old = source(&db, &message);
    let mut cache = Cache::default();
    for id in [&new, &old] {
        for offset in [2, 3, 4, 6, 7] {
            let error = cache.page(&db, id, offset, 16 * 1024).unwrap_err();
            assert!(error.is::<BoundaryError>(), "{error:#}");
            assert_eq!(
                error.to_string(),
                "entry offset must be a retained UTF-8 boundary"
            );
        }
        for offset in [10, usize::MAX] {
            assert!(!cache
                .page(&db, id, offset, 1024)
                .unwrap_err()
                .is::<BoundaryError>());
        }
        let end = cache.page(&db, id, 9, 1024).unwrap();
        assert!(end.text.is_empty());
        assert_eq!(end.next_offset, None);
        assert_eq!(end.total_bytes, 9);
    }
}

#[test]
fn bounded_tail_backsteps_preserve_the_straddling_character() {
    let mut db = database();
    let text = format!("{}🦀tail", "x".repeat(16 * 1024 - 1));
    let (id, _) = retained(&mut db, &assistant(&text));
    let first = page(&db, &id, 0, 16 * 1024).unwrap().unwrap();
    let mut offset = 16 * 1024;
    let tail = loop {
        match page(&db, &id, offset, 16 * 1024) {
            Ok(Some(page)) => break page,
            Err(error) if error.is::<BoundaryError>() && offset > 16 * 1024 - 3 => offset -= 1,
            result => panic!("unexpected tail result: {result:?}"),
        }
    };
    assert_eq!(first.next_offset, Some(offset));
    assert_eq!(format!("{}{}", first.text, tail.text), text);
}

#[test]
fn projection_reads_only_requested_chunks() {
    let mut db = database();
    let (source, visible) = retained(&mut db, &assistant("a".repeat(4 * CHUNK)));
    db.execute(
        "DELETE FROM chunks WHERE artifact=?1 AND ordinal=2",
        [&visible],
    )
    .unwrap();
    assert_eq!(
        page(&db, &source, 0, 16 * 1024)
            .unwrap()
            .unwrap()
            .text
            .len(),
        16 * 1024
    );
    assert!(page(&db, &source, 2 * CHUNK, 16 * 1024)
        .unwrap_err()
        .to_string()
        .contains("missing"));
    assert_eq!(
        page(&db, &source, 3 * CHUNK, 16 * 1024)
            .unwrap()
            .unwrap()
            .text
            .len(),
        16 * 1024
    );
}

#[test]
fn missing_short_oversized_and_wrong_type_projection_chunks_fail() {
    for mutation in [
        "DELETE FROM chunks WHERE artifact=?1",
        "UPDATE chunks SET data=x'61' WHERE artifact=?1",
        "UPDATE chunks SET data=zeroblob(65537) WHERE artifact=?1",
        "UPDATE chunks SET data='abcd' WHERE artifact=?1",
        "UPDATE chunks SET ordinal=1 WHERE artifact=?1",
    ] {
        let mut db = database();
        let (source, visible) = retained(&mut db, &assistant("abcd"));
        db.execute(mutation, [&visible]).unwrap();
        let error = page(&db, &source, 0, 4).unwrap_err();
        assert!(!error.is::<BoundaryError>(), "{mutation}: {error:#}");
        assert!(error.to_string().contains("chunk"), "{mutation}: {error:#}");
    }
}

#[test]
fn corrupt_utf8_is_not_a_seek_boundary_or_a_silent_truncation() {
    for bytes in [
        vec![0x80, b'a', b'b', b'c'],
        vec![b'a', b'b', b'c', 0xe2],
        vec![b'a', 0xff, b'b', b'c'],
    ] {
        let mut db = database();
        let (source, visible) = retained(&mut db, &assistant("abcd"));
        db.execute(
            "UPDATE chunks SET data=?2 WHERE artifact=?1",
            params![visible, bytes],
        )
        .unwrap();
        let error = page(&db, &source, 0, 4).unwrap_err();
        assert!(!error.is::<BoundaryError>());
        assert!(error.to_string().contains("UTF-8"));
    }
}

#[test]
fn mismatched_missing_and_oversized_artifact_metadata_fail() {
    for mutation in [
        "UPDATE artifacts SET length=3 WHERE id=?1",
        "UPDATE artifacts SET length=-1 WHERE id=?1",
        "UPDATE artifacts SET length=4194305 WHERE id=?1",
        "DELETE FROM artifacts WHERE id=?1",
    ] {
        let mut db = database();
        let (source, visible) = retained(&mut db, &assistant("abcd"));
        db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        db.execute(mutation, [&visible]).unwrap();
        assert!(page(&db, &source, 0, 4).is_err(), "{mutation}");
    }
}

#[test]
fn immutable_versioned_mapping_and_settlement_rollback() {
    let mut db = database();
    db.execute_batch(SCHEMA_SQL).unwrap();
    {
        let tx = db.transaction().unwrap();
        let message = assistant("rolled back");
        let id = source(&tx, &message);
        retain(&tx, &id, &message).unwrap();
    }
    let count: i64 = db
        .query_row("SELECT count(*) FROM artifacts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
    let message = assistant("kept");
    let (id, _) = retained(&mut db, &message);
    assert!(retain(&db, &id, &message)
        .unwrap_err()
        .to_string()
        .contains("transaction"));
    assert!(db
        .execute(
            "UPDATE assistant_visible_text SET version=2 WHERE source=?1",
            [&id]
        )
        .is_err());
    assert!(db
        .execute("DELETE FROM assistant_visible_text WHERE source=?1", [&id])
        .is_err());
    let tx = db.transaction().unwrap();
    assert!(retain(&tx, &id, &message)
        .unwrap_err()
        .to_string()
        .contains("already retained"));
    tx.rollback().unwrap();
    assert_eq!(page(&db, &id, 0, 16).unwrap().unwrap().text, "kept");
}

#[test]
fn legacy_is_extracted_once_then_pages_reuse_only_cached_visible_text() {
    let db = database();
    let text = "legacy界".repeat(20_000);
    let id = source(&db, &assistant(&text));
    let mut cache = Cache::default();
    assert!(page(&db, &id, 0, 16 * 1024).unwrap().is_none());
    assert!(is_preview(&db, &id, "legacy").unwrap().is_none());
    cache.page(&db, &id, 0, 16 * 1024).unwrap();
    db.execute("DELETE FROM chunks WHERE artifact=?1", [&id])
        .unwrap();
    for _ in 0..3 {
        assert_eq!(
            read_all(
                |offset, limit| cache.page(&db, &id, offset, limit),
                16 * 1024
            ),
            text
        );
    }
    assert_eq!(cache.stats().extractions, 1);
    assert!(Cache::default().page(&db, &id, 0, 16 * 1024).is_err());
}

#[test]
fn legacy_sources_are_validated_before_decoding_or_large_allocation() {
    for mutation in [
        "UPDATE artifacts SET length=4194305 WHERE id=?1",
        "UPDATE artifacts SET length=-1 WHERE id=?1",
        "DELETE FROM chunks WHERE artifact=?1",
        "UPDATE chunks SET ordinal=2 WHERE artifact=?1",
        "UPDATE chunks SET data=zeroblob(4194305) WHERE artifact=?1",
        "UPDATE chunks SET data=x'61' WHERE artifact=?1",
    ] {
        let db = database();
        let id = source(&db, &assistant("legacy"));
        db.execute(mutation, [&id]).unwrap();
        let mut cache = Cache::default();
        let empty_bytes = cache.stats().retained_bytes;
        assert!(cache.page(&db, &id, 0, 4).is_err(), "{mutation}");
        assert_eq!(cache.stats().extractions, 0);
        assert_eq!(cache.stats().retained_bytes, empty_bytes);
    }
    let db = database();
    let missing = ArtifactId::new();
    assert!(Cache::default()
        .page(&db, &missing, 0, 4)
        .unwrap_err()
        .to_string()
        .contains("missing"));
    for invalid in [
        "not json",
        r#"{"role":"assistant","content":[{"type":"text"}]}"#,
        r#"{"role":"assistant","content":[{"type":"unknown"}]}"#,
    ] {
        let id = artifact(&db, invalid).unwrap();
        assert!(Cache::default()
            .page(&db, &id, 0, 4)
            .unwrap_err()
            .to_string()
            .contains("corrupt"));
    }
    let user = source(&db, &Message::user_text("not assistant"));
    assert!(Cache::default().page(&db, &user, 0, 4).is_err());
}

#[test]
fn legacy_decode_rejects_an_open_transaction() {
    let mut db = database();
    let id = source(&db, &assistant("legacy"));
    let tx = db.transaction().unwrap();
    let error = Cache::default().page(&tx, &id, 0, 4).unwrap_err();
    assert!(error.to_string().contains("cannot hold a transaction"));
}

#[test]
fn cache_bytes_entries_eviction_and_transient_highwater_are_bounded() {
    let db = database();
    let ids: Vec<_> = (0..5)
        .map(|_| source(&db, &assistant("a".repeat(500))))
        .collect();
    let mut cache = Cache::with_limits(1200, 2);
    for id in &ids {
        cache.page(&db, id, 0, 100).unwrap();
        assert!(cache.entries.len() <= 2);
        assert_eq!(
            cache.stats().retained_bytes,
            cache.entries.capacity() * std::mem::size_of::<CachedText>()
                + cache.entries.iter().map(CachedText::bytes).sum::<usize>()
        );
        assert!(cache.stats().retained_bytes <= 1200);
        assert!(cache.stats().high_water_bytes <= 1200 + EXTRACTION_RESERVATION);
    }
    assert_eq!(cache.stats().extractions, 5);
    cache.page(&db, &ids[4], 100, 100).unwrap();
    assert_eq!(cache.stats().extractions, 5);
    cache.page(&db, &ids[0], 100, 100).unwrap();
    assert_eq!(cache.stats().extractions, 6);
    let mut too_small = Cache::with_limits(100, 2);
    assert!(too_small
        .page(&db, &ids[0], 0, 4)
        .unwrap_err()
        .to_string()
        .contains("cache byte budget"));
    assert_eq!(
        too_small.stats().retained_bytes,
        too_small.entries.capacity() * std::mem::size_of::<CachedText>()
    );
    let mut disabled = Cache::with_limits(0, 0);
    assert!(disabled.page(&db, &ids[0], 0, 4).is_err());
    assert_eq!(disabled.stats().high_water_bytes, 0);
}

#[test]
fn empty_legacy_entries_are_entry_bounded_and_many_text_blocks_are_exact() {
    let db = database();
    let mut cache = Cache::default();
    for _ in 0..CACHE_ENTRIES * 2 {
        let id = source(&db, &assistant(""));
        assert!(cache.page(&db, &id, 0, 4).unwrap().text.is_empty());
        assert!(cache.entries.len() <= CACHE_ENTRIES);
        assert!(cache.stats().retained_bytes <= CACHE_BYTES);
    }
    let message = Message {
        role: Role::Assistant,
        content: (0..12_000)
            .map(|_| ContentBlock::Text { text: "界".into() })
            .collect(),
    };
    let id = source(&db, &message);
    assert_eq!(
        read_all(
            |offset, limit| cache.page(&db, &id, offset, limit),
            16 * 1024
        ),
        message.text()
    );
}

#[test]
fn legacy_works_without_projection_schema_or_writes() {
    let db = database();
    let id = source(&db, &assistant("old read-only store"));
    db.execute_batch("DROP TABLE assistant_visible_text; PRAGMA query_only=ON")
        .unwrap();
    let before: i64 = db
        .query_row("SELECT total_changes()", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        Cache::default().page(&db, &id, 0, 1024).unwrap().text,
        "old read-only store"
    );
    assert_eq!(
        db.query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        before
    );
}

#[test]
fn exact_source_retention_limit_remains_pageable() {
    let mut db = database();
    let overhead = serde_json::to_string(&assistant("")).unwrap().len();
    let text = "x".repeat(MAX_ARTIFACT - overhead);
    let message = assistant(&text);
    let (new, _) = retained(&mut db, &message);
    let old = source(&db, &message);
    assert_eq!(artifact_length(&db, &old).unwrap(), MAX_ARTIFACT);
    let mut cache = Cache::default();
    for id in [&new, &old] {
        let tail = cache.page(&db, id, text.len() - 1024, 1024).unwrap();
        assert_eq!(tail.text, "x".repeat(1024));
        assert_eq!(tail.total_bytes, text.len());
        assert_eq!(tail.next_offset, None);
    }
    assert!(cache.stats().retained_bytes <= CACHE_BYTES);
    assert!(cache.stats().high_water_bytes <= CACHE_BYTES + EXTRACTION_RESERVATION);
    assert_eq!(cache.stats().extractions, 1);
}

#[test]
fn corrupt_projection_version_never_falls_back_to_legacy_decode() {
    let mut db = database();
    let (source, _) = retained(&mut db, &assistant("visible"));
    db.execute_batch(
        "DROP TRIGGER assistant_visible_text_immutable; PRAGMA ignore_check_constraints=ON",
    )
    .unwrap();
    db.execute(
        "UPDATE assistant_visible_text SET version=2 WHERE source=?1",
        [&source],
    )
    .unwrap();
    let mut cache = Cache::default();
    let error = cache.page(&db, &source, 0, 1024).unwrap_err();
    assert!(error.to_string().contains("unsupported"));
    assert_eq!(cache.stats().extractions, 0);
}

#[test]
fn read_only_connection_serves_new_and_legacy_without_backfill() {
    let directory = std::env::temp_dir().join(format!("bob-visible-text-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0.join("fixture.sqlite"));
            let _ = std::fs::remove_dir(&self.0);
        }
    }
    let _cleanup = Cleanup(directory.clone());
    let path = directory.join("fixture.sqlite");
    let mut writer = Connection::open(&path).unwrap();
    initialize(&writer);
    let (new, _) = retained(&mut writer, &assistant("new read-only"));
    let old = source(&writer, &assistant("old read-only"));
    drop(writer);
    let before = std::fs::read(&path).unwrap();
    let reader =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let mut cache = Cache::default();
    assert_eq!(
        cache.page(&reader, &new, 0, 1024).unwrap().text,
        "new read-only"
    );
    assert_eq!(
        cache.page(&reader, &old, 0, 1024).unwrap().text,
        "old read-only"
    );
    assert_eq!(
        reader
            .query_row("SELECT total_changes()", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        0
    );
    let mappings: i64 = reader
        .query_row("SELECT count(*) FROM assistant_visible_text", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(mappings, 1);
    drop(reader);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
