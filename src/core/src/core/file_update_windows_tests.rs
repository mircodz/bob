use super::*;
use crate::core::file_update::atomic_replace;
use crate::test_support::TestDir;
use std::os::windows::fs::OpenOptionsExt;
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
};

fn dacl_string(descriptor: *mut std::ffi::c_void) -> String {
    let mut text = std::ptr::null_mut();
    let mut length = 0;
    assert_ne!(
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                &mut length,
            )
        },
        0
    );
    let value =
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, length as usize - 1) });
    unsafe {
        LocalFree(text.cast());
    }
    value
}

fn read_dacl(path: &Path) -> String {
    let path = wide(path);
    let mut descriptor = std::ptr::null_mut();
    let result = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(result, 0, "GetNamedSecurityInfoW failed: {result}");
    let descriptor = SecurityDescriptor(descriptor);
    dacl_string(descriptor.0)
}

fn set_dacl(path: &Path, sddl: &str) {
    let descriptor = descriptor(sddl).unwrap();
    let path = wide(path);
    assert_ne!(
        unsafe {
            SetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor.0,
            )
        },
        0
    );
}

#[test]
fn windows_temporary_file_is_owner_private_before_any_secret_write() {
    let root = TestDir::new();
    let path = root.join("private.tmp");
    let file = create_private(&path).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 0);
    drop(file);
    let expected = descriptor("D:P(A;;FA;;;OW)").unwrap();
    assert_eq!(read_dacl(&path), dacl_string(expected.0));
    assert!(create_private(&path).is_err());
}

#[test]
fn windows_replacement_preserves_custom_dacl_for_config_and_auth() {
    let root = TestDir::new();
    for private in [false, true] {
        let path = root.write(
            if private { "auth.json" } else { "config.toml" },
            "original",
        );
        set_dacl(&path, "D:P(A;;FA;;;OW)(A;;FR;;;SY)");
        let before = read_dacl(&path);
        atomic_replace(&path, b"replacement", private).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        assert_eq!(read_dacl(&path), before);
        assert!(!sibling(&path, ".update-recovery").exists());
    }
    assert_eq!(std::fs::read_dir(&*root).unwrap().count(), 2);
}

#[test]
fn windows_failed_replacement_retains_recovery_files_and_blocks_later_writes() {
    let root = TestDir::new();
    let path = root.write("auth.json", "original");
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    let error = atomic_replace(&path, b"replacement", true).unwrap_err();
    drop(held);
    assert!(error.to_string().contains("recovery"));
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    let marker = sibling(&path, ".update-recovery");
    assert!(marker.exists());
    let staged: Vec<_> = std::fs::read_dir(&*root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
        .collect();
    assert_eq!(staged.len(), 1);
    assert_eq!(std::fs::read(&staged[0]).unwrap(), b"replacement");
    let count = std::fs::read_dir(&*root).unwrap().count();
    assert!(atomic_replace(&path, b"must-not-overwrite", true).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_eq!(std::fs::read_dir(&*root).unwrap().count(), count);
}

#[test]
fn windows_recovery_marker_prevents_recreating_missing_target_and_losing_backup() {
    let root = TestDir::new();
    let path = root.join("config.toml");
    let backup = root.write("unique.backup", "only original copy");
    root.write(
        "config.toml.update-recovery",
        format!("Backup: {}\n", backup.display()),
    );
    assert!(crate::core::config::set_default_provider_if_unset_at(&path, "openai").is_err());
    assert!(!path.exists());
    assert_eq!(std::fs::read(backup).unwrap(), b"only original copy");
}
