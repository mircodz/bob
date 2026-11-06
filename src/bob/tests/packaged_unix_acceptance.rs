#![cfg(any(target_os = "macos", target_os = "linux"))]

use anyhow::{bail, ensure, Context, Result};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const DRIVER: &str = "packaged-unix-test-driver";
const HELPER: &str = "bob-exec";
const EXECUTE_FIXTURE: &str = "fixtures::packaged_helper_fixture";
const MISSING_FIXTURE: &str = "fixtures::packaged_missing_helper_fixture";
const MARKER: &str = "BOB_PRIVATE_PACKAGED_UNIX_FIXTURE";
const ROOT: &str = "BOB_PRIVATE_PACKAGED_UNIX_ROOT";
const CASE: &str = "BOB_PRIVATE_PACKAGED_UNIX_CASE";
const WAIT: Duration = Duration::from_secs(90);
const CLEANUP: Duration = Duration::from_secs(5);
const LOG_LIMIT: u64 = 1024 * 1024;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Result<Self> {
        let path = Path::new("/tmp").join(format!(
            "bob package qualification 終 {}",
            uuid::Uuid::new_v4()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        let root = Self(path.canonicalize()?);
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .context("Bob manifest must be inside the checkout")?;
        let checkout = checkout
            .canonicalize()
            .unwrap_or_else(|_| checkout.to_path_buf());
        ensure!(
            !root.0.starts_with(checkout),
            "staging must be outside checkout"
        );
        Ok(root)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn frontend_name() -> Result<String> {
    let name = std::env::var("BOB_REPLACEMENT_FRONTEND_NAME").unwrap_or_else(|_| "bob".into());
    ensure!(name == "bob", "expected bob");
    Ok(name)
}

fn built_binary(name: &str) -> Result<PathBuf> {
    let variable = format!("CARGO_BIN_EXE_{name}");
    let compiled = match name {
        "bob" => option_env!("CARGO_BIN_EXE_bob"),
        HELPER => option_env!("CARGO_BIN_EXE_bob-exec"),
        _ => unreachable!(),
    };
    if let Some(path) = std::env::var_os(variable).or_else(|| compiled.map(Into::into)) {
        return Ok(PathBuf::from(path));
    }
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .context("test executable has no directory")?;
    let profile = if directory.file_name().is_some_and(|name| name == "deps") {
        directory
            .parent()
            .context("test executable has no profile directory")?
    } else {
        directory
    };
    Ok(profile.join(name))
}

fn stage_executable(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source).with_context(|| {
        format!(
            "prebuild or extract the real executable {}",
            source.display()
        )
    })?;
    ensure!(
        metadata.is_file(),
        "not a regular executable: {}",
        source.display()
    );
    ensure!(
        metadata.permissions().mode() & 0o111 != 0,
        "not executable: {}",
        source.display()
    );
    let mut magic = [0; 4];
    File::open(source)?.read_exact(&mut magic)?;
    ensure!(
        matches!(
            magic,
            [0x7f, b'E', b'L', b'F']
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xfe, 0xed, 0xfa, 0xce]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
                | [0xca, 0xfe, 0xba, 0xbf]
                | [0xbf, 0xba, 0xfe, 0xca]
        ),
        "expected a compiled ELF/Mach-O executable, not a script: {}",
        source.display()
    );
    fs::copy(source, destination)?;
    ensure!(
        fs::metadata(destination)?.len() == metadata.len(),
        "incomplete executable copy"
    );
    Ok(())
}

struct Isolation {
    directories: Vec<PathBuf>,
}

impl Isolation {
    fn new(root: &Path) -> Result<Self> {
        let mut directories = Vec::new();
        for name in ["home", "config", "cache", "data", "state", "runtime", "tmp"] {
            let directory = root.join(name);
            fs::DirBuilder::new().mode(0o700).create(&directory)?;
            directories.push(directory);
        }
        Ok(Self { directories })
    }

    fn command(&self, executable: &Path, cwd: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.directories[0])
            .env("XDG_CONFIG_HOME", &self.directories[1])
            .env("XDG_CACHE_HOME", &self.directories[2])
            .env("XDG_DATA_HOME", &self.directories[3])
            .env("XDG_STATE_HOME", &self.directories[4])
            .env("XDG_RUNTIME_DIR", &self.directories[5])
            .env("TMPDIR", &self.directories[6])
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .env_remove("BOB_EXEC_HELPER")
            .current_dir(cwd)
            .stdin(Stdio::null())
            .process_group(0);
        command
    }

    fn assert_untouched(&self) -> Result<()> {
        for directory in &self.directories {
            ensure!(
                fs::read_dir(directory)?.next().is_none(),
                "unexpected config/auth/cache write in {}",
                directory.display()
            );
        }
        Ok(())
    }
}

struct ChildGuard {
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    fn poll(&mut self) -> Result<Option<ExitStatus>> {
        let status = self.child.try_wait()?;
        self.reaped |= status.is_some();
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // The helper owns a separate group: closing the driver's liveness pipe lets it
        // clean its shell first. Never kill that helper before its cleanup grace period.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let deadline = Instant::now() + CLEANUP;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.reaped = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        std::thread::sleep(CLEANUP);
    }
}

struct Captured {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

impl Captured {
    fn diagnostic(&self) -> String {
        format!(
            "{}\nstdout:\n{}\nstderr:\n{}",
            self.status, self.stdout, self.stderr
        )
    }

    fn success(&self) -> Result<()> {
        ensure!(
            self.status.success() && self.stderr.is_empty(),
            "{}",
            self.diagnostic()
        );
        Ok(())
    }
}

fn log_text(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(LOG_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= LOG_LIMIT,
        "subprocess output exceeded 1 MiB: {}",
        path.display()
    );
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn run(mut command: Command, logs: &Path, label: &str) -> Result<Captured> {
    let stdout = logs.join(format!("{label}.stdout"));
    let stderr = logs.join(format!("{label}.stderr"));
    command
        .stdout(File::create(&stdout)?)
        .stderr(File::create(&stderr)?);
    let mut child = ChildGuard {
        child: command.spawn().with_context(|| format!("spawn {label}"))?,
        reaped: false,
    };
    let deadline = Instant::now() + WAIT;
    let status = loop {
        if let Some(status) = child.poll()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    drop(child);
    let stdout = log_text(&stdout)?;
    let stderr = log_text(&stderr)?;
    let Some(status) = status else {
        bail!("{label} watchdog expired\nstdout:\n{stdout}\nstderr:\n{stderr}");
    };
    Ok(Captured {
        status,
        stdout,
        stderr,
    })
}

fn fixture_args(name: &str) -> [&str; 5] {
    [
        "--ignored",
        "--exact",
        name,
        "--nocapture",
        "--test-threads=1",
    ]
}

fn run_fixture(
    isolation: &Isolation,
    package: &Path,
    workspace: &Path,
    logs: &Path,
    name: &str,
    case: usize,
) -> Result<()> {
    fs::create_dir(workspace)?;
    let mut command = isolation.command(&package.join(DRIVER), package);
    command
        .args(fixture_args(name))
        .env(MARKER, name)
        .env(ROOT, workspace)
        .env(CASE, case.to_string());
    let output = run(command, logs, &format!("fixture-{case}-{name}"))?;
    output.success()?;
    ensure!(
        output.stdout.contains("running 1 test")
            && output.stdout.contains("1 passed; 0 failed")
            && output
                .stdout
                .contains(&format!("PACKAGED_UNIX_OK {name} {case}")),
        "fixture must actually run exactly one guarded test: {}",
        output.diagnostic()
    );
    isolation.assert_untouched()
}

#[test]
fn packaged_unix_frontend_and_helper_acceptance() -> Result<()> {
    let root = TestDir::new()?;
    let package = root.0.join("extracted package 界");
    let logs = root.0.join("logs");
    fs::create_dir(&package)?;
    fs::create_dir(&logs)?;
    let isolation = Isolation::new(&root.0)?;
    let name = frontend_name()?;
    let (frontend, helper) = if let Some(source) = std::env::var_os("BOB_REPLACEMENT_PACKAGE_DIR") {
        let source = PathBuf::from(source)
            .canonicalize()
            .context("open extracted release candidate")?;
        (source.join(&name), source.join(HELPER))
    } else {
        (built_binary(&name)?, built_binary(HELPER)?)
    };
    ensure!(
        frontend
            .file_name()
            .is_some_and(|file| file == name.as_str())
            && helper.file_name().is_some_and(|file| file == HELPER),
        "package inputs must use the exact frontend/helper sibling names"
    );
    ensure!(
        frontend
            .parent()
            .context("frontend directory missing")?
            .canonicalize()?
            == helper
                .parent()
                .context("helper directory missing")?
                .canonicalize()?,
        "frontend and helper must come from the same package/profile directory"
    );
    stage_executable(&frontend, &package.join(&name))?;
    stage_executable(&helper, &package.join(HELPER))?;
    stage_executable(&std::env::current_exe()?, &package.join(DRIVER))?;

    let mut command = isolation.command(&package.join(&name), &package);
    command.arg("--help");
    let help = run(command, &logs, "help")?;
    help.success()?;
    ensure!(
        help.stdout.contains(&format!("Usage: {name}")) && help.stdout.contains("--inspect"),
        "unexpected frontend help: {}",
        help.diagnostic()
    );
    let mut command = isolation.command(&package.join(&name), &package);
    command.arg("--version");
    let version = run(command, &logs, "version")?;
    version.success()?;
    let fields: Vec<_> = version.stdout.split_whitespace().collect();
    ensure!(
        fields.len() == 2
            && fields[0] == name
            && fields[1].chars().next().is_some_and(|c| c.is_ascii_digit()),
        "unexpected frontend version: {}",
        version.diagnostic()
    );
    isolation.assert_untouched()?;

    let database = root.0.join("foreign database 終.sqlite");
    {
        let db = rusqlite::Connection::open(&database)?;
        db.execute_batch("PRAGMA application_id=12345; CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES('preserve 界');")?;
    }
    let before = fs::read(&database)?;
    let mut command = isolation.command(&package.join(&name), &package);
    command
        .args([
            "--inspect",
            "--provider",
            "packaging-must-not-initialize",
            "--database",
        ])
        .arg(&database);
    let rejected = run(command, &logs, "foreign-database")?;
    ensure!(
        !rejected.status.success() && rejected.stdout.is_empty(),
        "foreign database was not rejected: {}",
        rejected.diagnostic()
    );
    ensure!(
        rejected
            .stderr
            .contains("unsupported kernel database format; preserved without modification"),
        "inspection did not reach the offline database guard: {}",
        rejected.diagnostic()
    );
    ensure!(
        fs::read(&database)? == before,
        "inspection changed the foreign database"
    );
    for suffix in ["-wal", "-shm", "-journal", ".memories.sqlite"] {
        let mut path = database.as_os_str().to_os_string();
        path.push(suffix);
        ensure!(
            !Path::new(&path).exists(),
            "inspection created a database sidecar"
        );
    }
    isolation.assert_untouched()?;

    for case in 0..6 {
        run_fixture(
            &isolation,
            &package,
            &root.0.join(format!("workspace {case} 界")),
            &logs,
            EXECUTE_FIXTURE,
            case,
        )?;
    }
    let missing = root.0.join("missing helper package 終");
    fs::create_dir(&missing)?;
    stage_executable(&package.join(&name), &missing.join(&name))?;
    stage_executable(&package.join(DRIVER), &missing.join(DRIVER))?;
    run_fixture(
        &isolation,
        &missing,
        &root.0.join("missing helper workspace 界"),
        &logs,
        MISSING_FIXTURE,
        0,
    )?;
    Ok(())
}

mod fixtures {
    use super::*;
    use async_trait::async_trait;
    use bob::{adapters::ToolService, native_worker};
    use bob_core::core::shell::{ShellKind, ShellSelection};
    use bob_kernel::*;
    use bob_types::{
        Completion, ContentBlock, Message, ReasoningEffort, Role, StopReason, ToolSpec, Usage,
    };
    use serde_json::{json, Value};
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use tokio::sync::watch;

    fn guard(name: &str) -> Result<Option<(PathBuf, PathBuf, usize)>> {
        if std::env::var(MARKER).ok().as_deref() != Some(name)
            || std::env::args().skip(1).collect::<Vec<_>>() != fixture_args(name)
        {
            return Ok(None);
        }
        ensure!(
            std::env::var_os("BOB_EXEC_HELPER").is_none(),
            "helper override is forbidden"
        );
        let executable = std::env::current_exe()?.canonicalize()?;
        ensure!(
            executable.file_name().is_some_and(|name| name == DRIVER),
            "fixture must run from the staged test driver"
        );
        let package = executable
            .parent()
            .context("missing package directory")?
            .to_path_buf();
        ensure!(
            std::env::current_dir()?.canonicalize()? == package,
            "fixture cwd must be the package"
        );
        let root =
            PathBuf::from(std::env::var_os(ROOT).context("private fixture workspace missing")?)
                .canonicalize()?;
        ensure!(
            root.parent() == package.parent() && root != package,
            "fixture workspace must be an isolated sibling of its package"
        );
        let case = std::env::var(CASE)?.parse()?;
        Ok(Some((package, root, case)))
    }

    async fn bounded<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
        tokio::time::timeout(Duration::from_secs(35), future)
            .await
            .context("private fixture watchdog expired")?
    }

    fn service() -> ToolService {
        ToolService::new(
            Some(ShellSelection {
                dialect: ShellKind::Bash,
                executable: PathBuf::from("/bin/bash"),
            }),
            None,
        )
    }

    fn input(background: bool, code: i32) -> Value {
        json!({
            "command": format!("set -eu; IFS= read -r value < 'input 終'; [ \"$value\" = 'cwd 界' ] || exit 92; printf x >> effect; printf 'stdout 界'; printf 'stderr 終' >&2; exit {code}"),
            "timeout": 10.0,
            "run_in_background": background
        })
    }

    struct Driver {
        tools: ToolService,
        next: Mutex<Option<Value>>,
        expected: Value,
        workspace: String,
        helper: PathBuf,
        prepared: Mutex<usize>,
        executed: Mutex<usize>,
        allowed: Mutex<Option<OperationId>>,
        outcomes: Mutex<Vec<ToolOutcome>>,
    }

    #[async_trait]
    impl Executor for Driver {
        fn models(&self) -> Vec<ModelInfo> {
            vec![ModelInfo {
                id: "synthetic-packaging".into(),
                context_window: 128_000,
            }]
        }

        fn tools(&self) -> Vec<ToolDescriptor> {
            vec![ToolDescriptor {
                effect: EffectClass::Write,
                spec: ToolSpec {
                    name: "bash".into(),
                    description: "Private packaging fixture only".into(),
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
            let (block, stop_reason) = match self.next.lock().unwrap().take() {
                Some(input) => (
                    ContentBlock::ToolUse {
                        id: uuid::Uuid::new_v4().to_string(),
                        name: "bash".into(),
                        input,
                    },
                    StopReason::ToolUse,
                ),
                None => (
                    ContentBlock::Text {
                        text: "Private packaging fixture complete.".into(),
                    },
                    StopReason::EndTurn,
                ),
            };
            Ok(Completion {
                message: Message {
                    role: Role::Assistant,
                    content: vec![block],
                },
                stop_reason,
                usage: Usage::default(),
            })
        }

        async fn prepare(&self, invocation: Invocation) -> Result<PreparedTool> {
            ensure!(
                invocation.name == "bash"
                    && invocation.input == self.expected
                    && invocation.workspace == self.workspace,
                "unexpected fixture invocation"
            );
            let prepared = self.tools.prepare(invocation).await?;
            let helper: PathBuf =
                serde_json::from_value(prepared.specification["action"]["helper"]["path"].clone())?;
            ensure!(
                helper == self.helper,
                "preparation did not bind the exact staged helper"
            );
            *self.prepared.lock().unwrap() += 1;
            Ok(prepared)
        }

        async fn execute(
            &self,
            prepared: PreparedTool,
            cancel: watch::Receiver<bool>,
        ) -> Result<ToolOutcome> {
            *self.executed.lock().unwrap() += 1;
            ensure!(
                self.allowed.lock().unwrap().as_ref() == Some(&prepared.invocation.operation_id),
                "execution without exact fixture approval"
            );
            ensure!(
                prepared.invocation.input == self.expected
                    && prepared.invocation.workspace == self.workspace,
                "approved command changed"
            );
            let outcome = self.tools.execute(prepared, cancel).await?;
            self.outcomes.lock().unwrap().push(outcome.clone());
            Ok(outcome)
        }
    }

    async fn until(
        handle: &Handle,
        thread: &ThreadId,
        predicate: impl Fn(&View) -> bool,
    ) -> Result<View> {
        loop {
            let view = handle.view(Some(thread.clone()), None, 100).await?;
            ensure!(view.error.is_none(), "kernel failure: {:?}", view.error);
            ensure!(
                !view.threads.iter().any(|t| t.id == *thread
                    && matches!(
                        t.state,
                        ThreadState::Failed | ThreadState::Cancelled | ThreadState::Interrupted
                    )),
                "fixture thread failed: {:?}",
                view.threads
            );
            if predicate(&view) {
                return Ok(view);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn exercise(
        handle: &Handle,
        driver: &Driver,
        root: &Path,
        approve: bool,
        background: bool,
        code: i32,
    ) -> Result<()> {
        let receipt = handle
            .command(bob_kernel::Command::CreateSession {
                id: CommandId::new(),
                workspace: driver.workspace.clone(),
                profile: Profile {
                    model: "synthetic-packaging".into(),
                    instructions: "Private synthetic packaging fixture.".into(),
                    read_only: false,
                    max_requests: 8,
                    reasoning: ReasoningEffort::Off,
                },
                policy: Policy::default(),
            })
            .await?;
        handle
            .command(bob_kernel::Command::Submit {
                id: CommandId::new(),
                thread: receipt.thread.clone(),
                text: "Run the single private fixture command.".into(),
            })
            .await?;
        let pending = until(handle, &receipt.thread, |view| !view.decisions.is_empty()).await?;
        ensure!(
            pending.decisions.len() == 1,
            "expected one exact approval decision"
        );
        let decision = &pending.decisions[0];
        let operation = pending
            .operations
            .iter()
            .find(|op| op.id == decision.operation)
            .context("missing pending operation")?;
        ensure!(
            decision.name == "bash" && operation.name == "bash",
            "unexpected approval tool"
        );
        ensure!(
            operation.specification["invocation"]["input"] == driver.expected,
            "approval input differs from fixture command"
        );
        ensure!(
            operation.specification["invocation"]["workspace"] == driver.workspace,
            "approval workspace differs from fixture"
        );
        ensure!(
            *driver.prepared.lock().unwrap() == 1,
            "expected exactly one preparation"
        );
        ensure!(
            !root.join("effect").exists()
                && *driver.executed.lock().unwrap() == 0
                && driver.outcomes.lock().unwrap().is_empty(),
            "command executed before approval"
        );
        if approve {
            *driver.allowed.lock().unwrap() = Some(decision.operation.clone());
        }
        handle
            .command(bob_kernel::Command::Decide {
                id: CommandId::new(),
                operation: decision.operation.clone(),
                revision: decision.revision,
                approve,
            })
            .await?;
        until(handle, &receipt.thread, |view| {
            view.threads
                .iter()
                .any(|t| t.id == receipt.thread && t.state == ThreadState::Completed)
        })
        .await?;
        let jobs = loop {
            let jobs = handle.jobs(receipt.session.clone()).await?;
            if jobs.iter().all(|job| {
                !matches!(
                    job.state,
                    JobState::Queued | JobState::Running | JobState::Stopping
                )
            }) {
                break jobs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let shell_jobs: Vec<_> = jobs
            .iter()
            .filter(|job| job.kind == JobKind::Shell)
            .collect();
        if approve {
            ensure!(
                fs::read(root.join("effect"))? == b"x",
                "command did not execute exactly once in its workspace"
            );
            let outcomes = driver.outcomes.lock().unwrap();
            ensure!(
                outcomes.len() == 1 && *driver.executed.lock().unwrap() == 1,
                "expected one actual helper execution and result"
            );
            let outcome = &outcomes[0];
            ensure!(
                outcome.is_error == (code != 0) && !outcome.interrupted,
                "unexpected helper outcome: {}",
                outcome.text
            );
            let value: Value = serde_json::from_str(&outcome.text)?;
            ensure!(
                value["exit_code"] == code
                    && value["stdout"]["text"] == "stdout 界"
                    && value["stderr"]["text"] == "stderr 終",
                "incorrect stdout/stderr/exit: {}",
                outcome.text
            );
            if background {
                ensure!(
                    shell_jobs.len() == 1,
                    "background execution did not publish exactly one shell job"
                );
                let job = shell_jobs[0];
                ensure!(
                    job.background
                        && job.operation.as_ref() == Some(&decision.operation)
                        && job.result.is_some(),
                    "wrong background shell job"
                );
                ensure!(
                    job.state
                        == if code == 0 {
                            JobState::Completed
                        } else {
                            JobState::Failed
                        },
                    "wrong background exit state: {:?}",
                    job.state
                );
            } else {
                ensure!(
                    shell_jobs.is_empty(),
                    "foreground command unexpectedly became a background job"
                );
            }
        } else {
            ensure!(
                !root.join("effect").exists()
                    && *driver.executed.lock().unwrap() == 0
                    && driver.outcomes.lock().unwrap().is_empty()
                    && shell_jobs.is_empty(),
                "denied command attempted execution, had effects, or launched a job"
            );
        }
        ensure!(
            fs::read(root.join("input 終"))? == "cwd 界\n".as_bytes(),
            "fixture input changed"
        );
        Ok(())
    }

    #[test]
    #[ignore = "private subprocess fixture; only the packaging parent may launch this exact test"]
    fn packaged_helper_fixture() -> Result<()> {
        let Some((package, root, case)) = guard(EXECUTE_FIXTURE)? else {
            return Ok(());
        };
        let (approve, background, code) = match case {
            0 => (false, false, 0),
            1 => (false, true, 0),
            2 => (true, false, 0),
            3 => (true, false, 7),
            4 => (true, true, 0),
            5 => (true, true, 7),
            _ => bail!("unknown private fixture case"),
        };
        ensure!(
            native_worker::helper_path()? == package.join(HELPER),
            "discovery must select the exact helper sibling"
        );
        fs::write(root.join("input 終"), "cwd 界\n")?;
        let expected = input(background, code);
        let driver = Arc::new(Driver {
            tools: service(),
            next: Mutex::new(Some(expected.clone())),
            expected,
            workspace: root.to_str().context("UTF-8 fixture path")?.into(),
            helper: package.join(HELPER),
            prepared: Mutex::new(0),
            executed: Mutex::new(0),
            allowed: Mutex::new(None),
            outcomes: Mutex::new(Vec::new()),
        });
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let handle =
                    bounded(Runtime::open(&root.join("runtime.db"), driver.clone())).await?;
                let result =
                    bounded(exercise(&handle, &driver, &root, approve, background, code)).await;
                let shutdown = tokio::time::timeout(CLEANUP, handle.shutdown())
                    .await
                    .context("kernel shutdown watchdog expired");
                result?;
                shutdown??;
                Ok::<_, anyhow::Error>(())
            })?;
        println!("PACKAGED_UNIX_OK {EXECUTE_FIXTURE} {case}");
        Ok(())
    }

    #[test]
    #[ignore = "private subprocess fixture; only the packaging parent may launch this exact test"]
    fn packaged_missing_helper_fixture() -> Result<()> {
        let Some((package, root, case)) = guard(MISSING_FIXTURE)? else {
            return Ok(());
        };
        ensure!(
            case == 0 && !package.join(HELPER).exists(),
            "missing-helper fixture is not isolated"
        );
        let error = native_worker::helper_path().expect_err("must not find a development helper");
        ensure!(
            format!("{error:#}").contains("native worker is missing"),
            "wrong missing-helper error: {error:#}"
        );
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                for background in [false, true] {
                    let result = bounded(service().prepare(Invocation {
                        operation_id: OperationId::new(),
                        run_id: RunId::new(),
                        thread_id: ThreadId::new(),
                        workspace: root.to_str().context("UTF-8 fixture path")?.into(),
                        name: "bash".into(),
                        input: input(background, 0),
                    }))
                    .await;
                    let error = result
                        .err()
                        .context("preparation succeeded without its helper")?;
                    ensure!(
                        format!("{error:#}").contains("native worker is missing"),
                        "wrong preparation error: {error:#}"
                    );
                    ensure!(
                        fs::read_dir(&root)?.next().is_none(),
                        "missing-helper preparation had effects"
                    );
                }
                Ok::<_, anyhow::Error>(())
            })?;
        println!("PACKAGED_UNIX_OK {MISSING_FIXTURE} {case}");
        Ok(())
    }
}
