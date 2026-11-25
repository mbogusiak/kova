//! Multi-cursor editing helpers coordinating [`Buffer`] mutations and [`SelectionSet`] updates.

use std::ops::Range;

use crate::buffer::Buffer;
use crate::error::BufferError;
use crate::grapheme::{next_grapheme_boundary, prev_grapheme_boundary};
use crate::position::{BufferVersion, TextEdit};
use crate::selection::{Selection, SelectionSet};

/// Replaces every selection in `selections` with `text` and places carets
/// immediately after each inserted segment in updated buffer coordinates.
///
/// # Errors
///
/// Propagates any [`BufferError`] returned by [`Buffer::apply_edits`].
pub fn apply_insert_all(
    buffer: &mut Buffer,
    selections: &mut SelectionSet,
    text: &str,
) -> Result<BufferVersion, BufferError> {
    let edits: Vec<TextEdit> = selections
        .as_slice()
        .iter()
        .map(|sel| TextEdit::new(sel.range(), text))
        .collect();

    apply_edits_and_collapse_carets(buffer, selections, &edits)
}

/// Replaces each selection `i` in `selections` with `texts[i]`, updating all caret positions.
///
/// # Errors
///
/// Returns [`BufferError::CursorCountMismatch`] if `texts.len() != selections.len()`,
/// or propagates any [`BufferError`] from [`Buffer::apply_edits`].
pub fn apply_insert_per_cursor(
    buffer: &mut Buffer,
    selections: &mut SelectionSet,
    texts: &[&str],
) -> Result<BufferVersion, BufferError> {
    if texts.len() != selections.len() {
        return Err(BufferError::CursorCountMismatch {
            expected: selections.len(),
            actual: texts.len(),
        });
    }

    let edits: Vec<TextEdit> = selections
        .as_slice()
        .iter()
        .zip(texts.iter())
        .map(|(sel, &rep)| TextEdit::new(sel.range(), rep))
        .collect();

    apply_edits_and_collapse_carets(buffer, selections, &edits)
}

/// Deletes every non-empty selection, or one extended grapheme cluster before each empty caret.
///
/// Overlapping deletion ranges produced by adjacent cursors are merged before mutating `buffer`.
///
/// # Errors
///
/// Propagates any [`BufferError`] returned by [`Buffer::apply_edits`].
pub fn apply_delete_backward(
    buffer: &mut Buffer,
    selections: &mut SelectionSet,
) -> Result<BufferVersion, BufferError> {
    apply_delete_with(buffer, selections, |snap, sel| {
        if sel.is_empty() {
            let start = prev_grapheme_boundary(snap, sel.head());
            start..sel.head()
        } else {
            sel.range()
        }
    })
}

/// Deletes every non-empty selection, or one extended grapheme cluster after each empty caret.
///
/// Overlapping deletion ranges produced by adjacent cursors are merged before mutating `buffer`.
///
/// # Errors
///
/// Propagates any [`BufferError`] returned by [`Buffer::apply_edits`].
pub fn apply_delete_forward(
    buffer: &mut Buffer,
    selections: &mut SelectionSet,
) -> Result<BufferVersion, BufferError> {
    apply_delete_with(buffer, selections, |snap, sel| {
        if sel.is_empty() {
            let end = next_grapheme_boundary(snap, sel.head());
            sel.head()..end
        } else {
            sel.range()
        }
    })
}

fn apply_delete_with<F>(
    buffer: &mut Buffer,
    selections: &mut SelectionSet,
    compute_range: F,
) -> Result<BufferVersion, BufferError>
where
    F: Fn(&crate::snapshot::BufferSnapshot, &Selection) -> Range<usize>,
{
    let pre_snap = buffer.snapshot();
    let primary_idx = selections.primary_index();
    let mut raw_ranges: Vec<(Range<usize>, bool)> = selections
        .as_slice()
        .iter()
        .enumerate()
        .map(|(idx, sel)| (compute_range(&pre_snap, sel), idx == primary_idx))
        .collect();

    raw_ranges.sort_by(|(a, _), (b, _)| a.start.cmp(&b.start).then_with(|| a.end.cmp(&b.end)));

    let mut merged_ranges: Vec<(Range<usize>, bool)> = Vec::with_capacity(raw_ranges.len());
    for (range, is_primary) in raw_ranges {
        if let Some((prev, prev_primary)) = merged_ranges.last_mut()
            && (range.start < prev.end
                || (range.start == range.end
                    && prev.start == prev.end
                    && range.start == prev.start))
        {
            prev.end = prev.end.max(range.end);
            *prev_primary |= is_primary;
            continue;
        }
        merged_ranges.push((range, is_primary));
    }

    let edits: Vec<TextEdit> = merged_ranges
        .iter()
        .filter(|(r, _)| !r.is_empty())
        .map(|(r, _)| TextEdit::delete(r.clone()))
        .collect();

    let version = buffer.apply_edits(&edits)?;
    let post_snap = buffer.snapshot();

    let mut delta: isize = 0;
    let mut updated_carets: Vec<Selection> = Vec::with_capacity(merged_ranges.len());
    let mut new_primary_idx = 0;

    for (idx, (range, is_primary)) in merged_ranges.into_iter().enumerate() {
        let base = isize::try_from(range.start).unwrap_or(isize::MAX);
        let new_pos = usize::try_from(base.saturating_add(delta).max(0)).unwrap_or(0);
        updated_carets.push(Selection::caret(new_pos));
        if is_primary {
            new_primary_idx = idx;
        }
        let removed = isize::try_from(range.len()).unwrap_or(isize::MAX);
        delta = delta.saturating_sub(removed);
    }

    *selections = SelectionSet::from_selections(updated_carets, new_primary_idx, &post_snap);
    Ok(version)
}

fn apply_edits_and_collapse_carets(
    buffer: &mut Buffer,
    selections: &mut SelectionSet,
    edits: &[TextEdit],
) -> Result<BufferVersion, BufferError> {
    let version = buffer.apply_edits(edits)?;
    let post_snap = buffer.snapshot();
    let primary_idx = selections.primary_index();

    let mut delta: isize = 0;
    let mut updated: Vec<Selection> = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = isize::try_from(edit.range.start).unwrap_or(isize::MAX);
        let rep_len = isize::try_from(edit.replacement.len()).unwrap_or(isize::MAX);
        let old_len = isize::try_from(edit.range.len()).unwrap_or(isize::MAX);

        let caret_pos = usize::try_from(start.saturating_add(delta).saturating_add(rep_len).max(0))
            .unwrap_or(0);
        updated.push(Selection::caret(caret_pos));
        delta = delta.saturating_add(rep_len - old_len);
    }

    *selections = SelectionSet::from_selections(updated, primary_idx, &post_snap);
    Ok(version)
}
