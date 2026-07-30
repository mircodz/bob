//! Central color themes for the TUI. A `Theme` is a full set of colors including
//! a base background (`bg`), which the draw loop paints across the whole screen —
//! so a theme fully controls bob's look regardless of the terminal's own colors.
//! One theme is active at a time (chosen from config or `/theme`), stored in a
//! global; call sites read it via `Palette::TEXT()` etc.

use ratatui::style::Color;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// A complete color set. `bg` is the base background painted behind everything;
/// the `*_bg` fields are for specific surfaces (input band, popups, diffs).
#[derive(Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub text: Color,
    pub dim: Color,
    pub faint: Color,
    pub accent: Color,
    pub user: Color,
    pub ok: Color,
    pub error: Color,
    pub warn: Color,
    pub running: Color,
    pub heading: Color,
    pub list_marker: Color,
    pub inline_code: Color,
    pub link: Color,
    pub blockquote_bar: Color,
    pub rule: Color,
    pub table_border: Color,
    pub code_default: Color,
    pub diff_add: Color,
    pub diff_remove: Color,
    pub diff_add_bg: Color,
    pub diff_remove_bg: Color,
    pub diff_gutter: Color,
    pub input_bg: Color,
    pub popup_bg: Color,
    pub selected_bg: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

impl Theme {
    /// Default dark theme — modeled on kitty's default: a true-black background
    /// with kitty's bright, slightly saturated ANSI-ish accent set.
    pub const fn dark() -> Theme {
        Theme {
            bg: rgb(0x00, 0x00, 0x00),
            text: rgb(0xdd, 0xdd, 0xdd),
            dim: rgb(0x8a, 0x8f, 0x98),
            faint: rgb(0x55, 0x5a, 0x63),
            accent: rgb(0x51, 0xaf, 0xef),
            user: rgb(0x56, 0xc8, 0xd8),
            ok: rgb(0x8c, 0xc2, 0x65),
            error: rgb(0xf7, 0x6b, 0x6b),
            warn: rgb(0xef, 0xbd, 0x5d),
            running: rgb(0xef, 0xbd, 0x5d),
            heading: rgb(0x51, 0xaf, 0xef),
            list_marker: rgb(0x8a, 0x8f, 0x98),
            inline_code: rgb(0xe8, 0x9d, 0x6a),
            link: rgb(0x56, 0xc8, 0xd8),
            blockquote_bar: rgb(0x3a, 0x3e, 0x46),
            rule: rgb(0x2a, 0x2e, 0x36),
            table_border: rgb(0x2a, 0x2e, 0x36),
            code_default: rgb(0xd4, 0xd4, 0xd4),
            diff_add: rgb(0x8c, 0xc2, 0x65),
            diff_remove: rgb(0xf7, 0x6b, 0x6b),
            diff_add_bg: rgb(0x0d, 0x1a, 0x0d),
            diff_remove_bg: rgb(0x1f, 0x0e, 0x0e),
            diff_gutter: rgb(0x55, 0x5a, 0x63),
            input_bg: rgb(0x1a, 0x1a, 0x20),
            popup_bg: rgb(0x12, 0x13, 0x16),
            selected_bg: rgb(0x1e, 0x33, 0x4a),
        }
    }

    /// Default light theme — modeled on the macOS Terminal "Basic" profile:
    /// near-black text on a white background, with muted, readable accents.
    pub const fn light() -> Theme {
        Theme {
            bg: rgb(0xff, 0xff, 0xff),
            text: rgb(0x1d, 0x1d, 0x1f),
            dim: rgb(0x6a, 0x73, 0x7d),
            faint: rgb(0x9d, 0xa5, 0xb4),
            accent: rgb(0x1f, 0x6f, 0xb2),
            user: rgb(0x0b, 0x5c, 0xad),
            ok: rgb(0x2e, 0x7d, 0x32),
            error: rgb(0xc1, 0x2f, 0x2f),
            warn: rgb(0x9a, 0x63, 0x00),
            running: rgb(0x9a, 0x63, 0x00),
            heading: rgb(0x1f, 0x6f, 0xb2),
            list_marker: rgb(0x6a, 0x73, 0x7d),
            inline_code: rgb(0xa6, 0x3d, 0x11),
            link: rgb(0x0a, 0x7d, 0x6b),
            blockquote_bar: rgb(0xc4, 0xcb, 0xd3),
            rule: rgb(0xd8, 0xdd, 0xe3),
            table_border: rgb(0xc4, 0xcb, 0xd3),
            code_default: rgb(0x2a, 0x2f, 0x35),
            diff_add: rgb(0x2e, 0x7d, 0x32),
            diff_remove: rgb(0xc1, 0x2f, 0x2f),
            diff_add_bg: rgb(0xe4, 0xf2, 0xe4),
            diff_remove_bg: rgb(0xfb, 0xe6, 0xe6),
            diff_gutter: rgb(0x9d, 0xa5, 0xb4),
            input_bg: rgb(0xf2, 0xf3, 0xf5),
            popup_bg: rgb(0xec, 0xee, 0xf1),
            selected_bg: rgb(0xd4, 0xe4, 0xf4),
        }
    }

    /// Catppuccin Mocha — official palette (catppuccin/palette).
    pub const fn catppuccin() -> Theme {
        Theme {
            bg: rgb(0x1e, 0x1e, 0x2e),
            text: rgb(0xcd, 0xd6, 0xf4),
            dim: rgb(0xa6, 0xad, 0xc8),
            faint: rgb(0x6c, 0x70, 0x86),
            accent: rgb(0x89, 0xb4, 0xfa),
            user: rgb(0x89, 0xdc, 0xeb),
            ok: rgb(0xa6, 0xe3, 0xa1),
            error: rgb(0xf3, 0x8b, 0xa8),
            warn: rgb(0xf9, 0xe2, 0xaf),
            running: rgb(0xf9, 0xe2, 0xaf),
            heading: rgb(0xcb, 0xa6, 0xf7),
            list_marker: rgb(0x6c, 0x70, 0x86),
            inline_code: rgb(0xfa, 0xb3, 0x87),
            link: rgb(0x94, 0xe2, 0xd5),
            blockquote_bar: rgb(0x45, 0x47, 0x5a),
            rule: rgb(0x31, 0x32, 0x44),
            table_border: rgb(0x31, 0x32, 0x44),
            code_default: rgb(0xcd, 0xd6, 0xf4),
            diff_add: rgb(0xa6, 0xe3, 0xa1),
            diff_remove: rgb(0xf3, 0x8b, 0xa8),
            diff_add_bg: rgb(0x31, 0x32, 0x44),
            diff_remove_bg: rgb(0x31, 0x32, 0x44),
            diff_gutter: rgb(0x6c, 0x70, 0x86),
            input_bg: rgb(0x18, 0x18, 0x25),
            popup_bg: rgb(0x18, 0x18, 0x25),
            selected_bg: rgb(0x45, 0x47, 0x5a),
        }
    }

    /// Catppuccin Macchiato — official palette (catppuccin/palette).
    pub const fn catppuccin_macchiato() -> Theme {
        Theme {
            bg: rgb(0x24, 0x27, 0x3a),
            text: rgb(0xca, 0xd3, 0xf5),
            dim: rgb(0xa5, 0xad, 0xcb),
            faint: rgb(0x6e, 0x73, 0x8d),
            accent: rgb(0x8a, 0xad, 0xf4),
            user: rgb(0x91, 0xd7, 0xe3),
            ok: rgb(0xa6, 0xda, 0x95),
            error: rgb(0xed, 0x87, 0x96),
            warn: rgb(0xee, 0xd4, 0x9f),
            running: rgb(0xee, 0xd4, 0x9f),
            heading: rgb(0xc6, 0xa0, 0xf6),
            list_marker: rgb(0x6e, 0x73, 0x8d),
            inline_code: rgb(0xf5, 0xa9, 0x7f),
            link: rgb(0x8b, 0xd5, 0xca),
            blockquote_bar: rgb(0x49, 0x4d, 0x64),
            rule: rgb(0x36, 0x3a, 0x4f),
            table_border: rgb(0x36, 0x3a, 0x4f),
            code_default: rgb(0xca, 0xd3, 0xf5),
            diff_add: rgb(0xa6, 0xda, 0x95),
            diff_remove: rgb(0xed, 0x87, 0x96),
            diff_add_bg: rgb(0x36, 0x3a, 0x4f),
            diff_remove_bg: rgb(0x36, 0x3a, 0x4f),
            diff_gutter: rgb(0x6e, 0x73, 0x8d),
            input_bg: rgb(0x1e, 0x20, 0x30),
            popup_bg: rgb(0x1e, 0x20, 0x30),
            selected_bg: rgb(0x49, 0x4d, 0x64),
        }
    }

    /// Catppuccin Frappe — official palette (catppuccin/palette).
    pub const fn catppuccin_frappe() -> Theme {
        Theme {
            bg: rgb(0x30, 0x34, 0x46),
            text: rgb(0xc6, 0xd0, 0xf5),
            dim: rgb(0xa5, 0xad, 0xce),
            faint: rgb(0x73, 0x79, 0x94),
            accent: rgb(0x8c, 0xaa, 0xee),
            user: rgb(0x99, 0xd1, 0xdb),
            ok: rgb(0xa6, 0xd1, 0x89),
            error: rgb(0xe7, 0x82, 0x84),
            warn: rgb(0xe5, 0xc8, 0x90),
            running: rgb(0xe5, 0xc8, 0x90),
            heading: rgb(0xca, 0x9e, 0xe6),
            list_marker: rgb(0x73, 0x79, 0x94),
            inline_code: rgb(0xef, 0x9f, 0x76),
            link: rgb(0x81, 0xc8, 0xbe),
            blockquote_bar: rgb(0x51, 0x57, 0x6d),
            rule: rgb(0x41, 0x45, 0x59),
            table_border: rgb(0x41, 0x45, 0x59),
            code_default: rgb(0xc6, 0xd0, 0xf5),
            diff_add: rgb(0xa6, 0xd1, 0x89),
            diff_remove: rgb(0xe7, 0x82, 0x84),
            diff_add_bg: rgb(0x41, 0x45, 0x59),
            diff_remove_bg: rgb(0x41, 0x45, 0x59),
            diff_gutter: rgb(0x73, 0x79, 0x94),
            input_bg: rgb(0x29, 0x2c, 0x3c),
            popup_bg: rgb(0x29, 0x2c, 0x3c),
            selected_bg: rgb(0x51, 0x57, 0x6d),
        }
    }

    /// Catppuccin Latte — official palette (catppuccin/palette). A light theme.
    pub const fn catppuccin_latte() -> Theme {
        Theme {
            bg: rgb(0xef, 0xf1, 0xf5),
            text: rgb(0x4c, 0x4f, 0x69),
            dim: rgb(0x6c, 0x6f, 0x85),
            faint: rgb(0x9c, 0xa0, 0xb0),
            accent: rgb(0x1e, 0x66, 0xf5),
            user: rgb(0x04, 0xa5, 0xe5),
            ok: rgb(0x40, 0xa0, 0x2b),
            error: rgb(0xd2, 0x0f, 0x39),
            warn: rgb(0xdf, 0x8e, 0x1d),
            running: rgb(0xdf, 0x8e, 0x1d),
            heading: rgb(0x88, 0x39, 0xef),
            list_marker: rgb(0x9c, 0xa0, 0xb0),
            inline_code: rgb(0xfe, 0x64, 0x0b),
            link: rgb(0x17, 0x92, 0x99),
            blockquote_bar: rgb(0xbc, 0xc0, 0xcc),
            rule: rgb(0xcc, 0xd0, 0xda),
            table_border: rgb(0xcc, 0xd0, 0xda),
            code_default: rgb(0x4c, 0x4f, 0x69),
            diff_add: rgb(0x40, 0xa0, 0x2b),
            diff_remove: rgb(0xd2, 0x0f, 0x39),
            diff_add_bg: rgb(0xcc, 0xd0, 0xda),
            diff_remove_bg: rgb(0xcc, 0xd0, 0xda),
            diff_gutter: rgb(0x9c, 0xa0, 0xb0),
            input_bg: rgb(0xe6, 0xe9, 0xef),
            popup_bg: rgb(0xe6, 0xe9, 0xef),
            selected_bg: rgb(0xbc, 0xc0, 0xcc),
        }
    }

    /// base16 Default Dark (Chris Kempson). A widely-recognized dark scheme.
    pub const fn base16_dark() -> Theme {
        // base00 bg .. base07 text; base08-0F accents.
        let base00 = rgb(0x18, 0x18, 0x18);
        let base01 = rgb(0x28, 0x28, 0x28);
        let base02 = rgb(0x38, 0x38, 0x38);
        let base03 = rgb(0x58, 0x58, 0x58);
        let base04 = rgb(0xb8, 0xb8, 0xb8);
        let base05 = rgb(0xd8, 0xd8, 0xd8);
        let base08 = rgb(0xab, 0x46, 0x42); // red
        let base0a = rgb(0xf7, 0xca, 0x88); // yellow
        let base0b = rgb(0xa1, 0xb5, 0x6c); // green
        let base0c = rgb(0x86, 0xc1, 0xb9); // cyan
        let base0d = rgb(0x7c, 0xaf, 0xc2); // blue
        let base0e = rgb(0xba, 0x8b, 0xaf); // magenta
        Theme {
            bg: base00,
            text: base05,
            dim: base04,
            faint: base03,
            accent: base0d,
            user: base0c,
            ok: base0b,
            error: base08,
            warn: base0a,
            running: base0a,
            heading: base0d,
            list_marker: base04,
            inline_code: base0e,
            link: base0c,
            blockquote_bar: base02,
            rule: base02,
            table_border: base02,
            code_default: base05,
            diff_add: base0b,
            diff_remove: base08,
            diff_add_bg: base01,
            diff_remove_bg: base01,
            diff_gutter: base03,
            input_bg: base01,
            popup_bg: base01,
            selected_bg: base02,
        }
    }

    /// base16 Solarized Light (Ethan Schoonover).
    pub const fn solarized_light() -> Theme {
        let base3 = rgb(0xfd, 0xf6, 0xe3); // bg
        let base2 = rgb(0xee, 0xe8, 0xd5);
        let base1 = rgb(0x93, 0xa1, 0xa1);
        let base01 = rgb(0x58, 0x6e, 0x75);
        let base00 = rgb(0x65, 0x7b, 0x83); // body text
        let yellow = rgb(0xb5, 0x89, 0x00);
        let red = rgb(0xdc, 0x32, 0x2f);
        let green = rgb(0x85, 0x99, 0x00);
        let cyan = rgb(0x2a, 0xa1, 0x98);
        let blue = rgb(0x26, 0x8b, 0xd2);
        let magenta = rgb(0xd3, 0x36, 0x82);
        Theme {
            bg: base3,
            text: base00,
            dim: base1,
            faint: base1,
            accent: blue,
            user: cyan,
            ok: green,
            error: red,
            warn: yellow,
            running: yellow,
            heading: blue,
            list_marker: base1,
            inline_code: magenta,
            link: cyan,
            blockquote_bar: base2,
            rule: base2,
            table_border: base2,
            code_default: base01,
            diff_add: green,
            diff_remove: red,
            diff_add_bg: rgb(0xed, 0xf0, 0xd6),
            diff_remove_bg: rgb(0xf6, 0xe3, 0xdd),
            diff_gutter: base1,
            input_bg: base2,
            popup_bg: base2,
            selected_bg: rgb(0xd9, 0xe6, 0xe8),
        }
    }

    /// Solarized Dark (Ethan Schoonover) — canonical values.
    pub const fn solarized_dark() -> Theme {
        let base03 = rgb(0x00, 0x2b, 0x36); // bg
        let base02 = rgb(0x07, 0x36, 0x42);
        let base01 = rgb(0x58, 0x6e, 0x75);
        let base00 = rgb(0x65, 0x7b, 0x83);
        let base0 = rgb(0x83, 0x94, 0x96); // body text
        let yellow = rgb(0xb5, 0x89, 0x00);
        let red = rgb(0xdc, 0x32, 0x2f);
        let green = rgb(0x85, 0x99, 0x00);
        let cyan = rgb(0x2a, 0xa1, 0x98);
        let blue = rgb(0x26, 0x8b, 0xd2);
        let magenta = rgb(0xd3, 0x36, 0x82);
        Theme {
            bg: base03,
            text: base0,
            dim: base01,
            faint: base01,
            accent: blue,
            user: cyan,
            ok: green,
            error: red,
            warn: yellow,
            running: yellow,
            heading: blue,
            list_marker: base01,
            inline_code: magenta,
            link: cyan,
            blockquote_bar: base02,
            rule: base02,
            table_border: base02,
            code_default: base0,
            diff_add: green,
            diff_remove: red,
            diff_add_bg: base02,
            diff_remove_bg: base02,
            diff_gutter: base00,
            input_bg: base02,
            popup_bg: base02,
            selected_bg: base01,
        }
    }

    /// GitHub Dark — GitHub's Primer dark palette.
    pub const fn github_dark() -> Theme {
        Theme {
            bg: rgb(0x0d, 0x11, 0x17),
            text: rgb(0xc9, 0xd1, 0xd9),
            dim: rgb(0x8b, 0x94, 0x9e),
            faint: rgb(0x6e, 0x76, 0x81),
            accent: rgb(0x58, 0xa6, 0xff),
            user: rgb(0x79, 0xc0, 0xff),
            ok: rgb(0x3f, 0xb9, 0x50),
            error: rgb(0xf8, 0x51, 0x49),
            warn: rgb(0xd2, 0x9c, 0x22),
            running: rgb(0xd2, 0x9c, 0x22),
            heading: rgb(0x58, 0xa6, 0xff),
            list_marker: rgb(0x8b, 0x94, 0x9e),
            inline_code: rgb(0xff, 0xa6, 0x57),
            link: rgb(0x58, 0xa6, 0xff),
            blockquote_bar: rgb(0x30, 0x36, 0x3d),
            rule: rgb(0x21, 0x26, 0x2d),
            table_border: rgb(0x30, 0x36, 0x3d),
            code_default: rgb(0xc9, 0xd1, 0xd9),
            diff_add: rgb(0x3f, 0xb9, 0x50),
            diff_remove: rgb(0xf8, 0x51, 0x49),
            diff_add_bg: rgb(0x0f, 0x2a, 0x18),
            diff_remove_bg: rgb(0x2a, 0x12, 0x12),
            diff_gutter: rgb(0x6e, 0x76, 0x81),
            input_bg: rgb(0x01, 0x04, 0x09),
            popup_bg: rgb(0x16, 0x1b, 0x22),
            selected_bg: rgb(0x1f, 0x2d, 0x3d),
        }
    }

    /// GitHub Light — GitHub's Primer light palette.
    pub const fn github_light() -> Theme {
        Theme {
            bg: rgb(0xff, 0xff, 0xff),
            text: rgb(0x24, 0x29, 0x2f),
            dim: rgb(0x57, 0x60, 0x6a),
            faint: rgb(0x8c, 0x95, 0x9f),
            accent: rgb(0x09, 0x69, 0xda),
            user: rgb(0x02, 0x55, 0xac),
            ok: rgb(0x1a, 0x7f, 0x37),
            error: rgb(0xcf, 0x22, 0x2e),
            warn: rgb(0x9a, 0x66, 0x00),
            running: rgb(0x9a, 0x66, 0x00),
            heading: rgb(0x09, 0x69, 0xda),
            list_marker: rgb(0x57, 0x60, 0x6a),
            inline_code: rgb(0x95, 0x38, 0x00),
            link: rgb(0x09, 0x69, 0xda),
            blockquote_bar: rgb(0xd0, 0xd7, 0xde),
            rule: rgb(0xd8, 0xde, 0xe4),
            table_border: rgb(0xd0, 0xd7, 0xde),
            code_default: rgb(0x24, 0x29, 0x2f),
            diff_add: rgb(0x1a, 0x7f, 0x37),
            diff_remove: rgb(0xcf, 0x22, 0x2e),
            diff_add_bg: rgb(0xda, 0xfb, 0xe1),
            diff_remove_bg: rgb(0xff, 0xeb, 0xe9),
            diff_gutter: rgb(0x8c, 0x95, 0x9f),
            input_bg: rgb(0xf6, 0xf8, 0xfa),
            popup_bg: rgb(0xf6, 0xf8, 0xfa),
            selected_bg: rgb(0xdd, 0xf4, 0xff),
        }
    }

    /// Resolve a theme by name; unknown names fall back to dark.
    pub fn by_name(name: &str) -> Theme {
        match name.to_ascii_lowercase().as_str() {
            "light" => Theme::light(),
            "catppuccin" | "catppuccin-mocha" | "mocha" => Theme::catppuccin(),
            "catppuccin-macchiato" | "macchiato" => Theme::catppuccin_macchiato(),
            "catppuccin-frappe" | "frappe" => Theme::catppuccin_frappe(),
            "catppuccin-latte" | "latte" => Theme::catppuccin_latte(),
            "base16" | "base16-dark" => Theme::base16_dark(),
            "solarized" | "solarized-dark" => Theme::solarized_dark(),
            "solarized-light" => Theme::solarized_light(),
            "github" | "github-dark" => Theme::github_dark(),
            "github-light" => Theme::github_light(),
            _ => Theme::dark(),
        }
    }

    /// The names offered by the `/theme` command and shown in help.
    pub const NAMES: &'static [&'static str] = &[
        "dark",
        "light",
        "catppuccin",
        "catppuccin-macchiato",
        "catppuccin-frappe",
        "catppuccin-latte",
        "github-dark",
        "github-light",
        "solarized-dark",
        "solarized-light",
        "base16-dark",
    ];
}

static ACTIVE: RwLock<Theme> = RwLock::new(Theme::dark());
/// Bumped on every theme change so render caches can invalidate cheaply.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Select the active theme. Safe at startup or at runtime (`/theme`); the next
/// render picks it up.
pub fn set_theme(theme: Theme) {
    if let Ok(mut w) = ACTIVE.write() {
        *w = theme;
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The current theme generation. Changes whenever `set_theme` is called; a
/// render cache keyed on this value invalidates automatically on theme switch.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

fn active() -> Theme {
    ACTIVE.read().map(|t| *t).unwrap_or_else(|_| Theme::dark())
}

/// Accessor namespace: `Palette::TEXT()` reads the active theme's `text`, etc.
pub struct Palette;

#[allow(dead_code, non_snake_case)]
impl Palette {
    pub fn BG() -> Color {
        active().bg
    }
    pub fn TEXT() -> Color {
        active().text
    }
    pub fn DIM() -> Color {
        active().dim
    }
    pub fn FAINT() -> Color {
        active().faint
    }
    pub fn ACCENT() -> Color {
        active().accent
    }
    pub fn USER() -> Color {
        active().user
    }
    pub fn OK() -> Color {
        active().ok
    }
    pub fn ERROR() -> Color {
        active().error
    }
    pub fn WARN() -> Color {
        active().warn
    }
    pub fn RUNNING() -> Color {
        active().running
    }
    pub fn HEADING() -> Color {
        active().heading
    }
    pub fn LIST_MARKER() -> Color {
        active().list_marker
    }
    pub fn INLINE_CODE() -> Color {
        active().inline_code
    }
    pub fn LINK() -> Color {
        active().link
    }
    pub fn BLOCKQUOTE_BAR() -> Color {
        active().blockquote_bar
    }
    pub fn RULE() -> Color {
        active().rule
    }
    pub fn TABLE_BORDER() -> Color {
        active().table_border
    }
    pub fn CODE_DEFAULT() -> Color {
        active().code_default
    }
    pub fn DIFF_ADD() -> Color {
        active().diff_add
    }
    pub fn DIFF_REMOVE() -> Color {
        active().diff_remove
    }
    pub fn DIFF_ADD_BG() -> Color {
        active().diff_add_bg
    }
    pub fn DIFF_REMOVE_BG() -> Color {
        active().diff_remove_bg
    }
    pub fn DIFF_GUTTER() -> Color {
        active().diff_gutter
    }
    pub fn INPUT_BG() -> Color {
        active().input_bg
    }
    pub fn POPUP_BG() -> Color {
        active().popup_bg
    }
    pub fn SELECTED_BG() -> Color {
        active().selected_bg
    }
}
