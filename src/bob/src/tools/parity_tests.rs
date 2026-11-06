use super::tests::{executor, run, TestDir};
use super::*;

async fn invoke(
    tools: &ToolService,
    root: &TestDir,
    thread: &ThreadId,
    name: &str,
    input: Value,
) -> ToolOutcome {
    run(
        tools,
        tools
            .prepare(root.invocation(thread, name, input))
            .await
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn paging_tracks_only_displayed_unicode_bytes_and_allows_surgical_edits() {
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    let content = format!("{}終\r\nsecond\n", "é".repeat(2000));
    fs::write(root.file("file"), &content).unwrap();
    let first = invoke(
        &tools,
        &root,
        &thread,
        "read_file",
        json!({"path":"file","limit":1}),
    )
    .await;
    assert!(first.text.contains("column_offset=2001"));
    assert!(!first.text.contains('終'));
    let overwrite = || {
        root.invocation(
            &thread,
            "write_file",
            json!({"path":"file","content":"new"}),
        )
    };
    assert!(tools
        .prepare(overwrite())
        .await
        .unwrap_err()
        .to_string()
        .contains("fully shown"));
    assert!(tools
        .prepare(root.invocation(
            &thread,
            "edit_file",
            json!({"path":"file","old_string":"second","new_string":"changed"})
        ))
        .await
        .is_ok());
    invoke(
        &tools,
        &root,
        &thread,
        "read_file",
        json!({"path":"file","offset":2}),
    )
    .await;
    assert!(tools.prepare(overwrite()).await.is_err());
    invoke(
        &tools,
        &root,
        &thread,
        "read_file",
        json!({"path":"file","offset":1,"column_offset":2001,"limit":1}),
    )
    .await;
    let prepared = tools.prepare(overwrite()).await.unwrap();
    assert!(!run(&tools, prepared).await.is_error);
    assert_eq!(fs::read_to_string(root.file("file")).unwrap(), "new");
}

#[tokio::test]
async fn normal_sixteen_mib_files_are_readable_without_granting_full_evidence() {
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(root.file("large"), "a".repeat(READ_LIMIT)).unwrap();
    let result = invoke(&tools, &root, &thread, "read_file", json!({"path":"large"})).await;
    assert!(!result.is_error);
    assert!(result.text.len() < 4096);
    let state = tools.files.lock().await;
    let evidence = state.reads.values().next().unwrap();
    assert_eq!(evidence.text.len(), READ_LIMIT);
    assert_eq!(
        evidence.visible,
        std::iter::once(0..2000).collect::<Vec<_>>()
    );
    assert!(!evidence.fully_read());
}

#[tokio::test]
async fn empty_reads_and_invalid_pages_do_not_invent_full_coverage() {
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(root.file("empty"), "").unwrap();
    invoke(
        &tools,
        &root,
        &thread,
        "read_file",
        json!({"path":"empty","offset":2}),
    )
    .await;
    assert!(tools.files.lock().await.reads.is_empty());
    for input in [
        json!({"path":"empty","offset":0}),
        json!({"path":"empty","limit":0}),
        json!({"path":"empty","column_offset":2}),
    ] {
        assert!(tools
            .prepare(root.invocation(&thread, "read_file", input))
            .await
            .is_err());
    }
    invoke(&tools, &root, &thread, "read_file", json!({"path":"empty"})).await;
    assert!(tools
        .files
        .lock()
        .await
        .reads
        .values()
        .next()
        .unwrap()
        .fully_read());
    assert!(
        !invoke(
            &tools,
            &root,
            &thread,
            "write_file",
            json!({"path":"empty","content":"created content"})
        )
        .await
        .is_error
    );
}

#[tokio::test]
async fn changed_snapshot_and_new_identity_discard_old_page_coverage() {
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(root.file("file"), "first\nsecond\n").unwrap();
    invoke(
        &tools,
        &root,
        &thread,
        "read_file",
        json!({"path":"file","limit":1}),
    )
    .await;
    fs::rename(root.file("file"), root.file("previous")).unwrap();
    fs::write(root.file("file"), "first\nsecond\n").unwrap();
    invoke(
        &tools,
        &root,
        &thread,
        "read_file",
        json!({"path":"file","offset":2}),
    )
    .await;
    assert!(!tools
        .files
        .lock()
        .await
        .reads
        .values()
        .next()
        .unwrap()
        .fully_read());
    assert!(tools
        .prepare(root.invocation(
            &thread,
            "write_file",
            json!({"path":"file","content":"new"})
        ))
        .await
        .is_err());
}

#[tokio::test]
async fn multi_edit_is_sequential_and_has_one_atomic_publication() {
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(root.file("file"), "one one\r\nkeep\n").unwrap();
    invoke(&tools, &root, &thread, "read_file", json!({"path":"file"})).await;
    let input = json!({"path":"file","edits":[{"old_string":"one","new_string":"two","replace_all":true},{"old_string":"two two","new_string":"three"}]});
    let prepared = tools
        .prepare(root.invocation(&thread, "multi_edit", input.clone()))
        .await
        .unwrap();
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 1);
    let mut bad = input;
    bad["edits"][1]["old_string"] = json!("missing");
    assert!(tools
        .prepare(root.invocation(&thread, "multi_edit", bad))
        .await
        .is_err());
    assert_eq!(
        fs::read_to_string(root.file("file")).unwrap(),
        "one one\r\nkeep\n"
    );
    assert!(!run(&tools, prepared).await.is_error);
    assert_eq!(
        fs::read_to_string(root.file("file")).unwrap(),
        "three\r\nkeep\n"
    );
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 2);
}

#[test]
fn replacement_uses_nonoverlapping_matches_and_rejects_noops_or_excess_results() {
    let input = |old: &str, new: &str, all| EditInput {
        path: "unused".into(),
        old_string: old.into(),
        new_string: new.into(),
        replace_all: all,
    };
    assert_eq!(replacement("aaa", &input("aa", "b", false)).unwrap(), "ba");
    assert_eq!(replacement("aaaa", &input("aa", "b", true)).unwrap(), "bb");
    assert!(replacement("same", &input("same", "same", false)).is_err());
    assert!(replacement("xx", &input("x", &"y".repeat(READ_LIMIT), true)).is_err());
}

#[tokio::test]
async fn write_creation_never_overwrites_racing_creator() {
    let root = TestDir::new();
    let tools = executor();
    let prepared = tools
        .prepare(root.invocation(
            &ThreadId::new(),
            "write_file",
            json!({"path":"new","content":"approved"}),
        ))
        .await
        .unwrap();
    let target = root.file("new");
    create::CREATE_PUBLISH_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || fs::write(target, "other writer").unwrap()))
    });
    let outcome = run(&tools, prepared).await;
    assert!(outcome.is_error && !outcome.interrupted);
    assert_eq!(
        fs::read_to_string(root.file("new")).unwrap(),
        "other writer"
    );
}

#[tokio::test]
async fn external_paths_bind_explicit_capability_and_tampering_is_rejected() {
    let workspace = TestDir::new();
    let external = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(external.file("file"), "outside").unwrap();
    let prepared = tools
        .prepare(workspace.invocation(&thread, "read_file", json!({"path":external.file("file")})))
        .await
        .unwrap();
    assert!(!tools.capabilities("read_file").external_access);
    let capabilities = tools.prepared_capabilities(&prepared).unwrap();
    assert!(capabilities.external_access && capabilities.requires_approval);
    assert!(!capabilities.mutates_workspace);
    assert!(tools.files.lock().await.reads.is_empty());
    let mut changed = prepared.clone();
    changed.specification["root"] = json!(workspace.0);
    assert!(tools.prepared_capabilities(&changed).is_err());
    assert!(run(&tools, changed).await.is_error);
    assert!(!run(&tools, prepared).await.is_error);
    let write = tools
        .prepare(workspace.invocation(
            &thread,
            "write_file",
            json!({"path":external.file("file"),"content":"approved outside"}),
        ))
        .await
        .unwrap();
    assert!(
        tools
            .prepared_capabilities(&write)
            .unwrap()
            .requires_approval
    );
    assert!(!run(&tools, write).await.is_error);
    assert_eq!(
        fs::read_to_string(external.file("file")).unwrap(),
        "approved outside"
    );
}

#[test]
fn write_plans_validate_all_originals_before_any_publication() {
    let root = TestDir::new();
    fs::write(root.file("a"), "a").unwrap();
    fs::write(root.file("b"), "b").unwrap();
    let plan = PreparedWrites::prepare(
        &root.0,
        vec![
            (root.file("a"), "a".into(), "new a".into()),
            (root.file("b"), "b".into(), "new b".into()),
        ],
    )
    .unwrap();
    assert!(plan.preview().contains("+new a"));
    fs::write(root.file("b"), "other writer").unwrap();
    let (_sender, cancel) = watch::channel(false);
    assert!(plan.execute(&cancel).is_err());
    assert_eq!(fs::read_to_string(root.file("a")).unwrap(), "a");
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 2);
}

#[tokio::test]
async fn write_file_creates_only_approved_missing_parents_at_execution() {
    for external in [false, true] {
        let root = TestDir::new();
        let outside = TestDir::new();
        let tools = executor();
        let target = if external {
            outside.file("new/deep/file")
        } else {
            root.file("new/deep/file")
        };
        let prepared = tools
            .prepare(root.invocation(
                &ThreadId::new(),
                "write_file",
                json!({"path":target,"content":"new"}),
            ))
            .await
            .unwrap();
        assert!(!target.parent().unwrap().exists());
        assert_eq!(
            tools
                .prepared_capabilities(&prepared)
                .unwrap()
                .external_access,
            external
        );
        let result = run(&tools, prepared).await;
        assert!(!result.is_error, "{}", result.text);
        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn newly_raced_parent_symlink_never_grants_external_creation() {
    let root = TestDir::new();
    let outside = TestDir::new();
    let tools = executor();
    let prepared = tools
        .prepare(root.invocation(
            &ThreadId::new(),
            "write_file",
            json!({"path":"new/deep/file","content":"approved"}),
        ))
        .await
        .unwrap();
    std::os::unix::fs::symlink(&outside.0, root.file("new")).unwrap();
    let outcome = run(&tools, prepared).await;
    assert!(outcome.is_error);
    assert!(!outside.file("deep").exists());
}

#[test]
fn numbered_pages_are_bounded_and_diff_previews_keep_newline_evidence() {
    let text = "abcdefghij".repeat(100) + "\n";
    let text = text.repeat(3000);
    let input: ReadInput =
        serde_json::from_value(json!({"path":"unused","limit":usize::MAX})).unwrap();
    let page = read::page(&text, &input).unwrap();
    assert!(page.body.len() < 256 * 1024 + 200);
    assert!(page.body.contains("output truncated"));
    assert!(page.visible.last().unwrap().end < text.len());
    assert!(presentation::diff_preview("old", "new\n", 2048).contains("No newline at end of file"));
}
