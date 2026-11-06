use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::path::{Component, Path, PathBuf};

pub fn normalize_path(path: &Path) -> Result<PathBuf> {
    // Normalize representation only: resolving interior links or folding case would change grants.
    let mut normalized = PathBuf::new();
    #[cfg(windows)]
    {
        use std::path::Prefix;
        ensure!(
            !path.has_root() || path.is_absolute(),
            "root-relative Windows paths are ambiguous; use a drive or UNC absolute path"
        );
        if let Some(Component::Prefix(prefix)) = path.components().next() {
            ensure!(
                path.is_absolute(),
                "drive-relative Windows paths are ambiguous; use a drive absolute path"
            );
            match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                    ensure!(drive.is_ascii_alphabetic(), "invalid Windows drive");
                    normalized.push(format!("\\\\?\\{}:\\", char::from(drive.to_ascii_uppercase())));
                }
                Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                    check_name(server)?;
                    check_name(share)?;
                    let mut unc = std::ffi::OsString::from("\\\\?\\UNC\\");
                    unc.push(server);
                    unc.push("\\");
                    unc.push(share);
                    unc.push("\\");
                    normalized.push(unc);
                }
                _ => bail!("Windows device namespaces and unsupported verbatim paths are not supported; use a drive or UNC filesystem path"),
            }
        }
    }
    for component in path.components() {
        match component {
            Component::ParentDir => bail!("parent traversal is not allowed"),
            Component::CurDir => {}
            Component::Normal(name) => {
                check_name(name)?;
                normalized.push(name);
            }
            Component::RootDir | Component::Prefix(_) => {
                #[cfg(not(windows))]
                normalized.push(component);
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        ensure!(
            normalized.as_os_str().encode_wide().count() < 32767,
            "Windows path exceeds the extended-length limit"
        );
    }
    if normalized.as_os_str().is_empty() && !path.as_os_str().is_empty() {
        normalized.push(".");
    }
    Ok(normalized)
}

#[cfg(windows)]
fn publication_path(path: &Path) -> Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let normalized = normalize_path(path)?;
    ensure!(
        normalized == path && path.is_absolute(),
        "publication path differs from the prepared normalized target"
    );
    Ok(normalized
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect())
}

pub fn identity(file: &File) -> Result<Value> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        Ok(json!({"dev":m.dev(), "ino":m.ino(), "mode":m.mode(), "uid":m.uid(), "gid":m.gid()}))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0,
            "identify handle: {}",
            std::io::Error::last_os_error()
        );
        Ok(
            json!({"volume":info.dwVolumeSerialNumber, "index_high":info.nFileIndexHigh, "index_low":info.nFileIndexLow}),
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        bail!("stable file identity unavailable")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
    Unknown,
}

pub struct DirectoryEntry {
    pub name: std::ffi::OsString,
    pub kind: EntryKind,
}

pub struct DirectoryEntries {
    pub entries: Vec<DirectoryEntry>,
    pub truncated: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Publication {
    Published,
    Exists,
}

pub struct Directory {
    path: PathBuf,
    chain: Vec<File>,
}

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        let path = normalize_path(path)?;
        ensure!(path.is_absolute(), "directory must be absolute");
        let mut current = PathBuf::new();
        let mut chain = Vec::new();
        for part in path.components() {
            match part {
                Component::Prefix(_) | Component::RootDir => {
                    current.push(part);
                    if matches!(part, Component::RootDir) {
                        chain.push(open_directory(&current)?);
                    }
                }
                Component::Normal(name) => {
                    check_name(name)?;
                    let parent = chain.last().context("directory has no root")?;
                    current.push(name);
                    #[cfg(unix)]
                    let file = open_at(parent, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                    #[cfg(not(unix))]
                    let file = {
                        let _ = parent;
                        open_directory(&current)?
                    };
                    chain.push(file);
                }
                _ => bail!("non-normal directory component"),
            }
        }
        ensure!(!chain.is_empty(), "missing directory handle");
        let directory = Self {
            path: current,
            chain,
        };
        directory.verify()?;
        Ok(directory)
    }

    #[cfg(unix)]
    pub fn private_child(&self, name: &OsStr) -> Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        check_name(name)?;
        for file in &self.chain {
            let m = file.metadata()?;
            ensure!(m.uid() == 0 && (m.mode() & 0o022 == 0 || m.mode() & 0o1000 != 0),
                "protected executable copies require a root-owned, non-writable or sticky temporary hierarchy");
        }
        self.verify()?;
        let c = c_name(name)?;
        ensure!(
            unsafe { libc::mkdirat(self.file().as_raw_fd(), c.as_ptr(), 0o700) } == 0,
            "create protected image directory: {}",
            std::io::Error::last_os_error()
        );
        let file = open_at(self.file(), name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        let m = file.metadata()?;
        ensure!(
            m.uid() == unsafe { libc::geteuid() } && m.mode() & 0o077 == 0,
            "private executable directory ownership/permissions changed"
        );
        let mut chain = self
            .chain
            .iter()
            .map(File::try_clone)
            .collect::<std::io::Result<Vec<_>>>()?;
        chain.push(file);
        let child = Self {
            path: self.path.join(name),
            chain,
        };
        child.verify()?;
        Ok(child)
    }

    pub fn file(&self) -> &File {
        self.chain.last().expect("directory handle")
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn ancestor(&self, path: &Path) -> Result<Self> {
        let path = normalize_path(path)?;
        let suffix = self.path.strip_prefix(&path)?;
        let length = self.chain.len() - suffix.components().count();
        let directory = Self {
            path: path.to_path_buf(),
            chain: self.chain[..length]
                .iter()
                .map(File::try_clone)
                .collect::<std::io::Result<Vec<_>>>()?,
        };
        directory.verify()?;
        Ok(directory)
    }
    pub fn identities(&self) -> Result<Value> {
        self.chain
            .iter()
            .map(identity)
            .collect::<Result<Vec<_>>>()
            .map(Value::Array)
    }
    pub fn verify(&self) -> Result<()> {
        #[cfg(unix)]
        {
            let mut parts = self.path.components().filter_map(|p| match p {
                Component::Normal(name) => Some(name),
                _ => None,
            });
            for pair in self.chain.windows(2) {
                let name = parts.next().context("directory chain mismatch")?;
                let named = open_at(&pair[0], name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                ensure!(
                    identity(&named)? == identity(&pair[1])?,
                    "directory was moved or replaced: {}",
                    self.path.display()
                );
            }
        }
        #[cfg(windows)]
        for (path, pinned) in self
            .path
            .ancestors()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .zip(&self.chain)
        {
            ensure_directory(pinned)?;
            let named = open_directory(path)?;
            ensure!(
                identity(&named)? == identity(pinned)?,
                "directory was moved or replaced: {}",
                path.display()
            );
        }
        Ok(())
    }
    pub fn open_file(&self, name: &OsStr) -> Result<File> {
        check_name(name)?;
        #[cfg(unix)]
        let file = open_at(
            self.file(),
            name,
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY,
            0,
        )?;
        #[cfg(not(unix))]
        let file = regular_options().open(self.path.join(name))?;
        ensure_regular(&file)?;
        Ok(file)
    }
    pub fn create_child(&self, name: &OsStr) -> Result<Self> {
        check_name(name)?;
        self.verify()?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = c_name(name)?;
            ensure!(
                unsafe { libc::mkdirat(self.file().as_raw_fd(), name.as_ptr(), 0o755) } == 0,
                "create anchored directory: {}",
                std::io::Error::last_os_error()
            );
        }
        #[cfg(windows)]
        std::fs::create_dir(self.path.join(name))?;
        #[cfg(not(any(unix, windows)))]
        bail!("anchored directory creation unavailable on this platform");
        let child = self.open_child(name)?;
        self.sync()?;
        Ok(child)
    }

    pub fn open_child(&self, name: &OsStr) -> Result<Self> {
        check_name(name)?;
        self.verify()?;
        #[cfg(unix)]
        let file = open_at(self.file(), name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
        #[cfg(not(unix))]
        let file = open_directory(&self.path.join(name))?;
        let mut chain = self
            .chain
            .iter()
            .map(File::try_clone)
            .collect::<std::io::Result<Vec<_>>>()?;
        chain.push(file);
        let child = Self {
            path: self.path.join(name),
            chain,
        };
        child.verify()?;
        Ok(child)
    }
    pub fn name_exists(&self, name: &OsStr) -> Result<bool> {
        check_name(name)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = c_name(name)?;
            let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe {
                libc::fstatat(
                    self.file().as_raw_fd(),
                    name.as_ptr(),
                    &mut metadata,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
            {
                return Ok(true);
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(false);
            }
            Err(error.into())
        }
        #[cfg(not(unix))]
        match std::fs::symlink_metadata(self.path.join(name)) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
    pub fn entries(&self, limit: usize, stop: impl Fn() -> bool) -> Result<DirectoryEntries> {
        self.verify()?;
        let mut entries = Vec::new();
        let mut truncated = false;
        #[cfg(unix)]
        {
            use std::os::fd::IntoRawFd;
            use std::os::unix::ffi::OsStrExt;
            let fd = open_at(
                self.file(),
                OsStr::new("."),
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )?
            .into_raw_fd();
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                let error = std::io::Error::last_os_error();
                unsafe {
                    libc::close(fd);
                }
                return Err(error.into());
            }
            struct Stream(*mut libc::DIR);
            impl Drop for Stream {
                fn drop(&mut self) {
                    unsafe {
                        libc::closedir(self.0);
                    }
                }
            }
            let stream = Stream(stream);
            loop {
                if entries.len() >= limit || stop() {
                    truncated = true;
                    break;
                }
                #[cfg(target_os = "linux")]
                unsafe {
                    *libc::__errno_location() = 0;
                }
                #[cfg(target_os = "macos")]
                unsafe {
                    *libc::__error() = 0;
                }
                let entry = unsafe { libc::readdir(stream.0) };
                if entry.is_null() {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error().unwrap_or(0) != 0 {
                        return Err(error.into());
                    }
                    break;
                }
                let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
                let name = OsStr::from_bytes(name.to_bytes());
                if name == OsStr::new(".") || name == OsStr::new("..") {
                    continue;
                }
                let kind = match unsafe { (*entry).d_type } {
                    libc::DT_REG => EntryKind::File,
                    libc::DT_DIR => EntryKind::Directory,
                    libc::DT_UNKNOWN => EntryKind::Unknown,
                    _ => EntryKind::Other,
                };
                entries.push(DirectoryEntry {
                    name: name.to_owned(),
                    kind,
                });
            }
        }
        #[cfg(windows)]
        for entry in std::fs::read_dir(&self.path)? {
            if entries.len() >= limit || stop() {
                truncated = true;
                break;
            }
            let entry = entry?;
            use std::os::windows::fs::MetadataExt;
            let metadata = entry.metadata()?;
            let kind = if metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                != 0
            {
                EntryKind::Other
            } else if metadata.is_dir() {
                EntryKind::Directory
            } else if metadata.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            entries.push(DirectoryEntry {
                name: entry.file_name(),
                kind,
            });
        }
        #[cfg(not(any(unix, windows)))]
        bail!("anchored directory enumeration unavailable on this platform");
        self.verify()?;
        Ok(DirectoryEntries { entries, truncated })
    }
    pub fn publish_new(&self, target: &OsStr, staged: &OsStr) -> Result<Publication> {
        check_name(target)?;
        check_name(staged)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let target = c_name(target)?;
            let staged = c_name(staged)?;
            let fd = self.file().as_raw_fd();
            #[cfg(target_os = "linux")]
            let rc = unsafe {
                libc::renameat2(
                    fd,
                    staged.as_ptr(),
                    fd,
                    target.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            };
            #[cfg(target_os = "macos")]
            let rc = unsafe {
                libc::renameatx_np(fd, staged.as_ptr(), fd, target.as_ptr(), libc::RENAME_EXCL)
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (fd, target, staged);
                bail!("atomic no-clobber publication unavailable");
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    return Ok(Publication::Exists);
                }
                return Err(error.into());
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
            self.verify()?;
            let staged_path = publication_path(&self.path.join(staged))?;
            let target_path = publication_path(&self.path.join(target))?;
            if unsafe {
                MoveFileExW(
                    staged_path.as_ptr(),
                    target_path.as_ptr(),
                    MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    return Ok(Publication::Exists);
                }
                return Err(error.into());
            }
        }
        #[cfg(not(any(unix, windows)))]
        bail!("atomic no-clobber publication unavailable");
        Ok(Publication::Published)
    }
    pub fn create_file(&self, name: &OsStr) -> Result<File> {
        check_name(name)?;
        #[cfg(unix)]
        let file = open_at(
            self.file(),
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )?;
        #[cfg(not(unix))]
        let file = regular_options()
            .write(true)
            .create_new(true)
            .open(self.path.join(name))?;
        Ok(file)
    }
    #[cfg(windows)]
    pub fn create_pinned_stage(&self, name: &OsStr) -> Result<File> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
        };
        check_name(name)?;
        self.verify()?;
        Ok(regular_options()
            .write(true)
            .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
            .share_mode(FILE_SHARE_READ)
            .create_new(true)
            .open(self.path.join(name))?)
    }
    pub fn remove_file(&self, name: &OsStr) -> Result<()> {
        check_name(name)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = c_name(name)?;
            ensure!(
                unsafe { libc::unlinkat(self.file().as_raw_fd(), name.as_ptr(), 0) } == 0,
                "unlink anchored file: {}",
                std::io::Error::last_os_error()
            );
        }
        #[cfg(not(unix))]
        std::fs::remove_file(self.path.join(name))?;
        Ok(())
    }
    pub fn publish(
        &self,
        target: &OsStr,
        staged: &OsStr,
        backup: &OsStr,
        #[cfg(windows)] pinned_stage: File,
    ) -> Result<()> {
        check_name(target)?;
        check_name(staged)?;
        check_name(backup)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let _ = backup;
            let target = c_name(target)?;
            let staged = c_name(staged)?;
            let fd = self.file().as_raw_fd();
            #[cfg(target_os = "linux")]
            let rc = unsafe {
                libc::renameat2(
                    fd,
                    staged.as_ptr(),
                    fd,
                    target.as_ptr(),
                    libc::RENAME_EXCHANGE,
                )
            };
            #[cfg(target_os = "macos")]
            let rc = unsafe {
                libc::renameatx_np(fd, staged.as_ptr(), fd, target.as_ptr(), libc::RENAME_SWAP)
            };
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                let _ = (fd, target, staged);
                bail!("atomic exchange unavailable on this platform");
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            ensure!(
                rc == 0,
                "anchored atomic exchange: {}",
                std::io::Error::last_os_error()
            );
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
            self.verify()?;
            // The backup is an absent destination, never an earlier copied file.
            match std::fs::symlink_metadata(self.path.join(backup)) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
                Ok(_) => bail!("backup destination already exists; refusing to remove it"),
            }
            let target = publication_path(&self.path.join(target))?;
            let staged = publication_path(&self.path.join(staged))?;
            let backup = publication_path(&self.path.join(backup))?;
            // ReplaceFileW must reopen the stage with write/delete access. Releasing this
            // pin leaves a pathname substitution window; post-publication checks can only
            // report unknown and retain recovery files, not prevent unapproved publication.
            drop(pinned_stage);
            #[cfg(test)]
            STAGE_RELEASED_HOOK.with(|slot| {
                if let Some(hook) = slot.borrow_mut().take() {
                    hook();
                }
            });
            ensure!(
                unsafe {
                    ReplaceFileW(
                        target.as_ptr(),
                        staged.as_ptr(),
                        backup.as_ptr(),
                        0,
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                } != 0,
                "ReplaceFileW: {}",
                std::io::Error::last_os_error()
            );
        }
        #[cfg(not(any(unix, windows)))]
        bail!("safe replacement unavailable");
        Ok(())
    }
    pub fn sync(&self) -> Result<()> {
        #[cfg(unix)]
        self.file().sync_all()?;
        Ok(())
    }
}

#[cfg(windows)]
pub fn discard_pinned_stage(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    ensure!(
        unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&disposition as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
            )
        } != 0,
        "discard pinned stage: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(all(test, windows))]
thread_local! {
    pub(crate) static STAGE_RELEASED_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

pub fn open_regular(path: &Path) -> Result<(Directory, File)> {
    let directory = Directory::open(path.parent().context("file has no parent")?)?;
    let file = directory.open_file(path.file_name().context("file has no name")?)?;
    directory.verify()?;
    Ok((directory, file))
}

pub(crate) fn check_name(name: &OsStr) -> Result<()> {
    let mut parts = Path::new(name).components();
    ensure!(
        matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none(),
        "expected a single filename"
    );
    ensure!(
        !name.as_encoded_bytes().contains(&0),
        "filename contains NUL"
    );
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        ensure!(
            name.encode_wide().count() <= 255,
            "Windows filename exceeds 255 UTF-16 code units"
        );
        let text = name.to_str().context("non-Unicode Windows filename")?;
        ensure!(
            !text.ends_with(['.', ' '])
                && !text.chars().any(|c| c < ' ' || "<>:\"/\\|?*".contains(c)),
            "Windows aliases, streams and special filenames are not supported"
        );
        let stem = text
            .split('.')
            .next()
            .unwrap_or(text)
            .trim_end_matches(' ')
            .to_uppercase();
        let device = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(*prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        ensure!(!device, "Windows device names are not supported");
    }
    Ok(())
}

fn ensure_regular(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "only regular files are supported");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                == 0,
            "reparse points are not supported"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn c_name(name: &OsStr) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    Ok(std::ffi::CString::new(name.as_bytes())?)
}

#[cfg(unix)]
fn open_at(parent: &File, name: &OsStr, flags: i32, mode: libc::c_uint) -> Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = c_name(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    ensure!(
        fd >= 0,
        "open anchored component: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_directory(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::*;
        options
            .share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options
        .open(path)
        .with_context(|| format!("open directory {}", path.display()))?;
    ensure_directory(&file).with_context(|| format!("verify directory {}", path.display()))?;
    Ok(file)
}

fn ensure_directory(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(metadata.is_dir(), "component is not a directory");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                == 0,
            "interior directory reparse points are not supported; select the physical directory as the workspace instead"
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn regular_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::*;
        options
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
}
