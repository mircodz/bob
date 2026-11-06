use crate::native_fs as filesystem;
use anyhow::{ensure, Context, Result};
use bob_kernel::{Invocation, ThreadId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
pub(crate) const READ_LIMIT: usize = 16 * 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub(crate) identity: Value,
    pub(crate) modified: Value,
    pub(crate) len: u64,
    pub(crate) sha256: String,
    pub(crate) ancestors: Value,
}
#[derive(Default)]
pub(crate) struct FileState {
    pub(crate) reads: HashMap<(ThreadId, PathBuf, PathBuf), Evidence>,
}

pub(crate) struct Evidence {
    pub(crate) snapshot: Snapshot,
    pub(crate) text: String,
    pub(crate) visible: Vec<std::ops::Range<usize>>,
}

impl Evidence {
    pub(crate) fn add_visible(&mut self, ranges: Vec<std::ops::Range<usize>>) {
        self.visible.extend(ranges);
        self.visible.sort_by_key(|range| range.start);
        let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
        for range in self.visible.drain(..) {
            if let Some(last) = merged.last_mut() {
                if range.start <= last.end {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            merged.push(range);
        }
        self.visible = merged;
    }

    pub(crate) fn fully_read(&self) -> bool {
        self.visible
            .iter()
            .any(|range| range.start == 0 && range.end == self.text.len())
    }
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(crate) fn identity(metadata: &Metadata) -> Value {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        json!({"dev": metadata.dev(), "ino": metadata.ino(), "mode": metadata.mode(), "uid":metadata.uid(), "gid":metadata.gid()})
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        json!({"created":metadata.creation_time(), "attributes":metadata.file_attributes()})
    }
    #[cfg(not(any(unix, windows)))]
    {
        json!({"created": format!("{:?}", metadata.created()), "readonly":metadata.permissions().readonly()})
    }
}

pub(crate) fn file_identity(file: &File) -> Result<Value> {
    filesystem::identity(file)
}

pub(crate) fn path_identity(path: &Path) -> Result<Value> {
    if fs::symlink_metadata(path)?.is_dir() {
        file_identity(filesystem::Directory::open(path)?.file())
    } else {
        file_identity(&filesystem::open_regular(path)?.1)
    }
}

pub(crate) fn modification(metadata: &Metadata) -> Value {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        json!([
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ])
    }
    #[cfg(not(unix))]
    {
        json!(format!("{:?}", metadata.modified()))
    }
}

pub(crate) fn root_path(workspace: &str) -> Result<PathBuf> {
    ensure!(!workspace.is_empty(), "workspace must not be empty");
    #[cfg(windows)]
    let requested = filesystem::normalize_path(Path::new(workspace))?;
    #[cfg(not(windows))]
    let requested = Path::new(workspace);
    let root = filesystem::normalize_path(
        &fs::canonicalize(requested).context("resolve workspace root")?,
    )?;
    ensure!(root.is_dir(), "workspace is not a directory");
    Ok(root)
}

pub(crate) fn requested_path(root: &Path, value: &str) -> Result<PathBuf> {
    ensure!(
        !value.is_empty() && value.len() <= 4096,
        "path must contain 1..4096 bytes"
    );
    let root = filesystem::normalize_path(root)?;
    ensure!(root.is_absolute(), "workspace root must be absolute");
    let path = filesystem::normalize_path(Path::new(value))?;
    filesystem::normalize_path(&root.join(path))
}

pub(crate) fn workspace_target(root: &Path, value: &str) -> Result<PathBuf> {
    let root = filesystem::normalize_path(root)?;
    let requested = requested_path(&root, value)?;
    ensure!(requested.starts_with(&root), "path escapes workspace root");
    Ok(requested)
}

pub(crate) fn relative_name(root: &Path, path: &Path) -> Result<String> {
    path.strip_prefix(root)?
        .components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .map(str::to_owned)
                .context("non-Unicode path cannot be represented")
        })
        .collect::<Result<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

pub(crate) fn workspace_directory(root: &Path, value: &str) -> Result<filesystem::Directory> {
    let requested = workspace_target(root, value)?;
    filesystem::Directory::open(&requested)
}

pub(crate) fn workspace_file(root: &Path, value: &str) -> Result<PathBuf> {
    let root = filesystem::normalize_path(root)?;
    let requested = workspace_target(&root, value)?;
    ensure!(requested != root, "file path names the workspace directory");
    filesystem::open_regular(&requested)?;
    Ok(requested)
}

pub(crate) fn read_snapshot(path: &Path) -> Result<(String, Snapshot)> {
    let (directory, file) = filesystem::open_regular(path)?;
    let result = read_open_snapshot(file, directory.identities()?)?;
    let named = directory.open_file(path.file_name().context("missing filename")?)?;
    ensure!(
        file_identity(&named)? == result.1.identity,
        "file path changed during read"
    );
    directory.verify()?;
    Ok(result)
}

pub(crate) fn read_open_snapshot(file: File, ancestors: Value) -> Result<(String, Snapshot)> {
    read_open_snapshot_limited(file, ancestors, READ_LIMIT)
}

pub(crate) fn read_open_snapshot_limited(
    mut file: File,
    ancestors: Value,
    limit: usize,
) -> Result<(String, Snapshot)> {
    let limit = limit.min(READ_LIMIT);
    let before = file.metadata()?;
    ensure!(
        before.len() <= limit as u64,
        "file exceeds the {limit}-byte UTF-8 read limit; no read evidence recorded"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limit,
        "file exceeds the complete-read limit; no read evidence recorded"
    );
    let after = file.metadata()?;
    ensure!(
        identity(&before) == identity(&after)
            && modification(&before) == modification(&after)
            && before.len() == after.len(),
        "file changed during read"
    );
    let snapshot = Snapshot {
        identity: file_identity(&file)?,
        modified: modification(&after),
        len: bytes.len() as u64,
        sha256: digest(&bytes),
        ancestors,
    };
    let text = String::from_utf8(bytes).context("file is not UTF-8; no read evidence recorded")?;
    Ok((text, snapshot))
}
pub(crate) fn require_evidence<'a>(
    state: &'a FileState,
    invocation: &Invocation,
    root: &Path,
    path: &Path,
    snapshot: &Snapshot,
) -> Result<&'a str> {
    let evidence = state
        .reads
        .get(&(
            invocation.thread_id.clone(),
            root.to_path_buf(),
            path.to_path_buf(),
        ))
        .context("read_file must return file content in this thread before editing")?;
    ensure!(
        &evidence.snapshot == snapshot && digest(evidence.text.as_bytes()) == snapshot.sha256,
        "file changed since this thread read it; read it again"
    );
    if invocation.name == "write_file" {
        ensure!(evidence.fully_read(), "file has not been fully shown; read every page and clipped line before overwriting, or use edit_file");
    }
    Ok(&evidence.text)
}
