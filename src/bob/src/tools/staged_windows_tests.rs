use super::*;
use crate::tools::tests::TestDir;
use std::cell::Cell;
use std::ffi::OsStr;
use std::os::windows::fs::OpenOptionsExt;
use std::rc::Rc;
use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};

#[test]
fn stage_pin_blocks_mutation_before_snapshot_reopen() {
    let root = TestDir::new();
    let target = root.file("source.txt");
    let stage = root.file("stage");
    let moved = root.file("moved");
    let replacement = root.file("replacement");
    fs::write(&target, "before").unwrap();
    fs::write(&replacement, "unapproved").unwrap();
    let (directory, source) = filesystem::open_regular(&target).unwrap();
    let ran = Rc::new(Cell::new(false));
    let hook_ran = ran.clone();
    let hook_stage = stage.clone();
    let hook_replacement = replacement.clone();
    STAGE_SNAPSHOT_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            hook_ran.set(true);
            assert!(fs::write(&hook_stage, "unapproved").is_err());
            assert!(fs::rename(&hook_stage, moved).is_err());
            assert!(fs::rename(&hook_replacement, &hook_stage).is_err());
            assert!(fs::remove_file(&hook_stage).is_err());
        }));
    });
    let staged = Staged::create(&directory, OsStr::new("stage"), "after", &source).unwrap();
    assert!(ran.get());
    assert_eq!(
        staged.snapshot.identity,
        file_identity(staged.pinned.as_ref().unwrap()).unwrap()
    );
    assert_eq!(fs::read_to_string(&stage).unwrap(), "after");
    assert_eq!(fs::read_to_string(&replacement).unwrap(), "unapproved");
    staged.verify().unwrap();
    drop(staged);
    assert!(!stage.exists());
    assert_eq!(fs::read_to_string(target).unwrap(), "before");
}

#[test]
fn snapshot_reopen_rejects_replaced_identity_and_changed_intended_bytes() {
    for replace_identity in [false, true] {
        let root = TestDir::new();
        let directory = filesystem::Directory::open(&root.0).unwrap();
        let name = OsStr::new("stage");
        let stage = root.file("stage");
        let mut created = directory.create_file(name).unwrap();
        created.write_all(b"after").unwrap();
        created.sync_all().unwrap();
        let identity = file_identity(&created).unwrap();
        assert!(read_created_stage(&directory, name, &identity, "after").is_ok());
        if replace_identity {
            fs::rename(&stage, root.file("moved")).unwrap();
            fs::write(&stage, "after").unwrap();
        } else {
            fs::write(&stage, "unapproved").unwrap();
        }
        let error = read_created_stage(&directory, name, &identity, "after").unwrap_err();
        assert!(
            error.to_string().contains("identity or contents"),
            "{error:#}"
        );
        assert_eq!(
            fs::read_to_string(&stage).unwrap(),
            if replace_identity {
                "after"
            } else {
                "unapproved"
            }
        );
        if replace_identity {
            assert_eq!(fs::read_to_string(root.file("moved")).unwrap(), "after");
        } else {
            assert_eq!(
                file_identity(&directory.open_file(name).unwrap()).unwrap(),
                identity
            );
        }
    }
}

#[test]
fn cleanup_pin_blocks_replacement_after_verification() {
    let root = TestDir::new();
    let target = root.file("source.txt");
    let stage = root.file("stage");
    let replacement = root.file("replacement");
    let moved = root.file("moved");
    fs::write(&target, "before").unwrap();
    fs::write(&replacement, "unapproved").unwrap();
    let (directory, source) = filesystem::open_regular(&target).unwrap();
    let staged = Staged::create(&directory, OsStr::new("stage"), "after", &source).unwrap();
    let ran = Rc::new(Cell::new(false));
    let hook_ran = ran.clone();
    let hook_stage = stage.clone();
    let hook_replacement = replacement.clone();
    STAGE_DISCARD_HOOK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            hook_ran.set(true);
            assert!(fs::rename(&hook_stage, moved).is_err());
            assert!(fs::rename(hook_replacement, &hook_stage).is_err());
            assert!(fs::write(hook_stage, "unapproved").is_err());
        }));
    });
    drop(staged);
    assert!(ran.get());
    assert!(!stage.exists());
    assert_eq!(fs::read_to_string(replacement).unwrap(), "unapproved");
    assert_eq!(fs::read_to_string(target).unwrap(), "before");
}

#[test]
fn handle_disposition_never_deletes_a_replacement_at_the_old_name() {
    let root = TestDir::new();
    let stage = root.file("stage");
    let moved = root.file("moved");
    let mut created = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .create_new(true)
        .open(&stage)
        .unwrap();
    created.write_all(b"after").unwrap();
    created.sync_all().unwrap();
    fs::rename(&stage, &moved).unwrap();
    fs::write(&stage, "unapproved").unwrap();
    filesystem::discard_pinned_stage(&created).unwrap();
    drop(created);
    assert_eq!(fs::read_to_string(stage).unwrap(), "unapproved");
    assert!(!moved.exists());
}

#[test]
fn cancellation_removes_only_the_pinned_stage() {
    let root = TestDir::new();
    let target = root.file("source.txt");
    let stage = root.file("stage");
    let backup = root.file("backup");
    fs::write(&target, "before").unwrap();
    let approved = read_snapshot(&target).unwrap().1;
    let outcome = guarded_replace(
        &target,
        &stage,
        &backup,
        ("before", &approved),
        "after",
        || Ok(()),
        || true,
    )
    .unwrap();
    assert!(outcome.is_error && !outcome.interrupted, "{}", outcome.text);
    assert_eq!(fs::read_to_string(target).unwrap(), "before");
    assert!(!stage.exists());
    assert!(!backup.exists());
}

#[test]
fn publication_failure_retains_the_stage_and_existing_backup() {
    let root = TestDir::new();
    let target = root.file("source.txt");
    let stage = root.file("stage");
    let backup = root.file("backup");
    fs::write(&target, "before").unwrap();
    fs::write(&backup, "unapproved backup").unwrap();
    let approved = read_snapshot(&target).unwrap().1;
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
    assert_eq!(fs::read_to_string(target).unwrap(), "before");
    assert_eq!(fs::read_to_string(stage).unwrap(), "after");
    assert_eq!(fs::read_to_string(backup).unwrap(), "unapproved backup");
}

#[test]
fn release_window_is_detected_only_after_unapproved_publication() {
    for replace_identity in [false, true] {
        let root = TestDir::new();
        let target = root.file("source.txt");
        let stage = root.file("stage");
        let backup = root.file("backup");
        let moved = root.file("moved");
        fs::write(&target, "before").unwrap();
        let approved = read_snapshot(&target).unwrap().1;
        let hook_stage = stage.clone();
        let hook_moved = moved.clone();
        filesystem::STAGE_RELEASED_HOOK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                if replace_identity {
                    fs::rename(&hook_stage, hook_moved).unwrap();
                }
                fs::write(hook_stage, "unapproved").unwrap();
            }));
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
        assert!(
            outcome
                .text
                .contains("published target changed after replacement"),
            "{}",
            outcome.text
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "unapproved");
        assert_eq!(fs::read_to_string(backup).unwrap(), "before");
        if replace_identity {
            assert_eq!(fs::read_to_string(moved).unwrap(), "after");
        }
    }
}
