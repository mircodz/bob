use super::*;

fn state(enabled: bool) -> State {
    State {
        enabled,
        saved: false,
        last: None,
    }
}

#[test]
fn title_payloads_cannot_emit_terminal_controls() {
    let controls: String = ('\0'..='\u{1f}').chain('\u{7f}'..='\u{9f}').collect();
    let text = sanitize(&format!("café{controls}界\x1b]52;c;payload\x07 🦀"));
    assert!(!text.chars().any(char::is_control));
    assert!(text.contains("café"));
    assert!(text.contains("界]52;c;payload"));
    assert!(text.ends_with('🦀'));
    assert_eq!(sanitize("  a\r\n\t b\u{a0}\u{2003}c  "), "a b c");
    assert_eq!(sanitize(&"界".repeat(200)), "界".repeat(MAX_TITLE_CHARS));
    assert_eq!(session_title(" \n\x07"), "Bob");
    assert_eq!(session_title("Fix\nparser"), "Bob · Fix parser");
    assert_eq!(
        sanitize("safe\u{202e}exe\u{202c}\u{2066}title\u{2069}"),
        "safeexetitle"
    );
    assert_eq!(
        session_title(&"🦀".repeat(200)).chars().count(),
        MAX_TITLE_CHARS
    );
}

#[test]
fn identical_sanitized_updates_only_write_once() {
    let mut state = state(true);
    let mut actions = Vec::new();
    let mut perform = |action: Action<'_>| {
        actions.push(format!("{action:?}"));
        Ok(())
    };
    state.update("Bob · café\n界", &mut perform).unwrap();
    state.update("Bob · café 界\x07", &mut perform).unwrap();
    state.update("Bob · next", &mut perform).unwrap();
    state.restore(&mut perform).unwrap();
    state.restore(&mut perform).unwrap();
    state.update("after restoration", &mut perform).unwrap();
    assert_eq!(
        actions,
        [
            "Save",
            "Set(\"Bob · café 界\")",
            "Set(\"Bob · next\")",
            "Restore"
        ]
    );
}

#[test]
fn opt_out_performs_no_capture_write_or_restoration() {
    let mut state = state(false);
    state
        .update("title", |_| panic!("disabled title I/O"))
        .unwrap();
    state
        .restore(|_| panic!("disabled restoration I/O"))
        .unwrap();
    let mut title = TerminalTitle::new(false);
    let mut writer = RejectIo;
    title.update(&mut writer, "title").unwrap();
    title.restore(&mut writer).unwrap();
}

struct RejectIo;
impl Write for RejectIo {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        panic!("unexpected title output")
    }
    fn flush(&mut self) -> io::Result<()> {
        panic!("unexpected title flush")
    }
}

#[test]
fn failed_capture_never_restores_and_failed_set_restores_once() {
    for fail_save in [true, false] {
        let mut state = state(true);
        assert!(state
            .update("title", |action| {
                if fail_save || matches!(action, Action::Set(_)) {
                    Err(io::Error::other("title unavailable"))
                } else {
                    Ok(())
                }
            })
            .is_err());
        state
            .update("retry", |_| {
                panic!("should not retry failed terminal output")
            })
            .unwrap();
        let mut restores = 0;
        let restored = state.restore(|action| {
            assert_eq!(action, Action::Restore);
            restores += 1;
            Err(io::Error::other("restoration unavailable"))
        });
        assert_eq!(restored.is_err(), !fail_save);
        state.restore(|_| panic!("must not pop twice")).unwrap();
        assert_eq!(restores, usize::from(!fail_save));
    }
}

#[cfg(not(windows))]
#[test]
fn unix_stack_is_balanced_and_updates_do_not_flush() {
    #[derive(Default)]
    struct Output(Vec<u8>);
    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("title writes must share the frame flush")
        }
    }
    let mut output = Output::default();
    let mut title = TerminalTitle::new(true);
    title.update(&mut output, "Bob · workspace").unwrap();
    title.update(&mut output, "Bob · workspace").unwrap();
    title
        .update(&mut output, "Bob · fixed\x07\x1b\u{9c}")
        .unwrap();
    title.restore(&mut output).unwrap();
    title.restore(&mut output).unwrap();
    assert_eq!(
        String::from_utf8(output.0).unwrap(),
        "\x1b[22;0t\x1b]0;Bob · workspace\x07\x1b]0;Bob · fixed\x07\x1b[23;0t"
    );
}
