//! Synthetic record/decoder characterization, NOT native paste or IME acceptance.
use super::*;
use windows_sys::Win32::System::Console::{ReadConsoleInputW, INPUT_RECORD, KEY_EVENT};

const TRACE_FIXTURE: &str = "ui::windows_tests::trace::native_trace_fixture";
const SAMPLE: &str = "aé界🦀\r\n\x1b[200~1\r2\x1b[201~\x03";

#[tokio::test]
async fn native_trace_fixture() {
    let Some(directory) = std::env::var_os(FIXTURE_DIR) else {
        return;
    };
    let directory = PathBuf::from(directory);
    let mut guard = TerminalGuard::activate().unwrap();
    let input = OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")
        .unwrap();
    let mut trace =
        String::from("Synthetic ConPTY bytes only; not keyboard, IME, or paste acceptance.\n");
    publish(&directory.join("raw-ready"), b"ready");
    let mut ended = false;
    for _ in 0..512 {
        let mut record = INPUT_RECORD::default();
        let mut read = 0;
        assert_ne!(
            unsafe { ReadConsoleInputW(input.as_raw_handle(), &mut record, 1, &mut read) },
            0
        );
        assert_eq!(read, 1);
        if record.EventType == KEY_EVENT as u16 {
            let key = unsafe { record.Event.KeyEvent };
            let unit = unsafe { key.uChar.UnicodeChar };
            trace.push_str(&format!(
                "raw down={} repeat={} vk={:#x} scan={:#x} utf16={:#06x} modifiers={:#x}\n",
                key.bKeyDown,
                key.wRepeatCount,
                key.wVirtualKeyCode,
                key.wVirtualScanCode,
                unit,
                key.dwControlKeyState
            ));
            if key.bKeyDown != 0 && unit == 3 {
                ended = true;
                break;
            }
        } else {
            trace.push_str(&format!("raw event_type={}\n", record.EventType));
        }
    }
    assert!(ended, "synthetic raw trace exceeded bounded record budget");
    let console = ConsoleInput::open().unwrap();
    let baseline = console.mode().unwrap();
    let mut events = guard.events();
    let first = ready_event(
        &mut events,
        &console,
        baseline,
        &directory.join("decoded-ready"),
        b"ready",
    )
    .await;
    let mut next = Some(first);
    let mut text = String::new();
    let mut paste_events = 0;
    ended = false;
    for _ in 0..512 {
        let event = match next.take() {
            Some(event) => event,
            None => timeout(EVENT_TIMEOUT, input_event(&mut events))
                .await
                .unwrap(),
        };
        trace.push_str(&format!("decoded {event:?}\n"));
        match event {
            Event::Key(key)
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                ended = true;
                break;
            }
            Event::Key(key) => {
                if let KeyCode::Char(ch) = key.code {
                    text.push(ch);
                }
            }
            Event::Paste(_) => paste_events += 1,
            _ => {}
        }
    }
    trace.push_str(&format!(
        "decoded_text={text:?}\npaste_events={paste_events}\n"
    ));
    publish(
        &directory.join("synthetic-native-trace.txt"),
        trace.as_bytes(),
    );
    // Print bounded synthetic evidence so a failing acceptance run retains it in its log.
    eprintln!("{trace}");
    assert!(
        ended,
        "synthetic decoder trace exceeded bounded event budget"
    );
    assert_eq!(
        paste_events, 0,
        "decoder behavior changed: review paste framing before claiming support"
    );
    drop(events);
    guard.restore().unwrap();
    publish(&directory.join("done"), b"restored");
}

#[test]
fn conpty_characterizes_raw_unicode_and_paste_records_without_claiming_acceptance() {
    let directory = FixtureDirectory::new();
    let mut fixture = ConptyFixture::launch_test(&directory.0, TRACE_FIXTURE);
    let result: io::Result<()> = (|| {
        for marker in ["raw-ready", "decoded-ready"] {
            fixture.wait_marker(&directory.0.join(marker))?;
            fixture
                .input
                .as_mut()
                .unwrap()
                .write_all(SAMPLE.as_bytes())?;
        }
        fixture.wait_exit(&directory.0)
    })();
    let output = fixture.close();
    let trace =
        std::fs::read_to_string(directory.0.join("synthetic-native-trace.txt")).unwrap_or_default();
    eprintln!("{trace}");
    assert!(
        result.is_ok(),
        "{result:?}\n{trace}\nConPTY output:\n{}",
        String::from_utf8_lossy(&output)
    );
    assert!(trace.contains("raw down=") && trace.contains("decoded Key("));
}
