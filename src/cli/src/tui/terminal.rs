use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, LeaveAlternateScreen};

pub(crate) struct TerminalGuard {
    restore: fn(),
}

impl TerminalGuard {
    pub(crate) fn activate() -> std::io::Result<Self> {
        Self::activate_with(enable_raw_mode, restore_terminal)
    }

    fn activate_with(
        activate: impl FnOnce() -> std::io::Result<()>,
        restore: fn(),
    ) -> std::io::Result<Self> {
        activate()?;
        Ok(Self { restore })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        (self.restore)();
    }
}

pub(super) fn restore_terminal() {
    let _ = execute!(
        std::io::stdout(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture,
        crossterm::cursor::Show,
    );
    let _ = disable_raw_mode();
}

#[cfg(test)]
mod tests {
    use super::TerminalGuard;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn guard_restores_once_on_normal_drop_and_unwinding() {
        static RESTORED: AtomicUsize = AtomicUsize::new(0);
        fn restore() {
            RESTORED.fetch_add(1, Ordering::Relaxed);
        }
        let guard = TerminalGuard::activate_with(|| Ok(()), restore).unwrap();
        assert_eq!(RESTORED.load(Ordering::Relaxed), 0);
        drop(guard);
        assert_eq!(RESTORED.load(Ordering::Relaxed), 1);
        let result = std::panic::catch_unwind(|| {
            let _guard = TerminalGuard::activate_with(|| Ok(()), restore).unwrap();
            panic!("simulated draw panic");
        });
        assert!(result.is_err());
        assert_eq!(RESTORED.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn setup_failure_after_raw_activation_always_restores() {
        static RESTORED: AtomicUsize = AtomicUsize::new(0);
        fn restore() {
            RESTORED.fetch_add(1, Ordering::Relaxed);
        }
        fn failing_setup() -> std::io::Result<()> {
            let _guard = TerminalGuard::activate_with(|| Ok(()), restore)?;
            Err(std::io::Error::other("alternate-screen setup failed"))
        }
        assert!(failing_setup().is_err());
        assert_eq!(RESTORED.load(Ordering::Relaxed), 1);
        let failed_activation = TerminalGuard::activate_with(
            || Err(std::io::Error::other("raw activation failed")),
            restore,
        );
        assert!(failed_activation.is_err());
        assert_eq!(RESTORED.load(Ordering::Relaxed), 1);
    }
}
