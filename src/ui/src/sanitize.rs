use ratatui::text::Line;
use std::borrow::Cow;

/// Remove terminal controls from display text without changing the stored source.
/// Newlines and tabs remain for text layout; buffer symbols need to exclude those too.
pub fn safe_display_text(text: &str) -> Cow<'_, str> {
    DisplaySanitizer::default().text(text)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ControlState {
    #[default]
    Text,
    Escape,
    EscapeIntermediate,
    Csi,
    String {
        osc: bool,
        escape: bool,
    },
}

#[derive(Default)]
struct DisplaySanitizer {
    state: ControlState,
}

impl DisplaySanitizer {
    fn text<'a>(&mut self, text: &'a str) -> Cow<'a, str> {
        if self.state == ControlState::Text
            && !text
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
        {
            return Cow::Borrowed(text);
        }
        let mut clean = String::with_capacity(text.len());
        for c in text.chars() {
            if let ControlState::String { osc, escape } = self.state {
                self.state = if matches!(c, '\u{9c}' | '\u{18}' | '\u{1a}')
                    || (osc && c == '\u{7}')
                    || (escape && c == '\\')
                {
                    ControlState::Text
                } else {
                    ControlState::String {
                        osc,
                        escape: c == '\u{1b}',
                    }
                };
                continue;
            }
            match c {
                '\u{1b}' => self.state = ControlState::Escape,
                '\u{9b}' => self.state = ControlState::Csi,
                '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                    self.state = ControlState::String {
                        osc: c == '\u{9d}',
                        escape: false,
                    };
                }
                '\u{18}' | '\u{1a}' | '\u{9c}' => self.state = ControlState::Text,
                '\n' | '\t' => clean.push(c),
                c if c.is_control() => {}
                _ => match self.state {
                    ControlState::Text => clean.push(c),
                    ControlState::Escape => {
                        self.state = match c {
                            '[' => ControlState::Csi,
                            ']' | 'P' | 'X' | '^' | '_' => ControlState::String {
                                osc: c == ']',
                                escape: false,
                            },
                            '\u{20}'..='\u{2f}' => ControlState::EscapeIntermediate,
                            '\u{30}'..='\u{7e}' => ControlState::Text,
                            _ => {
                                clean.push(c);
                                ControlState::Text
                            }
                        };
                    }
                    ControlState::EscapeIntermediate => match c {
                        '\u{20}'..='\u{2f}' => {}
                        '\u{30}'..='\u{7e}' => self.state = ControlState::Text,
                        _ => {
                            self.state = ControlState::Text;
                            clean.push(c);
                        }
                    },
                    ControlState::Csi => match c {
                        '\u{20}'..='\u{3f}' => {}
                        '\u{40}'..='\u{7e}' => self.state = ControlState::Text,
                        _ => {
                            self.state = ControlState::Text;
                            clean.push(c);
                        }
                    },
                    ControlState::String { .. } => unreachable!(),
                },
            }
        }
        Cow::Owned(clean)
    }
}

/// Keep parser-produced controls and sequences split across spans out of display output.
pub fn sanitize_display_lines(lines: &mut [Line<'_>]) {
    let mut sanitizer = DisplaySanitizer::default();
    for line in lines {
        for span in &mut line.spans {
            if let Cow::Owned(clean) = sanitizer.text(span.content.as_ref()) {
                span.content = Cow::Owned(clean);
            }
        }
    }
}
