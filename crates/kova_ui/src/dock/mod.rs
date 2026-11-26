//! `IntelliJ` New UI Islands dockable shell models and state.
//!
//! All state models in this module are pure headless Rust structures that can
//! be unit-tested without a GPUI window or GPU context.

pub mod bottom_panel;
pub mod focus;
pub mod layout;
pub mod project_icons;
pub mod project_tree;
pub mod split;
pub mod status_bar;
pub mod structure;
pub mod tabs;

pub use bottom_panel::{BottomPanelModel, BottomTab, ProblemEntry, ProblemRow, ProblemSeverity};
pub use focus::{
    Direction, DockRestoreSnapshot, FocusModeVariant, FocusTilingController, FocusToggle,
    LayoutDensity, LayoutRect, compute_region_rects, find_neighbor_pane, region_visible,
};
pub use layout::{DockLayoutState, DockSide, DockToolId, FocusedRegion};
pub use project_icons::{
    ProjectIconPalette, render_chevron, render_collapse_all_icon, render_expand_all_icon,
    render_file_icon, render_folder_icon, render_header_chevron, render_hide_icon,
    render_locate_file_icon, render_new_item_icon, render_options_icon,
};
pub use project_tree::{
    FileIconKind, ProjectNode, ProjectNodeKind, ProjectTreeModel, TreeActivation,
    file_icon_kind_for, format_tilde_path,
};
pub use split::{PaneId, PaneRect, SplitAxis, SplitTree};
pub use status_bar::{BadgeVariant, StatusBarState, VimModeBadge};
pub use structure::{StructureModel, StructureSymbolItem};
pub use tabs::{TabGroupModel, TabId, TabItem};
