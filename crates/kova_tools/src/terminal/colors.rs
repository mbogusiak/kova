//! Resolution of [`CellColor`]s against the `kova_theme` terminal palette.
//!
//! The 16 ANSI colors, the default foreground/background and the cursor come from the
//! theme's `[terminal]` tokens; the xterm 6×6×6 cube and the grayscale ramp (`16..=255`) and
//! `TrueColor` values are absolute.

use kova_theme::{RgbaColor, TerminalPalette};

use super::term_state::CellColor;

/// Resolves a foreground color.
#[must_use]
pub fn resolve_foreground(palette: &TerminalPalette, color: CellColor) -> RgbaColor {
    resolve(palette, color)
}

/// Resolves a background color.
#[must_use]
pub fn resolve_background(palette: &TerminalPalette, color: CellColor) -> RgbaColor {
    resolve(palette, color)
}

fn resolve(palette: &TerminalPalette, color: CellColor) -> RgbaColor {
    match color {
        CellColor::DefaultForeground => palette.foreground,
        CellColor::DefaultBackground => palette.background,
        CellColor::Ansi(index) => palette.ansi(index).unwrap_or(palette.foreground),
        CellColor::Indexed(index) => palette
            .ansi(index)
            .unwrap_or_else(|| xterm_256_color(index)),
        CellColor::Rgb(r, g, b) => RgbaColor::rgb(r, g, b),
    }
}

/// Returns the standard xterm color for indices `16..=255` (cube and grayscale ramp).
///
/// Indices below 16 have no fixed value (they are theme colors) and map to black.
#[must_use]
pub const fn xterm_256_color(index: u8) -> RgbaColor {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => RgbaColor::rgb(0, 0, 0),
        16..=231 => {
            let i = index - 16;
            RgbaColor::rgb(
                LEVELS[(i / 36) as usize],
                LEVELS[((i / 6) % 6) as usize],
                LEVELS[(i % 6) as usize],
            )
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            RgbaColor::rgb(level, level, level)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> TerminalPalette {
        kova_theme::AetherTheme::builtin(kova_theme::BuiltinTheme::KovaIslandsDark)
            .tokens
            .terminal
    }

    #[test]
    fn ansi_and_defaults_come_from_the_kova_islands_dark_tokens() {
        let p = palette();
        assert_eq!(
            resolve_foreground(&p, CellColor::DefaultForeground),
            p.foreground
        );
        assert_eq!(
            resolve_background(&p, CellColor::DefaultBackground),
            p.background
        );
        for index in 0..16 {
            assert_eq!(
                resolve_foreground(&p, CellColor::Ansi(index)),
                p.ansi(index).unwrap_or(p.foreground)
            );
        }
        assert_eq!(resolve_foreground(&p, CellColor::Ansi(1)), p.red);
        assert_eq!(resolve_foreground(&p, CellColor::Ansi(12)), p.bright_blue);
    }

    #[test]
    fn indexed_and_truecolor_are_absolute() {
        let p = palette();
        assert_eq!(xterm_256_color(16), RgbaColor::rgb(0, 0, 0));
        assert_eq!(xterm_256_color(196), RgbaColor::rgb(255, 0, 0));
        assert_eq!(xterm_256_color(231), RgbaColor::rgb(255, 255, 255));
        assert_eq!(xterm_256_color(232), RgbaColor::rgb(8, 8, 8));
        assert_eq!(xterm_256_color(255), RgbaColor::rgb(238, 238, 238));
        assert_eq!(
            resolve_foreground(&p, CellColor::Rgb(10, 20, 30)),
            RgbaColor::rgb(10, 20, 30)
        );
    }
}
