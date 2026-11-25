//! Unicode UAX #29 extended grapheme cluster and code-word boundary navigation.

use std::borrow::Cow;

use ropey::RopeSlice;
use unicode_segmentation::UnicodeSegmentation;

use crate::snapshot::BufferSnapshot;

/// Affinity used when snapping an arbitrary byte offset to a grapheme cluster boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BoundaryAffinity {
    /// Snap down to the nearest grapheme boundary at or before the byte offset.
    #[default]
    Floor,
    /// Snap up to the nearest grapheme boundary at or after the byte offset.
    Ceil,
    /// Snap to the closer grapheme boundary (ties break toward [`BoundaryAffinity::Floor`]).
    Nearest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordClass {
    Word,
    Whitespace,
    Punctuation,
}

/// Converts a [`RopeSlice`] into a `&str` without allocation when single-chunk, or an owned `String` otherwise.
#[must_use]
pub(crate) fn slice_to_cow(slice: RopeSlice<'_>) -> Cow<'_, str> {
    slice
        .as_str()
        .map_or_else(|| Cow::Owned(slice.to_string()), Cow::Borrowed)
}

/// Snaps `byte` to a valid extended grapheme cluster boundary in `snapshot`.
///
/// Out-of-bounds offsets are clamped to `snapshot.len_bytes()`, and offsets splitting
/// multi-byte UTF-8 sequences or `\r\n` line breaks are safely aligned.
#[must_use]
pub fn snap_byte_to_grapheme_boundary(
    snapshot: &BufferSnapshot,
    byte: usize,
    affinity: BoundaryAffinity,
) -> usize {
    let clamped = snapshot.clamp_byte_to_char_boundary(byte);
    let row = snapshot.rope().byte_to_line(clamped);
    let line_start = snapshot.rope().line_to_byte(row);
    let content_len = snapshot.line_len_bytes(row).unwrap_or(0);
    let content_end = line_start + content_len;

    if clamped >= content_end {
        return content_end;
    }

    let Ok(line_slice) = snapshot.line_slice(row) else {
        return content_end;
    };
    let line_str = slice_to_cow(line_slice);
    let rel = clamped - line_start;
    if line_str.is_ascii() {
        return line_start + rel;
    }

    let mut prev_start: usize = 0;
    for (idx, grapheme) in line_str.grapheme_indices(true) {
        let g_end = idx + grapheme.len();
        if rel == idx {
            return line_start + idx;
        }
        if rel < g_end {
            let chosen = match affinity {
                BoundaryAffinity::Floor => idx,
                BoundaryAffinity::Ceil => g_end,
                BoundaryAffinity::Nearest => {
                    if rel - idx <= g_end - rel {
                        idx
                    } else {
                        g_end
                    }
                }
            };
            return line_start + chosen;
        }
        prev_start = g_end;
    }

    line_start + prev_start
}

/// Returns the byte offset of the previous extended grapheme cluster boundary before `byte`.
///
/// Stepping backward from the start of a line moves before the preceding line's `\n` or `\r\n`
/// separator to the end of that line's content. At byte `0`, returns `0`.
#[must_use]
pub fn prev_grapheme_boundary(snapshot: &BufferSnapshot, byte: usize) -> usize {
    let aligned = snap_byte_to_grapheme_boundary(snapshot, byte, BoundaryAffinity::Floor);
    if aligned == 0 {
        return 0;
    }

    let row = snapshot.rope().byte_to_line(aligned);
    let line_start = snapshot.rope().line_to_byte(row);

    if aligned == line_start {
        if row == 0 {
            return 0;
        }
        let prev_row = row - 1;
        let prev_start = snapshot.rope().line_to_byte(prev_row);
        let prev_len = snapshot.line_len_bytes(prev_row).unwrap_or(0);
        return prev_start + prev_len;
    }

    let Ok(line_slice) = snapshot.line_slice(row) else {
        return line_start;
    };
    let line_str = slice_to_cow(line_slice);
    let rel = aligned - line_start;

    let mut prev = 0;
    for (idx, _) in line_str.grapheme_indices(true) {
        if idx >= rel {
            break;
        }
        prev = idx;
    }
    line_start + prev
}

/// Returns the byte offset of the next extended grapheme cluster boundary after `byte`.
///
/// Stepping forward from the end of a line's content jumps past its `\n` or `\r\n` separator
/// to the start of the following line. At `snapshot.len_bytes()`, returns `snapshot.len_bytes()`.
#[must_use]
pub fn next_grapheme_boundary(snapshot: &BufferSnapshot, byte: usize) -> usize {
    let len = snapshot.len_bytes();
    let aligned = snap_byte_to_grapheme_boundary(snapshot, byte, BoundaryAffinity::Floor);
    if aligned >= len {
        return len;
    }

    let row = snapshot.rope().byte_to_line(aligned);
    let line_start = snapshot.rope().line_to_byte(row);
    let content_len = snapshot.line_len_bytes(row).unwrap_or(0);
    let content_end = line_start + content_len;

    if aligned >= content_end {
        return if row + 1 < snapshot.len_lines() {
            snapshot.rope().line_to_byte(row + 1)
        } else {
            len
        };
    }

    let Ok(line_slice) = snapshot.line_slice(row) else {
        return content_end;
    };
    let line_str = slice_to_cow(line_slice);
    let rel = aligned - line_start;

    for (idx, grapheme) in line_str.grapheme_indices(true) {
        let g_end = idx + grapheme.len();
        if g_end > rel {
            return line_start + g_end;
        }
    }
    content_end
}

/// Returns the byte offset of the previous code-word boundary before `byte`.
///
/// Consecutive horizontal whitespace is skipped, then consecutive graphemes of the same
/// lexical class (`Word` for alphanumeric/`_`/`$` identifiers vs `Punctuation`) are traversed.
#[must_use]
pub fn prev_word_boundary(snapshot: &BufferSnapshot, byte: usize) -> usize {
    let aligned = snap_byte_to_grapheme_boundary(snapshot, byte, BoundaryAffinity::Floor);
    if aligned == 0 {
        return 0;
    }

    let row = snapshot.rope().byte_to_line(aligned);
    let line_start = snapshot.rope().line_to_byte(row);

    if aligned == line_start {
        return prev_grapheme_boundary(snapshot, aligned);
    }

    let Ok(line_slice) = snapshot.line_slice(row) else {
        return line_start;
    };
    let line_str = slice_to_cow(line_slice);
    let rel = aligned - line_start;

    let clusters: Vec<(usize, WordClass)> = line_str
        .grapheme_indices(true)
        .take_while(|&(idx, _)| idx < rel)
        .map(|(idx, g)| (idx, classify_grapheme(g)))
        .collect();

    let Some(&(last_idx, mut target_class)) = clusters.last() else {
        return line_start;
    };

    let mut cursor = clusters.len();
    if target_class == WordClass::Whitespace {
        while cursor > 0 && clusters[cursor - 1].1 == WordClass::Whitespace {
            cursor -= 1;
        }
        if cursor == 0 {
            return line_start;
        }
        target_class = clusters[cursor - 1].1;
    } else {
        let _ = last_idx;
    }

    while cursor > 0 && clusters[cursor - 1].1 == target_class {
        cursor -= 1;
    }
    line_start + clusters[cursor].0
}

/// Returns the byte offset of the next code-word boundary after `byte`.
///
/// Consecutive horizontal whitespace is skipped, then consecutive graphemes of the same
/// lexical class (`Word` for alphanumeric/`_`/`$` identifiers vs `Punctuation`) are traversed.
#[must_use]
pub fn next_word_boundary(snapshot: &BufferSnapshot, byte: usize) -> usize {
    let len = snapshot.len_bytes();
    let aligned = snap_byte_to_grapheme_boundary(snapshot, byte, BoundaryAffinity::Floor);
    if aligned >= len {
        return len;
    }

    let row = snapshot.rope().byte_to_line(aligned);
    let line_start = snapshot.rope().line_to_byte(row);
    let content_len = snapshot.line_len_bytes(row).unwrap_or(0);
    let content_end = line_start + content_len;

    if aligned >= content_end {
        return next_grapheme_boundary(snapshot, aligned);
    }

    let Ok(line_slice) = snapshot.line_slice(row) else {
        return content_end;
    };
    let line_str = slice_to_cow(line_slice);
    let rel = aligned - line_start;

    let mut iter = line_str
        .grapheme_indices(true)
        .filter(|&(idx, _)| idx >= rel)
        .peekable();

    while let Some(&(_, g)) = iter.peek() {
        if classify_grapheme(g) == WordClass::Whitespace {
            let _ = iter.next();
        } else {
            break;
        }
    }

    let Some((_, first_non_ws)) = iter.next() else {
        return content_end;
    };
    let target_class = classify_grapheme(first_non_ws);

    while let Some(&(idx, g)) = iter.peek() {
        if classify_grapheme(g) == target_class {
            let _ = iter.next();
        } else {
            return line_start + idx;
        }
    }

    content_end
}

fn classify_grapheme(grapheme: &str) -> WordClass {
    let mut chars = grapheme.chars();
    let Some(first) = chars.next() else {
        return WordClass::Punctuation;
    };
    if grapheme.chars().all(char::is_whitespace) {
        return WordClass::Whitespace;
    }
    if first.is_alphanumeric() || first == '_' || first == '$' {
        return WordClass::Word;
    }
    WordClass::Punctuation
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buffer;

    #[test]
    fn navigates_combining_marks_zwj_flags_and_crlf() {
        // "e\u{0301}" (3B) + "👨‍👩‍👧‍👦" (25B) + "🇵🇱" (8B) + "\r\n" (2B) + "x" (1B)
        let text = "e\u{0301}👨‍👩‍👧‍👦🇵🇱\r\nx";
        let buf = Buffer::from_text(text);
        let snap = buf.snapshot();

        assert_eq!(next_grapheme_boundary(&snap, 0), 3);
        assert_eq!(next_grapheme_boundary(&snap, 3), 28);
        assert_eq!(next_grapheme_boundary(&snap, 28), 36);
        // From end of line 0 content (36), jumps past \r\n (2 bytes) to 'x' at 38
        assert_eq!(next_grapheme_boundary(&snap, 36), 38);
        assert_eq!(next_grapheme_boundary(&snap, 38), 39);
        assert_eq!(next_grapheme_boundary(&snap, 39), 39);

        // Backward traversal mirrors forward boundaries
        assert_eq!(prev_grapheme_boundary(&snap, 39), 38);
        assert_eq!(prev_grapheme_boundary(&snap, 38), 36);
        assert_eq!(prev_grapheme_boundary(&snap, 36), 28);
        assert_eq!(prev_grapheme_boundary(&snap, 28), 3);
        assert_eq!(prev_grapheme_boundary(&snap, 3), 0);
        assert_eq!(prev_grapheme_boundary(&snap, 0), 0);
    }

    #[test]
    fn snaps_interior_offsets_and_out_of_bounds_safely() {
        // "e\u{0301}\u{0302}" = 1 + 2 + 2 = 5 bytes, single grapheme cluster
        let buf = Buffer::from_text("e\u{0301}\u{0302}a\r\nb");
        let snap = buf.snapshot();

        // Offset 2 clamps down to char boundary 1 (closer to 0 than 5)
        assert_eq!(
            snap_byte_to_grapheme_boundary(&snap, 2, BoundaryAffinity::Floor),
            0
        );
        assert_eq!(
            snap_byte_to_grapheme_boundary(&snap, 2, BoundaryAffinity::Ceil),
            5
        );
        assert_eq!(
            snap_byte_to_grapheme_boundary(&snap, 2, BoundaryAffinity::Nearest),
            0
        );
        // Offset 3 is char boundary of \u{0302} (closer to 5 than 0)
        assert_eq!(
            snap_byte_to_grapheme_boundary(&snap, 3, BoundaryAffinity::Nearest),
            5
        );
        // Offset 7 splits \r\n (bytes 6..8)
        assert_eq!(
            snap_byte_to_grapheme_boundary(&snap, 7, BoundaryAffinity::Ceil),
            6
        );
        // Out of bounds clamps cleanly without panic
        assert_eq!(
            snap_byte_to_grapheme_boundary(&snap, 999, BoundaryAffinity::Floor),
            snap.len_bytes()
        );
    }

    #[test]
    fn navigates_long_multichunk_lines_without_panicking() {
        let mut long = "a".repeat(2500);
        long.push_str("e\u{0301}");
        long.push_str(&"b".repeat(2500));
        let buf = Buffer::from_text(&long);
        let snap = buf.snapshot();

        assert_eq!(next_grapheme_boundary(&snap, 2500), 2503);
        assert_eq!(prev_grapheme_boundary(&snap, 2503), 2500);
    }

    #[test]
    fn word_boundaries_respect_jvm_identifiers() {
        let buf = Buffer::from_text("  foo_bar.$baz()");
        let snap = buf.snapshot();

        assert_eq!(next_word_boundary(&snap, 0), 9); // "  foo_bar"
        assert_eq!(next_word_boundary(&snap, 9), 10); // "."
        assert_eq!(next_word_boundary(&snap, 10), 14); // "$baz"
        assert_eq!(next_word_boundary(&snap, 14), 16); // "()"

        assert_eq!(prev_word_boundary(&snap, 16), 14);
        assert_eq!(prev_word_boundary(&snap, 14), 10);
        assert_eq!(prev_word_boundary(&snap, 10), 9);
        assert_eq!(prev_word_boundary(&snap, 9), 2);
        assert_eq!(prev_word_boundary(&snap, 2), 0);
    }
}
