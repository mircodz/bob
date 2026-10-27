use crossterm::event::{DisableBracketedPaste, DisableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, LeaveAlternateScreen};
use futures::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};

#[cfg(windows)]
mod windows;

pub(crate) struct TerminalGuard {
    restore: fn(),
    #[cfg(windows)]
    original_input: Option<windows::SavedInput>,
}

impl TerminalGuard {
    pub(crate) fn activate() -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            let original = windows::SavedInput::capture()?;
            let mut guard = Self::activate_with(enable_raw_mode, restore_terminal)?;
            guard.original_input = Some(original);
            guard
                .original_input
                .as_ref()
                .unwrap()
                .input
                .ensure_record_mode()?;
            Ok(guard)
        }
        #[cfg(not(windows))]
        Self::activate_with(enable_raw_mode, restore_terminal)
    }

    fn activate_with(
        activate: impl FnOnce() -> std::io::Result<()>,
        restore: fn(),
    ) -> std::io::Result<Self> {
        activate()?;
        Ok(Self {
            restore,
            #[cfg(windows)]
            original_input: None,
        })
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

pub(super) struct InputEvents {
    events: crossterm::event::EventStream,
    #[cfg(windows)]
    input: windows::ConsoleInput,
}

impl InputEvents {
    pub(super) fn new() -> std::io::Result<Self> {
        #[cfg(windows)]
        let input = windows::ConsoleInput::open()?;
        #[cfg(windows)]
        input.ensure_record_mode()?;
        Ok(Self {
            events: crossterm::event::EventStream::new(),
            #[cfg(windows)]
            input,
        })
    }
}

impl Stream for InputEvents {
    type Item = std::io::Result<crossterm::event::Event>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        #[cfg(windows)]
        if let Err(error) = this.input.ensure_record_mode() {
            return Poll::Ready(Some(Err(error)));
        }
        let result = Pin::new(&mut this.events).poll_next(cx);
        #[cfg(windows)]
        if result.is_pending() {
            if let Err(error) = this.input.ensure_record_mode() {
                return Poll::Ready(Some(Err(error)));
            }
        }
        result
    }
}

pub(crate) fn read_event() -> std::io::Result<crossterm::event::Event> {
    #[cfg(windows)]
    windows::ConsoleInput::open()?.ensure_record_mode()?;
    crossterm::event::read()
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
