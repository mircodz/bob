use super::{sibling, TemporaryFile};
use std::fs::File;
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::{LocalFree, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, MoveFileExW, ReplaceFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL,
    MOVEFILE_WRITE_THROUGH,
};

struct SecurityDescriptor(*mut std::ffi::c_void);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn descriptor(sddl: &str) -> io::Result<SecurityDescriptor> {
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(SecurityDescriptor(descriptor))
}

pub(super) fn create_private(path: &Path) -> io::Result<File> {
    let descriptor = descriptor("D:P(A;;FA;;;OW)")?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path = wide(path);
    // A protected owner-rights DACL applies at creation, before any credential bytes exist.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_WRITE,
            0,
            &attributes,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}

pub(super) fn ensure_no_recovery(path: &Path) -> io::Result<()> {
    let marker = sibling(path, ".update-recovery");
    match std::fs::symlink_metadata(&marker) {
        Ok(_) => Err(io::Error::other(format!("file update requires recovery: inspect {} before changing {}; preserve all listed files and remove the marker only after restoring the intended file", marker.display(), path.display()))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn recovery_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::other(format!("file replacement or cleanup failed ({error}); recovery files retained. Inspect {} and its listed target, backup, and staged replacement; restore the intended file before removing this marker", sibling(path, ".update-recovery").display()))
}

pub(super) fn replace(temporary: &mut TemporaryFile, path: &Path) -> io::Result<()> {
    ensure_no_recovery(path)?;
    let exists = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => true,
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "replacement target is not a regular file",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error),
    };
    let from = wide(&temporary.0);
    let to = wide(path);
    if !exists {
        if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    let backup = sibling(&temporary.0, ".backup");
    match std::fs::symlink_metadata(&backup) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "recovery backup already exists",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let marker = sibling(path, ".update-recovery");
    let mut record = create_private(&marker)?;
    temporary.1 = true;
    writeln!(
        record,
        "Target: {}\nBackup: {}\nStaged replacement: {}",
        path.display(),
        backup.display(),
        temporary.0.display()
    )
    .and_then(|_| record.sync_all())
    .map_err(|error| recovery_error(path, error))?;
    drop(record);
    let backup_name = wide(&backup);
    // No ignore flags: failed ACL/metadata preservation must not publish weaker security.
    if unsafe {
        ReplaceFileW(
            to.as_ptr(),
            from.as_ptr(),
            backup_name.as_ptr(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(recovery_error(path, io::Error::last_os_error()));
    }
    std::fs::remove_file(&backup).map_err(|error| recovery_error(path, error))?;
    std::fs::remove_file(&marker).map_err(|error| recovery_error(path, error))?;
    temporary.1 = false;
    Ok(())
}

#[cfg(test)]
#[path = "file_update_windows_tests.rs"]
mod tests;
