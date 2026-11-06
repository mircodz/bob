use super::tests::{executor, run, TestDir};
use super::*;

fn dos(path: &Path) -> String {
    path.to_str()
        .unwrap()
        .strip_prefix(r"\\?\")
        .unwrap()
        .to_owned()
}

fn junction(path: &Path, target: &Path) {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA0000003;
    const FSCTL_SET_REPARSE_POINT: u32 = 0x900A4;
    let print = dos(target);
    let substitute = format!(r"\??\{print}");
    let substitute: Vec<u16> = substitute.encode_utf16().collect();
    let print: Vec<u16> = print.encode_utf16().collect();
    let data_len = 8 + (substitute.len() + print.len() + 2) * 2;
    assert!(data_len + 8 <= 16 * 1024);
    let mut buffer = Vec::new();
    buffer.extend_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    for value in [
        data_len as u16,
        0,
        0,
        (substitute.len() * 2) as u16,
        ((substitute.len() + 1) * 2) as u16,
        (print.len() * 2) as u16,
    ] {
        buffer.extend_from_slice(&value.to_le_bytes());
    }
    for value in substitute
        .into_iter()
        .chain(Some(0))
        .chain(print)
        .chain(Some(0))
    {
        buffer.extend_from_slice(&value.to_le_bytes());
    }
    fs::create_dir(path).unwrap();
    let directory = fs::OpenOptions::new()
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .unwrap();
    let mut returned = 0;
    assert_ne!(
        unsafe {
            DeviceIoControl(
                directory.as_raw_handle(),
                FSCTL_SET_REPARSE_POINT,
                buffer.as_ptr().cast(),
                buffer.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        },
        0,
        "create test junction: {}",
        std::io::Error::last_os_error()
    );
}

#[tokio::test]
async fn windows_launch_junction_resolves_but_interior_junctions_fail_closed() {
    let root = TestDir::new();
    let physical = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(physical.file("source.txt"), "physical source").unwrap();
    junction(&root.file("launch"), &physical.0);
    let mut invocation = root.invocation(&thread, "read_file", json!({"path":"source.txt"}));
    invocation.workspace = dos(&root.file("launch"));
    let prepared = tools.prepare(invocation).await.unwrap();
    assert_eq!(
        tools.prepared_authorization_target(&prepared).unwrap(),
        Some(physical.file("source.txt"))
    );
    assert!(
        !tools
            .prepared_capabilities(&prepared)
            .unwrap()
            .external_access
    );
    assert!(!run(&tools, prepared).await.is_error);
    for (name, input) in [
        ("read_file", json!({"path":"launch/source.txt"})),
        (
            "write_file",
            json!({"path":"launch/missing/child.txt","content":"no"}),
        ),
        ("project_instructions", json!({"path":"launch"})),
    ] {
        let error = tools
            .prepare(root.invocation(&thread, name, input))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("reparse"), "{error:#}");
    }
    assert!(!physical.file("missing").exists());
    assert_eq!(
        fs::read_to_string(physical.file("source.txt")).unwrap(),
        "physical source"
    );
    fs::remove_dir(root.file("launch")).unwrap();
}

#[tokio::test]
#[ignore = "requires native NTFS directory case-sensitivity support"]
async fn windows_case_sensitive_directory_keeps_distinct_targets_and_evidence() {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileCaseSensitiveInfo, SetFileInformationByHandle, FILE_CASE_SENSITIVE_INFO,
        FILE_FLAG_BACKUP_SEMANTICS,
    };
    let root = TestDir::new();
    let directory = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(&root.0)
        .unwrap();
    let info = FILE_CASE_SENSITIVE_INFO { Flags: 1 };
    assert_ne!(
        unsafe {
            SetFileInformationByHandle(
                directory.as_raw_handle(),
                FileCaseSensitiveInfo,
                (&info as *const FILE_CASE_SENSITIVE_INFO).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        },
        0,
        "enable directory case sensitivity: {}",
        std::io::Error::last_os_error()
    );
    drop(directory);
    fs::write(root.file("Case.txt"), "upper").unwrap();
    fs::write(root.file("case.txt"), "lower").unwrap();
    let tools = executor();
    let thread = ThreadId::new();
    let read = tools
        .prepare(root.invocation(
            &thread,
            "read_file",
            json!({"path":dos(&root.file("Case.txt"))}),
        ))
        .await
        .unwrap();
    assert_eq!(
        tools.prepared_authorization_target(&read).unwrap(),
        Some(root.file("Case.txt"))
    );
    assert!(!run(&tools, read).await.is_error);
    assert!(tools
        .prepare(root.invocation(
            &thread,
            "edit_file",
            json!({"path":"case.txt","old_string":"lower","new_string":"changed"})
        ))
        .await
        .is_err());
    assert_eq!(fs::read_to_string(root.file("case.txt")).unwrap(), "lower");
}

#[test]
fn windows_path_forms_are_explicit_and_case_is_preserved() {
    for (ordinary, extended) in [
        (r"C:\Work\日本語.txt", r"\\?\C:\Work\日本語.txt"),
        (r"c:\Work\日本語.txt", r"\\?\C:\Work\日本語.txt"),
        (r"C:/Work/日本語.txt", r"\\?\C:\Work\日本語.txt"),
        (
            r"\\server\share\Work\file",
            r"\\?\UNC\server\share\Work\file",
        ),
    ] {
        let expected = PathBuf::from(extended);
        assert_eq!(
            filesystem::normalize_path(Path::new(ordinary)).unwrap(),
            expected
        );
        assert_eq!(
            filesystem::normalize_path(Path::new(extended)).unwrap(),
            expected
        );
    }
    let root = Path::new(r"\\?\C:\Work");
    assert_eq!(
        workspace_target(root, r"C:\Work\missing\new.txt").unwrap(),
        root.join(r"missing\new.txt")
    );
    assert!(workspace_target(root, r"C:\work\file").is_err());
    assert!(workspace_target(root, r"C:\Workspace\file").is_err());
    assert_ne!(
        filesystem::normalize_path(Path::new(r"C:\Work\Case")).unwrap(),
        filesystem::normalize_path(Path::new(r"C:\Work\case")).unwrap()
    );
    for invalid in [
        r"C:relative",
        r"C:",
        r"\root-relative",
        "/root-relative",
        r"\\.\C:\file",
        r"\\?\GLOBALROOT\Device\HarddiskVolume1\file",
        r"\\?\Volume{00000000-0000-0000-0000-000000000000}\file",
        r"C:\a\..\b",
        r"\\?\C:\a\..\b",
        r"C:\file:stream",
        r"C:\bad.\file",
        r"C:\bad \file",
        r"C:\NUL.txt",
        r"C:\COM¹.txt",
        r"C:\CON .txt",
        r"C:\file?.txt",
        "C:\\bad\0name",
    ] {
        assert!(
            filesystem::normalize_path(Path::new(invalid)).is_err(),
            "{invalid:?}"
        );
    }
}

#[tokio::test]
async fn windows_invalid_missing_suffixes_have_zero_effects() {
    let root = TestDir::new();
    let tools = executor();
    for suffix in [
        "NUL.txt",
        "bad.",
        "bad ",
        "file:stream",
        "bad?name",
        "COM¹.txt",
        "CON .txt",
        "valid/NUL/leaf.txt",
        "valid/bad./leaf.txt",
        "valid/../leaf.txt",
        "bad\0name",
    ] {
        for path in [
            format!("missing/{suffix}"),
            format!("{}\\missing\\{suffix}", dos(&root.0)),
        ] {
            let result = tools
                .prepare(root.invocation(
                    &ThreadId::new(),
                    "write_file",
                    json!({"path":path,"content":"never written"}),
                ))
                .await;
            assert!(result.is_err(), "{suffix:?}");
            assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0, "{suffix:?}");
        }
    }
    let too_long = format!("missing/{}", "x".repeat(256));
    assert!(tools
        .prepare(root.invocation(
            &ThreadId::new(),
            "write_file",
            json!({"path":too_long,"content":"no"})
        ))
        .await
        .is_err());
    assert_eq!(fs::read_dir(&root.0).unwrap().count(), 0);
}

#[tokio::test]
async fn windows_equivalent_paths_share_evidence_and_primary_target_not_ancestor() {
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(root.file("source.txt"), "before").unwrap();
    let original = root.invocation(
        &thread,
        "read_file",
        json!({"path":dos(&root.file("source.txt"))}),
    );
    let original_bytes = serde_json::to_vec(&original).unwrap();
    let read = tools.prepare(original).await.unwrap();
    assert_eq!(
        serde_json::to_vec(&read.invocation).unwrap(),
        original_bytes
    );
    assert_eq!(
        tools.prepared_authorization_target(&read).unwrap(),
        Some(root.file("source.txt"))
    );
    assert!(!tools.prepared_capabilities(&read).unwrap().external_access);
    assert!(!run(&tools, read).await.is_error);
    let edit = tools
        .prepare(root.invocation(
            &thread,
            "edit_file",
            json!({
                "path":root.file("source.txt"), "old_string":"before", "new_string":"after"
            }),
        ))
        .await
        .unwrap();
    assert!(!run(&tools, edit).await.is_error);
    assert_eq!(
        fs::read_to_string(root.file("source.txt")).unwrap(),
        "after"
    );
    let create = tools
        .prepare(root.invocation(
            &thread,
            "write_file",
            json!({
                "path":dos(&root.file("missing/new.txt")), "content":"approved"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        tools.prepared_authorization_target(&create).unwrap(),
        Some(root.file("missing/new.txt"))
    );
    assert!(!root.file("missing").exists());
    let (_sender, cancelled) = watch::channel(true);
    assert!(
        tools
            .execute(create.clone(), cancelled)
            .await
            .unwrap()
            .is_error
    );
    assert!(!root.file("missing").exists());
    assert!(!run(&tools, create).await.is_error);
    assert_eq!(
        fs::read_to_string(root.file("missing/new.txt")).unwrap(),
        "approved"
    );
}

#[tokio::test]
async fn windows_external_and_default_directory_targets_remain_narrow() {
    let root = TestDir::new();
    let outside = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(outside.file("source.txt"), "outside").unwrap();
    for path in [
        dos(&outside.file("source.txt")),
        outside.file("source.txt").to_str().unwrap().to_owned(),
    ] {
        let prepared = tools
            .prepare(root.invocation(&thread, "read_file", json!({"path":path})))
            .await
            .unwrap();
        let caps = tools.prepared_capabilities(&prepared).unwrap();
        assert!(caps.external_access && caps.requires_approval);
        assert_eq!(
            tools.prepared_authorization_target(&prepared).unwrap(),
            Some(outside.file("source.txt"))
        );
    }
    let prepared = tools
        .prepare(root.invocation(
            &thread,
            "write_file",
            json!({
                "path":dos(&outside.file("absent/child/new.txt")), "content":"outside"
            }),
        ))
        .await
        .unwrap();
    assert!(
        tools
            .prepared_capabilities(&prepared)
            .unwrap()
            .requires_approval
    );
    assert_eq!(
        tools.prepared_authorization_target(&prepared).unwrap(),
        Some(outside.file("absent/child/new.txt"))
    );
    assert!(!outside.file("absent").exists());
    let prepared = tools
        .prepare(root.invocation(&thread, "list_dir", json!({})))
        .await
        .unwrap();
    assert_eq!(
        tools.prepared_authorization_target(&prepared).unwrap(),
        Some(root.0.clone())
    );
}

#[tokio::test]
async fn windows_long_unicode_paths_publish_and_load_scoped_instructions() {
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    let relative = format!(
        "{}/{}/{}/日本語.txt",
        "a".repeat(90),
        "b".repeat(90),
        "c".repeat(90)
    );
    let path = root.file(&relative);
    let prepared = tools
        .prepare(root.invocation(
            &thread,
            "write_file",
            json!({"path":dos(&path),"content":"before λ"}),
        ))
        .await
        .unwrap();
    assert!(!run(&tools, prepared).await.is_error);
    fs::write(
        path.parent().unwrap().join("AGENTS.md"),
        "scoped convention",
    )
    .unwrap();
    let prepared = tools
        .prepare(root.invocation(&thread, "read_file", json!({"path":dos(&path)})))
        .await
        .unwrap();
    let outcome = run(&tools, prepared).await;
    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(
        outcome.text.contains("scoped convention"),
        "{}",
        outcome.text
    );
    let prepared = tools
        .prepare(root.invocation(
            &thread,
            "edit_file",
            json!({"path":path,"old_string":"before","new_string":"after"}),
        ))
        .await
        .unwrap();
    let bound: Bound = serde_json::from_value(prepared.specification.clone()).unwrap();
    let Action::Edit { backup, .. } = bound.action else {
        panic!()
    };
    let outcome = run(&tools, prepared).await;
    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(fs::read_to_string(path).unwrap(), "after λ");
    assert_eq!(fs::read_to_string(backup).unwrap(), "before λ");
}

#[tokio::test]
async fn windows_replacement_sharing_conflict_retains_original_and_stage() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
    let root = TestDir::new();
    let tools = executor();
    let thread = ThreadId::new();
    fs::write(root.file("source.txt"), "before").unwrap();
    assert!(!super::tests::read(&tools, &root, &thread).await.is_error);
    let prepared = tools
        .prepare(super::tests::edit(&root, &thread, "before", "after"))
        .await
        .unwrap();
    let bound: Bound = serde_json::from_value(prepared.specification.clone()).unwrap();
    let Action::Edit { staged, backup, .. } = bound.action else {
        panic!()
    };
    let reader = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(root.file("source.txt"))
        .unwrap();
    let outcome = run(&tools, prepared).await;
    assert!(outcome.is_error && outcome.interrupted, "{}", outcome.text);
    assert!(outcome.text.contains("ReplaceFileW"), "{}", outcome.text);
    assert_eq!(
        fs::read_to_string(root.file("source.txt")).unwrap(),
        "before"
    );
    assert_eq!(fs::read_to_string(staged).unwrap(), "after");
    assert!(!backup.exists());
    drop(reader);
}

#[tokio::test]
async fn windows_replacement_races_preserve_displaced_bytes_and_report_unknown() {
    for after_publication in [false, true] {
        let root = TestDir::new();
        let tools = executor();
        let thread = ThreadId::new();
        fs::write(root.file("source.txt"), "before").unwrap();
        assert!(!super::tests::read(&tools, &root, &thread).await.is_error);
        let prepared = tools
            .prepare(super::tests::edit(&root, &thread, "before", "after"))
            .await
            .unwrap();
        let bound: Bound = serde_json::from_value(prepared.specification.clone()).unwrap();
        let Action::Edit { backup, .. } = bound.action else {
            panic!()
        };
        let path = root.file("source.txt");
        let hook: Box<dyn FnOnce()> = Box::new(move || fs::write(path, "racing writer").unwrap());
        if after_publication {
            file::PUBLISHED_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
        } else {
            file::PUBLISH_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
        }
        let outcome = run(&tools, prepared).await;
        assert!(outcome.is_error && outcome.interrupted, "{}", outcome.text);
        assert_eq!(
            fs::read_to_string(backup).unwrap(),
            if after_publication {
                "before"
            } else {
                "racing writer"
            }
        );
        assert_eq!(
            fs::read_to_string(root.file("source.txt")).unwrap(),
            if after_publication {
                "racing writer"
            } else {
                "after"
            }
        );
    }
}
