use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateInput {
    pub path: String,
    pub content: String,
}

pub(super) fn nearest_parent(path: &Path) -> Result<filesystem::Directory> {
    let mut parent = path.parent().context("missing parent directory")?;
    for _ in 0..=32 {
        match fs::symlink_metadata(parent) {
            Ok(_) => return filesystem::Directory::open(parent),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                parent = parent.parent().context("missing existing ancestor")?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    bail!("creation exceeds 32 missing parent directories")
}

pub(super) fn prepare(root: &Path, input: &CreateInput, create_parents: bool) -> Result<Action> {
    ensure!(
        input.content.len() <= READ_LIMIT,
        "new file exceeds the {READ_LIMIT}-byte limit"
    );
    let path = workspace_target(root, &input.path)?;
    ensure!(path != root, "file path names the workspace directory");
    let parent = if create_parents {
        nearest_parent(&path)?
    } else {
        filesystem::Directory::open(path.parent().context("missing parent directory")?)?
    };
    ensure!(
        parent.path().starts_with(root),
        "creation parent escapes prepared root"
    );
    if parent.path() == path.parent().context("missing parent")? {
        ensure!(
            !parent.name_exists(path.file_name().context("missing filename")?)?,
            "create_file target already exists (including a symlink)"
        );
    }
    let ancestor_parent = parent.path().to_path_buf();
    let ancestors = parent.identities()?;
    let staged = path
        .parent()
        .context("missing parent")?
        .join(format!(".bob-{}.create", uuid::Uuid::new_v4()));
    Ok(Action::Create {
        path,
        ancestor_parent,
        ancestors,
        staged,
        content_sha256: digest(input.content.as_bytes()),
    })
}

#[cfg(test)]
type RaceHook = std::cell::RefCell<Option<Box<dyn FnOnce()>>>;
#[cfg(test)]
thread_local! {
    pub(super) static CREATE_PUBLISH_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
    pub(super) static CREATE_PUBLISHED_HOOK: RaceHook = const { std::cell::RefCell::new(None) };
}

pub(super) fn execute(
    invocation: &Invocation,
    bound: &Bound,
    cancel: &watch::Receiver<bool>,
) -> Result<ToolOutcome> {
    ensure!(
        matches!(invocation.name.as_str(), "create_file" | "write_file"),
        "prepared tool kind mismatch"
    );
    let Action::Create {
        path,
        ancestor_parent,
        ancestors,
        staged,
        content_sha256,
    } = &bound.action
    else {
        bail!("expected create action");
    };
    let input: CreateInput = serde_json::from_value(invocation.input.clone())?;
    ensure!(
        input.content.len() <= READ_LIMIT && digest(input.content.as_bytes()) == *content_sha256,
        "prepared creation content mismatch"
    );
    ensure!(
        workspace_target(&bound.root, &input.path)? == *path,
        "prepared creation path mismatch"
    );
    ensure!(
        staged != path
            && staged.parent() == path.parent()
            && filesystem::normalize_path(staged)? == *staged,
        "invalid creation staging path"
    );
    filesystem::check_name(path.file_name().context("missing filename")?)?;
    filesystem::check_name(staged.file_name().context("missing stage filename")?)?;
    ensure!(
        ancestor_parent.starts_with(&bound.root),
        "creation ancestor escapes prepared root"
    );
    let parent_path = path.parent().context("missing parent directory")?;
    let missing = parent_path
        .strip_prefix(ancestor_parent)?
        .components()
        .collect::<Vec<_>>();
    ensure!(
        missing.len() <= 32 && (missing.is_empty() || invocation.name == "write_file"),
        "invalid prepared parent creation"
    );
    let mut directory = filesystem::Directory::open(ancestor_parent)?;
    ensure!(
        directory.identities()? == *ancestors,
        "creation parent changed since preparation"
    );
    verify_root(invocation, bound)?;
    if cancellation_requested(cancel) {
        return Ok(cancelled_before_effect("cancelled before parent creation"));
    }
    for part in missing {
        let std::path::Component::Normal(name) = part else {
            bail!("invalid parent component")
        };
        if cancellation_requested(cancel) {
            return Ok(ToolOutcome { text: format!("Cancelled during parent creation; created directories retained through {}. Target not published.", directory.path().display()), is_error: true, interrupted: false, presentation: None });
        }
        directory = match directory.create_child(name) {
            Ok(child) => child,
            Err(error) => return Ok(unknown_effect(format!("Parent creation outcome could not be verified: {error:#}; inspect {}. Target not published; no rollback attempted.", directory.path().join(name).display()))),
        };
    }
    let target_name = path.file_name().context("missing filename")?;
    let stage_name = staged.file_name().context("missing stage filename")?;
    ensure!(
        !directory.name_exists(target_name)?,
        "create_file target already exists; nothing overwritten"
    );
    verify_root(invocation, bound)?;
    if cancellation_requested(cancel) {
        return Ok(cancelled_before_effect("cancelled before file creation"));
    }
    let mut stage = directory.create_file(stage_name)?;
    let stage_identity = file_identity(&stage)?;
    let prepared = (|| -> Result<()> {
        stage.write_all(input.content.as_bytes())?;
        stage.sync_all()?;
        let (text, snapshot) =
            read_open_snapshot(directory.open_file(stage_name)?, directory.identities()?)?;
        ensure!(
            text == input.content && snapshot.identity == stage_identity,
            "creation staging contents changed"
        );
        verify_root(invocation, bound)?;
        directory.verify()?;
        Ok(())
    })();
    drop(stage);
    if let Err(error) = prepared {
        return Ok(ToolOutcome {
            text: format!(
                "Creation not published: {error:#}; staging file retained for inspection at {}",
                staged.display()
            ),
            is_error: true,
            interrupted: false,
            presentation: None,
        });
    }
    if cancellation_requested(cancel) {
        return Ok(cancelled_before_effect(&format!(
            "cancelled before creation publication; staging file retained at {}",
            staged.display()
        )));
    }
    #[cfg(test)]
    CREATE_PUBLISH_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    match directory.publish_new(target_name, stage_name) {
        Ok(filesystem::Publication::Exists) => return Ok(ToolOutcome { text: format!("Creation refused: target exists; nothing overwritten. Staging file retained at {}", staged.display()), is_error: true, interrupted: false, presentation: None }),
        Err(error) => return Ok(unknown_effect(format!("Creation publication outcome unknown: {error:#}; inspect target {} and staged name {} relative to original parent identity {}. No rollback or retry attempted.", path.display(), staged.display(), ancestors))),
        Ok(filesystem::Publication::Published) => {}
    }
    #[cfg(test)]
    CREATE_PUBLISHED_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
    let verified = (|| -> Result<()> {
        let (text, snapshot) =
            read_open_snapshot(directory.open_file(target_name)?, directory.identities()?)?;
        ensure!(
            text == input.content && snapshot.identity == stage_identity,
            "published creation changed before verification"
        );
        directory.verify()?;
        directory.sync()?;
        ensure!(
            !cancellation_requested(cancel),
            "cancelled after publication"
        );
        Ok(())
    })();
    match verified {
        Ok(()) => Ok(ToolOutcome { text: format!("Created {} ({} bytes, sha256 {})", path.display(), input.content.len(), content_sha256), is_error: false, interrupted: false, presentation: Some(bob_kernel::ToolResultPresentation { summary: Some("Created file".into()), body_preview: Some(String::new()), exit_code: None }) }),
        Err(error) => Ok(unknown_effect(format!("Creation was published but its final outcome could not be verified: {error:#}; inspect {} relative to original parent identity {}. No rollback or retry attempted.", path.display(), ancestors))),
    }
}
