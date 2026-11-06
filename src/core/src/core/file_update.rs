use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_RETRY: Duration = Duration::from_millis(10);

#[cfg(windows)]
#[path = "file_update_windows.rs"]
mod windows;

pub(crate) fn update_path(path: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => std::fs::canonicalize(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent)?;
            Ok(
                std::fs::canonicalize(parent)?.join(path.file_name().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "file update requires a filename",
                    )
                })?),
            )
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

pub(crate) struct FileLock {
    _file: File,
}

impl FileLock {
    fn open(path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    }

    fn try_acquire(file: &File) -> io::Result<bool> {
        match file.try_lock() {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let file = Self::open(path)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !Self::try_acquire(&file)? {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for file update lock",
                ));
            }
            std::thread::sleep(LOCK_RETRY);
        }
        Ok(Self { _file: file })
    }

    pub(crate) async fn acquire_async(path: &Path) -> io::Result<Self> {
        let file = Self::open(path)?;
        let deadline = Instant::now() + LOCK_TIMEOUT;
        while !Self::try_acquire(&file)? {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for file update lock",
                ));
            }
            tokio::time::sleep(LOCK_RETRY).await;
        }
        Ok(Self { _file: file })
    }
}

struct TemporaryFile(PathBuf, bool);

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.1 {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

pub(crate) fn atomic_replace(path: &Path, contents: &[u8], private: bool) -> io::Result<()> {
    #[cfg(windows)]
    windows::ensure_no_recovery(path)?;
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let (mut temporary, mut file) = loop {
        let mut random = [0u8; 16];
        getrandom::getrandom(&mut random).map_err(|error| io::Error::other(error.to_string()))?;
        let nonce: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let temporary = sibling(path, &format!(".{nonce}.tmp"));
        match create_private(&temporary) {
            Ok(file) => break (TemporaryFile(temporary, false), file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    if !private {
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
    }
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    replace_file(&mut temporary, path)
}

#[cfg(not(windows))]
fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(windows)]
use windows::create_private;

#[cfg(not(windows))]
fn replace_file(temporary: &mut TemporaryFile, to: &Path) -> io::Result<()> {
    std::fs::rename(&temporary.0, to)
}

#[cfg(windows)]
fn replace_file(temporary: &mut TemporaryFile, to: &Path) -> io::Result<()> {
    windows::replace(temporary, to)
}

#[cfg(test)]
#[path = "file_update_tests.rs"]
mod tests;
