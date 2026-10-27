use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::System::Console::{
    GetConsoleMode, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_INPUT,
};

pub(super) struct ConsoleInput(File);

impl ConsoleInput {
    pub(super) fn open() -> io::Result<Self> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .open("CONIN$")
            .map(Self)
    }

    pub(super) fn mode(&self) -> io::Result<u32> {
        let mut mode = 0;
        if unsafe { GetConsoleMode(self.0.as_raw_handle(), &mut mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(mode)
    }

    pub(super) fn set_mode(&self, mode: u32) -> io::Result<()> {
        if unsafe { SetConsoleMode(self.0.as_raw_handle(), mode) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn ensure_record_mode(&self) -> io::Result<()> {
        let mode = self.mode()?;
        if mode & ENABLE_VIRTUAL_TERMINAL_INPUT != 0 {
            // ConPTY otherwise delivers mouse reports and control keys as text records.
            self.set_mode(mode & !ENABLE_VIRTUAL_TERMINAL_INPUT)?;
        }
        Ok(())
    }
}

pub(super) struct SavedInput {
    pub(super) input: ConsoleInput,
    original: u32,
}

impl SavedInput {
    pub(super) fn capture() -> io::Result<Self> {
        let input = ConsoleInput::open()?;
        let original = input.mode()?;
        Ok(Self { input, original })
    }
}

impl Drop for SavedInput {
    fn drop(&mut self) {
        let _ = self.input.set_mode(self.original);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::terminal::{InputEvents, TerminalGuard};
    use crate::tui::KeyOutcome;
    use crossterm::event::{
        EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    };
    use futures::StreamExt;
    use std::io::{Read, Write};
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Console::{
        ClosePseudoConsole, CreatePseudoConsole, COORD, HPCON,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::*;

    const FIXTURE: &str = "tui::terminal::windows::tests::conpty_input_fixture";
    const ACCESS_FIXTURE: &str = "tui::terminal::windows::tests::console_access_fixture";
    const FIXTURE_DIR: &str = "BOB_CONPTY_FIXTURE_DIR";

    #[test]
    fn console_access_fixture() {
        let Ok(expected) = std::env::var("BOB_CONSOLE_ACCESS") else {
            return;
        };
        let input = ConsoleInput::open();
        assert_eq!(input.is_ok(), expected == "attached");
        if let Ok(input) = input {
            input
                .set_mode(input.mode().unwrap() | ENABLE_VIRTUAL_TERMINAL_INPUT)
                .unwrap();
        }
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
        let guard = TerminalGuard::activate().unwrap();
        assert_eq!(input.mode().unwrap() & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        crossterm::execute!(std::io::stdout(), EnableMouseCapture).unwrap();
        let baseline = input.mode().unwrap();

        let control = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", ACCESS_FIXTURE, "--nocapture"])
            .env("BOB_CONSOLE_ACCESS", "attached")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            control.status.success(),
            "{}",
            String::from_utf8_lossy(&control.stderr)
        );
        assert_ne!(input.mode().unwrap() & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        input.ensure_record_mode().unwrap();
        let mut isolated = bob_core::core::process::ManagedProcess::spawn(
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", ACCESS_FIXTURE, "--nocapture"])
                .env("BOB_CONSOLE_ACCESS", "isolated")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
        .unwrap();
        let output = tokio::time::timeout(Duration::from_secs(10), isolated.collect_output(4096))
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success(), "{}", output.stderr.text());
        assert_eq!(input.mode().unwrap(), baseline);

        let mut events = InputEvents::new().unwrap();
        for round in 0..2 {
            if round == 1 {
                input
                    .set_mode(baseline | ENABLE_VIRTUAL_TERMINAL_INPUT)
                    .unwrap();
            }
            let mut first = Box::pin(events.next());
            while let std::task::Poll::Ready(event) = futures::poll!(&mut first) {
                assert!(matches!(
                    event.unwrap().unwrap(),
                    Event::Resize(..) | Event::FocusGained
                ));
                drop(first);
                first = Box::pin(events.next());
            }
            assert_eq!(input.mode().unwrap(), baseline);
            std::fs::write(directory.join(format!("ready-{round}")), "ready").unwrap();
            let initial = tokio::time::timeout(Duration::from_secs(10), &mut first)
                .await
                .unwrap();
            drop(first);
            let mut initial = Some(initial);
            let mut app = crate::tui::test_support::test_app();
            app.input.set("draft");
            let mut submitted = false;
            let mut toggled = false;
            let mut quit = false;
            let mut clicked = false;
            let mut released = false;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !(submitted && toggled && quit && clicked && released) {
                    let event = match initial.take() {
                        Some(event) => event,
                        None => events.next().await,
                    }
                    .unwrap()
                    .unwrap();
                    if matches!(event, Event::Key(key) if key.kind == KeyEventKind::Release) {
                        continue;
                    }
                    let mut dirty = false;
                    let outcome = app.on_terminal_event(event.clone(), &mut dirty);
                    match event {
                        Event::Key(key) if key.code == KeyCode::Enter => {
                            assert!(
                                matches!(outcome, KeyOutcome::Submit(ref text) if text == "drafta")
                            );
                            submitted = true;
                        }
                        Event::Key(key) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            match key.code {
                                KeyCode::Char('t' | 'T') => {
                                    assert!(app.sidebar_open);
                                    toggled = true;
                                }
                                KeyCode::Char('c' | 'C') => {
                                    assert!(matches!(outcome, KeyOutcome::Quit));
                                    quit = true;
                                }
                                other => panic!("unexpected control key: {other:?}"),
                            }
                        }
                        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Down(_)) => {
                            assert_eq!((mouse.column, mouse.row), (2, 1));
                            clicked = true;
                        }
                        Event::Mouse(mouse) if matches!(mouse.kind, MouseEventKind::Up(_)) => {
                            released = true;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(app.input.text(), "");
        }
        drop(events);
        drop(guard);
        assert_eq!(input.mode().unwrap(), original);
        std::fs::write(directory.join("done"), "done").unwrap();
    }

    struct FixtureDirectory(PathBuf);
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
    struct FixtureProcess(OwnedHandle);
    impl Drop for FixtureProcess {
        fn drop(&mut self) {
            unsafe {
                TerminateProcess(self.0.as_raw_handle(), 1);
                WaitForSingleObject(self.0.as_raw_handle(), 5000);
            }
        }
    }

    fn pipe() -> (File, File) {
        let mut reader = std::ptr::null_mut();
        let mut writer = std::ptr::null_mut();
        assert_ne!(
            unsafe { CreatePipe(&mut reader, &mut writer, std::ptr::null(), 0) },
            0
        );
        unsafe { (File::from_raw_handle(reader), File::from_raw_handle(writer)) }
    }

    fn launch_fixture(console: &PseudoConsole, directory: &Path) -> FixtureProcess {
        let mut bytes = 0;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut bytes) };
        let mut storage = vec![0usize; bytes.div_ceil(std::mem::size_of::<usize>())];
        let attributes = storage.as_mut_ptr().cast();
        assert_ne!(
            unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &mut bytes) },
            0
        );
        assert_ne!(
            unsafe {
                UpdateProcThreadAttribute(
                    attributes,
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
        startup.lpAttributeList = attributes;
        let executable = std::env::current_exe().unwrap();
        let mut command: Vec<u16> = std::ffi::OsStr::new(&format!(
            "\"{}\" --exact {FIXTURE} --nocapture",
            executable.display()
        ))
        .encode_wide()
        .chain(Some(0))
        .collect();
        let mut variables: Vec<_> = std::env::vars_os()
            .filter(|(name, _)| name != FIXTURE_DIR)
            .map(|(name, value)| {
                let mut entry = name;
                entry.push("=");
                entry.push(value);
                entry
            })
            .collect();
        let mut fixture = std::ffi::OsString::from(format!("{FIXTURE_DIR}="));
        fixture.push(directory);
        variables.push(fixture);
        variables.sort_by_key(|entry| entry.to_string_lossy().to_ascii_uppercase());
        let mut environment: Vec<u16> = variables
            .iter()
            .flat_map(|entry| entry.encode_wide().chain(Some(0)))
            .collect();
        environment.push(0);
        let mut process = PROCESS_INFORMATION::default();
        let success = unsafe {
            CreateProcessW(
                std::ptr::null(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
                environment.as_ptr().cast(),
                std::ptr::null(),
                &startup.StartupInfo,
                &mut process,
            )
        };
        unsafe { DeleteProcThreadAttributeList(attributes) };
        assert_ne!(success, 0, "{}", io::Error::last_os_error());
        unsafe {
            drop(OwnedHandle::from_raw_handle(process.hThread));
            FixtureProcess(OwnedHandle::from_raw_handle(process.hProcess))
        }
    }

    #[test]
    fn conpty_mode_drift_preserves_keys_mouse_and_console_ownership() {
        let directory = FixtureDirectory(
            std::env::temp_dir().join(format!("bob-conpty-{}", uuid::Uuid::new_v4())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        let (input_read, mut input_write) = pipe();
        let (mut output_read, output_write) = pipe();
        let drain = std::thread::spawn(move || {
            let mut output = Vec::new();
            let mut buffer = [0; 4096];
            while let Ok(size) = output_read.read(&mut buffer) {
                if size == 0 {
                    break;
                }
                let keep = size.min(16_384usize.saturating_sub(output.len()));
                output.extend_from_slice(&buffer[..keep]);
            }
            output
        });
        let mut console = 0;
        assert_eq!(
            unsafe {
                CreatePseudoConsole(
                    COORD { X: 100, Y: 30 },
                    input_read.as_raw_handle(),
                    output_write.as_raw_handle(),
                    0,
                    &mut console,
                )
            },
            0
        );
        let console = PseudoConsole(console);
        drop(input_read);
        drop(output_write);
        let process = launch_fixture(&console, &directory.0);
        for round in 0..2 {
            let started = Instant::now();
            while !directory.0.join(format!("ready-{round}")).exists() {
                assert!(
                    started.elapsed() < Duration::from_secs(20),
                    "ConPTY fixture did not become ready"
                );
                assert_eq!(
                    unsafe { WaitForSingleObject(process.0.as_raw_handle(), 0) },
                    WAIT_TIMEOUT,
                    "fixture exited before readiness"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            input_write
                .write_all(b"a\r\x14\x03\x1b[<0;3;2M\x1b[<0;3;2m")
                .unwrap();
        }
        assert_eq!(
            unsafe { WaitForSingleObject(process.0.as_raw_handle(), 20_000) },
            WAIT_OBJECT_0
        );
        let mut exit_code = 0;
        assert_ne!(
            unsafe { GetExitCodeProcess(process.0.as_raw_handle(), &mut exit_code) },
            0
        );
        drop(process);
        drop(console);
        drop(input_write);
        let output = drain.join().unwrap();
        assert_eq!(exit_code, 0, "{}", String::from_utf8_lossy(&output));
        assert!(directory.0.join("done").is_file());
    }
}
