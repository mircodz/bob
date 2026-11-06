pub use crate::native_fs as filesystem;
#[cfg(test)]
#[path = "native_worker_contention_tests.rs"]
mod contention_tests;
#[path = "native_image.rs"]
mod image;

use anyhow::{bail, ensure, Context, Result};
use bob_core::core::process::{
    bounded_output_observer, CapturedOutput, ManagedProcess, OutputChunk, OutputObserver,
    OutputStream,
};
use bob_core::core::shell::{
    invocation_args, CommandShell, ShellKind, ShellSelection, PWSH_VERSION_CHECK,
};
use bob_kernel::{ToolOutcome, ToolProgress, ToolResultPresentation};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[cfg(all(test, unix))]
use sha2::{Digest, Sha256};
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, watch};

const STREAM_LIMIT: usize = 256 * 1024;
const REQUEST_LIMIT: usize = 128 * 1024;
const WIRE_LIMIT: usize = 8 * 1024 * 1024;
const PROGRESS_CHUNK_LIMIT: usize = 8192;
const PROGRESS_FRAME_LIMIT: usize = 512;
const PROGRESS_TEXT_LIMIT: usize = 2 * 1024 * 1024;
const PROGRESS_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
pub const CONTAINMENT: &str = "Shell cwd is workspace-bound, not a filesystem sandbox. Unix uses a separate worker monitoring parent liveness, with owned process-group cleanup on parent death; descendants that deliberately leave that group can escape, and killing the worker itself can prevent cleanup. Windows starts a normal pipe-monitored worker that joins a kill-on-close Job Object before creating children; children inherit containment at creation. Executable bytes are bound using a private verified copy on Unix and no-write/no-delete sharing handles on Windows. This does not sandbox a malicious same-user process, dynamic libraries, shell startup files, or commands launched by the shell. Unix private executable copies require a root-owned, non-writable or sticky /tmp hierarchy (checked before creation); hard parent death may leave a helper copy for manual cleanup.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellRequest {
    pub workspace: PathBuf,
    pub workspace_identity: serde_json::Value,
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub dialect: String,
    pub command: String,
    pub timeout_seconds: Option<f64>,
}

impl ShellRequest {
    fn validate(&self) -> Result<ShellKind> {
        ensure!(
            self.workspace.is_absolute(),
            "worker workspace must be absolute"
        );
        ensure!(
            self.executable.is_absolute(),
            "worker shell must be absolute"
        );
        ensure!(
            !self.command.is_empty()
                && self.command.len() <= 16 * 1024
                && !self.command.contains('\0'),
            "invalid worker command"
        );
        ensure!(
            self.timeout_seconds.is_none_or(|seconds| seconds > 0.0
                && Duration::try_from_secs_f64(seconds).is_ok_and(|duration| {
                    duration
                        .checked_add(STARTUP_TIMEOUT + CLEANUP_TIMEOUT)
                        .and_then(|duration| Instant::now().checked_add(duration))
                        .is_some()
                })),
            "invalid worker timeout"
        );
        let kind = match self.dialect.as_str() {
            "Bash" => ShellKind::Bash,
            "PowerShell 7" => ShellKind::Pwsh,
            _ => bail!("unknown shell dialect"),
        };
        invocation_args(&self.executable, kind, &self.command)?;
        Ok(kind)
    }
}

pub fn discover_shell(
    root: &Path,
    configured: Option<&ShellSelection>,
) -> Result<(PathBuf, ShellKind)> {
    let shell = CommandShell::resolve(root, configured);
    let executable = shell
        .executable()
        .map_err(|error| anyhow::anyhow!(error.to_owned()))?;
    Ok((executable.to_path_buf(), shell.kind()))
}

pub fn helper_path() -> Result<PathBuf> {
    if let Some(configured) = std::env::var_os("BOB_EXEC_HELPER") {
        let path = PathBuf::from(configured);
        ensure!(
            path.is_absolute(),
            "BOB_EXEC_HELPER must be an absolute path"
        );
        return checked_helper(&path).context("resolve configured bob-exec helper");
    }
    let executable = std::env::current_exe()?;
    let directory = executable
        .parent()
        .context("current executable has no parent")?;
    let directory = if directory.file_name().is_some_and(|name| name == "deps") {
        directory
            .parent()
            .context("test executable has no profile directory")?
    } else {
        directory
    };
    let name = if cfg!(windows) {
        "bob-exec.exe"
    } else {
        "bob-exec"
    };
    checked_helper(&directory.join(name)).with_context(|| format!(
        "native worker is missing at {}; build/install bob-exec beside bob, or set absolute BOB_EXEC_HELPER",
        directory.join(name).display()
    ))
}

fn checked_helper(path: &Path) -> Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    let metadata = std::fs::metadata(&path)?;
    ensure!(metadata.is_file(), "native helper is not a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o111 != 0,
            "native helper is not executable"
        );
    }
    Ok(path)
}

pub async fn execute(
    helper: &Path,
    helper_sha256: &str,
    request: ShellRequest,
    cancel: watch::Receiver<bool>,
) -> Result<ToolOutcome> {
    execute_with_progress(helper, helper_sha256, request, cancel, None).await
}

pub async fn execute_with_progress(
    helper: &Path,
    helper_sha256: &str,
    request: ShellRequest,
    mut cancel: watch::Receiver<bool>,
    progress: Option<ToolProgress>,
) -> Result<ToolOutcome> {
    if cancelled_now(&cancel) {
        return Ok(not_started());
    }
    request.validate()?;
    let timeout = request
        .timeout_seconds
        .map(|seconds| Duration::from_secs_f64(seconds) + STARTUP_TIMEOUT + CLEANUP_TIMEOUT);
    let mut payload = serde_json::to_vec(&request)?;
    ensure!(
        payload.len() < REQUEST_LIMIT,
        "worker request exceeds protocol limit"
    );
    payload.push(b'\n');
    let helper_image = image::Image::bind(helper, helper_sha256)?;
    let mut command = helper_image.command()?;
    configure_helper_launch(&mut command);
    let child = command.spawn().context("spawn native shell worker")?;
    let mut worker = Worker::new(child);
    let sent = tokio::select! {
        biased;
        _ = bob_kernel::cancelled(&mut cancel) => false,
        result = tokio::time::timeout(STARTUP_TIMEOUT, async {
            worker.control.as_mut().context("worker stdin unavailable")?.write_all(&payload).await?;
            Ok::<_, anyhow::Error>(())
        }) => matches!(result, Ok(Ok(()))),
    };
    let result = if sent {
        tokio::select! {
            biased;
            _ = bob_kernel::cancelled(&mut cancel) => {
                worker.control.take();
                worker.finish(Some(CLEANUP_TIMEOUT), None).await
            }
            result = worker.finish(timeout, progress.as_ref()) => result,
        }
    } else {
        worker.control.take();
        worker.finish(Some(CLEANUP_TIMEOUT), None).await
    };
    worker.control.take();
    match result {
        Ok((status, outcome, _)) if status.success() => Ok(outcome),
        Ok((status, _, stderr)) => Ok(unknown(format!(
            "worker exited {status}; shell effects require inspection; stderr: {}",
            String::from_utf8_lossy(&stderr)
        ))),
        Err(error) => {
            worker.control.take();
            let cleanup = worker.cleanup().await;
            Ok(unknown(format!(
                "worker completion failed: {error:#}; cleanup: {}",
                describe_finish(cleanup)
            )))
        }
    }
}

fn configure_helper_launch(command: &mut Command) {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false);
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
}

#[cfg(all(test, windows))]
#[path = "native_worker_windows_tests.rs"]
mod windows_tests;

fn describe_finish(result: Result<(ExitStatus, Vec<u8>)>) -> String {
    match result {
        Ok((status, stderr)) => format!("{status}; {}", String::from_utf8_lossy(&stderr)),
        Err(error) => {
            format!("{error:#}; worker hard-stop fallback armed after liveness grace period")
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireStream {
    Stdout,
    Stderr,
}

impl WireStream {
    fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case", deny_unknown_fields)]
enum WorkerFrame {
    Progress { stream: WireStream, text: String },
    Final { outcome: ToolOutcome },
    Error { message: String },
}

struct ProtocolReader<R> {
    source: Option<R>,
    buffer: Vec<u8>,
    scanned: usize,
    total: usize,
    progress_frames: usize,
    progress_bytes: usize,
    progress_lost: bool,
    outcome: Option<ToolOutcome>,
}

impl<R: AsyncRead + Unpin> ProtocolReader<R> {
    fn new(source: Option<R>) -> Self {
        Self {
            source,
            buffer: Vec::new(),
            scanned: 0,
            total: 0,
            progress_frames: 0,
            progress_bytes: 0,
            progress_lost: false,
            outcome: None,
        }
    }

    #[cfg(test)]
    async fn drain(&mut self, progress: Option<&ToolProgress>) -> Result<()> {
        self.drain_with(&mut |text| async move {
            if let Some(progress) = progress {
                progress.append(text).await?;
            }
            Ok(())
        })
        .await
    }

    #[cfg(test)]
    async fn drain_with<F, Fut>(&mut self, append: &mut F) -> Result<()>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        self.collect_with(None, async { Ok(()) }, append).await
    }

    async fn collect_with<F, Fut, C, T>(
        &mut self,
        timeout: Option<Duration>,
        collection: C,
        append: &mut F,
    ) -> Result<T>
    where
        F: FnMut(String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
        C: std::future::Future<Output = Result<T>>,
    {
        let (tx, mut rx) = mpsc::channel(PROGRESS_FRAME_LIMIT + 2);
        let mut lost = self.progress_lost;
        let forward = !lost;
        let transport = async {
            tokio::select! {
                result = async {
                    let ((), collected) = tokio::try_join!(
                        self.drain_frames(forward.then_some(tx)),
                        collection,
                    )?;
                    Ok(collected)
                } => result,
                _ = deadline(timeout) => bail!("worker did not finish within cleanup deadline"),
            }
        };
        let forwarding = async {
            while let Some(text) = rx.recv().await {
                if !lost && append(text).await.is_err() {
                    lost = true;
                }
            }
            Ok::<_, anyhow::Error>(lost)
        };
        let (collected, lost) = tokio::try_join!(transport, forwarding)?;
        self.progress_lost = lost;
        Ok(collected)
    }

    async fn drain_frames(&mut self, progress: Option<mpsc::Sender<String>>) -> Result<()> {
        loop {
            if let Some(relative) = self.buffer[self.scanned..].iter().position(|b| *b == b'\n') {
                let end = self.scanned + relative;
                ensure!(end <= WIRE_LIMIT, "worker frame exceeds protocol limit");
                ensure!(
                    self.outcome.is_none(),
                    "worker sent data after terminal frame"
                );
                let frame = serde_json::from_slice::<WorkerFrame>(&self.buffer[..end]);
                self.buffer.drain(..=end);
                self.scanned = 0;
                match frame.context("invalid worker protocol frame")? {
                    WorkerFrame::Progress { stream, text } => {
                        self.progress_frames += 1;
                        self.progress_bytes += text.len();
                        ensure!(
                            text.len() <= PROGRESS_CHUNK_LIMIT
                                && self.progress_frames <= PROGRESS_FRAME_LIMIT + 2
                                && self.progress_bytes <= PROGRESS_TEXT_LIMIT,
                            "worker progress exceeds protocol limit"
                        );
                        if let Some(progress) = &progress {
                            // Cancellation may lose buffered progress or an append acknowledgment.
                            // Clear this only after the sequential forwarder has settled.
                            self.progress_lost = true;
                            progress
                                .try_send(format!("{}:\n{text}", stream.label()))
                                .context("worker progress forwarding queue unavailable")?;
                        }
                    }
                    WorkerFrame::Final { outcome } => self.outcome = Some(outcome),
                    WorkerFrame::Error { message } => bail!("worker reported error: {message}"),
                }
                continue;
            }
            self.scanned = self.buffer.len();
            ensure!(
                self.buffer.len() <= WIRE_LIMIT,
                "worker frame exceeds protocol limit"
            );
            let Some(source) = &mut self.source else {
                ensure!(
                    self.buffer.is_empty(),
                    "worker returned an incomplete protocol frame"
                );
                ensure!(
                    self.outcome.is_some(),
                    "worker EOF without terminal outcome; effects unknown"
                );
                return Ok(());
            };
            let mut chunk = [0u8; 8192];
            let n = source.read(&mut chunk).await?;
            if n == 0 {
                self.source = None;
                continue;
            }
            self.total += n;
            ensure!(
                self.total <= WIRE_LIMIT + 4 * PROGRESS_TEXT_LIMIT,
                "worker transport exceeds protocol limit"
            );
            self.buffer.extend_from_slice(&chunk[..n]);
        }
    }

    fn take_outcome(&mut self) -> Result<ToolOutcome> {
        let mut outcome = self
            .outcome
            .take()
            .context("worker omitted terminal outcome")?;
        if self.progress_lost {
            let warning = "Shell progress lost: forwarding failed or was interrupted; final capture retained.";
            let presentation = outcome.presentation.get_or_insert_with(Default::default);
            presentation.summary = Some(match presentation.summary.take() {
                Some(summary) => format!("{summary}\n{warning}"),
                None => warning.into(),
            });
        }
        Ok(outcome)
    }

    async fn discard(&mut self) -> Result<()> {
        self.buffer.clear();
        Reader::new(self.source.take(), 0).drain().await
    }
}

struct Reader<R> {
    source: Option<R>,
    bytes: Vec<u8>,
    limit: usize,
    overflow: bool,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    fn new(source: Option<R>, limit: usize) -> Self {
        Self {
            source,
            bytes: Vec::new(),
            limit,
            overflow: false,
        }
    }
    async fn drain(&mut self) -> Result<()> {
        if let Some(source) = &mut self.source {
            let mut buf = [0u8; 8192];
            loop {
                let n = source.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                let keep = n.min(self.limit.saturating_sub(self.bytes.len()));
                self.bytes.extend_from_slice(&buf[..keep]);
                self.overflow |= keep < n;
            }
        }
        self.source = None;
        Ok(())
    }
}

struct Worker {
    child: Option<Child>,
    control: Option<ChildStdin>,
    stdout: ProtocolReader<ChildStdout>,
    stderr: Reader<ChildStderr>,
}

impl Worker {
    fn new(mut child: Child) -> Self {
        let stdout = ProtocolReader::new(child.stdout.take());
        let stderr = Reader::new(child.stderr.take(), STREAM_LIMIT);
        let control = child.stdin.take();
        Self {
            child: Some(child),
            control,
            stdout,
            stderr,
        }
    }
    async fn cleanup(&mut self) -> Result<(ExitStatus, Vec<u8>)> {
        self.control.take();
        let child = self.child.as_mut().context("worker already collected")?;
        let (status, (), ()) = tokio::time::timeout(CLEANUP_TIMEOUT, async {
            tokio::try_join!(
                async { child.wait().await.map_err(anyhow::Error::from) },
                self.stdout.discard(),
                self.stderr.drain(),
            )
        })
        .await
        .context("worker cleanup deadline elapsed")??;
        self.child.take();
        Ok((status, std::mem::take(&mut self.stderr.bytes)))
    }

    async fn finish(
        &mut self,
        timeout: Option<Duration>,
        progress: Option<&ToolProgress>,
    ) -> Result<(ExitStatus, ToolOutcome, Vec<u8>)> {
        let child = self.child.as_mut().context("worker already collected")?;
        let (status, ()) = self
            .stdout
            .collect_with(
                timeout,
                async {
                    tokio::try_join!(
                        async { child.wait().await.map_err(anyhow::Error::from) },
                        self.stderr.drain(),
                    )
                },
                &mut |text| async move {
                    if let Some(progress) = progress {
                        progress.append(text).await?;
                    }
                    Ok(())
                },
            )
            .await?;
        self.child.take();
        Ok((
            status,
            self.stdout.take_outcome()?,
            std::mem::take(&mut self.stderr.bytes),
        ))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.control.take();
        if let Some(mut child) = self.child.take() {
            // Closing liveness comes first. Killing the worker first would orphan its shell group.
            std::thread::spawn(move || {
                let deadline = Instant::now() + CLEANUP_TIMEOUT;
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) | Err(_) => break,
                        Ok(None) => {
                            if Instant::now() >= deadline {
                                let _ = child.start_kill();
                            }
                            std::thread::sleep(Duration::from_millis(20));
                        }
                    }
                }
            });
        }
    }
}

#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut text = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let end = consumed + error.valid_up_to();
                    text.push_str(
                        std::str::from_utf8(&self.pending[consumed..end])
                            .expect("validated prefix"),
                    );
                    consumed = end;
                    match error.error_len() {
                        Some(length) => {
                            text.push('\u{fffd}');
                            consumed += length;
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        text
    }
}

#[derive(Default)]
struct ProgressEncoder {
    decoders: [Utf8Decoder; 2],
    omitted: [u64; 2],
    frames: usize,
}

impl ProgressEncoder {
    async fn observe<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        chunk: OutputChunk,
    ) -> Result<()> {
        let index = match chunk.stream {
            OutputStream::Stdout => 0,
            OutputStream::Stderr => 1,
        };
        if self.omitted[index] != 0 || self.frames + 4 > PROGRESS_FRAME_LIMIT - 4 {
            self.omitted[index] += chunk.bytes.len() as u64;
            return Ok(());
        }
        let text = self.decoders[index].push(&chunk.bytes);
        self.emit(writer, index, &text).await
    }

    async fn emit<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        index: usize,
        mut text: &str,
    ) -> Result<()> {
        while !text.is_empty() {
            let mut end = text.len().min(PROGRESS_CHUNK_LIMIT);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            write_frame(
                writer,
                &WorkerFrame::Progress {
                    stream: if index == 0 {
                        WireStream::Stdout
                    } else {
                        WireStream::Stderr
                    },
                    text: text[..end].to_owned(),
                },
            )
            .await?;
            self.frames += 1;
            text = &text[end..];
        }
        Ok(())
    }

    async fn finish<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        core_omitted: [u64; 2],
    ) -> Result<()> {
        for (index, core_omitted) in core_omitted.into_iter().enumerate() {
            let mut omitted = core_omitted + self.omitted[index];
            let pending = std::mem::take(&mut self.decoders[index].pending);
            if omitted == 0 {
                self.emit(writer, index, &String::from_utf8_lossy(&pending))
                    .await?;
            } else {
                omitted += pending.len() as u64;
                self.emit(writer, index, &format!("\n[progress truncated: {omitted} source bytes omitted; final capture is authoritative]\n")).await?;
            }
        }
        Ok(())
    }
}

async fn forward_progress<W: AsyncWrite + Unpin>(
    writer: &mut W,
    mut receiver: mpsc::Receiver<OutputChunk>,
) -> Result<ProgressEncoder> {
    let mut encoder = ProgressEncoder::default();
    while let Some(chunk) = receiver.recv().await {
        encoder.observe(writer, chunk).await?;
    }
    Ok(encoder)
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &WorkerFrame) -> Result<()> {
    let mut payload = serde_json::to_vec(frame)?;
    ensure!(
        payload.len() <= WIRE_LIMIT,
        "worker frame exceeds protocol limit"
    );
    payload.push(b'\n');
    tokio::time::timeout(PROGRESS_WRITE_TIMEOUT, async {
        writer.write_all(&payload).await?;
        writer.flush().await
    })
    .await
    .context("worker protocol writer stalled")??;
    Ok(())
}

pub async fn worker_main() -> Result<()> {
    #[cfg(windows)]
    join_worker_job()?;
    let mut input = std::io::BufReader::new(std::io::stdin());
    let mut payload = Vec::new();
    Read::by_ref(&mut input)
        .take((REQUEST_LIMIT + 1) as u64)
        .read_until(b'\n', &mut payload)?;
    ensure!(
        payload.len() <= REQUEST_LIMIT && payload.last() == Some(&b'\n'),
        "worker request is oversized or incomplete"
    );
    let request: ShellRequest = serde_json::from_slice(&payload)?;
    let (sender, mut cancel) = watch::channel(false);
    let liveness = sender.clone();
    std::thread::spawn(move || {
        let mut byte = [0u8; 1];
        let _ = input.read(&mut byte);
        let _ = liveness.send(true);
    });
    let (observer, receiver) = bounded_output_observer(32, STREAM_LIMIT);
    let mut stdout = tokio::io::stdout();
    let (run, stream) = tokio::join!(
        async move {
            let outcome = run_owned(
                request,
                bob_kernel::cancelled(&mut cancel),
                Some(observer.clone()),
            )
            .await;
            let omitted = observer.omitted_bytes();
            drop(observer);
            (outcome, omitted)
        },
        async {
            let result = forward_progress(&mut stdout, receiver).await;
            if result.is_err() {
                let _ = sender.send(true);
            }
            result
        },
    );
    let mut stream = stream?;
    stream.finish(&mut stdout, run.1).await?;
    let frame = match run.0 {
        Ok(outcome) => WorkerFrame::Final { outcome },
        Err(error) => WorkerFrame::Error {
            message: format!("{error:#}"),
        },
    };
    write_frame(&mut stdout, &frame).await
}

fn cancelled_now(cancel: &watch::Receiver<bool>) -> bool {
    *cancel.borrow() || cancel.has_changed().is_err()
}

fn not_started() -> ToolOutcome {
    ToolOutcome {
        text: "cancelled before shell startup; no command executed".into(),
        is_error: true,
        interrupted: false,
        presentation: None,
    }
}

fn unknown(text: String) -> ToolOutcome {
    ToolOutcome {
        text,
        is_error: true,
        interrupted: true,
        presentation: None,
    }
}

async fn deadline(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending().await,
    }
}

async fn run_owned(
    request: ShellRequest,
    stop: impl std::future::Future<Output = ()>,
    observer: Option<OutputObserver>,
) -> Result<ToolOutcome> {
    let kind = request.validate()?;
    let workspace = filesystem::Directory::open(&request.workspace)?;
    ensure!(
        filesystem::identity(workspace.file())? == request.workspace_identity,
        "worker workspace identity changed since approval"
    );
    let executable = image::Image::bind(&request.executable, &request.executable_sha256)?;
    tokio::pin!(stop);
    tokio::select! {
        biased;
        _ = &mut stop => return Ok(not_started()),
        _ = std::future::ready(()) => {}
    }
    if kind == ShellKind::Pwsh {
        let mut command = executable.command()?;
        let args = invocation_args(
            Path::new(command.as_std().get_program()),
            kind,
            PWSH_VERSION_CHECK,
        )?;
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        bind_cwd(&mut command, &workspace)?;
        let mut probe = ManagedProcess::spawn(&mut command).with_context(|| {
            format!(
                "PowerShell 7 executable '{}' failed approved version probe startup",
                request.executable.display()
            )
        })?;
        let output = tokio::select! {
            biased;
            _ = &mut stop => {
                probe.terminate_and_collect(4096).await?;
                return Ok(unknown("cancelled during approved shell version probe; user command was not started".into()));
            }
            result = tokio::time::timeout(STARTUP_TIMEOUT, probe.collect_output(4096)) => match result {
                Ok(output) => output?,
                Err(_) => {
                    probe.terminate_and_collect(4096).await?;
                    return Ok(unknown("approved shell version probe timed out; user command was not started".into()));
                }
            },
        };
        ensure!(
            output.status.success(),
            "PowerShell 7 executable '{}' failed approved version validation (status {}); PowerShell Core 7 or later is required; user command was not started; probe stderr: {}",
            request.executable.display(),
            output.status,
            output.stderr.text()
        );
    }
    tokio::select! {
        biased;
        _ = &mut stop => return Ok(not_started()),
        _ = std::future::ready(()) => {}
    }
    let mut command = executable.command()?;
    let args = invocation_args(
        Path::new(command.as_std().get_program()),
        kind,
        &request.command,
    )?;
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    bind_cwd(&mut command, &workspace)?;
    let mut child = ManagedProcess::spawn(&mut command).with_context(|| {
        format!(
            "{} executable '{}' failed managed shell startup",
            kind.name(),
            request.executable.display()
        )
    })?;
    if let Some(observer) = observer {
        child.observe_output(observer);
    }
    let result = async {
        let (output, reason) = tokio::select! {
            biased;
            _ = &mut stop => (child.terminate_and_collect(STREAM_LIMIT).await?, "cancelled"),
            result = child.collect_output(STREAM_LIMIT) => {
                match result {
                    Ok(output) => (output, "exited"),
                    Err(error) => {
                        let output = child.terminate_and_collect(STREAM_LIMIT).await
                            .with_context(|| format!("shell collection failed: {error}; cleanup failed"))?;
                        return Ok::<_, anyhow::Error>(unknown(format!("shell collection failed: {error}\n{}", shell_text(&output.stdout, &output.stderr, &output.status, "collection_error"))));
                    }
                }
            }
            _ = deadline(request.timeout_seconds.map(Duration::from_secs_f64)) => (child.terminate_and_collect(STREAM_LIMIT).await?, "timed_out"),
        };
        Ok(ToolOutcome {
            text: shell_text(&output.stdout, &output.stderr, &output.status, reason),
            is_error: reason != "exited" || !output.status.success(),
            interrupted: reason != "exited",
            presentation: Some(ToolResultPresentation {
                summary: (reason != "exited").then(|| format!("Shell {reason}; outcome uncertain")),
                body_preview: Some(shell_preview(&output.stdout, &output.stderr)),
                exit_code: output.status.code(),
            }),
        })
    }.await;
    if let Err(error) = workspace.verify() {
        return Ok(unknown(format!("workspace path changed during shell execution: {error:#}; shell effects occurred in the bound directory; collected result: {}", result.map(|outcome| outcome.text).unwrap_or_else(|error| format!("{error:#}")))));
    }
    Ok(result.unwrap_or_else(|error| {
        unknown(format!(
            "shell cleanup failed after startup: {error:#}; effects may be partial or unknown"
        ))
    }))
}

fn bind_cwd(command: &mut Command, workspace: &filesystem::Directory) -> Result<()> {
    workspace.verify()?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let directory = workspace.file().try_clone()?;
        unsafe {
            command.pre_exec(move || {
                if libc::fchdir(directory.as_raw_fd()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    command.current_dir(workspace.path());
    Ok(())
}

#[cfg(windows)]
fn join_worker_job() -> Result<()> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};
    use windows_sys::Win32::System::{JobObjects::*, Threading::GetCurrentProcess};
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    ensure!(
        !raw.is_null(),
        "create worker job: {}",
        std::io::Error::last_os_error()
    );
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    ensure!(
        unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } != 0,
        "configure worker job: {}",
        std::io::Error::last_os_error()
    );
    ensure!(
        unsafe { AssignProcessToJobObject(raw, GetCurrentProcess()) } != 0,
        "join worker job before child creation: {}",
        std::io::Error::last_os_error()
    );
    // The OS closes this non-inheritable handle at worker exit, after the response is flushed.
    let _ = handle.into_raw_handle();
    Ok(())
}

fn shell_preview(stdout: &CapturedOutput, stderr: &CapturedOutput) -> String {
    let mut preview = String::new();
    let mut shortened = false;
    for (label, output) in [("", stdout), ("stderr:\n", stderr)] {
        let text = output.text();
        if text.is_empty() {
            continue;
        }
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str(label);
        let mut end = text.len().min(900);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        preview.push_str(&text[..end]);
        shortened |= end < text.len() || output.truncated() || output.incomplete;
    }
    if shortened {
        preview.push_str("\n… preview; inspect retained output");
    }
    preview
}

fn shell_text(
    stdout: &CapturedOutput,
    stderr: &CapturedOutput,
    status: &ExitStatus,
    reason: &str,
) -> String {
    let stream = |value: &CapturedOutput| {
        json!({
            "text":value.text(), "total_bytes":value.total_bytes, "retained_bytes":value.bytes.len(),
            "truncated":value.truncated(), "incomplete":value.incomplete,
        })
    };
    #[cfg(unix)]
    let (signal, core_dumped) = {
        use std::os::unix::process::ExitStatusExt;
        (status.signal(), status.core_dumped())
    };
    #[cfg(not(unix))]
    let (signal, core_dumped): (Option<i32>, bool) = (None, false);
    json!({"outcome":reason, "exit_code":status.code(), "signal":signal, "core_dumped":core_dumped, "stdout":stream(stdout), "stderr":stream(stderr), "containment_limitation":CONTAINMENT}).to_string()
}

#[derive(Deserialize)]
struct DisplayStream {
    text: String,
    truncated: bool,
    incomplete: bool,
}

#[derive(Deserialize)]
struct DisplayShell {
    stdout: DisplayStream,
    stderr: DisplayStream,
    signal: Option<i32>,
    core_dumped: bool,
}

pub(crate) fn display_output(text: &str) -> Result<String> {
    let output: DisplayShell = serde_json::from_str(text)?;
    let mut body = String::new();
    for (name, stream) in [("stdout", output.stdout), ("stderr", output.stderr)] {
        if !stream.text.is_empty() {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            if name == "stderr" {
                body.push_str("stderr:\n");
            }
            body.push_str(&stream.text);
        }
        if stream.truncated || stream.incomplete {
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&format!(
                "[!] {name} {}{}\n",
                if stream.truncated { "truncated" } else { "" },
                if stream.incomplete {
                    " collection incomplete"
                } else {
                    ""
                }
            ));
        }
    }
    if let Some(signal) = output.signal {
        if !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&format!(
            "signal {signal}{}",
            if output.core_dumped {
                "; core dumped"
            } else {
                ""
            }
        ));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn final_frame() -> WorkerFrame {
        WorkerFrame::Final {
            outcome: ToolOutcome {
                text: "authoritative final capture".into(),
                is_error: false,
                interrupted: false,
                presentation: None,
            },
        }
    }

    #[test]
    fn unicode_decoder_carries_arbitrary_chunk_boundaries() {
        let input = "a界🦀é\n".repeat(20000);
        for chunk_size in [1, 2, 3, 7, 8191, 8192] {
            let mut decoder = Utf8Decoder::default();
            let mut actual = String::new();
            for chunk in input.as_bytes().chunks(chunk_size) {
                actual.push_str(&decoder.push(chunk));
                assert!(decoder.pending.len() <= 3);
            }
            assert_eq!(actual, input, "chunk size {chunk_size}");
            assert!(decoder.pending.is_empty());
        }
    }

    #[tokio::test]
    async fn protocol_delivers_both_streams_before_terminal_frame() {
        let (reader, mut writer) = tokio::io::duplex(64);
        let (seen, gate) = tokio::sync::oneshot::channel();
        let mut seen = Some(seen);
        let mut output = Vec::new();
        let producer = async move {
            write_frame(
                &mut writer,
                &WorkerFrame::Progress {
                    stream: WireStream::Stdout,
                    text: "first界".into(),
                },
            )
            .await
            .unwrap();
            write_frame(
                &mut writer,
                &WorkerFrame::Progress {
                    stream: WireStream::Stderr,
                    text: "error🦀".into(),
                },
            )
            .await
            .unwrap();
            gate.await.unwrap();
            write_frame(&mut writer, &final_frame()).await.unwrap();
        };
        let mut reader = ProtocolReader::new(Some(reader));
        let mut append = |text| {
            output.push(text);
            if output.len() == 2 {
                let _ = seen.take().unwrap().send(());
            }
            std::future::ready(Ok(()))
        };
        let consumer = reader.drain_with(&mut append);
        let (_, result) = tokio::time::timeout(Duration::from_secs(3), async {
            tokio::join!(producer, consumer)
        })
        .await
        .unwrap();
        result.unwrap();
        assert_eq!(output, ["stdout:\nfirst界", "stderr:\nerror🦀"]);
        assert_eq!(reader.outcome.unwrap().text, "authoritative final capture");
    }

    #[tokio::test]
    async fn encoded_large_unicode_and_binary_chunks_respect_sink_limit() {
        let input = "a界🦀z".repeat(10000);
        let mut wire = Vec::new();
        let mut encoder = ProgressEncoder::default();
        for stream in [OutputStream::Stdout, OutputStream::Stderr] {
            for chunk in input.as_bytes().chunks(8191) {
                encoder
                    .observe(
                        &mut wire,
                        OutputChunk {
                            stream,
                            bytes: chunk.to_vec(),
                        },
                    )
                    .await
                    .unwrap();
            }
        }
        encoder.finish(&mut wire, [0, 0]).await.unwrap();
        write_frame(&mut wire, &final_frame()).await.unwrap();
        let mut actual = [String::new(), String::new()];
        let mut reader = ProtocolReader::new(Some(wire.as_slice()));
        reader
            .drain_with(&mut |text| {
                assert!(text.len() < 16 * 1024);
                let (label, body) = text.split_once('\n').unwrap();
                actual[if label == "stdout:" { 0 } else { 1 }].push_str(body);
                std::future::ready(Ok(()))
            })
            .await
            .unwrap();
        assert_eq!(actual, [input.clone(), input]);
        let mut wire = Vec::new();
        encoder
            .observe(
                &mut wire,
                OutputChunk {
                    stream: OutputStream::Stdout,
                    bytes: vec![255; 8192],
                },
            )
            .await
            .unwrap();
        write_frame(&mut wire, &final_frame()).await.unwrap();
        ProtocolReader::new(Some(wire.as_slice()))
            .drain(None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn progress_suppression_is_explicit_and_final_is_authoritative() {
        let mut wire = Vec::new();
        let mut encoder = ProgressEncoder::default();
        encoder
            .observe(
                &mut wire,
                OutputChunk {
                    stream: OutputStream::Stderr,
                    bytes: vec![0xe7],
                },
            )
            .await
            .unwrap();
        for _ in 0..2000 {
            encoder
                .observe(
                    &mut wire,
                    OutputChunk {
                        stream: OutputStream::Stdout,
                        bytes: b"x".to_vec(),
                    },
                )
                .await
                .unwrap();
        }
        let omitted = encoder.omitted[0];
        assert!(omitted > 0);
        encoder.finish(&mut wire, [19, 2]).await.unwrap();
        write_frame(&mut wire, &final_frame()).await.unwrap();
        let mut preview = String::new();
        let mut reader = ProtocolReader::new(Some(wire.as_slice()));
        reader
            .drain_with(&mut |text| {
                preview.push_str(&text);
                std::future::ready(Ok(()))
            })
            .await
            .unwrap();
        assert!(reader.progress_frames <= PROGRESS_FRAME_LIMIT);
        assert!(preview.contains(&format!("{} source bytes omitted", omitted + 19)));
        assert!(preview.contains("3 source bytes omitted"));
        assert!(!preview.contains('\u{fffd}'));
        assert_eq!(reader.outcome.unwrap().text, "authoritative final capture");
    }

    #[tokio::test]
    async fn failed_progress_sink_retains_buffered_final_once() {
        for (is_error, interrupted, summary) in [
            (false, false, None),
            (true, true, Some("Shell cancelled; outcome uncertain")),
        ] {
            let text = json!({
                "stdout": {"text": "complete stdout界".repeat(10000), "truncated": false, "incomplete": false},
                "stderr": {"text": "complete stderr🦀".repeat(10000), "truncated": false, "incomplete": false},
                "outcome": if interrupted { "cancelled" } else { "exited" },
                "exit_code": 0
            }).to_string();
            let mut wire = Vec::new();
            for stream in [WireStream::Stdout, WireStream::Stderr] {
                write_frame(
                    &mut wire,
                    &WorkerFrame::Progress {
                        stream,
                        text: "preview".into(),
                    },
                )
                .await
                .unwrap();
            }
            write_frame(
                &mut wire,
                &WorkerFrame::Final {
                    outcome: ToolOutcome {
                        text: text.clone(),
                        is_error,
                        interrupted,
                        presentation: Some(ToolResultPresentation {
                            summary: summary.map(str::to_owned),
                            body_preview: Some("original preview".into()),
                            exit_code: Some(0),
                        }),
                    },
                },
            )
            .await
            .unwrap();
            let mut reader = ProtocolReader::<&[u8]>::new(None);
            reader.buffer = wire;
            let mut attempts = 0;
            reader
                .drain_with(&mut |_| {
                    attempts += 1;
                    std::future::ready(Err(anyhow::anyhow!("sink failed {}", "x".repeat(10000))))
                })
                .await
                .unwrap();
            assert_eq!(attempts, 1);
            assert_eq!(reader.progress_frames, 2);
            let outcome = reader.take_outcome().unwrap();
            assert_eq!(outcome.text, text);
            assert_eq!(outcome.is_error, is_error);
            assert_eq!(outcome.interrupted, interrupted);
            let parsed: serde_json::Value = serde_json::from_str(&outcome.text).unwrap();
            assert!(parsed["stdout"]["text"]
                .as_str()
                .unwrap()
                .ends_with("stdout界"));
            assert!(parsed["stderr"]["text"]
                .as_str()
                .unwrap()
                .ends_with("stderr🦀"));
            let presentation = outcome.presentation.unwrap();
            assert_eq!(
                presentation.body_preview.as_deref(),
                Some("original preview")
            );
            assert_eq!(presentation.exit_code, Some(0));
            let warning = presentation.summary.unwrap();
            assert!(warning.contains("progress lost"));
            assert!(warning.len() < 200);
            if let Some(summary) = summary {
                assert!(warning.starts_with(summary));
            }
            assert!(reader.take_outcome().is_err());
        }
    }

    #[tokio::test]
    async fn failed_progress_sink_does_not_mask_bad_framing() {
        let mut wire = Vec::new();
        write_frame(
            &mut wire,
            &WorkerFrame::Progress {
                stream: WireStream::Stdout,
                text: "preview".into(),
            },
        )
        .await
        .unwrap();
        wire.extend_from_slice(b"invalid frame\n");
        let mut reader = ProtocolReader::new(Some(wire.as_slice()));
        let error = reader
            .drain_with(&mut |_| std::future::ready(Err(anyhow::anyhow!("sink failed"))))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid worker protocol frame"));
        assert!(reader.outcome.is_none());
    }

    #[tokio::test]
    async fn protocol_rejects_malformed_missing_duplicate_and_oversized_frames() {
        let mut final_bytes = Vec::new();
        write_frame(&mut final_bytes, &final_frame()).await.unwrap();
        let mut oversized = Vec::new();
        write_frame(
            &mut oversized,
            &WorkerFrame::Progress {
                stream: WireStream::Stdout,
                text: "x".repeat(PROGRESS_CHUNK_LIMIT + 1),
            },
        )
        .await
        .unwrap();
        let inputs = [
            Vec::new(),
            b"not json\n".to_vec(),
            b"{\"frame\":\"progress\",\"stream\":\"stdout\",\"text\":\"partial\"}\n".to_vec(),
            final_bytes[..final_bytes.len() - 1].to_vec(),
            [final_bytes.as_slice(), final_bytes.as_slice()].concat(),
            [final_bytes.as_slice(), b"junk\n"].concat(),
            b"{\"frame\":\"error\",\"message\":\"fixture failure\"}\n".to_vec(),
            oversized,
            vec![b'x'; WIRE_LIMIT + 1],
        ];
        for input in inputs {
            let mut reader = ProtocolReader::new(Some(input.as_slice()));
            assert!(reader.drain(None).await.is_err());
            assert!(reader.buffer.len() <= WIRE_LIMIT + 8192);
        }
    }

    #[tokio::test]
    async fn cancelled_progress_wait_resumes_terminal_collection() {
        let mut wire = Vec::new();
        write_frame(
            &mut wire,
            &WorkerFrame::Progress {
                stream: WireStream::Stdout,
                text: "pending".into(),
            },
        )
        .await
        .unwrap();
        write_frame(
            &mut wire,
            &WorkerFrame::Progress {
                stream: WireStream::Stderr,
                text: "must not forward after cancellation".into(),
            },
        )
        .await
        .unwrap();
        write_frame(&mut wire, &final_frame()).await.unwrap();
        let mut reader = ProtocolReader::new(Some(wire.as_slice()));
        let mut attempts = 0;
        assert!(tokio::time::timeout(
            Duration::from_millis(10),
            reader.drain_with(&mut |_| {
                attempts += 1;
                std::future::pending::<Result<()>>()
            })
        )
        .await
        .is_err());
        assert_eq!(attempts, 1);
        assert!(reader.progress_lost);
        reader
            .drain_with(&mut |_| -> std::future::Ready<Result<()>> {
                panic!("cancelled progress must not be replayed")
            })
            .await
            .unwrap();
        let outcome = reader.take_outcome().unwrap();
        assert_eq!(outcome.text, "authoritative final capture");
        assert!(outcome
            .presentation
            .unwrap()
            .summary
            .unwrap()
            .contains("progress lost"));
    }

    #[tokio::test(start_paused = true)]
    async fn protocol_writer_reports_parent_disconnect_and_stall() {
        let (reader, mut writer) = tokio::io::duplex(1);
        drop(reader);
        assert!(write_frame(&mut writer, &final_frame()).await.is_err());
        let (_reader, mut writer) = tokio::io::duplex(1);
        assert!(write_frame(&mut writer, &final_frame())
            .await
            .unwrap_err()
            .to_string()
            .contains("stalled"));
    }

    #[test]
    fn retained_display_keeps_full_streams_and_capture_warnings() {
        let stdout = "界".repeat(10000);
        let raw = json!({"stdout":{"text":stdout,"truncated":false,"incomplete":false},"stderr":{"text":"","truncated":true,"incomplete":true},"signal":9,"core_dumped":false}).to_string();
        let display = display_output(&raw).unwrap();
        assert!(display.starts_with(&stdout));
        assert!(display.contains("stderr truncated collection incomplete"));
        assert!(display.ends_with("signal 9"));
        assert!(!display.contains("\"stdout\""));
        assert!(display_output("plain startup error").is_err());
    }

    #[tokio::test]
    async fn reader_drains_but_never_exceeds_the_protocol_bound() {
        let bytes = vec![b'x'; 20000];
        let mut reader = Reader::new(Some(bytes.as_slice()), 17);
        reader.drain().await.unwrap();
        assert_eq!(reader.bytes, vec![b'x'; 17]);
        assert!(reader.overflow);
        assert!(reader.source.is_none());
    }

    #[test]
    fn pwsh_discovery_does_not_execute_candidate() {
        let root = std::env::temp_dir().join(format!("bob-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        let candidate = root.join("pwsh.exe");
        std::fs::write(
            &candidate,
            b"not an executable; discovery must not probe it",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let selection = ShellSelection {
            dialect: ShellKind::Pwsh,
            executable: candidate.clone(),
        };
        assert_eq!(
            discover_shell(&root, Some(&selection)).unwrap(),
            (candidate, ShellKind::Pwsh)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    mod windows {
        use super::*;
        use std::os::windows::process::CommandExt;

        struct Fixture(std::process::Child);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        #[test]
        fn job_worker_fixture() {
            let Some(path) = std::env::var_os("BOB_JOB_FIXTURE") else {
                return;
            };
            join_worker_job().unwrap();
            let child = Fixture(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            std::fs::write(path, child.0.id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(60));
        }

        #[test]
        fn worker_death_kills_child_suspended_before_secondary_job_assignment() {
            use std::os::windows::io::{FromRawHandle, OwnedHandle};
            use windows_sys::Win32::{
                Foundation::WAIT_OBJECT_0,
                System::Threading::{OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
            };
            let path = std::env::temp_dir().join(format!("bob-job-child-{}", uuid::Uuid::new_v4()));
            let mut worker = Fixture(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "native_worker::tests::windows::job_worker_fixture",
                        "--nocapture",
                    ])
                    .env("BOB_JOB_FIXTURE", &path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let deadline = Instant::now() + Duration::from_secs(10);
            let pid = loop {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    if let Ok(pid) = text.parse::<u32>() {
                        break pid;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "worker did not publish suspended child PID"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
            assert!(
                !raw.is_null(),
                "open suspended child: {}",
                std::io::Error::last_os_error()
            );
            let _handle = unsafe { OwnedHandle::from_raw_handle(raw) };
            worker.0.kill().unwrap();
            worker.0.wait().unwrap();
            assert_eq!(
                unsafe { WaitForSingleObject(raw, 10_000) },
                WAIT_OBJECT_0,
                "suspended child survived worker death"
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use bob_core::core::shell::CommandShell;

        struct TestDir(PathBuf);
        impl TestDir {
            fn new() -> Self {
                let root =
                    std::env::temp_dir().join(format!("bob-worker-{}", uuid::Uuid::new_v4()));
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
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        fn helper_hash(path: &Path) -> String {
            format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
        }

        fn request(root: &Path) -> ShellRequest {
            let shell = CommandShell::resolve(root, None);
            let executable = std::fs::canonicalize(shell.executable().unwrap()).unwrap();
            let bytes = std::fs::read(&executable).unwrap();
            ShellRequest {
                workspace: root.into(),
                workspace_identity: filesystem::identity(
                    filesystem::Directory::open(root).unwrap().file(),
                )
                .unwrap(),
                executable,
                executable_sha256: format!("{:x}", Sha256::digest(bytes)),
                dialect: shell.kind().name().into(),
                timeout_seconds: Some(60.0),
                command: "sleep 60 & printf '%s %s %s\\n' \"$PPID\" \"$$\" \"$!\" > pids; wait"
                    .into(),
            }
        }

        #[tokio::test]
        async fn owned_shell_progress_unlocks_completion_and_preserves_final_capture() {
            let root = TestDir::new();
            let mut request = request(&root.0);
            request.timeout_seconds = Some(5.0);
            request.command = "printf first; printf error >&2; while [ ! -e release ]; do sleep 0.01; done; printf final; printf x >> executions".into();
            let (observer, mut receiver) = bounded_output_observer(16, STREAM_LIMIT);
            let run = run_owned(request, std::future::pending(), Some(observer));
            let progress = async {
                let mut streams = [Vec::new(), Vec::new()];
                while let Some(chunk) = receiver.recv().await {
                    let index = if chunk.stream == OutputStream::Stdout {
                        0
                    } else {
                        1
                    };
                    streams[index].extend(chunk.bytes);
                    if streams[0] == b"first" && streams[1] == b"error" {
                        std::fs::write(root.0.join("release"), b"go").unwrap();
                    }
                }
                streams
            };
            let (outcome, streams) = tokio::time::timeout(Duration::from_secs(8), async {
                tokio::join!(run, progress)
            })
            .await
            .unwrap();
            let outcome = outcome.unwrap();
            assert!(!outcome.is_error, "{}", outcome.text);
            assert_eq!(streams, [b"firstfinal".to_vec(), b"error".to_vec()]);
            let value: serde_json::Value = serde_json::from_str(&outcome.text).unwrap();
            assert_eq!(value["stdout"]["text"], "firstfinal");
            assert_eq!(value["stderr"]["text"], "error");
            assert_eq!(std::fs::read(root.0.join("executions")).unwrap(), b"x");
        }

        #[tokio::test]
        async fn execute_wrapper_and_optional_progress_each_execute_once() {
            let root = TestDir::new();
            let helper = helper_path().expect("build bob-exec before native worker tests");
            let hash = helper_hash(&helper);
            let mut request = request(&root.0);
            request.command = "printf x >> executions; printf output; printf error >&2".into();
            let (_sender, cancel) = watch::channel(false);
            let first = execute(&helper, &hash, request.clone(), cancel.clone())
                .await
                .unwrap();
            assert!(!first.is_error, "{}", first.text);
            assert_eq!(std::fs::read(root.0.join("executions")).unwrap(), b"x");
            let second = execute_with_progress(&helper, &hash, request, cancel, None)
                .await
                .unwrap();
            assert!(!second.is_error, "{}", second.text);
            assert_eq!(std::fs::read(root.0.join("executions")).unwrap(), b"xx");
            assert_eq!(first.text, second.text);
        }

        #[tokio::test]
        async fn worker_rejects_replaced_root_before_command_creation() {
            let root = TestDir::new();
            let workspace = root.0.join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let mut request = request(&workspace);
            request.command = "touch must-not-exist".into();
            std::fs::rename(&workspace, root.0.join("moved")).unwrap();
            std::fs::create_dir(&workspace).unwrap();
            let result = run_owned(request, std::future::pending(), None).await;
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("workspace identity"));
            assert!(!workspace.join("must-not-exist").exists());
            assert!(!root.0.join("moved/must-not-exist").exists());
        }

        #[tokio::test]
        async fn cwd_handle_survives_path_replacement_at_spawn() {
            let root = TestDir::new();
            let workspace_path = root.0.join("workspace");
            let moved = root.0.join("moved");
            let outside = root.0.join("outside");
            std::fs::create_dir(&workspace_path).unwrap();
            std::fs::create_dir(&outside).unwrap();
            let workspace = filesystem::Directory::open(&workspace_path).unwrap();
            let request = request(&workspace_path);
            let image =
                image::Image::bind(&request.executable, &request.executable_sha256).unwrap();
            let mut command = image.command().unwrap();
            command.args(["-c", "printf bound > marker"]);
            bind_cwd(&mut command, &workspace).unwrap();
            std::fs::rename(&workspace_path, &moved).unwrap();
            std::os::unix::fs::symlink(&outside, &workspace_path).unwrap();
            assert!(command.status().await.unwrap().success());
            assert_eq!(
                std::fs::read_to_string(moved.join("marker")).unwrap(),
                "bound"
            );
            assert!(!outside.join("marker").exists());
            assert!(workspace.verify().is_err());
        }

        async fn pids(root: &Path) -> Vec<i32> {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(root.join("pids")) {
                        let pids: Vec<i32> = text
                            .split_whitespace()
                            .filter_map(|n| n.parse().ok())
                            .collect();
                        if pids.len() == 3 {
                            break pids;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("worker shell and descendant should publish PIDs")
        }

        fn stopped(pid: i32) -> bool {
            if unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                return true;
            }
            #[cfg(target_os = "linux")]
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                if stat
                    .rsplit_once(") ")
                    .is_some_and(|(_, fields)| fields.starts_with('Z'))
                {
                    return true;
                }
            }
            false
        }

        async fn assert_stopped(pids: &[i32]) {
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                while !pids.iter().copied().all(stopped) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            if result.is_err() {
                for pid in pids {
                    unsafe {
                        libc::kill(*pid, libc::SIGKILL);
                    }
                }
            }
            assert!(result.is_ok(), "worker/shell/descendant survived: {pids:?}");
        }

        #[test]
        fn parent_fixture() {
            let Some(root) = std::env::var_os("BOB_PARENT_DEATH_FIXTURE") else {
                return;
            };
            let root = PathBuf::from(root);
            let helper = helper_path().unwrap();
            let (_sender, receiver) = watch::channel(false);
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let _ = execute(&helper, &helper_hash(&helper), request(&root), receiver).await;
                });
        }

        #[tokio::test]
        async fn worker_cleans_group_after_hard_parent_death() {
            let root = TestDir::new();
            let helper = helper_path().expect("build bob-exec before running native worker tests");
            let mut parent = Fixture(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "native_worker::tests::unix::parent_fixture",
                        "--nocapture",
                    ])
                    .env("BOB_PARENT_DEATH_FIXTURE", &root.0)
                    .env("BOB_EXEC_HELPER", helper)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
            let owned = pids(&root.0).await;
            parent.0.kill().unwrap();
            parent.0.wait().unwrap();
            assert_stopped(&owned).await;
        }

        #[tokio::test]
        async fn cancellation_and_dropped_parent_future_allow_worker_cleanup() {
            for abort in [false, true] {
                let root = TestDir::new();
                let helper =
                    helper_path().expect("build bob-exec before running native worker tests");
                let request = request(&root.0);
                let (sender, receiver) = watch::channel(false);
                let task = tokio::spawn(async move {
                    execute(&helper, &helper_hash(&helper), request, receiver).await
                });
                let owned = pids(&root.0).await;
                if abort {
                    task.abort();
                    assert!(task.await.unwrap_err().is_cancelled());
                } else {
                    sender.send(true).unwrap();
                    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap();
                    assert!(outcome.is_error && outcome.interrupted, "{}", outcome.text);
                    let value: serde_json::Value = serde_json::from_str(&outcome.text).unwrap();
                    assert_eq!(value["outcome"], "cancelled");
                }
                assert_stopped(&owned).await;
            }
        }
    }
}
