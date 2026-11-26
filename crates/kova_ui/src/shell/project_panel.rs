//! `IntelliJ` New UI `Project` dock panel: `Project ⌄` header with hover toolbar,
//! inline `+` (`New File` / `New Folder`) modal, `Options (⋮)` popover, and vector-icon
//! virtualized tree rendering (`KOVA-055`).

use std::cell::Cell;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use gpui::{
    Bounds, ClickEvent, Context, Div, FontWeight, Hsla, InteractiveElement as _, MouseButton,
    MouseDownEvent, ParentElement as _, Pixels, ScrollStrategy, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Window, canvas, div, px, uniform_list,
};
use kova_core::project::is_safe_relative_path;

use super::{DOCK_HEADER_HEIGHT_PX, FILTER_BAR_HEIGHT_PX, Palette, WorkspaceShell};
use crate::controls::{ControlPalette, TextInputEvent, TextInputState};
use crate::dock::project_icons::{
    ProjectIconPalette, render_chevron, render_collapse_all_icon, render_expand_all_icon,
    render_file_icon, render_folder_icon, render_header_chevron, render_hide_icon,
    render_locate_file_icon, render_new_item_icon, render_options_icon,
};
use crate::dock::project_tree::{PROJECT_INDENT_PX, PROJECT_ROW_HEIGHT_PX};
use crate::dock::{DockToolId, FocusedRegion, ProjectNode, ProjectNodeKind, TreeActivation};
use crate::document::StatusKind;
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Kind of entry created by the inline `+` modal in the `Project` dock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NewEntryKind {
    /// Create an empty file (and missing parent directories) and open it in the editor.
    #[default]
    File,
    /// Create a directory (and missing parent directories) and select it in the tree.
    Directory,
}

impl NewEntryKind {
    /// Human-readable pill label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Directory => "Folder",
        }
    }

    /// Toggles between [`Self::File`] and [`Self::Directory`].
    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::File => Self::Directory,
            Self::Directory => Self::File,
        }
    }
}

/// Outcome of feeding a keystroke to [`NewProjectItemState::handle_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewItemKeyOutcome {
    /// Key was consumed by the modal (text edit, mode switch, navigation).
    Consumed,
    /// User pressed `Enter` to submit the creation form.
    Submit,
    /// User pressed `Escape` to cancel without writing to disk.
    Cancel,
    /// Key was ignored (e.g. unhandled modifier shortcut).
    Ignored,
}

/// Pure headless state for the inline `+` (*New File / New Folder*) modal (`KOVA-055`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewProjectItemState {
    open: bool,
    kind: NewEntryKind,
    base_dir: PathBuf,
    input: TextInputState,
    error: Option<String>,
}

impl NewProjectItemState {
    /// Opens the modal in `kind` mode anchored at project-relative `base_dir`.
    pub fn open(&mut self, kind: NewEntryKind, base_dir: PathBuf) {
        self.open = true;
        self.kind = kind;
        self.base_dir = base_dir;
        self.input.clear();
        self.error = None;
    }

    /// Closes the modal and clears transient input/error state.
    pub fn close(&mut self) {
        self.open = false;
        self.input.clear();
        self.error = None;
    }

    /// Returns `true` while the inline modal is visible.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Returns the currently selected entry kind (`File` or `Folder`).
    #[must_use]
    pub const fn kind(&self) -> NewEntryKind {
        self.kind
    }

    /// Switches the entry kind and clears any validation error.
    pub fn set_kind(&mut self, kind: NewEntryKind) {
        self.kind = kind;
        self.error = None;
    }

    /// Toggles between `File` and `Folder`.
    pub fn toggle_kind(&mut self) {
        self.set_kind(self.kind.toggled());
    }

    /// Project-relative target directory into which the new entry will be created.
    #[must_use]
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Human-readable description of the base directory (`<project-root>` when empty).
    #[must_use]
    pub fn base_dir_label(&self) -> String {
        if self.base_dir.as_os_str().is_empty() {
            "<project-root>".to_owned()
        } else {
            self.base_dir.display().to_string()
        }
    }

    /// Returns the headless text input state.
    #[must_use]
    pub const fn input(&self) -> &TextInputState {
        &self.input
    }

    /// Returns the inline validation error, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Sets or clears the inline validation error.
    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    /// Handles a GPUI keystroke while the inline modal owns keyboard focus.
    pub fn handle_key(&mut self, keystroke: &gpui::Keystroke) -> NewItemKeyOutcome {
        let mods = &keystroke.modifiers;
        let key = keystroke.key.as_str();

        if key == "escape" && !mods.platform && !mods.control && !mods.alt {
            return NewItemKeyOutcome::Cancel;
        }
        if key == "enter" && !mods.platform && !mods.control && !mods.alt {
            return NewItemKeyOutcome::Submit;
        }
        if matches!(key, "tab" | "up" | "down") && !mods.platform && !mods.control && !mods.alt {
            self.toggle_kind();
            return NewItemKeyOutcome::Consumed;
        }
        if key == "backspace" && (mods.platform || mods.control) && !mods.alt {
            if !self.input.text().is_empty() {
                self.input.clear();
                self.error = None;
            }
            return NewItemKeyOutcome::Consumed;
        }
        if mods.platform || mods.control {
            return NewItemKeyOutcome::Ignored;
        }

        if !mods.alt
            && matches!(
                key,
                "backspace" | "delete" | "left" | "right" | "home" | "end"
            )
        {
            if self.input.handle_named_key(key) == TextInputEvent::Edited {
                self.error = None;
            }
            return NewItemKeyOutcome::Consumed;
        }

        // `Option+<key>` on macOS (`mods.alt`) produces national characters (e.g. `ą`, `ę`) when
        // paired with a single character key; ignore named function/navigation keys.
        let is_character_key = key.chars().count() == 1 || key == "space";
        if (!mods.alt || is_character_key)
            && let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text
                .chars()
                .any(|ch| ch.is_control() || ('\u{f700}'..='\u{f8ff}').contains(&ch))
        {
            if self.input.insert(text) == TextInputEvent::Edited {
                self.error = None;
            }
            return NewItemKeyOutcome::Consumed;
        }

        if !mods.alt && key == "space" && self.input.insert(" ") == TextInputEvent::Edited {
            self.error = None;
            return NewItemKeyOutcome::Consumed;
        }

        NewItemKeyOutcome::Consumed
    }

    /// Validates the entered relative path against `project_root` and returns `(rel_path, abs_path)`.
    ///
    /// # Errors
    ///
    /// Returns a user-facing validation error if no disk project is open, the input is empty,
    /// contains unsafe segments (`.`, `..`, leading `/`/`\`, NUL, `.idea`), escapes `project_root`
    /// through a symlink, or collides with an existing entry on disk.
    pub fn validate_and_resolve(
        &self,
        project_root: Option<&Path>,
    ) -> Result<(PathBuf, PathBuf), String> {
        let Some(root) = project_root else {
            return Err("Open a project folder to create files on disk".to_owned());
        };

        let raw = self.input.text().trim();
        if raw.is_empty() {
            return Err(match self.kind {
                NewEntryKind::File => "Enter a file name".to_owned(),
                NewEntryKind::Directory => "Enter a folder name".to_owned(),
            });
        }
        if raw.contains('\0') {
            return Err("Name cannot contain NUL characters".to_owned());
        }
        if raw.starts_with('/') || raw.starts_with('\\') {
            return Err("Path must be relative to the selected folder".to_owned());
        }
        if raw.contains('\\') {
            return Err("Use '/' to separate nested folders".to_owned());
        }

        let trimmed = match self.kind {
            NewEntryKind::File => {
                if raw.ends_with('/') {
                    return Err("File name cannot end with '/'".to_owned());
                }
                raw
            }
            NewEntryKind::Directory => raw.trim_end_matches('/'),
        };
        if trimmed.is_empty() {
            return Err("Enter a folder name".to_owned());
        }

        for segment in trimmed.split('/') {
            if segment.is_empty() {
                return Err("Path cannot contain empty segments ('//')".to_owned());
            }
            if segment == "." || segment == ".." {
                return Err("Path cannot contain '.' or '..'".to_owned());
            }
            if segment == ".idea" {
                return Err("Creating '.idea' entries is disallowed in Kova".to_owned());
            }
        }

        let rel_input = Path::new(trimmed);
        if !is_safe_relative_path(rel_input) {
            return Err("Invalid relative path".to_owned());
        }

        let combined_rel = if self.base_dir.as_os_str().is_empty() {
            rel_input.to_path_buf()
        } else {
            if !is_safe_relative_path(&self.base_dir) {
                return Err("Invalid target directory".to_owned());
            }
            self.base_dir.join(rel_input)
        };

        if !is_safe_relative_path(&combined_rel)
            || combined_rel
                .components()
                .any(|c| matches!(c, Component::Normal(s) if s == ".idea"))
        {
            return Err("Invalid target path".to_owned());
        }

        let canonical_root = std::fs::canonicalize(root)
            .map_err(|err| format!("Cannot access project root: {err}"))?;
        let abs_path = canonical_root.join(&combined_rel);

        // Verify the deepest existing ancestor remains inside `canonical_root` (blocks symlink escapes).
        let mut ancestor = abs_path.parent();
        while let Some(dir) = ancestor {
            if dir.exists() {
                let canonical_ancestor = std::fs::canonicalize(dir)
                    .map_err(|err| format!("Cannot resolve target directory: {err}"))?;
                if !canonical_ancestor.starts_with(&canonical_root) {
                    return Err("Target path escapes the project directory".to_owned());
                }
                break;
            }
            ancestor = dir.parent();
        }

        if std::fs::symlink_metadata(&abs_path).is_ok() {
            return Err(format!("'{}' already exists", combined_rel.display()));
        }

        Ok((combined_rel, abs_path))
    }
}

/// Wrapper holding [`NewProjectItemState`] plus painted bounds for outside-click dismissal.
#[derive(Debug, Clone, Default)]
pub(super) struct NewProjectItemModal {
    state: NewProjectItemState,
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    trigger_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl NewProjectItemModal {
    pub(super) const fn is_open(&self) -> bool {
        self.state.is_open()
    }

    pub(super) const fn state(&self) -> &NewProjectItemState {
        &self.state
    }

    pub(super) const fn state_mut(&mut self) -> &mut NewProjectItemState {
        &mut self.state
    }

    pub(super) fn clear_bounds(&self) {
        self.bounds.set(None);
    }

    pub(super) fn contains_point_or_trigger(&self, point: gpui::Point<Pixels>) -> bool {
        self.bounds.get().is_some_and(|b| b.contains(&point))
            || self
                .trigger_bounds
                .get()
                .is_some_and(|b| b.contains(&point))
    }
}

/// Atomically creates a file or directory on disk inside `abs_path`.
///
/// # Errors
///
/// Returns a user-facing error message if creating parent directories or the entry fails.
pub fn create_project_item_on_disk(kind: NewEntryKind, abs_path: &Path) -> Result<(), String> {
    match kind {
        NewEntryKind::File => {
            if let Some(parent) = abs_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| format!("Cannot create parent directory: {err}"))?;
            }
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(abs_path)
                .map_err(|err| format!("Cannot create file: {err}"))?;
            Ok(())
        }
        NewEntryKind::Directory => {
            std::fs::create_dir_all(abs_path)
                .map_err(|err| format!("Cannot create folder: {err}"))?;
            Ok(())
        }
    }
}

fn track_bounds(slot: Rc<Cell<Option<Bounds<Pixels>>>>) -> gpui::Canvas<()> {
    canvas(
        move |bounds, _, _| {
            slot.set(Some(bounds));
        },
        |_, (), _, _| {},
    )
    .absolute()
    .size_full()
}

impl WorkspaceShell {
    /// Opens the inline `+` (*New File / New Folder*) modal anchored to the selected node's directory.
    pub(super) fn open_new_item_modal(
        &mut self,
        kind: NewEntryKind,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let base_dir = self.project_tree.target_dir_for_creation();
        self.project_options_open = false;
        self.project_options_bounds.set(None);
        self.close_completion_popup(cx);
        self.ex_line = None;
        self.keymap.reset();
        self.chord_hint = None;
        self.new_item_modal.state_mut().open(kind, base_dir);
        self.dock_layout.focus(FocusedRegion::ProjectDock);
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Closes the inline `+` modal without modifying disk.
    pub(super) fn close_new_item_modal(
        &mut self,
        restore_project_focus: bool,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !self.new_item_modal.is_open() {
            return;
        }
        self.new_item_modal.state_mut().close();
        self.new_item_modal.clear_bounds();
        if restore_project_focus {
            self.focus_dock(FocusedRegion::ProjectDock, window, cx);
        } else {
            cx.notify();
        }
    }

    pub(super) fn submit_new_item_modal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let kind = self.new_item_modal.state().kind();
        let (rel_path, abs_path) = match self
            .new_item_modal
            .state()
            .validate_and_resolve(self.project.root())
        {
            Ok(paths) => paths,
            Err(err) => {
                self.new_item_modal.state_mut().set_error(Some(err));
                cx.notify();
                return;
            }
        };

        if let Err(err) = create_project_item_on_disk(kind, &abs_path) {
            self.new_item_modal.state_mut().set_error(Some(err));
            cx.notify();
            return;
        }

        let is_dir = kind == NewEntryKind::Directory;
        self.project_tree
            .insert_created_entry(&rel_path, is_dir, false);
        self.test_runner.set_project_files(
            self.project_tree
                .file_paths()
                .map(Path::to_path_buf)
                .collect(),
        );
        self.project_scroll
            .scroll_to_item(self.project_tree.selected_index(), ScrollStrategy::Center);

        self.new_item_modal.state_mut().close();
        self.new_item_modal.clear_bounds();

        match kind {
            NewEntryKind::File => {
                self.open_path(&rel_path, None, window, cx);
            }
            NewEntryKind::Directory => {
                self.focus_dock(FocusedRegion::ProjectDock, window, cx);
            }
        }
        cx.notify();
    }

    /// Capture-phase key handler for the inline `+` modal and `Project` options popover.
    pub(super) fn handle_new_item_modal_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if self.new_item_modal.is_open() {
            match self.new_item_modal.state_mut().handle_key(keystroke) {
                NewItemKeyOutcome::Cancel => {
                    self.close_new_item_modal(true, window, cx);
                }
                NewItemKeyOutcome::Submit => {
                    self.submit_new_item_modal(window, cx);
                }
                NewItemKeyOutcome::Consumed => cx.notify(),
                NewItemKeyOutcome::Ignored => {}
            }
            return true;
        }

        if self.project_options_open
            && keystroke.key == "escape"
            && !keystroke.modifiers.platform
            && !keystroke.modifiers.control
            && !keystroke.modifiers.alt
        {
            self.close_project_options(cx);
            return true;
        }

        false
    }

    /// Handles outside clicks while the `Project` options popover or inline `+` modal is open.
    pub(super) fn handle_project_overlay_outside_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.project_options_open {
            let inside = self
                .project_options_bounds
                .get()
                .is_some_and(|b| b.contains(&event.position))
                || self
                    .project_options_btn_bounds
                    .get()
                    .is_some_and(|b| b.contains(&event.position));
            if !inside {
                self.close_project_options(cx);
            }
        }

        if self.new_item_modal.is_open()
            && !self
                .new_item_modal
                .contains_point_or_trigger(event.position)
        {
            self.close_new_item_modal(false, window, cx);
        }
    }

    /// Reveals and centers the currently opened file in the `Project` tree (`Select Opened File`).
    pub(super) fn select_opened_file_in_tree(
        &mut self,
        focus_dock_window: Option<&mut Window>,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(active_path) = self.active_document_key() else {
            return;
        };
        if self.project_tree.reveal_path(&active_path) {
            self.project_scroll
                .scroll_to_item(self.project_tree.selected_index(), ScrollStrategy::Center);
            if let Some(window) = focus_dock_window {
                self.focus_dock(FocusedRegion::ProjectDock, window, cx);
            } else {
                cx.notify();
            }
        } else if focus_dock_window.is_some() {
            self.set_notice(
                StatusKind::Info,
                format!(
                    "'{}' is not part of the project tree",
                    active_path.display()
                ),
                cx,
            );
        }
    }

    /// Expands all directories in the `Project` tree.
    pub(super) fn expand_all_project_tree(&mut self, cx: &mut Context<'_, Self>) {
        self.project_tree.expand_all();
        self.project_scroll
            .scroll_to_item(self.project_tree.selected_index(), ScrollStrategy::Center);
        cx.notify();
    }

    /// Collapses all non-root directories in the `Project` tree, leaving the root expanded.
    pub(super) fn collapse_all_project_tree(&mut self, cx: &mut Context<'_, Self>) {
        self.project_tree.collapse_all();
        self.project_scroll
            .scroll_to_item(self.project_tree.selected_index(), ScrollStrategy::Center);
        cx.notify();
    }

    pub(super) fn toggle_project_options(&mut self, cx: &mut Context<'_, Self>) {
        self.project_options_open = !self.project_options_open;
        if !self.project_options_open {
            self.project_options_bounds.set(None);
        }
        cx.notify();
    }

    pub(super) fn close_project_options(&mut self, cx: &mut Context<'_, Self>) {
        if self.project_options_open {
            self.project_options_open = false;
            self.project_options_bounds.set(None);
            cx.notify();
        }
    }

    pub(super) fn hide_project_dock(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.project_options_open = false;
        self.project_options_bounds.set(None);
        self.new_item_modal.state_mut().close();
        self.new_item_modal.clear_bounds();
        if self.dock_layout.left_open == Some(DockToolId::Project) {
            self.dock_layout
                .toggle_tool(DockToolId::Project, self.active_pane);
        }
        self.focus_active_editor(window, cx);
    }

    /// Renders the `IntelliJ` New UI `Project` left dock (`Project ⌄` header, hover toolbar,
    /// inline `+` modal, options popover, virtualized vector tree, and type-to-filter bar).
    pub(super) fn render_project_dock(
        &self,
        island: Stateful<Div>,
        p: Palette,
        cx: &Context<'_, Self>,
    ) -> Stateful<Div> {
        let query = self.project_tree.filter_query();
        let (filter_text, filter_color) = if query.is_empty() {
            ("Type to filter tree...".to_owned(), p.text_muted)
        } else {
            (format!("⌕ {query}"), p.text_primary)
        };

        let mut dock = island
            .relative()
            .on_hover(cx.listener(|this, hovered: &bool, _window, cx| {
                if this.project_dock_hovered != *hovered {
                    this.project_dock_hovered = *hovered;
                    cx.notify();
                }
            }))
            .child(self.render_project_header(p, cx));

        if self.new_item_modal.is_open() {
            dock = dock.child(self.render_new_item_modal_card(p, cx.global::<KovaTheme>(), cx));
        }

        dock = dock
            .child(
                uniform_list(
                    "project-tree",
                    self.project_tree.visible_len(),
                    cx.processor(Self::render_project_tree_rows),
                )
                .track_scroll(self.project_scroll.clone())
                .flex_1()
                .w_full()
                .py(px(4.0)),
            )
            .child(
                div()
                    .h(px(FILTER_BAR_HEIGHT_PX))
                    .flex_none()
                    .mx(px(6.0))
                    .mb(px(6.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .rounded(px(6.0))
                    .bg(p.input)
                    .border_1()
                    .border_color(p.border_island)
                    .text_size(px(11.0))
                    .text_color(filter_color)
                    .child(filter_text),
            );

        if self.project_options_open {
            dock = dock.child(self.render_project_options_popover(p, cx));
        }
        dock
    }

    fn render_project_header(&self, p: Palette, cx: &Context<'_, Self>) -> Stateful<Div> {
        let mut left = div()
            .id("project-header-title")
            .flex()
            .items_center()
            .gap(px(5.0))
            .cursor_pointer()
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.focus_dock(FocusedRegion::ProjectDock, window, cx);
            }))
            .child(
                div()
                    .text_size(px(12.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(p.text_primary)
                    .child("Project"),
            )
            .child(render_header_chevron(p.text_secondary));

        if self.project.is_loading() {
            left = left.child(
                div()
                    .ml(px(4.0))
                    .text_size(px(10.5))
                    .text_color(p.text_muted)
                    .child("Loading…"),
            );
        }

        div()
            .id("project-dock-header")
            .h(px(DOCK_HEADER_HEIGHT_PX))
            .px(px(10.0))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(p.border_subtle)
            .child(left)
            .child(self.render_project_toolbar(p, cx))
    }

    fn render_project_toolbar(&self, p: Palette, cx: &Context<'_, Self>) -> Div {
        let toolbar_active =
            self.project_dock_hovered || self.new_item_modal.is_open() || self.project_options_open;
        let icon_color = if toolbar_active {
            p.text_secondary
        } else {
            let mut muted = p.text_muted;
            muted.a *= 0.55;
            muted
        };

        let action_btn = |id: &'static str| {
            div()
                .id(id)
                .relative()
                .w(px(22.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.0))
                .cursor_pointer()
                .hover(move |s| s.bg(p.hover))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        };

        let new_btn_bounds = Rc::clone(&self.new_item_modal.trigger_bounds);
        let options_btn_bounds = Rc::clone(&self.project_options_btn_bounds);

        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .opacity(if toolbar_active { 1.0 } else { 0.42 })
            .child(
                action_btn("project-btn-new")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        if this.new_item_modal.is_open() {
                            this.close_new_item_modal(true, window, cx);
                        } else {
                            this.open_new_item_modal(NewEntryKind::File, window, cx);
                        }
                    }))
                    .child(track_bounds(new_btn_bounds))
                    .child(render_new_item_icon(icon_color)),
            )
            .child(div().w(px(1.0)).h(px(14.0)).mx(px(2.0)).bg(p.border_subtle))
            .child(
                action_btn("project-btn-locate")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.select_opened_file_in_tree(Some(window), cx);
                    }))
                    .child(render_locate_file_icon(icon_color)),
            )
            .child(
                action_btn("project-btn-expand-all")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.expand_all_project_tree(cx);
                    }))
                    .child(render_expand_all_icon(icon_color)),
            )
            .child(
                action_btn("project-btn-collapse-all")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.collapse_all_project_tree(cx);
                    }))
                    .child(render_collapse_all_icon(icon_color)),
            )
            .child(
                action_btn("project-btn-options")
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.toggle_project_options(cx);
                    }))
                    .child(track_bounds(options_btn_bounds))
                    .child(render_options_icon(icon_color)),
            )
            .child(
                action_btn("project-btn-hide")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.hide_project_dock(window, cx);
                    }))
                    .child(render_hide_icon(icon_color)),
            )
    }

    #[allow(
        clippy::cast_precision_loss,
        reason = "tree depth is a small u16 that converts exactly to f32"
    )]
    fn render_project_tree_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Stateful<Div>> {
        let theme = cx.global::<KovaTheme>();
        let p = Palette::from_theme(theme);
        let icon_palette = ProjectIconPalette::from_theme(theme);
        let selected = self.project_tree.selected_index();
        let dock_focused = self.dock_layout.focused == FocusedRegion::ProjectDock;
        let show_indent_guides = self.show_indent_guides;

        let mut inactive_sel = p.active;
        inactive_sel.a *= 0.65;

        self.project_tree
            .visible_range(range)
            .map(|(row, node)| {
                let indent = 6.0 + f32::from(node.depth) * PROJECT_INDENT_PX;
                let mut inner = div()
                    .relative()
                    .flex_1()
                    .h_full()
                    .pl(px(indent))
                    .pr(px(8.0))
                    .rounded(px(6.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .text_size(px(12.0))
                    .overflow_hidden();

                inner = if row == selected {
                    inner.bg(if dock_focused { p.active } else { inactive_sel })
                } else {
                    inner.hover(move |s| s.bg(p.hover))
                };

                if show_indent_guides {
                    for level in 0..node.depth {
                        inner = inner.child(
                            div()
                                .absolute()
                                .top(px(0.0))
                                .bottom(px(0.0))
                                .left(px(6.0 + f32::from(level) * PROJECT_INDENT_PX + 7.0))
                                .w(px(1.0))
                                .bg(p.border_subtle),
                        );
                    }
                }

                let text_color = if node.is_git_ignored {
                    icon_palette.git_ignored
                } else {
                    p.text_primary
                };

                inner = inner
                    .child(render_tree_chevron_slot(node, &icon_palette, p))
                    .child(render_tree_node_icon(node, &icon_palette))
                    .child(render_tree_node_label(node, text_color, p));

                div()
                    .id(("tree-row", row))
                    .h(px(PROJECT_ROW_HEIGHT_PX))
                    .w_full()
                    .px(px(6.0))
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.click_project_tree_row(row, window, cx);
                    }))
                    .child(inner)
            })
            .collect()
    }

    fn render_new_item_modal_card(
        &self,
        p: Palette,
        theme: &KovaTheme,
        cx: &Context<'_, Self>,
    ) -> Stateful<Div> {
        let cp = ControlPalette::from_theme(theme);
        let elevated = hsla_from_rgba(theme.surfaces().elevated);
        let border_strong = hsla_from_rgba(theme.borders().strong);
        let state = self.new_item_modal.state();
        let active_kind = state.kind();

        let pill = |kind: NewEntryKind| {
            let active = active_kind == kind;
            let mut el = div()
                .id(SharedString::from(format!(
                    "new-item-kind-{}",
                    kind.label()
                )))
                .px(px(8.0))
                .py(px(2.0))
                .rounded(px(5.0))
                .text_size(px(11.0))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.new_item_modal.state_mut().set_kind(kind);
                    window.focus(&this.focus_handle);
                    cx.notify();
                }))
                .child(kind.label());
            if active {
                el = el
                    .bg(p.active)
                    .border_1()
                    .border_color(p.border_focus)
                    .text_color(p.text_primary);
            } else {
                el = el
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .text_color(p.text_muted)
                    .hover(move |s| s.bg(p.hover).text_color(p.text_primary));
            }
            el
        };

        let modal_bounds = Rc::clone(&self.new_item_modal.bounds);
        let mut card = div()
            .id("new-project-item-modal")
            .relative()
            .flex_none()
            .mx(px(6.0))
            .mt(px(6.0))
            .mb(px(2.0))
            .p(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .rounded(px(8.0))
            .bg(elevated)
            .border_1()
            .border_color(border_strong)
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(track_bounds(modal_bounds))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .child(pill(NewEntryKind::File))
                            .child(pill(NewEntryKind::Directory)),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(p.text_muted)
                            .child("Tab: switch · Enter"),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(p.text_muted)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(format!("in {}", state.base_dir_label())),
            )
            .child(render_new_item_input(state, &cp, p));

        if let Some(error) = state.error() {
            card = card.child(
                div()
                    .text_size(px(11.0))
                    .text_color(p.error)
                    .child(error.to_owned()),
            );
        }

        card
    }

    fn render_project_options_popover(&self, p: Palette, cx: &Context<'_, Self>) -> Stateful<Div> {
        let theme = cx.global::<KovaTheme>();
        let elevated = hsla_from_rgba(theme.surfaces().elevated);
        let border_strong = hsla_from_rgba(theme.borders().strong);
        let popover_bounds = Rc::clone(&self.project_options_bounds);

        div()
            .id("project-options-popover")
            .absolute()
            .top(px(DOCK_HEADER_HEIGHT_PX + 2.0))
            .right(px(6.0))
            .w(px(214.0))
            .p(px(4.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .rounded(px(8.0))
            .bg(elevated)
            .border_1()
            .border_color(border_strong)
            .shadow_lg()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(track_bounds(popover_bounds))
            .child(
                project_option_toggle_row(
                    "project-opt-always-select",
                    "Always Select Opened File",
                    self.always_select_opened_file,
                    p,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.always_select_opened_file = !this.always_select_opened_file;
                    if this.always_select_opened_file {
                        this.select_opened_file_in_tree(None, cx);
                    }
                    this.close_project_options(cx);
                })),
            )
            .child(
                project_option_toggle_row(
                    "project-opt-show-git-ignored",
                    "Show Git-Ignored Files",
                    self.project_tree.show_git_ignored(),
                    p,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    let next = !this.project_tree.show_git_ignored();
                    this.project_tree.set_show_git_ignored(next);
                    this.close_project_options(cx);
                    cx.notify();
                })),
            )
            .child(
                project_option_toggle_row(
                    "project-opt-indent-guides",
                    "Show Indent Guides",
                    self.show_indent_guides,
                    p,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.show_indent_guides = !this.show_indent_guides;
                    this.close_project_options(cx);
                    cx.notify();
                })),
            )
            .child(div().h(px(1.0)).my(px(2.0)).bg(p.border_subtle))
            .child(
                project_option_action_row("project-opt-expand-all", "Expand All", p).on_click(
                    cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.expand_all_project_tree(cx);
                        this.close_project_options(cx);
                    }),
                ),
            )
            .child(
                project_option_action_row("project-opt-collapse-all", "Collapse All", p).on_click(
                    cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.collapse_all_project_tree(cx);
                        this.close_project_options(cx);
                    }),
                ),
            )
            .child(
                project_option_action_row("project-opt-reload", "Reload from Disk", p).on_click(
                    cx.listener(|this, _: &ClickEvent, _window, cx| {
                        this.close_project_options(cx);
                        this.load_project(cx);
                        cx.notify();
                    }),
                ),
            )
    }

    /// Click handler for Project tree rows.
    pub(super) fn click_project_tree_row(
        &mut self,
        row: usize,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.project_tree.select_index(row);
        self.dock_layout.focus(FocusedRegion::ProjectDock);
        match self.project_tree.activate_selected() {
            Some(TreeActivation::OpenFile(path)) => self.open_path(&path, None, window, cx),
            Some(TreeActivation::Toggled) | None => {
                self.focus_dock(FocusedRegion::ProjectDock, window, cx);
            }
        }
    }
}

fn render_tree_chevron_slot(
    node: &ProjectNode,
    icon_palette: &ProjectIconPalette,
    p: Palette,
) -> Div {
    if let ProjectNodeKind::Directory { expanded } = &node.kind {
        let chevron_color = if node.is_git_ignored {
            icon_palette.git_ignored
        } else {
            p.text_secondary
        };
        div()
            .w(px(14.0))
            .h(px(16.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .child(render_chevron(*expanded, chevron_color))
    } else {
        div().w(px(14.0)).h(px(16.0)).flex_none()
    }
}

fn render_tree_node_icon(
    node: &ProjectNode,
    icon_palette: &ProjectIconPalette,
) -> gpui::AnyElement {
    if node.is_dir() {
        let folder_color = if node.is_git_ignored {
            icon_palette.git_ignored
        } else {
            icon_palette.folder_stroke
        };
        render_folder_icon(
            node.is_root,
            folder_color,
            icon_palette.module_badge,
            icon_palette.island_bg,
        )
    } else {
        render_file_icon(node.icon_kind, icon_palette, node.is_git_ignored)
    }
}

fn render_tree_node_label(node: &ProjectNode, text_color: Hsla, p: Palette) -> Div {
    if node.is_root {
        let mut root_row = div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .overflow_hidden()
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::BOLD)
                    .text_color(text_color)
                    .whitespace_nowrap()
                    .child(node.name.clone()),
            );
        if let Some(hint) = &node.root_path_hint {
            root_row = root_row.child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(11.5))
                    .text_color(p.text_muted)
                    .child(hint.clone()),
            );
        }
        root_row
    } else if node.is_kova_config {
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .overflow_hidden()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(text_color)
                    .whitespace_nowrap()
                    .child(node.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(4.0))
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(p.warning)
                    .text_size(px(9.0))
                    .text_color(p.warning)
                    .child("project config"),
            )
    } else {
        div()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_color(text_color)
            .child(node.name.clone())
    }
}

fn render_new_item_input(
    state: &NewProjectItemState,
    cp: &ControlPalette,
    p: Palette,
) -> Stateful<Div> {
    let (before_caret, after_caret) = state.input().split_at_cursor();
    let input_empty = before_caret.is_empty() && after_caret.is_empty();
    let placeholder = match state.kind() {
        NewEntryKind::File => "Name (e.g. Service.java or dir/doc.md)",
        NewEntryKind::Directory => "Folder name (e.g. domain/model)",
    };

    let mut input_content = div()
        .flex()
        .items_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(cp.text)
        .child(SharedString::from(before_caret.to_owned()))
        .child(div().w(px(1.5)).h(px(13.0)).bg(cp.caret))
        .child(SharedString::from(after_caret.to_owned()));
    if input_empty {
        input_content = input_content.child(
            div()
                .ml(px(2.0))
                .text_color(cp.text_muted)
                .child(placeholder),
        );
    }

    div()
        .id("new-project-item-input")
        .w_full()
        .h(px(26.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .rounded(px(6.0))
        .bg(cp.input)
        .border_1()
        .border_color(if state.error().is_some() {
            p.error
        } else {
            p.border_focus
        })
        .text_size(px(12.0))
        .cursor_text()
        .child(input_content)
}

fn project_option_toggle_row(
    id: &'static str,
    label: &'static str,
    checked: bool,
    p: Palette,
) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(24.0))
        .px(px(8.0))
        .rounded(px(5.0))
        .flex()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .text_size(px(11.5))
        .text_color(p.text_primary)
        .cursor_pointer()
        .hover(move |s| s.bg(p.hover))
        .child(label)
        .child(
            div()
                .w(px(14.0))
                .text_size(px(11.0))
                .font_weight(FontWeight::BOLD)
                .text_color(if checked {
                    p.border_focus
                } else {
                    gpui::transparent_black()
                })
                .child(if checked { "✓" } else { "" }),
        )
}

fn project_option_action_row(id: &'static str, label: &'static str, p: Palette) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(24.0))
        .px(px(8.0))
        .rounded(px(5.0))
        .flex()
        .items_center()
        .text_size(px(11.5))
        .text_color(p.text_primary)
        .cursor_pointer()
        .hover(move |s| s.bg(p.hover))
        .child(label)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "unit tests assert modal validation and filesystem creation invariants"
    )]

    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use gpui::Keystroke;

    use super::{
        NewEntryKind, NewItemKeyOutcome, NewProjectItemState, create_project_item_on_disk,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> std::io::Result<Self> {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("kova-055-panel-test-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir)?;
            Ok(Self(dir))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    fn key(name: &str, ch: Option<&str>) -> Keystroke {
        Keystroke {
            key: name.to_owned(),
            key_char: ch.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn modal_state_handles_kind_toggling_typing_and_submit_cancel() {
        let mut state = NewProjectItemState::default();
        assert!(!state.is_open());
        state.open(NewEntryKind::File, PathBuf::from("src/main"));
        assert!(state.is_open());
        assert_eq!(state.kind(), NewEntryKind::File);
        assert_eq!(state.base_dir_label(), "src/main");

        assert_eq!(
            state.handle_key(&key("tab", None)),
            NewItemKeyOutcome::Consumed
        );
        assert_eq!(state.kind(), NewEntryKind::Directory);
        assert_eq!(
            state.handle_key(&key("down", None)),
            NewItemKeyOutcome::Consumed
        );
        assert_eq!(state.kind(), NewEntryKind::File);

        for ch in ["O", "r", "d", "e", "r", ".", "j", "a", "v", "a"] {
            assert_eq!(
                state.handle_key(&key(ch, Some(ch))),
                NewItemKeyOutcome::Consumed
            );
        }
        assert_eq!(state.input().text(), "Order.java");
        assert_eq!(
            state.handle_key(&key("enter", None)),
            NewItemKeyOutcome::Submit
        );
        assert_eq!(
            state.handle_key(&key("escape", None)),
            NewItemKeyOutcome::Cancel
        );

        state.close();
        assert!(!state.is_open());
        assert_eq!(state.input().text(), "");
    }

    #[test]
    fn validation_rejects_unsafe_paths_and_missing_project_root() {
        let mut state = NewProjectItemState::default();
        state.open(NewEntryKind::File, PathBuf::new());
        assert_eq!(state.base_dir_label(), "<project-root>");
        assert!(state.validate_and_resolve(None).is_err());

        let temp = TempDir::new().expect("tempdir");
        std::fs::write(temp.path().join("existing.md"), "hi").expect("existing");

        for bad in [
            "",
            "   ",
            "../secret.txt",
            "a/../b.txt",
            "/abs/path.txt",
            "\\win\\path.txt",
            "dir/",
            "a//b.txt",
            ".idea/workspace.xml",
            "existing.md",
        ] {
            state.open(NewEntryKind::File, PathBuf::new());
            state.input.insert(bad);
            assert!(
                state.validate_and_resolve(Some(temp.path())).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn creates_nested_file_and_folder_atomically_on_disk() {
        let temp = TempDir::new().expect("tempdir");
        let mut state = NewProjectItemState::default();

        // Create nested file inside `src/main`.
        state.open(NewEntryKind::File, PathBuf::from("src/main"));
        state.input.insert("domain/Order.java");
        let (rel, abs) = state
            .validate_and_resolve(Some(temp.path()))
            .expect("valid nested file");
        assert_eq!(rel, Path::new("src/main/domain/Order.java"));
        create_project_item_on_disk(NewEntryKind::File, &abs).expect("create file");
        assert!(abs.is_file());

        // Attempting to recreate the same file fails validation.
        assert!(state.validate_and_resolve(Some(temp.path())).is_err());

        // Create nested folder from root.
        state.open(NewEntryKind::Directory, PathBuf::new());
        state.input.insert("docs/api/");
        let (dir_rel, dir_abs) = state
            .validate_and_resolve(Some(temp.path()))
            .expect("valid folder");
        assert_eq!(dir_rel, Path::new("docs/api"));
        create_project_item_on_disk(NewEntryKind::Directory, &dir_abs).expect("create folder");
        assert!(dir_abs.is_dir());
    }
}
