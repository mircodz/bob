//! Owned child processes with bounded capture and process-tree termination.

use std::io;
use std::process::ExitStatus;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

pub struct ManagedProcess {
    child: Option<Child>,
    tree: ProcessTree,
    stdout_capture: Option<Capture<ChildStdout>>,
    stderr_capture: Option<Capture<ChildStderr>>,
}

impl ManagedProcess {
    /// Owns the process group/job and Windows creation flags (including suspended startup).
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
        command.kill_on_drop(true);
        let mut child = command.spawn()?;
        match ProcessTree::new(&child) {
            Ok(tree) => Ok(Self {
                child: Some(child),
                tree,
                stdout_capture: None,
                stderr_capture: None,
            }),
            Err(error) => {
                let _ = child.start_kill();
                reap(child);
                Err(error)
            }
        }
    }

    pub fn id(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.as_mut()?.stdin.take()
    }

    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.as_mut()?.stdout.take()
    }

    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.as_mut().expect("owned child").wait().await?;
        self.tree.terminate()?;
        Ok(status)
    }

    pub async fn terminate_and_wait(&mut self) -> io::Result<ExitStatus> {
        let tree_result = self.tree.terminate();
        let child = self.child.as_mut().expect("owned child");
        let _ = child.start_kill();
        let result = child.wait().await;
        tree_result?;
        result
    }

    pub async fn terminate_and_collect(&mut self, limit: usize) -> io::Result<ProcessOutput> {
        let status = self.terminate_and_wait().await?;
        match tokio::time::timeout(
            std::time::Duration::from_secs(1),
            self.collect_output(limit),
        )
        .await
        {
            Ok(output) => output,
            // Escaped descendants may retain pipe handles after the owned tree is gone.
            Err(_) => Ok(ProcessOutput {
                status,
                stdout: self.stdout_capture.take().unwrap().finish(),
                stderr: self.stderr_capture.take().unwrap().finish(),
            }),
        }
    }

    /// Cancellation retains reader state; the next collection resumes with the original limit.
    pub async fn collect_output(&mut self, limit: usize) -> io::Result<ProcessOutput> {
        self.take_stdin();
        if self.stdout_capture.is_none() {
            self.stdout_capture = Some(Capture::new(self.take_stdout(), limit));
            self.stderr_capture = Some(Capture::new(self.take_stderr(), limit));
        }
        let child = self.child.as_mut().expect("owned child");
        let tree = &mut self.tree;
        let (status, (), ()) = tokio::try_join!(
            async {
                let status = child.wait().await?;
                tree.terminate()?;
                Ok::<_, io::Error>(status)
            },
            self.stdout_capture.as_mut().unwrap().read(),
            self.stderr_capture.as_mut().unwrap().read(),
        )?;
        Ok(ProcessOutput {
            status,
            stdout: self.stdout_capture.take().unwrap().finish(),
            stderr: self.stderr_capture.take().unwrap().finish(),
        })
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        let _ = self.tree.terminate();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            reap(child);
        }
    }
}

fn reap(mut child: Child) {
    if child.id().is_none() {
        return;
    }
    // Reaping must survive cancellation and teardown of the Tokio runtime.
    std::thread::spawn(move || loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    });
}

pub struct ProcessOutput {
    pub status: ExitStatus,
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
}

pub struct CapturedOutput {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub incomplete: bool,
}

impl CapturedOutput {
    pub fn truncated(&self) -> bool {
        self.incomplete || self.total_bytes > self.bytes.len() as u64
    }

    pub fn text(&self) -> String {
        let mut text = if self.total_bytes <= self.bytes.len() as u64 {
            String::from_utf8_lossy(&self.bytes).into_owned()
        } else {
            let split = self.bytes.len().div_ceil(2);
            format!(
                "{}\n... [{} bytes truncated] ...\n{}",
                String::from_utf8_lossy(&self.bytes[..split]),
                self.total_bytes - self.bytes.len() as u64,
                String::from_utf8_lossy(&self.bytes[split..])
            )
        };
        if self.incomplete {
            text.push_str("\n... [stream closed before EOF] ...");
        }
        text
    }
}

struct Capture<R> {
    reader: Option<R>,
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    limit: usize,
    total_bytes: u64,
}

impl<R: AsyncRead + Unpin> Capture<R> {
    fn new(reader: Option<R>, limit: usize) -> Self {
        Self {
            reader,
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            limit,
            total_bytes: 0,
        }
    }

    async fn read(&mut self) -> io::Result<()> {
        let Some(reader) = &mut self.reader else {
            return Ok(());
        };
        let head_limit = self.limit.div_ceil(2);
        let tail_limit = self.limit / 2;
        let mut chunk = [0u8; 8192];
        loop {
            let n = reader.read(&mut chunk).await?;
            if n == 0 {
                self.reader = None;
                return Ok(());
            }
            self.total_bytes = self.total_bytes.saturating_add(n as u64);
            let head_count = (head_limit - self.head.len()).min(n);
            self.head.extend_from_slice(&chunk[..head_count]);
            for byte in &chunk[head_count..n] {
                if tail_limit == 0 {
                    continue;
                }
                if self.tail.len() == tail_limit {
                    self.tail.pop_front();
                }
                self.tail.push_back(*byte);
            }
        }
    }

    fn finish(mut self) -> CapturedOutput {
        self.head.extend(self.tail);
        CapturedOutput {
            bytes: self.head,
            total_bytes: self.total_bytes,
            incomplete: self.reader.is_some(),
        }
    }
}

#[cfg(test)]
async fn capture<R: AsyncRead + Unpin>(
    reader: Option<R>,
    limit: usize,
) -> io::Result<CapturedOutput> {
    let mut capture = Capture::new(reader, limit);
    capture.read().await?;
    Ok(capture.finish())
}

#[cfg(unix)]
struct ProcessTree {
    group: Option<i32>,
}

#[cfg(unix)]
impl ProcessTree {
    fn new(child: &Child) -> io::Result<Self> {
        Ok(Self {
            group: child.id().map(|pid| pid as i32),
        })
    }

    fn terminate(&mut self) -> io::Result<()> {
        let Some(group) = self.group.take() else {
            return Ok(());
        };
        // The child starts a fresh group; a negative pid targets only that group.
        if unsafe { libc::kill(-group, libc::SIGKILL) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
struct ProcessTree {
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
unsafe impl Send for ProcessTree {}

#[cfg(windows)]
impl ProcessTree {
    fn new(child: &Child) -> io::Result<Self> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::*;
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(job, child.raw_handle().expect("live child")) == 0
            {
                let error = io::Error::last_os_error();
                CloseHandle(job);
                return Err(error);
            }
            let tree = Self { job };
            resume_initial_thread(child.id().expect("live child"))?;
            Ok(tree)
        }
    }

    fn terminate(&mut self) -> io::Result<()> {
        if self.job.is_null() {
            return Ok(());
        }
        if unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn resume_initial_thread(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut found = Thread32First(snapshot, &mut entry) != 0;
        let mut result = Err(io::Error::new(
            io::ErrorKind::NotFound,
            "suspended child thread not found",
        ));
        while found {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    result = Err(io::Error::last_os_error());
                } else {
                    result = if ResumeThread(thread) == u32::MAX {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    };
                    CloseHandle(thread);
                }
                break;
            }
            found = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        result
    }
}

#[cfg(windows)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

#[cfg(not(any(unix, windows)))]
struct ProcessTree;

#[cfg(not(any(unix, windows)))]
impl ProcessTree {
    fn new(_: &Child) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "managed process trees require Unix or Windows",
        ))
    }
    fn terminate(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_capture_keeps_head_tail_and_byte_count() {
        let input = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let output = capture(Some(&input[..]), 10).await.unwrap();
        assert_eq!(output.bytes, b"01234vwxyz");
        assert_eq!(output.total_bytes, input.len() as u64);
        assert!(output.truncated());
        assert!(output.text().contains("26 bytes truncated"));
        let empty = capture(Some(&input[..]), 0).await.unwrap();
        assert!(empty.bytes.is_empty());
        assert_eq!(empty.total_bytes, input.len() as u64);
    }

    #[tokio::test]
    async fn capture_is_bounded_for_large_binary_output() {
        let input = vec![255; 1_000_000];
        let output = capture(Some(&input[..]), 127).await.unwrap();
        assert_eq!(output.bytes.len(), 127);
        assert_eq!(output.total_bytes, 1_000_000);
        assert!(output.text().contains("999873 bytes truncated"));
    }

    struct ChunkedReader<'a> {
        remaining: &'a [u8],
        chunk_size: usize,
        fail_at_end: bool,
    }

    impl AsyncRead for ChunkedReader<'_> {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            if self.remaining.is_empty() && self.fail_at_end {
                return std::task::Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "fixture read failure",
                )));
            }
            let count = self
                .remaining
                .len()
                .min(self.chunk_size)
                .min(buf.remaining());
            buf.put_slice(&self.remaining[..count]);
            self.remaining = &self.remaining[count..];
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn capture_boundaries_do_not_depend_on_read_chunking() {
        for length in [0usize, 1, 2, 9, 8191, 8192, 8193, 16390] {
            let input: Vec<_> = (0..length).map(|i| (i % 251) as u8).collect();
            for limit in [0, 1, 2, 3, 10, 127, length, length + 1] {
                let expected = if length <= limit {
                    input.clone()
                } else {
                    [&input[..limit.div_ceil(2)], &input[length - limit / 2..]].concat()
                };
                for chunk_size in [1, 3, 8192] {
                    let output = capture(
                        Some(ChunkedReader {
                            remaining: &input,
                            chunk_size,
                            fail_at_end: false,
                        }),
                        limit,
                    )
                    .await
                    .unwrap();
                    assert_eq!(
                        output.bytes, expected,
                        "length={length}, limit={limit}, chunk={chunk_size}"
                    );
                    assert_eq!(output.total_bytes, length as u64);
                    assert_eq!(output.truncated(), length > limit);
                }
            }
        }
    }

    #[tokio::test]
    async fn capture_missing_stream_and_read_errors_are_not_confused_with_truncation() {
        let empty = capture::<&[u8]>(None, 10).await.unwrap();
        assert!(empty.bytes.is_empty());
        assert_eq!(empty.total_bytes, 0);
        assert!(!empty.truncated());
        assert_eq!(empty.text(), "");
        for limit in [0, 1, 100] {
            let error = capture(
                Some(ChunkedReader {
                    remaining: b"partial output",
                    chunk_size: 2,
                    fail_at_end: true,
                }),
                limit,
            )
            .await
            .err()
            .expect("read errors must propagate even after the cap");
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
            assert_eq!(error.to_string(), "fixture read failure");
        }
    }

    #[test]
    fn captured_text_reports_exact_loss_and_decodes_binary_lossily() {
        let complete = CapturedOutput {
            bytes: b"a\xffz".to_vec(),
            total_bytes: 3,
            incomplete: false,
        };
        assert_eq!(complete.text(), "a\u{fffd}z");
        let truncated = CapturedOutput {
            bytes: b"abcxy".to_vec(),
            total_bytes: 12,
            incomplete: false,
        };
        assert_eq!(truncated.text(), "abc\n... [7 bytes truncated] ...\nxy");
        let zero = CapturedOutput {
            bytes: vec![],
            total_bytes: 5,
            incomplete: false,
        };
        assert_eq!(zero.text(), "\n... [5 bytes truncated] ...\n");
    }

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::process::Stdio;
        use std::time::Duration;
        use tokio::io::{AsyncBufReadExt, BufReader};

        async fn fixture() -> (ManagedProcess, i32, i32) {
            let mut process = ManagedProcess::spawn(
                Command::new("sh")
                    .arg("-c")
                    .arg("sleep 60 & echo $!; read gate; wait")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped()),
            )
            .unwrap();
            let parent = process.id().unwrap() as i32;
            let mut stdout = BufReader::new(process.take_stdout().unwrap());
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(3), stdout.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            (process, parent, line.trim().parse().unwrap())
        }

        async fn assert_not_running(pid: i32) {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if unsafe { libc::kill(pid, 0) } == -1 {
                        break;
                    }
                    let mut check = ManagedProcess::spawn(
                        Command::new("ps")
                            .args(["-o", "stat=", "-p", &pid.to_string()])
                            .stdout(Stdio::piped()),
                    )
                    .unwrap();
                    let output = check.collect_output(128).await.unwrap();
                    let state = output.stdout.text();
                    // Orphans are reaped by the OS, not by their former grandparent.
                    if !output.status.success() || state.trim_start().starts_with('Z') {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("owned process must no longer execute");
        }

        fn assert_reaped(pid: i32) {
            assert_eq!(
                unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
                -1
            );
            assert_eq!(
                io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }

        #[tokio::test]
        async fn explicit_termination_kills_group_and_reaps_direct_child() {
            let (mut process, parent, descendant) = fixture().await;
            let status = process.terminate_and_wait().await.unwrap();
            assert!(!status.success());
            assert_reaped(parent);
            assert_not_running(descendant).await;
        }

        #[tokio::test]
        async fn dropped_wait_future_kills_group_and_reaps_child() {
            let (process, parent, descendant) = fixture().await;
            let (entered, waiting) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let mut process = process;
                let mut wait = Box::pin(process.wait());
                assert!(futures::poll!(&mut wait).is_pending());
                entered.send(()).unwrap();
                wait.await
            });
            tokio::time::timeout(Duration::from_secs(3), waiting)
                .await
                .unwrap()
                .unwrap();
            task.abort();
            let _ = task.await;
            assert_not_running(parent).await;
            assert_not_running(descendant).await;
            tokio::time::timeout(Duration::from_secs(3), async {
                while unsafe { libc::kill(parent, 0) } == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_reaped(parent);
        }

        #[tokio::test]
        async fn cancelled_collections_resume_without_losing_or_recounting_streams() {
            let mut process = ManagedProcess::spawn(
                Command::new("sh")
                    .args(["-c", "printf before; printf error >&2; sleep 60"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped()),
            )
            .unwrap();
            let pid = process.id().unwrap() as i32;
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let mut collection = Box::pin(process.collect_output(100));
                    assert!(futures::poll!(&mut collection).is_pending());
                    drop(collection);
                    if process.stdout_capture.as_ref().unwrap().total_bytes == 6
                        && process.stderr_capture.as_ref().unwrap().total_bytes == 5
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(20), process.collect_output(100))
                    .await
                    .is_err()
            );
            let output = process.terminate_and_collect(100).await.unwrap();
            assert_eq!(output.stdout.bytes, b"before");
            assert_eq!(output.stdout.total_bytes, 6);
            assert_eq!(output.stderr.bytes, b"error");
            assert_eq!(output.stderr.total_bytes, 5);
            assert!(!output.status.success());
            assert_reaped(pid);
        }

        #[tokio::test]
        async fn termination_bounds_drain_when_an_external_writer_holds_the_pipe() {
            use std::io::Write;
            use std::os::fd::OwnedFd;
            use std::os::unix::net::UnixStream;

            let (reader, mut writer) = UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            writer.write_all(b"partial").unwrap();
            let reader =
                ChildStdout::from_std(std::process::ChildStdout::from(OwnedFd::from(reader)))
                    .unwrap();
            let mut process =
                ManagedProcess::spawn(Command::new("sh").args(["-c", "sleep 60"])).unwrap();
            let pid = process.id().unwrap() as i32;
            process.stdout_capture = Some(Capture::new(Some(reader), 100));
            process.stderr_capture = Some(Capture::new(None, 100));
            let output =
                tokio::time::timeout(Duration::from_secs(3), process.terminate_and_collect(100))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(output.stdout.bytes, b"partial");
            assert_eq!(output.stdout.total_bytes, 7);
            assert!(output.stdout.incomplete);
            assert!(output.stdout.truncated());
            assert!(!output.stderr.incomplete);
            assert!(!output.stderr.truncated());
            assert_reaped(pid);
            drop(writer);
        }

        #[tokio::test]
        async fn collection_timeout_can_terminate_and_reap() {
            let (mut process, parent, descendant) = fixture().await;
            assert!(
                tokio::time::timeout(Duration::from_millis(20), process.wait())
                    .await
                    .is_err()
            );
            process.terminate_and_wait().await.unwrap();
            assert_reaped(parent);
            assert_not_running(descendant).await;
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_descendant_fixture() {
        let Ok(pid_file) = std::env::var("BOB_PROCESS_FIXTURE_PID_FILE") else {
            return;
        };
        if std::env::var_os("BOB_PROCESS_FIXTURE_CHILD").is_some() {
            println!("descendant stdout");
            eprintln!("descendant stderr");
            std::fs::write(pid_file, std::process::id().to_string()).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(60));
        } else {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "core::process::tests::windows_descendant_fixture",
                    "--nocapture",
                ])
                .env("BOB_PROCESS_FIXTURE_CHILD", "1")
                .spawn()
                .unwrap();
            child.wait().unwrap();
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_owned_job_cleans_up_descendants_on_timeout_and_drop() {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };

        fn handle(pid: u32) -> OwnedHandle {
            let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            assert!(
                !raw.is_null(),
                "open fixture process {pid}: {}",
                io::Error::last_os_error()
            );
            unsafe { OwnedHandle::from_raw_handle(raw) }
        }

        async fn assert_exited(handle: &OwnedHandle) {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let mut code = 0;
                    assert_ne!(
                        unsafe { GetExitCodeProcess(handle.as_raw_handle(), &mut code) },
                        0
                    );
                    if code != 259 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("owned descendant must terminate");
        }

        for drop_process in [false, true] {
            let root = crate::test_support::TestDir::new();
            let pid_file = root.join("descendant pid 終");
            let mut process = ManagedProcess::spawn(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "core::process::tests::windows_descendant_fixture",
                        "--nocapture",
                    ])
                    .env("BOB_PROCESS_FIXTURE_PID_FILE", &pid_file)
                    .env_remove("BOB_PROCESS_FIXTURE_CHILD")
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped()),
            )
            .unwrap();
            let parent = handle(process.id().unwrap());
            let descendant_pid = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                loop {
                    if let Ok(text) = std::fs::read_to_string(&pid_file) {
                        if let Ok(pid) = text.parse::<u32>() {
                            break pid;
                        }
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("native descendant should start");
            let descendant = handle(descendant_pid);
            if drop_process {
                drop(process);
            } else {
                assert!(tokio::time::timeout(
                    std::time::Duration::from_millis(20),
                    process.collect_output(4096),
                )
                .await
                .is_err());
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    process.terminate_and_collect(4096),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(!output.status.success());
                assert!(output.stdout.text().contains("descendant stdout"));
                assert!(output.stderr.text().contains("descendant stderr"));
                assert!(!output.stdout.incomplete);
                assert!(!output.stderr.incomplete);
            }
            assert_exited(&parent).await;
            assert_exited(&descendant).await;
        }
    }
}
