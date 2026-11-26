//! Project switching and native menu commands of [`WorkspaceShell`] (`KOVA-049`).
//!
//! `File → Open Project…` and `File → Recent Projects` end in [`WorkspaceShell::switch_project`];
//! the view commands of the menu bar (tool windows, Focus Mode, Close Tab, Settings,
//! Search Everywhere) reuse the `JetBrains` keymap handlers through [`ShellAction`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui::{Context, Window};

use super::{PaneState, WorkspaceShell};
use crate::app_menu::{
    CloseTab, OpenSettings, SearchEverywhere, ToggleFocusMode, ToggleGit, ToggleProjectTree,
    ToggleTerminal,
};
use crate::dock::{
    BottomPanelModel, DockToolId, FocusModeVariant, FocusedRegion, ProjectTreeModel, SplitTree,
    StructureModel,
};
use crate::document::StatusKind;
use crate::keymap::ShellAction;
use crate::terminal::TerminalShortcut;

/// Why [`WorkspaceShell::switch_project`] kept the current project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchProjectError {
    /// The target is not a directory.
    NotADirectory(PathBuf),
    /// These open documents have unsaved changes.
    UnsavedChanges(Vec<PathBuf>),
}

impl WorkspaceShell {
    /// Makes `root` the project of this window: closes every tab, re-roots the tree, Git,
    /// terminal, test runner and run configurations, and rescans the tree off-thread.
    ///
    /// Refused (with a status-bar warning) while documents have unsaved changes, so a switch
    /// never discards edits.
    ///
    /// # Errors
    ///
    /// Returns [`SwitchProjectError`] when the switch was refused; nothing changed.
    pub fn switch_project(
        &mut self,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), SwitchProjectError> {
        if !root.is_dir() {
            self.set_notice(
                StatusKind::Error,
                format!("Cannot open project: {} is not a directory", root.display()),
                cx,
            );
            return Err(SwitchProjectError::NotADirectory(root));
        }
        let dirty = self.dirty_document_keys();
        if !dirty.is_empty() {
            let names = dirty
                .iter()
                .map(|key| key.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            self.set_notice(
                StatusKind::Warning,
                format!("Save or close modified files before switching project: {names}"),
                cx,
            );
            return Err(SwitchProjectError::UnsavedChanges(dirty));
        }

        self.project.switch_to(root);
        self.reset_project_state(cx);
        self.load_project(cx);
        self.clear_notice(cx);
        self.focus_active_editor(window, cx);
        window.focus(&self.focus_handle);
        cx.notify();
        Ok(())
    }

    /// Keys of open documents with unsaved changes, sorted.
    fn dirty_document_keys(&self) -> Vec<PathBuf> {
        let mut dirty: Vec<PathBuf> = self
            .doc_states
            .keys()
            .filter(|key| self.is_document_dirty(key))
            .cloned()
            .collect();
        dirty.sort();
        dirty
    }

    /// Drops every piece of state that belongs to the previous project.
    fn reset_project_state(&mut self, cx: &mut Context<'_, Self>) {
        let root_pane = self.split_tree.first_pane();
        self.split_tree = SplitTree::new(root_pane);
        let mut panes = HashMap::new();
        panes.insert(root_pane, PaneState::new(root_pane));
        self.panes = panes;
        self.active_pane = root_pane;
        self.dock_layout.focus(FocusedRegion::EditorPane(root_pane));

        self.documents.clear();
        self.doc_states.clear();
        self.read_only_docs.clear();
        self.nav_history.clear();
        self.pending_definition_request = None;
        self.queued_go_to_definition_byte = None;
        self.doc_notice = None;
        self.close_after_save = None;
        self.ex_line = None;
        self.analysis_task = None;
        self.project_task = None;

        self.project_tree = ProjectTreeModel::default();
        self.project_tree.set_filter("");
        self.structure = StructureModel::default();
        self.bottom_panel = BottomPanelModel::default();
        self.test_runner.set_project_files(Vec::new());
        self.run_configs.clear();
        self.run_configs_error = None;
        self.http_panel = None;

        // The PTY runs in the old root: drop it, the next `Alt+F12` starts one in the new root.
        self.terminal = None;
        if self.dock_layout.bottom_open == Some(DockToolId::Terminal) {
            self.dock_layout
                .toggle_tool(DockToolId::Terminal, root_pane);
        }

        self.commit_panel_open = false;
        self.commit_panel = None;
        self.search_everywhere.close();
        self.git_discover_task = None;
        self.status_bar.git_branch.clear();
        self.set_git_repo(None, None, cx);
    }

    /// Whether a modal surface owns the keyboard, so a menu shortcut must fall through to
    /// the key capture that serves it (settings, commit panel, evaluate, Ex line, Search Everywhere).
    fn modal_owns_keys(&self) -> bool {
        self.settings_open
            || self.commit_panel_open
            || self.search_everywhere.is_open()
            || self.ex_line.is_some()
            || self.debugger.evaluate_modal().is_some()
    }

    /// Runs a menu command through the keymap handler unless a modal surface owns the keys.
    fn run_menu_action(
        &mut self,
        action: ShellAction,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.modal_owns_keys() {
            cx.propagate();
            return;
        }
        self.run_shell_action(action, window, cx);
        cx.notify();
    }

    pub(super) fn close_tab_action(
        &mut self,
        _: &CloseTab,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // With the terminal focused, `Cmd+W` closes the terminal tab, as before the menu.
        if !self.modal_owns_keys()
            && let Some(dock) = self.terminal_focused(window, cx)
        {
            self.run_terminal_shortcut(TerminalShortcut::CloseTab, &dock, window, cx);
            return;
        }
        self.run_menu_action(ShellAction::CloseActiveTab, window, cx);
    }

    pub(super) fn open_settings_action(
        &mut self,
        _: &OpenSettings,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(ShellAction::OpenSettings, window, cx);
    }

    pub(super) fn search_everywhere_action(
        &mut self,
        _: &SearchEverywhere,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(ShellAction::OpenSearchEverywhere, window, cx);
    }

    pub(super) fn toggle_project_tree_action(
        &mut self,
        _: &ToggleProjectTree,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(ShellAction::ToggleTool(DockToolId::Project), window, cx);
    }

    pub(super) fn toggle_terminal_action(
        &mut self,
        _: &ToggleTerminal,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(ShellAction::ToggleTool(DockToolId::Terminal), window, cx);
    }

    pub(super) fn toggle_git_action(
        &mut self,
        _: &ToggleGit,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(ShellAction::ToggleTool(DockToolId::Commit), window, cx);
    }

    pub(super) fn toggle_focus_mode_action(
        &mut self,
        _: &ToggleFocusMode,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(
            ShellAction::ToggleFocusMode(FocusModeVariant::Zen),
            window,
            cx,
        );
    }
}

/// Project-relative document key of `path` (absolute paths outside `root` stay absolute).
#[must_use]
pub(super) fn document_key(root: Option<&Path>, path: &Path) -> PathBuf {
    root.and_then(|root| path.strip_prefix(root).ok())
        .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::document_key;

    #[test]
    fn document_key_is_relative_inside_the_project() {
        let root = Path::new("/work/orders");
        assert_eq!(
            document_key(Some(root), Path::new("/work/orders/src/App.java")),
            Path::new("src/App.java")
        );
        assert_eq!(
            document_key(Some(root), Path::new("/tmp/notes.md")),
            Path::new("/tmp/notes.md")
        );
        assert_eq!(
            document_key(None, Path::new("/tmp/notes.md")),
            Path::new("/tmp/notes.md")
        );
    }
}
