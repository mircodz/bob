use super::*;
pub(super) fn executable_snapshot(path: &Path) -> Result<Snapshot> {
    let (directory, mut file) = filesystem::open_regular(path)?;
    let before = file.metadata()?;
    ensure!(
        before.len() <= 256 * 1024 * 1024,
        "shell executable exceeds the 256 MiB verification limit"
    );
    let mut hash = Sha256::new();
    let mut buf = [0u8; 32 * 1024];
    let mut count = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        ensure!(
            count <= 256 * 1024 * 1024,
            "shell executable grew beyond verification limit"
        );
        hash.update(&buf[..n]);
    }
    let after = file.metadata()?;
    ensure!(
        identity(&before) == identity(&after)
            && modification(&before) == modification(&after)
            && after.len() == count,
        "shell executable changed during verification"
    );
    let identity = file_identity(&file)?;
    ensure!(
        identity == path_identity(path)?,
        "shell executable target changed during verification"
    );
    Ok(Snapshot {
        identity,
        modified: modification(&after),
        len: count,
        sha256: format!("{:x}", hash.finalize()),
        ancestors: directory.identities()?,
    })
}
pub(super) fn shell_kind(dialect: &str) -> Result<ShellKind> {
    match dialect {
        "Bash" => Ok(ShellKind::Bash),
        "PowerShell 7" => Ok(ShellKind::Pwsh),
        _ => bail!("unknown bound shell dialect"),
    }
}

fn effective_timeout(input: &ShellInput) -> Option<f64> {
    if input.run_in_background {
        None
    } else {
        input.timeout_seconds
    }
}

pub(super) fn timeout_description(input: &ShellInput) -> String {
    effective_timeout(input).map_or_else(
        || "none (cancellation and parent-liveness controlled)".into(),
        |seconds| format!("{seconds} seconds"),
    )
}

pub(super) fn check_shell_input(input: &ShellInput) -> Result<()> {
    ensure!(
        !input.command.is_empty() && !input.command.contains('\0'),
        "command must be nonempty without NUL bytes"
    );
    ensure!(
        input.command.len() <= COMMAND_LIMIT,
        "command exceeds {COMMAND_LIMIT} bytes"
    );
    if let Some(timeout) = input.timeout_seconds {
        ensure!(
            timeout.is_finite()
                && timeout > 0.0
                && std::time::Duration::try_from_secs_f64(timeout).is_ok(),
            "timeout must be finite, positive, and representable as a duration"
        );
    }
    Ok(())
}

pub(super) fn prepare(
    root: &Path,
    invocation: &Invocation,
    configured_shell: Option<&bob_core::core::shell::ShellSelection>,
) -> Result<Action> {
    let input: ShellInput = serde_json::from_value(invocation.input.clone())?;
    check_shell_input(&input)?;
    let (executable, kind) = native_worker::discover_shell(root, configured_shell)?;
    bob_core::core::shell::invocation_args(&executable, kind, &input.command)?;
    let snapshot = executable_snapshot(&executable)?;
    let helper = {
        let path = native_worker::helper_path()?;
        let snapshot = executable_snapshot(&path)?;
        Some(BoundExecutable { path, snapshot })
    };
    Ok(Action::Shell {
        executable,
        snapshot,
        dialect: kind.name().into(),
        helper,
    })
}

pub(super) async fn execute(
    invocation: &Invocation,
    bound: &Bound,
    cancel: &watch::Receiver<bool>,
    progress: Option<bob_kernel::ToolProgress>,
) -> Result<ToolOutcome> {
    let Action::Shell {
        executable,
        snapshot,
        dialect,
        helper,
    } = &bound.action
    else {
        bail!("expected shell action");
    };
    ensure!(invocation.name == "bash", "prepared tool kind mismatch");
    let input: ShellInput = serde_json::from_value(invocation.input.clone())?;
    check_shell_input(&input)?;
    ensure!(
        &fs::canonicalize(executable)? == executable
            && &executable_snapshot(executable)? == snapshot,
        "shell executable changed since preparation"
    );
    verify_root(invocation, bound)?;
    let request = ShellRequest {
        workspace: bound.root.clone(),
        workspace_identity: bound.root_identity.clone(),
        executable: executable.clone(),
        executable_sha256: snapshot.sha256.clone(),
        dialect: dialect.clone(),
        timeout_seconds: effective_timeout(&input),
        command: input.command,
    };
    {
        let helper = helper
            .as_ref()
            .context("shell requires a bound native worker")?;
        ensure!(
            fs::canonicalize(&helper.path)? == helper.path
                && executable_snapshot(&helper.path)? == helper.snapshot,
            "native worker changed since preparation"
        );
        native_worker::execute_with_progress(
            &helper.path,
            &helper.snapshot.sha256,
            request,
            cancel.clone(),
            progress,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_preview_uses_effective_timeout() {
        let root = PathBuf::from("workspace");
        let bound = Bound {
            version: 3,
            invocation_sha256: String::new(),
            workspace_root: root.clone(),
            workspace_identity: Value::Null,
            root,
            root_identity: Value::Null,
            action: Action::Shell {
                executable: PathBuf::from("bash"),
                snapshot: Snapshot {
                    identity: Value::Null,
                    modified: Value::Null,
                    len: 0,
                    sha256: String::new(),
                    ancestors: Value::Null,
                },
                dialect: "Bash".into(),
                helper: None,
            },
        };
        for (background, timeout, expected) in [
            (false, Some(12.5), "12.5 seconds"),
            (
                false,
                None,
                "none (cancellation and parent-liveness controlled)",
            ),
            (
                true,
                Some(12.5),
                "none (cancellation and parent-liveness controlled)",
            ),
            (
                true,
                None,
                "none (cancellation and parent-liveness controlled)",
            ),
        ] {
            let invocation = Invocation {
                operation_id: bob_kernel::OperationId("timeout-preview".into()),
                run_id: bob_kernel::RunId("timeout-preview".into()),
                thread_id: bob_kernel::ThreadId("timeout-preview".into()),
                workspace: "workspace".into(),
                name: "bash".into(),
                input: json!({"command": "echo test", "timeout": timeout, "run_in_background": background}),
            };
            let input: ShellInput = serde_json::from_value(invocation.input.clone()).unwrap();
            assert_eq!(
                effective_timeout(&input),
                if background { None } else { timeout }
            );
            let timeout_line = format!("Timeout: {expected}");
            let preview = super::super::preview(&invocation, &bound).unwrap();
            assert!(preview.lines().any(|line| line == timeout_line));
            let presentation = super::super::presentation::prepared(&invocation, &bound).unwrap();
            assert!(presentation.details.contains(&timeout_line));
        }
    }
}
