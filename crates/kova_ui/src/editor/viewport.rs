//! Pure headless viewport virtualization math for the 120 FPS editor.
//!
//! All functions in this module are deterministic, allocation-free, and tested
//! without a GPU window.

use std::ops::Range;

/// Default editor code font size in logical pixels (`editor_ux_guidelines.md` §2.1).
pub const DEFAULT_EDITOR_FONT_SIZE_PX: f32 = 13.0;

/// Default gutter line-number font size in logical pixels (`font_size - 1.0`).
pub const DEFAULT_GUTTER_FONT_SIZE_PX: f32 = 12.0;

/// Canonical line-height multiplier (`round(font_size * 1.45)` -> `19.0 px` for `13 px`).
pub const LINE_HEIGHT_FACTOR: f32 = 1.45;

/// Number of extra lines shaped above and below the visible viewport to absorb fast scrolls.
pub const OVERSCAN_LINES: usize = 2;

/// Maximum UTF-8 bytes shaped for a single unwrapped line before horizontal windowing activates.
pub const MAX_SHAPED_BYTES_PER_LINE: usize = 16_384;

/// Minimum reserved digit columns in the gutter (`< 1000` lines still reserve 3 digits).
pub const MIN_GUTTER_DIGITS: usize = 3;

/// Left padding of the editor island gutter (`10 px`).
pub const GUTTER_LEFT_PAD_PX: f32 = 10.0;

/// Minimum width of the line-number column (`28 px`).
pub const GUTTER_MIN_DIGITS_WIDTH_PX: f32 = 28.0;

/// Gap between line numbers and interactive gutter slots (`6 px`).
pub const GUTTER_DIGITS_GAP_PX: f32 = 6.0;

/// Reserved width for the Run / Debug / Breakpoint icon slot (`18 px`).
pub const GUTTER_RUN_SLOT_PX: f32 = 18.0;

/// Reserved width for the code-folding chevron slot (`14 px`).
pub const GUTTER_FOLD_SLOT_PX: f32 = 14.0;

/// Reserved width for the Git diff marker bar slot (`6 px`).
pub const GUTTER_GIT_BAR_SLOT_PX: f32 = 6.0;

/// Right padding between the Git diff slot and the first code column (`10 px`).
pub const GUTTER_RIGHT_PAD_PX: f32 = 10.0;

/// Overlay scrollbar thumb width (`6 px`).
pub const SCROLLBAR_THUMB_WIDTH_PX: f32 = 6.0;

/// Overlay scrollbar thumb corner radius (`3 px`).
pub const SCROLLBAR_THUMB_RADIUS_PX: f32 = 3.0;

/// Horizontal inset from the right edge of the editor island (`4 px`).
pub const SCROLLBAR_RIGHT_MARGIN_PX: f32 = 4.0;

/// Minimum vertical scrollbar thumb height (`24 px`) so 1M-line files remain draggable.
pub const SCROLLBAR_MIN_THUMB_LEN_PX: f32 = 24.0;

/// Quantized viewport metrics used to compute $O(1)$ visible line slices.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportMetrics {
    /// Fixed quantized row height in logical pixels (e.g. `19.0` for `13 px` font).
    pub line_height: f32,
    /// Monospace character advance in logical pixels.
    pub char_advance: f32,
    /// Width of the text content area (excluding gutter) in logical pixels.
    pub viewport_width: f32,
    /// Height of the editor viewport in logical pixels.
    pub viewport_height: f32,
    /// Extra lines shaped above and below the visible viewport.
    pub overscan_lines: usize,
}

impl ViewportMetrics {
    /// Computes the quantized integer line height (`round(font_size * 1.45)`).
    #[must_use]
    pub fn quantized_line_height(font_size_px: f32) -> f32 {
        if !font_size_px.is_finite() || font_size_px <= 0.0 {
            return (DEFAULT_EDITOR_FONT_SIZE_PX * LINE_HEIGHT_FACTOR).round();
        }
        (font_size_px * LINE_HEIGHT_FACTOR).round().max(1.0)
    }

    /// Creates a new [`ViewportMetrics`] with [`OVERSCAN_LINES`] overscan.
    #[must_use]
    pub fn new(
        line_height: f32,
        char_advance: f32,
        viewport_width: f32,
        viewport_height: f32,
    ) -> Self {
        Self {
            line_height: if line_height.is_finite() && line_height > 0.0 {
                line_height
            } else {
                1.0
            },
            char_advance: if char_advance.is_finite() && char_advance > 0.0 {
                char_advance
            } else {
                1.0
            },
            viewport_width: if viewport_width.is_finite() {
                viewport_width.max(0.0)
            } else {
                0.0
            },
            viewport_height: if viewport_height.is_finite() {
                viewport_height.max(0.0)
            } else {
                0.0
            },
            overscan_lines: OVERSCAN_LINES,
        }
    }

    /// Returns the maximum valid vertical scroll offset in logical pixels.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "line counts in practical files fit cleanly within f32 mantissa"
    )]
    pub fn max_scroll_y(&self, total_lines: usize) -> f32 {
        (total_lines.saturating_sub(1) as f32) * self.line_height
    }

    /// Clamps `scroll_y` into `[0.0, max_scroll_y(total_lines)]`.
    #[must_use]
    pub fn clamp_scroll_y(&self, scroll_y: f32, total_lines: usize) -> f32 {
        if !scroll_y.is_finite() {
            return 0.0;
        }
        scroll_y.clamp(0.0, self.max_scroll_y(total_lines))
    }

    /// Clamps `scroll_x` into `[0.0, max_scroll_x.max(0.0)]`.
    #[must_use]
    pub fn clamp_scroll_x(&self, scroll_x: f32, max_scroll_x: f32) -> f32 {
        if !scroll_x.is_finite() {
            return 0.0;
        }
        let max_x = if max_scroll_x.is_finite() {
            max_scroll_x.max(0.0)
        } else {
            0.0
        };
        scroll_x.clamp(0.0, max_x)
    }

    /// Computes the half-open range of 0-based line indices visible at `scroll_y`
    /// (including [`Self::overscan_lines`] above and below).
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped_y / line_height is guaranteed non-negative and bounded by total_lines"
    )]
    pub fn visible_line_range(&self, scroll_y: f32, total_lines: usize) -> Range<usize> {
        if total_lines == 0 || self.viewport_height <= 0.0 {
            return 0..0;
        }

        let clamped_y = self.clamp_scroll_y(scroll_y, total_lines);
        let first_visible = (clamped_y / self.line_height).floor().max(0.0) as usize;
        let first = first_visible.saturating_sub(self.overscan_lines);

        let last_visible = ((clamped_y + self.viewport_height) / self.line_height)
            .ceil()
            .max(0.0) as usize;
        let last = last_visible
            .saturating_add(self.overscan_lines)
            .min(total_lines);

        first.min(last)..last
    }

    /// Returns the number of digit columns reserved in the gutter (`>= 3`).
    #[must_use]
    pub fn gutter_digit_count(total_lines: usize) -> usize {
        let digits = total_lines.max(1).ilog10() as usize + 1;
        digits.max(MIN_GUTTER_DIGITS)
    }

    /// Computes total gutter width in logical pixels, reserving all Phase 0..3 slots.
    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        reason = "digit count is a small integer (< 20)"
    )]
    pub fn gutter_width_px(total_lines: usize, gutter_char_advance: f32) -> f32 {
        let digits = Self::gutter_digit_count(total_lines);
        let advance = if gutter_char_advance.is_finite() && gutter_char_advance > 0.0 {
            gutter_char_advance
        } else {
            7.2
        };
        let digits_width = (digits as f32 * advance).max(GUTTER_MIN_DIGITS_WIDTH_PX);
        GUTTER_LEFT_PAD_PX
            + digits_width
            + GUTTER_DIGITS_GAP_PX
            + GUTTER_RUN_SLOT_PX
            + GUTTER_FOLD_SLOT_PX
            + GUTTER_GIT_BAR_SLOT_PX
            + GUTTER_RIGHT_PAD_PX
    }

    /// Returns `(byte_range, x_offset_px)` for shaping a single line safely within frame budget.
    ///
    /// Lines up to [`MAX_SHAPED_BYTES_PER_LINE`] are returned in full (`0..line.len()`, `0.0`).
    /// Longer lines (e.g. minified JSON/JS) are sliced to a viewport-sized window aligned
    /// strictly to UTF-8 character boundaries (MSRV 1.85 compatible).
    #[must_use]
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "pixel-to-byte window indices are clamped to string length and non-negative"
    )]
    pub fn visible_utf8_window(
        line: &str,
        scroll_x: f32,
        char_advance: f32,
        viewport_width: f32,
    ) -> (Range<usize>, f32) {
        if line.len() <= MAX_SHAPED_BYTES_PER_LINE {
            return (0..line.len(), 0.0);
        }

        let advance = if char_advance.is_finite() && char_advance > 0.0 {
            char_advance
        } else {
            1.0
        };
        let safe_scroll_x = if scroll_x.is_finite() {
            scroll_x.max(0.0)
        } else {
            0.0
        };
        let safe_width = if viewport_width.is_finite() {
            viewport_width.max(0.0)
        } else {
            0.0
        };

        let start_est = ((safe_scroll_x / advance).floor() as usize).saturating_sub(32);
        let start_byte = floor_char_boundary(line, start_est);

        let visible_cols = ((safe_width / advance).ceil() as usize).saturating_add(64);
        let window_bytes = visible_cols
            .saturating_mul(4)
            .clamp(256, MAX_SHAPED_BYTES_PER_LINE);
        let end_est = start_byte.saturating_add(window_bytes).min(line.len());
        let end_byte = ceil_char_boundary(line, end_est);

        let x_offset_px = start_byte as f32 * advance;
        (start_byte..end_byte.max(start_byte), x_offset_px)
    }

    /// Computes vertical overlay scrollbar `(thumb_y, thumb_height)` in logical pixels,
    /// or `None` when the entire file fits inside the viewport.
    #[must_use]
    pub fn vertical_scrollbar_thumb(
        &self,
        scroll_y: f32,
        total_lines: usize,
    ) -> Option<(f32, f32)> {
        let max_scroll = self.max_scroll_y(total_lines);
        if max_scroll <= 0.0 || self.viewport_height <= 0.0 {
            return None;
        }

        let total_scrollable_height = max_scroll + self.viewport_height;
        let thumb_height = ((self.viewport_height / total_scrollable_height)
            * self.viewport_height)
            .max(SCROLLBAR_MIN_THUMB_LEN_PX)
            .min(self.viewport_height);

        let clamped_y = self.clamp_scroll_y(scroll_y, total_lines);
        let scroll_ratio = (clamped_y / max_scroll).clamp(0.0, 1.0);
        let thumb_y = scroll_ratio * (self.viewport_height - thumb_height);

        Some((thumb_y, thumb_height))
    }
}

/// MSRV 1.85-compatible `floor_char_boundary` using `str::is_char_boundary`.
#[must_use]
fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// MSRV 1.85-compatible `ceil_char_boundary` using `str::is_char_boundary`.
#[must_use]
fn ceil_char_boundary(s: &str, index: usize) -> usize {
    let mut i = index.min(s.len());
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantized_line_height_matches_guidelines() {
        assert!((ViewportMetrics::quantized_line_height(13.0) - 19.0).abs() < f32::EPSILON);
        assert!((ViewportMetrics::quantized_line_height(14.0) - 20.0).abs() < f32::EPSILON);
        assert!((ViewportMetrics::quantized_line_height(12.0) - 17.0).abs() < f32::EPSILON);
    }

    #[test]
    fn visible_line_range_handles_empty_and_single_line_buffer() {
        let metrics = ViewportMetrics::new(19.0, 7.8, 800.0, 600.0);
        assert_eq!(metrics.visible_line_range(0.0, 0), 0..0);
        assert_eq!(metrics.visible_line_range(0.0, 1), 0..1);
        assert_eq!(metrics.visible_line_range(-50.0, 1), 0..1);
        assert_eq!(metrics.visible_line_range(500.0, 1), 0..1);
    }

    #[test]
    fn visible_line_range_scales_in_o1_for_one_million_lines() {
        let metrics = ViewportMetrics::new(19.0, 7.8, 1200.0, 800.0);
        let total = 1_000_000;

        let top = metrics.visible_line_range(0.0, total);
        assert_eq!(top.start, 0);
        assert!(top.len() <= 46);

        let mid_scroll = 500_000.0 * 19.0;
        let mid = metrics.visible_line_range(mid_scroll, total);
        assert_eq!(mid.start, 500_000 - OVERSCAN_LINES);
        assert!(mid.len() <= 48);

        let end_scroll = metrics.max_scroll_y(total) + 10_000.0;
        let end = metrics.visible_line_range(end_scroll, total);
        assert_eq!(end.end, total);
        assert!(end.len() <= 46);
    }

    #[test]
    fn clamp_scroll_handles_bounds_and_non_finite() {
        let metrics = ViewportMetrics::new(19.0, 7.8, 800.0, 600.0);
        assert!((metrics.clamp_scroll_y(-10.0, 100) - 0.0).abs() < f32::EPSILON);
        assert!((metrics.clamp_scroll_y(f32::NAN, 100) - 0.0).abs() < f32::EPSILON);
        assert!((metrics.clamp_scroll_y(f32::INFINITY, 100) - 0.0).abs() < f32::EPSILON);
        assert!((metrics.clamp_scroll_y(99_999.0, 100) - 99.0 * 19.0).abs() < f32::EPSILON);

        assert!((metrics.clamp_scroll_x(-5.0, 200.0) - 0.0).abs() < f32::EPSILON);
        assert!((metrics.clamp_scroll_x(500.0, 200.0) - 200.0).abs() < f32::EPSILON);
    }

    #[test]
    fn gutter_digits_and_width_reserve_minimum_92px() {
        assert_eq!(ViewportMetrics::gutter_digit_count(1), 3);
        assert_eq!(ViewportMetrics::gutter_digit_count(999), 3);
        assert_eq!(ViewportMetrics::gutter_digit_count(1000), 4);
        assert_eq!(ViewportMetrics::gutter_digit_count(1_000_000), 7);

        let w_small = ViewportMetrics::gutter_width_px(10, 7.2);
        assert!((w_small - 92.0).abs() < f32::EPSILON);

        let w_million = ViewportMetrics::gutter_width_px(1_000_000, 7.2);
        assert!(w_million > w_small);
    }

    #[test]
    fn visible_utf8_window_never_splits_multibyte_chars_or_emoji() {
        // Build a > 20 KB line with Polish diacritics (2 bytes) and 4-byte emoji.
        let chunk = "zażółć gęślą jaźń 🚀👨‍👩‍👧‍👦 ";
        let long_line = chunk.repeat(600);
        assert!(long_line.len() > MAX_SHAPED_BYTES_PER_LINE);

        for scroll_step in 0_u16..40_u16 {
            let scroll_x = f32::from(scroll_step) * 173.0;
            let (range, x_offset) =
                ViewportMetrics::visible_utf8_window(&long_line, scroll_x, 7.8, 900.0);
            assert!(range.start <= range.end);
            assert!(range.end <= long_line.len());
            assert!(long_line.is_char_boundary(range.start));
            assert!(long_line.is_char_boundary(range.end));
            assert!(x_offset >= 0.0);
            assert_ne!(&long_line[range], "");
        }
    }

    #[test]
    fn vertical_scrollbar_thumb_respects_min_height_for_huge_files() {
        let metrics = ViewportMetrics::new(19.0, 7.8, 800.0, 600.0);
        assert_eq!(metrics.vertical_scrollbar_thumb(0.0, 1), None);

        let (thumb_y_top, thumb_h) = metrics
            .vertical_scrollbar_thumb(0.0, 1_000_000)
            .expect("1M lines requires scrollbar");
        assert!((thumb_y_top - 0.0).abs() < f32::EPSILON);
        assert!((thumb_h - SCROLLBAR_MIN_THUMB_LEN_PX).abs() < f32::EPSILON);

        let (thumb_y_end, _) = metrics
            .vertical_scrollbar_thumb(metrics.max_scroll_y(1_000_000), 1_000_000)
            .expect("1M lines requires scrollbar");
        assert!((thumb_y_end - (600.0 - thumb_h)).abs() < 1e-3);
    }
}
