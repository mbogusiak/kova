//! Multi-cursor selection model (`Selection`, `SelectionSet`) with automatic range merging.

use std::ops::Range;

use smallvec::{SmallVec, smallvec};

use crate::grapheme::{
    BoundaryAffinity, next_grapheme_boundary, next_word_boundary, prev_grapheme_boundary,
    prev_word_boundary, snap_byte_to_grapheme_boundary,
};
use crate::motion::{
    Motion, MotionConfig, byte_at_visual_column, line_first_non_whitespace_byte, visual_column_at,
};
use crate::position::TextEdit;
use crate::snapshot::BufferSnapshot;

/// Orientation of a [`Selection`] relative to its anchor and head.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// `head >= anchor`.
    Forward,
    /// `head < anchor`.
    Backward,
}

/// A single cursor or text selection anchored by byte offsets in a buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Selection {
    anchor: usize,
    head: usize,
    goal_column: Option<usize>,
}

impl Selection {
    /// Creates an empty caret selection (`anchor == head == offset`).
    #[must_use]
    pub const fn caret(offset: usize) -> Self {
        Self {
            anchor: offset,
            head: offset,
            goal_column: None,
        }
    }

    /// Creates a selection spanning from `anchor` to `head`.
    #[must_use]
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self {
            anchor,
            head,
            goal_column: None,
        }
    }

    /// Returns a copy of this selection with `goal_column` updated.
    #[must_use]
    pub const fn with_goal_column(mut self, goal_column: Option<usize>) -> Self {
        self.goal_column = goal_column;
        self
    }

    /// Returns the anchor byte offset (the stationary end when extending a selection).
    #[must_use]
    pub const fn anchor(&self) -> usize {
        self.anchor
    }

    /// Returns the head (active cursor) byte offset.
    #[must_use]
    pub const fn head(&self) -> usize {
        self.head
    }

    /// Returns the preserved visual goal column for vertical movement, if any.
    #[must_use]
    pub const fn goal_column(&self) -> Option<usize> {
        self.goal_column
    }

    /// Returns the lower byte bound `min(anchor, head)`.
    #[must_use]
    pub const fn start(&self) -> usize {
        if self.anchor <= self.head {
            self.anchor
        } else {
            self.head
        }
    }

    /// Returns the upper byte bound `max(anchor, head)`.
    #[must_use]
    pub const fn end(&self) -> usize {
        if self.anchor >= self.head {
            self.anchor
        } else {
            self.head
        }
    }

    /// Returns `start()..end()` as a half-open byte range.
    #[must_use]
    pub const fn range(&self) -> Range<usize> {
        self.start()..self.end()
    }

    /// Returns `true` when this selection is an empty caret (`anchor == head`).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Returns `true` when `head < anchor`.
    #[must_use]
    pub const fn is_reversed(&self) -> bool {
        self.head < self.anchor
    }

    /// Returns [`Direction::Backward`] if `head < anchor`, else [`Direction::Forward`].
    #[must_use]
    pub const fn direction(&self) -> Direction {
        if self.head < self.anchor {
            Direction::Backward
        } else {
            Direction::Forward
        }
    }

    /// Collapses the selection to `start()`, clearing `goal_column`.
    #[must_use]
    pub const fn collapse_to_start(self) -> Self {
        Self::caret(self.start())
    }

    /// Collapses the selection to `end()`, clearing `goal_column`.
    #[must_use]
    pub const fn collapse_to_end(self) -> Self {
        Self::caret(self.end())
    }

    /// Collapses the selection to `head()`, clearing `goal_column`.
    #[must_use]
    pub const fn collapse_to_head(self) -> Self {
        Self::caret(self.head)
    }

    /// Snaps both `anchor` and `head` to valid extended grapheme boundaries in `snapshot`.
    #[must_use]
    pub fn clamp(self, snapshot: &BufferSnapshot) -> Self {
        let anchor = snap_byte_to_grapheme_boundary(snapshot, self.anchor, BoundaryAffinity::Floor);
        let head = if self.head == self.anchor {
            anchor
        } else {
            snap_byte_to_grapheme_boundary(snapshot, self.head, BoundaryAffinity::Floor)
        };
        Self {
            anchor,
            head,
            goal_column: self.goal_column,
        }
    }

    #[must_use]
    const fn from_bounds(
        start: usize,
        end: usize,
        direction: Direction,
        goal_column: Option<usize>,
    ) -> Self {
        match direction {
            Direction::Forward => Self {
                anchor: start,
                head: end,
                goal_column,
            },
            Direction::Backward => Self {
                anchor: end,
                head: start,
                goal_column,
            },
        }
    }
}

/// Ordered, non-empty, disjoint set of [`Selection`]s with a designated primary cursor.
///
/// Invariants guaranteed after every operation:
/// 1. `len() >= 1` (never empty).
/// 2. Sorted strictly by `(start(), end())`.
/// 3. Overlapping selections, duplicate carets at the same offset, and carets touching
///    the boundary of a selection are automatically merged, whereas adjacent non-empty
///    ranges (`[0..3)` and `[3..6)`) remain distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSet {
    selections: SmallVec<[Selection; 1]>,
    primary_index: usize,
}

impl Default for SelectionSet {
    fn default() -> Self {
        Self::caret(0)
    }
}

impl SelectionSet {
    /// Creates a [`SelectionSet`] containing a single selection.
    #[must_use]
    pub fn single(selection: Selection) -> Self {
        Self {
            selections: smallvec![selection],
            primary_index: 0,
        }
    }

    /// Creates a [`SelectionSet`] containing a single caret at `offset`.
    #[must_use]
    pub fn caret(offset: usize) -> Self {
        Self::single(Selection::caret(offset))
    }

    /// Builds a normalized [`SelectionSet`] from `selections` against `snapshot`.
    ///
    /// If `selections` is empty, falls back to a single caret at offset `0`.
    /// Out-of-bounds `primary_index` is clamped to the last input selection before merging.
    #[must_use]
    pub fn from_selections(
        selections: impl IntoIterator<Item = Selection>,
        primary_index: usize,
        snapshot: &BufferSnapshot,
    ) -> Self {
        let collected: SmallVec<[Selection; 1]> = selections.into_iter().collect();
        if collected.is_empty() {
            return Self::caret(0);
        }
        let clamped_primary = primary_index.min(collected.len() - 1);
        let mut set = Self {
            selections: collected,
            primary_index: clamped_primary,
        };
        set.normalize_and_merge(snapshot);
        set
    }

    /// Returns the primary [`Selection`].
    #[must_use]
    pub fn primary(&self) -> &Selection {
        &self.selections[self.primary_index]
    }

    /// Returns the 0-based index of the primary selection within [`Self::as_slice`].
    #[must_use]
    pub const fn primary_index(&self) -> usize {
        self.primary_index
    }

    /// Returns all selections in sorted document order.
    #[must_use]
    pub fn as_slice(&self) -> &[Selection] {
        &self.selections
    }

    /// Returns the number of active selections (always `>= 1`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.selections.len()
    }

    /// Always returns `false` because [`SelectionSet`] is guaranteed non-empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Adds `selection`, merges any overlaps against `snapshot`, and optionally promotes it to primary.
    pub fn push(&mut self, selection: Selection, make_primary: bool, snapshot: &BufferSnapshot) {
        self.selections.push(selection);
        if make_primary {
            self.primary_index = self.selections.len() - 1;
        }
        self.normalize_and_merge(snapshot);
    }

    /// Collapses all cursors down to the primary selection (`Esc` behavior).
    pub fn collapse_to_primary(&mut self) {
        let primary = self.selections[self.primary_index];
        self.selections.clear();
        self.selections.push(primary);
        self.primary_index = 0;
    }

    /// Selects the entire buffer `[0..len_bytes()]` as a single forward selection.
    pub fn select_all(&mut self, snapshot: &BufferSnapshot) {
        self.selections.clear();
        self.selections
            .push(Selection::new(0, snapshot.len_bytes()));
        self.primary_index = 0;
    }

    /// Sets the primary selection index, returning `false` if `index >= len()`.
    pub fn set_primary(&mut self, index: usize) -> bool {
        if index >= self.selections.len() {
            return false;
        }
        self.primary_index = index;
        true
    }

    /// Moves every selection according to `motion` and `extend`, then merges overlaps.
    pub fn move_by(
        &mut self,
        snapshot: &BufferSnapshot,
        motion: Motion,
        extend: bool,
        cfg: MotionConfig,
    ) {
        for sel in &mut self.selections {
            *sel = move_single(*sel, snapshot, motion, extend, cfg);
        }
        self.normalize_and_merge(snapshot);
    }

    /// Adds a cursor on the line immediately above the topmost or primary cursor (preserving `goal_column`).
    pub fn add_cursor_above(&mut self, snapshot: &BufferSnapshot, cfg: MotionConfig) {
        let base = self.selections[self.primary_index];
        let clamped_head =
            snap_byte_to_grapheme_boundary(snapshot, base.head(), BoundaryAffinity::Floor);
        let row = snapshot.rope().byte_to_line(clamped_head);
        if row == 0 {
            return;
        }
        let goal = base
            .goal_column()
            .unwrap_or_else(|| visual_column_at(snapshot, clamped_head, cfg));
        let new_head = byte_at_visual_column(snapshot, row - 1, goal, cfg);
        self.push(
            Selection::caret(new_head).with_goal_column(Some(goal)),
            true,
            snapshot,
        );
    }

    /// Adds a cursor on the line immediately below the primary cursor (preserving `goal_column`).
    pub fn add_cursor_below(&mut self, snapshot: &BufferSnapshot, cfg: MotionConfig) {
        let base = self.selections[self.primary_index];
        let clamped_head =
            snap_byte_to_grapheme_boundary(snapshot, base.head(), BoundaryAffinity::Floor);
        let row = snapshot.rope().byte_to_line(clamped_head);
        if row + 1 >= snapshot.len_lines() {
            return;
        }
        let goal = base
            .goal_column()
            .unwrap_or_else(|| visual_column_at(snapshot, clamped_head, cfg));
        let new_head = byte_at_visual_column(snapshot, row + 1, goal, cfg);
        self.push(
            Selection::caret(new_head).with_goal_column(Some(goal)),
            true,
            snapshot,
        );
    }

    /// Selects the word under `primary` if empty, or adds the next occurrence of `primary`'s text
    /// (wrapping around EOF). Returns `false` if no new occurrence could be added.
    pub fn add_next_occurrence(&mut self, snapshot: &BufferSnapshot) -> bool {
        let primary = self.selections[self.primary_index].clamp(snapshot);
        if primary.is_empty() {
            let Some((w_start, w_end)) = word_under_offset(snapshot, primary.head()) else {
                return false;
            };
            self.selections[self.primary_index] = Selection::new(w_start, w_end);
            self.normalize_and_merge(snapshot);
            return true;
        }

        let Ok(needle_slice) = snapshot.slice_bytes(primary.range()) else {
            return false;
        };
        let needle = needle_slice.to_string();
        if needle.is_empty() {
            return false;
        }

        let full_text = snapshot.text();
        let search_start = primary.end();

        if let Some(pos) = find_next_non_overlapping(&full_text, &needle, search_start, self) {
            self.push(Selection::new(pos, pos + needle.len()), true, snapshot);
            return true;
        }
        false
    }

    /// Translates all selection offsets across a batch of pre-edit [`TextEdit`]s
    /// into `post_snapshot` coordinates, clearing `goal_column` and merging overlaps.
    pub fn map_through_edits(&mut self, pre_edits: &[TextEdit], post_snapshot: &BufferSnapshot) {
        if pre_edits.is_empty() {
            self.normalize_and_merge(post_snapshot);
            return;
        }

        let mut sorted_edits: Vec<&TextEdit> = pre_edits.iter().collect();
        sorted_edits.sort_by(|a, b| {
            a.range
                .start
                .cmp(&b.range.start)
                .then_with(|| a.range.end.cmp(&b.range.end))
        });

        for sel in &mut self.selections {
            let new_anchor = map_offset(sel.anchor, &sorted_edits);
            let new_head = map_offset(sel.head, &sorted_edits);
            *sel = Selection::new(new_anchor, new_head);
        }
        self.normalize_and_merge(post_snapshot);
    }

    pub(crate) fn normalize_and_merge(&mut self, snapshot: &BufferSnapshot) {
        let mut tagged: SmallVec<[(Selection, bool); 4]> = self
            .selections
            .iter()
            .enumerate()
            .map(|(idx, &sel)| (sel.clamp(snapshot), idx == self.primary_index))
            .collect();

        tagged.sort_by(|(a, _), (b, _)| {
            a.start()
                .cmp(&b.start())
                .then_with(|| a.end().cmp(&b.end()))
        });

        let mut merged: SmallVec<[Selection; 1]> = SmallVec::with_capacity(tagged.len());
        let mut primary_flags: SmallVec<[bool; 4]> = SmallVec::with_capacity(tagged.len());

        for (sel, is_primary) in tagged {
            let prev_idx = merged.len().saturating_sub(1);
            if let Some(prev) = merged.last_mut() {
                let prev_is_primary = primary_flags[prev_idx];

                let same_empty = prev.is_empty() && sel.is_empty() && prev.start() == sel.start();
                let overlaps = sel.start() < prev.end();
                let caret_touches =
                    sel.start() == prev.end() && (prev.is_empty() || sel.is_empty());

                if same_empty || overlaps || caret_touches {
                    let merged_start = prev.start();
                    let merged_end = prev.end().max(sel.end());
                    let combined_primary = prev_is_primary || is_primary;

                    let (direction, goal) = if prev_is_primary && !is_primary {
                        (prev.direction(), prev.goal_column())
                    } else if is_primary || sel.end() >= prev.end() {
                        (sel.direction(), sel.goal_column())
                    } else {
                        (prev.direction(), prev.goal_column())
                    };

                    *prev = Selection::from_bounds(merged_start, merged_end, direction, goal);
                    primary_flags[prev_idx] = combined_primary;
                    continue;
                }
            }

            merged.push(sel);
            primary_flags.push(is_primary);
        }

        if merged.is_empty() {
            merged.push(Selection::caret(0));
            primary_flags.push(true);
        }

        self.selections = merged;
        self.primary_index = primary_flags.iter().position(|&p| p).unwrap_or(0);
    }
}

fn move_single(
    sel: Selection,
    snapshot: &BufferSnapshot,
    motion: Motion,
    extend: bool,
    cfg: MotionConfig,
) -> Selection {
    let clamped = sel.clamp(snapshot);
    let head = clamped.head();

    match motion {
        Motion::Left => {
            if !extend && !clamped.is_empty() {
                return clamped.collapse_to_start();
            }
            let new_head = prev_grapheme_boundary(snapshot, head);
            if extend {
                Selection::new(clamped.anchor(), new_head)
            } else {
                Selection::caret(new_head)
            }
        }
        Motion::Right => {
            if !extend && !clamped.is_empty() {
                return clamped.collapse_to_end();
            }
            let new_head = next_grapheme_boundary(snapshot, head);
            if extend {
                Selection::new(clamped.anchor(), new_head)
            } else {
                Selection::caret(new_head)
            }
        }
        Motion::Up => {
            let row = snapshot.rope().byte_to_line(head);
            if row == 0 {
                return if extend {
                    Selection::new(clamped.anchor(), 0)
                } else {
                    Selection::caret(0)
                };
            }
            let goal = clamped
                .goal_column()
                .unwrap_or_else(|| visual_column_at(snapshot, head, cfg));
            let new_head = byte_at_visual_column(snapshot, row - 1, goal, cfg);
            if extend {
                Selection::new(clamped.anchor(), new_head).with_goal_column(Some(goal))
            } else {
                Selection::caret(new_head).with_goal_column(Some(goal))
            }
        }
        Motion::Down => {
            let row = snapshot.rope().byte_to_line(head);
            if row + 1 >= snapshot.len_lines() {
                let eof = snapshot.len_bytes();
                return if extend {
                    Selection::new(clamped.anchor(), eof)
                } else {
                    Selection::caret(eof)
                };
            }
            let goal = clamped
                .goal_column()
                .unwrap_or_else(|| visual_column_at(snapshot, head, cfg));
            let new_head = byte_at_visual_column(snapshot, row + 1, goal, cfg);
            if extend {
                Selection::new(clamped.anchor(), new_head).with_goal_column(Some(goal))
            } else {
                Selection::caret(new_head).with_goal_column(Some(goal))
            }
        }
        Motion::WordLeft => {
            let new_head = prev_word_boundary(snapshot, head);
            finish_horizontal(clamped.anchor(), new_head, extend)
        }
        Motion::WordRight => {
            let new_head = next_word_boundary(snapshot, head);
            finish_horizontal(clamped.anchor(), new_head, extend)
        }
        Motion::LineStart => {
            let row = snapshot.rope().byte_to_line(head);
            let new_head = snapshot.rope().line_to_byte(row);
            finish_horizontal(clamped.anchor(), new_head, extend)
        }
        Motion::LineFirstNonWhitespace => {
            let row = snapshot.rope().byte_to_line(head);
            let line_start = snapshot.rope().line_to_byte(row);
            let first_non_ws = line_first_non_whitespace_byte(snapshot, row);
            let new_head = if head == first_non_ws {
                line_start
            } else {
                first_non_ws
            };
            finish_horizontal(clamped.anchor(), new_head, extend)
        }
        Motion::LineEnd => {
            let row = snapshot.rope().byte_to_line(head);
            let line_start = snapshot.rope().line_to_byte(row);
            let content_len = snapshot.line_len_bytes(row).unwrap_or(0);
            finish_horizontal(clamped.anchor(), line_start + content_len, extend)
        }
        Motion::BufferStart => finish_horizontal(clamped.anchor(), 0, extend),
        Motion::BufferEnd => finish_horizontal(clamped.anchor(), snapshot.len_bytes(), extend),
    }
}

#[must_use]
const fn finish_horizontal(anchor: usize, new_head: usize, extend: bool) -> Selection {
    if extend {
        Selection::new(anchor, new_head)
    } else {
        Selection::caret(new_head)
    }
}

fn word_under_offset(snapshot: &BufferSnapshot, offset: usize) -> Option<(usize, usize)> {
    let row = snapshot.rope().byte_to_line(offset);
    let line_start = snapshot.rope().line_to_byte(row);
    let Ok(line_slice) = snapshot.line_slice(row) else {
        return None;
    };
    let line_str = crate::grapheme::slice_to_cow(line_slice);
    let rel = offset - line_start;

    let is_word_byte = |b: usize| -> bool {
        line_str[b..]
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$')
    };

    if rel < line_str.len() && is_word_byte(rel) {
        let start = prev_word_start_in_line(&line_str, rel);
        let end = next_word_end_in_line(&line_str, rel);
        return (start < end).then_some((line_start + start, line_start + end));
    }

    // If cursor sits immediately at the end of an identifier, select that identifier.
    if rel > 0 {
        let prev_char_start = line_str[..rel]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i);
        if is_word_byte(prev_char_start) {
            let start = prev_word_start_in_line(&line_str, prev_char_start);
            let end = next_word_end_in_line(&line_str, prev_char_start);
            return (start < end).then_some((line_start + start, line_start + end));
        }
    }

    None
}

fn prev_word_start_in_line(line: &str, mut idx: usize) -> usize {
    for (pos, ch) in line[..idx].char_indices().rev() {
        if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            idx = pos;
        } else {
            break;
        }
    }
    idx
}

fn next_word_end_in_line(line: &str, idx: usize) -> usize {
    let mut end = idx;
    for (pos, ch) in line[idx..].char_indices() {
        if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            end = idx + pos + ch.len_utf8();
        } else {
            break;
        }
    }
    end
}

fn find_next_non_overlapping(
    haystack: &str,
    needle: &str,
    search_start: usize,
    existing: &SelectionSet,
) -> Option<usize> {
    let is_free = |cand_start: usize, cand_end: usize| -> bool {
        !existing
            .as_slice()
            .iter()
            .any(|s| cand_start < s.end() && s.start() < cand_end)
    };

    let mut pos = search_start;
    while pos <= haystack.len() {
        if let Some(rel) = haystack[pos..].find(needle) {
            let abs = pos + rel;
            if is_free(abs, abs + needle.len()) {
                return Some(abs);
            }
            pos = abs + 1;
            while pos < haystack.len() && !haystack.is_char_boundary(pos) {
                pos += 1;
            }
        } else {
            break;
        }
    }

    // Wrap around from 0 up to `search_start`.
    let mut wrap_pos = 0;
    while wrap_pos < search_start {
        if let Some(rel) = haystack[wrap_pos..search_start.min(haystack.len())].find(needle) {
            let abs = wrap_pos + rel;
            if is_free(abs, abs + needle.len()) {
                return Some(abs);
            }
            wrap_pos = abs + 1;
            while wrap_pos < search_start && !haystack.is_char_boundary(wrap_pos) {
                wrap_pos += 1;
            }
        } else {
            break;
        }
    }

    None
}

#[must_use]
fn map_offset(offset: usize, sorted_edits: &[&TextEdit]) -> usize {
    let mut delta: isize = 0;
    for edit in sorted_edits {
        if offset < edit.range.start {
            break;
        }
        let old_len = isize::try_from(edit.range.len()).unwrap_or(isize::MAX);
        let new_len = isize::try_from(edit.replacement.len()).unwrap_or(isize::MAX);
        if offset <= edit.range.end {
            let base = isize::try_from(edit.range.start).unwrap_or(isize::MAX);
            let mapped = base.saturating_add(delta).saturating_add(new_len);
            return usize::try_from(mapped.max(0)).unwrap_or(0);
        }
        delta = delta.saturating_add(new_len - old_len);
    }
    let shifted = isize::try_from(offset)
        .unwrap_or(isize::MAX)
        .saturating_add(delta);
    usize::try_from(shifted.max(0)).unwrap_or(0)
}
