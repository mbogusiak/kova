//! Headless multi-cursor clipboard model, history ring, and Copy/Cut/Paste operations.
//!
//! Operates strictly in memory without OS windowing or native clipboard dependencies
//! (`arboard`, `objc`, `wayland`, `x11`), exposing plain `String` payloads for synchronization
//! with the OS clipboard in `kova_ui`/`kova_app`.

use std::time::Instant;

use ropey::Rope;

use crate::buffer::Buffer;
use crate::error::HistoryError;
use crate::history::{EditOrigin, History};
use crate::line_ending::{line_content_len_bytes, line_ending_len_bytes};
use crate::position::{BufferVersion, TextEdit};
use crate::selection::{Selection, SelectionSet};
use crate::snapshot::BufferSnapshot;

/// Default maximum number of entries retained in [`MultiCursorClipboard`].
pub const DEFAULT_MAX_CLIPBOARD_ENTRIES: usize = 32;

/// A single clipboard item capturing both flattened OS text and per-cursor segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardEntry {
    /// Full flattened text (segments joined by line separator for OS clipboard interoperability).
    pub text: String,
    /// Per-cursor slices in ascending document order (at least 1 element when non-empty).
    pub segments: Vec<String>,
    /// `true` when produced by whole-line copy/cut (empty selections with `linewise_on_empty`).
    pub linewise: bool,
}

/// In-memory multi-cursor clipboard ring buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiCursorClipboard {
    max_entries: usize,
    entries: Vec<ClipboardEntry>,
}

impl Default for MultiCursorClipboard {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_CLIPBOARD_ENTRIES)
    }
}

impl MultiCursorClipboard {
    /// Creates an empty [`MultiCursorClipboard`] retaining up to `max_entries` entries.
    #[must_use]
    pub const fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            entries: Vec::new(),
        }
    }

    /// Returns the maximum number of entries retained in the ring.
    #[must_use]
    pub const fn max_entries(&self) -> usize {
        self.max_entries
    }

    /// Returns the number of entries currently stored in the ring.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns `true` if no entries have been pushed yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Pushes `entry` as the newest clipboard item, evicting the oldest if at capacity.
    pub fn push(&mut self, entry: ClipboardEntry) {
        if self.max_entries == 0 {
            return;
        }
        if self.entries.len() >= self.max_entries {
            self.entries.truncate(self.max_entries - 1);
        }
        self.entries.insert(0, entry);
    }

    /// Returns the most recently pushed [`ClipboardEntry`], if any.
    #[must_use]
    pub fn latest(&self) -> Option<&ClipboardEntry> {
        self.entries.first()
    }

    /// Returns all retained entries ordered from newest (`[0]`) to oldest.
    #[must_use]
    pub fn history(&self) -> &[ClipboardEntry] {
        &self.entries
    }

    /// Clears all entries from the clipboard history.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Wraps plain text imported from the operating system clipboard into a [`ClipboardEntry`].
    ///
    /// Preserves `\r\n` and `\n` separators so [`resolve_paste_texts`] can split lines across
    /// multiple cursors when the line count matches the active cursor count.
    #[must_use]
    pub fn from_external_os_text(text: &str) -> ClipboardEntry {
        ClipboardEntry {
            text: text.to_owned(),
            segments: vec![text.to_owned()],
            linewise: false,
        }
    }

    /// Imports plain text from the OS clipboard and pushes it onto the ring.
    ///
    /// Returns `None` if `max_entries == 0`.
    pub fn push_external_os_text(&mut self, text: &str) -> Option<&ClipboardEntry> {
        let entry = Self::from_external_os_text(text);
        self.push(entry);
        self.latest()
    }
}

/// Copies the active selections (or unique cursor lines when all selections are empty and
/// `linewise_on_empty` is `true`) from `snapshot` and pushes the resulting entry to `clipboard`.
///
/// Returns `None` without modifying `clipboard` if all selections are empty and
/// `linewise_on_empty` is `false`.
pub fn copy_selections(
    clipboard: &mut MultiCursorClipboard,
    snapshot: &BufferSnapshot,
    selections: &SelectionSet,
    linewise_on_empty: bool,
) -> Option<ClipboardEntry> {
    let entry = extract_clipboard_entry(snapshot, selections, linewise_on_empty)?;
    clipboard.push(entry.clone());
    Some(entry)
}

/// Cuts the active selections (or whole lines when all selections are empty and
/// `linewise_on_empty` is `true`), recording an atomic transaction in `history`,
/// updating `selections` and `buffer`, and pushing the cut entry to `clipboard`.
///
/// Returns `Ok(true)` if content was copied and removed, or `Ok(false)` if there was nothing
/// to cut (all selections empty with `linewise_on_empty == false`, or empty buffer).
///
/// # Errors
///
/// Propagates any [`HistoryError`] from [`History::record_and_apply`] without modifying
/// `clipboard`.
pub fn cut_selections(
    clipboard: &mut MultiCursorClipboard,
    buffer: &mut Buffer,
    selections: &mut SelectionSet,
    history: &mut History,
    linewise_on_empty: bool,
    now: Instant,
) -> Result<bool, HistoryError> {
    let pre_snap = buffer.snapshot();
    let Some(entry) = extract_clipboard_entry(&pre_snap, selections, linewise_on_empty) else {
        return Ok(false);
    };

    let edits: Vec<TextEdit> = if entry.linewise {
        build_linewise_delete_edits(pre_snap.rope(), selections)
    } else {
        selections
            .as_slice()
            .iter()
            .filter(|sel| !sel.is_empty())
            .map(|sel| TextEdit::delete(sel.range()))
            .collect()
    };

    if edits.is_empty() {
        return Ok(false);
    }

    let _version =
        history.record_and_apply(buffer, selections, &edits, EditOrigin::Compound, now)?;
    clipboard.push(entry);
    Ok(true)
}

/// Resolves the per-cursor paste strings for `cursor_count` active cursors from `entry`.
///
/// Implements the three distribution rules:
/// 1. **1-to-1 match:** if `entry.segments.len() == cursor_count`, cursor `i` receives `segments[i]`.
/// 2. **Multi-line OS text for `cursor_count > 1`:** if `entry.segments.len() == 1` and splitting
///    `entry.text` by `\r\n` / `\n` (ignoring an optional single trailing newline when that leaves
///    exactly `cursor_count` lines) yields `cursor_count` lines, cursor `i` receives `line[i]`.
/// 3. **Broadcast:** otherwise every cursor receives the full `entry.text`.
#[must_use]
pub fn resolve_paste_texts(entry: &ClipboardEntry, cursor_count: usize) -> Vec<&str> {
    if cursor_count == 0 {
        return Vec::new();
    }

    if entry.segments.len() == cursor_count {
        return entry.segments.iter().map(String::as_str).collect();
    }

    if cursor_count > 1
        && entry.segments.len() == 1
        && let Some(lines) = split_lines_for_multicursor(&entry.text, cursor_count)
    {
        return lines;
    }

    vec![entry.text.as_str(); cursor_count]
}

/// Pastes `entry` across all selections in `selections` as a single atomic [`EditOrigin::Paste`]
/// transaction in `history`, updating `buffer`, `selections`, and `buffer`'s [`EditLog`](crate::EditLog).
///
/// # Errors
///
/// Propagates any [`HistoryError`] from [`History::record_and_apply`].
pub fn paste(
    entry: &ClipboardEntry,
    buffer: &mut Buffer,
    selections: &mut SelectionSet,
    history: &mut History,
    now: Instant,
) -> Result<BufferVersion, HistoryError> {
    let texts = resolve_paste_texts(entry, selections.len());
    let edits: Vec<TextEdit> = selections
        .as_slice()
        .iter()
        .zip(texts)
        .map(|(sel, text)| TextEdit::new(sel.range(), text))
        .collect();

    history.record_and_apply(buffer, selections, &edits, EditOrigin::Paste, now)
}

/// Pastes the latest entry from `clipboard` across `selections`, returning `Ok(None)` if empty.
///
/// # Errors
///
/// Propagates any [`HistoryError`] from [`paste`].
pub fn paste_latest(
    clipboard: &MultiCursorClipboard,
    buffer: &mut Buffer,
    selections: &mut SelectionSet,
    history: &mut History,
    now: Instant,
) -> Result<Option<BufferVersion>, HistoryError> {
    let Some(entry) = clipboard.latest() else {
        return Ok(None);
    };
    paste(entry, buffer, selections, history, now).map(Some)
}

fn extract_clipboard_entry(
    snapshot: &BufferSnapshot,
    selections: &SelectionSet,
    linewise_on_empty: bool,
) -> Option<ClipboardEntry> {
    let all_empty = selections.as_slice().iter().all(Selection::is_empty);
    let separator = snapshot.line_ending().as_str();

    if all_empty {
        if !linewise_on_empty {
            return None;
        }
        let rows = unique_cursor_rows(snapshot.rope(), selections);
        let mut segments = Vec::with_capacity(rows.len());
        for row in rows {
            let line = snapshot
                .line_slice(row)
                .map_or_else(|_| String::new(), |slice| slice.to_string());
            segments.push(line);
        }
        let text = segments.join(separator);
        return Some(ClipboardEntry {
            text,
            segments,
            linewise: true,
        });
    }

    let mut segments = Vec::with_capacity(selections.len());
    for sel in selections.as_slice() {
        if sel.is_empty() {
            segments.push(String::new());
        } else {
            let segment = snapshot
                .slice_bytes(sel.range())
                .map_or_else(|_| String::new(), |slice| slice.to_string());
            segments.push(segment);
        }
    }
    let text = segments.join(separator);
    Some(ClipboardEntry {
        text,
        segments,
        linewise: false,
    })
}

fn unique_cursor_rows(rope: &Rope, selections: &SelectionSet) -> Vec<usize> {
    let mut rows = Vec::with_capacity(selections.len());
    let len = rope.len_bytes();
    for sel in selections.as_slice() {
        let clamped = sel.head().min(len);
        let row = rope.byte_to_line(clamped);
        if rows.last().copied() != Some(row) {
            rows.push(row);
        }
    }
    rows
}

fn build_linewise_delete_edits(rope: &Rope, selections: &SelectionSet) -> Vec<TextEdit> {
    let rows = unique_cursor_rows(rope, selections);
    if rows.is_empty() {
        return Vec::new();
    }

    let total_lines = rope.len_lines();
    let mut edits = Vec::new();
    let mut run_start = rows[0];
    let mut run_end = rows[0];

    for &row in &rows[1..] {
        if row == run_end + 1 {
            run_end = row;
        } else {
            push_line_run_delete(rope, total_lines, run_start, run_end, &mut edits);
            run_start = row;
            run_end = row;
        }
    }
    push_line_run_delete(rope, total_lines, run_start, run_end, &mut edits);
    edits
}

fn push_line_run_delete(
    rope: &Rope,
    total_lines: usize,
    start_row: usize,
    end_row: usize,
    edits: &mut Vec<TextEdit>,
) {
    let end_line = rope.line(end_row);
    let end_has_eol = line_ending_len_bytes(end_line) > 0;
    let is_last_line_without_eol = end_row + 1 == total_lines && !end_has_eol;

    let delete_start = if is_last_line_without_eol && start_row > 0 {
        let prev_row = start_row - 1;
        rope.line_to_byte(prev_row) + line_content_len_bytes(rope.line(prev_row))
    } else {
        rope.line_to_byte(start_row)
    };
    let delete_end = rope.line_to_byte(end_row) + end_line.len_bytes();

    if delete_start < delete_end {
        edits.push(TextEdit::delete(delete_start..delete_end));
    }
}

fn split_lines_for_multicursor(text: &str, cursor_count: usize) -> Option<Vec<&str>> {
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();

    if lines.len() == cursor_count {
        return Some(lines);
    }

    if text.ends_with('\n') && lines.len() == cursor_count + 1 {
        lines.pop();
        return Some(lines);
    }

    None
}
