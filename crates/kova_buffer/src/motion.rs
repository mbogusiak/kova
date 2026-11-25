//! Visual display columns, `goal_column` calculation, and cursor motion primitives.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::grapheme::slice_to_cow;
use crate::snapshot::BufferSnapshot;

/// Configuration governing visual column calculation and cursor motion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MotionConfig {
    /// Tab stop width in visual columns (values `< 1` are treated as `1`).
    pub tab_width: usize,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self { tab_width: 4 }
    }
}

impl MotionConfig {
    /// Creates a [`MotionConfig`] with the specified `tab_width` (clamped to at least `1`).
    #[must_use]
    pub const fn new(tab_width: usize) -> Self {
        Self {
            tab_width: if tab_width == 0 { 1 } else { tab_width },
        }
    }

    #[must_use]
    const fn effective_tab_width(self) -> usize {
        if self.tab_width == 0 {
            1
        } else {
            self.tab_width
        }
    }
}

/// Logical cursor/selection movement directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Motion {
    /// Move left by one extended grapheme cluster (or collapse selection to start).
    Left,
    /// Move right by one extended grapheme cluster (or collapse selection to end).
    Right,
    /// Move up one line preserving visual `goal_column`.
    Up,
    /// Move down one line preserving visual `goal_column`.
    Down,
    /// Move left to the previous code-word boundary.
    WordLeft,
    /// Move right to the next code-word boundary.
    WordRight,
    /// Move to byte column `0` of the current line.
    LineStart,
    /// Smart Home: toggle between the first non-whitespace grapheme of the line and column `0`.
    LineFirstNonWhitespace,
    /// Move to the end of the current line's content (before `\n` or `\r\n`).
    LineEnd,
    /// Move to byte offset `0`.
    BufferStart,
    /// Move to the end of the buffer (`len_bytes()`).
    BufferEnd,
}

#[must_use]
fn advance_visual_column(col: usize, grapheme: &str, tab_width: usize) -> usize {
    if grapheme == "\t" {
        col + (tab_width - (col % tab_width))
    } else {
        col + grapheme.width().max(1)
    }
}

/// Computes the 0-based visual display column at byte offset `byte` within its line.
///
/// Accounts for `\t` tab stops via `cfg.tab_width` and Unicode display widths
/// (e.g., wide CJK/emoji clusters count as width `2`; zero-width clusters advance by `1`).
#[must_use]
pub fn visual_column_at(snapshot: &BufferSnapshot, byte: usize, cfg: MotionConfig) -> usize {
    let clamped = snapshot.clamp_byte_to_char_boundary(byte);
    let row = snapshot.rope().byte_to_line(clamped);
    let line_start = snapshot.rope().line_to_byte(row);
    let content_len = snapshot.line_len_bytes(row).unwrap_or(0);
    let target_rel = (clamped - line_start).min(content_len);

    let Ok(line_slice) = snapshot.line_slice(row) else {
        return 0;
    };
    let line_str = slice_to_cow(line_slice);
    if line_str.is_ascii() && !line_str.as_bytes().contains(&b'\t') {
        return target_rel;
    }
    let tab_width = cfg.effective_tab_width();

    let mut col: usize = 0;
    for (idx, grapheme) in line_str.grapheme_indices(true) {
        if idx >= target_rel {
            break;
        }
        col = advance_visual_column(col, grapheme, tab_width);
    }
    col
}

/// Finds the closest grapheme boundary on `row` whose visual column does not exceed `target_col`
/// (or the end of the line content when the line is shorter than `target_col`).
#[must_use]
pub fn byte_at_visual_column(
    snapshot: &BufferSnapshot,
    row: usize,
    target_col: usize,
    cfg: MotionConfig,
) -> usize {
    let last_row = snapshot.len_lines().saturating_sub(1);
    let clamped_row = row.min(last_row);
    let line_start = snapshot.rope().line_to_byte(clamped_row);
    let content_len = snapshot.line_len_bytes(clamped_row).unwrap_or(0);

    if target_col == 0 {
        return line_start;
    }

    let Ok(line_slice) = snapshot.line_slice(clamped_row) else {
        return line_start;
    };
    let line_str = slice_to_cow(line_slice);
    if line_str.is_ascii() && !line_str.as_bytes().contains(&b'\t') {
        return line_start + target_col.min(content_len);
    }
    let tab_width = cfg.effective_tab_width();

    let mut col: usize = 0;
    for (idx, grapheme) in line_str.grapheme_indices(true) {
        if col >= target_col {
            return line_start + idx;
        }
        let next_col = advance_visual_column(col, grapheme, tab_width);
        if next_col > target_col {
            // Choose the boundary closer to `target_col` (prefer staying before wide/tab on tie).
            return if target_col - col <= next_col - target_col {
                line_start + idx
            } else {
                line_start + idx + grapheme.len()
            };
        }
        col = next_col;
    }

    line_start + content_len
}

/// Returns the byte offset of the first non-whitespace grapheme on `row`,
/// or `line_start + content_len` if the line is empty or whitespace-only.
#[must_use]
pub(crate) fn line_first_non_whitespace_byte(snapshot: &BufferSnapshot, row: usize) -> usize {
    let last_row = snapshot.len_lines().saturating_sub(1);
    let clamped_row = row.min(last_row);
    let line_start = snapshot.rope().line_to_byte(clamped_row);
    let content_len = snapshot.line_len_bytes(clamped_row).unwrap_or(0);

    let Ok(line_slice) = snapshot.line_slice(clamped_row) else {
        return line_start;
    };
    let line_str = slice_to_cow(line_slice);

    for (idx, grapheme) in line_str.grapheme_indices(true) {
        if !grapheme.chars().all(char::is_whitespace) {
            return line_start + idx;
        }
    }
    line_start + content_len
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;

    #[test]
    fn visual_column_handles_tabs_cjk_and_zero_width_clusters() {
        // "\tab🚀x\n" -> '\t' (cols 0..4), 'a' (4..5), 'b' (5..6), '🚀' (6..8), 'x' (8..9)
        let buf = Buffer::from_text("\tab🚀x\n\u{200B}z\n");
        let snap = buf.snapshot();
        let cfg = MotionConfig::new(4);

        assert_eq!(visual_column_at(&snap, 0, cfg), 0);
        assert_eq!(visual_column_at(&snap, 1, cfg), 4); // after \t
        assert_eq!(visual_column_at(&snap, 3, cfg), 6); // before 🚀
        assert_eq!(visual_column_at(&snap, 7, cfg), 8); // after 🚀
        assert_eq!(visual_column_at(&snap, 8, cfg), 9); // after x

        assert_eq!(byte_at_visual_column(&snap, 0, 4, cfg), 1);
        assert_eq!(byte_at_visual_column(&snap, 0, 6, cfg), 3);
        assert_eq!(byte_at_visual_column(&snap, 0, 99, cfg), 8);

        // Zero-width formatting char \u{200B} (3 bytes) counts as width 1 so columns remain distinct
        let line1_start = snap.rope().line_to_byte(1);
        assert_eq!(visual_column_at(&snap, line1_start, cfg), 0);
        assert_eq!(visual_column_at(&snap, line1_start + 3, cfg), 1);
        assert_eq!(byte_at_visual_column(&snap, 1, 1, cfg), line1_start + 3);
    }
}
