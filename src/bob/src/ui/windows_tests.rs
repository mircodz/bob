use super::{state::*, ConsoleInput, InputEvents, TerminalGuard};
use bob_core::core::process::ManagedProcess;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use futures::StreamExt;
use ratatui::{backend::TestBackend, Terminal};
use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::{Path, PathBuf},
    process::Stdio,
    thread::JoinHandle,
    time::{Duration, Instant},
};
use tokio::{task::JoinSet, time::timeout};
use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        Console::{
            ClosePseudoConsole, CreatePseudoConsole, GetConsoleMode, SetConsoleMode, COORD,
            ENABLE_VIRTUAL_TERMINAL_INPUT, HPCON,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Pipes::CreatePipe,
        Threading::*,
    },
};

const INPUT_FIXTURE: &str = "ui::windows_tests::conpty_input_fixture";

#[path = "windows_terminal_tests.rs"]
mod lifecycle;
#[path = "windows_runtime_tests.rs"]
mod runtime;
#[path = "windows_trace_tests.rs"]
mod trace;
const ACCESS_FIXTURE: &str = "ui::windows_tests::console_access_fixture";
const FIXTURE_DIR: &str = "BOB_CONPTY_FIXTURE_DIR";
const CONSOLE_ACCESS: &str = "BOB_CONSOLE_ACCESS";
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

#[test]
fn console_access_fixture() {
    let Ok(expected) = std::env::var(CONSOLE_ACCESS) else {
        return;
    };
    let input = OpenOptions::new().read(true).write(true).open("CONIN$");
    match expected.as_str() {
        "attached" => {
            let input = input.expect("attached child must share the fixture console");
            let mut mode = 0;
            assert_ne!(
                unsafe { GetConsoleMode(input.as_raw_handle(), &mut mode) },
                0
            );
            assert_ne!(
                unsafe {
                    SetConsoleMode(input.as_raw_handle(), mode | ENABLE_VIRTUAL_TERMINAL_INPUT)
                },
                0
            );
        }
        "isolated" => {
            assert!(
                input.is_err(),
                "managed child must not open its parent's CONIN$"
            );
            assert!(
                TerminalGuard::activate().is_err(),
                "a console-less child must not activate the Bob terminal"
            );
        }
        other => panic!("unknown console access expectation: {other}"),
    }
}

async fn drift_from_attached_child() {
    let output = timeout(
        EVENT_TIMEOUT,
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", ACCESS_FIXTURE, "--nocapture", "--test-threads=1"])
            .env_remove(FIXTURE_DIR)
            .env(CONSOLE_ACCESS, "attached")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("attached console probe timed out")
    .unwrap();
    assert!(
        output.status.success(),
        "attached probe: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn assert_managed_child_isolated(input: &ConsoleInput, baseline: u32) {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", ACCESS_FIXTURE, "--nocapture", "--test-threads=1"])
        .env_remove(FIXTURE_DIR)
        .env(CONSOLE_ACCESS, "isolated")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ManagedProcess::spawn(&mut command).unwrap();
    let output = timeout(EVENT_TIMEOUT, child.collect_output(16_384))
        .await
        .expect("managed console probe timed out")
        .unwrap();
    assert!(
        output.status.success(),
        "isolated probe: {}\n{}",
        output.stdout.text(),
        output.stderr.text()
    );
    assert_eq!(
        input.mode().unwrap(),
        baseline,
        "isolated child changed the UI console mode"
    );
}

async fn input_event(events: &mut InputEvents<'_>) -> Event {
    loop {
        let event = events
            .next()
            .await
            .expect("Bob event stream ended")
            .expect("Bob event stream failed");
        match event {
            Event::Resize(..) | Event::FocusGained | Event::FocusLost => {}
            Event::Key(key) if key.kind == KeyEventKind::Release => {}
            _ => return event,
        }
    }
}

fn publish(path: &Path, content: impl AsRef<[u8]>) {
    let pending = path.with_extension("pending");
    std::fs::write(&pending, content).unwrap();
    std::fs::rename(pending, path).unwrap();
}

async fn ready_event(
    events: &mut InputEvents<'_>,
    input: &ConsoleInput,
    baseline: u32,
    marker: &Path,
    content: &[u8],
) -> Event {
    let mut first = Box::pin(input_event(events));
    assert!(
        futures::poll!(&mut first).is_pending(),
        "unexpected input before fixture readiness"
    );
    assert_eq!(
        input.mode().unwrap(),
        baseline,
        "guard.events() did not repair mode drift before waiting"
    );
    publish(marker, content);
    timeout(EVENT_TIMEOUT, first)
        .await
        .expect("native input did not arrive after readiness")
}

fn key(event: Event, code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    let Event::Key(key) = event else {
        panic!("expected {code:?}, received {event:?}")
    };
    assert_eq!(key.code, code);
    assert_eq!(key.modifiers, modifiers);
    assert_eq!(key.kind, KeyEventKind::Press);
    key
}

#[tokio::test]
async fn conpty_input_fixture() {
    let Some(directory) = std::env::var_os(FIXTURE_DIR) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let input = ConsoleInput::open().unwrap();
    let original = input.mode().unwrap() | ENABLE_VIRTUAL_TERMINAL_INPUT;
    input.set_mode(original).unwrap();
    let mut guard = TerminalGuard::activate().unwrap();
    let baseline = input.mode().unwrap();
    assert_eq!(baseline & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
    assert_managed_child_isolated(&input, baseline).await;
    let mut events = guard.events();

    for round in 0..3 {
        match round {
            0 => {}
            1 => input
                .set_mode(baseline | ENABLE_VIRTUAL_TERMINAL_INPUT)
                .unwrap(),
            2 => drift_from_attached_child().await,
            _ => unreachable!(),
        }
        if round != 0 {
            assert_ne!(input.mode().unwrap() & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        }
        let mut app = App::new(80);
        let mut jobs = JoinSet::new();
        app.draft_mut().insert("draft");
        let event = ready_event(
            &mut events,
            &input,
            baseline,
            &directory.join(format!("ready-{round}")),
            b"ready",
        )
        .await;
        assert!(!app.key(
            key(event, KeyCode::Char('a'), KeyModifiers::NONE),
            &mut jobs
        ));
        assert_eq!(app.draft_mut().text, "drafta");

        let enter = key(
            timeout(EVENT_TIMEOUT, input_event(&mut events))
                .await
                .unwrap(),
            KeyCode::Enter,
            KeyModifiers::NONE,
        );
        assert!(!app.key(enter, &mut jobs));
        assert_eq!(app.draft_mut().text, "drafta");
        assert_eq!(app.status, "Not ready; draft preserved. Ctrl+C exits.");
        assert!(jobs.is_empty());
        let mut quit_app = App::new(80);
        quit_app.draft_mut().insert("/quit");
        assert!(
            quit_app.key(enter, &mut jobs),
            "native Enter must dispatch local commands"
        );

        let toggle = key(
            timeout(EVENT_TIMEOUT, input_event(&mut events))
                .await
                .unwrap(),
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
        );
        assert!(!app.sidebar);
        assert!(!app.key(toggle, &mut jobs));
        assert!(app.sidebar);
        assert_eq!(
            app.draft_mut().text,
            "drafta",
            "control input leaked into the draft"
        );

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| super::render::draw(frame, &mut app))
            .unwrap();
        let area = app
            .hits
            .iter()
            .find(|hit| matches!(hit.action, HitAction::Composer))
            .expect("rendered composer hit region")
            .area;
        assert!(area.width > 0 && area.height > 0);
        let column = area.x + area.width / 2;
        let row = area.y + area.height / 2;
        app.focus = Focus::Sidebar;
        let mouse_bytes = format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m\x03",
            column + 1,
            row + 1,
            column + 1,
            row + 1
        );
        let event = ready_event(
            &mut events,
            &input,
            baseline,
            &directory.join(format!("mouse-{round}")),
            mouse_bytes.as_bytes(),
        )
        .await;
        let Event::Mouse(mouse) = event else {
            panic!("expected mouse down, received {event:?}")
        };
        assert_eq!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
        assert_eq!((mouse.column, mouse.row), (column, row));
        app.mouse(mouse, &mut jobs);
        assert_eq!(app.focus, Focus::Composer);
        let event = timeout(EVENT_TIMEOUT, input_event(&mut events))
            .await
            .unwrap();
        let Event::Mouse(mouse) = event else {
            panic!("expected mouse up, received {event:?}")
        };
        assert_eq!(mouse.kind, MouseEventKind::Up(MouseButton::Left));
        assert_eq!((mouse.column, mouse.row), (column, row));
        app.mouse(mouse, &mut jobs);
        assert_eq!(app.focus, Focus::Composer);
        let interrupt = key(
            timeout(EVENT_TIMEOUT, input_event(&mut events))
                .await
                .unwrap(),
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        );
        assert!(
            app.key(interrupt, &mut jobs),
            "Ctrl+C must remain a key event and request exit"
        );
        assert_eq!(app.draft_mut().text, "drafta");
        assert!(jobs.is_empty());
        assert_eq!(input.mode().unwrap(), baseline);
    }
    drop(events);
    guard.restore().unwrap();
    guard.restore().unwrap();
    drop(guard);
    assert_eq!(
        input.mode().unwrap(),
        original,
        "Bob guard did not restore the exact pre-activation mode"
    );
    publish(&directory.join("done"), b"restored");
}

struct FixtureDirectory(PathBuf);
impl FixtureDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("bob-conpty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct PseudoConsole(HPCON);
impl Drop for PseudoConsole {
    fn drop(&mut self) {
        unsafe { ClosePseudoConsole(self.0) };
    }
}

struct FixtureProcess {
    process: OwnedHandle,
    _job: OwnedHandle,
}
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        unsafe {
            TerminateProcess(self.process.as_raw_handle(), 1);
            WaitForSingleObject(self.process.as_raw_handle(), 5000);
        }
    }
}

struct Attributes(Vec<usize>);
impl Drop for Attributes {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.0.as_mut_ptr().cast()) };
    }
}

fn pipe() -> (File, File) {
    let mut reader = std::ptr::null_mut();
    let mut writer = std::ptr::null_mut();
    assert_ne!(
        unsafe { CreatePipe(&mut reader, &mut writer, std::ptr::null(), 0) },
        0,
        "{}",
        io::Error::last_os_error()
    );
    unsafe { (File::from_raw_handle(reader), File::from_raw_handle(writer)) }
}

fn launch_fixture(console: &PseudoConsole, directory: &Path, test: &str) -> FixtureProcess {
    let mut bytes = 0;
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes) };
    assert!(bytes > 0);
    let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
    assert_ne!(
        unsafe { InitializeProcThreadAttributeList(storage.as_mut_ptr().cast(), 1, 0, &mut bytes) },
        0
    );
    let mut attributes = Attributes(storage);
    let attributes_ptr = attributes.0.as_mut_ptr().cast();
    assert_ne!(
        unsafe {
            UpdateProcThreadAttribute(
                attributes_ptr,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                console.0 as *const _,
                std::mem::size_of::<HPCON>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        },
        0
    );
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes_ptr;
    let executable = std::env::current_exe().unwrap();
    let executable_wide: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut command = OsString::from("\"");
    command.push(&executable);
    command.push(format!("\" --exact {test} --nocapture --test-threads=1"));
    let mut command: Vec<u16> = command.encode_wide().chain(Some(0)).collect();
    let mut variables: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| {
            !name.to_string_lossy().eq_ignore_ascii_case(FIXTURE_DIR)
                && !name.to_string_lossy().eq_ignore_ascii_case(CONSOLE_ACCESS)
        })
        .map(|(mut name, value)| {
            name.push("=");
            name.push(value);
            name
        })
        .collect();
    let mut fixture = OsString::from(format!("{FIXTURE_DIR}="));
    fixture.push(directory);
    variables.push(fixture);
    variables.sort_by_key(|entry| entry.to_string_lossy().to_ascii_uppercase());
    let mut environment: Vec<u16> = variables
        .iter()
        .flat_map(|entry| entry.encode_wide().chain(Some(0)))
        .collect();
    environment.push(0);

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    assert!(!job.is_null(), "{}", io::Error::last_os_error());
    let job = unsafe { OwnedHandle::from_raw_handle(job) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    assert_ne!(
        unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        },
        0
    );
    let mut process = PROCESS_INFORMATION::default();
    assert_ne!(
        unsafe {
            CreateProcessW(
                executable_wide.as_ptr(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                environment.as_ptr().cast(),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut process,
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    let thread = unsafe { OwnedHandle::from_raw_handle(process.hThread) };
    let process = FixtureProcess {
        process: unsafe { OwnedHandle::from_raw_handle(process.hProcess) },
        _job: job,
    };
    assert_ne!(
        unsafe {
            AssignProcessToJobObject(
                process._job.as_raw_handle(),
                process.process.as_raw_handle(),
            )
        },
        0,
        "{}",
        io::Error::last_os_error()
    );
    assert_ne!(unsafe { ResumeThread(thread.as_raw_handle()) }, u32::MAX);
    process
}

struct ConptyFixture {
    process: Option<FixtureProcess>,
    console: Option<PseudoConsole>,
    input: Option<File>,
    output: Option<JoinHandle<Vec<u8>>>,
}
impl ConptyFixture {
    fn launch(directory: &Path) -> Self {
        Self::launch_test(directory, INPUT_FIXTURE)
    }

    fn launch_test(directory: &Path, test: &str) -> Self {
        let (input_read, input_write) = pipe();
        let (mut output_read, output_write) = pipe();
        let mut console = 0;
        let result = unsafe {
            CreatePseudoConsole(
                COORD { X: 100, Y: 30 },
                input_read.as_raw_handle(),
                output_write.as_raw_handle(),
                0,
                &mut console,
            )
        };
        assert_eq!(result, 0, "CreatePseudoConsole failed: {result:#x}");
        let console = PseudoConsole(console);
        drop(input_read);
        drop(output_write);
        let mut fixture = Self {
            process: None,
            console: Some(console),
            input: Some(input_write),
            output: Some(std::thread::spawn(move || {
                let mut output = Vec::new();
                let mut buffer = [0; 4096];
                while let Ok(size) = output_read.read(&mut buffer) {
                    if size == 0 {
                        break;
                    }
                    let keep = size.min(32_768usize.saturating_sub(output.len()));
                    output.extend_from_slice(&buffer[..keep]);
                }
                output
            })),
        };
        fixture.process = Some(launch_fixture(
            fixture.console.as_ref().unwrap(),
            directory,
            test,
        ));
        fixture
    }

    fn wait_marker(&self, path: &Path) -> io::Result<()> {
        let started = Instant::now();
        loop {
            if path.is_file() {
                return Ok(());
            }
            if started.elapsed() >= PROCESS_TIMEOUT {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("fixture did not publish {}", path.display()),
                ));
            }
            let state = unsafe {
                WaitForSingleObject(self.process.as_ref().unwrap().process.as_raw_handle(), 0)
            };
            if state != WAIT_TIMEOUT {
                return Err(io::Error::other(format!(
                    "fixture exited before {} (wait status {state:#x})",
                    path.display()
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn exercise(&mut self, directory: &Path) -> io::Result<()> {
        for round in 0..3 {
            self.wait_marker(&directory.join(format!("ready-{round}")))?;
            self.input.as_mut().unwrap().write_all(b"a\r\x14")?;
            let mouse = directory.join(format!("mouse-{round}"));
            self.wait_marker(&mouse)?;
            self.input
                .as_mut()
                .unwrap()
                .write_all(&std::fs::read(mouse)?)?;
        }
        self.wait_exit(directory)
    }

    fn wait_exit(&self, directory: &Path) -> io::Result<()> {
        let process = self.process.as_ref().unwrap().process.as_raw_handle();
        let state = unsafe { WaitForSingleObject(process, PROCESS_TIMEOUT.as_millis() as u32) };
        if state != WAIT_OBJECT_0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("fixture did not exit (wait status {state:#x})"),
            ));
        }
        let mut exit_code = 0;
        if unsafe { GetExitCodeProcess(process, &mut exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if exit_code != 0 {
            return Err(io::Error::other(format!("fixture exit {exit_code}")));
        }
        if std::fs::read(directory.join("done"))? != b"restored" {
            return Err(io::Error::other(
                "fixture did not confirm original-mode restoration",
            ));
        }
        Ok(())
    }

    fn close(&mut self) -> Vec<u8> {
        drop(self.process.take());
        // Keep draining while ClosePseudoConsole flushes the console output pipe.
        drop(self.console.take());
        drop(self.input.take());
        self.output
            .take()
            .map(|thread| thread.join().unwrap_or_default())
            .unwrap_or_default()
    }
}
impl Drop for ConptyFixture {
    fn drop(&mut self) {
        self.close();
    }
}

#[test]
fn conpty_mode_drift_preserves_lab_keys_mouse_and_console_ownership() {
    let directory = FixtureDirectory::new();
    let mut fixture = ConptyFixture::launch(&directory.0);
    let result = fixture.exercise(&directory.0);
    let output = fixture.close();
    assert!(
        result.is_ok(),
        "{result:?}\nConPTY output:\n{}",
        String::from_utf8_lossy(&output)
    );
}
