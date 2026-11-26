//! Dock layout state, tool window identifiers, and focus tracking.

use crate::dock::split::PaneId;

/// Minimum width of the left dock in logical pixels.
pub const LEFT_DOCK_MIN_WIDTH_PX: f32 = 160.0;
/// Maximum width of the left dock in logical pixels.
pub const LEFT_DOCK_MAX_WIDTH_PX: f32 = 520.0;
/// Default width of the left dock in logical pixels (matches `UiSettings::sidebar_width`).
pub const LEFT_DOCK_DEFAULT_WIDTH_PX: f32 = 260.0;

/// Minimum width of the right dock (`Structure`) in logical pixels.
pub const RIGHT_DOCK_MIN_WIDTH_PX: f32 = 220.0;
/// Maximum width of the right dock (`Structure`) in logical pixels.
pub const RIGHT_DOCK_MAX_WIDTH_PX: f32 = 360.0;
/// Default width of the right dock (`Structure`) in logical pixels.
pub const RIGHT_DOCK_DEFAULT_WIDTH_PX: f32 = 260.0;

/// Minimum height of the bottom panel in logical pixels.
pub const BOTTOM_PANEL_MIN_HEIGHT_PX: f32 = 100.0;
/// Maximum height of the bottom panel in logical pixels.
pub const BOTTOM_PANEL_MAX_HEIGHT_PX: f32 = 520.0;
/// Default height of the bottom panel in logical pixels (matches `UiSettings::bottom_panel_height`).
pub const BOTTOM_PANEL_DEFAULT_HEIGHT_PX: f32 = 240.0;

/// Identifier of a dockable tool window in the `IntelliJ` New UI Islands shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DockToolId {
    /// Project file tree (`Alt+1`).
    Project,
    /// Git commit / changes tool window (`Alt+0`).
    Commit,
    /// Run output tool window (`Alt+4`).
    Run,
    /// Diagnostics / problems tool window (`Alt+6`).
    Problems,
    /// Document symbol structure outline (`Alt+7`).
    Structure,
    /// Integrated terminal tool window (`Alt+F12`).
    Terminal,
    /// `JUnit` Test Runner tool window (`Alt+8`, `KOVA-040`).
    Tests,
    /// JVM Debugger tool window (`Alt+5`, `KOVA-041`).
    Debug,
}

impl DockToolId {
    /// All dockable tool window identifiers in canonical order.
    pub const ALL: [Self; 8] = [
        Self::Project,
        Self::Commit,
        Self::Run,
        Self::Debug,
        Self::Problems,
        Self::Tests,
        Self::Terminal,
        Self::Structure,
    ];

    /// Returns the human-readable title of this tool window.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Project => "Project",
            Self::Commit => "Commit",
            Self::Run => "Run",
            Self::Problems => "Problems",
            Self::Structure => "Structure",
            Self::Terminal => "Terminal",
            Self::Tests => "Tests",
            Self::Debug => "Debug",
        }
    }

    /// Returns the short badge label for the tool window stripe.
    #[must_use]
    pub const fn stripe_label(self) -> &'static str {
        match self {
            Self::Project => "PRJ",
            Self::Commit => "GIT",
            Self::Run => "RUN",
            Self::Problems => "PRB",
            Self::Structure => "STR",
            Self::Terminal => "TRM",
            Self::Tests => "TST",
            Self::Debug => "DBG",
        }
    }

    /// Returns the default keyboard shortcut label for this tool window.
    #[must_use]
    pub const fn shortcut(self) -> &'static str {
        match self {
            Self::Project => "Alt+1",
            Self::Commit => "Alt+0",
            Self::Run => "Alt+4",
            Self::Problems => "Alt+6",
            Self::Structure => "Alt+7",
            Self::Terminal => "Alt+F12",
            Self::Tests => "Alt+8",
            Self::Debug => "Alt+5",
        }
    }

    /// Returns the platform-native keyboard shortcut label matching the active preset keymap.
    #[must_use]
    pub const fn display_shortcut(self) -> &'static str {
        #[cfg(target_os = "macos")]
        {
            match self {
                Self::Project => "⌘1",
                Self::Commit => "⌘9",
                Self::Run => "⌘4",
                Self::Debug => "⌘5",
                Self::Problems => "⌘6",
                Self::Tests => "⌥8",
                Self::Terminal => "⌥F12",
                Self::Structure => "⌘7",
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            match self {
                Self::Project => "Alt+1",
                Self::Commit => "Alt+9",
                Self::Run => "Alt+4",
                Self::Debug => "Alt+5",
                Self::Problems => "Alt+6",
                Self::Tests => "Alt+8",
                Self::Terminal => "Alt+F12",
                Self::Structure => "Alt+7",
            }
        }
    }

    /// Returns the tooltip text combining the tool window title and platform shortcut
    /// (for example, `"Project (⌘1)"` on `macOS` or `"Project (Alt+1)"` on `Linux`).
    #[must_use]
    pub fn tooltip_label(self) -> String {
        format!("{} ({})", self.title(), self.display_shortcut())
    }

    /// Returns the dock slot this tool window is anchored to in the Islands layout.
    #[must_use]
    pub const fn side(self) -> DockSide {
        match self {
            Self::Project | Self::Commit => DockSide::Left,
            Self::Structure => DockSide::Right,
            Self::Run | Self::Problems | Self::Terminal | Self::Tests | Self::Debug => {
                DockSide::Bottom
            }
        }
    }
}

/// Top tool-window group rendered on the left stripe rail.
pub const LEFT_STRIPE_TOP_TOOLS: [DockToolId; 4] = [
    DockToolId::Project,
    DockToolId::Commit,
    DockToolId::Run,
    DockToolId::Debug,
];

/// Bottom tool-window group rendered on the left stripe rail.
pub const LEFT_STRIPE_BOTTOM_TOOLS: [DockToolId; 3] = [
    DockToolId::Problems,
    DockToolId::Tests,
    DockToolId::Terminal,
];

/// Top tool-window group rendered on the right stripe rail.
pub const RIGHT_STRIPE_TOP_TOOLS: [DockToolId; 1] = [DockToolId::Structure];

/// Bottom tool-window group rendered on the right stripe rail.
pub const RIGHT_STRIPE_BOTTOM_TOOLS: [DockToolId; 0] = [];

/// Dock slot a tool window is anchored to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DockSide {
    /// Left dock (next to the 32 px left stripe).
    Left,
    /// Right dock (next to the 30 px right stripe).
    Right,
    /// Bottom panel below the editor area.
    Bottom,
}

/// Identifies which island currently holds keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FocusedRegion {
    /// Left Project dock.
    ProjectDock,
    /// Right Structure dock.
    StructureDock,
    /// Bottom panel (Problems / Run / Terminal).
    BottomPanel,
    /// An editor split pane.
    EditorPane(PaneId),
}

impl FocusedRegion {
    /// Returns a human-readable label for the status bar focus indicator.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ProjectDock => "Focus: Project",
            Self::StructureDock => "Focus: Structure",
            Self::BottomPanel => "Focus: Bottom",
            Self::EditorPane(_) => "Focus: Editor",
        }
    }
}

/// Persistent headless state of the dockable Islands workspace.
#[derive(Debug, Clone, PartialEq)]
pub struct DockLayoutState {
    /// Currently open tool in the left dock, if any.
    pub left_open: Option<DockToolId>,
    /// Currently open tool in the right dock, if any.
    pub right_open: Option<DockToolId>,
    /// Currently open tool in the bottom panel, if any.
    pub bottom_open: Option<DockToolId>,
    /// Width of the left dock in logical pixels.
    pub left_width: f32,
    /// Width of the right dock in logical pixels.
    pub right_width: f32,
    /// Height of the bottom panel in logical pixels.
    pub bottom_height: f32,
    /// Currently focused island.
    pub focused: FocusedRegion,
}

impl Default for DockLayoutState {
    fn default() -> Self {
        Self {
            left_open: Some(DockToolId::Project),
            right_open: None,
            bottom_open: None,
            left_width: LEFT_DOCK_DEFAULT_WIDTH_PX,
            right_width: RIGHT_DOCK_DEFAULT_WIDTH_PX,
            bottom_height: BOTTOM_PANEL_DEFAULT_HEIGHT_PX,
            focused: FocusedRegion::EditorPane(PaneId(1)),
        }
    }
}

impl DockLayoutState {
    /// Creates a [`DockLayoutState`] initialized from `UiSettings` dimensions,
    /// clamping to safe UI dock bounds without rejecting valid config values.
    #[must_use]
    pub fn from_ui_settings(sidebar_width: f32, bottom_panel_height: f32) -> Self {
        let mut state = Self::default();
        state.set_left_width(sidebar_width);
        state.set_bottom_height(bottom_panel_height);
        state
    }

    /// Toggles `tool` in the dock slot it belongs to ([`DockToolId::side`]).
    ///
    /// Opening a tool focuses its dock; closing the focused dock returns focus to `editor_pane`.
    pub fn toggle_tool(&mut self, tool: DockToolId, editor_pane: PaneId) {
        let was_open = self.is_tool_active(tool);
        match tool.side() {
            DockSide::Left => self.toggle_left(tool),
            DockSide::Right => self.toggle_right(tool),
            DockSide::Bottom => self.toggle_bottom(tool),
        }
        if was_open && self.focused == Self::region_for_side(tool.side()) {
            self.focused = FocusedRegion::EditorPane(editor_pane);
        }
    }

    /// Returns the focus region that hosts tools anchored to `side`.
    #[must_use]
    pub const fn region_for_side(side: DockSide) -> FocusedRegion {
        match side {
            DockSide::Left => FocusedRegion::ProjectDock,
            DockSide::Right => FocusedRegion::StructureDock,
            DockSide::Bottom => FocusedRegion::BottomPanel,
        }
    }

    /// Toggles a tool in the left dock. Clicking the active tool closes it.
    pub fn toggle_left(&mut self, tool: DockToolId) {
        if self.left_open == Some(tool) {
            self.left_open = None;
        } else {
            self.left_open = Some(tool);
            self.focused = FocusedRegion::ProjectDock;
        }
    }

    /// Toggles a tool in the right dock. Clicking the active tool closes it.
    pub fn toggle_right(&mut self, tool: DockToolId) {
        if self.right_open == Some(tool) {
            self.right_open = None;
        } else {
            self.right_open = Some(tool);
            self.focused = FocusedRegion::StructureDock;
        }
    }

    /// Toggles a tool in the bottom panel. Clicking the active tool closes it.
    pub fn toggle_bottom(&mut self, tool: DockToolId) {
        if self.bottom_open == Some(tool) {
            self.bottom_open = None;
        } else {
            self.bottom_open = Some(tool);
            self.focused = FocusedRegion::BottomPanel;
        }
    }

    /// Sets the left dock width, clamped to `[LEFT_DOCK_MIN_WIDTH_PX, LEFT_DOCK_MAX_WIDTH_PX]`.
    pub fn set_left_width(&mut self, width: f32) {
        if width.is_finite() {
            self.left_width = width.clamp(LEFT_DOCK_MIN_WIDTH_PX, LEFT_DOCK_MAX_WIDTH_PX);
        }
    }

    /// Sets the right dock width, clamped to `[RIGHT_DOCK_MIN_WIDTH_PX, RIGHT_DOCK_MAX_WIDTH_PX]`.
    pub fn set_right_width(&mut self, width: f32) {
        if width.is_finite() {
            self.right_width = width.clamp(RIGHT_DOCK_MIN_WIDTH_PX, RIGHT_DOCK_MAX_WIDTH_PX);
        }
    }

    /// Sets the bottom panel height, clamped to `[BOTTOM_PANEL_MIN_HEIGHT_PX, BOTTOM_PANEL_MAX_HEIGHT_PX]`.
    pub fn set_bottom_height(&mut self, height: f32) {
        if height.is_finite() {
            self.bottom_height =
                height.clamp(BOTTOM_PANEL_MIN_HEIGHT_PX, BOTTOM_PANEL_MAX_HEIGHT_PX);
        }
    }

    /// Moves keyboard focus to the given region.
    pub fn focus(&mut self, region: FocusedRegion) {
        self.focused = region;
    }

    /// Returns `true` if `tool` is currently open in any dock slot.
    #[must_use]
    pub fn is_tool_active(&self, tool: DockToolId) -> bool {
        self.left_open == Some(tool)
            || self.right_open == Some(tool)
            || self.bottom_open == Some(tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_opens_project_dock_and_focuses_editor() {
        let state = DockLayoutState::default();
        assert_eq!(state.left_open, Some(DockToolId::Project));
        assert_eq!(state.right_open, None);
        assert_eq!(state.bottom_open, None);
        assert_eq!(state.focused, FocusedRegion::EditorPane(PaneId(1)));
        assert!(state.is_tool_active(DockToolId::Project));
        assert!(!state.is_tool_active(DockToolId::Structure));
    }

    #[test]
    fn toggling_active_tool_closes_it_and_toggling_new_tool_opens_and_focuses() {
        let mut state = DockLayoutState::default();
        state.toggle_left(DockToolId::Project);
        assert_eq!(state.left_open, None);

        state.toggle_right(DockToolId::Structure);
        assert_eq!(state.right_open, Some(DockToolId::Structure));
        assert_eq!(state.focused, FocusedRegion::StructureDock);

        state.toggle_bottom(DockToolId::Problems);
        assert_eq!(state.bottom_open, Some(DockToolId::Problems));
        assert_eq!(state.focused, FocusedRegion::BottomPanel);

        state.toggle_bottom(DockToolId::Terminal);
        assert_eq!(state.bottom_open, Some(DockToolId::Terminal));
    }

    #[test]
    fn toggle_tool_routes_to_anchored_side_and_restores_editor_focus() {
        let mut state = DockLayoutState::default();
        state.toggle_tool(DockToolId::Structure, PaneId(1));
        assert_eq!(state.right_open, Some(DockToolId::Structure));
        assert_eq!(state.focused, FocusedRegion::StructureDock);

        state.toggle_tool(DockToolId::Problems, PaneId(1));
        assert_eq!(state.bottom_open, Some(DockToolId::Problems));
        state.toggle_tool(DockToolId::Run, PaneId(1));
        assert_eq!(state.bottom_open, Some(DockToolId::Run));

        state.toggle_tool(DockToolId::Run, PaneId(7));
        assert_eq!(state.bottom_open, None);
        assert_eq!(state.focused, FocusedRegion::EditorPane(PaneId(7)));

        state.toggle_tool(DockToolId::Commit, PaneId(1));
        assert_eq!(state.left_open, Some(DockToolId::Commit));
        assert!(!state.is_tool_active(DockToolId::Project));
        assert_eq!(DockToolId::Terminal.side(), DockSide::Bottom);
        assert_eq!(DockToolId::Tests.side(), DockSide::Bottom);
        assert_eq!(DockToolId::Structure.shortcut(), "Alt+7");
    }

    #[test]
    fn dimensions_are_clamped_and_ignore_non_finite_values() {
        let mut state = DockLayoutState::from_ui_settings(50.0, 999.0);
        assert!((state.left_width - LEFT_DOCK_MIN_WIDTH_PX).abs() < f32::EPSILON);
        assert!((state.bottom_height - BOTTOM_PANEL_MAX_HEIGHT_PX).abs() < f32::EPSILON);

        state.set_left_width(f32::NAN);
        assert!((state.left_width - LEFT_DOCK_MIN_WIDTH_PX).abs() < f32::EPSILON);

        state.set_right_width(500.0);
        assert!((state.right_width - RIGHT_DOCK_MAX_WIDTH_PX).abs() < f32::EPSILON);
    }

    #[test]
    fn display_shortcut_and_tooltip_label_cover_all_tools_on_current_platform() {
        let expected: [(DockToolId, &str, &str); 8] = if cfg!(target_os = "macos") {
            [
                (DockToolId::Project, "⌘1", "Project (⌘1)"),
                (DockToolId::Commit, "⌘9", "Commit (⌘9)"),
                (DockToolId::Run, "⌘4", "Run (⌘4)"),
                (DockToolId::Debug, "⌘5", "Debug (⌘5)"),
                (DockToolId::Problems, "⌘6", "Problems (⌘6)"),
                (DockToolId::Tests, "⌥8", "Tests (⌥8)"),
                (DockToolId::Terminal, "⌥F12", "Terminal (⌥F12)"),
                (DockToolId::Structure, "⌘7", "Structure (⌘7)"),
            ]
        } else {
            [
                (DockToolId::Project, "Alt+1", "Project (Alt+1)"),
                (DockToolId::Commit, "Alt+9", "Commit (Alt+9)"),
                (DockToolId::Run, "Alt+4", "Run (Alt+4)"),
                (DockToolId::Debug, "Alt+5", "Debug (Alt+5)"),
                (DockToolId::Problems, "Alt+6", "Problems (Alt+6)"),
                (DockToolId::Tests, "Alt+8", "Tests (Alt+8)"),
                (DockToolId::Terminal, "Alt+F12", "Terminal (Alt+F12)"),
                (DockToolId::Structure, "Alt+7", "Structure (Alt+7)"),
            ]
        };

        assert_eq!(expected.len(), DockToolId::ALL.len());
        for (tool, shortcut, tooltip) in expected {
            assert_eq!(tool.display_shortcut(), shortcut);
            assert_eq!(tool.tooltip_label(), tooltip);
        }
    }

    #[test]
    fn stripe_tool_lists_include_debug_and_cover_every_tool_once() {
        let mut seen = Vec::new();
        for group in [
            LEFT_STRIPE_TOP_TOOLS.as_slice(),
            LEFT_STRIPE_BOTTOM_TOOLS.as_slice(),
            RIGHT_STRIPE_TOP_TOOLS.as_slice(),
            RIGHT_STRIPE_BOTTOM_TOOLS.as_slice(),
        ] {
            for &tool in group {
                assert!(
                    !seen.contains(&tool),
                    "duplicate tool {tool:?} in stripe lists"
                );
                seen.push(tool);
            }
        }

        assert_eq!(seen.len(), DockToolId::ALL.len());
        for tool in DockToolId::ALL {
            assert!(
                seen.contains(&tool),
                "missing tool {tool:?} in stripe lists"
            );
        }
        assert!(LEFT_STRIPE_TOP_TOOLS.contains(&DockToolId::Debug));
        assert_eq!(RIGHT_STRIPE_TOP_TOOLS, [DockToolId::Structure]);
        assert_eq!(RIGHT_STRIPE_BOTTOM_TOOLS.as_slice(), []);
    }

    #[test]
    fn toggling_debug_tool_activates_bottom_panel_and_stripe_state() {
        let mut state = DockLayoutState::default();
        assert!(!state.is_tool_active(DockToolId::Debug));

        state.toggle_tool(DockToolId::Debug, PaneId(1));
        assert_eq!(state.bottom_open, Some(DockToolId::Debug));
        assert_eq!(state.focused, FocusedRegion::BottomPanel);
        assert!(state.is_tool_active(DockToolId::Debug));

        state.toggle_tool(DockToolId::Debug, PaneId(1));
        assert_eq!(state.bottom_open, None);
        assert!(!state.is_tool_active(DockToolId::Debug));
    }
}
