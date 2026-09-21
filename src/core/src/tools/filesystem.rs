//! Shared bounded reads and staged, snapshot-checked file replacement.

use crate::tools::registry::{ToolContext, ToolError};
use std::collections::{HashMap, HashSet};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;

pub fn bounded_output(mut output: String) -> String {
    if output.len() > MAX_OUTPUT_BYTES {
        let mut end = MAX_OUTPUT_BYTES;
        while !output.is_char_boundary(end) {
            end -= 1;
        }
        output.truncate(end);
        output.push_str("\n... [output truncated]");
    }
    output
}

pub fn required_string<'a>(input: &'a serde_json::Value, name: &str) -> Result<&'a str, ToolError> {
    input
        .get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolError::invalid_input(format!("{name} must be a string")))
}

pub fn required_path(input: &serde_json::Value) -> Result<&str, ToolError> {
    let path = required_string(input, "path")?;
    if path.is_empty() {
        return Err(ToolError::invalid_input("path must not be empty"));
    }
    Ok(path)
}

pub fn optional_bool(input: &serde_json::Value, name: &str) -> Result<bool, ToolError> {
    match input.get(name) {
        None => Ok(false),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| ToolError::invalid_input(format!("{name} must be a boolean"))),
    }
}

pub fn optional_usize(
    input: &serde_json::Value,
    name: &str,
    default: usize,
) -> Result<usize, ToolError> {
    match input.get(name) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .or_else(|| {
                let n = v.as_f64()?;
                // usize::MAX rounds up on 64-bit targets; use the exclusive power-of-two bound.
                (n >= 0.0 && n < 2_f64.powi(usize::BITS as i32) && n.fract() == 0.0)
                    .then_some(n as usize)
            })
            .ok_or_else(|| {
                ToolError::invalid_input(format!("{name} must be a nonnegative integer"))
            }),
    }
}

pub fn workspace_path(cwd: &str, path: &Path) -> Result<PathBuf, ToolError> {
    let root = std::fs::canonicalize(cwd)?;
    crate::core::policies::resolve_workspace_path(&root, path).map_err(ToolError::from)
}

fn open_regular(path: &Path) -> Result<File, ToolError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_NOCTTY);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if file.metadata()?.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(ToolError::invalid_input(
                "reparse-point files are not supported",
            ));
        }
    }
    if !file.metadata()?.is_file() {
        return Err(ToolError::invalid_input("only regular files are supported"));
    }
    Ok(file)
}

pub fn read_regular(path: &Path) -> Result<String, ToolError> {
    read_regular_limited(path, MAX_FILE_BYTES)
}

pub fn read_regular_limited(path: &Path, limit: usize) -> Result<String, ToolError> {
    let limit = limit.min(MAX_FILE_BYTES);
    let file = open_regular(path)?;
    if file.metadata()?.len() > limit as u64 {
        return Err(ToolError::invalid_input(format!(
            "file exceeds the {limit}-byte safe-read limit"
        )));
    }
    let mut bytes = Vec::new();
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(ToolError::invalid_input(format!(
            "file exceeds the {limit}-byte safe-read limit"
        )));
    }
    String::from_utf8(bytes).map_err(|_| ToolError::invalid_input("file is not UTF-8 text"))
}

fn same_identity(a: &Metadata, b: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev()
            && a.ino() == b.ino()
            && a.mode() == b.mode()
            && a.uid() == b.uid()
            && a.gid() == b.gid()
    }
    #[cfg(not(unix))]
    {
        a.is_file() == b.is_file()
            && a.created().ok() == b.created().ok()
            && a.permissions().readonly() == b.permissions().readonly()
    }
}

fn same_snapshot(a: &Metadata, b: &Metadata) -> bool {
    if !same_identity(a, b) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.ctime() == b.ctime() && a.ctime_nsec() == b.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        a.modified().ok() == b.modified().ok()
    }
}

#[cfg(target_os = "linux")]
fn extended_attributes(file: &File) -> Result<HashMap<Vec<u8>, Vec<u8>>, ToolError> {
    // The kernel exposes only caller-visible namespaces; privileged hidden attributes are not covered.
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let size = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
    if size < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut names = vec![0u8; size as usize];
    let size = unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), names.len()) };
    if size < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if size as usize > names.len() {
        return Err(ToolError::failed(
            "extended attributes changed while copying",
        ));
    }
    names.truncate(size as usize);
    let mut attributes = HashMap::new();
    for name in names.split_inclusive(|byte| *byte == 0) {
        let name = std::ffi::CStr::from_bytes_with_nul(name)
            .map_err(|_| ToolError::failed("invalid extended attribute name"))?;
        let size = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut value = vec![0u8; size as usize];
        let size =
            unsafe { libc::fgetxattr(fd, name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
        if size < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if size as usize > value.len() {
            return Err(ToolError::failed(
                "extended attribute changed while copying",
            ));
        }
        value.truncate(size as usize);
        attributes.insert(name.to_bytes_with_nul().to_vec(), value);
    }
    Ok(attributes)
}

fn copy_metadata(source: &File, destination: &File) -> Result<(), ToolError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let before = source.metadata()?;
        let current = destination.metadata()?;
        if (current.uid(), current.gid()) != (before.uid(), before.gid())
            && unsafe { libc::fchown(destination.as_raw_fd(), before.uid(), before.gid()) } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        destination.set_permissions(before.permissions())?;
        #[cfg(target_os = "macos")]
        {
            // Metadata only: copying DATA here would overwrite the prepared contents.
            if unsafe {
                libc::fcopyfile(
                    source.as_raw_fd(),
                    destination.as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_METADATA,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(target_os = "linux")]
        {
            let attributes = extended_attributes(source)?;
            for name in extended_attributes(destination)?.keys() {
                if !attributes.contains_key(name)
                    && unsafe { libc::fremovexattr(destination.as_raw_fd(), name.as_ptr().cast()) }
                        != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            for (name, value) in &attributes {
                if unsafe {
                    libc::fsetxattr(
                        destination.as_raw_fd(),
                        name.as_ptr().cast(),
                        value.as_ptr().cast(),
                        value.len(),
                        0,
                    )
                } != 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
            if extended_attributes(destination)? != attributes {
                return Err(ToolError::failed("extended attributes were not preserved"));
            }
        }
        let after = destination.metadata()?;
        if (before.mode(), before.uid(), before.gid()) != (after.mode(), after.uid(), after.gid())
            || !same_snapshot(&before, &source.metadata()?)
        {
            return Err(ToolError::failed(
                "file security metadata was not preserved",
            ));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        // ReplaceFileW merges security and streams at commit; do not publish by rename.
        destination.set_permissions(source.metadata()?.permissions())?;
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (source, destination);
        Err(ToolError::failed(
            "safe replacement is unavailable on this platform: cannot preserve ACLs and extended attributes",
        ))
    }
}

fn path_lock(path: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>> = OnceLock::new();
    let mut locks = LOCKS.get_or_init(Mutex::default).lock().unwrap();
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

pub struct WriteRequest {
    pub path: PathBuf,
    pub resolved: PathBuf,
    pub content: String,
    /// None requires a new file; Some requires these exact existing bytes.
    pub expected: Option<String>,
}

struct StagedFile {
    requested: PathBuf,
    target: PathBuf,
    parent_metadata: Metadata,
    metadata: Option<Metadata>,
    expected: Option<String>,
    content: String,
    staged: PathBuf,
    staged_metadata: Option<Metadata>,
    backup: Option<PathBuf>,
    backup_metadata: Option<Metadata>,
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.staged);
        if let Some(backup) = &self.backup {
            let _ = std::fs::remove_file(backup);
        }
    }
}

pub struct StagedBatch {
    cwd: String,
    files: Vec<StagedFile>,
    _guards: Vec<OwnedMutexGuard<()>>,
}

fn stage(target: &Path, content: &str, source: Option<&File>) -> Result<PathBuf, ToolError> {
    let parent = target
        .parent()
        .ok_or_else(|| ToolError::invalid_input("file needs a parent directory"))?;
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random).map_err(|e| ToolError::failed(e.to_string()))?;
    let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let path = parent.join(format!(".bob-stage-{name}"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    let result = (|| -> Result<(), ToolError> {
        file.write_all(content.as_bytes())?;
        if let Some(source) = source {
            copy_metadata(source, &file).map_err(|error| {
                ToolError::failed(format!(
                    "cannot preserve file metadata before replacement: {error}"
                ))
            })?;
        }
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

impl StagedFile {
    fn validate(&self, cwd: &str) -> Result<(), ToolError> {
        if workspace_path(cwd, &self.requested)? != self.target
            || !same_identity(
                &self.parent_metadata,
                &std::fs::metadata(self.target.parent().unwrap())?,
            )
        {
            return Err(ToolError::failed(
                "file path changed while preparing the write",
            ));
        }
        if let Some(metadata) = &self.staged_metadata {
            if !same_snapshot(metadata, &std::fs::symlink_metadata(&self.staged)?)
                || read_regular(&self.staged)? != self.content
            {
                return Err(ToolError::failed(
                    "staged content changed before replacement",
                ));
            }
        }
        if let Some(backup) = &self.backup {
            if !same_snapshot(
                self.backup_metadata.as_ref().unwrap(),
                &std::fs::symlink_metadata(backup)?,
            ) || read_regular(backup)? != *self.expected.as_ref().unwrap()
            {
                return Err(ToolError::failed("backup changed before replacement"));
            }
        }
        match (&self.expected, &self.metadata) {
            (Some(expected), Some(metadata)) => {
                let current = std::fs::symlink_metadata(&self.target)?;
                if !same_snapshot(metadata, &current) || read_regular(&self.target)? != *expected {
                    return Err(ToolError::failed(format!(
                        "{} changed while preparing the write; read it again",
                        self.target.display()
                    )));
                }
            }
            (None, None) => match std::fs::symlink_metadata(&self.target) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => {
                    return Err(ToolError::failed(
                        "new file appeared while preparing the write",
                    ))
                }
            },
            _ => unreachable!(),
        }
        Ok(())
    }

    #[cfg(not(windows))]
    fn replace_existing(&mut self, _cwd: &str) -> Result<(), ToolError> {
        std::fs::rename(&self.staged, &self.target).map_err(ToolError::from)
    }

    #[cfg(windows)]
    fn replace_existing(&mut self, cwd: &str) -> Result<(), ToolError> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
        let wide =
            |path: &Path| -> Vec<u16> { path.as_os_str().encode_wide().chain(Some(0)).collect() };
        let result = (|| -> Result<(), ToolError> {
            let backup = self.backup.as_ref().unwrap();
            let target_name = wide(&self.target);
            let staged_name = wide(&self.staged);
            let backup_name = wide(backup);
            std::fs::remove_file(backup)?;
            if unsafe {
                ReplaceFileW(
                    target_name.as_ptr(),
                    staged_name.as_ptr(),
                    backup_name.as_ptr(),
                    0,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            self.staged_metadata = Some(std::fs::symlink_metadata(&self.target)?);
            self.backup_metadata = Some(std::fs::symlink_metadata(backup)?);
            if read_regular(&self.target)? != self.content
                || read_regular(backup)? != *self.expected.as_ref().unwrap()
            {
                return Err(ToolError::failed(
                    "content changed during native replacement",
                ));
            }
            Ok(())
        })();
        result.map_err(|error| {
            // ReplaceFileW failures can leave partially moved files; never delete recovery paths.
            let paths_trusted = workspace_path(cwd, &self.requested)
                .is_ok_and(|path| path == self.target)
                && std::fs::metadata(self.target.parent().unwrap())
                    .is_ok_and(|metadata| same_identity(&self.parent_metadata, &metadata));
            let state = |path: &Path, expected: &str| {
                if !paths_trusted {
                    return "path mapping changed; not inspected".to_string();
                }
                match read_regular(path) {
                    Ok(content) if content == expected => "expected bytes present".to_string(),
                    Ok(_) => "different bytes present".to_string(),
                    Err(error) => format!("unavailable: {error}"),
                }
            };
            let target_state = state(&self.target, self.expected.as_ref().unwrap());
            let staged = std::mem::take(&mut self.staged);
            let backup = self.backup.take().unwrap();
            let backup_state = state(&backup, self.expected.as_ref().unwrap());
            ToolError::failed(format!(
                "native replacement failed: {error}; target {}: {target_state}; recovery backup retained if present at {}: {backup_state}; staged recovery file retained if present at {}",
                self.target.display(), backup.display(), staged.display()
            ))
        })
    }

    fn rollback(&mut self, cwd: &str) -> Result<(), ToolError> {
        let restored = (|| -> Result<(), ToolError> {
            if workspace_path(cwd, &self.requested)? != self.target
                || !same_identity(
                    &self.parent_metadata,
                    &std::fs::metadata(self.target.parent().unwrap())?,
                )
                || !same_identity(
                    self.staged_metadata.as_ref().unwrap(),
                    &std::fs::symlink_metadata(&self.target)?,
                )
                || read_regular(&self.target)? != self.content
            {
                return Err(ToolError::failed("intervening change; rollback refused"));
            }
            if let Some(backup) = &self.backup {
                if !same_snapshot(
                    self.backup_metadata.as_ref().unwrap(),
                    &std::fs::symlink_metadata(backup)?,
                ) || read_regular(backup)? != *self.expected.as_ref().unwrap()
                {
                    return Err(ToolError::failed("backup changed; rollback refused"));
                }
                std::fs::rename(backup, &self.target)?;
            } else {
                std::fs::remove_file(&self.target)?;
            }
            Ok(())
        })();
        restored.map_err(|error| {
            // Taking the path prevents Drop from deleting the only recovery copy.
            match self.backup.take() {
                Some(backup) => ToolError::failed(format!(
                    "{error}; recovery backup retained at {}",
                    backup.display()
                )),
                None => error,
            }
        })
    }
}

pub async fn prepare_batch(
    ctx: &ToolContext,
    requests: Vec<WriteRequest>,
) -> Result<StagedBatch, ToolError> {
    let mut targets = Vec::new();
    let mut unique = HashSet::new();
    for request in &requests {
        if request.content.len() > MAX_FILE_BYTES {
            return Err(ToolError::invalid_input("content exceeds 16 MiB"));
        }
        let target = workspace_path(&ctx.cwd, &request.path)?;
        if target != request.resolved {
            return Err(ToolError::failed(
                "file path changed since its contents were read or authorized",
            ));
        }
        if !unique.insert(target.clone()) {
            return Err(ToolError::invalid_input(
                "multiple writes resolve to the same file",
            ));
        }
        targets.push(target);
    }
    let mut ordered = targets.clone();
    ordered.sort();
    let mut guards = Vec::new();
    for target in ordered {
        guards.push(path_lock(&target).lock_owned().await);
    }
    let mut batch = StagedBatch {
        cwd: ctx.cwd.clone(),
        files: Vec::new(),
        _guards: guards,
    };
    for (request, target) in requests.into_iter().zip(targets) {
        if workspace_path(&ctx.cwd, &request.path)? != target {
            return Err(ToolError::failed(
                "file path changed while waiting to write",
            ));
        }
        let parent = target
            .parent()
            .ok_or_else(|| ToolError::invalid_input("file needs a parent directory"))?;
        if request.expected.is_none() && !parent.exists() {
            std::fs::create_dir_all(parent)?;
            if workspace_path(&ctx.cwd, &request.path)? != target {
                return Err(ToolError::failed(
                    "file path changed while creating its parent directory",
                ));
            }
        }
        let parent_metadata = std::fs::metadata(parent)?;
        let metadata = match std::fs::symlink_metadata(&target) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err(ToolError::invalid_input(
                        "only regular file targets are supported",
                    ));
                }
                Some(metadata)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if metadata.is_some() != request.expected.is_some() {
            return Err(ToolError::failed(
                "file existence changed; read it again before writing",
            ));
        }
        let mut file = StagedFile {
            requested: request.path,
            target,
            parent_metadata,
            metadata,
            expected: request.expected,
            content: request.content,
            staged: PathBuf::new(),
            staged_metadata: None,
            backup: None,
            backup_metadata: None,
        };
        file.validate(&batch.cwd)?;
        let source = if let Some(metadata) = &file.metadata {
            let source = open_regular(&file.target)?;
            if !same_snapshot(metadata, &source.metadata()?) {
                return Err(ToolError::failed(
                    "file changed before copying its metadata",
                ));
            }
            Some(source)
        } else {
            None
        };
        file.staged = stage(&file.target, &file.content, source.as_ref())?;
        file.staged_metadata = Some(std::fs::symlink_metadata(&file.staged)?);
        if let Some(before) = &file.expected {
            file.backup = Some(stage(&file.target, before, source.as_ref())?);
            file.backup_metadata = Some(std::fs::symlink_metadata(file.backup.as_ref().unwrap())?);
        }
        batch.files.push(file);
    }
    for file in &batch.files {
        file.validate(&batch.cwd)?;
    }
    Ok(batch)
}

impl StagedBatch {
    pub fn commit(mut self) -> Result<Vec<PathBuf>, ToolError> {
        for file in &self.files {
            file.validate(&self.cwd)?;
        }
        let mut written = Vec::new();
        for index in 0..self.files.len() {
            let file = &mut self.files[index];
            let result = file.validate(&self.cwd).and_then(|_| {
                if file.expected.is_none() {
                    // A hard link publishes the staged inode without clobbering a racing creator.
                    std::fs::hard_link(&file.staged, &file.target)?;
                    Ok(())
                } else {
                    file.replace_existing(&self.cwd)
                }
            });
            if let Err(error) = result {
                let mut rollback = Vec::new();
                for previous in self.files[..written.len()].iter_mut().rev() {
                    let restored = previous.rollback(&self.cwd);
                    rollback.push(format!(
                        "{}: {}",
                        previous.target.display(),
                        match restored {
                            Ok(()) => "restored".to_string(),
                            Err(e) => e.to_string(),
                        }
                    ));
                }
                return Err(ToolError::failed(format!(
                    "{error}; written targets: {written:?}; rollback: {}",
                    rollback.join("; ")
                )));
            }
            written.push(file.target.clone());
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::files::FileFixture as Fixture;

    #[derive(Clone, Copy)]
    enum Outcome {
        Commit,
        Rollback,
        RetainedBackup,
        RemovedTarget,
    }

    fn finish_batch(
        mut batch: StagedBatch,
        outcome: Outcome,
        inspect_replacement_backup: impl FnOnce(&Path),
    ) -> PathBuf {
        let path = batch.files[0].target.clone();
        let original = batch.files[0].expected.clone().unwrap();
        let replacement = batch.files[0].content.clone();
        let backup = batch.files[0].backup.clone().unwrap();
        let cwd = batch.cwd.clone();
        let recovered = match outcome {
            Outcome::Commit => {
                batch.commit().unwrap();
                assert_eq!(read_regular(&path).unwrap(), replacement);
                path
            }
            Outcome::Rollback | Outcome::RetainedBackup | Outcome::RemovedTarget => {
                let file = &mut batch.files[0];
                file.replace_existing(&cwd).unwrap();
                inspect_replacement_backup(&backup);
                match outcome {
                    Outcome::Rollback => {
                        file.rollback(&cwd).unwrap();
                        drop(batch);
                        assert_eq!(read_regular(&path).unwrap(), original);
                        path
                    }
                    Outcome::RetainedBackup | Outcome::RemovedTarget => {
                        if matches!(outcome, Outcome::RemovedTarget) {
                            std::fs::remove_file(&path).unwrap();
                        } else {
                            std::fs::write(&path, "external change").unwrap();
                        }
                        let error = file.rollback(&cwd).unwrap_err();
                        assert!(error.message.contains("recovery backup retained at"));
                        assert!(error.message.contains(backup.to_str().unwrap()));
                        drop(batch);
                        assert_eq!(read_regular(&backup).unwrap(), original);
                        if matches!(outcome, Outcome::RemovedTarget) {
                            assert!(!path.exists());
                        } else {
                            assert_eq!(read_regular(&path).unwrap(), "external change");
                        }
                        backup
                    }
                    Outcome::Commit => unreachable!(),
                }
            }
        };
        assert_eq!(
            std::fs::read_dir(&cwd).unwrap().count(),
            if matches!(outcome, Outcome::RetainedBackup) {
                2
            } else {
                1
            }
        );
        recovered
    }

    fn request(path: &Path, before: &str, after: &str) -> WriteRequest {
        WriteRequest {
            path: path.to_path_buf(),
            resolved: std::fs::canonicalize(path).unwrap(),
            content: after.to_string(),
            expected: Some(before.to_string()),
        }
    }

    #[tokio::test]
    async fn batch_preflight_leaves_all_bytes_unchanged() {
        let fixture = Fixture::new();
        let first = fixture.write("first", "one");
        let second = fixture.write("second", "two");
        assert!(prepare_batch(
            &fixture.ctx,
            vec![
                request(&first, "one", "changed"),
                request(&second, "stale", "changed")
            ]
        )
        .await
        .is_err());
        assert_eq!(read_regular(&first).unwrap(), "one");
        assert_eq!(read_regular(&second).unwrap(), "two");
        assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn precommit_change_is_rejected_without_writing() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "old");
        let batch = prepare_batch(&fixture.ctx, vec![request(&path, "old", "new")])
            .await
            .unwrap();
        std::fs::write(&path, "external").unwrap();
        assert!(batch.commit().is_err());
        assert_eq!(read_regular(&path).unwrap(), "external");
    }

    #[tokio::test]
    async fn concurrent_batches_do_not_lose_a_write() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "old");
        let ctx = fixture.ctx.clone();
        let left = path.clone();
        let right = path.clone();
        let (first, second) = tokio::join!(
            async {
                prepare_batch(&ctx, vec![request(&left, "old", "first")])
                    .await?
                    .commit()
            },
            async {
                prepare_batch(&ctx, vec![request(&right, "old", "second")])
                    .await?
                    .commit()
            },
        );
        assert_ne!(first.is_ok(), second.is_ok());
        assert!(matches!(
            read_regular(&path).unwrap().as_str(),
            "first" | "second"
        ));
    }

    #[tokio::test]
    async fn existing_content_is_not_replaced_by_a_new_file_request() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "existing");
        assert!(prepare_batch(
            &fixture.ctx,
            vec![WriteRequest {
                path: path.clone(),
                resolved: path.clone(),
                expected: None,
                content: String::new(),
            }]
        )
        .await
        .is_err());
        assert_eq!(read_regular(&path).unwrap(), "existing");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn atomic_replacement_preserves_mode_and_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let fixture = Fixture::new();
        let path = fixture.write("target", "old");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        let link = fixture.dir.join("link");
        symlink("target", &link).unwrap();
        prepare_batch(&fixture.ctx, vec![request(&link, "old", "new")])
            .await
            .unwrap()
            .commit()
            .unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(read_regular(&path).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_swap_after_preflight_is_rejected() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let first = fixture.write("first", "old");
        let second = fixture.write("second", "old");
        let link = fixture.dir.join("link");
        symlink("first", &link).unwrap();
        let batch = prepare_batch(&fixture.ctx, vec![request(&link, "old", "new")])
            .await
            .unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink("second", &link).unwrap();
        assert!(batch.commit().is_err());
        assert_eq!(read_regular(&first).unwrap(), "old");
        assert_eq!(read_regular(&second).unwrap(), "old");
    }

    #[cfg(unix)]
    #[test]
    fn fifo_and_directories_are_rejected_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let fixture = Fixture::new();
        let fifo = fixture.dir.join("fifo");
        let path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        assert!(read_regular(&fifo).is_err());
        assert!(read_regular(&fixture.dir).is_err());
    }

    #[tokio::test]
    async fn failed_rollback_retains_backup_after_drop_and_reports_recovery_path() {
        for outcome in [Outcome::RetainedBackup, Outcome::RemovedTarget] {
            let fixture = Fixture::new();
            let path = fixture.write("file", "original");
            let batch = prepare_batch(
                &fixture.ctx,
                vec![request(&path, "original", "replacement")],
            )
            .await
            .unwrap();
            finish_batch(batch, outcome, |_| {});
        }
    }

    #[tokio::test]
    async fn successful_commit_and_rollback_clean_up_staged_files_and_backups() {
        for outcome in [Outcome::Commit, Outcome::Rollback] {
            let fixture = Fixture::new();
            let path = fixture.write("file", "original");
            let batch = prepare_batch(
                &fixture.ctx,
                vec![request(&path, "original", "replacement")],
            )
            .await
            .unwrap();
            finish_batch(batch, outcome, |_| {});
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn set_fixture_xattr(path: &Path, value: &[u8]) {
        use std::os::fd::AsRawFd;
        let file = open_regular(path).unwrap();
        #[cfg(target_os = "macos")]
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                c"user.bob-fixture".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        #[cfg(target_os = "linux")]
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                c"user.bob-fixture".as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn fixture_xattr(path: &Path) -> Vec<u8> {
        use std::os::fd::AsRawFd;
        let file = open_regular(path).unwrap();
        let mut value = vec![0u8; 256];
        #[cfg(target_os = "macos")]
        let size = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                c"user.bob-fixture".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        #[cfg(target_os = "linux")]
        let size = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                c"user.bob-fixture".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        };
        assert!(size >= 0, "{}", std::io::Error::last_os_error());
        value.truncate(size as usize);
        value
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn replacement_and_recovery_preserve_xattrs_and_executable_bits() {
        use std::os::unix::fs::PermissionsExt;
        for outcome in [Outcome::Commit, Outcome::Rollback, Outcome::RetainedBackup] {
            let fixture = Fixture::new();
            let path = fixture.write("file", "original");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
            set_fixture_xattr(&path, b"original attribute");
            let batch = prepare_batch(
                &fixture.ctx,
                vec![request(&path, "original", "replacement")],
            )
            .await
            .unwrap();
            let backup = batch.files[0].backup.clone().unwrap();
            for staged in [&batch.files[0].staged, &backup] {
                assert_eq!(fixture_xattr(staged), b"original attribute");
                assert_eq!(
                    std::fs::metadata(staged).unwrap().permissions().mode() & 0o7777,
                    0o751
                );
            }
            assert_eq!(read_regular(&batch.files[0].staged).unwrap(), "replacement");
            let recovered = finish_batch(batch, outcome, |_| {});
            assert_eq!(fixture_xattr(&recovered), b"original attribute");
            assert_eq!(
                std::fs::metadata(&recovered).unwrap().permissions().mode() & 0o7777,
                0o751
            );
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn metadata_only_changes_to_source_stage_or_backup_reject_commit() {
        for changed in ["source", "stage", "backup"] {
            let fixture = Fixture::new();
            let path = fixture.write("file", "old");
            set_fixture_xattr(&path, b"before");
            let batch = prepare_batch(&fixture.ctx, vec![request(&path, "old", "new")])
                .await
                .unwrap();
            let changed_path = match changed {
                "source" => &path,
                "stage" => &batch.files[0].staged,
                _ => batch.files[0].backup.as_ref().unwrap(),
            };
            set_fixture_xattr(changed_path, b"after");
            assert!(batch.commit().is_err());
            assert_eq!(read_regular(&path).unwrap(), "old");
            assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 1);
        }
    }

    #[cfg(target_os = "macos")]
    fn fixture_acl(path: &Path) -> String {
        let output = std::process::Command::new("ls")
            .arg("-le")
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_acl_survives_replacement_and_retained_recovery_backup() {
        for outcome in [Outcome::Commit, Outcome::RetainedBackup] {
            let fixture = Fixture::new();
            let path = fixture.write("file", "old");
            let output = std::process::Command::new("chmod")
                .args(["+a", "everyone deny execute"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(output.status.success(), "{:?}", output.stderr);
            let acl = fixture_acl(&path);
            assert!(acl.contains("everyone deny execute"), "{acl}");
            let batch = prepare_batch(&fixture.ctx, vec![request(&path, "old", "new")])
                .await
                .unwrap();
            let backup = batch.files[0].backup.clone().unwrap();
            assert_eq!(fixture_acl(&batch.files[0].staged), acl);
            assert_eq!(fixture_acl(&backup), acl);
            let recovered = finish_batch(batch, outcome, |_| {});
            assert_eq!(fixture_acl(&recovered), acl);
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_posix_acl_is_preserved_when_setfacl_is_available() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "old");
        let output = match std::process::Command::new("setfacl")
            .args(["-m", "u:65534:r--"])
            .arg(&path)
            .output()
        {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("ACL fixture skipped: setfacl is not installed");
                return;
            }
            Err(error) => panic!("cannot create ACL fixture: {error}"),
        };
        assert!(output.status.success(), "{:?}", output.stderr);
        let attributes = extended_attributes(&open_regular(&path).unwrap()).unwrap();
        assert!(attributes.contains_key(c"system.posix_acl_access".to_bytes_with_nul()));
        let batch = prepare_batch(&fixture.ctx, vec![request(&path, "old", "new")])
            .await
            .unwrap();
        for staged in [
            &batch.files[0].staged,
            batch.files[0].backup.as_ref().unwrap(),
        ] {
            assert_eq!(
                extended_attributes(&open_regular(staged).unwrap()).unwrap(),
                attributes
            );
        }
        batch.commit().unwrap();
        assert_eq!(
            extended_attributes(&open_regular(&path).unwrap()).unwrap(),
            attributes
        );
        assert_eq!(read_regular(&path).unwrap(), "new");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_native_replacement_preserves_streams_and_recovery_metadata() {
        for outcome in [Outcome::Commit, Outcome::Rollback, Outcome::RetainedBackup] {
            let fixture = Fixture::new();
            let path = fixture.write("file", "old");
            let stream = |path: &Path| {
                let mut name = path.as_os_str().to_os_string();
                name.push(":bob-fixture");
                PathBuf::from(name)
            };
            std::fs::write(stream(&path), b"original stream").unwrap();
            let batch = prepare_batch(&fixture.ctx, vec![request(&path, "old", "new")])
                .await
                .unwrap();
            let recovered = finish_batch(batch, outcome, |backup| {
                assert_eq!(std::fs::read(stream(backup)).unwrap(), b"original stream");
            });
            assert_eq!(
                std::fs::read(stream(&recovered)).unwrap(),
                b"original stream"
            );
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_native_failure_reports_state_and_retains_recovery_files() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        let fixture = Fixture::new();
        let path = fixture.write("file", "old");
        let mut batch = prepare_batch(&fixture.ctx, vec![request(&path, "old", "new")])
            .await
            .unwrap();
        let staged = batch.files[0].staged.clone();
        let guard = File::options()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&path)
            .unwrap();
        let error = batch.files[0]
            .replace_existing(&fixture.ctx.cwd)
            .unwrap_err();
        assert!(error.message.contains("native replacement failed"));
        assert!(error.message.contains("expected bytes present"));
        assert!(error.message.contains(staged.to_str().unwrap()));
        drop(batch);
        assert_eq!(read_regular(&path).unwrap(), "old");
        assert_eq!(read_regular(&staged).unwrap(), "new");
        drop(guard);
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    #[tokio::test]
    async fn unsupported_metadata_preservation_rejects_without_replacement() {
        let fixture = Fixture::new();
        let path = fixture.write("file", "old");
        let error = prepare_batch(&fixture.ctx, vec![request(&path, "old", "new")])
            .await
            .err()
            .unwrap();
        assert!(error
            .message
            .contains("cannot preserve ACLs and extended attributes"));
        assert_eq!(read_regular(&path).unwrap(), "old");
        assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 1);
    }

    #[test]
    fn bounded_output_preserves_exact_limit_and_never_splits_a_unicode_scalar() {
        for content in [String::new(), "x".repeat(MAX_OUTPUT_BYTES)] {
            assert_eq!(bounded_output(content.clone()), content);
        }
        for scalar in ['é', '終', '🙂'] {
            let prefix = "x".repeat(MAX_OUTPUT_BYTES - 1);
            let output = bounded_output(format!("{prefix}{scalar}tail"));
            assert_eq!(output, format!("{prefix}\n... [output truncated]"));
        }
    }

    #[test]
    fn safe_reads_enforce_byte_limits_and_utf8_without_truncating_content() {
        use crate::tools::registry::ToolErrorKind;
        let fixture = Fixture::new();
        let path = fixture.write("file", "é🙂");
        assert_eq!(read_regular_limited(&path, 6).unwrap(), "é🙂");
        let error = read_regular_limited(&path, 5).unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
        assert_eq!(error.message, "file exceeds the 5-byte safe-read limit");
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        let error = read_regular_limited(&path, 2).unwrap_err();
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
        assert_eq!(error.message, "file is not UTF-8 text");
        std::fs::write(&path, "").unwrap();
        assert_eq!(read_regular_limited(&path, 0).unwrap(), "");
    }

    #[tokio::test]
    async fn duplicate_resolved_targets_are_rejected_before_staging() {
        use crate::tools::registry::ToolErrorKind;
        let fixture = Fixture::new();
        let path = fixture.write("file", "original");
        let mut alias = request(&path, "original", "second");
        alias.path = fixture.dir.join(".").join("file");
        let error = prepare_batch(
            &fixture.ctx,
            vec![request(&path, "original", "first"), alias],
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind, ToolErrorKind::InvalidInput);
        assert_eq!(error.message, "multiple writes resolve to the same file");
        assert_eq!(read_regular(&path).unwrap(), "original");
        assert_eq!(std::fs::read_dir(&fixture.dir).unwrap().count(), 1);
    }

    #[test]
    fn optional_usize_accepts_integral_numbers_without_saturating_overflow() {
        use serde_json::json;
        assert_eq!(optional_usize(&json!({}), "n", 7).unwrap(), 7);
        for value in [
            json!(0),
            json!(0.0),
            json!(-0.0),
            json!(1),
            json!(1.0),
            json!(1e3),
            json!(usize::MAX),
        ] {
            let expected = value
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or_else(|| value.as_f64().unwrap() as usize);
            assert_eq!(
                optional_usize(&json!({"n":value}), "n", 7).unwrap(),
                expected
            );
        }
        let exclusive_max = 2_f64.powi(usize::BITS as i32);
        let largest_float = f64::from_bits(exclusive_max.to_bits() - 1).floor();
        assert_eq!(
            optional_usize(&json!({"n":largest_float}), "n", 0).unwrap(),
            largest_float as usize
        );
        for value in [
            json!(-1),
            json!(-1.0),
            json!(0.5),
            json!(1.5),
            json!(exclusive_max),
            json!(1e100),
            json!("1"),
            json!(true),
            json!(null),
        ] {
            assert!(optional_usize(&json!({"n":value}), "n", 7).is_err());
        }
    }
}
