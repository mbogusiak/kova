//! Color representation (`RgbaColor`), hex parsing, alpha compositing, and WCAG 2.1 contrast math.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

/// An sRGB color with an 8-bit alpha channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RgbaColor {
    /// Red channel (`0..=255`).
    pub r: u8,
    /// Green channel (`0..=255`).
    pub g: u8,
    /// Blue channel (`0..=255`).
    pub b: u8,
    /// Alpha channel (`0` = transparent, `255` = opaque).
    pub a: u8,
}

impl RgbaColor {
    /// Creates an opaque `RgbaColor` from 8-bit red, green, and blue channels.
    #[must_use]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Creates an `RgbaColor` from 8-bit red, green, blue, and alpha channels.
    #[must_use]
    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Parses a color from `#RRGGBB` or `#RRGGBBAA` hexadecimal notation.
    ///
    /// # Errors
    ///
    /// Returns [`ColorParseError`] if the string does not start with `#`, has a
    /// length other than 7 or 9 bytes, or contains non-hexadecimal ASCII digits.
    pub fn from_hex(input: &str) -> Result<Self, ColorParseError> {
        let Some(hex) = input.strip_prefix('#') else {
            return Err(ColorParseError::MissingHashPrefix {
                input: input.to_owned(),
            });
        };

        if !hex.is_ascii() || (hex.len() != 6 && hex.len() != 8) {
            return Err(ColorParseError::InvalidLength {
                input: input.to_owned(),
            });
        }

        let r = parse_hex_byte(&hex[0..2], input)?;
        let g = parse_hex_byte(&hex[2..4], input)?;
        let b = parse_hex_byte(&hex[4..6], input)?;
        let a = if hex.len() == 8 {
            parse_hex_byte(&hex[6..8], input)?
        } else {
            255
        };

        Ok(Self { r, g, b, a })
    }

    /// Formats this color as uppercase `#RRGGBB` when opaque, or `#RRGGBBAA` otherwise.
    #[must_use]
    pub fn to_hex_string(self) -> String {
        if self.a == 255 {
            format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", self.r, self.g, self.b, self.a)
        }
    }

    /// Returns normalized `[r, g, b, a]` floating-point components in `0.0..=1.0`.
    #[must_use]
    pub fn to_f32_array(self) -> [f32; 4] {
        [
            f32::from(self.r) / 255.0,
            f32::from(self.g) / 255.0,
            f32::from(self.b) / 255.0,
            f32::from(self.a) / 255.0,
        ]
    }

    /// Returns a copy of this color with a replaced alpha channel.
    #[must_use]
    pub const fn with_alpha(self, a: u8) -> Self {
        Self { a, ..self }
    }

    /// Composites `self` (foreground) over `backdrop` using sRGB source-over alpha blending.
    #[must_use]
    pub fn over(self, backdrop: Self) -> Self {
        if self.a == 255 {
            return self;
        }
        if self.a == 0 {
            return backdrop;
        }

        let fg_a = f64::from(self.a) / 255.0;
        let bg_a = f64::from(backdrop.a) / 255.0;
        let out_a = fg_a + bg_a * (1.0 - fg_a);
        if out_a <= f64::EPSILON {
            return Self::rgba(0, 0, 0, 0);
        }

        let r = blend_channel(self.r, backdrop.r, fg_a, bg_a, out_a);
        let g = blend_channel(self.g, backdrop.g, fg_a, bg_a, out_a);
        let b = blend_channel(self.b, backdrop.b, fg_a, bg_a, out_a);
        let a = quantize_unit(out_a);
        Self { r, g, b, a }
    }
}

fn parse_hex_byte(pair: &str, full_input: &str) -> Result<u8, ColorParseError> {
    u8::from_str_radix(pair, 16).map_err(|_| ColorParseError::InvalidHexDigits {
        input: full_input.to_owned(),
    })
}

fn blend_channel(fg: u8, bg: u8, fg_a: f64, bg_a: f64, out_a: f64) -> u8 {
    let fg_norm = f64::from(fg) / 255.0;
    let bg_norm = f64::from(bg) / 255.0;
    let mixed = (fg_norm * fg_a + bg_norm * bg_a * (1.0 - fg_a)) / out_a;
    quantize_unit(mixed)
}

fn quantize_unit(value: f64) -> u8 {
    let scaled = (value * 255.0).round().clamp(0.0, 255.0);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "scaled is rounded and clamped to 0.0..=255.0"
    )]
    let byte = scaled as u8;
    byte
}

impl fmt::Display for RgbaColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex_string())
    }
}

impl Serialize for RgbaColor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_hex_string())
    }
}

impl<'de> Deserialize<'de> for RgbaColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::from_hex(&raw).map_err(de::Error::custom)
    }
}

/// Errors produced when parsing a hex color string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ColorParseError {
    /// Hex string did not start with `#`.
    #[error("color `{input}` must start with `#`")]
    MissingHashPrefix {
        /// Original invalid input string.
        input: String,
    },
    /// Hex string did not have 6 (`#RRGGBB`) or 8 (`#RRGGBBAA`) hex characters.
    #[error("color `{input}` must be `#RRGGBB` or `#RRGGBBAA`")]
    InvalidLength {
        /// Original invalid input string.
        input: String,
    },
    /// Hex string contained non-hexadecimal characters.
    #[error("color `{input}` contains invalid hexadecimal digits")]
    InvalidHexDigits {
        /// Original invalid input string.
        input: String,
    },
}

/// WCAG 2.1 contrast calculation helpers.
pub mod wcag {
    use super::RgbaColor;

    /// WCAG 2.1 conformance thresholds for contrast ratios.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum WcagLevel {
        /// WCAG AA for normal text and syntax tokens (`>= 4.5:1`).
        AaNormal,
        /// WCAG AA for large text, active UI components, and focus rings (`>= 3.0:1`).
        AaLargeOrUi,
        /// WCAG AAA for normal text (`>= 7.0:1`).
        AaaNormal,
    }

    impl WcagLevel {
        /// Minimum contrast ratio required for this conformance level.
        #[must_use]
        pub const fn threshold(self) -> f64 {
            match self {
                Self::AaNormal => 4.5,
                Self::AaLargeOrUi => 3.0,
                Self::AaaNormal => 7.0,
            }
        }
    }

    /// Computes WCAG 2.1 relative luminance (`0.0..=1.0`) for the RGB channels of `color`.
    #[must_use]
    pub fn relative_luminance(color: RgbaColor) -> f64 {
        let r = srgb_to_linear(color.r);
        let g = srgb_to_linear(color.g);
        let b = srgb_to_linear(color.b);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// Computes the WCAG 2.1 contrast ratio (`1.0..=21.0`) of `foreground` over `background`.
    ///
    /// Semi-transparent foreground colors are composited over `background` first via
    /// [`RgbaColor::over`].
    #[must_use]
    pub fn contrast_ratio(foreground: RgbaColor, background: RgbaColor) -> f64 {
        let effective_fg = foreground.over(background);
        let l1 = relative_luminance(effective_fg);
        let l2 = relative_luminance(background);
        let (brighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
        (brighter + 0.05) / (darker + 0.05)
    }

    /// Returns `true` when `foreground` over `background` meets the requested [`WcagLevel`].
    #[must_use]
    pub fn meets_wcag(foreground: RgbaColor, background: RgbaColor, level: WcagLevel) -> bool {
        contrast_ratio(foreground, background) >= level.threshold()
    }

    fn srgb_to_linear(channel: u8) -> f64 {
        let c = f64::from(channel) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ColorParseError, RgbaColor,
        wcag::{WcagLevel, contrast_ratio, meets_wcag, relative_luminance},
    };

    #[test]
    fn parses_and_formats_hex_colors() {
        let opaque = RgbaColor::from_hex("#1a202b").expect("valid 6-digit hex");
        assert_eq!(opaque, RgbaColor::rgb(0x1A, 0x20, 0x2B));
        assert_eq!(opaque.to_hex_string(), "#1A202B");

        let translucent = RgbaColor::from_hex("#39445880").expect("valid 8-digit hex");
        assert_eq!(translucent, RgbaColor::rgba(0x39, 0x44, 0x58, 0x80));
        assert_eq!(translucent.to_hex_string(), "#39445880");
    }

    #[test]
    fn rejects_invalid_hex_colors() {
        assert!(matches!(
            RgbaColor::from_hex("1A202B"),
            Err(ColorParseError::MissingHashPrefix { .. })
        ));
        assert!(matches!(
            RgbaColor::from_hex("#FFF"),
            Err(ColorParseError::InvalidLength { .. })
        ));
        assert!(matches!(
            RgbaColor::from_hex("#GGGGGG"),
            Err(ColorParseError::InvalidHexDigits { .. })
        ));
    }

    #[test]
    fn computes_exact_wcag_bounds_and_alpha_compositing() {
        let black = RgbaColor::rgb(0, 0, 0);
        let white = RgbaColor::rgb(255, 255, 255);
        assert!((relative_luminance(black) - 0.0).abs() < 1e-9);
        assert!((relative_luminance(white) - 1.0).abs() < 1e-9);
        assert!((contrast_ratio(white, black) - 21.0).abs() < 1e-6);
        assert!((contrast_ratio(black, black) - 1.0).abs() < 1e-6);
        assert!(meets_wcag(white, black, WcagLevel::AaaNormal));

        let semi = RgbaColor::rgba(255, 255, 255, 128);
        let blended = semi.over(black);
        assert_eq!(blended, RgbaColor::rgb(128, 128, 128));
    }
}
