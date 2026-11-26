//! Headless code-folding model (`FoldMap`): collapsed regions and the
//! `visual_row ↔ buffer_line` mapping used by the virtualized editor.
//!
//! Folding never changes the line height (`19 px` at `13 px`): a collapsed region keeps its
//! header line visible and hides `start_line + 1 ..= end_line`. Nested or overlapping collapsed
//! regions are merged into disjoint hidden intervals with prefix sums, so every conversion is
//! $O(\log n)$ in the number of collapsed regions and allocation-free.

use kova_syntax::FoldRange;

/// A foldable region proposed by the syntax layer (header line plus inclusive end line).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FoldCandidate {
    /// 0-based header line that stays visible and shows the fold chevron.
    pub start_line: usize,
    /// 0-based inclusive last line hidden when collapsed (`end_line > start_line`).
    pub end_line: usize,
}

impl FoldCandidate {
    /// Creates a new [`FoldCandidate`].
    #[must_use]
    pub const fn new(start_line: usize, end_line: usize) -> Self {
        Self {
            start_line,
            end_line,
        }
    }

    /// Returns `true` when `line` lies inside the region (header included).
    #[must_use]
    pub const fn contains(&self, line: usize) -> bool {
        line >= self.start_line && line <= self.end_line
    }

    const fn span(&self) -> usize {
        self.end_line - self.start_line
    }
}

impl From<&FoldRange> for FoldCandidate {
    fn from(range: &FoldRange) -> Self {
        Self::new(range.start_line, range.end_line)
    }
}

/// A collapsed region: `start_line` stays visible, `start_line + 1 ..= end_line` is hidden.
pub type CollapsedRegion = FoldCandidate;

/// Where a buffer line ends up on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualLineMapping {
    /// The line is visible at `visual_row`.
    Visible(usize),
    /// The line is hidden inside a collapsed region whose visible header is given.
    HiddenInsideFold {
        /// Buffer line of the visible header of the outermost collapsed region.
        header_buffer_line: usize,
        /// Visual row of that header.
        header_visual_row: usize,
    },
}

/// Disjoint hidden interval (inclusive) with the number of hidden lines before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HiddenInterval {
    first: usize,
    last: usize,
    hidden_before: usize,
}

impl HiddenInterval {
    const fn len(&self) -> usize {
        self.last - self.first + 1
    }

    /// Visual row at which the interval would start if it were visible.
    const fn visual_anchor(&self) -> usize {
        self.first - self.hidden_before
    }
}

/// Collapsed fold regions plus a prefix-offset index for $O(\log n)$ row conversions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FoldMap {
    collapsed: Vec<CollapsedRegion>,
    hidden: Vec<HiddenInterval>,
}

impl FoldMap {
    /// Creates an empty [`FoldMap`] (visual rows equal buffer lines).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            collapsed: Vec::new(),
            hidden: Vec::new(),
        }
    }

    /// Returns the collapsed regions sorted by `(start_line, end_line)`.
    #[must_use]
    pub fn collapsed(&self) -> &[CollapsedRegion] {
        &self.collapsed
    }

    /// Returns `true` when no region is collapsed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.collapsed.is_empty()
    }

    /// Returns the number of visible rows for a buffer of `total_lines` lines (`>= 1` when
    /// `total_lines >= 1`). Hidden lines past the end of the buffer are ignored.
    #[must_use]
    pub fn visible_line_count(&self, total_lines: usize) -> usize {
        let idx = self.hidden.partition_point(|h| h.first < total_lines);
        let hidden = self.hidden[..idx].last().map_or(0, |h| {
            h.hidden_before + h.last.min(total_lines.saturating_sub(1)) + 1 - h.first
        });
        total_lines.saturating_sub(hidden)
    }

    /// Maps a visual row to its buffer line. Rows past the end map past the end of the
    /// buffer linearly; callers clamp them with [`Self::visible_line_count`].
    #[must_use]
    pub fn visual_to_buffer_line(&self, visual_row: usize) -> usize {
        let idx = self
            .hidden
            .partition_point(|h| h.visual_anchor() <= visual_row);
        match idx.checked_sub(1).map(|i| self.hidden[i]) {
            Some(h) => visual_row + h.hidden_before + h.len(),
            None => visual_row,
        }
    }

    /// Maps a buffer line to its visual row, or to the header of the collapsed region hiding it.
    #[must_use]
    pub fn buffer_to_visual_line(&self, buffer_line: usize) -> VisualLineMapping {
        let idx = self.hidden.partition_point(|h| h.first <= buffer_line);
        let Some(h) = idx.checked_sub(1).map(|i| self.hidden[i]) else {
            return VisualLineMapping::Visible(buffer_line);
        };
        if buffer_line <= h.last {
            let header_buffer_line = h.first.saturating_sub(1);
            VisualLineMapping::HiddenInsideFold {
                header_buffer_line,
                header_visual_row: header_buffer_line - h.hidden_before,
            }
        } else {
            VisualLineMapping::Visible(buffer_line - h.hidden_before - h.len())
        }
    }

    /// Returns the visual row of `buffer_line`, or of its fold header when hidden.
    #[must_use]
    pub fn visual_row_of(&self, buffer_line: usize) -> usize {
        match self.buffer_to_visual_line(buffer_line) {
            VisualLineMapping::Visible(row) => row,
            VisualLineMapping::HiddenInsideFold {
                header_visual_row, ..
            } => header_visual_row,
        }
    }

    /// Returns `true` when `buffer_line` is hidden inside a collapsed region.
    #[must_use]
    pub fn is_hidden(&self, buffer_line: usize) -> bool {
        matches!(
            self.buffer_to_visual_line(buffer_line),
            VisualLineMapping::HiddenInsideFold { .. }
        )
    }

    /// Returns the outermost collapsed region whose header is `line` and that is itself visible
    /// (used to paint the `... (N lines)` pill and the `›` chevron).
    #[must_use]
    pub fn collapsed_at(&self, line: usize) -> Option<CollapsedRegion> {
        if self.is_hidden(line) {
            return None;
        }
        self.collapsed
            .iter()
            .filter(|r| r.start_line == line)
            .max_by_key(|r| r.end_line)
            .copied()
    }

    /// Returns `true` when a region with header `line` is collapsed.
    #[must_use]
    pub fn is_collapsed_at(&self, line: usize) -> bool {
        self.collapsed.iter().any(|r| r.start_line == line)
    }

    /// Collapses `region` (ignored when single-line). Returns `true` when the map changed.
    pub fn collapse(&mut self, region: CollapsedRegion) -> bool {
        if region.end_line <= region.start_line || self.collapsed.contains(&region) {
            return false;
        }
        let pos = self
            .collapsed
            .partition_point(|r| (r.start_line, r.end_line) < (region.start_line, region.end_line));
        self.collapsed.insert(pos, region);
        self.rebuild();
        true
    }

    /// Expands every collapsed region with header `line`. Returns `true` when the map changed.
    pub fn expand_at(&mut self, line: usize) -> bool {
        let before = self.collapsed.len();
        self.collapsed.retain(|r| r.start_line != line);
        let changed = self.collapsed.len() != before;
        if changed {
            self.rebuild();
        }
        changed
    }

    /// Toggles the fold at `line`: a candidate starting at `line` wins, otherwise the innermost
    /// candidate containing `line`. Expands it when collapsed, collapses it otherwise.
    /// Returns `true` when the map changed.
    pub fn toggle_fold_at(&mut self, line: usize, candidates: &[FoldCandidate]) -> bool {
        let Some(target) = Self::target_candidate(line, candidates) else {
            return false;
        };
        if self.is_collapsed_at(target.start_line) {
            self.expand_at(target.start_line)
        } else {
            self.collapse(target)
        }
    }

    /// Collapses the fold at `line` (see [`Self::toggle_fold_at`] for target selection).
    pub fn fold_at(&mut self, line: usize, candidates: &[FoldCandidate]) -> bool {
        Self::target_candidate(line, candidates).is_some_and(|target| self.collapse(target))
    }

    /// Expands the fold at `line`, or every collapsed region containing it.
    pub fn unfold_at(&mut self, line: usize) -> bool {
        let before = self.collapsed.len();
        self.collapsed.retain(|r| !r.contains(line));
        let changed = self.collapsed.len() != before;
        if changed {
            self.rebuild();
        }
        changed
    }

    /// Collapses every multi-line candidate (`zM` / *Collapse All*).
    pub fn fold_all(&mut self, candidates: &[FoldCandidate]) -> bool {
        let mut next: Vec<CollapsedRegion> = candidates
            .iter()
            .filter(|c| c.end_line > c.start_line)
            .copied()
            .collect();
        next.sort_unstable_by_key(|r| (r.start_line, r.end_line));
        next.dedup();
        if next == self.collapsed {
            return false;
        }
        self.collapsed = next;
        self.rebuild();
        true
    }

    /// Expands everything (`zR` / *Expand All*). Returns `true` when the map changed.
    pub fn unfold_all(&mut self) -> bool {
        if self.collapsed.is_empty() {
            return false;
        }
        self.collapsed.clear();
        self.hidden.clear();
        true
    }

    /// Expands every collapsed region hiding `buffer_line` so a jump (*Go to Line*, *Structure*,
    /// *Search Everywhere*, *Problems*, caret motion) never lands on an invisible line.
    /// Returns `true` when the map changed.
    pub fn reveal_line(&mut self, buffer_line: usize) -> bool {
        if !self.is_hidden(buffer_line) {
            return false;
        }
        self.collapsed
            .retain(|r| !(buffer_line > r.start_line && buffer_line <= r.end_line));
        self.rebuild();
        true
    }

    /// Shifts collapsed regions after a buffer edit that inserted (`delta > 0`) or removed
    /// lines below `edit_line`, so folds stay anchored until fresh candidates arrive.
    pub fn shift_after_edit(&mut self, edit_line: usize, delta: isize) -> bool {
        if delta == 0 || self.collapsed.is_empty() {
            return false;
        }
        let shift = |line: usize| line.saturating_add_signed(delta).max(edit_line);
        let mut changed = false;
        for region in &mut self.collapsed {
            if region.start_line > edit_line {
                region.start_line = shift(region.start_line);
                region.end_line = shift(region.end_line);
                changed = true;
            } else if region.end_line > edit_line {
                region.end_line = shift(region.end_line);
                changed = true;
            }
        }
        if changed {
            self.collapsed.retain(|r| r.end_line > r.start_line);
            self.collapsed
                .sort_unstable_by_key(|r| (r.start_line, r.end_line));
            self.collapsed.dedup();
            self.rebuild();
        }
        changed
    }

    /// Re-anchors collapsed regions on fresh syntax candidates: a region survives when a
    /// candidate shares its header line (taking the candidate's end line), others are dropped.
    /// Returns `true` when the map changed.
    pub fn reconcile(&mut self, candidates: &[FoldCandidate]) -> bool {
        if self.collapsed.is_empty() {
            return false;
        }
        let mut next: Vec<CollapsedRegion> = self
            .collapsed
            .iter()
            .filter_map(|r| {
                candidates
                    .iter()
                    .filter(|c| c.start_line == r.start_line && c.end_line > c.start_line)
                    .min_by_key(|c| c.end_line.abs_diff(r.end_line))
                    .copied()
            })
            .collect();
        next.sort_unstable_by_key(|r| (r.start_line, r.end_line));
        next.dedup();
        if next == self.collapsed {
            return false;
        }
        self.collapsed = next;
        self.rebuild();
        true
    }

    fn target_candidate(line: usize, candidates: &[FoldCandidate]) -> Option<FoldCandidate> {
        let multi_line = || candidates.iter().filter(|c| c.end_line > c.start_line);
        multi_line()
            .filter(|c| c.start_line == line)
            .max_by_key(|c| c.span())
            .or_else(|| {
                multi_line()
                    .filter(|c| c.contains(line))
                    .min_by_key(|c| c.span())
            })
            .copied()
    }

    /// Rebuilds the merged hidden intervals and their prefix sums.
    fn rebuild(&mut self) {
        self.hidden.clear();
        let mut hidden_before = 0;
        for region in &self.collapsed {
            let first = region.start_line + 1;
            let last = region.end_line;
            match self.hidden.last_mut() {
                Some(prev) if first <= prev.last + 1 => {
                    if last > prev.last {
                        hidden_before += last - prev.last;
                        prev.last = last;
                    }
                }
                _ => {
                    self.hidden.push(HiddenInterval {
                        first,
                        last,
                        hidden_before,
                    });
                    hidden_before += last - first + 1;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    fn c(start: usize, end: usize) -> FoldCandidate {
        FoldCandidate::new(start, end)
    }

    #[test]
    fn empty_map_is_identity() {
        let map = FoldMap::new();
        assert_eq!(map.visible_line_count(10), 10);
        for line in 0..10 {
            assert_eq!(map.visual_to_buffer_line(line), line);
            assert_eq!(
                map.buffer_to_visual_line(line),
                VisualLineMapping::Visible(line)
            );
        }
        assert_eq!(map.visible_line_count(0), 0);
    }

    #[test]
    fn single_fold_hides_body_and_keeps_header() {
        let mut map = FoldMap::new();
        assert!(map.toggle_fold_at(2, &[c(2, 5)]));
        assert_eq!(map.visible_line_count(10), 7);
        assert_eq!(map.visual_to_buffer_line(2), 2);
        assert_eq!(map.visual_to_buffer_line(3), 6);
        assert_eq!(map.visual_to_buffer_line(6), 9);
        assert_eq!(map.buffer_to_visual_line(6), VisualLineMapping::Visible(3));
        assert_eq!(
            map.buffer_to_visual_line(4),
            VisualLineMapping::HiddenInsideFold {
                header_buffer_line: 2,
                header_visual_row: 2
            }
        );
        assert_eq!(map.collapsed_at(2), Some(c(2, 5)));
        assert!(map.toggle_fold_at(2, &[c(2, 5)]));
        assert!(map.is_empty());
    }

    #[test]
    fn nested_folds_do_not_double_count_hidden_lines() {
        // Class 0..=20 with method 3..=8 inside.
        let candidates = [c(0, 20), c(3, 8)];
        let mut map = FoldMap::new();
        assert!(map.toggle_fold_at(3, &candidates));
        assert_eq!(map.visible_line_count(30), 25);
        assert!(map.toggle_fold_at(0, &candidates));
        assert_eq!(map.visible_line_count(30), 10, "only 1..=20 hidden once");
        assert_eq!(map.visual_to_buffer_line(1), 21);
        assert_eq!(map.collapsed_at(3), None, "nested header is hidden");
        assert_eq!(map.collapsed_at(0), Some(c(0, 20)));
        // Expanding the class restores the still-collapsed method.
        assert!(map.toggle_fold_at(0, &candidates));
        assert_eq!(map.visible_line_count(30), 25);
        assert_eq!(map.visual_to_buffer_line(4), 9);
    }

    #[test]
    fn overlapping_and_adjacent_regions_merge() {
        let mut map = FoldMap::new();
        map.collapse(c(2, 6));
        map.collapse(c(5, 9)); // header 5 is hidden by the first fold, end extends to 9
        map.collapse(c(9, 12)); // header 9 hidden as well
        assert_eq!(map.visible_line_count(20), 20 - 10);
        assert_eq!(map.visual_to_buffer_line(3), 13);
        assert_eq!(map.buffer_to_visual_line(13), VisualLineMapping::Visible(3));
        // Adjacent: 14..=15 hidden right after 13..
        map.collapse(c(13, 15));
        assert_eq!(map.visible_line_count(20), 20 - 12);
        assert_eq!(map.visual_to_buffer_line(4), 16);
    }

    #[test]
    fn single_line_candidates_are_rejected() {
        let mut map = FoldMap::new();
        assert!(!map.toggle_fold_at(3, &[c(3, 3)]));
        assert!(!map.collapse(c(5, 4)));
        assert!(!map.toggle_fold_at(7, &[]));
        assert!(map.is_empty());
    }

    #[test]
    fn toggle_inside_body_targets_innermost_candidate() {
        let candidates = [c(0, 20), c(3, 8), c(4, 6)];
        let mut map = FoldMap::new();
        assert!(map.toggle_fold_at(5, &candidates));
        assert_eq!(map.collapsed(), &[c(4, 6)]);
    }

    #[test]
    fn reveal_line_expands_every_enclosing_fold() {
        let candidates = [c(0, 20), c(3, 8), c(30, 40)];
        let mut map = FoldMap::new();
        map.fold_all(&candidates);
        assert_eq!(map.collapsed().len(), 3);
        assert!(map.reveal_line(5));
        assert_eq!(map.collapsed(), &[c(30, 40)]);
        assert!(!map.reveal_line(5));
        assert!(!map.reveal_line(30), "a header line is already visible");
        assert!(map.reveal_line(35));
        assert!(map.is_empty());
    }

    #[test]
    fn fold_all_and_unfold_all() {
        let candidates = [c(0, 20), c(3, 8), c(1, 1)];
        let mut map = FoldMap::new();
        assert!(map.fold_all(&candidates));
        assert!(!map.fold_all(&candidates));
        assert_eq!(map.visible_line_count(21), 1);
        assert!(map.unfold_all());
        assert!(!map.unfold_all());
        assert_eq!(map.visible_line_count(21), 21);
    }

    #[test]
    fn regions_past_buffer_end_are_tolerated() {
        let mut map = FoldMap::new();
        map.collapse(c(5, 100));
        assert_eq!(map.visible_line_count(10), 6);
        assert_eq!(map.visible_line_count(3), 3);
        map.collapse(c(200, 300));
        assert_eq!(map.visible_line_count(10), 6);
        assert_eq!(map.visual_to_buffer_line(5), 5);
    }

    #[test]
    fn shift_and_reconcile_keep_folds_anchored() {
        let mut map = FoldMap::new();
        map.collapse(c(10, 20));
        // Two lines inserted at line 2.
        assert!(map.shift_after_edit(2, 2));
        assert_eq!(map.collapsed(), &[c(12, 22)]);
        // Lines removed inside the fold body are unreachable (caret reveals first); removing
        // above shifts back.
        assert!(map.shift_after_edit(2, -2));
        assert_eq!(map.collapsed(), &[c(10, 20)]);
        // Fresh candidates: the method grew by one line, another fold disappeared.
        map.collapse(c(30, 35));
        assert!(map.reconcile(&[c(10, 21), c(40, 45)]));
        assert_eq!(map.collapsed(), &[c(10, 21)]);
        assert!(!map.reconcile(&[c(10, 21)]));
    }

    #[test]
    fn round_trips_on_large_file_within_budget() {
        // 100_000 lines with 5_000 collapsed 10-line blocks.
        let mut map = FoldMap::new();
        let candidates: Vec<_> = (0..5_000).map(|i| c(i * 20, i * 20 + 10)).collect();
        assert!(map.fold_all(&candidates));
        let total = 100_000;
        let visible = map.visible_line_count(total);
        assert_eq!(visible, total - 5_000 * 10);

        let started = Instant::now();
        for row in 0..visible {
            let line = map.visual_to_buffer_line(row);
            assert!(line < total);
            assert_eq!(
                map.buffer_to_visual_line(line),
                VisualLineMapping::Visible(row)
            );
        }
        let per_lookup = started.elapsed() / u32::try_from(visible).unwrap();
        assert!(
            per_lookup.as_micros() < 50,
            "visual/buffer conversion must stay in the microsecond range ({per_lookup:?})"
        );
        assert!(map.reveal_line(5));
        assert_eq!(map.visible_line_count(total), total - 4_999 * 10);
    }
}
