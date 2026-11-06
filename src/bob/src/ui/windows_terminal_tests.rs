use super::*;
use windows_sys::Win32::System::Console::{
    GetConsoleTitleW, PeekConsoleInputW, SetConsoleTitleW, WriteConsoleInputW,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, INPUT_RECORD, KEY_EVENT,
};

const LIFECYCLE_FIXTURE: &str = "ui::windows_tests::lifecycle::terminal_lifecycle_fixture";

fn output_mode(output: &File) -> u32 {
    let mut mode = 0;
    assert_ne!(
        unsafe { GetConsoleMode(output.as_raw_handle(), &mut mode) },
        0
    );
    mode
}

fn console_title() -> Vec<u16> {
    let mut title = vec![0; 65536];
    let length = unsafe { GetConsoleTitleW(title.as_mut_ptr(), title.len() as u32) } as usize;
    assert!(length > 0);
    title.truncate(length);
    title
}

#[test]
fn terminal_lifecycle_fixture() {
    let Some(directory) = std::env::var_os(FIXTURE_DIR) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let original_title: Vec<u16> = "Original terminal café 界 🦀"
        .encode_utf16()
        .chain([0])
        .collect();
    assert_ne!(unsafe { SetConsoleTitleW(original_title.as_ptr()) }, 0);
    let original_title = console_title();
    let input = ConsoleInput::open().unwrap();
    let output = OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONOUT$")
        .unwrap();
    let original_input = input.mode().unwrap() | ENABLE_VIRTUAL_TERMINAL_INPUT;
    input.set_mode(original_input).unwrap();
    let original_output = output_mode(&output) & !ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    assert_ne!(
        unsafe { SetConsoleMode(output.as_raw_handle(), original_output) },
        0
    );

    for phase in ["partial setup", "draw", "read"] {
        let result: io::Result<()> = (|| {
            let guard = TerminalGuard::activate_with_titles(true)?;
            guard.set_title(&mut io::stdout(), "Bob · temporary café 界 🦀");
            io::stdout().flush()?;
            assert_eq!(
                console_title(),
                "Bob · temporary café 界 🦀"
                    .encode_utf16()
                    .collect::<Vec<_>>()
            );
            assert_eq!(input.mode()? & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
            assert_ne!(output_mode(&output) & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);
            Err(io::Error::other(format!("injected {phase} failure")))
        })();
        assert!(result.is_err());
        assert_eq!(input.mode().unwrap(), original_input, "{phase}");
        assert_eq!(output_mode(&output), original_output, "{phase}");
        assert_eq!(console_title(), original_title, "{phase}");
    }
    let panic = std::panic::catch_unwind(|| {
        let guard = TerminalGuard::activate_with_titles(true).unwrap();
        guard.set_title(&mut io::stdout(), "Bob · unwind title");
        io::stdout().flush().unwrap();
        panic!("injected terminal unwind");
    });
    assert!(panic.is_err());
    assert_eq!(input.mode().unwrap(), original_input);
    assert_eq!(output_mode(&output), original_output);
    assert_eq!(console_title(), original_title);

    {
        let mut disabled = TerminalGuard::activate_with_titles(false).unwrap();
        disabled.set_title(&mut io::stdout(), "Must not replace original title");
        io::stdout().flush().unwrap();
        assert_eq!(console_title(), original_title);
        disabled.restore().unwrap();
    }

    let mut guard = TerminalGuard::activate_with_titles(true).unwrap();
    guard.set_title(&mut io::stdout(), "Bob · normal exit title");
    io::stdout().flush().unwrap();
    assert_ne!(output_mode(&output) & ENABLE_VIRTUAL_TERMINAL_PROCESSING, 0);
    let raw = OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    let mut queued = INPUT_RECORD {
        EventType: KEY_EVENT as u16,
        ..Default::default()
    };
    unsafe {
        queued.Event.KeyEvent.bKeyDown = 1;
        queued.Event.KeyEvent.wRepeatCount = 1;
        queued.Event.KeyEvent.wVirtualKeyCode = b'Q' as u16;
        queued.Event.KeyEvent.uChar.UnicodeChar = b'q' as u16;
    }
    let mut written = 0;
    assert_ne!(
        unsafe { WriteConsoleInputW(raw.as_raw_handle(), &queued, 1, &mut written) },
        0
    );
    assert_eq!(written, 1);
    guard.restore().unwrap();
    guard.restore().unwrap();
    drop(guard);
    assert_eq!(console_title(), original_title);
    assert_eq!(input.mode().unwrap(), original_input);
    assert_eq!(output_mode(&output), original_output);
    let mut pending = [INPUT_RECORD::default(); 128];
    let mut count = 0;
    assert_ne!(
        unsafe {
            PeekConsoleInputW(
                raw.as_raw_handle(),
                pending.as_mut_ptr(),
                pending.len() as u32,
                &mut count,
            )
        },
        0
    );
    assert!(
        pending[..count as usize].iter().any(|record| {
            record.EventType == KEY_EVENT as u16
                && unsafe { record.Event.KeyEvent.uChar.UnicodeChar == b'q' as u16 }
        }),
        "restoration discarded queued input"
    );
    publish(&directory.join("done"), b"restored");
}

#[test]
fn conpty_terminal_cleanup_restores_both_modes_and_preserves_queued_records() {
    let directory = FixtureDirectory::new();
    let mut fixture = ConptyFixture::launch_test(&directory.0, LIFECYCLE_FIXTURE);
    let result = fixture.wait_exit(&directory.0);
    let output = fixture.close();
    assert!(
        result.is_ok(),
        "{result:?}\nConPTY output:\n{}",
        String::from_utf8_lossy(&output)
    );
}
