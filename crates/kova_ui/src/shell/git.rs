//! Git integration of [`WorkspaceShell`] (`KOVA-043`): `gix` repository discovery,
//! status-bar branch display, per-editor gutter diff / inline blame attachment, the left
//! *Commit* dock and the `Cmd+K` Commit modal overlay.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    AppContext as _, ClickEvent, Context, Div, Entity, FontWeight, InteractiveElement as _,
    MouseButton, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Window, div, px,
};
use kova_theme::PluginVariant;
use kova_tools::git::{ChangeKind, GitRepository, HeadState};

use super::{Palette, WorkspaceShell};
use crate::controls::{Button, ControlPalette};
use crate::dock::DockToolId;
use crate::document::StatusKind;
use crate::editor::EditorView;
use crate::git::{CheckState, CommitPanel, CommitPanelEvent, ToggleCommitPanel};
use crate::theme::KovaTheme;

impl WorkspaceShell {
    /// Returns the discovered `gix` repository for the workspace, if any.
    #[must_use]
    pub const fn git_repository(&self) -> Option<&Arc<GitRepository>> {
        self.git_repo.as_ref()
    }

    /// Whether the `Cmd+K` Commit modal overlay is open.
    #[must_use]
    pub const fn is_commit_panel_open(&self) -> bool {
        self.commit_panel_open
    }

    /// Formats a [`HeadState`] for the status bar and titlebar branch pill.
    pub(super) fn format_head_state(head: &HeadState) -> String {
        match head {
            HeadState::Branch(name) | HeadState::Unborn(name) => name.clone(),
            HeadState::Detached(sha) => format!("{sha} (detached)"),
        }
    }

    /// Installs the discovered Git repository and attaches it to all open editors and the
    /// Commit panel.
    pub(super) fn set_git_repo(
        &mut self,
        repo: Option<Arc<GitRepository>>,
        branch: Option<String>,
        cx: &mut Context<'_, Self>,
    ) {
        self.git_repo.clone_from(&repo);
        if let Some(branch) = branch {
            self.status_bar.git_branch = branch;
        }
        if let Some((panel, _)) = &self.commit_panel {
            panel.update(cx, |panel, cx| panel.attach_repo(repo, cx));
        }
        self.attach_open_editors_git(cx);
        cx.notify();
    }

    /// Lazily discovers a Git repository when a single file is opened without a project root.
    pub(super) fn discover_git_for_path(&mut self, path: &Path, cx: &mut Context<'_, Self>) {
        if self.git_repo.is_some() || self.git_discover_task.is_some() {
            return;
        }
        let target = path.parent().unwrap_or(path).to_path_buf();
        let discover = cx.background_executor().spawn(async move {
            let repo = GitRepository::discover(&target).ok().map(Arc::new);
            let branch = repo
                .as_ref()
                .and_then(|r| r.head().ok())
                .map(|h| Self::format_head_state(&h));
            (repo, branch)
        });
        self.git_discover_task = Some(cx.spawn(async move |this, cx| {
            let (repo, branch) = discover.await;
            let _ignored = this.update(cx, |shell, cx| {
                shell.git_discover_task = None;
                if shell.git_repo.is_none() && repo.is_some() {
                    shell.set_git_repo(repo, branch, cx);
                }
            });
        }));
    }

    /// Resolves the disk path of a document key (`doc_states` path or `project_root/key`).
    fn disk_path_for_key(&self, key: &Path) -> Option<PathBuf> {
        if let Some(path) = self.doc_states.get(key).and_then(|d| d.path.as_ref()) {
            return Some(path.clone());
        }
        self.project.root().map(|root| root.join(key))
    }

    /// Attaches Git gutter diff & inline blame to `editor` if `key` resides in `self.git_repo`.
    pub(super) fn attach_editor_git(
        &self,
        key: &Path,
        editor: &Entity<EditorView>,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(repo) = self.git_repo.as_ref() else {
            return;
        };
        let Some(disk_path) = self.disk_path_for_key(key) else {
            return;
        };
        let Ok(rel_path) = repo.relative_path(&disk_path) else {
            return;
        };
        let repo = Arc::clone(repo);
        editor.update(cx, |editor, cx| editor.attach_git(repo, rel_path, cx));
    }

    fn attach_open_editors_git(&self, cx: &mut Context<'_, Self>) {
        for pane in self.panes.values() {
            for tab in pane.tabs.tabs() {
                if let (Some(key), Some(editor)) = (tab.path.as_ref(), pane.editors.get(&tab.id)) {
                    self.attach_editor_git(key, editor, cx);
                }
            }
        }
    }

    fn refresh_open_editors_git(&self, cx: &mut Context<'_, Self>) {
        for pane in self.panes.values() {
            for editor in pane.editors.values() {
                editor.update(cx, EditorView::refresh_git_base);
            }
        }
    }

    /// Returns the Commit panel entity, creating it lazily and attaching `self.git_repo`.
    fn ensure_commit_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Entity<CommitPanel> {
        if let Some((panel, _)) = &self.commit_panel {
            return panel.clone();
        }
        let panel = cx.new(CommitPanel::new);
        let repo = self.git_repo.clone();
        panel.update(cx, |panel, cx| panel.attach_repo(repo, cx));
        let subscription = cx.subscribe_in(
            &panel,
            window,
            |this, _panel, event: &CommitPanelEvent, window, cx| {
                this.handle_commit_panel_event(event, window, cx);
            },
        );
        self.commit_panel = Some((panel.clone(), subscription));
        panel
    }

    fn handle_commit_panel_event(
        &mut self,
        event: &CommitPanelEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            CommitPanelEvent::CloseRequested => {
                self.close_commit_panel(window, cx);
            }
            CommitPanelEvent::Committed(summary) => {
                self.close_commit_panel(window, cx);
                self.refresh_open_editors_git(cx);
                self.set_notice(
                    StatusKind::Info,
                    format!("Committed {}: {}", summary.short_id(), summary.summary),
                    cx,
                );
            }
            CommitPanelEvent::StatusUpdated { branch, .. } => {
                if !branch.is_empty() && self.status_bar.git_branch != *branch {
                    self.status_bar.git_branch.clone_from(branch);
                    cx.notify();
                }
            }
            CommitPanelEvent::OpenFile(rel_path) => {
                self.close_commit_panel(window, cx);
                self.open_path(Path::new(rel_path), None, window, cx);
            }
        }
    }

    /// GPUI action handler for `ToggleCommitPanel` (`Cmd+K` / `Ctrl+K`).
    pub(super) fn toggle_commit_panel_action(
        &mut self,
        _: &ToggleCommitPanel,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.toggle_commit_panel(window, cx);
    }

    /// Toggles the `Cmd+K` Commit modal overlay.
    pub fn toggle_commit_panel(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.commit_panel_open {
            self.close_commit_panel(window, cx);
        } else {
            self.open_commit_panel(window, cx);
        }
    }

    /// Opens the `Cmd+K` Commit modal overlay and refreshes Git status in the background.
    pub fn open_commit_panel(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let panel = self.ensure_commit_panel(window, cx);
        self.close_completion_popup(cx);
        self.commit_panel_open = true;
        self.ex_line = None;
        self.keymap.reset();
        self.chord_hint = None;
        panel.update(cx, |panel, cx| {
            panel.refresh(cx);
            panel.focus_message(window, cx);
        });
        cx.notify();
    }

    /// Closes the `Cmd+K` Commit modal overlay and restores focus to the active editor.
    pub fn close_commit_panel(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if !self.commit_panel_open {
            return;
        }
        self.commit_panel_open = false;
        self.focus_active_editor(window, cx);
        cx.notify();
    }

    /// Refreshes the Commit panel when the *Commit* left dock (`Alt+0`) is opened.
    pub(super) fn refresh_commit_dock(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.dock_layout.left_open == Some(DockToolId::Commit) {
            let panel = self.ensure_commit_panel(window, cx);
            panel.update(cx, CommitPanel::refresh);
        }
    }

    /// Refreshes the Commit panel after a file save when it is visible.
    pub(super) fn on_git_file_saved(&mut self, cx: &mut Context<'_, Self>) {
        if (self.commit_panel_open || self.dock_layout.left_open == Some(DockToolId::Commit))
            && let Some((panel, _)) = &self.commit_panel
        {
            panel.update(cx, CommitPanel::refresh);
        }
    }

    /// Intercepts capture-phase keys while the Commit modal is open.
    ///
    /// Returns `true` when `handle_key_capture` should stop here (either consuming `Cmd+K`
    /// to toggle closed, or letting the focused [`CommitPanel`] receive the key unhindered).
    pub(super) fn handle_commit_panel_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !self.commit_panel_open {
            return false;
        }
        let mods = &keystroke.modifiers;
        let primary = if cfg!(target_os = "macos") {
            mods.platform && !mods.control && !mods.alt
        } else {
            mods.control && !mods.platform && !mods.alt
        };
        if primary && keystroke.key == "k" && !mods.shift {
            self.close_commit_panel(window, cx);
            cx.stop_propagation();
            return true;
        }
        // Let the focused CommitPanel receive all other keys (message typing, Tab, Esc, Cmd+Enter).
        true
    }

    /// Renders the `Cmd+K` Commit modal overlay when open.
    pub(super) fn render_commit_overlay(&self) -> Option<Div> {
        if !self.commit_panel_open {
            return None;
        }
        let (panel, _) = self.commit_panel.as_ref()?;
        Some(div().absolute().inset_0().child(panel.clone()))
    }

    /// Renders the left *Commit* dock (`Alt+0`) with live `gix` status and a `Commit… (⌘K)`
    /// button.
    pub(super) fn render_commit_dock(
        &self,
        island: Div,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Div {
        let theme = cx.global::<KovaTheme>().clone();
        let cp = ControlPalette::from_theme(&theme);
        let entries = self
            .commit_panel
            .as_ref()
            .map(|(panel, _)| panel.read(cx).model().entries().to_vec())
            .unwrap_or_default();
        let detail = if self.git_repo.is_some() {
            format!("{} changed", entries.len())
        } else {
            "No repo".to_owned()
        };

        let mut list = div()
            .flex_1()
            .w_full()
            .overflow_hidden()
            .flex()
            .flex_col()
            .p(px(6.0))
            .gap(px(2.0));

        if self.git_repo.is_none() {
            list = list.child(
                div()
                    .p(px(8.0))
                    .text_size(px(12.0))
                    .text_color(p.text_muted)
                    .child("Open a Git repository to view changes."),
            );
        } else if entries.is_empty() {
            list = list.child(
                div()
                    .p(px(8.0))
                    .text_size(px(12.0))
                    .text_color(p.text_muted)
                    .child("Working tree clean."),
            );
        } else {
            for (idx, entry) in entries.iter().enumerate() {
                list = list.child(Self::render_commit_dock_row(idx, entry, &theme, &cp, p, cx));
            }
        }

        let open_label = if cfg!(target_os = "macos") {
            "Commit… (⌘K)"
        } else {
            "Commit… (Ctrl+K)"
        };

        island
            .child(Self::dock_header("Commit", detail, p))
            .child(list)
            .child(
                div()
                    .p(px(8.0))
                    .border_t_1()
                    .border_color(p.border_subtle)
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        Button::ghost("Refresh", PluginVariant::Muted)
                            .render("dock-git-refresh", &cp)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                let panel = this.ensure_commit_panel(window, cx);
                                panel.update(cx, CommitPanel::refresh);
                            })),
                    )
                    .child(
                        Button::new(open_label, PluginVariant::Primary)
                            .render("dock-git-commit-open", &cp)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_commit_panel(window, cx);
                            })),
                    ),
            )
    }

    fn render_commit_dock_row(
        idx: usize,
        entry: &kova_tools::git::FileStatus,
        theme: &KovaTheme,
        cp: &ControlPalette,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> gpui::Stateful<Div> {
        let check = CheckState::from_status(entry);
        let kind = entry.display_kind();
        let kind_color = match kind {
            ChangeKind::Added | ChangeKind::Untracked => {
                crate::theme::hsla_from_rgba(theme.gutter().git_added)
            }
            ChangeKind::Modified | ChangeKind::Renamed => {
                crate::theme::hsla_from_rgba(theme.gutter().git_modified)
            }
            ChangeKind::Deleted | ChangeKind::Conflicted => {
                crate::theme::hsla_from_rgba(theme.gutter().git_deleted)
            }
        };
        let rel = entry.path.clone();
        let (box_bg, box_border) = match check {
            CheckState::Checked | CheckState::Partial => (cp.primary.background, cp.primary.border),
            CheckState::Unchecked => (cp.input, cp.border),
        };
        div()
            .id(SharedString::from(format!("dock-git-row-{idx}")))
            .h(px(24.0))
            .w_full()
            .px(px(6.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(4.0))
            .hover(|s| s.bg(p.hover))
            .cursor_pointer()
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.open_path(Path::new(&rel), None, window, cx);
            }))
            .child(
                div()
                    .id(SharedString::from(format!("dock-git-chk-{idx}")))
                    .size(px(14.0))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(box_border)
                    .bg(box_bg)
                    .text_size(px(9.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(cp.primary.foreground)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let panel = this.ensure_commit_panel(window, cx);
                        panel.update(cx, |panel, cx| panel.toggle_row(idx, cx));
                    }))
                    .child(check.glyph()),
            )
            .child(
                div()
                    .w(px(10.0))
                    .flex_none()
                    .text_size(px(10.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(kind_color)
                    .child(kind.code().to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(12.0))
                    .text_color(p.text_primary)
                    .child(entry.path.clone()),
            )
    }
}
