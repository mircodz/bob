use super::*;
pub(super) fn replacement(text: &str, input: &EditInput) -> Result<String> {
    ensure!(!input.old_string.is_empty(), "old_string must not be empty");
    ensure!(
        input.old_string.len() <= READ_LIMIT && input.new_string.len() <= READ_LIMIT,
        "edit strings exceed the {READ_LIMIT}-byte limit"
    );
    let matches = text.matches(&input.old_string).count();
    ensure!(
        input.old_string != input.new_string,
        "old_string and new_string are identical"
    );
    ensure!(matches > 0, "old_string not found");
    ensure!(matches == 1 || input.replace_all, "old_string must match exactly once; found {matches} matches; add context or set replace_all");
    let replaced = if input.replace_all { matches } else { 1 };
    let size = text
        .len()
        .checked_sub(replaced * input.old_string.len())
        .and_then(|size| {
            input
                .new_string
                .len()
                .checked_mul(replaced)
                .and_then(|added| size.checked_add(added))
        })
        .context("replacement size overflow")?;
    ensure!(
        size <= READ_LIMIT,
        "replacement exceeds the {READ_LIMIT}-byte limit"
    );
    let result = if input.replace_all {
        text.replace(&input.old_string, &input.new_string)
    } else {
        text.replacen(&input.old_string, &input.new_string, 1)
    };
    ensure!(
        result.len() <= READ_LIMIT,
        "replacement exceeds the {READ_LIMIT}-byte limit"
    );
    Ok(result)
}
pub(super) fn record_known_write(
    state: &mut FileState,
    invocation: &Invocation,
    root: &Path,
    path: &Path,
    expected: &str,
    fully_known: bool,
) {
    let Ok((text, snapshot)) = read_snapshot(path) else {
        return;
    };
    if text != expected {
        return;
    }
    let visible = if fully_known {
        std::iter::once(0..text.len()).collect()
    } else {
        Vec::new()
    };
    state.reads.insert(
        (
            invocation.thread_id.clone(),
            root.to_path_buf(),
            path.to_path_buf(),
        ),
        Evidence {
            snapshot,
            text,
            visible,
        },
    );
}

pub(super) fn verify_file(
    invocation: &Invocation,
    bound: &Bound,
    requested: &str,
    path: &Path,
    snapshot: &Snapshot,
) -> Result<String> {
    verify_root(invocation, bound)?;
    ensure!(
        workspace_file(&bound.root, requested)? == path,
        "file target changed since preparation"
    );
    let (text, actual) = read_snapshot(path)?;
    ensure!(
        &actual == snapshot,
        "file target or contents changed since preparation; read and prepare again"
    );
    ensure!(
        workspace_file(&bound.root, requested)? == path,
        "file path changed during verification"
    );
    Ok(text)
}
struct Staged<'a> {
    directory: &'a filesystem::Directory,
    name: &'a std::ffi::OsStr,
    snapshot: Snapshot,
    #[cfg(windows)]
    pinned: Option<File>,
    keep: bool,
}

impl<'a> Staged<'a> {
    fn create(
        directory: &'a filesystem::Directory,
        name: &'a std::ffi::OsStr,
        text: &str,
        source: &File,
    ) -> Result<Self> {
        #[cfg(not(windows))]
        let mut file = directory.create_file(name)?;
        #[cfg(windows)]
        let mut file = directory.create_pinned_stage(name)?;
        let created = file_identity(&file)?;
        let result = (|| -> Result<Snapshot> {
            file.write_all(text.as_bytes())?;
            copy_metadata(source, &file)?;
            file.sync_all()?;
            #[cfg(not(windows))]
            {
                drop(file);
                read_open_snapshot(directory.open_file(name)?, directory.identities()?)
                    .map(|(_, snapshot)| snapshot)
            }
            #[cfg(windows)]
            {
                #[cfg(test)]
                STAGE_SNAPSHOT_HOOK.with(|slot| {
                    if let Some(hook) = slot.borrow_mut().take() {
                        hook();
                    }
                });
                read_created_stage(directory, name, &created, text)
            }
        })();
        match result {
            Ok(snapshot) => Ok(Self {
                directory,
                name,
                snapshot,
                #[cfg(windows)]
                pinned: Some(file),
                keep: false,
            }),
            Err(error) => {
                #[cfg(windows)]
                let _ = filesystem::discard_pinned_stage(&file);
                #[cfg(not(windows))]
                if directory
                    .open_file(name)
                    .and_then(|file| file_identity(&file))
                    .is_ok_and(|id| id == created)
                {
                    let _ = directory.remove_file(name);
                }
                Err(error)
            }
        }
    }
    fn verify(&self) -> Result<()> {
        ensure!(
            read_open_snapshot(
                self.directory.open_file(self.name)?,
                self.directory.identities()?
            )?
            .1 == self.snapshot,
            "staged file changed"
        );
        Ok(())
    }
}

#[cfg(windows)]
fn read_created_stage(
    directory: &filesystem::Directory,
    name: &std::ffi::OsStr,
    created: &Value,
    expected: &str,
) -> Result<Snapshot> {
    let (text, snapshot) = read_open_snapshot(directory.open_file(name)?, directory.identities()?)?;
    ensure!(
        snapshot.identity == *created && text == expected,
        "staged file identity or contents differ from the created replacement"
    );
    Ok(snapshot)
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        if !self.keep && self.verify().is_ok() {
            #[cfg(not(windows))]
            let _ = self.directory.remove_file(self.name);
            #[cfg(windows)]
            if let Some(file) = &self.pinned {
                #[cfg(test)]
                STAGE_DISCARD_HOOK.with(|slot| {
                    if let Some(hook) = slot.borrow_mut().take() {
                        hook();
                    }
                });
                let _ = filesystem::discard_pinned_stage(file);
            }
        }
    }
}

#[cfg(test)]
type RaceHook = std::cell::RefCell<Option<Box<dyn FnOnce()>>>;

#[cfg(test)]
thread_local! {
    pub(super) static PUBLISH_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
    pub(super) static PUBLISHED_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
    pub(super) static STAGE_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
    pub(super) static READ_INSTRUCTIONS_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, windows))]
thread_local! {
    static STAGE_SNAPSHOT_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
    static STAGE_DISCARD_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, windows))]
#[path = "staged_windows_tests.rs"]
mod staged_windows_tests;

fn displaced_matches(actual: &Snapshot, approved: &Snapshot) -> bool {
    // Exchanging names changes ctime on Unix, but not the approved data's mtime.
    #[cfg(unix)]
    let modified =
        actual.modified[0] == approved.modified[0] && actual.modified[1] == approved.modified[1];
    #[cfg(not(unix))]
    let modified = actual.modified == approved.modified;
    actual.identity == approved.identity
        && actual.ancestors == approved.ancestors
        && actual.sha256 == approved.sha256
        && actual.len == approved.len
        && modified
}

pub(super) fn guarded_replace(
    target: &Path,
    staged_path: &Path,
    backup_path: &Path,
    approved: (&str, &Snapshot),
    after: &str,
    validate: impl Fn() -> Result<()>,
    cancelled: impl Fn() -> bool,
) -> Result<ToolOutcome> {
    let (before, snapshot) = approved;
    let (directory, source) = filesystem::open_regular(target)?;
    let directory_identity = file_identity(directory.file())?;
    let target_name = target.file_name().context("missing target name")?;
    let staged_name = staged_path.file_name().context("missing stage name")?;
    let backup_name = backup_path.file_name().context("missing backup name")?;
    ensure!(
        staged_path.parent() == target.parent() && backup_path.parent() == target.parent(),
        "recovery path outside target directory"
    );
    validate()?;
    ensure!(
        read_open_snapshot(source.try_clone()?, directory.identities()?)?.1 == *snapshot,
        "anchored target differs from approved snapshot"
    );
    #[cfg(test)]
    STAGE_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    directory.verify()?;
    let mut staged = Staged::create(&directory, staged_name, after, &source)?;
    validate()?;
    directory.verify()?;
    staged.verify()?;
    if cancelled() {
        return Ok(cancelled_before_effect(
            "cancelled before atomic replacement; target unchanged",
        ));
    }
    #[cfg(test)]
    PUBLISH_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    staged.keep = true;
    #[cfg(unix)]
    let recovery_name = staged_name;
    #[cfg(not(unix))]
    let recovery_name = backup_name;
    let recovery = directory.path().join(recovery_name);
    let checked = (|| -> Result<()> {
        directory.publish(
            target_name,
            staged_name,
            backup_name,
            #[cfg(windows)]
            staged.pinned.take().context("missing pinned stage")?,
        )?;
        #[cfg(test)]
        PUBLISHED_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let (displaced, actual) =
            read_open_snapshot(directory.open_file(recovery_name)?, directory.identities()?)?;
        ensure!(
            displaced == before && displaced_matches(&actual, snapshot),
            "actual displaced inode/content differs from approved snapshot"
        );
        let (installed, actual) =
            read_open_snapshot(directory.open_file(target_name)?, directory.identities()?)?;
        ensure!(
            installed == after && actual.identity == staged.snapshot.identity,
            "published target changed after replacement"
        );
        directory.verify()?;
        directory.sync()?;
        Ok(())
    })();
    match checked {
        Ok(()) => Ok(ToolOutcome {
            text: format!("Edited {} (sha256 {}); displaced original retained at {}\n{}", target.display(), digest(after.as_bytes()), recovery.display(), presentation::diff_preview(before, after, 256 * 1024)),
            is_error: false,
            interrupted: false,
            presentation: Some(bob_kernel::ToolResultPresentation { summary: Some("File edited; displaced original retained".into()), body_preview: Some(presentation::diff_preview(before, after, 2048)), exit_code: None }),
        }),
        Err(error) => Ok(unknown_effect(format!("replacement of {} could not be verified: {error:#}; no automatic rollback or recovery deletion attempted. Inspect the target, staged name {} and backup name {} in the original directory (identity {}). If that directory moved, these names remain relative to that moved directory.", target.display(), staged_path.display(), backup_path.display(), directory_identity))),
    }
}

fn copy_metadata(source: &File, destination: &File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let before = source.metadata()?;
        let current = destination.metadata()?;
        if (before.uid(), before.gid()) != (current.uid(), current.gid()) {
            ensure!(
                unsafe { libc::fchown(destination.as_raw_fd(), before.uid(), before.gid()) } == 0,
                "cannot preserve file ownership: {}",
                std::io::Error::last_os_error()
            );
        }
        destination.set_permissions(before.permissions())?;
        #[cfg(target_os = "macos")]
        ensure!(
            unsafe {
                libc::fcopyfile(
                    source.as_raw_fd(),
                    destination.as_raw_fd(),
                    std::ptr::null_mut(),
                    libc::COPYFILE_METADATA,
                )
            } == 0,
            "cannot preserve file metadata: {}",
            std::io::Error::last_os_error()
        );
        #[cfg(target_os = "linux")]
        {
            let attributes = extended_attributes(source)?;
            for name in extended_attributes(destination)?.keys() {
                if !attributes.contains_key(name) {
                    ensure!(
                        unsafe {
                            libc::fremovexattr(destination.as_raw_fd(), name.as_ptr().cast())
                        } == 0,
                        "cannot clear inherited extended attribute"
                    );
                }
            }
            for (name, value) in &attributes {
                ensure!(
                    unsafe {
                        libc::fsetxattr(
                            destination.as_raw_fd(),
                            name.as_ptr().cast(),
                            value.as_ptr().cast(),
                            value.len(),
                            0,
                        )
                    } == 0,
                    "cannot preserve extended attribute"
                );
            }
            ensure!(
                extended_attributes(destination)? == attributes,
                "extended attribute copy verification failed"
            );
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        bail!("safe metadata-preserving replacement is unavailable on this Unix platform");
        Ok(())
    }
    #[cfg(windows)]
    {
        destination
            .set_permissions(source.metadata()?.permissions())
            .map_err(Into::into)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (source, destination);
        bail!("safe file replacement is unavailable on this platform")
    }
}

#[cfg(target_os = "linux")]
fn extended_attributes(file: &File) -> Result<HashMap<Vec<u8>, Vec<u8>>> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    let size = unsafe { libc::flistxattr(fd, std::ptr::null_mut(), 0) };
    ensure!(
        (0..=1024 * 1024).contains(&size),
        "cannot read bounded extended attribute names"
    );
    let mut names = vec![0u8; size as usize];
    let size = unsafe { libc::flistxattr(fd, names.as_mut_ptr().cast(), names.len()) };
    ensure!(
        size >= 0 && size as usize <= names.len(),
        "extended attributes changed"
    );
    names.truncate(size as usize);
    let mut attributes = HashMap::new();
    let mut total = 0usize;
    for name in names.split_inclusive(|b| *b == 0) {
        let name = std::ffi::CStr::from_bytes_with_nul(name)?;
        let size = unsafe { libc::fgetxattr(fd, name.as_ptr(), std::ptr::null_mut(), 0) };
        ensure!(size >= 0, "cannot read extended attribute");
        total = total
            .checked_add(size as usize)
            .context("extended attribute size overflow")?;
        ensure!(total <= 1024 * 1024, "extended attributes exceed 1 MiB");
        let mut value = vec![0u8; size as usize];
        let size =
            unsafe { libc::fgetxattr(fd, name.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
        ensure!(
            size >= 0 && size as usize <= value.len(),
            "extended attribute changed"
        );
        value.truncate(size as usize);
        attributes.insert(name.to_bytes_with_nul().to_vec(), value);
    }
    Ok(attributes)
}

pub(super) async fn execute(
    files: &Mutex<FileState>,
    invocation: Invocation,
    bound: Bound,
    cancel: &mut watch::Receiver<bool>,
    discovery_home: Option<&Path>,
) -> Result<ToolOutcome> {
    let mut state = tokio::select! {
        biased;
        _ = bob_kernel::cancelled(cancel) => return Ok(cancelled_before_effect("cancelled before file execution")),
        state = files.lock() => state,
    };
    match &bound.action {
        Action::Read { path, snapshot } => {
            ensure!(
                invocation.name == "read_file",
                "prepared tool kind mismatch"
            );
            let input: ReadInput = serde_json::from_value(invocation.input.clone())?;
            let text = verify_file(&invocation, &bound, &input.path, path, snapshot)?;
            if cancellation_requested(cancel) {
                return Ok(cancelled_before_effect(
                    "cancelled before returning file contents",
                ));
            }
            #[cfg(test)]
            READ_INSTRUCTIONS_HOOK.with(|slot| {
                if let Some(hook) = slot.borrow_mut().take() {
                    hook();
                }
            });
            let report = (|| -> Result<crate::project_instructions::InstructionReport> {
                let directory =
                    filesystem::Directory::open(path.parent().context("missing file parent")?)?;
                ensure!(
                    directory.identities()? == snapshot.ancestors,
                    "instruction scope changed since file read"
                );
                let context = crate::project_instructions::DiscoveryContext::from_workspace(
                    &bound.workspace_root,
                    discovery_home,
                )?;
                crate::project_instructions::load_with_context(
                    &bound.root,
                    &directory,
                    &context,
                    || cancellation_requested(cancel),
                )
            })()
            .unwrap_or_else(|error| {
                crate::project_instructions::InstructionReport {
                    failures: vec![format!("Project conventions unavailable: {error:#}")],
                    truncated: true,
                    ..Default::default()
                }
            });
            let attachment = crate::project_instructions::attachment(&report)?;
            if cancellation_requested(cancel) {
                return Ok(cancelled_before_effect(
                    "cancelled before returning file conventions",
                ));
            }
            let page = read::page(&text, &input)?;
            let presentation = page.presentation(&report);
            if !page.visible.is_empty() {
                let evidence = state
                    .reads
                    .entry((invocation.thread_id, bound.root, path.clone()))
                    .or_insert_with(|| Evidence {
                        snapshot: snapshot.clone(),
                        text: text.clone(),
                        visible: Vec::new(),
                    });
                if evidence.snapshot != *snapshot {
                    *evidence = Evidence {
                        snapshot: snapshot.clone(),
                        text,
                        visible: Vec::new(),
                    };
                }
                evidence.add_visible(page.visible);
            }
            Ok(ToolOutcome {
                text: format!("{}{attachment}", page.body),
                is_error: false,
                interrupted: false,
                presentation: Some(presentation),
            })
        }
        Action::Edit {
            path,
            snapshot,
            result_sha256,
            staged,
            backup,
        } => {
            ensure!(
                matches!(
                    invocation.name.as_str(),
                    "edit_file" | "multi_edit" | "write_file"
                ),
                "prepared tool kind mismatch"
            );
            let requested = mutation_path(&invocation)?;
            let before = verify_file(&invocation, &bound, &requested, path, snapshot)?;
            require_evidence(&state, &invocation, &bound.root, path, snapshot)?;
            let fully_known = state
                .reads
                .get(&(
                    invocation.thread_id.clone(),
                    bound.root.clone(),
                    path.clone(),
                ))
                .is_some_and(Evidence::fully_read);
            let after = mutation_result(&before, &invocation)?;
            ensure!(
                digest(after.as_bytes()) == *result_sha256,
                "prepared replacement hash mismatch"
            );
            if cancellation_requested(cancel) {
                return Ok(cancelled_before_effect("cancelled before file replacement"));
            }
            ensure!(
                staged.parent() == path.parent()
                    && backup.parent() == path.parent()
                    && staged != backup
                    && staged != path
                    && backup != path,
                "invalid staged recovery paths"
            );
            let outcome = guarded_replace(
                path,
                staged,
                backup,
                (&before, snapshot),
                &after,
                || verify_file(&invocation, &bound, &requested, path, snapshot).map(|_| ()),
                || cancellation_requested(cancel),
            );
            state.reads.retain(|(_, _, target), _| target != path);
            if outcome
                .as_ref()
                .is_ok_and(|outcome| !outcome.is_error && !outcome.interrupted)
            {
                record_known_write(
                    &mut state,
                    &invocation,
                    &bound.root,
                    path,
                    &after,
                    fully_known,
                );
            }
            outcome
        }
        Action::Shell { .. }
        | Action::Create { .. }
        | Action::Repository { .. }
        | Action::Instructions { .. } => unreachable!(),
    }
}
