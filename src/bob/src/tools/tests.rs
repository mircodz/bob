use super::*;
use bob_kernel::{OperationId, RunId};

pub(super) struct TestDir(pub(super) PathBuf);

impl TestDir {
    pub(super) fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-adapter-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
    pub(super) fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
    pub(super) fn invocation(&self, thread: &ThreadId, name: &str, input: Value) -> Invocation {
        Invocation {
            operation_id: OperationId::new(),
            run_id: RunId::new(),
            thread_id: thread.clone(),
            workspace: self.0.to_str().unwrap().into(),
            name: name.into(),
            input,
        }
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) fn executor() -> ToolService {
    ToolService::default()
}

pub(super) async fn run(executor: &ToolService, prepared: PreparedTool) -> ToolOutcome {
    let (_sender, receiver) = watch::channel(false);
    executor.execute(prepared, receiver).await.unwrap()
}

pub(super) fn edit(root: &TestDir, thread: &ThreadId, old: &str, new: &str) -> Invocation {
    root.invocation(
        thread,
        "edit_file",
        json!({"path":"source.txt", "old_string":old, "new_string":new}),
    )
}

pub(super) async fn read(executor: &ToolService, root: &TestDir, thread: &ThreadId) -> ToolOutcome {
    let invocation = root.invocation(thread, "read_file", json!({"path":"source.txt"}));
    run(executor, executor.prepare(invocation).await.unwrap()).await
}

#[tokio::test]
async fn prepare_is_read_only_and_only_returned_text_authorizes_this_thread() {
    let root = TestDir::new();
    fs::write(root.file("source.txt"), "before\r\n日本語\n").unwrap();
    let executor = executor();
    let thread = ThreadId::new();
    let prepared_read = executor
        .prepare(root.invocation(&thread, "read_file", json!({"path":"source.txt"})))
        .await
        .unwrap();
    assert!(executor
        .prepare(edit(&root, &thread, "before", "after"))
        .await
        .is_err());
    assert_eq!(
        run(&executor, prepared_read).await.text,
        "     1\tbefore\r\n     2\t日本語"
    );
    assert!(executor
        .prepare(edit(&root, &ThreadId::new(), "before", "after"))
        .await
        .is_err());
    let prepared = executor
        .prepare(edit(&root, &thread, "before", "after"))
        .await
        .unwrap();
    let bound: Bound = serde_json::from_value(prepared.specification.clone()).unwrap();
    let Action::Edit { staged, backup, .. } = bound.action else {
        panic!()
    };
    assert!(!staged.exists() && !backup.exists());
    assert_eq!(
        fs::read_to_string(root.file("source.txt")).unwrap(),
        "before\r\n日本語\n"
    );
    let outcome = run(&executor, prepared).await;
    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
        fs::read_to_string(root.file("source.txt")).unwrap(),
        "after\r\n日本語\n"
    );
    #[cfg(unix)]
    assert_eq!(fs::read_to_string(&staged).unwrap(), "before\r\n日本語\n");
    #[cfg(windows)]
    assert_eq!(fs::read_to_string(&backup).unwrap(), "before\r\n日本語\n");
    assert!(executor
        .prepare(edit(&root, &thread, "after", "next"))
        .await
        .is_ok());
}

#[tokio::test]
async fn stale_or_tampered_prepared_operations_never_overwrite() {
    let root = TestDir::new();
    fs::write(root.file("source.txt"), "before").unwrap();
    let executor = executor();
    let thread = ThreadId::new();
    assert!(!read(&executor, &root, &thread).await.is_error);
    let prepared = executor
        .prepare(edit(&root, &thread, "before", "after"))
        .await
        .unwrap();
    let mut tampered = prepared.clone();
    tampered.invocation.input["new_string"] = json!("not approved");
    assert!(run(&executor, tampered).await.is_error);
    fs::write(root.file("source.txt"), "external change").unwrap();
    let outcome = run(&executor, prepared).await;
    assert!(outcome.is_error && !outcome.interrupted);
    assert_eq!(
        fs::read_to_string(root.file("source.txt")).unwrap(),
        "external change"
    );
}

#[tokio::test]
async fn failed_and_cancelled_reads_do_not_grant_evidence() {
    let root = TestDir::new();
    fs::write(root.file("source.txt"), "before").unwrap();
    let executor = executor();
    let thread = ThreadId::new();
    let prepared = executor
        .prepare(root.invocation(&thread, "read_file", json!({"path":"source.txt"})))
        .await
        .unwrap();
    let (_sender, receiver) = watch::channel(true);
    let outcome = executor.execute(prepared, receiver).await.unwrap();
    assert!(outcome.is_error && !outcome.interrupted);
    assert!(executor
        .prepare(edit(&root, &thread, "before", "after"))
        .await
        .is_err());
    fs::write(root.file("source.txt"), vec![b'x'; READ_LIMIT + 1]).unwrap();
    assert!(executor
        .prepare(root.invocation(&thread, "read_file", json!({"path":"source.txt"})))
        .await
        .is_err());
    assert!(executor.files.lock().await.reads.is_empty());
    fs::write(root.file("source.txt"), [0xff, 0xfe]).unwrap();
    assert!(executor
        .prepare(root.invocation(&thread, "read_file", json!({"path":"source.txt"})))
        .await
        .is_err());
}

#[test]
fn replacement_rejects_ambiguous_and_missing_matches() {
    for (text, old) in [("x x", "x"), ("abc", "z"), ("abc", "")] {
        let input = EditInput {
            path: "unused".into(),
            old_string: old.into(),
            new_string: "new".into(),
            replace_all: false,
        };
        assert!(replacement(text, &input).is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn escapes_and_retargeted_symlinks_are_rejected() {
    use std::os::unix::fs::symlink;
    let root = TestDir::new();
    let outside = TestDir::new();
    fs::write(outside.file("source.txt"), "outside").unwrap();
    symlink(&outside.0, root.file("escape")).unwrap();
    for path in [
        "escape/source.txt".to_owned(),
        "../source.txt".into(),
        outside.file("source.txt").to_str().unwrap().into(),
    ] {
        assert!(workspace_file(&root.0, &path).is_err());
    }
    fs::write(root.file("source.txt"), "inside").unwrap();
    let executor = executor();
    let thread = ThreadId::new();
    read(&executor, &root, &thread).await;
    let prepared = executor
        .prepare(edit(&root, &thread, "inside", "after"))
        .await
        .unwrap();
    fs::remove_file(root.file("source.txt")).unwrap();
    symlink(outside.file("source.txt"), root.file("source.txt")).unwrap();
    assert!(run(&executor, prepared).await.is_error);
    assert_eq!(
        fs::read_to_string(outside.file("source.txt")).unwrap(),
        "outside"
    );
}

#[test]
fn staging_rechecks_contents_and_cancellation_without_clobbering() {
    let root = TestDir::new();
    let target = root.file("source.txt");
    let stage = root.file("stage");
    let backup = root.file("backup");
    fs::write(&target, "before").unwrap();
    let calls = std::cell::Cell::new(0);
    let outcome = guarded_replace(
        &target,
        &stage,
        &backup,
        ("before", &read_snapshot(&target).unwrap().1),
        "after",
        || {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                fs::write(&target, "external")?;
                bail!("concurrent writer");
            }
            Ok(())
        },
        || false,
    );
    assert!(outcome.is_err());
    assert_eq!(fs::read_to_string(&target).unwrap(), "external");
    assert!(!stage.exists() && !backup.exists());
    fs::write(&target, "before").unwrap();
    let outcome = guarded_replace(
        &target,
        &stage,
        &backup,
        ("before", &read_snapshot(&target).unwrap().1),
        "after",
        || Ok(()),
        || true,
    )
    .unwrap();
    assert!(outcome.is_error && !outcome.interrupted);
    assert_eq!(fs::read_to_string(&target).unwrap(), "before");
    assert!(!stage.exists() && !backup.exists());
}

#[cfg(unix)]
#[test]
fn publish_retains_actual_displaced_writer_and_never_rolls_back_over_later_writer() {
    for (replace_inode, later_writer) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let root = TestDir::new();
        let target = root.file("source.txt");
        let stage = root.file("stage");
        let backup = root.file("backup");
        fs::write(&target, "before").unwrap();
        let approved = read_snapshot(&target).unwrap().1;
        let writer_target = target.clone();
        let external = root.file("external");
        PUBLISH_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                if replace_inode {
                    fs::write(&external, "before").unwrap();
                    fs::rename(&external, &writer_target).unwrap();
                } else {
                    fs::write(&writer_target, "concurrent").unwrap();
                }
            }))
        });
        let later_target = target.clone();
        PUBLISHED_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                if later_writer {
                    fs::write(&later_target, "later writer").unwrap();
                }
            }))
        });
        let outcome = guarded_replace(
            &target,
            &stage,
            &backup,
            ("before", &approved),
            "after",
            || Ok(()),
            || false,
        )
        .unwrap();
        assert!(outcome.is_error && outcome.interrupted, "{}", outcome.text);
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            if later_writer {
                "later writer"
            } else {
                "after"
            }
        );
        assert_eq!(
            fs::read_to_string(&stage).unwrap(),
            if replace_inode {
                "before"
            } else {
                "concurrent"
            }
        );
        assert!(!backup.exists());
    }
}

#[cfg(unix)]
#[test]
fn ancestor_moves_cannot_redirect_staging_publication_or_cleanup() {
    use std::os::unix::fs::symlink;
    for phase in ["stage", "publish", "cleanup"] {
        let root = TestDir::new();
        let outside = TestDir::new();
        let parent = root.file("parent");
        let moved = root.file("moved");
        fs::create_dir(&parent).unwrap();
        fs::write(parent.join("source.txt"), "before").unwrap();
        fs::write(outside.file("source.txt"), "outside").unwrap();
        fs::write(outside.file("stage"), "outside stage").unwrap();
        let target = parent.join("source.txt");
        let stage = parent.join("stage");
        let backup = parent.join("backup");
        let approved = read_snapshot(&target).unwrap().1;
        let move_parent = parent.clone();
        let moved_parent = moved.clone();
        let outside_path = outside.0.clone();
        let hook: Box<dyn FnOnce()> = Box::new(move || {
            fs::rename(move_parent.clone(), &moved_parent).unwrap();
            symlink(&outside_path, &move_parent).unwrap();
        });
        match phase {
            "stage" => STAGE_HOOK.with(|slot| *slot.borrow_mut() = Some(hook)),
            "publish" => PUBLISH_HOOK.with(|slot| *slot.borrow_mut() = Some(hook)),
            _ => STAGE_HOOK.with(|slot| *slot.borrow_mut() = Some(hook)),
        }
        // For cleanup, move after stage creation in the second validation.
        let cleanup_hook = if phase == "cleanup" {
            STAGE_HOOK.with(|slot| slot.borrow_mut().take())
        } else {
            None
        };
        let cleanup_hook = std::cell::RefCell::new(cleanup_hook);
        let calls = std::cell::Cell::new(0);
        let result = guarded_replace(
            &target,
            &stage,
            &backup,
            ("before", &approved),
            "after",
            || {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    if let Some(hook) = cleanup_hook.borrow_mut().take() {
                        hook();
                        bail!("directory moved");
                    }
                }
                Ok(())
            },
            || false,
        );
        if phase == "publish" {
            let outcome = result.unwrap();
            assert!(outcome.is_error && outcome.interrupted, "{}", outcome.text);
            assert_eq!(
                fs::read_to_string(moved.join("source.txt")).unwrap(),
                "after"
            );
            assert_eq!(fs::read_to_string(moved.join("stage")).unwrap(), "before");
        } else {
            assert!(result.is_err());
            assert_eq!(
                fs::read_to_string(moved.join("source.txt")).unwrap(),
                "before"
            );
            assert!(!moved.join("stage").exists());
        }
        assert_eq!(
            fs::read_to_string(outside.file("source.txt")).unwrap(),
            "outside"
        );
        assert_eq!(
            fs::read_to_string(outside.file("stage")).unwrap(),
            "outside stage"
        );
        assert!(!outside.file("backup").exists());
    }
}

#[test]
fn successful_publication_retains_late_writes_through_old_handle() {
    let root = TestDir::new();
    let target = root.file("source.txt");
    let stage = root.file("stage");
    let backup = root.file("backup");
    fs::write(&target, "before").unwrap();
    let approved = read_snapshot(&target).unwrap().1;
    let mut writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&target)
        .unwrap();
    let outcome = guarded_replace(
        &target,
        &stage,
        &backup,
        ("before", &approved),
        "after",
        || Ok(()),
        || false,
    )
    .unwrap();
    assert!(!outcome.is_error, "{}", outcome.text);
    writer.write_all(b"writer").unwrap();
    writer.sync_all().unwrap();
    #[cfg(unix)]
    let recovery = stage;
    #[cfg(not(unix))]
    let recovery = backup;
    assert_eq!(fs::read_to_string(recovery).unwrap(), "writer");
    assert_eq!(fs::read_to_string(target).unwrap(), "after");
}

#[tokio::test]
async fn strict_inputs_reject_unknown_fields_and_wrong_types() {
    let root = TestDir::new();
    let executor = executor();
    let thread = ThreadId::new();
    for (name, input) in [
        ("read_file", json!({"path":"source.txt", "offset":"1"})),
        (
            "edit_file",
            json!({"path":"source.txt", "old_string":"x", "new_string":"y", "replace_all":"true"}),
        ),
        ("bash", json!({"command":"echo hi", "timeout":"1"})),
        ("bash", json!({"command":"echo hi", "timeout":0})),
        ("bash", json!({"command":"echo hi", "timeout":-1})),
        ("bash", json!({"command":"echo hi", "timeout":1, "cwd":"/"})),
    ] {
        assert!(executor
            .prepare(root.invocation(&thread, name, input))
            .await
            .is_err());
    }
    for tool in executor.tools() {
        assert_eq!(tool.spec.input_schema["additionalProperties"], false);
        assert_eq!(
            tool.effect,
            if matches!(
                tool.spec.name.as_str(),
                "read_file" | "list_dir" | "glob" | "grep" | "project_instructions"
            ) {
                EffectClass::Read
            } else {
                EffectClass::Write
            }
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_reports_exact_dialect_exit_and_bounded_streams() {
    let root = TestDir::new();
    let executor = executor();
    let thread = ThreadId::new();
    let invocation = root.invocation(
        &thread,
        "bash",
        json!({"command":"printf 'out'; printf 'err' >&2; exit 7", "timeout":5}),
    );
    let prepared = executor.prepare(invocation).await.unwrap();
    let bound: Bound = serde_json::from_value(prepared.specification.clone()).unwrap();
    let Action::Shell {
        executable,
        dialect,
        ..
    } = bound.action
    else {
        panic!()
    };
    assert_eq!(dialect, "Bash");
    assert!(prepared.preview.contains(executable.to_str().unwrap()));
    assert!(prepared
        .preview
        .contains("printf 'out'; printf 'err' >&2; exit 7"));
    let outcome = run(&executor, prepared).await;
    assert!(outcome.is_error && !outcome.interrupted, "{}", outcome.text);
    let value: Value = serde_json::from_str(&outcome.text).unwrap();
    assert_eq!(value["exit_code"], 7);
    assert_eq!(value["stdout"]["text"], "out");
    assert_eq!(value["stderr"]["text"], "err");
    let prepared = executor.prepare(root.invocation(&thread, "bash", json!({"command":"head -c 300000 /dev/zero | tr '\\000' x; head -c 300000 /dev/zero | tr '\\000' y >&2", "timeout":5}))).await.unwrap();
    let output = run(&executor, prepared).await;
    assert!(!output.is_error, "{}", output.text);
    let value: Value = serde_json::from_str(&output.text).unwrap();
    for stream in ["stdout", "stderr"] {
        assert_eq!(value[stream]["total_bytes"], 300000);
        assert_eq!(value[stream]["retained_bytes"], STREAM_LIMIT);
        assert_eq!(value[stream]["truncated"], true);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_is_uncertain_but_precancelled_shell_does_not_start() {
    let root = TestDir::new();
    let executor = executor();
    let thread = ThreadId::new();
    let prepared = executor
        .prepare(root.invocation(
            &thread,
            "bash",
            json!({"command":"printf partial; sleep 60", "timeout":1}),
        ))
        .await
        .unwrap();
    // The watchdog includes image verification, two startup budgets, and cleanup;
    // the one-second command deadline starts only after the shell is spawned.
    let outcome = tokio::time::timeout(Duration::from_secs(40), run(&executor, prepared))
        .await
        .unwrap();
    assert!(outcome.is_error && outcome.interrupted);
    let value: Value = serde_json::from_str(&outcome.text).unwrap();
    assert_eq!(value["outcome"], "timed_out");
    assert!(value["stdout"]["text"]
        .as_str()
        .unwrap()
        .contains("partial"));
    let prepared = executor
        .prepare(root.invocation(
            &thread,
            "bash",
            json!({"command":"touch must-not-exist", "timeout":1}),
        ))
        .await
        .unwrap();
    let (_sender, receiver) = watch::channel(true);
    let outcome = executor.execute(prepared, receiver).await.unwrap();
    assert!(outcome.is_error && !outcome.interrupted);
    assert!(!root.file("must-not-exist").exists());
}
