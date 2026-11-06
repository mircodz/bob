#![cfg(windows)]

use anyhow::{ensure, Result};
use async_trait::async_trait;
use bob::{adapters::ToolService, native_worker};
use bob_core::core::shell::{ShellKind, ShellSelection};
use bob_kernel::*;
use bob_types::{
    Completion, ContentBlock, Message, ReasoningEffort, Role, StopReason, ToolSpec, Usage,
};
use serde_json::{json, Value};
use std::future::Future;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Console::{GetStdHandle, STD_OUTPUT_HANDLE},
        Pipes::GetNamedPipeServerProcessId,
        Threading::{
            OpenProcess, TerminateProcess, WaitForSingleObject, CREATE_NO_WINDOW,
            PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        },
    },
};

const WAIT: Duration = Duration::from_secs(60);

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob acceptance 終 {}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        if std::thread::panicking() {
            if let Some(artifacts) = std::env::var_os("BOB_NATIVE_ARTIFACTS") {
                let destination = PathBuf::from(artifacts).join(uuid::Uuid::new_v4().to_string());
                let _ = std::fs::create_dir_all(&destination);
                for name in [
                    "effect",
                    "shell.pid",
                    "helper.pid",
                    "descendant.pid",
                    "receipt.json",
                ] {
                    let source = self.0.join(name);
                    if std::fs::metadata(&source).is_ok_and(|m| m.len() <= 65536) {
                        let _ = std::fs::copy(source, destination.join(name));
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("native acceptance watchdog expired")
}

fn selection(kind: ShellKind) -> ShellSelection {
    let (variable, installed) = match kind {
        ShellKind::Pwsh => ("BOB_NATIVE_PWSH", "PowerShell/7/pwsh.exe"),
        ShellKind::Bash => ("BOB_NATIVE_BASH", "Git/usr/bin/bash.exe"),
    };
    ShellSelection {
        dialect: kind,
        executable: std::env::var_os(variable)
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("ProgramFiles").expect("ProgramFiles"))
                    .join(installed)
            }),
    }
}

fn completion(block: ContentBlock, stop_reason: StopReason) -> Completion {
    Completion {
        message: Message {
            role: Role::Assistant,
            content: vec![block],
        },
        stop_reason,
        usage: Usage::default(),
    }
}

struct Driver {
    tools: ToolService,
    next: Mutex<Option<Value>>,
    outcomes: Mutex<Vec<ToolOutcome>>,
    expected_helper: PathBuf,
}
impl Driver {
    fn new(kind: ShellKind, input: Option<Value>) -> Arc<Self> {
        Arc::new(Self {
            tools: ToolService::new(Some(selection(kind)), None),
            next: Mutex::new(input),
            outcomes: Mutex::new(Vec::new()),
            expected_helper: native_worker::helper_path()
                .expect("build or extract the real helper"),
        })
    }
}
#[async_trait]
impl Executor for Driver {
    fn models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: "deterministic-native".into(),
            context_window: 128_000,
        }]
    }
    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![ToolDescriptor {
            effect: EffectClass::Write,
            spec: ToolSpec {
                name: "bash".into(),
                description: "Native acceptance shell".into(),
                input_schema: json!({"type":"object", "properties":{
                    "command":{"type":"string"}, "timeout":{"type":"number"},
                    "run_in_background":{"type":"boolean"}}, "required":["command"],
                    "additionalProperties":false}),
            },
        }]
    }
    fn tool_capabilities(&self, _: &str) -> ToolCapabilities {
        ToolCapabilities {
            executes_process: true,
            mutates_workspace: true,
            requires_approval: true,
            plan_safe: false,
            ..Default::default()
        }
    }
    async fn generate(
        &self,
        _: RequestSpec,
        _: watch::Receiver<bool>,
        _: ModelProgress,
    ) -> Result<Completion> {
        Ok(match self.next.lock().unwrap().take() {
            Some(input) => completion(
                ContentBlock::ToolUse {
                    id: uuid::Uuid::new_v4().to_string(),
                    name: "bash".into(),
                    input,
                },
                StopReason::ToolUse,
            ),
            None => completion(
                ContentBlock::Text {
                    text: "Acceptance complete.".into(),
                },
                StopReason::EndTurn,
            ),
        })
    }
    async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
        let prepared = self.tools.prepare(invocation).await?;
        let helper: PathBuf =
            serde_json::from_value(prepared.specification["action"]["helper"]["path"].clone())?;
        ensure!(
            helper == self.expected_helper,
            "preparation selected a different helper"
        );
        Ok(prepared)
    }
    async fn execute(
        &self,
        prepared: PreparedTool,
        cancel: watch::Receiver<bool>,
    ) -> Result<ToolOutcome> {
        let outcome = self.tools.execute(prepared, cancel).await?;
        self.outcomes.lock().unwrap().push(outcome.clone());
        Ok(outcome)
    }
}

async fn start(handle: &Handle, root: &Path) -> Receipt {
    let receipt = bounded(handle.command(Command::CreateSession {
        id: CommandId::new(),
        workspace: root.to_str().unwrap().into(),
        profile: Profile {
            model: "deterministic-native".into(),
            instructions: "Synthetic acceptance only.".into(),
            read_only: false,
            max_requests: 8,
            reasoning: ReasoningEffort::Off,
        },
        policy: Policy::default(),
    }))
    .await
    .unwrap();
    bounded(handle.command(Command::Submit {
        id: CommandId::new(),
        thread: receipt.thread.clone(),
        text: "Run the synthetic fixture.".into(),
    }))
    .await
    .unwrap();
    receipt
}

async fn until(handle: &Handle, thread: &ThreadId, predicate: impl Fn(&View) -> bool) -> View {
    bounded(async {
        loop {
            let view = handle.view(Some(thread.clone()), None, 100).await.unwrap();
            assert!(view.error.is_none(), "{:?}", view.error);
            if predicate(&view) {
                return view;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
}

async fn decide(handle: &Handle, thread: &ThreadId, approve: bool) {
    let pending = until(handle, thread, |view| !view.decisions.is_empty()).await;
    assert_eq!(pending.decisions.len(), 1);
    let decision = &pending.decisions[0];
    bounded(handle.command(Command::Decide {
        id: CommandId::new(),
        operation: decision.operation.clone(),
        revision: decision.revision,
        approve,
    }))
    .await
    .unwrap();
}

fn short_script(kind: ShellKind, code: i32) -> String {
    match kind {
        ShellKind::Pwsh => format!("$ErrorActionPreference='Stop'; if ((Get-Content -LiteralPath 'input 終' -Raw) -ne 'cwd 界') {{ exit 92 }}; [IO.File]::AppendAllText('effect','x'); [Console]::Out.Write('stdout 界'); [Console]::Error.Write('stderr 終'); exit {code}"),
        ShellKind::Bash => format!("set -eu; [ \"$(cat 'input 終')\" = 'cwd 界' ] || exit 92; printf x >> effect; printf 'stdout 界'; printf 'stderr 終' >&2; exit {code}"),
    }
}

#[tokio::test]
#[ignore = "run extracted acceptance driver with scripts/windows-acceptance.ps1 -Package"]
async fn packaged_helper_requires_approval_and_executes_both_shells() {
    assert!(
        std::env::var_os("BOB_EXEC_HELPER").is_none(),
        "package discovery must not use an override"
    );
    let package = std::fs::canonicalize(
        std::env::var_os("BOB_NATIVE_PACKAGE_DIR").expect("package runner required"),
    )
    .unwrap();
    let executable = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
    assert_eq!(executable.parent().unwrap(), package);
    assert_eq!(
        native_worker::helper_path().unwrap(),
        package.join("bob-exec.exe")
    );
    assert!(package.join("bob.exe").is_file());
    let missing = TestDir::new();
    let isolated_driver = missing.0.join("native-acceptance-driver.exe");
    std::fs::copy(&executable, &isolated_driver).unwrap();
    let mut child = ChildGuard(
        ProcessCommand::new(isolated_driver)
            .args([
                "--ignored",
                "--exact",
                "packaged_missing_helper_fixture",
                "--nocapture",
            ])
            .env_remove("BOB_EXEC_HELPER")
            .env("BOB_NATIVE_MISSING_ROOT", &missing.0)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let status = bounded(async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        status.success(),
        "missing sibling must not discover a development helper"
    );
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        for (approve, background, code) in [
            (false, false, 0),
            (true, false, 0),
            (true, false, 7),
            (true, true, 0),
            (true, true, 7),
        ] {
            let root = TestDir::new();
            std::fs::write(root.0.join("input 終"), "cwd 界").unwrap();
            let driver = Driver::new(
                kind,
                Some(
                    json!({"command":short_script(kind, code), "timeout":20.0, "run_in_background":background}),
                ),
            );
            let handle = bounded(Runtime::open(&root.0.join("runtime.db"), driver.clone()))
                .await
                .unwrap();
            let receipt = start(&handle, &root.0).await;
            until(&handle, &receipt.thread, |v| !v.decisions.is_empty()).await;
            assert!(!root.0.join("effect").exists(), "work ran before approval");
            decide(&handle, &receipt.thread, approve).await;
            until(&handle, &receipt.thread, |v| {
                v.threads
                    .iter()
                    .any(|t| t.id == receipt.thread && t.state == ThreadState::Completed)
            })
            .await;
            if approve {
                bounded(async {
                    loop {
                        let jobs = handle.jobs(receipt.session.clone()).await.unwrap();
                        if !driver.outcomes.lock().unwrap().is_empty()
                            && jobs.iter().all(|job| {
                                !matches!(
                                    job.state,
                                    JobState::Queued | JobState::Running | JobState::Stopping
                                )
                            })
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await;
                assert_eq!(std::fs::read(root.0.join("effect")).unwrap(), b"x");
                let outcomes = driver.outcomes.lock().unwrap();
                assert_eq!(outcomes.len(), 1);
                assert_eq!(outcomes[0].is_error, code != 0, "{}", outcomes[0].text);
                assert!(!outcomes[0].interrupted);
                let value: Value = serde_json::from_str(&outcomes[0].text).unwrap();
                assert_eq!(value["exit_code"], code);
                assert!(value["stdout"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("stdout 界"));
                assert!(value["stderr"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("stderr 終"));
            } else {
                assert!(!root.0.join("effect").exists());
                assert!(driver.outcomes.lock().unwrap().is_empty());
            }
            bounded(handle.shutdown()).await.unwrap();
        }
    }
}

#[tokio::test]
#[ignore = "subprocess fixture only"]
async fn packaged_missing_helper_fixture() {
    let root = PathBuf::from(std::env::var_os("BOB_NATIVE_MISSING_ROOT").expect("fixture root"));
    assert!(native_worker::helper_path().is_err());
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        let service = ToolService::new(Some(selection(kind)), None);
        let invocation = Invocation {
            operation_id: OperationId::new(),
            run_id: RunId::new(),
            thread_id: ThreadId::new(),
            workspace: root.to_str().unwrap().into(),
            name: "bash".into(),
            input: json!({"command":short_script(kind, 0), "timeout":20.0}),
        };
        assert!(service.prepare(invocation).await.is_err());
        assert!(!root.join("effect").exists());
    }
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct ProcessGuard(OwnedHandle);
impl ProcessGuard {
    fn open(pid: u32) -> Self {
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        assert!(
            !raw.is_null(),
            "open process {pid}: {}",
            std::io::Error::last_os_error()
        );
        Self(unsafe { OwnedHandle::from_raw_handle(raw) })
    }
    fn exited(&self) -> bool {
        let status = unsafe { WaitForSingleObject(self.0.as_raw_handle(), 0) };
        assert!(status == WAIT_OBJECT_0 || status == WAIT_TIMEOUT);
        status == WAIT_OBJECT_0
    }
}
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        unsafe {
            if WaitForSingleObject(self.0.as_raw_handle(), 0) == WAIT_TIMEOUT {
                TerminateProcess(self.0.as_raw_handle(), 1);
                WaitForSingleObject(self.0.as_raw_handle(), 5000);
            }
        }
    }
}

#[test]
#[ignore = "subprocess fixture only"]
fn acceptance_descendant_fixture() {
    let root = PathBuf::from(std::env::var_os("BOB_NATIVE_RECOVERY_ROOT").expect("fixture root"));
    let mut helper = 0;
    assert_ne!(
        unsafe { GetNamedPipeServerProcessId(GetStdHandle(STD_OUTPUT_HANDLE), &mut helper) },
        0
    );
    std::fs::write(root.join("helper.pid"), helper.to_string()).unwrap();
    std::fs::write(root.join("descendant.pid"), std::process::id().to_string()).unwrap();
    std::thread::sleep(Duration::from_secs(120));
    panic!("descendant should have been killed with its owner");
}

fn recovery_script(kind: ShellKind) -> String {
    let exe = std::env::current_exe().unwrap();
    match kind {
        ShellKind::Pwsh => format!(
            r#"
$ErrorActionPreference='Stop'
[IO.File]::AppendAllText('effect','x')
[IO.File]::WriteAllText('shell.pid',[string]$PID)
$p=[Diagnostics.ProcessStartInfo]::new()
$p.FileName='{exe}'
$p.UseShellExecute=$false
$p.ArgumentList.Add('--ignored'); $p.ArgumentList.Add('--exact'); $p.ArgumentList.Add('acceptance_descendant_fixture'); $p.ArgumentList.Add('--nocapture')
$c=[Diagnostics.Process]::Start($p)
$c.WaitForExit()
"#,
            exe = exe.to_str().unwrap().replace('\'', "''")
        ),
        ShellKind::Bash => format!(
            r#"
set -eu
printf x >> effect
cat /proc/$$/winpid > shell.pid
'{exe}' --ignored --exact acceptance_descendant_fixture --nocapture
"#,
            exe = exe
                .to_str()
                .unwrap()
                .trim_start_matches(r"\\?\")
                .replace('\\', "/")
                .replace('\'', "'\\''")
        ),
    }
}

#[tokio::test]
#[ignore = "subprocess fixture only"]
async fn acceptance_owner_fixture() {
    let root = PathBuf::from(std::env::var_os("BOB_NATIVE_RECOVERY_ROOT").expect("fixture root"));
    let kind = match std::env::var("BOB_NATIVE_RECOVERY_SHELL").unwrap().as_str() {
        "pwsh" => ShellKind::Pwsh,
        "bash" => ShellKind::Bash,
        other => panic!("invalid dialect {other}"),
    };
    let driver = Driver::new(
        kind,
        Some(json!({"command":recovery_script(kind), "timeout":120.0})),
    );
    let handle = bounded(Runtime::open(&root.join("runtime.db"), driver))
        .await
        .unwrap();
    let receipt = start(&handle, &root).await;
    std::fs::write(
        root.join("receipt.json"),
        serde_json::to_vec(&receipt).unwrap(),
    )
    .unwrap();
    decide(&handle, &receipt.thread, true).await;
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn real_helper_owner_death_preserves_unknown_and_never_replays() {
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        let root = TestDir::new();
        let helper = native_worker::helper_path().expect("prebuild native helper");
        let mut owner = ChildGuard(
            ProcessCommand::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "acceptance_owner_fixture",
                    "--nocapture",
                ])
                .env("BOB_NATIVE_RECOVERY_ROOT", &root.0)
                .env(
                    "BOB_NATIVE_RECOVERY_SHELL",
                    if kind == ShellKind::Pwsh {
                        "pwsh"
                    } else {
                        "bash"
                    },
                )
                .env("BOB_EXEC_HELPER", helper)
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let mut processes = Vec::new();
        bounded(async {
            for name in ["shell", "helper", "descendant"] {
                loop {
                    assert!(
                        owner.0.try_wait().unwrap().is_none(),
                        "owner exited before effect"
                    );
                    if let Some(pid) = std::fs::read_to_string(root.0.join(format!("{name}.pid")))
                        .ok()
                        .and_then(|text| text.trim().parse().ok())
                    {
                        let process = ProcessGuard::open(pid);
                        assert!(!process.exited());
                        processes.push(process);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        })
        .await;
        let db = root.0.join("runtime.db");
        for alias in [
            db.clone(),
            root.0.join(".").join("runtime.db"),
            std::fs::canonicalize(&db).unwrap(),
        ] {
            let result = bounded(Runtime::open(&alias, Driver::new(kind, None))).await;
            assert!(result
                .err()
                .unwrap()
                .to_string()
                .contains("execution owner"));
        }
        let hardlink = root.0.join("alias.db");
        match std::fs::hard_link(&db, &hardlink) {
            Ok(()) => {
                let error = bounded(Runtime::open(&hardlink, Driver::new(kind, None)))
                    .await
                    .err()
                    .unwrap();
                assert!(error.to_string().contains("hard-linked"), "{error:#}");
            }
            Err(error) => assert!(matches!(error.raw_os_error(), Some(5 | 32)), "{error}"),
        }
        owner.0.kill().unwrap();
        bounded(async {
            while owner.0.try_wait().unwrap().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            while !processes.iter().all(ProcessGuard::exited) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        if hardlink.exists() {
            std::fs::remove_file(hardlink).unwrap();
        }
        assert_eq!(std::fs::read(root.0.join("effect")).unwrap(), b"x");
        let receipt: Receipt =
            serde_json::from_slice(&std::fs::read(root.0.join("receipt.json")).unwrap()).unwrap();
        let driver = Driver::new(kind, None);
        let handle = bounded(Runtime::open(&db, driver.clone())).await.unwrap();
        let recovered = bounded(handle.view(Some(receipt.thread.clone()), None, 100))
            .await
            .unwrap();
        let operation = recovered
            .operations
            .iter()
            .find(|op| op.name == "bash")
            .unwrap();
        assert_eq!(operation.state, OperationState::Unknown);
        let recovery_artifact = operation
            .result
            .clone()
            .expect("inspectable recovery evidence");
        let recovery_text = bounded(handle.artifact(recovery_artifact.clone(), 0, 65536))
            .await
            .unwrap();
        assert!(recovery_text.contains("unknown"), "{recovery_text}");
        assert!(bounded(handle.command(Command::Resume {
            id: CommandId::new(),
            thread: receipt.thread.clone()
        }))
        .await
        .is_err());
        bounded(
            handle.command(Command::ResolveUnknown {
                id: CommandId::new(),
                operation: operation.id.clone(),
                evidence:
                    "Inspected synthetic effect: exactly one x; helper and descendants are dead."
                        .into(),
            }),
        )
        .await
        .unwrap();
        bounded(handle.command(Command::Resume {
            id: CommandId::new(),
            thread: receipt.thread.clone(),
        }))
        .await
        .unwrap();
        until(&handle, &receipt.thread, |v| {
            v.threads
                .iter()
                .any(|t| t.id == receipt.thread && t.state == ThreadState::Completed)
        })
        .await;
        assert!(driver.outcomes.lock().unwrap().is_empty());
        assert_eq!(std::fs::read(root.0.join("effect")).unwrap(), b"x");
        bounded(handle.shutdown()).await.unwrap();
        let reopened = bounded(Runtime::open(&db, Driver::new(kind, None)))
            .await
            .unwrap();
        let view = bounded(reopened.view(Some(receipt.thread.clone()), None, 100))
            .await
            .unwrap();
        assert!(view
            .operations
            .iter()
            .any(|op| op.id == operation.id && op.state == OperationState::Resolved));
        assert_eq!(
            bounded(reopened.artifact(recovery_artifact, 0, 65536))
                .await
                .unwrap(),
            recovery_text
        );
        assert_eq!(std::fs::read(root.0.join("effect")).unwrap(), b"x");
        bounded(reopened.shutdown()).await.unwrap();
    }
}
