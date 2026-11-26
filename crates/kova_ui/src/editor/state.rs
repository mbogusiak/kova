//! Headless editing state behind [`crate::editor::EditorView`].
//!
//! [`EditorState`] holds a [`SharedBuffer`] handle (the [`Buffer`] plus its Undo/Redo
//! [`History`] tree, shared with sibling split panes showing the same file), the multi-cursor
//! [`SelectionSet`], the in-app multi-cursor clipboard and the IME composition range.
//! It has no GPUI dependency, so every keyboard, mouse, clipboard and IME mutation can be
//! tested without a window. Every text mutation goes through
//! [`History::record_and_apply`], so each one is undoable.

use std::ops::Range;
use std::time::Instant;

#[cfg(doc)]
use kova_buffer::History;
use kova_buffer::{
    Buffer, BufferSnapshot, BufferVersion, ClipboardEntry, EditOrigin, Motion, MotionConfig,
    MultiCursorClipboard, Selection, SelectionSet, TextEdit, copy_selections, cut_selections,
    next_grapheme_boundary, paste, prev_grapheme_boundary,
};

use crate::editor::input::split_paste_for_cursors;
use crate::editor::shared::SharedBuffer;

/// Headless editor model: buffer, history, selections, clipboard ring and IME marked range.
#[derive(Debug)]
pub struct EditorState {
    buffer: SharedBuffer,
    /// Document version this state's selections were last valid for.
    seen_version: BufferVersion,
    selections: SelectionSet,
    clipboard: MultiCursorClipboard,
    marked_range: Option<Range<usize>>,
    motion_config: MotionConfig,
    read_only: bool,
}

impl EditorState {
    /// Creates a state with a single caret at the start of `buffer`.
    #[must_use]
    pub fn new(buffer: Buffer) -> Self {
        Self::with_shared_buffer(SharedBuffer::new(buffer))
    }

    /// Creates a state over an existing [`SharedBuffer`] (the same file in another split),
    /// with its own caret at the start of the document.
    #[must_use]
    pub fn with_shared_buffer(buffer: SharedBuffer) -> Self {
        Self {
            seen_version: buffer.version(),
            buffer,
            selections: SelectionSet::caret(0),
            clipboard: MultiCursorClipboard::default(),
            marked_range: None,
            motion_config: MotionConfig::default(),
            read_only: false,
        }
    }

    /// Returns `true` when the buffer is read-only (mutations and history are blocked).
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Enables or disables read-only mode (for decompiled or external library files).
    pub const fn set_read_only(&mut self, read_only: bool) {
        self.read_only = read_only;
    }

    /// Creates a state from an in-memory UTF-8 string.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self::new(Buffer::from_text(text))
    }

    /// Returns an $O(1)$ snapshot of the buffer.
    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        self.buffer.snapshot()
    }

    /// Returns the active selections and carets.
    #[must_use]
    pub const fn selections(&self) -> &SelectionSet {
        &self.selections
    }

    /// Returns the number of active cursors.
    #[must_use]
    pub fn cursor_count(&self) -> usize {
        self.selections.len()
    }

    /// Returns the IME composition range in buffer bytes, if a composition is active.
    #[must_use]
    pub fn marked_range(&self) -> Option<Range<usize>> {
        self.marked_range.clone()
    }

    /// Returns a clone of the [`SharedBuffer`] handle backing this state.
    #[must_use]
    pub fn shared_buffer(&self) -> SharedBuffer {
        self.buffer.clone()
    }

    /// Returns `true` when the shared document history has a transaction to undo.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.buffer.can_undo()
    }

    /// Re-validates the selections after a sibling split edited the shared document.
    ///
    /// Selections are clamped to valid grapheme boundaries of the current document and the IME
    /// composition is dropped. Returns `true` when the document changed since the last sync.
    pub fn sync_external_edits(&mut self) -> bool {
        let version = self.buffer.version();
        if version == self.seen_version {
            return false;
        }
        self.seen_version = version;
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let len = snap.len_bytes();
        let clamped: Vec<Selection> = self
            .selections
            .as_slice()
            .iter()
            .map(|sel| Selection::new(sel.anchor().min(len), sel.head().min(len)).clamp(&snap))
            .collect();
        self.selections =
            SelectionSet::from_selections(clamped, self.selections.primary_index(), &snap);
        true
    }

    /// Moves the primary caret to the 1-based `(line, column)` (columns in characters),
    /// clamping out-of-range positions. Used by *Structure* and *Problems* navigation.
    pub fn set_line_col(&mut self, line: usize, col: usize) {
        self.sync_external_edits();
        let snap = self.buffer.snapshot();
        let row = line
            .saturating_sub(1)
            .min(snap.len_lines().saturating_sub(1));
        let line_start = snap.rope().line_to_byte(row);
        let line_len = snap.line_len_bytes(row).unwrap_or(0);
        let start_char = snap.byte_to_char(line_start).unwrap_or(0);
        let line_chars = snap
            .slice_bytes(line_start..line_start + line_len)
            .map_or(0, |s| s.len_chars());
        let byte = snap
            .char_to_byte(start_char + col.saturating_sub(1).min(line_chars))
            .unwrap_or(line_start);
        self.set_caret(byte);
    }

    /// Returns the 0-based `(row, byte_column)` of the primary cursor head.
    #[must_use]
    pub fn primary_point(&self) -> (usize, usize) {
        let snap = self.buffer.snapshot();
        let head = snap.clamp_byte_to_char_boundary(self.selections.primary().head());
        snap.byte_to_point(head)
            .map_or((0, 0), |pt| (pt.row, pt.column))
    }

    /// Returns the 1-based `(line, column)` of the primary cursor, counting columns in characters.
    #[must_use]
    pub fn primary_line_col(&self) -> (usize, usize) {
        let snap = self.buffer.snapshot();
        let head = snap.clamp_byte_to_char_boundary(self.selections.primary().head());
        let Ok(pt) = snap.byte_to_point(head) else {
            return (1, 1);
        };
        let line_start = snap.rope().line_to_byte(pt.row);
        let col_chars = snap
            .slice_bytes(line_start..head)
            .map_or(pt.column, |s| s.len_chars());
        (pt.row + 1, col_chars + 1)
    }

    // ----------------------------------------------------------------- editing

    /// Replaces every selection with `text` (typing, `Enter`, `Tab`). Returns `true` if the buffer changed.
    pub fn insert_text(&mut self, text: &str) -> bool {
        self.sync_external_edits();
        self.marked_range = None;
        let edits: Vec<TextEdit> = self
            .selections
            .as_slice()
            .iter()
            .map(|sel| TextEdit::new(sel.range(), text))
            .collect();
        self.record(&edits, EditOrigin::Typing)
    }

    /// Deletes each selection, or one grapheme before each empty caret (`Backspace`).
    pub fn delete_backward(&mut self) -> bool {
        self.delete_with(EditOrigin::DeleteBackward, |snap, sel| {
            if sel.is_empty() {
                prev_grapheme_boundary(snap, sel.head())..sel.head()
            } else {
                sel.range()
            }
        })
    }

    /// Deletes each selection, or one grapheme after each empty caret (`Delete`).
    pub fn delete_forward(&mut self) -> bool {
        self.delete_with(EditOrigin::DeleteForward, |snap, sel| {
            if sel.is_empty() {
                sel.head()..next_grapheme_boundary(snap, sel.head())
            } else {
                sel.range()
            }
        })
    }

    /// Reverts the latest history transaction. Returns `true` if anything was undone.
    pub fn undo(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        self.marked_range = None;
        self.sync_external_edits();
        let selections = &mut self.selections;
        let undone = self
            .buffer
            .with_document(|buffer, history| history.undo(buffer, selections));
        self.seen_version = self.buffer.version();
        matches!(undone, Ok(true))
    }

    /// Re-applies the latest undone transaction. Returns `true` if anything was redone.
    pub fn redo(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        self.marked_range = None;
        self.sync_external_edits();
        let selections = &mut self.selections;
        let redone = self
            .buffer
            .with_document(|buffer, history| history.redo(buffer, selections));
        self.seen_version = self.buffer.version();
        matches!(redone, Ok(true))
    }

    // --------------------------------------------------------------- clipboard

    /// Returns the selected text of all cursors joined by the buffer line ending.
    ///
    /// Returns `None` when every selection is empty, so the OS clipboard is never
    /// overwritten with an empty string.
    pub fn copy(&mut self) -> Option<String> {
        let snap = self.buffer.snapshot();
        copy_selections(&mut self.clipboard, &snap, &self.selections, false).map(|e| e.text)
    }

    /// Copies like [`Self::copy`] and deletes the selected text as one undo transaction.
    pub fn cut(&mut self) -> Option<String> {
        if self.read_only {
            return None;
        }
        self.marked_range = None;
        self.sync_external_edits();
        let (clipboard, selections) = (&mut self.clipboard, &mut self.selections);
        let cut = self.buffer.with_document(|buffer, history| {
            cut_selections(
                clipboard,
                buffer,
                selections,
                history,
                false,
                Instant::now(),
            )
        });
        self.seen_version = self.buffer.version();
        match cut {
            Ok(true) => self.clipboard.latest().map(|e| e.text.clone()),
            _ => None,
        }
    }

    /// Pastes `text` (read from the OS clipboard) as one undo transaction.
    ///
    /// If `text` matches the latest in-app copy, its per-cursor segments are reused.
    /// Otherwise [`split_paste_for_cursors`] decides: with `K > 1` cursors and exactly
    /// `K` lines, line `i` goes to cursor `i`; else the whole text goes to every cursor.
    pub fn paste(&mut self, text: &str) -> bool {
        if self.read_only {
            return false;
        }
        self.marked_range = None;
        let entry = match self.clipboard.latest() {
            Some(latest) if latest.text == text => latest.clone(),
            _ => {
                let parts = split_paste_for_cursors(text, self.selections.len());
                ClipboardEntry {
                    text: text.to_owned(),
                    segments: parts.into_iter().map(str::to_owned).collect(),
                    linewise: false,
                }
            }
        };
        self.sync_external_edits();
        let before = self.buffer.version();
        let selections = &mut self.selections;
        let pasted = self.buffer.with_document(|buffer, history| {
            paste(&entry, buffer, selections, history, Instant::now())
        });
        self.seen_version = self.buffer.version();
        pasted.is_ok_and(|v| v != before)
    }

    // -------------------------------------------------------------- navigation

    /// Moves (or extends with `extend`) every selection by `motion`.
    pub fn move_selections(&mut self, motion: Motion, extend: bool) {
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        self.selections
            .move_by(&snap, motion, extend, self.motion_config);
    }

    /// Moves every selection `lines` rows up (`PageUp`) or down (`PageDown`).
    pub fn move_vertical_page(&mut self, lines: usize, down: bool, extend: bool) {
        let motion = if down { Motion::Down } else { Motion::Up };
        for _ in 0..lines.max(1) {
            self.move_selections(motion, extend);
        }
    }

    /// Selects the whole buffer.
    pub fn select_all(&mut self) {
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        self.selections.select_all(&snap);
    }

    /// Collapses multiple cursors to the primary one, or clears a single selection.
    pub fn escape(&mut self) {
        self.marked_range = None;
        if self.selections.len() > 1 {
            self.selections.collapse_to_primary();
        } else {
            let head = self.selections.primary().head();
            self.selections = SelectionSet::caret(head);
        }
    }

    /// Adds a cursor on the line above (`up == true`) or below the existing cursors.
    pub fn add_cursor_vertical(&mut self, up: bool) {
        let snap = self.buffer.snapshot();
        if up {
            self.selections.add_cursor_above(&snap, self.motion_config);
        } else {
            self.selections.add_cursor_below(&snap, self.motion_config);
        }
    }

    // ------------------------------------------- JetBrains actions and Vim bridge (KOVA-026)

    /// Returns the byte offset of the primary cursor head.
    #[must_use]
    pub fn primary_head(&self) -> usize {
        self.selections.primary().head()
    }

    /// Replaces all selections with one selection from `anchor` to `head` (clamped), as used
    /// to mirror the Vim engine's caret or visual selection.
    pub fn set_selection(&mut self, anchor: usize, head: usize) {
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let len = snap.len_bytes();
        let anchor = snap.clamp_byte_to_char_boundary(anchor.min(len));
        let head = snap.clamp_byte_to_char_boundary(head.min(len));
        self.selections = SelectionSet::single(Selection::new(anchor, head));
    }

    /// Applies `edits` (ranges in pre-edit coordinates) as one undoable transaction.
    /// Returns `true` if the buffer changed.
    pub fn apply_edits(&mut self, edits: &[TextEdit], origin: EditOrigin) -> bool {
        self.marked_range = None;
        self.record(edits, origin)
    }

    /// `Cmd+D` / `Ctrl+D`: duplicates every non-empty selection right after itself, and the
    /// whole line of every empty caret (the caret moves onto the copy below).
    pub fn duplicate_lines_or_selections(&mut self) -> bool {
        self.sync_external_edits();
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let mut edits: Vec<TextEdit> = Vec::with_capacity(self.selections.len());
        let mut rows: Vec<usize> = Vec::new();
        for sel in self.selections.as_slice() {
            if sel.is_empty() {
                let Ok(pt) = snap.byte_to_point(snap.clamp_byte_to_char_boundary(sel.head()))
                else {
                    continue;
                };
                if rows.contains(&pt.row) {
                    continue;
                }
                rows.push(pt.row);
                let start = line_start_byte(&snap, pt.row);
                let end = line_end_with_ending_byte(&snap, pt.row);
                let mut text = slice_text(&snap, start..end);
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                edits.push(TextEdit::new(start..start, text.as_str()));
            } else {
                let range = sel.range();
                let text = slice_text(&snap, range.clone());
                edits.push(TextEdit::new(range.end..range.end, text.as_str()));
            }
        }
        if edits.is_empty() {
            return false;
        }
        self.record(&edits, EditOrigin::Compound)
    }

    /// `Cmd+/` / `Ctrl+/`: toggles the line comment `prefix` (`//`, `#`) on every line touched
    /// by a selection. When every non-blank line is already commented the prefix (and one
    /// following space) is removed; otherwise `prefix` plus a space is added at the line start.
    pub fn toggle_line_comment(&mut self, prefix: &str) -> bool {
        if prefix.is_empty() {
            return false;
        }
        self.sync_external_edits();
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let mut rows: Vec<usize> = Vec::new();
        for sel in self.selections.as_slice() {
            let range = sel.range();
            let (Ok(first), Ok(last)) = (
                snap.byte_to_point(snap.clamp_byte_to_char_boundary(range.start)),
                snap.byte_to_point(snap.clamp_byte_to_char_boundary(range.end)),
            ) else {
                continue;
            };
            // A selection ending at column 0 does not include that line.
            let last_row = if !sel.is_empty() && last.column == 0 && last.row > first.row {
                last.row - 1
            } else {
                last.row
            };
            rows.extend(first.row..=last_row);
        }
        rows.sort_unstable();
        rows.dedup();

        let lines: Vec<(usize, String)> = rows
            .into_iter()
            .map(|row| (row, slice_text(&snap, line_content_range(&snap, row))))
            .filter(|(_, text)| !text.trim().is_empty())
            .collect();
        if lines.is_empty() {
            return false;
        }
        let all_commented = lines
            .iter()
            .all(|(_, text)| text.trim_start().starts_with(prefix));
        let mut insert = String::with_capacity(prefix.len() + 1);
        insert.push_str(prefix);
        insert.push(' ');
        let edits: Vec<TextEdit> = lines
            .iter()
            .map(|(row, text)| {
                let start = line_start_byte(&snap, *row);
                if all_commented {
                    let indent = text.len() - text.trim_start().len();
                    let after = &text[indent + prefix.len()..];
                    let extra = usize::from(after.starts_with(' '));
                    let from = start + indent;
                    TextEdit::delete(from..from + prefix.len() + extra)
                } else {
                    TextEdit::new(start..start, insert.as_str())
                }
            })
            .collect();
        self.record(&edits, EditOrigin::Compound)
    }

    /// `Ctrl+G` (macOS) / `Alt+J` (Linux): selects the word under the caret, or adds a selection
    /// for the next occurrence of the primary selection. Returns `true` if a selection changed.
    pub fn add_next_occurrence(&mut self) -> bool {
        self.sync_external_edits();
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        self.selections.add_next_occurrence(&snap)
    }

    // ------------------------------------------------------------------- mouse

    /// Replaces all selections with a single caret at `byte` (plain click).
    pub fn set_caret(&mut self, byte: usize) {
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        self.selections = SelectionSet::caret(snap.clamp_byte_to_char_boundary(byte));
    }

    /// Keeps the primary anchor and moves the head to `byte` (`Shift+Click`, drag).
    pub fn extend_primary_to(&mut self, byte: usize) {
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let anchor = self.selections.primary().anchor();
        let head = snap.clamp_byte_to_char_boundary(byte);
        self.selections = SelectionSet::single(Selection::new(anchor, head));
    }

    /// Adds a caret at `byte` and makes it primary (`Alt+Click`).
    pub fn add_cursor_at(&mut self, byte: usize) {
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let caret = Selection::caret(snap.clamp_byte_to_char_boundary(byte));
        self.selections.push(caret, true, &snap);
    }

    /// Selects the word under `byte` (double click).
    pub fn select_word_at(&mut self, byte: usize) {
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let range = word_range_at(&snap, byte);
        self.selections = SelectionSet::single(Selection::new(range.start, range.end));
    }

    /// Selects whole lines from `anchor_row` to `head_row`, including the line endings
    /// (triple click, gutter click and gutter drag).
    pub fn select_lines(&mut self, anchor_row: usize, head_row: usize) {
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let (anchor, head) = if head_row >= anchor_row {
            (
                line_start_byte(&snap, anchor_row),
                line_end_with_ending_byte(&snap, head_row),
            )
        } else {
            (
                line_end_with_ending_byte(&snap, anchor_row),
                line_start_byte(&snap, head_row),
            )
        };
        self.selections = SelectionSet::single(Selection::new(anchor, head));
    }

    // --------------------------------------------------------------------- IME

    /// Converts a document UTF-16 offset to a byte offset in $O(\log N)$, clamped to the buffer.
    #[must_use]
    pub fn utf16_to_byte(&self, utf16: usize) -> usize {
        let snap = self.buffer.snapshot();
        snap.utf16_to_byte(utf16.min(snap.len_utf16()))
            .unwrap_or_else(|_| snap.len_bytes())
    }

    /// Converts a byte offset to a document UTF-16 offset in $O(\log N)$, clamped to the buffer.
    #[must_use]
    pub fn byte_to_utf16(&self, byte: usize) -> usize {
        let snap = self.buffer.snapshot();
        let clamped = snap.clamp_byte_to_char_boundary(byte.min(snap.len_bytes()));
        snap.byte_to_utf16(clamped).unwrap_or(0)
    }

    /// Converts a document UTF-16 range to an ordered byte range.
    #[must_use]
    pub fn utf16_range_to_bytes(&self, range: &Range<usize>) -> Range<usize> {
        let a = self.utf16_to_byte(range.start);
        let b = self.utf16_to_byte(range.end);
        a.min(b)..a.max(b)
    }

    /// Returns the document text in `range_utf16` and the clamped UTF-16 range actually used.
    #[must_use]
    pub fn text_for_utf16_range(&self, range_utf16: &Range<usize>) -> (String, Range<usize>) {
        let snap = self.buffer.snapshot();
        let max = snap.len_utf16();
        let start = range_utf16.start.min(max);
        let end = range_utf16.end.max(start).min(max);
        let bytes = self.utf16_range_to_bytes(&(start..end));
        let text = snap
            .slice_bytes(bytes)
            .map_or_else(|_| String::new(), |s| s.to_string());
        (text, start..end)
    }

    /// Returns the primary selection as a UTF-16 range and whether it is reversed.
    #[must_use]
    pub fn primary_selection_utf16(&self) -> (Range<usize>, bool) {
        let primary = *self.selections.primary();
        let start = self.byte_to_utf16(primary.start());
        let end = self.byte_to_utf16(primary.end());
        (start..end, primary.is_reversed())
    }

    /// Returns the marked (composition) range in UTF-16 units.
    #[must_use]
    pub fn marked_range_utf16(&self) -> Option<Range<usize>> {
        let marked = self.marked_range.as_ref()?;
        Some(self.byte_to_utf16(marked.start)..self.byte_to_utf16(marked.end))
    }

    /// Ends the IME composition, keeping the composed text in the buffer.
    pub fn unmark(&mut self) -> bool {
        self.marked_range.take().is_some()
    }

    /// Commits IME / typed text (`EntityInputHandler::replace_text_in_range`).
    ///
    /// The target is `range_utf16` if given, else the marked range, else the selections.
    /// With several cursors and neither range, `text` is inserted at every cursor.
    pub fn replace_text_in_range(
        &mut self,
        range_utf16: Option<&Range<usize>>,
        text: &str,
    ) -> bool {
        if self.read_only {
            self.marked_range = None;
            return false;
        }
        let target = match (range_utf16, self.marked_range.take()) {
            (Some(r), _) => self.utf16_range_to_bytes(r),
            (None, Some(marked)) => marked,
            (None, None) => return self.insert_text(text),
        };
        let edits = [TextEdit::new(target.clone(), text)];
        let changed = self.record(&edits, EditOrigin::Typing);
        let snap = self.buffer.snapshot();
        let caret = snap.clamp_byte_to_char_boundary(target.start + text.len());
        self.selections = SelectionSet::caret(caret);
        changed
    }

    /// Inserts composition text and marks it (`EntityInputHandler::replace_and_mark_text_in_range`).
    ///
    /// `selected_utf16` is relative to the start of `text`; without it the caret goes to the end.
    pub fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<&Range<usize>>,
        text: &str,
        selected_utf16: Option<&Range<usize>>,
    ) -> bool {
        if self.read_only {
            self.marked_range = None;
            return false;
        }
        let target = match (range_utf16, self.marked_range.clone()) {
            (Some(r), _) => self.utf16_range_to_bytes(r),
            (None, Some(marked)) => marked,
            (None, None) => self.selections.primary().range(),
        };
        let edits = [TextEdit::new(target.clone(), text)];
        let changed = self.record(&edits, EditOrigin::Typing);

        let start = target.start;
        let end = start + text.len();
        self.marked_range = (!text.is_empty()).then_some(start..end);

        let (sel_start, sel_end) = selected_utf16.map_or((end, end), |rel| {
            let base = self.byte_to_utf16(start);
            let a = self.utf16_to_byte(base + rel.start).clamp(start, end);
            let b = self.utf16_to_byte(base + rel.end).clamp(start, end);
            (a, b)
        });
        self.selections = SelectionSet::single(Selection::new(sel_start, sel_end));
        changed
    }

    // ---------------------------------------------------------------- internal

    fn record(&mut self, edits: &[TextEdit], origin: EditOrigin) -> bool {
        if self.read_only {
            return false;
        }
        self.sync_external_edits();
        let before = self.buffer.version();
        let selections = &mut self.selections;
        let applied = self.buffer.with_document(|buffer, history| {
            history.record_and_apply(buffer, selections, edits, origin, Instant::now())
        });
        self.seen_version = self.buffer.version();
        applied.is_ok_and(|v| v != before)
    }

    fn delete_with<F>(&mut self, origin: EditOrigin, range_for: F) -> bool
    where
        F: Fn(&BufferSnapshot, &Selection) -> Range<usize>,
    {
        self.sync_external_edits();
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let mut ranges: Vec<Range<usize>> = self
            .selections
            .as_slice()
            .iter()
            .map(|sel| range_for(&snap, sel))
            .filter(|r| !r.is_empty())
            .collect();
        ranges.sort_by_key(|r| (r.start, r.end));
        let edits: Vec<TextEdit> = merge_ranges(ranges)
            .into_iter()
            .map(TextEdit::delete)
            .collect();
        if edits.is_empty() {
            return false;
        }
        self.record(&edits, origin)
    }
}

/// Merges sorted, possibly overlapping byte ranges into disjoint ranges.
fn merge_ranges(sorted: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(sorted.len());
    for range in sorted {
        if let Some(prev) = merged.last_mut()
            && range.start <= prev.end
        {
            prev.end = prev.end.max(range.end);
            continue;
        }
        merged.push(range);
    }
    merged
}

fn line_start_byte(snap: &BufferSnapshot, row: usize) -> usize {
    let row = row.min(snap.len_lines().saturating_sub(1));
    snap.rope().line_to_byte(row)
}

fn line_end_with_ending_byte(snap: &BufferSnapshot, row: usize) -> usize {
    let start = line_start_byte(snap, row);
    snap.line_slice_with_ending(row)
        .map_or(start, |s| start + s.len_bytes())
}

/// Byte range of `row` without its line ending.
fn line_content_range(snap: &BufferSnapshot, row: usize) -> Range<usize> {
    let start = line_start_byte(snap, row);
    start..start + snap.line_len_bytes(row).unwrap_or(0)
}

/// Copies `range` of the document into a `String` (empty when out of bounds).
fn slice_text(snap: &BufferSnapshot, range: Range<usize>) -> String {
    snap.slice_bytes(range)
        .map(|s| s.chunks().collect())
        .unwrap_or_default()
}

/// Character class used for double-click word selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharClass {
    Word,
    Whitespace,
    Punctuation,
}

fn classify(ch: char) -> CharClass {
    if ch.is_alphanumeric() || ch == '_' || ch == '$' {
        CharClass::Word
    } else if ch.is_whitespace() {
        CharClass::Whitespace
    } else {
        CharClass::Punctuation
    }
}

/// Returns the byte range of the run of same-class characters around `byte`, within its line.
///
/// A click right after the last character of a word (at a word end) selects that word.
fn word_range_at(snap: &BufferSnapshot, byte: usize) -> Range<usize> {
    let byte = snap.clamp_byte_to_char_boundary(byte.min(snap.len_bytes()));
    let Ok(pt) = snap.byte_to_point(byte) else {
        return byte..byte;
    };
    let line_start = snap.rope().line_to_byte(pt.row);
    let Ok(line) = snap.line_slice(pt.row) else {
        return byte..byte;
    };
    let line: String = line.chunks().collect();
    let rel = pt.column.min(line.len());

    let chars: Vec<(usize, char)> = line.char_indices().collect();
    if chars.is_empty() {
        return byte..byte;
    }
    // Index of the char at `rel`, or the last char when clicking past the line end.
    let mut idx = chars
        .iter()
        .position(|&(i, _)| i >= rel)
        .unwrap_or(chars.len() - 1);
    // Prefer the word on the left when the click lands on a boundary after a word.
    if idx > 0
        && classify(chars[idx].1) != CharClass::Word
        && classify(chars[idx - 1].1) == CharClass::Word
        && chars[idx].0 == rel
    {
        idx -= 1;
    }
    let class = classify(chars[idx].1);
    let mut lo = idx;
    while lo > 0 && classify(chars[lo - 1].1) == class {
        lo -= 1;
    }
    let mut hi = idx + 1;
    while hi < chars.len() && classify(chars[hi].1) == class {
        hi += 1;
    }
    let start = chars[lo].0;
    let end = chars.get(hi).map_or(line.len(), |&(i, _)| i);
    (line_start + start)..(line_start + end)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::time::Duration;

    use kova_core::perf::budgets::INPUT_LATENCY_MAX;

    use super::*;
    use crate::editor::input::InputLatencyStats;

    fn text(state: &EditorState) -> String {
        state.snapshot().text()
    }

    #[test]
    fn split_states_share_one_document_and_undo_history() {
        let mut left = EditorState::from_text("class A {}\nint x;\n");
        let mut right = EditorState::with_shared_buffer(left.shared_buffer());

        // Right caret sits at the very end; left then deletes the second line.
        right.select_all();
        right.escape();
        right.set_caret(usize::MAX);
        assert_eq!(right.primary_line_col(), (3, 1));

        left.select_lines(1, 1);
        assert!(left.delete_backward());
        assert_eq!(text(&right), "class A {}\n");

        // The stale right selection is clamped before the next edit lands.
        assert!(right.sync_external_edits());
        assert!(!right.sync_external_edits());
        assert!(right.insert_text("// end"));
        assert_eq!(text(&left), "class A {}\n// end");

        // Undo is per document: undoing in the left pane reverts the right pane's edit.
        assert!(right.can_undo());
        assert!(left.undo());
        assert_eq!(text(&right), "class A {}\n");

        left.set_line_col(1, 7);
        assert_eq!(left.primary_line_col(), (1, 7));
        left.set_line_col(99, 99);
        assert_eq!(left.primary_line_col(), (2, 1));
    }

    #[test]
    fn typing_and_backspace_are_undoable() {
        let mut st = EditorState::from_text("");
        assert!(st.insert_text("zażółć"));
        assert!(st.insert_text("\n"));
        assert!(st.delete_backward());
        assert_eq!(text(&st), "zażółć");
        assert!(st.can_undo());
        while st.undo() {}
        assert_eq!(text(&st), "");
        assert!(st.redo());
        assert_ne!(text(&st), "");
    }

    #[test]
    fn multi_cursor_insert_and_delete() {
        let mut st = EditorState::from_text("ab\ncd\nef\n");
        st.set_caret(0);
        st.add_cursor_vertical(false);
        st.add_cursor_vertical(false);
        assert_eq!(st.cursor_count(), 3);
        assert!(st.insert_text("> "));
        assert_eq!(text(&st), "> ab\n> cd\n> ef\n");
        assert!(st.delete_backward());
        assert_eq!(text(&st), ">ab\n>cd\n>ef\n");
        assert!(st.undo());
        assert_eq!(text(&st), "> ab\n> cd\n> ef\n");
    }

    #[test]
    fn delete_forward_removes_whole_grapheme() {
        let mut st = EditorState::from_text("🦀x");
        st.set_caret(0);
        assert!(st.delete_forward());
        assert_eq!(text(&st), "x");
    }

    #[test]
    fn copy_cut_paste_roundtrip_with_multi_cursor_distribution() {
        let mut st = EditorState::from_text("one\ntwo\nthree\n");
        assert_eq!(
            st.copy(),
            None,
            "empty selection never overwrites clipboard"
        );

        st.select_lines(0, 0);
        let copied = st.copy();
        assert_eq!(copied.as_deref(), Some("one\n"));

        st.select_word_at(5);
        let cut = st.cut();
        assert_eq!(cut.as_deref(), Some("two"));
        assert_eq!(text(&st), "one\n\nthree\n");
        assert!(st.undo());
        assert_eq!(text(&st), "one\ntwo\nthree\n");

        // Three cursors + three external clipboard lines -> one line per cursor.
        let mut st = EditorState::from_text("a\nb\nc\n");
        st.set_caret(1);
        st.add_cursor_vertical(false);
        st.add_cursor_vertical(false);
        assert!(st.paste("1\n2\n3"));
        assert_eq!(text(&st), "a1\nb2\nc3\n");
        assert!(st.undo(), "paste is a single undo transaction");
        assert_eq!(text(&st), "a\nb\nc\n");

        // Mismatched line count -> whole text on every cursor.
        assert!(st.paste("xy"));
        assert_eq!(text(&st), "axy\nbxy\ncxy\n");
    }

    #[test]
    fn word_and_line_selection() {
        let mut st = EditorState::from_text("let zażółć = 1;\nnext");
        // Click on the first byte of the word.
        st.select_word_at(4);
        let sel = *st.selections().primary();
        assert_eq!(&text(&st)[sel.range()], "zażółć");
        // Click right after the word (boundary) still selects it.
        st.select_word_at(4 + "zażółć".len());
        let sel = *st.selections().primary();
        assert_eq!(&text(&st)[sel.range()], "zażółć");
        // Click inside "let".
        st.select_word_at(1);
        let sel = *st.selections().primary();
        assert_eq!(&text(&st)[sel.range()], "let");

        st.select_lines(0, 1);
        let sel = *st.selections().primary();
        assert_eq!(sel.range(), 0..text(&st).len());
        st.select_lines(1, 0);
        assert!(st.selections().primary().is_reversed());
    }

    #[test]
    fn click_shift_click_alt_click_and_escape() {
        let mut st = EditorState::from_text("hello world\nsecond line\n");
        st.set_caret(2);
        st.extend_primary_to(8);
        assert_eq!(st.selections().primary().range(), 2..8);
        st.add_cursor_at(14);
        assert_eq!(st.cursor_count(), 2);
        st.escape();
        assert_eq!(st.cursor_count(), 1);
        st.escape();
        assert!(st.selections().primary().is_empty());
    }

    #[test]
    fn ime_composition_polish_and_emoji() {
        let mut st = EditorState::from_text("🦀 ");
        // Caret after "🦀 " = 3 UTF-16 units.
        st.set_caret("🦀 ".len());
        assert_eq!(st.primary_selection_utf16(), (3..3, false));

        // Dead key: macOS shows "˛" marked, then commits "ą".
        assert!(st.replace_and_mark_text_in_range(None, "˛", None));
        assert_eq!(st.marked_range_utf16(), Some(3..4));
        assert!(st.replace_text_in_range(None, "ą"));
        assert_eq!(st.marked_range(), None);
        assert_eq!(text(&st), "🦀 ą");
        assert_eq!(st.primary_selection_utf16(), (4..4, false));

        // Explicit UTF-16 range replacement over the surrogate pair.
        assert!(st.replace_text_in_range(Some(&(0..2)), "ż"));
        assert_eq!(text(&st), "ż ą");
        let (slice, used) = st.text_for_utf16_range(&(0..99));
        assert_eq!(slice, "ż ą");
        assert_eq!(used, 0..3);

        // Composition with an inner selection (CJK-style).
        st.set_caret(text(&st).len());
        assert!(st.replace_and_mark_text_in_range(None, "にほん", Some(&(1..2))));
        let sel = *st.selections().primary();
        assert_eq!(&text(&st)[sel.range()], "ほ");
        assert!(st.unmark());
        assert_eq!(st.marked_range(), None);
        assert_eq!(text(&st), "ż ąにほん");
    }

    #[test]
    fn ime_commit_without_range_inserts_at_all_cursors() {
        let mut st = EditorState::from_text("a\nb\n");
        st.set_caret(1);
        st.add_cursor_vertical(false);
        assert!(st.replace_text_in_range(None, "ę"));
        assert_eq!(text(&st), "aę\nbę\n");
    }

    #[test]
    fn page_navigation_and_select_all() {
        let mut st = EditorState::from_text("0\n1\n2\n3\n4\n5\n");
        st.move_vertical_page(3, true, false);
        assert_eq!(st.primary_point().0, 3);
        st.move_vertical_page(2, false, true);
        assert_eq!(st.primary_point().0, 1);
        assert!(!st.selections().primary().is_empty());
        st.select_all();
        assert_eq!(
            st.selections().primary().range(),
            0..st.snapshot().len_bytes()
        );
    }

    #[test]
    fn primary_line_col_counts_characters() {
        let mut st = EditorState::from_text("x\nżółw");
        st.set_caret("x\nżół".len());
        assert_eq!(st.primary_line_col(), (2, 4));
    }

    /// AC-5 / V-3: dispatch of every input path on a 100k-line buffer stays far below 5 ms.
    #[test]
    fn input_dispatch_on_100k_lines_stays_within_latency_budget() {
        let mut big = String::with_capacity(32 * 100_000);
        for i in 0..100_000usize {
            let _ = writeln!(big, "    line {i:06} zażółć gęślą 🦀");
        }
        let mut st = EditorState::from_text(&big);
        drop(big);
        let mut stats = InputLatencyStats::default();
        let mid = st.snapshot().rope().line_to_byte(50_000);
        st.set_caret(mid);

        let mut measure = |st: &mut EditorState, f: &dyn Fn(&mut EditorState)| {
            let t0 = Instant::now();
            f(st);
            stats.record(t0.elapsed());
        };
        for _ in 0..50 {
            measure(&mut st, &|s| {
                let _ = s.insert_text("ą");
            });
            measure(&mut st, &|s| s.move_selections(Motion::Down, false));
            measure(&mut st, &|s| s.move_selections(Motion::WordRight, true));
            measure(&mut st, &|s| {
                let _ = s.delete_backward();
            });
            measure(&mut st, &|s| {
                let _ = s.replace_and_mark_text_in_range(None, "˛", None);
                let _ = s.replace_text_in_range(None, "ę");
            });
            measure(&mut st, &|s| {
                let _ = s.primary_selection_utf16();
            });
        }
        measure(&mut st, &|s| {
            let _ = s.undo();
        });

        assert!(
            stats.max_observed < INPUT_LATENCY_MAX,
            "max dispatch {:?} exceeded {:?}",
            stats.max_observed,
            INPUT_LATENCY_MAX
        );
        assert!(stats.max_observed < Duration::from_millis(5));
    }

    #[test]
    fn duplicate_line_and_selection() {
        let mut st = EditorState::from_text("alpha\nbeta");
        st.set_line_col(2, 3);
        assert!(st.duplicate_lines_or_selections());
        assert_eq!(st.snapshot().text(), "alpha\nbeta\nbeta");
        assert_eq!(
            st.primary_line_col(),
            (3, 3),
            "caret moves onto the copy below"
        );

        st.set_selection(0, 2);
        assert!(st.duplicate_lines_or_selections());
        assert_eq!(st.snapshot().text(), "alalpha\nbeta\nbeta");
        assert!(st.undo());
        assert_eq!(st.snapshot().text(), "alpha\nbeta\nbeta");
    }

    #[test]
    fn toggle_line_comment_adds_and_removes_prefix() {
        let mut st = EditorState::from_text("    int a;\n\nint b;\nint c;");
        // Lines 1..=3 (selection ends at column 0 of line 4).
        let end = st.snapshot().text().find("int c").unwrap_or(0);
        st.set_selection(0, end);
        assert!(st.toggle_line_comment("//"));
        assert_eq!(st.snapshot().text(), "//     int a;\n\n// int b;\nint c;");
        let end = st.snapshot().text().find("int c").unwrap_or(0);
        st.set_selection(0, end);
        assert!(st.toggle_line_comment("//"));
        assert_eq!(st.snapshot().text(), "    int a;\n\nint b;\nint c;");

        let mut toml = EditorState::from_text("key = 1");
        assert!(toml.toggle_line_comment("#"));
        assert_eq!(toml.snapshot().text(), "# key = 1");
        assert!(!toml.toggle_line_comment(""));
    }

    #[test]
    fn add_next_occurrence_selects_word_then_next_match() {
        let mut st = EditorState::from_text("foo bar foo");
        assert!(st.add_next_occurrence());
        assert_eq!(st.cursor_count(), 1);
        assert!(st.add_next_occurrence());
        assert_eq!(st.cursor_count(), 2);
    }

    #[test]
    fn read_only_blocks_all_mutations_while_allowing_copy_and_navigation() {
        let mut st = EditorState::from_text("class Decompiled {}");
        st.set_read_only(true);
        assert!(st.is_read_only());

        assert!(!st.insert_text("x"));
        assert!(!st.delete_backward());
        assert!(!st.delete_forward());
        assert!(!st.paste("y"));
        assert_eq!(st.cut(), None);
        assert!(!st.apply_edits(&[TextEdit::new(0..0, "z")], EditOrigin::Typing));
        assert!(!st.duplicate_lines_or_selections());
        assert!(!st.toggle_line_comment("//"));
        assert!(!st.replace_text_in_range(None, "abc"));
        assert!(!st.replace_and_mark_text_in_range(None, "abc", None));
        assert!(!st.undo());
        assert!(!st.redo());
        assert_eq!(text(&st), "class Decompiled {}");

        st.select_word_at(6);
        assert_eq!(st.copy().as_deref(), Some("Decompiled"));
        assert_eq!(text(&st), "class Decompiled {}");
    }
}
