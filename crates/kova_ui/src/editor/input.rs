//! Headless input helpers: UTF-16 ↔ UTF-8 conversion, multi-cursor paste, and latency stats.

use std::ops::Range;
use std::time::Duration;

use kova_buffer::Motion;
use kova_core::perf::budgets::INPUT_LATENCY_MAX;
use kova_core::perf::{BudgetKind, check_duration_budget};

/// Tracks input-dispatch latency against [`INPUT_LATENCY_MAX`] (`5 ms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputLatencyStats {
    /// Duration of the most recent input event dispatch.
    pub last_dispatch: Duration,
    /// Maximum observed dispatch duration since creation or reset.
    pub max_observed: Duration,
    /// Whether `last_dispatch` stayed within [`INPUT_LATENCY_MAX`].
    pub within_budget: bool,
}

impl Default for InputLatencyStats {
    fn default() -> Self {
        Self {
            last_dispatch: Duration::ZERO,
            max_observed: Duration::ZERO,
            within_budget: true,
        }
    }
}

impl InputLatencyStats {
    /// Records a new input-dispatch measurement and checks it against [`INPUT_LATENCY_MAX`].
    pub fn record(&mut self, elapsed: Duration) {
        let report = check_duration_budget(BudgetKind::InputLatency, elapsed, INPUT_LATENCY_MAX);
        self.last_dispatch = elapsed;
        if elapsed > self.max_observed {
            self.max_observed = elapsed;
        }
        self.within_budget = report.is_within_budget();
    }
}

/// Converts a UTF-16 code-unit offset within `text` to a clamped UTF-8 byte offset.
///
/// Use this only for single lines or small slices; for whole-document offsets use
/// `BufferSnapshot::utf16_to_byte` ($O(\log N)$).
#[must_use]
pub fn utf16_offset_to_utf8_byte(text: &str, utf16_offset: usize) -> usize {
    let mut utf16_count = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        if utf16_count >= utf16_offset {
            return byte_idx;
        }
        utf16_count += ch.len_utf16();
    }
    text.len()
}

/// Converts a UTF-8 byte offset within `text` to a UTF-16 code-unit offset.
///
/// If `byte_offset` lands inside a multi-byte character or past `text.len()`,
/// it is clamped to the nearest preceding char boundary (or `text.len()`).
#[must_use]
pub fn utf8_byte_to_utf16_offset(text: &str, byte_offset: usize) -> usize {
    let clamped = byte_offset.min(text.len());
    let mut utf16_count = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        if byte_idx >= clamped {
            break;
        }
        utf16_count += ch.len_utf16();
    }
    utf16_count
}

/// Converts a UTF-16 code-unit range within `text` to a UTF-8 byte range.
#[must_use]
pub fn utf16_range_to_byte_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = utf16_offset_to_utf8_byte(text, range.start);
    let end = utf16_offset_to_utf8_byte(text, range.end.max(range.start));
    start..end
}

/// Converts a UTF-8 byte range within `text` to a UTF-16 code-unit range.
#[must_use]
pub fn byte_range_to_utf16_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = utf8_byte_to_utf16_offset(text, range.start);
    let end = utf8_byte_to_utf16_offset(text, range.end.max(range.start));
    start..end
}

/// Splits `clipboard_text` across `cursor_count` cursors.
///
/// When `cursor_count > 1` and the clipboard contains exactly `cursor_count` lines,
/// each cursor receives its corresponding line; otherwise every cursor receives
/// the full `clipboard_text`.
#[must_use]
pub fn split_paste_for_cursors<'a>(clipboard_text: &'a str, cursor_count: usize) -> Vec<&'a str> {
    let count = cursor_count.max(1);
    if count > 1 {
        let lines: Vec<&'a str> = clipboard_text.lines().collect();
        if lines.len() == count {
            return lines;
        }
    }
    vec![clipboard_text; count]
}

/// Platform-neutral modifier state for [`map_editor_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "plain keyboard modifier flags mirror gpui::Modifiers"
)]
pub struct KeyMods {
    /// `Cmd` on macOS, `Ctrl` elsewhere.
    pub primary: bool,
    /// `Alt`/`Option` key.
    pub alt: bool,
    /// `Shift` key (extends the selection for motions).
    pub shift: bool,
    /// `Ctrl` on macOS or `Super` elsewhere: shortcuts the editor leaves to the app keymap.
    pub secondary: bool,
}

/// Editor command produced from a non-printable key press (Phase 0 keymap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorKeyCommand {
    /// Collapse multi-cursors or clear the selection.
    Escape,
    /// Insert a line break at every cursor.
    Newline,
    /// Insert indentation at every cursor.
    Tab,
    /// `Backspace`.
    DeleteBackward,
    /// `Delete`.
    DeleteForward,
    /// Move (or extend, when `extend` is set) every selection.
    Move {
        /// Cursor motion.
        motion: Motion,
        /// Extend the selection instead of moving the caret.
        extend: bool,
    },
    /// Add a cursor on the line above (`up`) or below.
    AddCursor {
        /// `true` for above, `false` for below.
        up: bool,
    },
    /// Page up/down.
    Page {
        /// `true` for `PageDown`.
        down: bool,
        /// Extend the selection.
        extend: bool,
    },
}

/// Maps a gpui key name plus modifiers to an [`EditorKeyCommand`].
///
/// Returns `None` for printable keys (they arrive via `EntityInputHandler`) and for
/// shortcuts left to actions or the app keymap. `macos` selects platform conventions:
/// word moves are `Alt+Left/Right` on macOS and `Ctrl+Left/Right` elsewhere; line and file
/// bounds are `Cmd+Left/Right/Up/Down` on macOS and `Home/End`, `Ctrl+Home/End` elsewhere.
#[must_use]
pub fn map_editor_key(key: &str, mods: KeyMods, macos: bool) -> Option<EditorKeyCommand> {
    if mods.secondary {
        return None;
    }
    let extend = mods.shift;
    let word = if macos { mods.alt } else { mods.primary };
    let mv = |motion| Some(EditorKeyCommand::Move { motion, extend });
    match key {
        "escape" if !mods.primary => Some(EditorKeyCommand::Escape),
        "enter" if !mods.primary => Some(EditorKeyCommand::Newline),
        "tab" if !mods.primary && !mods.shift => Some(EditorKeyCommand::Tab),
        "backspace" if !mods.primary => Some(EditorKeyCommand::DeleteBackward),
        "delete" if !mods.primary => Some(EditorKeyCommand::DeleteForward),
        "left" if macos && mods.primary => mv(Motion::LineStart),
        "right" if macos && mods.primary => mv(Motion::LineEnd),
        "left" if word => mv(Motion::WordLeft),
        "right" if word => mv(Motion::WordRight),
        "left" => mv(Motion::Left),
        "right" => mv(Motion::Right),
        "up" | "down" if mods.alt && mods.shift => {
            Some(EditorKeyCommand::AddCursor { up: key == "up" })
        }
        "up" if macos && mods.primary => mv(Motion::BufferStart),
        "down" if macos && mods.primary => mv(Motion::BufferEnd),
        "up" if !mods.primary => mv(Motion::Up),
        "down" if !mods.primary => mv(Motion::Down),
        "home" if mods.primary => mv(Motion::BufferStart),
        "end" if mods.primary => mv(Motion::BufferEnd),
        "home" => mv(Motion::LineFirstNonWhitespace),
        "end" => mv(Motion::LineEnd),
        "pageup" => Some(EditorKeyCommand::Page {
            down: false,
            extend,
        }),
        "pagedown" => Some(EditorKeyCommand::Page { down: true, extend }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_utf8_roundtrip_handles_polish_and_emoji_surrogates() {
        let line = "zażółć 🚀!";
        // 'z'(1) 'a'(1) 'ż'(1) 'ó'(1) 'ł'(1) 'ć'(1) ' '(1) '🚀'(2) '!'(1) => 10 UTF-16 units
        assert_eq!(utf8_byte_to_utf16_offset(line, line.len()), 10);
        assert_eq!(utf16_offset_to_utf8_byte(line, 10), line.len());

        let emoji_start = line.find('🚀').expect("emoji present");
        let emoji_utf16 = utf8_byte_to_utf16_offset(line, emoji_start);
        assert_eq!(emoji_utf16, 7);
        assert_eq!(utf16_offset_to_utf8_byte(line, emoji_utf16), emoji_start);
        // Past-end clamps cleanly without panicking.
        assert_eq!(utf16_offset_to_utf8_byte(line, 999), line.len());
        assert_eq!(utf8_byte_to_utf16_offset(line, 999), 10);

        let range = utf16_range_to_byte_range(line, 2..9);
        assert_eq!(&line[range.clone()], "żółć 🚀");
        assert_eq!(byte_range_to_utf16_range(line, range), 2..9);
    }

    #[test]
    fn split_paste_distributes_matching_line_count_or_broadcasts() {
        assert_eq!(
            split_paste_for_cursors("alpha\nbeta\ngamma", 3),
            vec!["alpha", "beta", "gamma"]
        );
        assert_eq!(
            split_paste_for_cursors("alpha\nbeta", 3),
            vec!["alpha\nbeta"; 3]
        );
        assert_eq!(split_paste_for_cursors("single", 1), vec!["single"]);
    }

    #[test]
    fn key_mapping_follows_platform_conventions() {
        let none = KeyMods::default();
        let shift = KeyMods {
            shift: true,
            ..KeyMods::default()
        };
        let alt = KeyMods {
            alt: true,
            ..KeyMods::default()
        };
        let primary = KeyMods {
            primary: true,
            ..KeyMods::default()
        };
        let mv = |motion, extend| Some(EditorKeyCommand::Move { motion, extend });

        assert_eq!(
            map_editor_key("a", none, true),
            None,
            "printable keys go to IME"
        );
        assert_eq!(
            map_editor_key("c", primary, true),
            None,
            "clipboard goes via actions"
        );
        assert_eq!(map_editor_key("left", shift, true), mv(Motion::Left, true));
        // Word moves: Alt on macOS, Ctrl on Linux.
        assert_eq!(
            map_editor_key("left", alt, true),
            mv(Motion::WordLeft, false)
        );
        assert_eq!(
            map_editor_key("right", primary, false),
            mv(Motion::WordRight, false)
        );
        // Line bounds: Cmd+Left/Right on macOS.
        assert_eq!(
            map_editor_key("left", primary, true),
            mv(Motion::LineStart, false)
        );
        // File bounds: Cmd+Up/Down on macOS, Ctrl+Home/End on Linux.
        assert_eq!(
            map_editor_key("down", primary, true),
            mv(Motion::BufferEnd, false)
        );
        assert_eq!(
            map_editor_key("home", primary, false),
            mv(Motion::BufferStart, false)
        );
        assert_eq!(
            map_editor_key("home", none, false),
            mv(Motion::LineFirstNonWhitespace, false)
        );
        let alt_shift = KeyMods {
            alt: true,
            shift: true,
            ..KeyMods::default()
        };
        assert_eq!(
            map_editor_key("up", alt_shift, true),
            Some(EditorKeyCommand::AddCursor { up: true })
        );
        assert_eq!(
            map_editor_key("pagedown", none, false),
            Some(EditorKeyCommand::Page {
                down: true,
                extend: false
            })
        );
        assert_eq!(
            map_editor_key("enter", none, true),
            Some(EditorKeyCommand::Newline)
        );
        let secondary = KeyMods {
            secondary: true,
            ..KeyMods::default()
        };
        assert_eq!(map_editor_key("left", secondary, true), None);
    }

    #[test]
    fn input_latency_stats_tracks_budget() {
        let mut stats = InputLatencyStats::default();
        assert!(stats.within_budget);

        stats.record(Duration::from_micros(250));
        assert!(stats.within_budget);
        assert_eq!(stats.max_observed, Duration::from_micros(250));

        stats.record(Duration::from_millis(8));
        assert!(!stats.within_budget);
        assert_eq!(stats.max_observed, Duration::from_millis(8));
    }
}
