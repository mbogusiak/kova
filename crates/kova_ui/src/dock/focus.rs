//! Omarchy keyboard-first *Focus / Tiling* layout control (`KOVA-026`).
//!
//! Everything here is headless: [`FocusTilingController`] owns the Focus Mode snapshot and the
//! layout density, [`compute_region_rects`] derives the on-screen rectangle of every visible
//! island from the same geometry constants the shell renders with, and [`find_neighbor_pane`]
//! picks the island to focus for a directional shortcut.

use crate::dock::layout::{DockLayoutState, DockToolId, FocusedRegion};
use crate::dock::split::{PaneId, PaneRect, SplitTree};

/// Axis-aligned island rectangle in logical window pixels.
pub type LayoutRect = PaneRect;

/// Height of the integrated title bar in logical pixels.
pub const TITLEBAR_HEIGHT_PX: f32 = 36.0;
/// Height of the status bar island in logical pixels.
pub const STATUSBAR_HEIGHT_PX: f32 = 28.0;
/// Width of the left tool window stripe in logical pixels.
pub const LEFT_STRIPE_WIDTH_PX: f32 = 32.0;
/// Width of the right tool window stripe in logical pixels.
pub const RIGHT_STRIPE_WIDTH_PX: f32 = 30.0;

/// Tolerance used when comparing island edges (sub-pixel rounding of flex layout).
const EDGE_EPSILON_PX: f32 = 0.5;

/// Direction of a keyboard focus move, tab move or split resize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Towards smaller `x`.
    Left,
    /// Towards larger `x`.
    Right,
    /// Towards smaller `y`.
    Up,
    /// Towards larger `y`.
    Down,
}

impl Direction {
    /// All four directions in `Left, Right, Up, Down` order.
    pub const ALL: [Self; 4] = [Self::Left, Self::Right, Self::Up, Self::Down];

    /// Returns `true` for [`Self::Left`] and [`Self::Right`].
    #[must_use]
    pub const fn is_horizontal(self) -> bool {
        matches!(self, Self::Left | Self::Right)
    }

    /// Returns `true` for [`Self::Left`] and [`Self::Up`] (towards the origin).
    #[must_use]
    pub const fn is_backward(self) -> bool {
        matches!(self, Self::Left | Self::Up)
    }
}

/// Island spacing preset of the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LayoutDensity {
    /// Standard `IntelliJ` New UI Islands (10 px outer margin, 8 px island gap).
    #[default]
    Islands,
    /// Dense Omarchy/Hyprland Tiling (8 px outer margin, 6 px gap, strict focus ring).
    Tiling,
}

impl LayoutDensity {
    /// Outer margin between the window edge and the islands.
    #[must_use]
    pub const fn outer_margin_px(self) -> f32 {
        match self {
            Self::Islands => 10.0,
            Self::Tiling => 8.0,
        }
    }

    /// Gap between neighbouring islands (also the splitter hit width).
    #[must_use]
    pub const fn island_gap_px(self) -> f32 {
        match self {
            Self::Islands => 8.0,
            Self::Tiling => 6.0,
        }
    }

    /// Border width of the focused island (`borders.focus`); other islands keep `1 px`.
    #[must_use]
    pub const fn focus_border_px(self) -> f32 {
        match self {
            Self::Islands => 1.2,
            Self::Tiling => 1.5,
        }
    }

    /// Returns the other density.
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Islands => Self::Tiling,
            Self::Tiling => Self::Islands,
        }
    }

    /// Short status bar label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Islands => "Islands",
            Self::Tiling => "Tiling",
        }
    }
}

/// Which shortcut entered Focus Mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusModeVariant {
    /// `Cmd+Shift+F12` / `Ctrl+Shift+F12` (`tool_window.hide_all`): hide every dock.
    HideDocks,
    /// `Ctrl+Alt+Z` / `:only` (`view.toggle_focus_mode`): hide every dock and the stripes.
    Zen,
}

/// Dock state saved when Focus Mode starts and restored exactly when it ends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DockRestoreSnapshot {
    /// Tool open in the left dock.
    pub left_open: Option<DockToolId>,
    /// Tool open in the right dock.
    pub right_open: Option<DockToolId>,
    /// Tool open in the bottom panel.
    pub bottom_open: Option<DockToolId>,
    /// Left dock width.
    pub left_width: f32,
    /// Right dock width.
    pub right_width: f32,
    /// Bottom panel height.
    pub bottom_height: f32,
    /// Island focused before Focus Mode started.
    pub focused: FocusedRegion,
}

impl DockRestoreSnapshot {
    /// Captures the current dock state.
    #[must_use]
    pub const fn capture(layout: &DockLayoutState) -> Self {
        Self {
            left_open: layout.left_open,
            right_open: layout.right_open,
            bottom_open: layout.bottom_open,
            left_width: layout.left_width,
            right_width: layout.right_width,
            bottom_height: layout.bottom_height,
            focused: layout.focused,
        }
    }
}

/// Result of a Focus Mode toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusToggle {
    /// Docks were hidden; focus is on the given editor pane.
    Entered(PaneId),
    /// Docks were restored; focus returned to the given region.
    Restored(FocusedRegion),
    /// Focus Mode stays on, only its variant changed (e.g. hide-all → Zen).
    VariantChanged(FocusModeVariant),
}

/// Headless state machine for Focus Mode (dock hiding with exact restore) and layout density.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FocusTilingController {
    density: LayoutDensity,
    focus: Option<(FocusModeVariant, DockRestoreSnapshot)>,
}

impl FocusTilingController {
    /// Creates a controller in the Islands density with Focus Mode off.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            density: LayoutDensity::Islands,
            focus: None,
        }
    }

    /// Creates a controller honouring `UiSettings::focus_mode` as the initial Tiling switch.
    #[must_use]
    pub const fn with_density(density: LayoutDensity) -> Self {
        Self {
            density,
            focus: None,
        }
    }

    /// Current island spacing preset.
    #[must_use]
    pub const fn density(&self) -> LayoutDensity {
        self.density
    }

    /// Flips between [`LayoutDensity::Islands`] and [`LayoutDensity::Tiling`].
    pub const fn toggle_density(&mut self) -> LayoutDensity {
        self.density = self.density.toggled();
        self.density
    }

    /// Whether Focus Mode currently hides the docks.
    #[must_use]
    pub const fn is_focus_mode(&self) -> bool {
        self.focus.is_some()
    }

    /// Active Focus Mode variant, if any.
    #[must_use]
    pub fn focus_variant(&self) -> Option<FocusModeVariant> {
        self.focus.map(|(variant, _)| variant)
    }

    /// Whether the tool window stripes are hidden (Zen variant).
    #[must_use]
    pub fn hides_stripes(&self) -> bool {
        self.focus_variant() == Some(FocusModeVariant::Zen)
    }

    /// Saved dock snapshot while Focus Mode is on.
    #[must_use]
    pub fn snapshot(&self) -> Option<&DockRestoreSnapshot> {
        self.focus.as_ref().map(|(_, snapshot)| snapshot)
    }

    /// Toggles Focus Mode with `variant`.
    ///
    /// Entering saves a [`DockRestoreSnapshot`], closes every dock and focuses `active_pane`.
    /// Pressing the same shortcut again restores the docks with their exact sizes and returns
    /// focus to the island that had it (or to `active_pane` when that island is gone). Pressing
    /// the other Focus Mode shortcut while active only switches the variant. Only the open/closed
    /// flags change, so editor buffers, scroll offsets, cursors and undo history are untouched.
    pub fn toggle_focus_mode(
        &mut self,
        variant: FocusModeVariant,
        layout: &mut DockLayoutState,
        active_pane: PaneId,
        pane_exists: impl Fn(PaneId) -> bool,
    ) -> FocusToggle {
        match self.focus {
            Some((current, snapshot)) if current != variant => {
                self.focus = Some((variant, snapshot));
                FocusToggle::VariantChanged(variant)
            }
            Some((_, snapshot)) => {
                self.focus = None;
                layout.left_open = snapshot.left_open;
                layout.right_open = snapshot.right_open;
                layout.bottom_open = snapshot.bottom_open;
                layout.set_left_width(snapshot.left_width);
                layout.set_right_width(snapshot.right_width);
                layout.set_bottom_height(snapshot.bottom_height);
                let focus = match snapshot.focused {
                    FocusedRegion::EditorPane(pane) if pane_exists(pane) => snapshot.focused,
                    FocusedRegion::EditorPane(_) => FocusedRegion::EditorPane(active_pane),
                    region if region_visible(layout, region) => region,
                    _ => FocusedRegion::EditorPane(active_pane),
                };
                layout.focus(focus);
                FocusToggle::Restored(focus)
            }
            None => {
                self.focus = Some((variant, DockRestoreSnapshot::capture(layout)));
                layout.left_open = None;
                layout.right_open = None;
                layout.bottom_open = None;
                layout.focus(FocusedRegion::EditorPane(active_pane));
                FocusToggle::Entered(active_pane)
            }
        }
    }

    /// Leaves Focus Mode without restoring the snapshot, so an explicitly requested tool window
    /// (e.g. `Alt+1` while focused) opens deterministically on its own. Returns `true` when
    /// Focus Mode was active.
    pub const fn exit_for_explicit_tool(&mut self) -> bool {
        let was_active = self.focus.is_some();
        self.focus = None;
        was_active
    }
}

/// Returns `true` when the dock hosting `region` is open (editor panes always count as visible).
#[must_use]
pub const fn region_visible(layout: &DockLayoutState, region: FocusedRegion) -> bool {
    match region {
        FocusedRegion::ProjectDock => layout.left_open.is_some(),
        FocusedRegion::StructureDock => layout.right_open.is_some(),
        FocusedRegion::BottomPanel => layout.bottom_open.is_some(),
        FocusedRegion::EditorPane(_) => true,
    }
}

/// Computes the rectangle of every visible island for a window of `viewport_width × viewport_height`.
///
/// Mirrors the shell's flex layout: title bar on top, status bar at the bottom, optional
/// stripes, left dock, editor split area with the bottom panel underneath, right dock. The
/// spacing comes from `density`, so directional navigation always works on the geometry that
/// is actually painted.
#[must_use]
pub fn compute_region_rects(
    viewport_width: f32,
    viewport_height: f32,
    layout: &DockLayoutState,
    split_tree: &SplitTree,
    density: LayoutDensity,
    stripes_visible: bool,
) -> Vec<(FocusedRegion, LayoutRect)> {
    let margin = density.outer_margin_px();
    let gap = density.island_gap_px();
    let top = TITLEBAR_HEIGHT_PX;
    let bottom = (viewport_height - margin - STATUSBAR_HEIGHT_PX - gap).max(top);
    let height = bottom - top;

    let mut left = margin;
    let mut right = (viewport_width - margin).max(left);
    if stripes_visible {
        left += LEFT_STRIPE_WIDTH_PX + gap;
        right -= RIGHT_STRIPE_WIDTH_PX + gap;
    }

    let mut out = Vec::with_capacity(split_tree.pane_count() + 3);
    if layout.left_open.is_some() {
        let width = layout.left_width.min((right - left).max(0.0));
        out.push((
            FocusedRegion::ProjectDock,
            LayoutRect::new(left, top, width, height),
        ));
        left += width + gap;
    }
    if layout.right_open.is_some() {
        let width = layout.right_width.min((right - left).max(0.0));
        out.push((
            FocusedRegion::StructureDock,
            LayoutRect::new(right - width, top, width, height),
        ));
        right -= width + gap;
    }

    let center_width = (right - left).max(0.0);
    let mut editor_height = height;
    if layout.bottom_open.is_some() {
        let panel = layout.bottom_height.min(height);
        out.push((
            FocusedRegion::BottomPanel,
            LayoutRect::new(left, bottom - panel, center_width, panel),
        ));
        editor_height = (height - panel - gap).max(0.0);
    }

    let editor_area = LayoutRect::new(left, top, center_width, editor_height);
    out.extend(
        split_tree
            .compute_pane_bounds(editor_area, gap)
            .into_iter()
            .map(|(pane, rect)| (FocusedRegion::EditorPane(pane), rect)),
    );
    out
}

/// Length of the overlap of `[a0, a1)` and `[b0, b1)` (`0.0` when disjoint).
fn overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    (a1.min(b1) - a0.max(b0)).max(0.0)
}

/// Picks the island to focus when moving from `current` in direction `dir`.
///
/// Only islands lying entirely on the `dir` side of `current` that overlap it on the
/// perpendicular axis are candidates (no wrap-around at the window edges). Among them the
/// nearest one along the movement axis wins; ties are broken by the largest perpendicular
/// overlap and then by the closest perpendicular centre. Distance is ranked before overlap so
/// that a half-height split right next to the focused island beats a taller island further
/// away (Hyprland behaviour).
#[must_use]
pub fn find_neighbor_pane(
    rects: &[(FocusedRegion, LayoutRect)],
    current: FocusedRegion,
    dir: Direction,
) -> Option<FocusedRegion> {
    let (_, cur) = rects.iter().find(|(region, _)| *region == current)?;
    let cur_center = |r: &LayoutRect| {
        if dir.is_horizontal() {
            r.y + r.height / 2.0
        } else {
            r.x + r.width / 2.0
        }
    };
    let reference = cur_center(cur);

    rects
        .iter()
        .filter(|(region, _)| *region != current)
        .filter_map(|(region, r)| {
            let (distance, shared) = match dir {
                Direction::Right => (
                    r.x - (cur.x + cur.width),
                    overlap(cur.y, cur.y + cur.height, r.y, r.y + r.height),
                ),
                Direction::Left => (
                    cur.x - (r.x + r.width),
                    overlap(cur.y, cur.y + cur.height, r.y, r.y + r.height),
                ),
                Direction::Down => (
                    r.y - (cur.y + cur.height),
                    overlap(cur.x, cur.x + cur.width, r.x, r.x + r.width),
                ),
                Direction::Up => (
                    cur.y - (r.y + r.height),
                    overlap(cur.x, cur.x + cur.width, r.x, r.x + r.width),
                ),
            };
            (distance >= -EDGE_EPSILON_PX && shared > EDGE_EPSILON_PX).then(|| {
                (
                    *region,
                    distance.max(0.0),
                    shared,
                    (cur_center(r) - reference).abs(),
                )
            })
        })
        .min_by(|a, b| {
            a.1.total_cmp(&b.1)
                .then_with(|| b.2.total_cmp(&a.2))
                .then_with(|| a.3.total_cmp(&b.3))
        })
        .map(|(region, ..)| region)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dock::split::SplitAxis;

    /// Project | editor 1 | editor 2 | Structure, Problems under the editors.
    fn full_layout() -> (DockLayoutState, SplitTree) {
        let layout = DockLayoutState {
            right_open: Some(DockToolId::Structure),
            bottom_open: Some(DockToolId::Problems),
            ..DockLayoutState::default()
        };
        let mut tree = SplitTree::new(PaneId(1));
        assert!(tree.split_pane(PaneId(1), SplitAxis::Horizontal, PaneId(2)));
        (layout, tree)
    }

    fn rects_for(
        layout: &DockLayoutState,
        tree: &SplitTree,
        density: LayoutDensity,
    ) -> Vec<(FocusedRegion, LayoutRect)> {
        compute_region_rects(1600.0, 1000.0, layout, tree, density, true)
    }

    #[test]
    fn density_switches_margins_gaps_and_focus_ring() {
        assert!((LayoutDensity::Islands.outer_margin_px() - 10.0).abs() < f32::EPSILON);
        assert!((LayoutDensity::Islands.island_gap_px() - 8.0).abs() < f32::EPSILON);
        assert!((LayoutDensity::Tiling.outer_margin_px() - 8.0).abs() < f32::EPSILON);
        assert!((LayoutDensity::Tiling.island_gap_px() - 6.0).abs() < f32::EPSILON);
        assert!((LayoutDensity::Tiling.focus_border_px() - 1.5).abs() < f32::EPSILON);

        let mut controller = FocusTilingController::new();
        assert_eq!(controller.density(), LayoutDensity::Islands);
        assert_eq!(controller.toggle_density(), LayoutDensity::Tiling);
        assert_eq!(controller.density().label(), "Tiling");
        assert_eq!(controller.toggle_density(), LayoutDensity::Islands);
    }

    #[test]
    fn tiling_layout_is_denser_than_islands() {
        let (layout, tree) = full_layout();
        let islands = rects_for(&layout, &tree, LayoutDensity::Islands);
        let tiling = rects_for(&layout, &tree, LayoutDensity::Tiling);
        let width_of = |rects: &[(FocusedRegion, LayoutRect)], region| {
            rects
                .iter()
                .find(|(r, _)| *r == region)
                .map(|(_, rect)| rect.width)
                .unwrap()
        };
        let editor = FocusedRegion::EditorPane(PaneId(1));
        assert!(width_of(&tiling, editor) > width_of(&islands, editor));

        let p1 = tiling.iter().find(|(r, _)| *r == editor).unwrap().1;
        let p2 = tiling
            .iter()
            .find(|(r, _)| *r == FocusedRegion::EditorPane(PaneId(2)))
            .unwrap()
            .1;
        assert!(
            (p2.x - (p1.x + p1.width) - 6.0).abs() < 0.01,
            "6 px tiling gap"
        );
    }

    #[test]
    fn focus_mode_hides_docks_and_restores_exact_dimensions() {
        let (mut layout, _) = full_layout();
        layout.set_left_width(333.0);
        layout.set_bottom_height(222.0);
        layout.focus(FocusedRegion::StructureDock);
        let before = layout.clone();

        let mut controller = FocusTilingController::new();
        let entered = controller.toggle_focus_mode(
            FocusModeVariant::HideDocks,
            &mut layout,
            PaneId(2),
            |_| true,
        );
        assert_eq!(entered, FocusToggle::Entered(PaneId(2)));
        assert!(controller.is_focus_mode());
        assert!(!controller.hides_stripes());
        assert_eq!(
            (layout.left_open, layout.right_open, layout.bottom_open),
            (None, None, None)
        );
        assert_eq!(layout.focused, FocusedRegion::EditorPane(PaneId(2)));

        let restored = controller.toggle_focus_mode(
            FocusModeVariant::HideDocks,
            &mut layout,
            PaneId(2),
            |_| true,
        );
        assert_eq!(
            restored,
            FocusToggle::Restored(FocusedRegion::StructureDock)
        );
        assert_eq!(layout, before);
        assert!(!controller.is_focus_mode());
    }

    #[test]
    fn focus_mode_variants_and_explicit_tool_exit() {
        let (mut layout, _) = full_layout();
        let mut controller = FocusTilingController::new();
        controller.toggle_focus_mode(FocusModeVariant::Zen, &mut layout, PaneId(1), |_| true);
        assert!(controller.hides_stripes());

        // The other shortcut keeps docks hidden and only switches the variant.
        let changed = controller.toggle_focus_mode(
            FocusModeVariant::HideDocks,
            &mut layout,
            PaneId(1),
            |_| true,
        );
        assert_eq!(
            changed,
            FocusToggle::VariantChanged(FocusModeVariant::HideDocks)
        );
        assert_eq!(layout.right_open, None);
        assert!(controller.snapshot().is_some());

        // `Alt+1` while focused leaves Focus Mode and opens only the requested dock.
        assert!(controller.exit_for_explicit_tool());
        assert!(!controller.exit_for_explicit_tool());
        layout.toggle_tool(DockToolId::Project, PaneId(1));
        assert_eq!(layout.left_open, Some(DockToolId::Project));
        assert_eq!(layout.right_open, None);
        assert_eq!(layout.focused, FocusedRegion::ProjectDock);
    }

    #[test]
    fn restore_falls_back_to_active_pane_when_saved_pane_was_closed() {
        let mut layout = DockLayoutState::default();
        layout.focus(FocusedRegion::EditorPane(PaneId(9)));
        let mut controller = FocusTilingController::new();
        controller.toggle_focus_mode(FocusModeVariant::Zen, &mut layout, PaneId(9), |_| true);
        let restored =
            controller.toggle_focus_mode(FocusModeVariant::Zen, &mut layout, PaneId(3), |p| {
                p != PaneId(9)
            });
        assert_eq!(
            restored,
            FocusToggle::Restored(FocusedRegion::EditorPane(PaneId(3)))
        );
    }

    #[test]
    fn neighbor_navigation_covers_all_islands_without_wrapping() {
        let (layout, tree) = full_layout();
        let rects = rects_for(&layout, &tree, LayoutDensity::Tiling);
        let e1 = FocusedRegion::EditorPane(PaneId(1));
        let e2 = FocusedRegion::EditorPane(PaneId(2));
        let nav = |from, dir| find_neighbor_pane(&rects, from, dir);

        assert_eq!(nav(FocusedRegion::ProjectDock, Direction::Right), Some(e1));
        assert_eq!(nav(e1, Direction::Right), Some(e2));
        assert_eq!(
            nav(e2, Direction::Right),
            Some(FocusedRegion::StructureDock)
        );
        assert_eq!(nav(FocusedRegion::StructureDock, Direction::Left), Some(e2));
        assert_eq!(nav(e2, Direction::Left), Some(e1));
        assert_eq!(nav(e1, Direction::Left), Some(FocusedRegion::ProjectDock));
        assert_eq!(nav(e1, Direction::Down), Some(FocusedRegion::BottomPanel));
        assert_eq!(nav(e2, Direction::Down), Some(FocusedRegion::BottomPanel));
        assert_eq!(nav(FocusedRegion::BottomPanel, Direction::Up), Some(e1));
        assert_eq!(
            nav(FocusedRegion::BottomPanel, Direction::Left),
            Some(FocusedRegion::ProjectDock)
        );
        assert_eq!(
            nav(FocusedRegion::BottomPanel, Direction::Right),
            Some(FocusedRegion::StructureDock)
        );

        // Window edges: no wrap-around.
        assert_eq!(nav(FocusedRegion::ProjectDock, Direction::Left), None);
        assert_eq!(nav(FocusedRegion::StructureDock, Direction::Right), None);
        assert_eq!(nav(e1, Direction::Up), None);
        assert_eq!(nav(FocusedRegion::BottomPanel, Direction::Down), None);
        assert_eq!(nav(FocusedRegion::ProjectDock, Direction::Up), None);
        // Unknown current region.
        assert_eq!(
            nav(FocusedRegion::EditorPane(PaneId(42)), Direction::Up),
            None
        );
    }

    #[test]
    fn nearest_island_beats_taller_far_island() {
        let layout = DockLayoutState {
            left_open: None,
            ..DockLayoutState::default()
        };
        // [ 1 | 2 / 3 | 4 ]: from 1, the half-height pane 2 is closer than full-height 4.
        let mut tree = SplitTree::new(PaneId(1));
        tree.split_pane(PaneId(1), SplitAxis::Horizontal, PaneId(2));
        tree.split_pane(PaneId(2), SplitAxis::Horizontal, PaneId(4));
        tree.split_pane(PaneId(2), SplitAxis::Vertical, PaneId(3));
        let rects = compute_region_rects(
            1600.0,
            1000.0,
            &layout,
            &tree,
            LayoutDensity::Islands,
            false,
        );
        let pane = |n| FocusedRegion::EditorPane(PaneId(n));
        assert_eq!(
            find_neighbor_pane(&rects, pane(1), Direction::Right),
            Some(pane(2))
        );
        assert_eq!(
            find_neighbor_pane(&rects, pane(2), Direction::Down),
            Some(pane(3))
        );
        assert_eq!(
            find_neighbor_pane(&rects, pane(3), Direction::Up),
            Some(pane(2))
        );
        assert_eq!(
            find_neighbor_pane(&rects, pane(3), Direction::Right),
            Some(pane(4))
        );
        assert_eq!(
            find_neighbor_pane(&rects, pane(4), Direction::Left),
            Some(pane(2))
        );
    }

    #[test]
    fn hidden_docks_are_not_navigation_targets() {
        let (mut layout, tree) = full_layout();
        let mut controller = FocusTilingController::new();
        controller.toggle_focus_mode(FocusModeVariant::Zen, &mut layout, PaneId(1), |_| true);
        let rects =
            compute_region_rects(1600.0, 1000.0, &layout, &tree, LayoutDensity::Tiling, false);
        assert_eq!(rects.len(), 2);
        let e1 = FocusedRegion::EditorPane(PaneId(1));
        assert_eq!(find_neighbor_pane(&rects, e1, Direction::Left), None);
        assert_eq!(find_neighbor_pane(&rects, e1, Direction::Down), None);
        assert!(region_visible(&layout, e1));
        assert!(!region_visible(&layout, FocusedRegion::ProjectDock));
        assert!(Direction::Left.is_backward() && !Direction::Up.is_horizontal());
    }
}
