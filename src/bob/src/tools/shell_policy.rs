use super::*;
use bob_core::core::{
    permissions::{parse_bash, Decision, PermissionRequest},
    policies::{allow_bash_commands, flag_dangerous_bash, flag_out_of_workspace_paths},
};

fn request(invocation: &Invocation, bound: &Bound) -> Result<PermissionRequest> {
    let Action::Shell { dialect, .. } = &bound.action else {
        bail!("expected prepared shell action");
    };
    ensure!(invocation.name == "bash", "prepared tool kind mismatch");
    let input: ShellInput = serde_json::from_value(invocation.input.clone())?;
    check_shell_input(&input)?;
    let bash = match shell_kind(dialect)? {
        ShellKind::Bash => Some(parse_bash(&input.command)),
        ShellKind::Pwsh => None,
    };
    Ok(PermissionRequest {
        tool: invocation.name.clone(),
        input: invocation.input.clone(),
        cwd: invocation.workspace.clone(),
        bash,
        preview: None,
        agent: None,
        read_only: false,
    })
}

pub(super) fn mandatory(invocation: &Invocation, bound: &Bound) -> Result<bool> {
    let request = request(invocation, bound)?;
    Ok(matches!(
        flag_dangerous_bash()(&request),
        Some(Decision::RequireApproval)
    ) || matches!(
        flag_out_of_workspace_paths()(&request),
        Some(Decision::RequireApproval)
    ))
}

pub(super) fn allowed(invocation: &Invocation, bound: &Bound, names: &[String]) -> Result<bool> {
    let request = request(invocation, bound)?;
    Ok(matches!(
        allow_bash_commands(names.to_vec())(&request),
        Some(Decision::Allow)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path, command: &str, dialect: &str) -> (Invocation, Bound) {
        let invocation = Invocation {
            operation_id: bob_kernel::OperationId::new(),
            run_id: bob_kernel::RunId::new(),
            thread_id: bob_kernel::ThreadId::new(),
            workspace: root.to_string_lossy().into_owned(),
            name: "bash".into(),
            input: json!({"command": command}),
        };
        let snapshot = Snapshot {
            identity: Value::Null,
            modified: Value::Null,
            len: 0,
            sha256: String::new(),
            ancestors: Value::Null,
        };
        let bound = Bound {
            version: 3,
            workspace_root: root.into(),
            workspace_identity: Value::Null,
            invocation_sha256: String::new(),
            root: root.into(),
            root_identity: Value::Null,
            action: Action::Shell {
                executable: root.join("shell"),
                snapshot,
                dialect: dialect.into(),
                helper: None,
            },
        };
        (invocation, bound)
    }

    #[test]
    fn command_allowlist_matches_analyzed_commands_not_raw_prefixes() {
        let root = std::env::current_dir().unwrap();
        let names = vec!["git".into(), "echo".into()];
        for command in ["git status", "echo hello", "git diff && echo done"] {
            let (invocation, bound) = fixture(&root, command, "Bash");
            assert!(allowed(&invocation, &bound, &names).unwrap(), "{command}");
        }
        for command in [
            "git reset --hard",
            "echo hello > output",
            "echo $(sh payload)",
            "gitty status",
        ] {
            let (invocation, bound) = fixture(&root, command, "Bash");
            assert!(!allowed(&invocation, &bound, &names).unwrap(), "{command}");
        }
    }

    #[test]
    fn dangerous_external_and_unanalyzed_commands_require_fresh_approval() {
        let root = std::env::current_dir().unwrap();
        let external = root.parent().unwrap().join("outside.txt");
        for command in [
            "rm -rf /".to_owned(),
            format!("echo '{}'", external.display()),
            "echo $(".into(),
        ] {
            let (invocation, bound) = fixture(&root, &command, "Bash");
            assert!(mandatory(&invocation, &bound).unwrap(), "{command}");
        }
        let (invocation, bound) = fixture(&root, "echo hello", "PowerShell 7");
        assert!(mandatory(&invocation, &bound).unwrap());
        assert!(!allowed(&invocation, &bound, &["echo".into()]).unwrap());
    }
}
