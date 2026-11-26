//! `kova_ui` — GPUI presentation components: `IntelliJ` New UI dock, Focus/Tiling layout, editor and settings.
//!
//! GPU presentation layer (GPUI) — one of the only two crates allowed to depend
//! on windowing (`kova_ui` and `kova_app`).

pub mod app_menu;
pub mod controls;
pub mod debugger;
pub mod dock;
pub mod document;
pub mod editor;
pub mod git;
pub mod http_runner;
pub mod keymap;
pub mod plugin_ui;
pub mod search;
pub mod settings;
pub mod shell;
pub mod terminal;
pub mod test_runner;
pub mod theme;

pub use debugger::{
    BreakpointIcon, BreakpointSpec, DebugCommand, DebugCommandSink, DebugLaunchRequest, DebugStep,
    DebugUpdate, DebuggerModel, DebuggerStatus, EvaluateTarget, FrameItem, ThreadItem,
    VariableItem,
};
pub use dock::{
    BadgeVariant, BottomPanelModel, BottomTab, DockLayoutState, DockSide, DockToolId,
    FocusedRegion, PaneId, PaneRect, ProblemEntry, ProblemRow, ProblemSeverity, ProjectNode,
    ProjectNodeKind, ProjectTreeModel, SplitAxis, SplitTree, StatusBarState, StructureModel,
    StructureSymbolItem, TabGroupModel, TabId, TabItem, TreeActivation, VimModeBadge,
};
pub use document::{DocumentState, SaveRequest, SaveSink, SaveStatus, SaveTicket, StatusKind};
pub use editor::shared::SharedBuffer;
pub use editor::{EditorEvent, EditorView};
pub use git::{
    CheckState, CommitPanel, CommitPanelEvent, CommitPanelModel, EditorGitState, StageOp,
};
pub use plugin_ui::{PluginToolWindow, PluginUiEventSink};
pub use shell::{StartupMetrics, SwitchProjectError, WorkspaceShell};
pub use terminal::{TerminalDock, TerminalLaunch, TerminalView};
pub use test_runner::{TestRunRequest, TestRunSink, TestRunnerAction, TestRunnerModel};
pub use theme::{KovaTheme, ThemeState, apply_live_theme, hsla_from_rgba, rgba_to_gpui};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_ui");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
