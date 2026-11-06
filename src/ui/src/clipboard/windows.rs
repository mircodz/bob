use super::{unicode_text, CopyOutcome};
use std::{io, ptr, time::Duration};
use windows_sys::Win32::{
    Foundation::{GetLastError, GlobalFree, SetLastError, HGLOBAL, HWND},
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
    },
    UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE},
};

const CF_UNICODETEXT: u32 = 13;
const OPEN_ATTEMPTS: usize = 6;

struct Owner(HWND);
impl Owner {
    fn new() -> io::Result<Self> {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // A real, process-owned HWND is required: OpenClipboard(NULL) followed by
        // EmptyClipboard leaves no owner and SetClipboardData can then fail.
        let window = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if window.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(window))
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

struct Memory {
    handle: HGLOBAL,
    locked: bool,
}
impl Drop for Memory {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            if self.locked {
                unsafe { GlobalUnlock(self.handle) };
            }
            unsafe { GlobalFree(self.handle) };
        }
    }
}

struct Open(bool);
impl Open {
    fn close(&mut self) -> io::Result<()> {
        if self.0 {
            if unsafe { CloseClipboard() } == 0 {
                return Err(io::Error::last_os_error());
            }
            self.0 = false;
        }
        Ok(())
    }
}
impl Drop for Open {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn retry_open(
    mut attempt: impl FnMut() -> io::Result<()>,
    mut pause: impl FnMut(),
) -> io::Result<()> {
    for index in 0..OPEN_ATTEMPTS {
        match attempt() {
            Ok(()) => return Ok(()),
            Err(error) if index + 1 == OPEN_ATTEMPTS => return Err(error),
            Err(_) => pause(),
        }
    }
    unreachable!()
}

pub(super) fn copy(text: &str) -> io::Result<CopyOutcome> {
    let wide = unicode_text(text)?;
    let bytes = wide
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "Clipboard text is too large")
        })?;
    let mut memory = Memory {
        handle: unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) },
        locked: false,
    };
    if memory.handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    let target = unsafe { GlobalLock(memory.handle) }.cast::<u16>();
    if target.is_null() {
        return Err(io::Error::last_os_error());
    }
    memory.locked = true;
    unsafe {
        ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
    }
    unsafe {
        SetLastError(0);
    }
    if unsafe { GlobalUnlock(memory.handle) } == 0 {
        let error = unsafe { GetLastError() };
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
    }
    memory.locked = false;
    let owner = Owner::new()?;
    retry_open(
        || {
            if unsafe { OpenClipboard(owner.0) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        },
        || std::thread::sleep(Duration::from_millis(10)),
    )?;
    let mut opened = Open(true);
    if unsafe { EmptyClipboard() } == 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { SetClipboardData(CF_UNICODETEXT, memory.handle) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    memory.handle = ptr::null_mut(); // Ownership transfers only on successful SetClipboardData.
    opened.close()?;
    Ok(CopyOutcome::NativeCopied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contention_has_a_bounded_retry_budget_and_reports_failure() {
        let (mut attempts, mut pauses) = (0, 0);
        let error = retry_open(
            || {
                attempts += 1;
                Err(io::Error::other("busy"))
            },
            || pauses += 1,
        )
        .unwrap_err();
        assert_eq!((attempts, pauses), (OPEN_ATTEMPTS, OPEN_ATTEMPTS - 1));
        assert_eq!(error.to_string(), "busy");
        attempts = 0;
        retry_open(
            || {
                attempts += 1;
                if attempts == 2 {
                    Ok(())
                } else {
                    Err(io::Error::other("busy"))
                }
            },
            || {},
        )
        .unwrap();
        assert_eq!(attempts, 2);
    }

    #[test]
    #[ignore = "writes the real clipboard; isolated native acceptance only"]
    fn isolated_native_unicode_readback() {
        assert_eq!(
            std::env::var("BOB_ISOLATED_CLIPBOARD_ACCEPTANCE").as_deref(),
            Ok("1"),
            "run only on an isolated acceptance desktop, never a developer clipboard"
        );
        use windows_sys::Win32::System::{DataExchange::GetClipboardData, Memory::GlobalSize};
        for text in ["", "native é 界 🦀\r\nclipboard"] {
            assert_eq!(copy(text).unwrap(), CopyOutcome::NativeCopied);
            assert!(copy("invalid\0text").is_err());
            let (held_tx, held_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let holder = std::thread::spawn(move || {
                let owner = Owner::new().unwrap();
                retry_open(
                    || {
                        if unsafe { OpenClipboard(owner.0) } == 0 {
                            Err(io::Error::last_os_error())
                        } else {
                            Ok(())
                        }
                    },
                    || std::thread::sleep(Duration::from_millis(10)),
                )
                .unwrap();
                let mut opened = Open(true);
                held_tx.send(()).unwrap();
                let _ = release_rx.recv_timeout(Duration::from_secs(5));
                opened.close().unwrap();
            });
            held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            let busy = copy("must not replace clipboard during contention");
            let _ = release_tx.send(());
            holder.join().unwrap();
            assert!(busy.is_err());
            let owner = Owner::new().unwrap();
            retry_open(
                || {
                    if unsafe { OpenClipboard(owner.0) } == 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                },
                || std::thread::sleep(Duration::from_millis(10)),
            )
            .unwrap();
            let mut opened = Open(true);
            let memory = unsafe { GetClipboardData(CF_UNICODETEXT) };
            assert!(!memory.is_null());
            let size = unsafe { GlobalSize(memory) } / 2;
            let data = unsafe { GlobalLock(memory) }.cast::<u16>();
            assert!(!data.is_null());
            let units = unsafe { std::slice::from_raw_parts(data, size) };
            let length = units.iter().position(|unit| *unit == 0).unwrap();
            let actual = String::from_utf16(&units[..length]).unwrap();
            unsafe {
                GlobalUnlock(memory);
            }
            opened.close().unwrap();
            assert_eq!(actual, text);
        }
    }
}
