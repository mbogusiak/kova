//! Recursive binary split tree for editor panes (`SplitTree`).

/// Minimum split ratio allowed for either child.
pub const SPLIT_MIN_RATIO: f32 = 0.15;
/// Maximum split ratio allowed for either child.
pub const SPLIT_MAX_RATIO: f32 = 0.85;
/// Default split ratio (50/50).
pub const SPLIT_DEFAULT_RATIO: f32 = 0.50;

/// Unique identifier for an editor split pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

/// Axis along which an editor pane is split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitAxis {
    /// Side-by-side split (left | right).
    Horizontal,
    /// Stacked split (top / bottom).
    Vertical,
}

/// Axis-aligned rectangle in logical pixels used for headless pane geometry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
}

impl PaneRect {
    /// Creates a new [`PaneRect`].
    #[must_use]
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Splits this rectangle along `axis` at `ratio` (already clamped), separated by `gap` pixels.
    #[must_use]
    pub fn split(self, axis: SplitAxis, ratio: f32, gap: f32) -> (Self, Self) {
        match axis {
            SplitAxis::Horizontal => {
                let usable = (self.width - gap).max(0.0);
                let first = usable * ratio;
                (
                    Self::new(self.x, self.y, first, self.height),
                    Self::new(self.x + first + gap, self.y, usable - first, self.height),
                )
            }
            SplitAxis::Vertical => {
                let usable = (self.height - gap).max(0.0);
                let first = usable * ratio;
                (
                    Self::new(self.x, self.y, self.width, first),
                    Self::new(self.x, self.y + first + gap, self.width, usable - first),
                )
            }
        }
    }
}

/// Recursive binary tree representing editor pane splits.
#[derive(Debug, Clone, PartialEq)]
pub enum SplitTree {
    /// A single leaf editor pane.
    Leaf(PaneId),
    /// A split container with two child subtrees.
    Split {
        /// Split orientation.
        axis: SplitAxis,
        /// Proportion allocated to `left` (`[0.15, 0.85]`).
        ratio: f32,
        /// Left (or top) child.
        left: Box<SplitTree>,
        /// Right (or bottom) child.
        right: Box<SplitTree>,
    },
}

impl SplitTree {
    /// Creates a new [`SplitTree`] with a single root pane.
    #[must_use]
    pub const fn new(root_pane: PaneId) -> Self {
        Self::Leaf(root_pane)
    }

    /// Splits `target` along `axis`, placing `new_pane` in the right/bottom child.
    ///
    /// Returns `true` if `target` was found and split.
    pub fn split_pane(&mut self, target: PaneId, axis: SplitAxis, new_pane: PaneId) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                let original = *id;
                *self = Self::Split {
                    axis,
                    ratio: SPLIT_DEFAULT_RATIO,
                    left: Box::new(Self::Leaf(original)),
                    right: Box::new(Self::Leaf(new_pane)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { left, right, .. } => {
                left.split_pane(target, axis, new_pane) || right.split_pane(target, axis, new_pane)
            }
        }
    }

    /// Closes `target` and collapses its parent split into the remaining sibling.
    ///
    /// Returns `None` if `target` was the only pane in the tree.
    #[must_use]
    pub fn close_pane(self, target: PaneId) -> Option<Self> {
        match self {
            Self::Leaf(id) => {
                if id == target {
                    None
                } else {
                    Some(Self::Leaf(id))
                }
            }
            Self::Split {
                axis,
                ratio,
                left,
                right,
            } => {
                if left.contains(target) {
                    match left.close_pane(target) {
                        Some(new_left) => Some(Self::Split {
                            axis,
                            ratio,
                            left: Box::new(new_left),
                            right,
                        }),
                        None => Some(*right),
                    }
                } else if right.contains(target) {
                    match right.close_pane(target) {
                        Some(new_right) => Some(Self::Split {
                            axis,
                            ratio,
                            left,
                            right: Box::new(new_right),
                        }),
                        None => Some(*left),
                    }
                } else {
                    Some(Self::Split {
                        axis,
                        ratio,
                        left,
                        right,
                    })
                }
            }
        }
    }

    /// Closes `target` in place, collapsing its parent split into the remaining sibling.
    ///
    /// Refuses to close the last remaining pane and returns `false` in that case or when
    /// `target` is not part of the tree.
    pub fn close(&mut self, target: PaneId) -> bool {
        if self.pane_count() <= 1 || !self.contains(target) {
            return false;
        }
        let placeholder = Self::Leaf(target);
        let owned = std::mem::replace(self, placeholder);
        match owned.close_pane(target) {
            Some(tree) => {
                *self = tree;
                true
            }
            None => false,
        }
    }

    /// Sets the ratio of the innermost split that directly contains `pane` as a child leaf.
    ///
    /// The ratio is clamped to `[SPLIT_MIN_RATIO, SPLIT_MAX_RATIO]`. Returns `true` on success.
    pub fn set_ratio_for(&mut self, pane: PaneId, ratio: f32) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                ratio: current,
                left,
                right,
                ..
            } => {
                let direct = matches!(**left, Self::Leaf(id) if id == pane)
                    || matches!(**right, Self::Leaf(id) if id == pane);
                if direct {
                    *current = Self::clamp_ratio(ratio);
                    true
                } else {
                    left.set_ratio_for(pane, ratio) || right.set_ratio_for(pane, ratio)
                }
            }
        }
    }

    /// Computes the on-screen rectangle of every leaf pane inside `total`, separated by `gap`.
    #[must_use]
    pub fn compute_pane_bounds(&self, total: PaneRect, gap: f32) -> Vec<(PaneId, PaneRect)> {
        let mut out = Vec::with_capacity(self.pane_count());
        self.collect_bounds(total, gap, &mut out);
        out
    }

    fn collect_bounds(&self, rect: PaneRect, gap: f32, out: &mut Vec<(PaneId, PaneRect)>) {
        match self {
            Self::Leaf(id) => out.push((*id, rect)),
            Self::Split {
                axis,
                ratio,
                left,
                right,
            } => {
                let (a, b) = rect.split(*axis, Self::clamp_ratio(*ratio), gap);
                left.collect_bounds(a, gap, out);
                right.collect_bounds(b, gap, out);
            }
        }
    }

    /// Returns the largest [`PaneId`] value present in the tree (used to allocate new ids).
    #[must_use]
    pub fn max_pane_id(&self) -> u64 {
        self.pane_ids().iter().map(|id| id.0).max().unwrap_or(0)
    }

    /// Returns `true` if `pane` exists anywhere in this tree.
    #[must_use]
    pub fn contains(&self, pane: PaneId) -> bool {
        match self {
            Self::Leaf(id) => *id == pane,
            Self::Split { left, right, .. } => left.contains(pane) || right.contains(pane),
        }
    }

    /// Returns all leaf pane IDs in left-to-right / top-to-bottom order.
    #[must_use]
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        self.collect_pane_ids(&mut ids);
        ids
    }

    fn collect_pane_ids(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Split { left, right, .. } => {
                left.collect_pane_ids(out);
                right.collect_pane_ids(out);
            }
        }
    }

    /// Returns the first (leftmost/topmost) pane ID.
    #[must_use]
    pub fn first_pane(&self) -> PaneId {
        match self {
            Self::Leaf(id) => *id,
            Self::Split { left, .. } => left.first_pane(),
        }
    }

    /// Returns the total number of leaf panes in this tree.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split { left, right, .. } => left.pane_count() + right.pane_count(),
        }
    }

    /// Clamps a split ratio to `[SPLIT_MIN_RATIO, SPLIT_MAX_RATIO]`.
    #[must_use]
    pub fn clamp_ratio(ratio: f32) -> f32 {
        if ratio.is_finite() {
            ratio.clamp(SPLIT_MIN_RATIO, SPLIT_MAX_RATIO)
        } else {
            SPLIT_DEFAULT_RATIO
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_close_preserve_remaining_panes() {
        let mut tree = SplitTree::new(PaneId(1));
        assert_eq!(tree.pane_count(), 1);

        assert!(tree.split_pane(PaneId(1), SplitAxis::Horizontal, PaneId(2)));
        assert_eq!(tree.pane_ids(), vec![PaneId(1), PaneId(2)]);

        assert!(tree.split_pane(PaneId(2), SplitAxis::Vertical, PaneId(3)));
        assert_eq!(tree.pane_ids(), vec![PaneId(1), PaneId(2), PaneId(3)]);

        let collapsed = tree.close_pane(PaneId(2));
        assert!(collapsed.is_some());
        if let Some(t) = collapsed {
            assert_eq!(t.pane_ids(), vec![PaneId(1), PaneId(3)]);
            let last = t.close_pane(PaneId(1));
            assert_eq!(last, Some(SplitTree::Leaf(PaneId(3))));
            assert_eq!(last.and_then(|leaf| leaf.close_pane(PaneId(3))), None);
        }
    }

    #[test]
    fn in_place_close_refuses_last_pane() {
        let mut tree = SplitTree::new(PaneId(1));
        assert!(!tree.close(PaneId(1)));
        assert!(tree.split_pane(PaneId(1), SplitAxis::Vertical, PaneId(2)));
        assert!(!tree.close(PaneId(9)));
        assert!(tree.close(PaneId(1)));
        assert_eq!(tree, SplitTree::Leaf(PaneId(2)));
        assert_eq!(tree.max_pane_id(), 2);
    }

    #[test]
    fn computes_nested_bounds_and_clamps_extreme_ratios() {
        let mut tree = SplitTree::new(PaneId(1));
        assert!(tree.split_pane(PaneId(1), SplitAxis::Horizontal, PaneId(2)));
        assert!(tree.split_pane(PaneId(2), SplitAxis::Vertical, PaneId(3)));

        let total = PaneRect::new(0.0, 0.0, 808.0, 408.0);
        let bounds = tree.compute_pane_bounds(total, 8.0);
        assert_eq!(bounds.len(), 3);
        assert_eq!(
            bounds[0],
            (PaneId(1), PaneRect::new(0.0, 0.0, 400.0, 408.0))
        );
        assert_eq!(
            bounds[1],
            (PaneId(2), PaneRect::new(408.0, 0.0, 400.0, 200.0))
        );
        assert_eq!(
            bounds[2],
            (PaneId(3), PaneRect::new(408.0, 208.0, 400.0, 200.0))
        );

        // Extreme ratios are clamped to [0.15, 0.85].
        assert!(tree.set_ratio_for(PaneId(1), 0.0));
        let bounds = tree.compute_pane_bounds(total, 8.0);
        assert!((bounds[0].1.width - 800.0 * SPLIT_MIN_RATIO).abs() < 1e-3);
        assert!(tree.set_ratio_for(PaneId(3), 2.0));
        let bounds = tree.compute_pane_bounds(total, 8.0);
        assert!((bounds[1].1.height - 400.0 * SPLIT_MAX_RATIO).abs() < 1e-3);
        assert!(!tree.set_ratio_for(PaneId(42), 0.5));
    }

    #[test]
    fn clamp_ratio_enforces_bounds_and_handles_nan() {
        assert!((SplitTree::clamp_ratio(0.05) - SPLIT_MIN_RATIO).abs() < f32::EPSILON);
        assert!((SplitTree::clamp_ratio(0.95) - SPLIT_MAX_RATIO).abs() < f32::EPSILON);
        assert!((SplitTree::clamp_ratio(f32::NAN) - SPLIT_DEFAULT_RATIO).abs() < f32::EPSILON);
    }
}
