use crossterm::{terminal::SetTitle, QueueableCommand};
use std::io::{self, Write};

#[cfg(test)]
mod tests;
#[cfg(windows)]
mod windows;

pub const MAX_TITLE_CHARS: usize = 120;

pub fn sanitize(text: &str) -> String {
    let mut result = String::new();
    let mut space = false;
    let mut length = 0;
    for character in text.chars() {
        if character.is_whitespace() {
            space = !result.is_empty();
            continue;
        }
        if character.is_control()
            || matches!(character, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            continue;
        }
        if space {
            if length + 1 >= MAX_TITLE_CHARS {
                break;
            }
            result.push(' ');
            length += 1;
            space = false;
        }
        if length == MAX_TITLE_CHARS {
            break;
        }
        result.push(character);
        length += 1;
    }
    result
}

pub fn session_title(name: &str) -> String {
    let name = sanitize(name);
    if name.is_empty() {
        "Bob".into()
    } else {
        sanitize(&format!("Bob · {name}"))
    }
}

pub struct TerminalTitle {
    state: State,
    #[cfg(windows)]
    original: Option<windows::SavedTitle>,
}

impl TerminalTitle {
    pub fn new(enabled: bool) -> Self {
        Self {
            state: State {
                enabled,
                saved: false,
                last: None,
            },
            #[cfg(windows)]
            original: None,
        }
    }

    pub fn update(&mut self, output: &mut impl Write, title: &str) -> io::Result<()> {
        self.state.update(title, |action| match action {
            Action::Save => {
                #[cfg(windows)]
                {
                    self.original = Some(windows::SavedTitle::capture()?);
                    Ok(())
                }
                #[cfg(not(windows))]
                output.write_all(b"\x1b[22;0t")
            }
            Action::Set(title) => output.queue(SetTitle(title)).map(|_| ()),
            Action::Restore => unreachable!(),
        })
    }

    pub fn restore(&mut self, output: &mut impl Write) -> io::Result<()> {
        self.state.restore(|_| {
            #[cfg(windows)]
            {
                let _ = output;
                self.original.take().map_or(Ok(()), |title| title.restore())
            }
            #[cfg(not(windows))]
            output.write_all(b"\x1b[23;0t")
        })
    }
}

impl Drop for TerminalTitle {
    fn drop(&mut self) {
        if self.state.saved {
            let mut output = io::stdout();
            let _ = self.restore(&mut output);
            let _ = output.flush();
        }
    }
}

#[derive(Debug, PartialEq)]
enum Action<'a> {
    Save,
    Set(&'a str),
    Restore,
}

struct State {
    enabled: bool,
    saved: bool,
    last: Option<String>,
}

impl State {
    fn update(
        &mut self,
        title: &str,
        mut perform: impl FnMut(Action<'_>) -> io::Result<()>,
    ) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let title = sanitize(title);
        if self.last.as_ref() == Some(&title) {
            return Ok(());
        }
        if !self.saved {
            if let Err(error) = perform(Action::Save) {
                self.enabled = false;
                return Err(error);
            }
            self.saved = true;
        }
        if let Err(error) = perform(Action::Set(&title)) {
            self.enabled = false;
            return Err(error);
        }
        self.last = Some(title);
        Ok(())
    }

    fn restore(&mut self, mut perform: impl FnMut(Action<'_>) -> io::Result<()>) -> io::Result<()> {
        self.enabled = false;
        if std::mem::take(&mut self.saved) {
            perform(Action::Restore)?;
        }
        Ok(())
    }
}
