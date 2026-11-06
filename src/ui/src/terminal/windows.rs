use std::{
    fs::{File, OpenOptions},
    io,
    os::windows::io::AsRawHandle,
};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, SetConsoleMode, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING,
};

pub struct ConsoleInput(File);

impl ConsoleInput {
    pub fn open() -> io::Result<Self> {
        open("CONIN$").map(Self)
    }

    pub fn mode(&self) -> io::Result<u32> {
        mode(&self.0)
    }

    pub fn set_mode(&self, value: u32) -> io::Result<()> {
        set_mode(&self.0, value)
    }

    pub fn ensure_record_mode(&self) -> io::Result<()> {
        let current = self.mode()?;
        if current & ENABLE_VIRTUAL_TERMINAL_INPUT != 0 {
            // Crossterm reads INPUT_RECORDs, not the VT byte stream.
            self.set_mode(current & !ENABLE_VIRTUAL_TERMINAL_INPUT)?;
        }
        Ok(())
    }
}

pub struct SavedConsoleModes {
    pub input: ConsoleInput,
    output: File,
    input_mode: Option<u32>,
    output_mode: Option<u32>,
}

impl SavedConsoleModes {
    pub fn capture() -> io::Result<Self> {
        let input = ConsoleInput::open()?;
        let output = open("CONOUT$")?;
        let input_mode = Some(input.mode()?);
        let output_mode = Some(mode(&output)?);
        Ok(Self {
            input,
            output,
            input_mode,
            output_mode,
        })
    }

    pub fn enable_ansi_output(&self) -> io::Result<()> {
        let current = mode(&self.output)?;
        set_mode(
            &self.output,
            current | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        )
    }

    pub fn restore(&mut self) -> io::Result<()> {
        // Call after screen/mouse/raw cleanup: those operations also change modes.
        let output = restore_mode(&self.output, &mut self.output_mode);
        let input = restore_mode(&self.input.0, &mut self.input_mode);
        output.and(input)
    }
}

impl Drop for SavedConsoleModes {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn open(name: &str) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).open(name)
}

fn mode(file: &File) -> io::Result<u32> {
    let mut value = 0;
    if unsafe { GetConsoleMode(file.as_raw_handle(), &mut value) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

fn set_mode(file: &File, value: u32) -> io::Result<()> {
    if unsafe { SetConsoleMode(file.as_raw_handle(), value) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn restore_mode(file: &File, saved: &mut Option<u32>) -> io::Result<()> {
    if let Some(value) = *saved {
        set_mode(file, value)?;
        *saved = None;
    }
    Ok(())
}
