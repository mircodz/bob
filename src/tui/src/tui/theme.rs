//! Central color palette for the TUI. One place to retune the whole look.

use ratatui::style::Color;

pub struct Palette;

#[allow(dead_code)]
impl Palette {
    // Base text
    pub const TEXT: Color = Color::Rgb(0xdd, 0xdd, 0xdd);
    pub const DIM: Color = Color::Rgb(0x88, 0x88, 0x88);
    pub const FAINT: Color = Color::Rgb(0x66, 0x66, 0x66);

    // Accents
    pub const ACCENT: Color = Color::Rgb(0x7a, 0xa6, 0xc2); // bob cyan-blue
    pub const USER: Color = Color::Rgb(0x9c, 0xdc, 0xfe);

    // Status
    pub const OK: Color = Color::Rgb(0x6a, 0x99, 0x55);
    pub const ERROR: Color = Color::Rgb(0xc5, 0x50, 0x4b);
    pub const WARN: Color = Color::Rgb(0xd7, 0xba, 0x7d);
    pub const RUNNING: Color = Color::Rgb(0xd7, 0xba, 0x7d);

    // Markdown
    pub const HEADING: Color = Color::Rgb(0x7a, 0xa6, 0xc2);
    pub const LIST_MARKER: Color = Color::Rgb(0x88, 0x88, 0x88);
    pub const INLINE_CODE: Color = Color::Rgb(0xce, 0x91, 0x78);
    pub const LINK: Color = Color::Rgb(0x4e, 0xc9, 0xb0);
    pub const BLOCKQUOTE: Color = Color::Rgb(0xaa, 0xaa, 0xaa);
    pub const BLOCKQUOTE_BAR: Color = Color::Rgb(0x55, 0x55, 0x55);
    pub const RULE: Color = Color::Rgb(0x55, 0x55, 0x55);
    pub const TABLE_BORDER: Color = Color::Rgb(0x55, 0x55, 0x55);
    pub const CODE_DEFAULT: Color = Color::Rgb(0xd4, 0xd4, 0xd4);

    // Diff
    pub const DIFF_ADD: Color = Color::Rgb(0x6a, 0x99, 0x55);
    pub const DIFF_REMOVE: Color = Color::Rgb(0xc5, 0x50, 0x4b);
    pub const DIFF_ADD_BG: Color = Color::Rgb(0x1e, 0x2a, 0x1e);
    pub const DIFF_REMOVE_BG: Color = Color::Rgb(0x2a, 0x1e, 0x1e);
    pub const DIFF_GUTTER: Color = Color::Rgb(0x66, 0x66, 0x66);

    // Chrome
    pub const BORDER: Color = Color::Rgb(0x3a, 0x3a, 0x3a);
    pub const INPUT_BG: Color = Color::Rgb(0x1c, 0x1c, 0x1c);
    pub const POPUP_BG: Color = Color::Rgb(0x22, 0x22, 0x22);
    pub const SELECTED_BG: Color = Color::Rgb(0x2d, 0x3a, 0x4a);
}
