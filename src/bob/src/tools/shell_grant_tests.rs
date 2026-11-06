use super::super::{path_identity, Action, Snapshot};
use super::*;
use bob_kernel::ShellCommandGrant;
#[cfg(unix)]
use bob_kernel::{ApprovalOptions, PermissionGrant, PermissionRule, RuleDecision};

const FIRST: &str = "printf %s first";
const SECOND: &str = "printf %s second";

fn printf_scope() -> ShellCommandGrant {
    ShellCommandGrant {
        dialect: "bash".into(),
        program: "printf".into(),
    }
}

fn bound_shell(dir: &TestDir, command: &str, dialect: &str) -> PreparedTool {
    let root = dir.workspace();
    ToolService::default()
        .finish_prepare(
            Invocation {
                operation_id: OperationId::new(),
                run_id: bob_kernel::RunId::new(),
                thread_id: ThreadId::new(),
                workspace: root.to_string_lossy().into_owned(),
                name: "bash".into(),
                input: json!({"command": command}),
            },
            root.clone(),
            path_identity(&root).unwrap(),
            Action::Shell {
                executable: root.join("unexecuted-shell-fixture"),
                snapshot: Snapshot {
                    identity: Value::Null,
                    modified: Value::Null,
                    len: 0,
                    sha256: String::new(),
                    ancestors: Value::Null,
                },
                dialect: dialect.into(),
                helper: None,
            },
        )
        .unwrap()
}

#[test]
fn shell_candidate_accepts_only_simple_literal_bash_programs() {
    let dir = TestDir::new();
    let tools = ToolService::default();
    for command in [FIRST, SECOND, "printf %s literal-argument"] {
        let prepared = bound_shell(&dir, command, "Bash");
        assert_eq!(
            tools.prepared_shell_command(&prepared).unwrap(),
            Some(printf_scope())
        );
        assert!(
            !tools
                .prepared_capabilities(&prepared)
                .unwrap()
                .requires_approval
        );
    }
    for command in [
        "printf first; printf second",
        "printf $(printf nested)",
        "printf first > redirected.txt",
        "command printf first",
        "env printf first",
        "bash -c true",
        "printf '$HOME'",
        "printf \"$HOME\"",
        "printf *",
        "VALUE=first printf second",
        "printf first | printf second",
    ] {
        let prepared = bound_shell(&dir, command, "Bash");
        assert_eq!(
            tools.prepared_shell_command(&prepared).unwrap(),
            None,
            "{command}"
        );
    }
    dir.assert_empty();
}

#[test]
fn shell_candidate_abstains_for_mandatory_external_and_powershell_identity() {
    let dir = TestDir::new();
    let tools = ToolService::default();
    let external = format!("printf %s {}", dir.0.join("outside.txt").display());
    for (command, dialect) in [
        (external.as_str(), "Bash"),
        ("printf $(", "Bash"),
        (FIRST, "PowerShell 7"),
    ] {
        let prepared = bound_shell(&dir, command, dialect);
        assert!(
            tools
                .prepared_capabilities(&prepared)
                .unwrap()
                .requires_approval
        );
        assert_eq!(tools.prepared_shell_command(&prepared).unwrap(), None);
    }
    dir.assert_empty();
}

#[test]
fn shell_candidate_rejects_changed_bound_invocation_and_presentation() {
    let dir = TestDir::new();
    let tools = ToolService::default();
    let prepared = bound_shell(&dir, FIRST, "Bash");
    assert_eq!(
        tools.prepared_shell_command(&prepared).unwrap(),
        Some(printf_scope())
    );
    let mut changed = prepared.clone();
    changed.invocation.input["command"] = json!(SECOND);
    assert!(tools.prepared_shell_command(&changed).is_err());
    let mut changed = prepared.clone();
    changed.preview.push_str(" changed");
    assert!(tools.prepared_shell_command(&changed).is_err());
    let mut changed = prepared;
    changed.presentation = None;
    assert!(tools.prepared_shell_command(&changed).is_err());
    dir.assert_empty();
}

#[cfg(unix)]
async fn shell_options(handle: &Handle, decision: &Decision) -> ApprovalOptions {
    bounded(handle.approval_options(decision.operation.clone(), decision.revision))
        .await
        .unwrap()
}

#[cfg(unix)]
fn grant_command(decision: &Decision, grant: PermissionGrant) -> Command {
    Command::DecideWithGrant {
        id: CommandId::new(),
        operation: decision.operation.clone(),
        revision: decision.revision,
        approve: true,
        grant: Some(grant),
    }
}

#[cfg(unix)]
fn enqueue_shell(executor: &ScriptedExecutor, command: &str) {
    executor
        .observed
        .lock()
        .unwrap()
        .scripts
        .get_mut("root")
        .unwrap()
        .extend([
            Step::Reply(call("bash", json!({"command": command, "timeout": 5}))),
            Step::Reply(text("finished")),
        ]);
}

#[cfg(unix)]
async fn pending_shell(handle: &Handle, thread: &ThreadId) -> Decision {
    let pending = until(handle, thread, |snapshot| snapshot.decisions.len() == 1).await;
    let decision = pending.decisions[0].clone();
    assert_eq!(decision.name, "bash");
    assert_eq!(
        pending
            .operations
            .iter()
            .find(|op| op.id == decision.operation)
            .unwrap()
            .state,
        OperationState::Approval,
    );
    decision
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "requires parent-built bob-exec (or BOB_EXEC_HELPER) and Bash; executes only two local printf commands"]
async fn bash_program_grant_approval_reuse_reopen_and_fresh_guards() {
    crate::native_worker::helper_path()
        .expect("build bob-exec first, or set BOB_EXEC_HELPER to its absolute path");
    let dir = TestDir::new();
    let gate = ExecutionGate::new();
    let executor = ScriptedExecutor::new(vec![], vec![], Some(gate.clone()));
    enqueue_shell(&executor, FIRST);
    let handle = open(&dir, executor.clone()).await;
    let root = session(&handle, &dir, "root", Policy::default(), false).await;
    submit(&handle, &root.thread).await;
    let decision = pending_shell(&handle, &root.thread).await;
    let options = shell_options(&handle, &decision).await;
    assert_eq!(options.options.len(), 4);
    assert!(options.options.iter().any(|option| {
        option
            .grant
            .as_ref()
            .is_some_and(|grant| grant.command.as_deref() == Some(FIRST))
    }));
    let offered = options
        .options
        .iter()
        .find(|option| {
            option
                .grant
                .as_ref()
                .is_some_and(|grant| grant.shell_command == Some(printf_scope()))
        })
        .unwrap();
    assert!(offered.approve);
    let grant = offered.grant.clone().unwrap();
    assert_eq!(grant.tool, "bash");
    assert!(grant.command.is_none() && grant.path_prefix.is_none());
    executor.assert_calls(1, 0);
    dir.assert_empty();

    let mut forgeries = Vec::new();
    for (dialect, program) in [
        ("bash", "echo"),
        ("pwsh", "printf"),
        ("bash", ""),
        ("bash", "printf;true"),
        ("bash", "/bin/printf"),
    ] {
        forgeries.push(PermissionGrant {
            shell_command: Some(ShellCommandGrant {
                dialect: dialect.into(),
                program: program.into(),
            }),
            ..grant.clone()
        });
    }
    forgeries.push(PermissionGrant {
        shell_command: Some(ShellCommandGrant {
            dialect: "bash".into(),
            program: "p".repeat(65),
        }),
        ..grant.clone()
    });
    forgeries.push(PermissionGrant {
        command: Some(FIRST.into()),
        ..grant.clone()
    });
    forgeries.push(PermissionGrant {
        path_prefix: Some(dir.workspace().display().to_string()),
        ..grant.clone()
    });
    forgeries.push(PermissionGrant {
        tool: "read_file".into(),
        ..grant.clone()
    });
    for forged in forgeries {
        let error = bounded(handle.command(grant_command(&decision, forged)))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("offered"), "{error:#}");
        let unchanged = shell_options(&handle, &decision).await;
        assert_eq!(
            serde_json::to_value(&unchanged).unwrap(),
            serde_json::to_value(&options).unwrap()
        );
        let saved = bounded(handle.session(root.session.clone())).await.unwrap();
        assert!(saved.permissions.grants.is_empty());
        executor.assert_calls(1, 0);
        dir.assert_empty();
    }

    command(&handle, grant_command(&decision, grant.clone())).await;
    bounded(gate.entered.acquire()).await.unwrap().forget();
    executor.assert_calls(1, 1);
    dir.assert_empty();
    gate.release.add_permits(1);
    completed(&handle, &root.thread).await;
    let output: Value =
        serde_json::from_str(&executor.observed.lock().unwrap().outcomes[0].text).unwrap();
    assert_eq!(output["stdout"]["text"], "first");
    assert_eq!(output["exit_code"], 0);
    let saved = bounded(handle.session(root.session.clone())).await.unwrap();
    assert_eq!(saved.permissions.grants, vec![grant.clone()]);
    command(
        &handle,
        Command::StopThread {
            id: CommandId::new(),
            thread: root.thread.clone(),
        },
    )
    .await;
    bounded(handle.shutdown()).await.unwrap();

    let executor = ScriptedExecutor::new(vec![], vec![], None);
    let handle = bounded(Runtime::open_paused(&dir.db(), executor.clone()))
        .await
        .unwrap();
    let restored = bounded(handle.session(root.session.clone())).await.unwrap();
    assert_eq!(restored.permissions.grants, vec![grant.clone()]);
    assert_eq!(restored.policy_revision, saved.policy_revision);
    executor.assert_calls(0, 0);
    assert!(executor.observed.lock().unwrap().requests.is_empty());
    bounded(handle.activate_session(
        root.session.clone(),
        vec![Command::Resume {
            id: CommandId::new(),
            thread: root.thread.clone(),
        }],
    ))
    .await
    .unwrap();
    enqueue_shell(&executor, SECOND);
    submit(&handle, &root.thread).await;
    let settled = until(&handle, &root.thread, |snapshot| {
        assert!(
            snapshot.decisions.is_empty(),
            "remembered printf program asked again"
        );
        state(snapshot, &root.thread) == ThreadState::Completed
            && executor.observed.lock().unwrap().outcomes.len() == 1
    })
    .await;
    executor.assert_calls(1, 1);
    assert!(settled
        .operations
        .iter()
        .filter(|op| op.name == "bash")
        .all(|op| op.state == OperationState::Done));
    let output: Value =
        serde_json::from_str(&executor.observed.lock().unwrap().outcomes[0].text).unwrap();
    assert_eq!(output["stdout"]["text"], "second");
    assert_eq!(output["exit_code"], 0);

    let external = dir.0.join("outside.txt");
    fs::write(&external, "unchanged").unwrap();
    let commands = [
        "printf first; printf second".to_owned(),
        "printf $(printf nested)".into(),
        "printf first > redirected.txt".into(),
        "command printf first".into(),
        "env printf first".into(),
        format!("printf %s {}", external.display()),
    ];
    let mut preparations = 1;
    for (index, input) in commands.iter().enumerate() {
        enqueue_shell(&executor, input);
        submit(&handle, &root.thread).await;
        let decision = pending_shell(&handle, &root.thread).await;
        preparations += 1;
        let options = shell_options(&handle, &decision).await;
        assert!(options.options.iter().all(|option| {
            option
                .grant
                .as_ref()
                .is_none_or(|grant| grant.shell_command.is_none())
        }));
        if index == commands.len() - 1 {
            assert!(options.options.iter().all(|option| option.grant.is_none()));
        }
        assert!(
            bounded(handle.command(grant_command(&decision, grant.clone())))
                .await
                .is_err()
        );
        executor.assert_calls(preparations, 1);
        decide(&handle, &decision, false).await;
        let denied = completed(&handle, &root.thread).await;
        assert_eq!(
            denied
                .operations
                .iter()
                .find(|op| op.id == decision.operation)
                .unwrap()
                .state,
            OperationState::Failed
        );
        executor.assert_calls(preparations, 1);
        dir.assert_empty();
        assert_eq!(fs::read_to_string(&external).unwrap(), "unchanged");
    }

    let saved = bounded(handle.session(root.session.clone())).await.unwrap();
    let mut permissions = saved.permissions;
    permissions.rules.push(PermissionRule {
        tool: "bash".into(),
        decision: RuleDecision::RequireApproval,
    });
    command(
        &handle,
        Command::SetPermissions {
            id: CommandId::new(),
            session: root.session.clone(),
            expected_revision: saved.policy_revision,
            permissions,
        },
    )
    .await;
    enqueue_shell(&executor, "printf %s mandatory");
    submit(&handle, &root.thread).await;
    let decision = pending_shell(&handle, &root.thread).await;
    assert!(shell_options(&handle, &decision)
        .await
        .options
        .iter()
        .all(|option| option.grant.is_none()));
    assert!(
        bounded(handle.command(grant_command(&decision, grant.clone())))
            .await
            .is_err()
    );
    decide(&handle, &decision, false).await;
    completed(&handle, &root.thread).await;
    executor.assert_calls(preparations + 1, 1);
    let saved = bounded(handle.session(root.session.clone())).await.unwrap();
    assert_eq!(saved.permissions.grants, vec![grant]);
    dir.assert_empty();
    assert_eq!(fs::read_to_string(&external).unwrap(), "unchanged");
    bounded(handle.shutdown()).await.unwrap();
}
