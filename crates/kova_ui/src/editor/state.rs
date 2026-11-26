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
use kova_core::navigation::SourceLanguageKind;

use crate::editor::input::split_paste_for_cursors;
use crate::editor::shared::SharedBuffer;
use crate::editor::syntax::EditorSyntax;

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
    selection_stack: Vec<(BufferVersion, Selection)>,
    occurrence_stack: Vec<(BufferVersion, SelectionSet)>,
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
            selection_stack: Vec::new(),
            occurrence_stack: Vec::new(),
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
        self.clear_selection_stacks();
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
        self.clear_selection_stacks();
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
        self.clear_selection_stacks();
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
        self.clear_selection_stacks();
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
        self.clear_selection_stacks();
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
        self.clear_selection_stacks();
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
        self.clear_selection_stacks();
        let snap = self.buffer.snapshot();
        self.selections.select_all(&snap);
    }

    /// Collapses multiple cursors to the primary one, or clears a single selection.
    pub fn escape(&mut self) {
        self.marked_range = None;
        self.clear_selection_stacks();
        if self.selections.len() > 1 {
            self.selections.collapse_to_primary();
        } else {
            let head = self.selections.primary().head();
            self.selections = SelectionSet::caret(head);
        }
    }

    /// Adds a cursor on the line above (`up == true`) or below the existing cursors.
    pub fn add_cursor_vertical(&mut self, up: bool) {
        self.clear_selection_stacks();
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
        self.clear_selection_stacks();
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
        self.selection_stack.clear();
        let snap = self.buffer.snapshot();
        let before = self.selections.clone();
        if self.selections.add_next_occurrence(&snap) {
            self.occurrence_stack.push((snap.version(), before));
            true
        } else {
            false
        }
    }

    /// `Ctrl+Shift+G` (macOS) / `Alt+Shift+J` (Linux): unselects the most recently added
    /// occurrence cursor (or removes the primary cursor if multiple cursors exist).
    pub fn unselect_occurrence(&mut self) -> bool {
        self.sync_external_edits();
        self.marked_range = None;
        self.selection_stack.clear();
        let version = self.buffer.snapshot().version();
        while let Some((ver, prev)) = self.occurrence_stack.pop() {
            if ver == version {
                self.selections = prev;
                return true;
            }
        }
        self.selections.remove_primary()
    }

    /// `Ctrl+Cmd+G` (macOS) / `Ctrl+Alt+Shift+J` (Linux): selects all non-overlapping
    /// occurrences of the active selection or word under caret.
    pub fn select_all_occurrences(&mut self) -> bool {
        self.sync_external_edits();
        self.marked_range = None;
        self.selection_stack.clear();
        let snap = self.buffer.snapshot();
        let before = self.selections.clone();
        if self.selections.select_all_occurrences(&snap) {
            self.occurrence_stack.push((snap.version(), before));
            true
        } else {
            false
        }
    }

    /// `Cmd+Backspace` (macOS) / `Ctrl+Y` (Linux): deletes the full line(s) touched by every
    /// cursor/selection in a single undo transaction.
    pub fn delete_lines(&mut self) -> bool {
        if self.read_only {
            return false;
        }
        self.sync_external_edits();
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        if snap.len_bytes() == 0 {
            return false;
        }
        let mut intervals: Vec<(usize, usize)> = Vec::with_capacity(self.selections.len());
        for sel in self.selections.as_slice() {
            let range = sel.range();
            let (Ok(first), Ok(last)) = (
                snap.byte_to_point(snap.clamp_byte_to_char_boundary(range.start)),
                snap.byte_to_point(snap.clamp_byte_to_char_boundary(range.end)),
            ) else {
                continue;
            };
            let last_row = if !sel.is_empty() && last.column == 0 && last.row > first.row {
                last.row - 1
            } else {
                last.row
            };
            intervals.push((first.row, last_row));
        }
        if intervals.is_empty() {
            return false;
        }
        intervals.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(intervals.len());
        for (start_row, end_row) in intervals {
            if let Some(prev) = merged.last_mut()
                && start_row <= prev.1 + 1
            {
                prev.1 = prev.1.max(end_row);
            } else {
                merged.push((start_row, end_row));
            }
        }

        let len_lines = snap.len_lines();
        let mut edits = Vec::with_capacity(merged.len());
        for (r_start, r_end) in merged {
            let mut del_start = line_start_byte(&snap, r_start);
            let del_end = line_end_with_ending_byte(&snap, r_end);
            let has_trailing_nl = del_end > line_content_range(&snap, r_end).end;
            if r_end + 1 >= len_lines && !has_trailing_nl && r_start > 0 {
                // Deleting the final line without a trailing newline also removes the preceding EOL.
                del_start = line_content_range(&snap, r_start - 1).end;
            }
            if del_start < del_end {
                edits.push(TextEdit::delete(del_start..del_end));
            }
        }
        if edits.is_empty() {
            return false;
        }
        self.record(&edits, EditOrigin::DeleteBackward)
    }

    /// `Cmd+Alt+/` (macOS) / `Ctrl+Shift+/` (Linux): toggles block comments (`open` / `close`)
    /// around each non-empty selection or the enclosing block/caret on empty selections.
    pub fn toggle_block_comment(&mut self, open: &str, close: &str) -> bool {
        if self.read_only || open.is_empty() || close.is_empty() {
            return false;
        }
        self.sync_external_edits();
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let total = snap.len_bytes();
        let mut edits: Vec<TextEdit> = Vec::new();
        let mut planned_sels: Vec<(usize, usize, bool)> = Vec::with_capacity(self.selections.len());
        let mut delta: isize = 0;

        for sel in self.selections.as_slice() {
            let reversed = sel.is_reversed();
            if sel.is_empty() {
                let caret = snap.clamp_byte_to_char_boundary(sel.head());
                let Ok(pt) = snap.byte_to_point(caret) else {
                    continue;
                };
                let line_range = line_content_range(&snap, pt.row);
                let line_text = slice_text(&snap, line_range.clone());
                let rel = caret.saturating_sub(line_range.start).min(line_text.len());
                if let Some((open_rel, close_rel)) =
                    enclosing_block_comment_in_line(&line_text, rel, open, close)
                {
                    let open_abs = line_range.start + open_rel;
                    let close_abs = line_range.start + close_rel;
                    edits.push(TextEdit::delete(open_abs..open_abs + open.len()));
                    edits.push(TextEdit::delete(close_abs..close_abs + close.len()));
                    let new_pos = shift_isize(caret, delta).saturating_sub(open.len());
                    planned_sels.push((new_pos, new_pos, false));
                    delta -= isize::try_from(open.len() + close.len()).unwrap_or(0);
                } else {
                    let snippet = format!("{open}  {close}");
                    edits.push(TextEdit::new(caret..caret, snippet.as_str()));
                    let new_pos = shift_isize(caret, delta) + open.len() + 1;
                    planned_sels.push((new_pos, new_pos, false));
                    delta += isize::try_from(snippet.len()).unwrap_or(0);
                }
            } else {
                let range = sel.range();
                let selected = slice_text(&snap, range.clone());
                if selected.len() >= open.len() + close.len()
                    && selected.starts_with(open)
                    && selected.ends_with(close)
                {
                    edits.push(TextEdit::delete(range.start..range.start + open.len()));
                    edits.push(TextEdit::delete(range.end - close.len()..range.end));
                    let new_start = shift_isize(range.start, delta);
                    let new_end =
                        shift_isize(range.end, delta).saturating_sub(open.len() + close.len());
                    planned_sels.push((new_start, new_end, reversed));
                    delta -= isize::try_from(open.len() + close.len()).unwrap_or(0);
                } else if range.start >= open.len()
                    && range.end + close.len() <= total
                    && slice_text(&snap, range.start - open.len()..range.start) == open
                    && slice_text(&snap, range.end..range.end + close.len()) == close
                {
                    edits.push(TextEdit::delete(range.start - open.len()..range.start));
                    edits.push(TextEdit::delete(range.end..range.end + close.len()));
                    let new_start = shift_isize(range.start, delta).saturating_sub(open.len());
                    let new_end = shift_isize(range.end, delta).saturating_sub(open.len());
                    planned_sels.push((new_start, new_end, reversed));
                    delta -= isize::try_from(open.len() + close.len()).unwrap_or(0);
                } else {
                    edits.push(TextEdit::new(range.start..range.start, open));
                    edits.push(TextEdit::new(range.end..range.end, close));
                    let new_start = shift_isize(range.start, delta);
                    let new_end = shift_isize(range.end, delta) + open.len() + close.len();
                    planned_sels.push((new_start, new_end, reversed));
                    delta += isize::try_from(open.len() + close.len()).unwrap_or(0);
                }
            }
        }
        if edits.is_empty() {
            return false;
        }
        let primary_idx = self.selections.primary_index();
        let changed = self.record(&edits, EditOrigin::Compound);
        if changed && !planned_sels.is_empty() {
            let post = self.buffer.snapshot();
            let next_sels: Vec<Selection> = planned_sels
                .into_iter()
                .map(|(start, end, rev)| {
                    if rev {
                        Selection::new(end, start)
                    } else {
                        Selection::new(start, end)
                    }
                })
                .collect();
            self.selections = SelectionSet::from_selections(next_sels, primary_idx, &post);
        }
        changed
    }

    /// `Alt+Shift+Up/Down`: swaps the active line (or selected block of lines) with the adjacent
    /// line above (`up = true`) or below (`up = false`), preserving selection and relative offset.
    pub fn move_lines(&mut self, up: bool) -> bool {
        if self.read_only {
            return false;
        }
        self.sync_external_edits();
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let primary = *self.selections.primary();
        let range = primary.range();
        let (Ok(first_pt), Ok(last_pt)) = (
            snap.byte_to_point(snap.clamp_byte_to_char_boundary(range.start)),
            snap.byte_to_point(snap.clamp_byte_to_char_boundary(range.end)),
        ) else {
            return false;
        };
        let first = first_pt.row;
        let last = if !primary.is_empty() && last_pt.column == 0 && last_pt.row > first {
            last_pt.row - 1
        } else {
            last_pt.row
        };
        if up && first == 0 {
            return false;
        }
        if !up && last + 1 >= snap.len_lines() {
            return false;
        }

        let (span, replacement, shift_bytes): (Range<usize>, String, isize) = if up {
            let other = first - 1;
            let other_range = line_content_range(&snap, other);
            let block_start = line_start_byte(&snap, first);
            let block_end = line_content_range(&snap, last).end;
            let sep = slice_text(&snap, other_range.end..block_start);
            let other_text = slice_text(&snap, other_range.clone());
            let block_text = slice_text(&snap, block_start..block_end);
            let shift = isize::try_from(other_range.start).unwrap_or(0)
                - isize::try_from(block_start).unwrap_or(0);
            (
                other_range.start..block_end,
                format!("{block_text}{sep}{other_text}"),
                shift,
            )
        } else {
            let other = last + 1;
            let block_start = line_start_byte(&snap, first);
            let block_end = line_content_range(&snap, last).end;
            let other_range = line_content_range(&snap, other);
            let sep = slice_text(&snap, block_end..other_range.start);
            let block_text = slice_text(&snap, block_start..block_end);
            let other_text = slice_text(&snap, other_range.clone());
            let shift = isize::try_from(other_text.len() + sep.len()).unwrap_or(0);
            (
                block_start..other_range.end,
                format!("{other_text}{sep}{block_text}"),
                shift,
            )
        };

        let changed = self.record(
            &[TextEdit::new(span, replacement.as_str())],
            EditOrigin::Compound,
        );
        if changed {
            let post = self.buffer.snapshot();
            let shift_offset = |off: usize| -> usize {
                let shifted = isize::try_from(off)
                    .unwrap_or(0)
                    .saturating_add(shift_bytes)
                    .max(0);
                usize::try_from(shifted).unwrap_or(0).min(post.len_bytes())
            };
            let new_anchor = shift_offset(primary.anchor());
            let new_head = shift_offset(primary.head());
            self.selections =
                SelectionSet::single(Selection::new(new_anchor, new_head).clamp(&post));
        }
        changed
    }

    /// `Alt+Up` (macOS) / `Ctrl+W` (Linux): hierarchically expands the primary selection from
    /// caret -> word -> AST node (or delimiters/line) -> whole buffer, pushing onto history.
    pub fn extend_selection(&mut self, syntax: Option<&EditorSyntax>) -> bool {
        self.sync_external_edits();
        self.marked_range = None;
        self.occurrence_stack.clear();
        let snap = self.buffer.snapshot();
        if snap.len_bytes() == 0 {
            return false;
        }
        let primary = *self.selections.primary();
        let range = primary.range();

        let mut next: Option<Range<usize>> = None;
        if primary.is_empty() {
            let word = word_range_at(&snap, primary.head());
            if !word.is_empty()
                && slice_text(&snap, word.clone())
                    .chars()
                    .next()
                    .is_some_and(is_word_char)
            {
                next = Some(word);
            }
        }
        if next.is_none()
            && let Some(syn) = syntax
            && syn.snapshot().version() == snap.version()
        {
            next = syn.enclosing_node_range(range.start, range.end);
        }
        if next.is_none() {
            next = fallback_extend_range(&snap, &range);
        }
        let Some(next_range) = next else {
            return false;
        };
        if next_range == range || next_range.start > range.start || next_range.end < range.end {
            return false;
        }
        self.selection_stack.push((snap.version(), primary));
        self.selections = SelectionSet::single(Selection::new(next_range.start, next_range.end));
        true
    }

    /// `Alt+Down` (macOS) / `Ctrl+Shift+W` (Linux): restores the previous selection from the
    /// [`Self::extend_selection`] stack.
    pub fn shrink_selection(&mut self) -> bool {
        self.sync_external_edits();
        self.marked_range = None;
        self.occurrence_stack.clear();
        let snap = self.buffer.snapshot();
        let version = snap.version();
        while let Some((ver, prev)) = self.selection_stack.pop() {
            if ver == version {
                self.selections = SelectionSet::single(prev.clamp(&snap));
                return true;
            }
        }
        false
    }

    /// `Cmd+Alt+L` (macOS) / `Ctrl+Alt+L` (Linux): trims trailing whitespace and normalizes
    /// brace indentation (`indent_width` spaces per level) in one undo transaction.
    pub fn reformat_code(&mut self, language: SourceLanguageKind, indent_width: usize) -> bool {
        let brace_indent = !matches!(language, SourceLanguageKind::Unknown);
        self.reformat_code_with_braces(brace_indent, indent_width)
    }

    /// Formats the document using optional brace-based indentation plus trailing-whitespace trimming.
    pub fn reformat_code_with_braces(&mut self, brace_indent: bool, indent_width: usize) -> bool {
        if self.read_only {
            return false;
        }
        self.sync_external_edits();
        self.marked_range = None;
        let snap = self.buffer.snapshot();
        let original = snap.text();
        let formatted = format_document_text(&original, brace_indent, indent_width.max(1));
        if formatted == original {
            return false;
        }
        let (line, col) = self.primary_line_col();
        let changed = self.record(
            &[TextEdit::new(0..snap.len_bytes(), formatted.as_str())],
            EditOrigin::Compound,
        );
        if changed {
            self.set_line_col(line, col);
        }
        changed
    }

    // ------------------------------------------------------------------- mouse

    /// Replaces all selections with a single caret at `byte` (plain click).
    pub fn set_caret(&mut self, byte: usize) {
        self.marked_range = None;
        self.clear_selection_stacks();
        let snap = self.buffer.snapshot();
        self.selections = SelectionSet::caret(snap.clamp_byte_to_char_boundary(byte));
    }

    /// Keeps the primary anchor and moves the head to `byte` (`Shift+Click`, drag).
    pub fn extend_primary_to(&mut self, byte: usize) {
        self.marked_range = None;
        self.clear_selection_stacks();
        let snap = self.buffer.snapshot();
        let anchor = self.selections.primary().anchor();
        let head = snap.clamp_byte_to_char_boundary(byte);
        self.selections = SelectionSet::single(Selection::new(anchor, head));
    }

    /// Adds a caret at `byte` and makes it primary (`Alt+Click`).
    pub fn add_cursor_at(&mut self, byte: usize) {
        self.marked_range = None;
        self.clear_selection_stacks();
        let snap = self.buffer.snapshot();
        let caret = Selection::caret(snap.clamp_byte_to_char_boundary(byte));
        self.selections.push(caret, true, &snap);
    }

    /// Selects the word under `byte` (double click).
    pub fn select_word_at(&mut self, byte: usize) {
        self.marked_range = None;
        self.clear_selection_stacks();
        let snap = self.buffer.snapshot();
        let range = word_range_at(&snap, byte);
        self.selections = SelectionSet::single(Selection::new(range.start, range.end));
    }

    /// Selects whole lines from `anchor_row` to `head_row`, including the line endings
    /// (triple click, gutter click and gutter drag).
    pub fn select_lines(&mut self, anchor_row: usize, head_row: usize) {
        self.marked_range = None;
        self.clear_selection_stacks();
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

    fn clear_selection_stacks(&mut self) {
        self.selection_stack.clear();
        self.occurrence_stack.clear();
    }

    fn record(&mut self, edits: &[TextEdit], origin: EditOrigin) -> bool {
        if self.read_only {
            return false;
        }
        self.sync_external_edits();
        self.clear_selection_stacks();
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

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$'
}

fn shift_isize(offset: usize, delta: isize) -> usize {
    let shifted = isize::try_from(offset)
        .unwrap_or(0)
        .saturating_add(delta)
        .max(0);
    usize::try_from(shifted).unwrap_or(0)
}

fn enclosing_block_comment_in_line(
    line: &str,
    rel: usize,
    open: &str,
    close: &str,
) -> Option<(usize, usize)> {
    let open_rel = line[..rel].rfind(open)?;
    let search_from = open_rel + open.len();
    if search_from > rel {
        return None;
    }
    // If `close` already appeared between `open` and the caret, the caret is outside the block.
    if line[search_from..rel].contains(close) {
        return None;
    }
    let close_offset = line[rel..].find(close)?;
    Some((open_rel, rel + close_offset))
}

fn fallback_extend_range(snap: &BufferSnapshot, range: &Range<usize>) -> Option<Range<usize>> {
    let len = snap.len_bytes();
    if len == 0 {
        return None;
    }
    if range.is_empty() {
        let word = word_range_at(snap, range.start);
        if !word.is_empty()
            && slice_text(snap, word.clone())
                .chars()
                .next()
                .is_some_and(is_word_char)
        {
            return Some(word);
        }
    }

    let start_pt = snap
        .byte_to_point(snap.clamp_byte_to_char_boundary(range.start))
        .ok()?;
    let end_pt = snap
        .byte_to_point(snap.clamp_byte_to_char_boundary(range.end))
        .ok()?;
    if start_pt.row == end_pt.row {
        let line_range = line_content_range(snap, start_pt.row);
        let line = slice_text(snap, line_range.clone());
        let rel_start = range.start.saturating_sub(line_range.start).min(line.len());
        let rel_end = range.end.saturating_sub(line_range.start).min(line.len());

        if let Some(delim_range) = enclosing_delimiter_range(&line, rel_start..rel_end) {
            return Some(
                (line_range.start + delim_range.start)..(line_range.start + delim_range.end),
            );
        }

        let trimmed_start = line.len() - line.trim_start().len();
        let trimmed_end = line.trim_end().len();
        if trimmed_start < trimmed_end {
            let trimmed_abs = (line_range.start + trimmed_start)..(line_range.start + trimmed_end);
            if strictly_contains(&trimmed_abs, range) {
                return Some(trimmed_abs);
            }
        }
        if strictly_contains(&line_range, range) {
            return Some(line_range);
        }
        let with_eol = line_range.start..line_end_with_ending_byte(snap, start_pt.row);
        if strictly_contains(&with_eol, range) {
            return Some(with_eol);
        }
    }

    let full = 0..len;
    strictly_contains(&full, range).then_some(full)
}

fn strictly_contains(outer: &Range<usize>, inner: &Range<usize>) -> bool {
    outer.start <= inner.start && outer.end >= inner.end && outer != inner
}

fn enclosing_delimiter_range(text: &str, inner: Range<usize>) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let mut stack: Vec<(u8, usize)> = Vec::new();
    let mut best: Option<Range<usize>> = None;
    let mut in_quote: Option<(u8, usize)> = None;
    let mut escaped = false;

    for (idx, &b) in bytes.iter().enumerate() {
        if let Some((quote, q_start)) = in_quote {
            if escaped {
                escaped = false;
                continue;
            }
            if b == b'\\' {
                escaped = true;
                continue;
            }
            if b == quote {
                let candidate =
                    if q_start + 1 < idx && strictly_contains(&((q_start + 1)..idx), &inner) {
                        (q_start + 1)..idx
                    } else {
                        q_start..(idx + 1)
                    };
                if strictly_contains(&candidate, &inner)
                    && best.as_ref().is_none_or(|b| candidate.len() < b.len())
                {
                    best = Some(candidate);
                }
                in_quote = None;
            }
            continue;
        }
        if b == b'"' || b == b'\'' {
            in_quote = Some((b, idx));
            continue;
        }
        match b {
            b'(' | b'[' | b'{' => stack.push((b, idx)),
            b')' | b']' | b'}' => {
                let expected = match b {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                if let Some((open_b, open_idx)) = stack.pop()
                    && open_b == expected
                {
                    let candidate = if open_idx + 1 < idx
                        && strictly_contains(&((open_idx + 1)..idx), &inner)
                    {
                        (open_idx + 1)..idx
                    } else {
                        open_idx..(idx + 1)
                    };
                    if strictly_contains(&candidate, &inner)
                        && best.as_ref().is_none_or(|cur| candidate.len() < cur.len())
                    {
                        best = Some(candidate);
                    }
                }
            }
            _ => {}
        }
    }
    best
}

fn format_document_text(text: &str, brace_indent: bool, indent_width: usize) -> String {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let trailing_eol = text.ends_with('\n');
    let lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let line_count = if trailing_eol {
        lines.len().saturating_sub(1)
    } else {
        lines.len()
    };

    let mut out = String::with_capacity(text.len());
    let mut depth = 0usize;
    let mut in_block_comment = false;

    for (idx, raw_line) in lines.iter().take(line_count).enumerate() {
        let trimmed_end = raw_line.trim_end_matches([' ', '\t']);
        if trimmed_end.trim().is_empty() {
            if idx + 1 < line_count || trailing_eol {
                out.push_str(eol);
            }
            continue;
        }

        if brace_indent {
            let trimmed = trimmed_end.trim_start_matches([' ', '\t']);
            let leading_close = if in_block_comment {
                0
            } else {
                trimmed
                    .chars()
                    .take_while(|c| matches!(c, '}' | ')' | ']'))
                    .count()
            };
            let line_depth = depth.saturating_sub(leading_close);
            let extra_javadoc_space = usize::from(in_block_comment && trimmed.starts_with('*'));
            for _ in 0..(line_depth * indent_width + extra_javadoc_space) {
                out.push(' ');
            }
            out.push_str(trimmed);
            update_brace_depth(trimmed, &mut depth, &mut in_block_comment);
        } else {
            out.push_str(trimmed_end);
        }

        if idx + 1 < line_count || trailing_eol {
            out.push_str(eol);
        }
    }

    out
}

fn update_brace_depth(line: &str, depth: &mut usize, in_block_comment: &mut bool) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    let mut escaped = false;

    while i < bytes.len() {
        let b = bytes[i];
        if *in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                *in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            break;
        }
        if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            *in_block_comment = true;
            i += 2;
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
            i += 1;
            continue;
        }
        match b {
            b'{' | b'(' | b'[' => *depth += 1,
            b'}' | b')' | b']' => *depth = depth.saturating_sub(1),
            _ => {}
        }
        i += 1;
    }
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
        assert!(!st.delete_lines());
        assert!(!st.toggle_block_comment("/*", "*/"));
        assert!(!st.move_lines(true));
        assert!(!st.move_lines(false));
        assert!(!st.reformat_code(SourceLanguageKind::Java, 4));
        assert!(!st.replace_text_in_range(None, "abc"));
        assert!(!st.replace_and_mark_text_in_range(None, "abc", None));
        assert!(!st.undo());
        assert!(!st.redo());
        assert_eq!(text(&st), "class Decompiled {}");

        st.select_word_at(6);
        assert_eq!(st.copy().as_deref(), Some("Decompiled"));
        // Selection actions stay allowed in read-only mode.
        assert!(st.extend_selection(None));
        assert!(st.shrink_selection());
        st.set_caret(6);
        assert!(st.select_all_occurrences());
        assert!(st.unselect_occurrence());
        assert_eq!(text(&st), "class Decompiled {}");
    }

    #[test]
    fn delete_lines_handles_single_last_multicursor_and_undo() {
        let mut st = EditorState::from_text("one\ntwo\nthree");
        st.set_line_col(2, 2);
        assert!(st.delete_lines());
        assert_eq!(text(&st), "one\nthree");
        assert_eq!(st.primary_line_col().0, 2);
        assert!(st.undo());
        assert_eq!(text(&st), "one\ntwo\nthree");

        // Deleting last line without trailing newline removes preceding EOL too.
        st.set_line_col(3, 2);
        assert!(st.delete_lines());
        assert_eq!(text(&st), "one\ntwo");
        assert_eq!(st.primary_line_col(), (2, 4));

        // Multi-cursor delete lines merges into one undo step.
        let mut multi = EditorState::from_text("a\nb\nc\nd\n");
        multi.set_caret(0);
        multi.add_cursor_at(4);
        assert!(multi.delete_lines());
        assert_eq!(text(&multi), "b\nd\n");
        assert!(multi.undo());
        assert_eq!(text(&multi), "a\nb\nc\nd\n");
    }

    #[test]
    fn toggle_block_comment_wraps_unwraps_and_inserts_at_caret() {
        let mut st = EditorState::from_text("int value = 42;");
        let start = text(&st).find("value").unwrap();
        st.set_selection(start, start + "value".len());
        assert!(st.toggle_block_comment("/*", "*/"));
        assert_eq!(text(&st), "int /*value*/ = 42;");
        assert!(st.toggle_block_comment("/*", "*/"));
        assert_eq!(text(&st), "int value = 42;");

        // Empty caret inside an existing block removes delimiters; outside inserts `/*  */`.
        let mut empty = EditorState::from_text("alpha /* inside */ omega");
        let inside = text(&empty).find("inside").unwrap() + 2;
        empty.set_caret(inside);
        assert!(empty.toggle_block_comment("/*", "*/"));
        assert_eq!(text(&empty), "alpha  inside  omega");

        let mut blank = EditorState::from_text("abc");
        blank.set_caret(1);
        assert!(blank.toggle_block_comment("/*", "*/"));
        assert_eq!(text(&blank), "a/*  */bc");
        assert_eq!(blank.primary_head(), 4);
    }

    #[test]
    fn move_lines_swaps_single_and_multiline_blocks_preserving_eof() {
        let mut st = EditorState::from_text("first\nsecond\nthird");
        st.set_line_col(3, 3);
        assert!(!st.move_lines(false));
        assert!(st.move_lines(true));
        assert_eq!(text(&st), "first\nthird\nsecond");
        assert_eq!(st.primary_line_col(), (2, 3));
        assert!(st.move_lines(true));
        assert_eq!(text(&st), "third\nfirst\nsecond");
        assert!(!st.move_lines(true));
        assert!(st.move_lines(false));
        assert_eq!(text(&st), "first\nthird\nsecond");

        // Multi-line selection moves together as one undo transaction.
        let mut block = EditorState::from_text("0\n11\n22\n3");
        block.set_selection(2, 7); // "11\n22"
        assert!(block.move_lines(false));
        assert_eq!(text(&block), "0\n3\n11\n22");
        let sel = *block.selections().primary();
        assert_eq!(&text(&block)[sel.range()], "11\n22");
        assert!(block.undo());
        assert_eq!(text(&block), "0\n11\n22\n3");
    }

    #[test]
    fn extend_and_shrink_selection_uses_ast_and_fallback_stack() {
        use kova_syntax::{Highlighter, LanguageRegistry, SyntaxEngine, SyntaxEngineConfig};
        use kova_theme::SyntaxLanguage;

        let src = "class Foo {\n    int bar = (1 + 2);\n}\n";
        let registry = LanguageRegistry::with_defaults().unwrap();
        let engine = SyntaxEngine::new(
            registry.clone(),
            SyntaxEngineConfig {
                worker_threads: 1,
                ..SyntaxEngineConfig::default()
            },
        )
        .unwrap();
        let mut st = EditorState::from_text(src);
        let snap = st.snapshot();
        let tree = engine
            .parse_sync(SyntaxLanguage::Java, &snap, None)
            .unwrap();
        let syntax = EditorSyntax::build(tree, snap, Highlighter::new(registry));

        let bar_pos = src.find("bar").unwrap() + 1;
        st.set_caret(bar_pos);
        assert!(st.extend_selection(Some(&syntax)));
        assert_eq!(&text(&st)[st.selections().primary().range()], "bar");

        let mut steps = Vec::new();
        while st.extend_selection(Some(&syntax)) {
            let r = st.selections().primary().range();
            steps.push(text(&st)[r].to_owned());
        }
        assert!(steps.iter().any(|s| s.contains("int bar = (1 + 2);")));
        assert_eq!(steps.last().map(String::as_str), Some(src));

        // Shrink unwinds back to the original empty caret.
        for _ in 0..=steps.len() {
            assert!(st.shrink_selection());
        }
        assert!(!st.shrink_selection());
        assert!(st.selections().primary().is_empty());
        assert_eq!(st.primary_head(), bar_pos);

        // Fallback without AST handles delimiters, line, and whole buffer.
        let mut plain = EditorState::from_text("call(alpha, beta)\nnext");
        let a_pos = text(&plain).find("alpha").unwrap() + 2;
        plain.set_caret(a_pos);
        assert!(plain.extend_selection(None));
        assert_eq!(&text(&plain)[plain.selections().primary().range()], "alpha");
        assert!(plain.extend_selection(None));
        assert_eq!(
            &text(&plain)[plain.selections().primary().range()],
            "alpha, beta"
        );
        assert!(plain.extend_selection(None));
        assert_eq!(
            &text(&plain)[plain.selections().primary().range()],
            "(alpha, beta)"
        );
        assert!(plain.extend_selection(None));
        assert_eq!(
            &text(&plain)[plain.selections().primary().range()],
            "call(alpha, beta)"
        );
        assert!(plain.shrink_selection());
        assert_eq!(
            &text(&plain)[plain.selections().primary().range()],
            "(alpha, beta)"
        );
    }

    #[test]
    fn reformat_code_indents_braces_ignores_strings_and_trims_trailing_whitespace() {
        let messy = "class A {   \nString s = \"}{}\";\t\n   int x = 1;  \n   \n}\n";
        let mut st = EditorState::from_text(messy);
        st.set_line_col(3, 5);
        assert!(st.reformat_code(SourceLanguageKind::Java, 4));
        assert_eq!(
            text(&st),
            "class A {\n    String s = \"}{}\";\n    int x = 1;\n\n}\n"
        );
        assert!(!st.reformat_code(SourceLanguageKind::Java, 4));
        assert!(st.undo());
        assert_eq!(text(&st), messy);
    }

    #[test]
    fn unselect_and_select_all_occurrences_preserve_stack() {
        let mut st = EditorState::from_text("item x = item + item;");
        st.set_caret(1);
        assert!(st.add_next_occurrence());
        assert_eq!(st.cursor_count(), 1);
        assert!(st.add_next_occurrence());
        assert_eq!(st.cursor_count(), 2);
        assert!(st.unselect_occurrence());
        assert_eq!(st.cursor_count(), 1);
        assert!(st.unselect_occurrence());
        assert!(st.selections().primary().is_empty());

        assert!(st.select_all_occurrences());
        assert_eq!(st.cursor_count(), 3);
        assert!(st.unselect_occurrence());
        assert_eq!(st.cursor_count(), 1);
    }
}
