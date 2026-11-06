use super::*;
#[cfg(unix)]
use crate::core::shell::ShellSelection;
#[cfg(unix)]
use crate::test_support::TestDir;
#[cfg(unix)]
use std::path::Path;
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

#[test]
fn powershell_never_supplies_bash_allowlist_analysis() {
    let tool = BashTool::new(Arc::new(CommandShell::from_resolved(
        Err("not installed".into()),
        ShellKind::Pwsh,
    )));
    for command in ["git status", "echo harmless", "Get-ChildItem"] {
        assert!(tool
            .command_analysis(&json!({"command": command}))
            .is_none());
    }
}

#[cfg(unix)]
fn fixture(root: &Path, probe: &str) -> BashTool {
    use std::os::unix::fs::PermissionsExt;
    let executable = root.join("fake pwsh 終");
    std::fs::write(&executable, format!(
        "#!/bin/sh\ncase \"$5\" in\n *PSVersionTable*) printf validated > validation; {probe} ;;\n *) printf '%s' \"$5\" > user-command ;;\nesac\n"
    )).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let shell = CommandShell::resolve(
        root,
        Some(&ShellSelection {
            dialect: ShellKind::Pwsh,
            executable,
        }),
    );
    assert!(shell.executable().is_ok());
    assert!(
        !root.join("validation").exists(),
        "discovery must not execute a version probe"
    );
    BashTool::new(Arc::new(shell))
}

#[cfg(unix)]
fn context(root: &Path) -> ToolContext {
    ToolContext {
        cwd: root.to_str().unwrap().into(),
        files: Arc::new(crate::tools::file_tracker::FileTracker::new()),
        todos: Arc::new(crate::tools::todo::TodoStore::new()),
        jobs: crate::tools::jobs::JobRegistry::new(),
        user_asker: None,
        lsp: None,
        coord: None,
        permissions: None,
    }
}

#[cfg(unix)]
#[tokio::test]
async fn approved_validation_precedes_the_verbatim_user_command() {
    let root = TestDir::new();
    let tool = fixture(&root, "exit 0");
    let ctx = context(&root);
    let script = "quotes ' \" `\nUnicode 終🙂";
    tool.execute(json!({"command": script}), &ctx)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("validation")).unwrap(),
        "validated"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("user-command")).unwrap(),
        script
    );
}

#[cfg(unix)]
#[tokio::test]
async fn rejected_version_never_starts_foreground_or_background_user_work() {
    for background in [false, true] {
        let root = TestDir::new();
        let tool = fixture(&root, "exit 125");
        let ctx = context(&root);
        let result = tool
            .execute(
                json!({"command": "must-not-run", "run_in_background": background}),
                &ctx,
            )
            .await;
        let error = if background {
            result.unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while ctx.jobs.status_of("job_1") == Some(JobStatus::Running) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(ctx.jobs.status_of("job_1"), Some(JobStatus::Failed));
            let page = ctx.jobs.output_page("job_1", 0, 24_000).unwrap();
            assert!(ctx.jobs.shutdown().await);
            page.text
        } else {
            result.unwrap_err().message
        };
        assert!(error.contains("approved version validation"), "{error}");
        assert!(error.contains("user command was not started"), "{error}");
        assert!(root.join("validation").exists());
        assert!(!root.join("user-command").exists());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_background_version_validation_never_starts_user_work() {
    let root = TestDir::new();
    let tool = fixture(&root, "exec sleep 30");
    let ctx = context(&root);
    tool.execute(
        json!({"command": "must-not-run", "run_in_background": true}),
        &ctx,
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.join("validation").exists() {
            tokio::task::yield_now().await;
        }
        assert!(ctx.jobs.cancel_and_wait("job_1").await);
    })
    .await
    .unwrap();
    assert_eq!(ctx.jobs.status_of("job_1"), Some(JobStatus::Cancelled));
    assert!(!root.join("user-command").exists());
    assert!(ctx.jobs.shutdown().await);
}

#[cfg(unix)]
#[tokio::test]
async fn approved_version_probe_is_bounded_by_the_foreground_deadline() {
    let root = TestDir::new();
    let tool = fixture(&root, "exec sleep 30");
    let ctx = context(&root);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        tool.execute(json!({"command": "must-not-run", "timeout": 0.1}), &ctx),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(result.kind, crate::tools::registry::ToolErrorKind::Timeout);
    assert!(result.message.contains("timed out"), "{}", result.message);
    assert!(!root.join("user-command").exists());
}
