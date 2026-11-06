use super::tests::{executor, run, TestDir};
use super::*;
use bob_kernel::{ToolBody, ToolPresentation};

async fn prepare(
    root: &TestDir,
    tools: &ToolService,
    thread: &ThreadId,
    name: &str,
    input: Value,
) -> PreparedTool {
    tools
        .prepare(root.invocation(thread, name, input))
        .await
        .unwrap()
}

fn load_instructions(
    root: &TestDir,
    scope: &str,
    stop: impl Fn() -> bool,
) -> Result<crate::project_instructions::InstructionReport> {
    let directory = workspace_directory(&root.0, scope)?;
    crate::project_instructions::load(&root.0, &directory, stop)
}

#[tokio::test]
async fn creation_preparation_is_pure_and_binds_exact_action() {
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    let prepared = prepare(
        &root,
        &tools,
        &thread,
        "create_file",
        json!({"path":"new.txt", "content":"日本語\r\n"}),
    )
    .await;
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0);
    assert!(
        matches!(&prepared.presentation, Some(ToolPresentation { body: ToolBody::Create { content }, .. }) if content == "日本語\r\n")
    );
    let mut altered = prepared.clone();
    altered.presentation.as_mut().unwrap().body = ToolBody::Create {
        content: "not approved".into(),
    };
    assert!(run(&tools, altered).await.is_error);
    assert!(!root.file("new.txt").exists());
    let mut altered = prepared.clone();
    altered.invocation.input["content"] = json!("not approved");
    assert!(run(&tools, altered).await.is_error);
    let bound: Bound = serde_json::from_value(prepared.specification.clone()).unwrap();
    let Action::Create { path, staged, .. } = bound.action else {
        panic!()
    };
    create::CREATE_PUBLISH_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            assert!(!path.exists());
            assert_eq!(fs::read_to_string(staged).unwrap(), "日本語\r\n");
        }))
    });
    let result = run(&tools, prepared).await;
    assert!(!result.is_error, "{}", result.text);
    assert_eq!(
        result
            .presentation
            .as_ref()
            .unwrap()
            .body_preview
            .as_deref(),
        Some("")
    );
    assert_eq!(
        fs::read(root.file("new.txt")).unwrap(),
        "日本語\r\n".as_bytes()
    );
    assert!(tools.files.lock().await.reads.is_empty());
    assert!(tools
        .prepare(root.invocation(
            &thread,
            "edit_file",
            json!({"path":"new.txt","old_string":"日本語","new_string":"hello"})
        ))
        .await
        .is_err());
    assert!(tools
        .prepare(root.invocation(
            &thread,
            "create_file",
            json!({"path":"new.txt","content":"overwrite"})
        ))
        .await
        .is_err());
    assert!(tools
        .prepare(root.invocation(
            &thread,
            "create_file",
            json!({"path":"missing/new.txt","content":"no parents"})
        ))
        .await
        .is_err());
    assert!(tools
        .prepare(root.invocation(
            &thread,
            "create_file",
            json!({"path":"large.txt","content":"x".repeat(READ_LIMIT + 1)})
        ))
        .await
        .is_err());
}

#[tokio::test]
async fn racing_creator_and_postpublication_writer_are_preserved() {
    for after in [false, true] {
        let root = TestDir::new();
        let tools = executor();
        let prepared = prepare(
            &root,
            &tools,
            &ThreadId::new(),
            "create_file",
            json!({"path":"new.txt","content":"approved"}),
        )
        .await;
        let target = root.file("new.txt");
        let hook: Box<dyn FnOnce()> = Box::new(move || fs::write(&target, "other writer").unwrap());
        if after {
            create::CREATE_PUBLISHED_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
        } else {
            create::CREATE_PUBLISH_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
        }
        let result = run(&tools, prepared).await;
        assert!(result.is_error);
        assert!(result
            .presentation
            .as_ref()
            .is_none_or(|value| value.body_preview.is_none()));
        assert_eq!(result.interrupted, after, "{}", result.text);
        assert_eq!(
            fs::read_to_string(root.file("new.txt")).unwrap(),
            "other writer"
        );
    }
}

#[tokio::test]
async fn cancelled_creation_has_no_side_effects() {
    let root = TestDir::new();
    let tools = executor();
    let prepared = prepare(
        &root,
        &tools,
        &ThreadId::new(),
        "create_file",
        json!({"path":"new.txt","content":"approved"}),
    )
    .await;
    let (_sender, receiver) = watch::channel(true);
    let outcome = tools.execute(prepared, receiver).await.unwrap();
    assert!(outcome.is_error && !outcome.interrupted);
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn create_never_overwrites_dangling_symlinks_or_follows_moved_parent() {
    use std::os::unix::fs::symlink;
    let root = TestDir::new();
    let outside = TestDir::new();
    let tools = executor();
    symlink(outside.file("missing"), root.file("dangling")).unwrap();
    assert!(tools
        .prepare(root.invocation(
            &ThreadId::new(),
            "create_file",
            json!({"path":"dangling","content":"approved"})
        ))
        .await
        .is_err());
    let prepared = prepare(
        &root,
        &tools,
        &ThreadId::new(),
        "create_file",
        json!({"path":"racing","content":"approved"}),
    )
    .await;
    let target = root.file("racing");
    let outside_path = outside.file("missing");
    create::CREATE_PUBLISH_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || symlink(&outside_path, &target).unwrap()))
    });
    let outcome = run(&tools, prepared).await;
    assert!(outcome.is_error && !outcome.interrupted);
    assert!(fs::symlink_metadata(root.file("racing"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!outside.file("missing").exists());

    fs::create_dir(root.file("parent")).unwrap();
    let prepared = prepare(
        &root,
        &tools,
        &ThreadId::new(),
        "create_file",
        json!({"path":"parent/new.txt","content":"approved"}),
    )
    .await;
    let parent = root.file("parent");
    let moved = root.file("moved");
    let outside_path = outside.0.clone();
    create::CREATE_PUBLISH_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            fs::rename(&parent, &moved).unwrap();
            symlink(&outside_path, &parent).unwrap();
        }))
    });
    let outcome = run(&tools, prepared).await;
    assert!(outcome.is_error && outcome.interrupted, "{}", outcome.text);
    assert!(!outside.file("new.txt").exists());
    assert_eq!(
        fs::read_to_string(root.file("moved/new.txt")).unwrap(),
        "approved"
    );
}

#[tokio::test]
async fn search_honors_ignores_unicode_literals_regex_and_grants_no_evidence() {
    let root = TestDir::new();
    fs::create_dir(root.file("nested")).unwrap();
    fs::create_dir(root.file("target")).unwrap();
    fs::write(root.file(".gitignore"), "*.skip\n*.keep\n").unwrap();
    fs::write(root.file("nested/.gitignore"), "!special.keep\n").unwrap();
    fs::write(root.file("source.txt"), "日本語 A+B\naab\n").unwrap();
    fs::write(root.file("nested/日本語.txt"), "日本語 a+b\n").unwrap();
    fs::write(root.file("nested/hidden.skip"), "日本語 A+B\n").unwrap();
    fs::write(root.file("nested/special.keep"), "日本語 A+B\n").unwrap();
    fs::write(root.file("target/generated.txt"), "日本語 A+B\n").unwrap();
    let tools = executor();
    let thread = ThreadId::new();
    let result = run(
        &tools,
        prepare(
            &root,
            &tools,
            &thread,
            "grep",
            json!({"pattern":"A+B","literal":true,"ignore_case":true}),
        )
        .await,
    )
    .await;
    assert!(!result.is_error, "{}", result.text);
    let report: Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(
        report["entries"].as_array().unwrap().len(),
        3,
        "{}",
        result.text
    );
    assert_eq!(report["truncated"], false);
    assert!(report["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["path"] == "nested/日本語.txt" && entry["line"] == 1));
    let result = run(
        &tools,
        prepare(&root, &tools, &thread, "grep", json!({"pattern":"^a+b$"})).await,
    )
    .await;
    let report: Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(report["entries"].as_array().unwrap().len(), 1);
    assert_eq!(report["entries"][0]["line"], 2);
    let result = run(
        &tools,
        prepare(
            &root,
            &tools,
            &thread,
            "glob",
            json!({"pattern":"**/*.txt"}),
        )
        .await,
    )
    .await;
    let report: Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(report["entries"].as_array().unwrap().len(), 2);
    assert!(tools.files.lock().await.reads.is_empty());
    assert!(tools
        .prepare(root.invocation(
            &thread,
            "edit_file",
            json!({"path":"source.txt","old_string":"aab","new_string":"edited"})
        ))
        .await
        .is_err());
    let result = run(
        &tools,
        prepare(&root, &tools, &thread, "list_dir", json!({"path":"nested"})).await,
    )
    .await;
    let report: Value = serde_json::from_str(&result.text).unwrap();
    assert!(report["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["path"] == "nested/hidden.skip"));
}

#[tokio::test]
async fn explicit_search_scopes_cannot_bypass_excluded_ancestors() {
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(root.file(".gitignore"), "private/\n!private/deep/\n").unwrap();
    fs::write(root.file(".ignore"), "hidden/\n").unwrap();
    for excluded in [
        "private",
        "hidden",
        ".git",
        "node_modules",
        "target",
        ".bob-stage",
        ".bob-lab-stage",
    ] {
        fs::create_dir_all(root.file(&format!("{excluded}/deep"))).unwrap();
        fs::write(root.file(&format!("{excluded}/deep/file.txt")), "secret").unwrap();
        fs::write(root.file(&format!("{excluded}/.gitignore")), "!deep/\n").unwrap();
        for path in [excluded.to_owned(), format!("{excluded}/deep")] {
            for (name, mut input) in [
                ("glob", json!({"pattern":"**"})),
                ("grep", json!({"pattern":"secret"})),
            ] {
                input["path"] = json!(path);
                let result = run(&tools, prepare(&root, &tools, &thread, name, input).await).await;
                assert!(result.is_error, "{name} {path}: {}", result.text);
                let report: Value = serde_json::from_str(&result.text).unwrap();
                assert!(report["entries"].as_array().unwrap().is_empty());
                assert_eq!(report["visited"], 0);
                assert_eq!(report["truncated"], true);
                assert!(report["errors"][0]
                    .as_str()
                    .unwrap()
                    .contains("scope was not searched"));
            }
        }
    }
}

#[tokio::test]
async fn ignore_file_precedence_is_global_but_nested_rules_of_same_type_override() {
    let root = TestDir::new();
    fs::create_dir(root.file("nested")).unwrap();
    fs::write(root.file(".ignore"), "*.secret\n!*.visible\n*.local\n").unwrap();
    fs::write(root.file(".gitignore"), "*.git\n").unwrap();
    fs::write(
        root.file("nested/.gitignore"),
        "!*.secret\n*.visible\n!*.git\n",
    )
    .unwrap();
    fs::write(root.file("nested/.ignore"), "!keep.local\n").unwrap();
    for name in [
        "hidden.secret",
        "keep.visible",
        "keep.local",
        "hidden.local",
        "keep.git",
    ] {
        fs::write(root.file(&format!("nested/{name}")), "needle").unwrap();
    }
    let tools = executor();
    let thread = ThreadId::new();
    for path in [".", "nested"] {
        let result = run(
            &tools,
            prepare(
                &root,
                &tools,
                &thread,
                "grep",
                json!({"path":path,"pattern":"needle"}),
            )
            .await,
        )
        .await;
        assert!(!result.is_error, "{}", result.text);
        let report: Value = serde_json::from_str(&result.text).unwrap();
        let paths = report["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["path"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            [
                "nested/keep.git",
                "nested/keep.local",
                "nested/keep.visible"
            ]
        );
    }
}

#[tokio::test]
async fn search_reports_result_and_line_limits_errors_and_cancellation() {
    let root = TestDir::new();
    fs::write(root.file("one.txt"), "match\nmatch\nmatch\n").unwrap();
    let tools = executor();
    let thread = ThreadId::new();
    let result = run(
        &tools,
        prepare(
            &root,
            &tools,
            &thread,
            "grep",
            json!({"pattern":"match","max_results":1}),
        )
        .await,
    )
    .await;
    let report: Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(report["entries"].as_array().unwrap().len(), 1);
    assert_eq!(report["truncated"], true);
    fs::write(root.file("one.txt"), "語".repeat(10_000)).unwrap();
    let result = run(
        &tools,
        prepare(&root, &tools, &thread, "grep", json!({"pattern":"語"})).await,
    )
    .await;
    let report: Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(report["truncated"], true);
    assert!(report["entries"][0]["text"].as_str().unwrap().len() <= 16 * 1024);
    fs::write(root.file("one.txt"), vec![0xff, 0xfe]).unwrap();
    let result = run(
        &tools,
        prepare(&root, &tools, &thread, "grep", json!({"pattern":"x"})).await,
    )
    .await;
    assert!(result.is_error);
    let report: Value = serde_json::from_str(&result.text).unwrap();
    assert!(!report["errors"].as_array().unwrap().is_empty());
    let Action::Repository {
        path, ancestors, ..
    } = repository::prepare(&root.0, "glob", json!({"pattern":"**"})).unwrap()
    else {
        panic!()
    };
    let (_sender, receiver) = watch::channel(true);
    let report = repository::execute(
        &root.0,
        "glob",
        json!({"pattern":"**"}),
        &path,
        &ancestors,
        &receiver,
    )
    .unwrap();
    assert!(report.cancelled && report.truncated);
    assert!(report.entries.is_empty());
    for (name, input) in [
        ("grep", json!({"pattern":"["})),
        ("glob", json!({"pattern":"**","max_results":0})),
        ("list_dir", json!({"path":".."})),
    ] {
        assert!(tools
            .prepare(root.invocation(&thread, name, input))
            .await
            .is_err());
    }
}

#[cfg(unix)]
#[test]
fn search_cannot_follow_symlink_fifo_socket_or_retargeted_ancestor() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink, net::UnixListener};
    let path = Path::new("/tmp").join(format!("bob-sock-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&path).unwrap();
    let mut root = TestDir(path);
    root.0 = fs::canonicalize(&root.0).unwrap();
    let outside = TestDir::new();
    fs::write(outside.file("external.txt"), "outside secret").unwrap();
    symlink(&outside.0, root.file("escape")).unwrap();
    symlink(outside.file("external.txt"), root.file("link.txt")).unwrap();
    let fifo = std::ffi::CString::new(root.file("fifo").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let _socket = UnixListener::bind(root.file("socket")).unwrap();
    let (_sender, receiver) = watch::channel(false);
    let Action::Repository {
        path, ancestors, ..
    } = repository::prepare(&root.0, "grep", json!({"pattern":"secret"})).unwrap()
    else {
        panic!()
    };
    let report = repository::execute(
        &root.0,
        "grep",
        json!({"pattern":"secret"}),
        &path,
        &ancestors,
        &receiver,
    )
    .unwrap();
    assert!(report.entries.is_empty());
    assert_eq!(report.skipped_nonregular, 4);
    fs::create_dir(root.file("parent")).unwrap();
    fs::write(root.file("parent/external.txt"), "inside").unwrap();
    let Action::Repository {
        path, ancestors, ..
    } = repository::prepare(&root.0, "grep", json!({"path":"parent","pattern":"secret"})).unwrap()
    else {
        panic!()
    };
    let parent = root.file("parent");
    let moved = root.file("moved");
    let outside_path = outside.0.clone();
    repository::SEARCH_READ_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            fs::rename(&parent, &moved).unwrap();
            symlink(&outside_path, &parent).unwrap();
        }))
    });
    let report = repository::execute(
        &root.0,
        "grep",
        json!({"path":"parent","pattern":"secret"}),
        &path,
        &ancestors,
        &receiver,
    )
    .unwrap();
    assert!(report.entries.is_empty());
    assert!(report.truncated && !report.errors.is_empty());
}

#[tokio::test]
async fn scoped_conventions_are_quoted_refreshed_and_isolated_without_edit_evidence() {
    let outer = TestDir::new();
    fs::write(outer.file("AGENTS.md"), "OUTSIDE WORKSPACE").unwrap();
    fs::create_dir(outer.file("workspace")).unwrap();
    let root = TestDir(fs::canonicalize(outer.file("workspace")).unwrap());
    fs::create_dir(root.file("one")).unwrap();
    fs::create_dir(root.file("two")).unwrap();
    fs::write(root.file("CLAUDE.md"), "root Claude").unwrap();
    fs::write(root.file("AGENTS.md"), "root agents").unwrap();
    fs::write(root.file("one/AGENTS.md"), "only one").unwrap();
    fs::write(root.file("two/AGENTS.md"), "only two").unwrap();
    fs::write(root.file("one/source.txt"), "source content").unwrap();
    let tools = executor();
    let thread = ThreadId::new();
    let result = run(
        &tools,
        prepare(
            &root,
            &tools,
            &thread,
            "project_instructions",
            json!({"path":"one"}),
        )
        .await,
    )
    .await;
    assert!(result.text.contains("only one") && result.text.contains("root agents"));
    assert!(!result.text.contains("only two") && !result.text.contains("OUTSIDE WORKSPACE"));
    assert!(tools.files.lock().await.reads.is_empty());
    let report = load_instructions(&root, "one", || false).unwrap();
    assert_eq!(
        report
            .sources
            .iter()
            .map(|s| s.source.as_str())
            .collect::<Vec<_>>(),
        ["AGENTS.md", "one/AGENTS.md"]
    );
    assert_eq!(report.sources[1].scope, "one");
    let prepared = prepare(
        &root,
        &tools,
        &thread,
        "read_file",
        json!({"path":"one/source.txt"}),
    )
    .await;
    fs::write(root.file("one/AGENTS.md"), "refreshed instructions").unwrap();
    let result = run(&tools, prepared).await;
    assert!(!result.is_error, "{}", result.text);
    assert!(
        result.text.starts_with("     1\tsource content")
            && result.text.contains("refreshed instructions")
    );
    assert!(!result.text.contains("only two"));
    assert!(tools
        .prepare(root.invocation(
            &thread,
            "edit_file",
            json!({"path":"one/source.txt","old_string":"source content","new_string":"edited"})
        ))
        .await
        .is_ok());
    fs::write(root.file("one/AGENTS.md"), "x".repeat(64 * 1024 + 1)).unwrap();
    let report = load_instructions(&root, "one", || false).unwrap();
    assert!(!report.failures.is_empty());
    assert!(report
        .sources
        .iter()
        .all(|source| source.source != "one/AGENTS.md"));
}

#[cfg(unix)]
#[tokio::test]
async fn instruction_loading_rejects_replaced_workspace_and_scope_after_validation() {
    for replace_root in [false, true] {
        for entry in ["project_instructions", "read_file", "read_before_binding"] {
            let outer = TestDir::new();
            fs::create_dir_all(outer.file("workspace/one")).unwrap();
            let root = TestDir(outer.file("workspace"));
            fs::write(root.file("AGENTS.md"), "approved root conventions").unwrap();
            fs::write(root.file("one/source.txt"), "approved source").unwrap();
            let tools = executor();
            let (name, path) = if entry == "project_instructions" {
                (entry, "one")
            } else {
                ("read_file", "one/source.txt")
            };
            let prepared =
                prepare(&root, &tools, &ThreadId::new(), name, json!({"path":path})).await;
            let target = if replace_root {
                root.0.clone()
            } else {
                root.file("one")
            };
            let moved = outer.file("moved");
            let hook: Box<dyn FnOnce()> = Box::new(move || {
                fs::rename(&target, &moved).unwrap();
                fs::create_dir(&target).unwrap();
                fs::write(target.join("AGENTS.md"), "replacement conventions").unwrap();
                if replace_root {
                    fs::create_dir(target.join("one")).unwrap();
                    fs::write(target.join("one/AGENTS.md"), "replacement conventions").unwrap();
                }
            });
            if entry == "read_before_binding" {
                file::READ_INSTRUCTIONS_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
            } else {
                crate::project_instructions::INSTRUCTION_LOAD_HOOK
                    .with(|slot| *slot.borrow_mut() = Some(hook));
            }
            let result = run(&tools, prepared).await;
            assert!(
                !result.text.contains("replacement conventions"),
                "{}",
                result.text
            );
            if name == "project_instructions" {
                assert!(result.is_error, "{}", result.text);
                assert!(result.text.contains("moved or replaced"), "{}", result.text);
            } else {
                assert!(!result.is_error, "{}", result.text);
                assert!(result.text.starts_with("     1\tapproved source"));
                assert!(
                    result.text.contains("Project conventions unavailable"),
                    "{}",
                    result.text
                );
            }
            assert!(outer.file("moved").exists());
        }
    }
}

#[cfg(windows)]
#[test]
fn windows_pinned_directory_rejects_writers_rename_and_preserves_no_clobber_publication() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let root = TestDir::new();
    fs::create_dir(root.file("parent")).unwrap();
    let writer = || {
        fs::OpenOptions::new()
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(root.file("parent"))
    };
    let existing_writer = writer().unwrap();
    assert!(filesystem::Directory::open(&root.file("parent")).is_err());
    drop(existing_writer);
    let directory = filesystem::Directory::open(&root.file("parent")).unwrap();
    assert_eq!(
        writer().unwrap_err().raw_os_error(),
        Some(ERROR_SHARING_VIOLATION as i32)
    );
    directory.verify().unwrap();
    assert!(fs::rename(root.file("parent"), root.file("moved")).is_err());
    for name in [
        "NUL",
        "CON.txt",
        "COM1",
        "LPT².log",
        "file:stream",
        "alias.",
    ] {
        assert!(
            directory.create_file(std::ffi::OsStr::new(name)).is_err(),
            "{name}"
        );
    }
    let mut staged = directory
        .create_file(std::ffi::OsStr::new("staged"))
        .unwrap();
    staged.write_all(b"complete").unwrap();
    staged.sync_all().unwrap();
    drop(staged);
    fs::write(root.file("parent/target"), "other writer").unwrap();
    assert_eq!(
        directory
            .publish_new(
                std::ffi::OsStr::new("target"),
                std::ffi::OsStr::new("staged")
            )
            .unwrap(),
        filesystem::Publication::Exists
    );
    assert_eq!(
        fs::read_to_string(root.file("parent/target")).unwrap(),
        "other writer"
    );
    assert_eq!(
        directory
            .publish_new(
                std::ffi::OsStr::new("created"),
                std::ffi::OsStr::new("staged")
            )
            .unwrap(),
        filesystem::Publication::Published
    );
    assert_eq!(
        fs::read_to_string(root.file("parent/created")).unwrap(),
        "complete"
    );
    directory.verify().unwrap();
}

#[test]
fn repository_previews_are_human_bounded_and_keep_incomplete_diagnostics_visible() {
    let mut report = repository::SearchReport::default();
    report.entries.push(repository::SearchEntry {
        path: "source/日本語.txt".into(),
        kind: "match",
        line: Some(12),
        text: Some("語".repeat(1000)),
    });
    let preview = presentation::repository_result(&report).unwrap();
    assert!(preview.starts_with("source/日本語.txt:12:語"));
    assert!(preview.len() <= 2048);
    assert!(preview.ends_with("[Preview; inspect for full result]"));
    report.entries = vec![
        repository::SearchEntry {
            path: "source".into(),
            kind: "directory",
            line: None,
            text: None,
        },
        repository::SearchEntry {
            path: "source/main.rs".into(),
            kind: "file",
            line: None,
            text: None,
        },
    ];
    assert_eq!(
        presentation::repository_result(&report).unwrap(),
        "source/\nsource/main.rs\n[Preview; inspect for full result]"
    );
    report.errors.push("file changed during search read".into());
    assert!(presentation::repository_result(&report)
        .unwrap()
        .contains("file changed during search read"));
    report.errors.clear();
    report.truncated = true;
    assert!(presentation::repository_result(&report)
        .unwrap()
        .contains("Search incomplete"));
    report.truncated = false;
    report.cancelled = true;
    assert!(presentation::repository_result(&report)
        .unwrap()
        .contains("Search cancelled"));
}

#[tokio::test]
async fn search_output_and_native_directory_batches_are_bounded() {
    let root = TestDir::new();
    for index in 0..30 {
        fs::write(root.file(&format!("{index}.txt")), "x".repeat(16 * 1024)).unwrap();
    }
    let directory = filesystem::Directory::open(&root.0).unwrap();
    let batch = directory.entries(1, || false).unwrap();
    assert_eq!(batch.entries.len(), 1);
    assert!(batch.truncated);
    let batch = directory.entries(100, || true).unwrap();
    assert!(batch.truncated && batch.entries.is_empty());
    let tools = executor();
    let result = run(
        &tools,
        prepare(
            &root,
            &tools,
            &ThreadId::new(),
            "grep",
            json!({"pattern":"x"}),
        )
        .await,
    )
    .await;
    let report: Value = serde_json::from_str(&result.text).unwrap();
    assert_eq!(report["truncated"], true);
    assert!(report["limits_reached"]
        .as_array()
        .unwrap()
        .iter()
        .any(|limit| limit == "output_bytes"));
    assert!(result.text.len() <= 256 * 1024);
}

#[test]
fn instruction_aggregate_limit_and_cancellation_are_explicit() {
    let root = TestDir::new();
    fs::create_dir_all(root.file("one/two/three/four")).unwrap();
    for scope in [
        "",
        "one/",
        "one/two/",
        "one/two/three/",
        "one/two/three/four/",
    ] {
        fs::write(
            root.file(&format!("{scope}AGENTS.md")),
            "x".repeat(64 * 1024),
        )
        .unwrap();
    }
    let report = load_instructions(&root, "one/two/three/four", || false).unwrap();
    assert!(report.truncated && !report.failures.is_empty());
    assert_eq!(
        report
            .sources
            .iter()
            .map(|source| source.text.len())
            .sum::<usize>(),
        256 * 1024
    );
    let report = load_instructions(&root, "one", || true).unwrap();
    assert!(report.cancelled && report.truncated && report.sources.is_empty());
}

#[cfg(unix)]
#[test]
fn instruction_symlinks_never_load_outside_conventions() {
    let root = TestDir::new();
    let outside = TestDir::new();
    fs::write(outside.file("AGENTS.md"), "outside instructions").unwrap();
    std::os::unix::fs::symlink(outside.file("AGENTS.md"), root.file("AGENTS.md")).unwrap();
    std::os::unix::fs::symlink(&outside.0, root.file("escape")).unwrap();
    let report = load_instructions(&root, ".", || false).unwrap();
    assert!(!report.failures.is_empty() && report.sources.is_empty());
    assert!(load_instructions(&root, "escape", || false).is_err());
}
