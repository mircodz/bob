use std::io;
use windows_sys::Win32::{
    Foundation::{GetLastError, SetLastError},
    System::Console::{GetConsoleTitleW, SetConsoleTitleW},
};

pub(super) struct SavedTitle(Vec<u16>);

impl SavedTitle {
    pub(super) fn capture() -> io::Result<Self> {
        let mut title = vec![0; 65536];
        unsafe { SetLastError(0) };
        let length = unsafe { GetConsoleTitleW(title.as_mut_ptr(), title.len() as u32) } as usize;
        if length == 0 {
            let error = unsafe { GetLastError() };
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        title.truncate(length + 1);
        title[length] = 0;
        Ok(Self(title))
    }

    pub(super) fn restore(self) -> io::Result<()> {
        if unsafe { SetConsoleTitleW(self.0.as_ptr()) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires an attached native Windows console; changes the console title"]
    fn native_console_title_roundtrip_preserves_unicode() -> io::Result<()> {
        let original = SavedTitle::capture()?;
        let result = (|| {
            let expected: Vec<u16> = "Bob · café 界 🦀".encode_utf16().chain([0]).collect();
            SavedTitle(expected.clone()).restore()?;
            assert_eq!(SavedTitle::capture()?.0, expected);
            SavedTitle(vec![0]).restore()?;
            assert_eq!(SavedTitle::capture()?.0, [0]);
            Ok(())
        })();
        let restored = original.restore();
        result.and(restored)
    }
}
