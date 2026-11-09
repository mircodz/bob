pub mod title;
#[cfg(windows)]
pub mod windows;

pub fn restore_terminal() -> std::io::Result<()> {
    use crossterm::{
        cursor::Show,
        event::{DisableBracketedPaste, DisableFocusChange, DisableMouseCapture},
        execute,
        terminal::LeaveAlternateScreen,
    };
    let mut output = std::io::stdout();
    restore_stages(|stage| match stage {
        RestoreStage::Screen => execute!(output, LeaveAlternateScreen),
        RestoreStage::Paste => execute!(output, DisableBracketedPaste),
        RestoreStage::Mouse => execute!(output, DisableMouseCapture, DisableFocusChange),
        RestoreStage::Cursor => execute!(output, Show),
        RestoreStage::Raw => disable_raw_mode(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestoreStage {
    Screen,
    Paste,
    Mouse,
    Cursor,
    Raw,
}

fn restore_stages(
    mut restore: impl FnMut(RestoreStage) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut first_error = None;
    for stage in [
        RestoreStage::Screen,
        RestoreStage::Paste,
        RestoreStage::Mouse,
        RestoreStage::Cursor,
        RestoreStage::Raw,
    ] {
        if let Err(error) = restore(stage) {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

pub fn normalize_paste(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('\r') {
        std::borrow::Cow::Owned(text.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod normalization_tests {
    #[test]
    fn failed_cleanup_stages_do_not_suppress_later_restoration() {
        use super::{restore_stages, RestoreStage};
        for failure in 0..5 {
            let mut seen = Vec::new();
            let error = restore_stages(|stage| {
                seen.push(stage);
                if seen.len() > failure {
                    Err(std::io::Error::other(format!("{stage:?}")))
                } else {
                    Ok(())
                }
            })
            .unwrap_err();
            assert_eq!(
                seen,
                [
                    RestoreStage::Screen,
                    RestoreStage::Paste,
                    RestoreStage::Mouse,
                    RestoreStage::Cursor,
                    RestoreStage::Raw
                ]
            );
            assert_eq!(error.to_string(), format!("{:?}", seen[failure]));
        }
    }

    #[test]
    fn paste_preserves_unicode_and_normalizes_all_newline_forms() {
        assert_eq!(super::normalize_paste("é\r\n界\r🦀\n"), "é\n界\n🦀\n");
        assert!(matches!(
            super::normalize_paste("é界🦀"),
            std::borrow::Cow::Borrowed(_)
        ));
    }
}

pub fn disable_raw_mode() -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let _readiness = if crossterm::terminal::is_raw_mode_enabled().unwrap_or(false) {
        macos::restore_readiness().ok()
    } else {
        None
    };
    crossterm::terminal::disable_raw_mode()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        fs::{File, OpenOptions},
        io::{self, IsTerminal},
        os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    };

    pub(super) struct RestoreReadiness {
        _queue: OwnedFd,
        _terminal: Option<File>,
    }

    pub(super) fn restore_readiness() -> io::Result<RestoreReadiness> {
        let terminal = if io::stdin().is_terminal() {
            None
        } else {
            Some(OpenOptions::new().read(true).write(true).open("/dev/tty")?)
        };
        let fd = terminal
            .as_ref()
            .map_or(libc::STDIN_FILENO, AsRawFd::as_raw_fd);
        Ok(RestoreReadiness {
            _queue: register(fd)?,
            _terminal: terminal,
        })
    }

    fn register(fd: RawFd) -> io::Result<OwnedFd> {
        let queue = unsafe { libc::kqueue() };
        if queue == -1 {
            return Err(io::Error::last_os_error());
        }
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };
        if unsafe { libc::fcntl(queue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        // Darwin processes PENDIN synchronously under the old raw flags when this
        // filter spans restoration, preserving queued bytes without reading or flushing.
        let event = libc::kevent {
            ident: fd as usize,
            filter: libc::EVFILT_READ,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        if unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &event,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(queue)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{Read, Write};

        fn mode(fd: RawFd) -> libc::termios {
            let mut mode = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::tcgetattr(fd, &mut mode) }, 0);
            mode
        }

        #[test]
        fn restoration_preserves_flags_and_unconsumed_raw_input() {
            for bytes in [b"ab\x7fc".as_slice(), b"a\rb", b"a\x03b", b"plain"] {
                let (mut master, mut slave) = (-1, -1);
                assert_eq!(
                    unsafe {
                        libc::openpty(
                            &mut master,
                            &mut slave,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        )
                    },
                    0
                );
                let mut master = unsafe { File::from_raw_fd(master) };
                let mut slave = unsafe { File::from_raw_fd(slave) };
                let original = mode(slave.as_raw_fd());
                let mut raw = original;
                unsafe { libc::cfmakeraw(&mut raw) };
                assert_eq!(
                    unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &raw) },
                    0
                );
                master.write_all(bytes).unwrap();
                let readiness = register(slave.as_raw_fd()).unwrap();
                assert_eq!(
                    unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &original) },
                    0
                );
                drop(readiness);
                let restored = mode(slave.as_raw_fd());
                assert_eq!(
                    (
                        restored.c_iflag,
                        restored.c_oflag,
                        restored.c_cflag,
                        restored.c_lflag
                    ),
                    (
                        original.c_iflag,
                        original.c_oflag,
                        original.c_cflag,
                        original.c_lflag
                    )
                );
                assert_eq!(restored.c_cc, original.c_cc);
                assert_eq!(
                    (restored.c_ispeed, restored.c_ospeed),
                    (original.c_ispeed, original.c_ospeed)
                );
                assert_eq!(
                    unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &raw) },
                    0
                );
                assert_ne!(
                    unsafe { libc::fcntl(slave.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) },
                    -1
                );
                let mut received = [0; 32];
                let count = slave.read(&mut received).unwrap();
                assert_eq!(&received[..count], bytes);
            }
        }
    }
}
