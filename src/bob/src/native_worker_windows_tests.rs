use super::*;
use bob_core::core::shell::{ShellKind, ShellSelection};
use sha2::{Digest, Sha256};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    System::{
        Console::{GetStdHandle, STD_OUTPUT_HANDLE},
        Pipes::GetNamedPipeServerProcessId,
        Threading::{
            OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
            CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        },
    },
};

const READY_WAIT: Duration = Duration::from_secs(15);
const EXIT_WAIT: Duration = Duration::from_secs(10);
const EXECUTION_WAIT: Duration = Duration::from_secs(50);
const DESCENDANT_FIXTURE: &str = "native_worker::windows_tests::descendant_fixture";
const PARENT_FIXTURE: &str = "native_worker::windows_tests::parent_fixture";
const CONSOLE_PARENT_FIXTURE: &str = "native_worker::windows_tests::console_parent_fixture";
const CONSOLE_PROBE_FIXTURE: &str = "native_worker::windows_tests::console_probe_fixture";

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("bob-worker-{} 終", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        Self(std::fs::canonicalize(root).unwrap())
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture(std::process::Child);

impl Fixture {
    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "fixture did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        unsafe {
            WaitForSingleObject(self.0.as_raw_handle(), 2_000);
        }
    }
}

struct Process {
    name: &'static str,
    handle: OwnedHandle,
}

impl Process {
    fn open(name: &'static str, pid: u32) -> Self {
        let raw = unsafe {
            OpenProcess(
                PROCESS_SYNCHRONIZE | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
                0,
                pid,
            )
        };
        assert!(
            !raw.is_null(),
            "open {name} ({pid}): {}",
            std::io::Error::last_os_error()
        );
        Self {
            name,
            handle: unsafe { OwnedHandle::from_raw_handle(raw) },
        }
    }

    fn exited(&self) -> bool {
        match unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 0) } {
            WAIT_OBJECT_0 => true,
            WAIT_TIMEOUT => false,
            status => panic!(
                "wait for {} returned {status}: {}",
                self.name,
                std::io::Error::last_os_error()
            ),
        }
    }

    fn image(&self) -> PathBuf {
        use std::os::windows::ffi::OsStringExt;
        let mut buffer = vec![0u16; 32768];
        let mut length = buffer.len() as u32;
        assert_ne!(
            unsafe {
                QueryFullProcessImageNameW(
                    self.handle.as_raw_handle(),
                    0,
                    buffer.as_mut_ptr(),
                    &mut length,
                )
            },
            0,
            "query {} image: {}",
            self.name,
            std::io::Error::last_os_error()
        );
        std::fs::canonicalize(std::ffi::OsString::from_wide(&buffer[..length as usize])).unwrap()
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // A failed assertion must not leave a fixture running; success is checked before cleanup.
        unsafe {
            if WaitForSingleObject(self.handle.as_raw_handle(), 0) == WAIT_TIMEOUT {
                TerminateProcess(self.handle.as_raw_handle(), 1);
                WaitForSingleObject(self.handle.as_raw_handle(), 2_000);
            }
        }
    }
}

#[derive(Default)]
struct Processes(Vec<Process>);

impl Processes {
    async fn ready(&mut self, root: &Path) {
        tokio::time::timeout(READY_WAIT, async {
            for name in ["worker", "shell", "branch", "leaf"] {
                loop {
                    if let Some(pid) = std::fs::read_to_string(root.join(format!("{name}.pid")))
                        .ok()
                        .and_then(|text| text.trim().parse::<u32>().ok())
                    {
                        let process = Process::open(name, pid);
                        assert!(
                            !process.exited(),
                            "{name} exited before the lifecycle trigger"
                        );
                        self.0.push(process);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        })
        .await
        .expect("real helper, selected shell, and both descendants must publish PIDs");
    }

    async fn assert_exited(&self) {
        tokio::time::timeout(EXIT_WAIT, async {
            while !self.0.iter().all(Process::exited) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real helper/shell/descendants survived lifecycle cleanup");
    }

    fn assert_images(&self, helper: &Path, shell: &Path) {
        assert_eq!(
            self.0[0].image(),
            helper,
            "pipe server must be the actual helper"
        );
        assert_eq!(
            self.0[1].image(),
            shell,
            "request must execute the explicitly selected shell image"
        );
        let fixture = std::fs::canonicalize(std::env::current_exe().unwrap()).unwrap();
        assert_eq!(self.0[2].image(), fixture);
        assert_eq!(self.0[3].image(), fixture);
    }
}

fn hash(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn request(root: &Path, exit_code: i32, kind: ShellKind) -> ShellRequest {
    let (variable, installed) = match kind {
        ShellKind::Pwsh => ("BOB_NATIVE_PWSH", "PowerShell/7/pwsh.exe"),
        ShellKind::Bash => ("BOB_NATIVE_BASH", "Git/usr/bin/bash.exe"),
    };
    let executable = std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("ProgramFiles").expect("ProgramFiles")).join(installed)
        });
    let (executable, selected) = discover_shell(
        root,
        Some(&ShellSelection {
            dialect: kind,
            executable,
        }),
    )
    .expect("install the explicitly selected native shell; no dialect fallback");
    assert_eq!(selected, kind);
    std::fs::write(root.join("input file 終"), "fixture 終").unwrap();
    let fixture = std::env::current_exe().unwrap();
    let command = format!(
        r#"
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSEdition -ne 'Core' -or $PSVersionTable.PSVersion.Major -lt 7) {{ exit 91 }}
if ((Get-Content -LiteralPath 'input file 終' -Raw) -ne 'fixture 終') {{ exit 92 }}
[IO.File]::AppendAllText('executions', 'x')
[IO.File]::WriteAllText('shell.pid', [string]$PID)
[Console]::Out.Write('stdout 界'); [Console]::Out.Flush()
[Console]::Error.Write('stderr 終'); [Console]::Error.Flush()
$start = [Diagnostics.ProcessStartInfo]::new()
$start.FileName = {fixture}
$start.UseShellExecute = $false
$start.WorkingDirectory = (Get-Location).ProviderPath
$start.ArgumentList.Add('--exact')
$start.ArgumentList.Add({test})
$start.ArgumentList.Add('--nocapture')
$start.Environment['BOB_NATIVE_DESCENDANT'] = 'branch'
$start.Environment['BOB_NATIVE_ROOT'] = {root}
$child = [Diagnostics.Process]::Start($start)
$child.WaitForExit()
if ($child.ExitCode -ne 0) {{ exit 93 }}
[Console]::Out.Write('final 界')
exit {exit_code}
"#,
        fixture = quote(fixture.to_str().unwrap()),
        test = quote(DESCENDANT_FIXTURE),
        root = quote(root.to_str().unwrap())
    );
    let command = if kind == ShellKind::Bash {
        let fixture = fixture
            .to_str()
            .unwrap()
            .trim_start_matches(r"\\?\")
            .replace('\\', "/");
        let bash_quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
        format!(
            r#"
set -eu
[ "$(cat 'input file 終')" = 'fixture 終' ] || exit 92
printf x >> executions
cat /proc/$$/winpid > shell.pid
printf 'stdout 界'
printf 'stderr 終' >&2
BOB_NATIVE_DESCENDANT=branch BOB_NATIVE_ROOT={root} MSYS2_ARG_CONV_EXCL='*' {fixture} --exact {test} --nocapture
printf 'final 界'
exit {exit_code}
"#,
            root = bash_quote(root.to_str().unwrap()),
            fixture = bash_quote(&fixture),
            test = bash_quote(DESCENDANT_FIXTURE),
        )
    } else {
        command
    };
    ShellRequest {
        workspace: root.into(),
        workspace_identity: filesystem::identity(filesystem::Directory::open(root).unwrap().file())
            .unwrap(),
        executable_sha256: hash(&executable),
        executable,
        dialect: kind.name().into(),
        command,
        timeout_seconds: Some(20.0),
    }
}

fn assert_once(root: &Path) {
    for name in ["executions", "branch.executions", "leaf.executions"] {
        assert_eq!(
            std::fs::read(root.join(name)).unwrap(),
            b"x",
            "{name}: command must never be reexecuted"
        );
    }
}

#[test]
fn descendant_fixture() {
    let Some(mode) = std::env::var_os("BOB_NATIVE_DESCENDANT") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("BOB_NATIVE_ROOT").unwrap());
    let mode = mode.to_str().unwrap();
    assert!(matches!(mode, "branch" | "leaf"));
    let mut executions = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join(format!("{mode}.executions")))
        .unwrap();
    std::io::Write::write_all(&mut executions, b"x").unwrap();
    drop(executions);
    if mode == "branch" {
        // ProcessStartInfo inherits the shell's OS stdout, whose pipe server is bob-exec.
        let mut worker = 0;
        assert_ne!(
            unsafe { GetNamedPipeServerProcessId(GetStdHandle(STD_OUTPUT_HANDLE), &mut worker) },
            0,
            "find actual helper from inherited output pipe: {}",
            std::io::Error::last_os_error()
        );
        std::fs::write(root.join("worker.pid"), worker.to_string()).unwrap();
    }
    std::fs::write(
        root.join(format!("{mode}.pid")),
        std::process::id().to_string(),
    )
    .unwrap();
    if mode == "branch" {
        let mut leaf = Fixture(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", DESCENDANT_FIXTURE, "--nocapture"])
                .env("BOB_NATIVE_DESCENDANT", "leaf")
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        );
        assert!(leaf.wait(Duration::from_secs(90)).success());
    } else {
        let deadline = Instant::now() + Duration::from_secs(80);
        while !root.join("release").exists() {
            assert!(
                Instant::now() < deadline,
                "fixture release deadline elapsed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("descendant stdout 界");
        eprintln!("descendant stderr 終");
    }
}

#[tokio::test]
async fn real_helper_streams_unicode_cwd_and_exit_status() {
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        streams_unicode_cwd_and_exit_status(kind).await;
    }
}

async fn streams_unicode_cwd_and_exit_status(kind: ShellKind) {
    for exit_code in [0, 7] {
        let root = TestDir::new();
        let helper = helper_path().expect("prebuild bob-exec before Windows helper acceptance");
        let helper_hash = hash(&helper);
        let request = request(&root.0, exit_code, kind);
        let shell = request.executable.clone();
        let (_sender, receiver) = watch::channel(false);
        let mut owned = Processes::default();
        let mut execution = Box::pin(execute(&helper, &helper_hash, request, receiver));
        tokio::select! {
            _ = owned.ready(&root.0) => {},
            result = &mut execution => panic!("real helper exited before descendants started: {result:?}"),
        }
        owned.assert_images(&helper, &shell);
        std::fs::write(root.0.join("release"), b"go").unwrap();
        let outcome = tokio::time::timeout(EXIT_WAIT, &mut execution)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.is_error, exit_code != 0, "{}", outcome.text);
        assert!(!outcome.interrupted, "{}", outcome.text);
        let value: serde_json::Value = serde_json::from_str(&outcome.text).unwrap();
        assert_eq!(value["outcome"], "exited");
        assert_eq!(value["exit_code"], exit_code);
        let stdout = value["stdout"]["text"].as_str().unwrap();
        let stderr = value["stderr"]["text"].as_str().unwrap();
        for text in ["stdout 界", "descendant stdout 界", "final 界"] {
            assert!(stdout.contains(text), "{stdout}");
        }
        for text in ["stderr 終", "descendant stderr 終"] {
            assert!(stderr.contains(text), "{stderr}");
        }
        assert!(!stdout.contains("stderr 終"));
        assert!(!stderr.contains("stdout 界"));
        for stream in ["stdout", "stderr"] {
            assert_eq!(value[stream]["truncated"], false);
            assert_eq!(value[stream]["incomplete"], false);
            assert_eq!(
                value[stream]["total_bytes"],
                value[stream]["retained_bytes"]
            );
        }
        assert_eq!(outcome.presentation.unwrap().exit_code, Some(exit_code));
        owned.assert_exited().await;
        assert_once(&root.0);
    }
}

async fn stopped_execution(mode: &str, kind: ShellKind) {
    let root = TestDir::new();
    let helper = helper_path().expect("prebuild bob-exec before Windows helper acceptance");
    let helper_hash = hash(&helper);
    let request = request(&root.0, 0, kind);
    let shell = request.executable.clone();
    let (sender, receiver) = watch::channel(false);
    let mut owned = Processes::default();
    let mut execution = Box::pin(execute(&helper, &helper_hash, request, receiver));
    tokio::select! {
        _ = owned.ready(&root.0) => {},
        result = &mut execution => panic!("real helper exited before {mode}: {result:?}"),
    }
    owned.assert_images(&helper, &shell);
    if mode != "drop" {
        if mode == "cancel" {
            sender.send(true).unwrap();
        }
        let outcome = tokio::time::timeout(EXECUTION_WAIT, &mut execution)
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.is_error && outcome.interrupted, "{}", outcome.text);
        let value: serde_json::Value = serde_json::from_str(&outcome.text).unwrap();
        assert_eq!(
            value["outcome"],
            if mode == "cancel" {
                "cancelled"
            } else {
                "timed_out"
            }
        );
        assert!(value["stdout"]["text"]
            .as_str()
            .unwrap()
            .contains("stdout 界"));
        assert!(value["stderr"]["text"]
            .as_str()
            .unwrap()
            .contains("stderr 終"));
        assert!(!value["stdout"]["text"]
            .as_str()
            .unwrap()
            .contains("final 界"));
    }
    drop(execution);
    owned.assert_exited().await;
    assert_once(&root.0);
    assert!(!root.0.join("release").exists());
}

#[tokio::test]
async fn real_helper_timeout_kills_descendants() {
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        stopped_execution("timeout", kind).await;
    }
}

#[tokio::test]
async fn real_helper_cancellation_kills_descendants() {
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        stopped_execution("cancel", kind).await;
    }
}

#[tokio::test]
async fn real_helper_dropped_future_kills_descendants() {
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        stopped_execution("drop", kind).await;
    }
}

#[test]
fn parent_fixture() {
    let Some(path) = std::env::var_os("BOB_NATIVE_PARENT_REQUEST") else {
        return;
    };
    let request: ShellRequest = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let helper = helper_path().unwrap();
    let (_sender, receiver) = watch::channel(false);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let outcome = tokio::time::timeout(
                EXECUTION_WAIT,
                execute(&helper, &hash(&helper), request, receiver),
            )
            .await
            .unwrap()
            .unwrap();
            panic!("parent fixture should be terminated while execute is active: {outcome:?}");
        });
}

#[tokio::test]
async fn real_helper_hard_parent_death_kills_descendants_without_reexecution() {
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        hard_parent_death(kind).await;
    }
}

async fn hard_parent_death(kind: ShellKind) {
    let root = TestDir::new();
    let helper = helper_path().expect("prebuild bob-exec before Windows helper acceptance");
    let request = request(&root.0, 0, kind);
    let request_path = root.0.join("request.json");
    std::fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();
    let mut parent = Fixture(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", PARENT_FIXTURE, "--nocapture"])
            .env("BOB_NATIVE_PARENT_REQUEST", &request_path)
            .env("BOB_EXEC_HELPER", &helper)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut owned = Processes::default();
    owned.ready(&root.0).await;
    owned.assert_images(&helper, &request.executable);
    assert!(
        parent.0.try_wait().unwrap().is_none(),
        "fixture parent exited before hard kill"
    );
    parent.0.kill().unwrap();
    assert!(!parent.wait(EXIT_WAIT).success());
    owned.assert_exited().await;
    assert_once(&root.0);
    assert!(!root.0.join("release").exists());
}

#[tokio::test]
async fn real_helper_hard_death_kills_descendants_without_reexecution() {
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        let root = TestDir::new();
        let helper = helper_path().unwrap();
        let helper_hash = hash(&helper);
        let request = request(&root.0, 0, kind);
        let (_sender, receiver) = watch::channel(false);
        let mut owned = Processes::default();
        let mut execution = Box::pin(execute(&helper, &helper_hash, request, receiver));
        tokio::select! {
            _ = owned.ready(&root.0) => {},
            result = &mut execution => panic!("helper exited before hard kill: {result:?}"),
        }
        assert_ne!(
            unsafe { TerminateProcess(owned.0[0].handle.as_raw_handle(), 73) },
            0
        );
        let outcome = tokio::time::timeout(EXECUTION_WAIT, &mut execution)
            .await
            .unwrap()
            .unwrap();
        assert!(outcome.is_error && outcome.interrupted, "{}", outcome.text);
        owned.assert_exited().await;
        assert_once(&root.0);
    }
}

#[tokio::test]
async fn real_helper_invalid_images_have_zero_effects() {
    for kind in [ShellKind::Pwsh, ShellKind::Bash] {
        for mode in ["missing", "corrupt", "helper_changed", "shell_changed"] {
            let root = TestDir::new();
            let mut request = request(&root.0, 0, kind);
            let actual = helper_path().unwrap();
            let helper = match mode {
                "missing" | "corrupt" => root.0.join("invalid-helper.exe"),
                _ => actual.clone(),
            };
            if mode == "corrupt" {
                std::fs::write(&helper, b"not an executable").unwrap();
            }
            let helper_hash = match mode {
                "missing" | "helper_changed" => "0".repeat(64),
                _ => hash(&helper),
            };
            if mode == "shell_changed" {
                request.executable_sha256 = "0".repeat(64);
            }
            let (_sender, receiver) = watch::channel(false);
            let result = tokio::time::timeout(
                EXECUTION_WAIT,
                execute(&helper, &helper_hash, request, receiver),
            )
            .await
            .unwrap();
            assert!(
                result.as_ref().map_or(true, |outcome| outcome.is_error),
                "{mode}: {result:?}"
            );
            assert!(!root.0.join("executions").exists(), "{mode} ran user work");
            assert!(!root.0.join("branch.executions").exists());
        }
    }
}

fn console_input() -> Option<OwnedHandle> {
    let name: Vec<u16> = "CONIN$\0".encode_utf16().collect();
    let raw = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    (raw != INVALID_HANDLE_VALUE).then(|| unsafe { OwnedHandle::from_raw_handle(raw) })
}

#[test]
fn console_probe_fixture() {
    let Some(expected) = std::env::var_os("BOB_NATIVE_CONSOLE_PROBE") else {
        return;
    };
    let attached = console_input().is_some();
    println!("console probe stdout");
    eprintln!("console probe stderr");
    assert_eq!(
        attached,
        expected == "attached",
        "piped stdio must not imply console isolation"
    );
}

#[test]
fn console_parent_fixture() {
    if std::env::var_os("BOB_NATIVE_CONSOLE_PARENT").is_none() {
        return;
    }
    assert!(
        console_input().is_some(),
        "private fixture console must be available"
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let executable = std::env::current_exe().unwrap();
            let image = image::Image::bind(&executable, &hash(&executable)).unwrap();
            for isolated in [false, true] {
                let mut command = image.command().unwrap();
                configure_helper_launch(&mut command);
                if !isolated {
                    command.creation_flags(0);
                }
                command
                    .args(["--exact", CONSOLE_PROBE_FIXTURE, "--nocapture"])
                    .env(
                        "BOB_NATIVE_CONSOLE_PROBE",
                        if isolated { "isolated" } else { "attached" },
                    )
                    .kill_on_drop(true);
                let output = tokio::time::timeout(EXIT_WAIT, command.output())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "console isolation={isolated}: {} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(String::from_utf8_lossy(&output.stdout).contains("console probe stdout"));
                assert!(String::from_utf8_lossy(&output.stderr).contains("console probe stderr"));
            }
        });
}

#[test]
fn helper_launch_cannot_open_parent_console_input() {
    // Allocate a private console in a fixture, never detach or alter the test runner's console.
    let mut parent = Fixture(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CONSOLE_PARENT_FIXTURE, "--nocapture"])
            .env("BOB_NATIVE_CONSOLE_PARENT", "1")
            .creation_flags(CREATE_NEW_CONSOLE)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    assert!(
        parent.wait(Duration::from_secs(30)).success(),
        "native console isolation fixture failed"
    );
}
