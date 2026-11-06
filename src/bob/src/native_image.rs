use super::filesystem::{self, Directory};
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::File;
#[cfg(unix)]
use std::io::Seek;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub struct Image {
    path: PathBuf,
    file: File,
    directory: Directory,
    #[cfg(unix)]
    temporary: Option<PathBuf>,
}

impl Image {
    pub fn bind(path: &Path, approved_hash: &str) -> Result<Self> {
        let normalized = filesystem::normalize_path(path)?;
        let path = normalized.as_path();
        let directory = Directory::open(path.parent().context("executable has no parent")?)?;
        #[cfg(unix)]
        let mut source =
            directory.open_file(path.file_name().context("executable has no name")?)?;
        #[cfg(windows)]
        let mut source = {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
            };
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(path)?
        };
        #[cfg(not(any(unix, windows)))]
        let mut source = { anyhow::bail!("verified image launch unavailable on this platform") };
        let metadata = source.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= 256 * 1024 * 1024,
            "invalid executable size/type"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                metadata.file_attributes()
                    & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                    == 0,
                "executable reparse point rejected"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o111 != 0,
                "image is not executable"
            );
            #[cfg(target_os = "macos")]
            if read_only_mount(&source)? {
                ensure!(
                    copy_hash(&mut source, None)? == approved_hash,
                    "executable changed since approval"
                );
                directory.verify()?;
                return Ok(Self {
                    path: path.into(),
                    file: source,
                    directory,
                    temporary: None,
                });
            }
            let temporary_root = Directory::open(&std::fs::canonicalize("/tmp")?)?;
            let name = format!("bob-image-{}", uuid::Uuid::new_v4());
            let protected = temporary_root.private_child(std::ffi::OsStr::new(&name))?;
            let temporary = protected.path().to_path_buf();
            let name = path.file_name().context("executable has no filename")?;
            let destination = protected.create_file(name)?;
            let mut image = Self {
                path: temporary.join(name),
                file: destination,
                directory: protected,
                temporary: Some(temporary),
            };
            let copied_hash = copy_hash(&mut source, Some(&mut image.file))?;
            ensure!(
                copied_hash == approved_hash,
                "executable changed since approval"
            );
            image.file.sync_all()?;
            image
                .file
                .set_permissions(std::fs::Permissions::from_mode(0o500))?;
            // Close the write handle before exec (ETXTBSY on Linux).
            image.file = image.directory.open_file(name)?;
            ensure!(
                copy_hash(&mut image.file, None)? == approved_hash,
                "protected executable copy changed"
            );
            image.file.rewind()?;
            image.directory.verify()?;
            Ok(image)
        }
        #[cfg(not(unix))]
        {
            ensure!(
                copy_hash(&mut source, None)? == approved_hash,
                "executable changed since approval"
            );
            directory.verify()?;
            Ok(Self {
                path: path.into(),
                file: source,
                directory,
            })
        }
    }

    pub fn command(&self) -> Result<Command> {
        self.directory.verify()?;
        let named = self
            .directory
            .open_file(self.path.file_name().context("image filename missing")?)?;
        ensure!(
            filesystem::identity(&named)? == filesystem::identity(&self.file)?,
            "bound executable path changed"
        );
        Ok(Command::new(&self.path))
    }
}

#[cfg(unix)]
impl Drop for Image {
    fn drop(&mut self) {
        if let Some(temporary) = &self.temporary {
            if let Some(name) = self.path.file_name() {
                let _ = self.directory.remove_file(name);
            }
            let _ = std::fs::remove_dir(temporary);
        }
    }
}

#[cfg(target_os = "macos")]
fn read_only_mount(file: &File) -> Result<bool> {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let mut info: libc::statfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::fstatfs(file.as_raw_fd(), &mut info) } == 0,
        "inspect executable mount: {}",
        std::io::Error::last_os_error()
    );
    // Apple platform signatures cannot be relocated; use only the read-only system volume in place.
    Ok(info.f_flags & libc::MNT_RDONLY as u32 != 0
        && file.metadata()?.dev() == std::fs::metadata("/")?.dev())
}

fn copy_hash(source: &mut File, mut destination: Option<&mut File>) -> Result<String> {
    let mut hash = Sha256::new();
    let mut count = 0usize;
    let mut bytes = [0u8; 32768];
    loop {
        let n = source.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        if count == 0 {
            #[cfg(target_os = "linux")]
            let native = bytes[..n].starts_with(b"\x7fELF");
            #[cfg(target_os = "macos")]
            let native = matches!(
                bytes.get(..4).filter(|_| n >= 4),
                Some(
                    [0xfe, 0xed, 0xfa, 0xce]
                        | [0xce, 0xfa, 0xed, 0xfe]
                        | [0xfe, 0xed, 0xfa, 0xcf]
                        | [0xcf, 0xfa, 0xed, 0xfe]
                        | [0xca, 0xfe, 0xba, 0xbe]
                        | [0xbe, 0xba, 0xfe, 0xca]
                        | [0xca, 0xfe, 0xba, 0xbf]
                        | [0xbf, 0xba, 0xfe, 0xca]
                )
            );
            #[cfg(windows)]
            let native = bytes[..n].starts_with(b"MZ");
            #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
            let native = false;
            ensure!(native, "only native executable images are supported; scripts and interpreter fallbacks are refused");
        }
        count += n;
        ensure!(
            count <= 256 * 1024 * 1024,
            "executable exceeds verification limit"
        );
        if let Some(output) = destination.as_mut() {
            output.write_all(&bytes[..n])?;
        }
        hash.update(&bytes[..n]);
    }
    ensure!(count > 0, "empty executable image is not supported");
    Ok(format!("{:x}", hash.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("bob-image-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_after_image_binding_cannot_change_executed_bytes() {
        let root = TestDir::new();
        let path = root.0.join("test-image");
        std::fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
        let hash = format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
        let image = Image::bind(&path, &hash).unwrap();
        assert!(Image::bind(&path, "wrong approved hash").is_err());
        let saved = root.0.join("original");
        std::fs::rename(&path, &saved).unwrap();
        std::fs::write(&path, b"#!/bin/sh\nprintf replaced").unwrap();
        let output = image
            .command()
            .unwrap()
            .arg("--list")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{:?}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout)
            .contains("replacement_after_image_binding_cannot_change_executed_bytes"));
        assert_eq!(std::fs::read(&path).unwrap(), b"#!/bin/sh\nprintf replaced");
    }

    #[cfg(windows)]
    #[test]
    fn bound_image_denies_write_delete_and_ancestor_replacement_until_released() {
        let root = TestDir::new();
        let directory = root.0.join("program");
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("image.exe");
        std::fs::copy(std::env::current_exe().unwrap(), &path).unwrap();
        let hash = format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap()));
        let image = Image::bind(&path, &hash).unwrap();
        assert!(std::fs::write(&path, b"replacement").is_err());
        assert!(std::fs::rename(&path, directory.join("old.exe")).is_err());
        assert!(std::fs::rename(&directory, root.0.join("moved")).is_err());
        assert!(image.command().is_ok());
        drop(image);
        std::fs::write(&path, b"replacement").unwrap();
        std::fs::rename(&directory, root.0.join("moved")).unwrap();
    }
}
