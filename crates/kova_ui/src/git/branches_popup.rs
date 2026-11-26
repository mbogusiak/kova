//! IntelliJ-style **Git Branches** floating popup (`BranchesPopup`, headless
//! [`BranchesPopupModel`]): branch filtering, local/remote checkout, `+ New Branch...`,
//! branch deletion (with force-delete option when unmerged), and background `Fetch` / `Pull`.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, ClickEvent, Context, Div, EventEmitter, FocusHandle, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement as _, Render, ScrollStrategy, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Task, UniformListScrollHandle, Window, div, px,
    uniform_list,
};
use kova_theme::PluginVariant;
use kova_tools::git::{BranchInfo, BranchKind, BranchList, GitError, GitRepository, HeadState};

use super::{BRANCHES_POPUP_KEY_CONTEXT, CloseBranchesPopup};
use crate::controls::{Button, ControlPalette, TextInputEvent, TextInputState};
use crate::theme::{KovaTheme, hsla_from_rgba};

const POPUP_WIDTH_PX: f32 = 400.0;
const POPUP_MAX_HEIGHT_PX: f32 = 460.0;
const ROW_HEIGHT_PX: f32 = 28.0;
const LIST_MIN_HEIGHT_PX: f32 = 112.0;
const LIST_MAX_HEIGHT_PX: f32 = 256.0;

/// Active view mode inside [`BranchesPopupModel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BranchesMode {
    /// Filtering and navigating branches (`+ New Branch...`, Local, Remote).
    #[default]
    Browse,
    /// Entering the name for a new branch created from current `HEAD`.
    NewBranch,
}

/// One row in the flattened branch popup list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchesRow {
    /// First selectable action row (`+ New Branch...`).
    NewBranchAction,
    /// Non-selectable section header (`Local (N)` / `Remote (M)`).
    SectionHeader {
        /// Section display name (`"Local"` or `"Remote"`).
        title: &'static str,
        /// Number of visible items in this section.
        count: usize,
    },
    /// Local branch under `refs/heads/*`.
    LocalBranch(BranchInfo),
    /// Remote-tracking branch under `refs/remotes/*`.
    RemoteBranch(BranchInfo),
}

impl BranchesRow {
    /// Returns `true` when keyboard navigation and `Enter` can select this row.
    #[must_use]
    pub const fn is_selectable(&self) -> bool {
        !matches!(self, Self::SectionHeader { .. })
    }
}

/// Outcome of processing a keyboard event in [`BranchesPopupModel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchesKeyOutcome {
    /// Key was ignored by the popup.
    Ignored,
    /// Popup state updated internally (typing, selection move, mode toggle).
    Consumed,
    /// `Esc` in browse mode requested closing the popup.
    Close,
    /// `Enter` activated a branch row (local or remote).
    ActivateSelected(BranchesRow),
    /// `Enter` in new-branch mode requested creating and checking out `name`.
    CreateBranch(String),
    /// `Delete` / `Backspace` (with empty filter) requested deleting local branch `name`.
    DeleteBranch(String),
}

/// Callback returning display paths of dirty in-memory editor buffers that must be saved
/// before switching branches or pulling.
pub type UnsavedFilesCheck = Rc<dyn Fn(&App) -> Vec<String>>;

/// Pure headless state machine behind [`BranchesPopup`].
#[derive(Debug, Clone, Default)]
pub struct BranchesPopupModel {
    open: bool,
    has_repo: bool,
    mode: BranchesMode,
    filter: TextInputState,
    new_branch_input: TextInputState,
    branch_list: Option<BranchList>,
    rows: Vec<BranchesRow>,
    selected: Option<usize>,
    busy: Option<&'static str>,
    error: Option<String>,
    unmerged_branch: Option<String>,
    status_message: Option<String>,
}

impl BranchesPopupModel {
    /// Creates a closed [`BranchesPopupModel`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the popup is currently open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    /// Whether a Git repository is attached.
    #[must_use]
    pub const fn has_repo(&self) -> bool {
        self.has_repo
    }

    /// Active mode (`Browse` or `NewBranch`).
    #[must_use]
    pub const fn mode(&self) -> BranchesMode {
        self.mode
    }

    /// Filter input state.
    #[must_use]
    pub const fn filter(&self) -> &TextInputState {
        &self.filter
    }

    /// New branch name input state.
    #[must_use]
    pub const fn new_branch_input(&self) -> &TextInputState {
        &self.new_branch_input
    }

    /// Latest branch snapshot loaded from `GitRepository`.
    #[must_use]
    pub const fn branch_list(&self) -> Option<&BranchList> {
        self.branch_list.as_ref()
    }

    /// Flattened visible rows (including section headers).
    #[must_use]
    pub fn rows(&self) -> &[BranchesRow] {
        &self.rows
    }

    /// Index of the currently selected row in [`Self::rows`], if any.
    #[must_use]
    pub const fn selected_index(&self) -> Option<usize> {
        self.selected
    }

    /// Currently selected row, if any.
    #[must_use]
    pub fn selected_row(&self) -> Option<&BranchesRow> {
        self.selected.and_then(|idx| self.rows.get(idx))
    }

    /// Busy label when a background Git operation is running.
    #[must_use]
    pub const fn busy(&self) -> Option<&'static str> {
        self.busy
    }

    /// Error message shown in the popup banner, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Name of a branch that failed non-forced deletion because it has unmerged commits.
    #[must_use]
    pub fn unmerged_branch(&self) -> Option<&str> {
        self.unmerged_branch.as_deref()
    }

    /// Non-error status summary (e.g. after `Fetch` / `Pull`).
    #[must_use]
    pub fn status_message(&self) -> Option<&str> {
        self.status_message.as_deref()
    }

    /// Opens the popup and resets transient filter/error state.
    pub fn open(&mut self, has_repo: bool) {
        self.open = true;
        self.has_repo = has_repo;
        self.mode = BranchesMode::Browse;
        self.filter.clear();
        self.new_branch_input.clear();
        self.error = None;
        self.unmerged_branch = None;
        self.status_message = None;
        self.rebuild_rows(false);
    }

    /// Closes the popup and clears transient input/error state.
    pub fn close(&mut self) {
        self.open = false;
        self.mode = BranchesMode::Browse;
        self.filter.clear();
        self.new_branch_input.clear();
        self.error = None;
        self.unmerged_branch = None;
        self.status_message = None;
        self.busy = None;
    }

    /// Clears repository state (when no Git repository is open).
    pub fn clear_repo(&mut self) {
        self.has_repo = false;
        self.branch_list = None;
        self.busy = None;
        self.error = None;
        self.unmerged_branch = None;
        self.status_message = None;
        self.rebuild_rows(false);
    }

    /// Installs a freshly loaded [`BranchList`] while keeping filter and sensible selection.
    pub fn set_branch_list(&mut self, list: BranchList) {
        self.has_repo = true;
        self.branch_list = Some(list);
        self.rebuild_rows(false);
    }

    /// Marks whether a background Git task is running.
    pub const fn set_busy(&mut self, busy: Option<&'static str>) {
        self.busy = busy;
    }

    /// Sets or clears the popup error banner.
    pub fn set_error(&mut self, error: Option<String>, unmerged_branch: Option<String>) {
        self.error = error;
        self.unmerged_branch = unmerged_branch;
        if self.error.is_some() {
            self.status_message = None;
        }
    }

    /// Sets or clears a non-error status summary inside the popup.
    pub fn set_status_message(&mut self, message: Option<String>) {
        self.status_message = message;
    }

    /// Switches into `+ New Branch...` entry mode.
    pub fn enter_new_branch_mode(&mut self) {
        if !self.has_repo {
            return;
        }
        self.mode = BranchesMode::NewBranch;
        self.new_branch_input.clear();
        self.error = None;
        self.unmerged_branch = None;
    }

    /// Returns from `NewBranch` mode back to `Browse` mode.
    pub fn exit_new_branch_mode(&mut self) {
        self.mode = BranchesMode::Browse;
        self.new_branch_input.clear();
        self.error = None;
        self.unmerged_branch = None;
    }

    /// Updates the filter query and jumps selection to the first matching branch (or `+ New Branch...`).
    pub fn set_filter(&mut self, query: impl Into<String>) {
        self.filter = TextInputState::new(query);
        self.rebuild_rows(true);
    }

    /// Selects a specific row index if it is selectable.
    pub fn select_row(&mut self, index: usize) -> bool {
        if self.rows.get(index).is_some_and(BranchesRow::is_selectable) {
            self.selected = Some(index);
            true
        } else {
            false
        }
    }

    /// Moves selection down to the next selectable row, skipping section headers.
    pub fn move_down(&mut self) -> Option<usize> {
        let start = self.selected.map_or(0, |idx| idx + 1);
        for idx in start..self.rows.len() {
            if self.rows[idx].is_selectable() {
                self.selected = Some(idx);
                return Some(idx);
            }
        }
        self.selected
    }

    /// Moves selection up to the previous selectable row, skipping section headers.
    pub fn move_up(&mut self) -> Option<usize> {
        let Some(cur) = self.selected else {
            return self.first_selectable();
        };
        for idx in (0..cur).rev() {
            if self.rows[idx].is_selectable() {
                self.selected = Some(idx);
                return Some(idx);
            }
        }
        self.selected
    }

    fn first_selectable(&self) -> Option<usize> {
        self.rows.iter().position(BranchesRow::is_selectable)
    }

    fn first_branch_row(&self) -> Option<usize> {
        self.rows.iter().position(|row| {
            matches!(
                row,
                BranchesRow::LocalBranch(_) | BranchesRow::RemoteBranch(_)
            )
        })
    }

    fn rebuild_rows(&mut self, prefer_first_branch_on_filter: bool) {
        let prev_key = self.selected_row().and_then(|row| match row {
            BranchesRow::NewBranchAction => Some((0u8, String::new())),
            BranchesRow::LocalBranch(info) => Some((1u8, info.name.clone())),
            BranchesRow::RemoteBranch(info) => Some((2u8, info.name.clone())),
            BranchesRow::SectionHeader { .. } => None,
        });

        self.rows.clear();
        if !self.has_repo {
            self.selected = None;
            return;
        }

        self.rows.push(BranchesRow::NewBranchAction);

        let query = self.filter.text().trim().to_owned();
        if let Some(list) = &self.branch_list {
            let local_matches: Vec<BranchInfo> = list
                .local
                .iter()
                .filter(|b| branch_matches(&b.name, &query))
                .cloned()
                .collect();
            if !local_matches.is_empty() {
                self.rows.push(BranchesRow::SectionHeader {
                    title: "Local",
                    count: local_matches.len(),
                });
                for branch in local_matches {
                    self.rows.push(BranchesRow::LocalBranch(branch));
                }
            }

            let remote_matches: Vec<BranchInfo> = list
                .remote
                .iter()
                .filter(|b| branch_matches(&b.name, &query))
                .cloned()
                .collect();
            if !remote_matches.is_empty() {
                self.rows.push(BranchesRow::SectionHeader {
                    title: "Remote",
                    count: remote_matches.len(),
                });
                for branch in remote_matches {
                    self.rows.push(BranchesRow::RemoteBranch(branch));
                }
            }
        }

        if prefer_first_branch_on_filter && !query.is_empty() {
            self.selected = self.first_branch_row().or_else(|| self.first_selectable());
            return;
        }

        if let Some((kind, name)) = prev_key
            && let Some(idx) = self.rows.iter().position(|row| match row {
                BranchesRow::NewBranchAction => kind == 0,
                BranchesRow::LocalBranch(b) => kind == 1 && b.name == name,
                BranchesRow::RemoteBranch(b) => kind == 2 && b.name == name,
                BranchesRow::SectionHeader { .. } => false,
            })
        {
            self.selected = Some(idx);
            return;
        }

        if query.is_empty() {
            self.selected = self.first_selectable();
        } else {
            self.selected = self.first_branch_row().or_else(|| self.first_selectable());
        }
    }

    /// Processes a single GPUI keystroke while the popup is open.
    pub fn handle_key(&mut self, keystroke: &gpui::Keystroke) -> BranchesKeyOutcome {
        if !self.open {
            return BranchesKeyOutcome::Ignored;
        }
        let mods = keystroke.modifiers;
        let key = keystroke.key.as_str();

        match self.mode {
            BranchesMode::NewBranch => self.handle_new_branch_key(key, mods, keystroke),
            BranchesMode::Browse => self.handle_browse_key(key, mods, keystroke),
        }
    }

    fn handle_new_branch_key(
        &mut self,
        key: &str,
        mods: gpui::Modifiers,
        keystroke: &gpui::Keystroke,
    ) -> BranchesKeyOutcome {
        if key == "escape" && !mods.platform && !mods.control && !mods.alt {
            self.exit_new_branch_mode();
            return BranchesKeyOutcome::Consumed;
        }
        if key == "enter" && !mods.platform && !mods.control && !mods.alt {
            if self.busy.is_some() {
                return BranchesKeyOutcome::Consumed;
            }
            let name = self.new_branch_input.text().trim().to_owned();
            if name.is_empty() {
                self.set_error(Some("Enter a branch name.".to_owned()), None);
                return BranchesKeyOutcome::Consumed;
            }
            return BranchesKeyOutcome::CreateBranch(name);
        }
        if key == "backspace" && (mods.platform || mods.control) && !mods.alt {
            self.new_branch_input.clear();
            self.set_error(None, None);
            return BranchesKeyOutcome::Consumed;
        }
        if mods.platform || mods.control {
            return BranchesKeyOutcome::Ignored;
        }
        let edited = if !mods.alt
            && let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text.chars().any(char::is_control)
        {
            self.new_branch_input.insert(text) == TextInputEvent::Edited
        } else if mods.alt
            && let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && text.chars().all(|c| !c.is_control())
            && !text.is_ascii()
        {
            self.new_branch_input.insert(text) == TextInputEvent::Edited
        } else {
            self.new_branch_input.handle_named_key(key) == TextInputEvent::Edited
        };
        if edited {
            self.set_error(None, None);
            BranchesKeyOutcome::Consumed
        } else {
            BranchesKeyOutcome::Ignored
        }
    }

    fn handle_browse_key(
        &mut self,
        key: &str,
        mods: gpui::Modifiers,
        keystroke: &gpui::Keystroke,
    ) -> BranchesKeyOutcome {
        if key == "escape" && !mods.platform && !mods.control && !mods.alt {
            return BranchesKeyOutcome::Close;
        }
        if (key == "down" && !mods.platform && !mods.control && !mods.alt)
            || (mods.control && !mods.platform && !mods.alt && !mods.shift && key == "n")
        {
            self.move_down();
            return BranchesKeyOutcome::Consumed;
        }
        if (key == "up" && !mods.platform && !mods.control && !mods.alt)
            || (mods.control && !mods.platform && !mods.alt && !mods.shift && key == "p")
        {
            self.move_up();
            return BranchesKeyOutcome::Consumed;
        }
        if key == "enter" && !mods.platform && !mods.control && !mods.alt {
            if self.busy.is_some() || !self.has_repo {
                return BranchesKeyOutcome::Consumed;
            }
            if let Some(row) = self.selected_row().cloned() {
                if matches!(row, BranchesRow::NewBranchAction) {
                    self.enter_new_branch_mode();
                    return BranchesKeyOutcome::Consumed;
                }
                return BranchesKeyOutcome::ActivateSelected(row);
            }
            return BranchesKeyOutcome::Consumed;
        }
        if (key == "delete" || key == "backspace")
            && !mods.platform
            && !mods.control
            && !mods.alt
            && self.filter.text().is_empty()
            && self.busy.is_none()
            && let Some(BranchesRow::LocalBranch(info)) = self.selected_row()
            && !info.is_current
        {
            return BranchesKeyOutcome::DeleteBranch(info.name.clone());
        }
        if key == "backspace" && (mods.platform || mods.control) && !mods.alt {
            if !self.filter.text().is_empty() {
                self.filter.clear();
                self.set_error(None, None);
                self.rebuild_rows(true);
            }
            return BranchesKeyOutcome::Consumed;
        }
        if mods.platform || mods.control {
            return BranchesKeyOutcome::Ignored;
        }

        let prev_text = self.filter.text().to_owned();
        let edited = if !mods.alt
            && let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && !text.chars().any(char::is_control)
        {
            self.filter.insert(text) == TextInputEvent::Edited
        } else if mods.alt
            && let Some(text) = keystroke.key_char.as_deref()
            && !text.is_empty()
            && text.chars().all(|c| !c.is_control())
            && !text.is_ascii()
        {
            self.filter.insert(text) == TextInputEvent::Edited
        } else {
            self.filter.handle_named_key(key) == TextInputEvent::Edited
        };

        if edited {
            if self.filter.text() != prev_text {
                self.set_error(None, None);
                self.rebuild_rows(true);
            }
            BranchesKeyOutcome::Consumed
        } else {
            BranchesKeyOutcome::Ignored
        }
    }
}

/// Case-insensitive substring or fuzzy ordered-subsequence match on branch names.
fn branch_matches(name: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let name_lower = name.to_lowercase();
    let query_lower = query.to_lowercase();
    if name_lower.contains(&query_lower) {
        return true;
    }
    let mut needle_chars = query_lower.chars();
    let Some(mut target) = needle_chars.next() else {
        return true;
    };
    for ch in name_lower.chars() {
        if ch == target {
            let Some(next) = needle_chars.next() else {
                return true;
            };
            target = next;
        }
    }
    false
}

/// Events emitted by [`BranchesPopup`] to [`crate::shell::WorkspaceShell`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchesPopupEvent {
    /// Popup requested closing (`Esc` or backdrop click).
    CloseRequested,
    /// Active branch or working tree changed (checkout, create, or pull).
    BranchChanged {
        /// Updated `HEAD` state.
        head: HeadState,
        /// `true` when working-tree files on disk may have changed.
        files_changed: bool,
    },
    /// Remote refs updated (`Fetch` or up-to-date `Pull`).
    RemoteUpdated {
        /// Human-readable summary message.
        summary: String,
    },
}

/// Floating IntelliJ-style **Git Branches** GPUI entity.
pub struct BranchesPopup {
    repo: Option<Arc<GitRepository>>,
    model: BranchesPopupModel,
    focus_handle: FocusHandle,
    list_scroll: UniformListScrollHandle,
    refresh_epoch: u64,
    refresh_task: Option<Task<()>>,
    op_task: Option<Task<()>>,
    unsaved_files_check: Option<UnsavedFilesCheck>,
}

impl std::fmt::Debug for BranchesPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchesPopup")
            .field("has_repo", &self.repo.is_some())
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<BranchesPopupEvent> for BranchesPopup {}

impl BranchesPopup {
    /// Creates a new [`BranchesPopup`] entity.
    #[must_use]
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        Self {
            repo: None,
            model: BranchesPopupModel::new(),
            focus_handle: cx.focus_handle(),
            list_scroll: UniformListScrollHandle::new(),
            refresh_epoch: 0,
            refresh_task: None,
            op_task: None,
            unsaved_files_check: None,
        }
    }

    /// Returns the popup's GPUI [`FocusHandle`].
    #[must_use]
    pub const fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Returns the underlying headless model.
    #[must_use]
    pub const fn model(&self) -> &BranchesPopupModel {
        &self.model
    }

    /// Installs a callback checked before branch-mutating operations (`checkout`, `pull`).
    pub fn set_unsaved_files_check(&mut self, check: UnsavedFilesCheck) {
        self.unsaved_files_check = Some(check);
    }

    /// Attaches a repository (or `None` when none is open).
    pub fn attach_repo(&mut self, repo: Option<Arc<GitRepository>>, cx: &mut Context<'_, Self>) {
        self.repo = repo;
        if self.repo.is_none() {
            self.refresh_epoch = self.refresh_epoch.wrapping_add(1);
            self.refresh_task = None;
            self.op_task = None;
            self.model.clear_repo();
            cx.notify();
            return;
        }
        if self.model.is_open() {
            self.refresh(cx);
        }
    }

    /// Opens the popup, focuses the filter input, and triggers a background branch scan.
    pub fn open(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.model.open(self.repo.is_some());
        window.focus(&self.focus_handle);
        if self.repo.is_some() {
            self.refresh(cx);
        }
        cx.notify();
    }

    /// Closes the popup and drops transient state.
    pub fn close(&mut self, cx: &mut Context<'_, Self>) {
        self.refresh_epoch = self.refresh_epoch.wrapping_add(1);
        self.refresh_task = None;
        self.op_task = None;
        self.model.close();
        cx.notify();
    }

    /// Loads `GitRepository::branches` on the background executor.
    pub fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            self.model.clear_repo();
            cx.notify();
            return;
        };
        self.refresh_epoch = self.refresh_epoch.wrapping_add(1);
        let epoch = self.refresh_epoch;
        if self.model.branch_list().is_none() && self.model.busy().is_none() {
            self.model.set_busy(Some("Loading branches…"));
        }

        let job = cx
            .background_executor()
            .spawn(async move { repo.branches() });
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ignored = this.update(cx, |popup, cx| {
                if popup.refresh_epoch != epoch {
                    return;
                }
                if popup.model.busy() == Some("Loading branches…") {
                    popup.model.set_busy(None);
                }
                match result {
                    Ok(list) => {
                        popup.model.set_branch_list(list);
                        popup.scroll_selected_into_view();
                    }
                    Err(err) => popup.model.set_error(Some(err.to_string()), None),
                }
                cx.notify();
            });
        }));
    }

    fn check_unsaved_buffers(&self, cx: &App) -> bool {
        if let Some(check) = &self.unsaved_files_check {
            let dirty = check(cx);
            if !dirty.is_empty() {
                return false;
            }
        }
        true
    }

    fn unsaved_buffers_error(&self, cx: &App) -> Option<String> {
        let check = self.unsaved_files_check.as_ref()?;
        let dirty = check(cx);
        if dirty.is_empty() {
            return None;
        }
        Some(format!(
            "Save or discard unsaved editor buffers first: {}",
            dirty.join(", ")
        ))
    }

    /// Checks out a local branch on the background executor.
    pub fn checkout_local(&mut self, name: String, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if self.model.busy().is_some() {
            return;
        }
        if !self.check_unsaved_buffers(cx) {
            let msg = self.unsaved_buffers_error(cx);
            self.model.set_error(msg, None);
            cx.notify();
            return;
        }
        self.model.set_busy(Some("Checking out…"));
        self.model.set_error(None, None);
        cx.notify();

        let job = cx
            .background_executor()
            .spawn(async move { repo.checkout_local(&name) });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ignored = this.update(cx, |popup, cx| {
                popup.model.set_busy(None);
                match result {
                    Ok(res) => {
                        cx.emit(BranchesPopupEvent::BranchChanged {
                            head: res.current,
                            files_changed: res.changed_files,
                        });
                    }
                    Err(err) => {
                        popup.model.set_error(Some(err.to_string()), None);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Creates a tracking local branch for `remote_branch` and checks it out in the background.
    pub fn checkout_remote(&mut self, remote_branch: String, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if self.model.busy().is_some() {
            return;
        }
        if !self.check_unsaved_buffers(cx) {
            let msg = self.unsaved_buffers_error(cx);
            self.model.set_error(msg, None);
            cx.notify();
            return;
        }
        self.model.set_busy(Some("Checking out remote…"));
        self.model.set_error(None, None);
        cx.notify();

        let job = cx
            .background_executor()
            .spawn(async move { repo.checkout_remote(&remote_branch) });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ignored = this.update(cx, |popup, cx| {
                popup.model.set_busy(None);
                match result {
                    Ok(res) => {
                        cx.emit(BranchesPopupEvent::BranchChanged {
                            head: res.current,
                            files_changed: res.changed_files,
                        });
                    }
                    Err(err) => {
                        popup.model.set_error(Some(err.to_string()), None);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Creates a new local branch at `HEAD` and checks it out in the background.
    pub fn create_branch(&mut self, name: String, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if self.model.busy().is_some() {
            return;
        }
        self.model.set_busy(Some("Creating branch…"));
        self.model.set_error(None, None);
        cx.notify();

        let job = cx
            .background_executor()
            .spawn(async move { repo.create_branch_and_checkout(&name) });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ignored = this.update(cx, |popup, cx| {
                popup.model.set_busy(None);
                match result {
                    Ok(head) => {
                        cx.emit(BranchesPopupEvent::BranchChanged {
                            head,
                            files_changed: false,
                        });
                    }
                    Err(err) => {
                        popup.model.set_error(Some(err.to_string()), None);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Deletes local branch `name` in the background.
    pub fn delete_branch(&mut self, name: String, force: bool, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if self.model.busy().is_some() {
            return;
        }
        self.model.set_busy(Some("Deleting branch…"));
        self.model.set_error(None, None);
        cx.notify();

        let branch_for_err = name.clone();
        let job = cx
            .background_executor()
            .spawn(async move { repo.delete_branch(&name, force) });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ignored = this.update(cx, |popup, cx| {
                popup.model.set_busy(None);
                match result {
                    Ok(()) => {
                        popup
                            .model
                            .set_status_message(Some(format!("Deleted branch `{branch_for_err}`")));
                        popup.refresh(cx);
                    }
                    Err(GitError::BranchNotMerged(ref unmerged)) => {
                        popup.model.set_error(
                            Some(format!("Branch `{unmerged}` is not fully merged.")),
                            Some(unmerged.clone()),
                        );
                    }
                    Err(err) => {
                        popup.model.set_error(Some(err.to_string()), None);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Runs `git fetch --all --prune` on the background executor and refreshes the branch list.
    pub fn fetch(&mut self, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if self.model.busy().is_some() {
            return;
        }
        self.model.set_busy(Some("Fetching…"));
        self.model.set_error(None, None);
        self.model.set_status_message(None);
        cx.notify();

        let job = cx.background_executor().spawn(async move { repo.fetch() });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ignored = this.update(cx, |popup, cx| {
                popup.model.set_busy(None);
                match result {
                    Ok(summary) => {
                        popup
                            .model
                            .set_status_message(Some(summary.message.clone()));
                        popup.refresh(cx);
                        cx.emit(BranchesPopupEvent::RemoteUpdated {
                            summary: summary.message,
                        });
                    }
                    Err(err) => {
                        popup.model.set_error(Some(err.to_string()), None);
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Runs `git pull --ff-only` on the background executor and refreshes workspace on change.
    pub fn pull(&mut self, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if self.model.busy().is_some() {
            return;
        }
        if !self.check_unsaved_buffers(cx) {
            let msg = self.unsaved_buffers_error(cx);
            self.model.set_error(msg, None);
            cx.notify();
            return;
        }
        self.model.set_busy(Some("Pulling…"));
        self.model.set_error(None, None);
        self.model.set_status_message(None);
        cx.notify();

        let job = cx.background_executor().spawn(async move { repo.pull() });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ignored = this.update(cx, |popup, cx| {
                popup.model.set_busy(None);
                match result {
                    Ok(summary) => {
                        popup
                            .model
                            .set_status_message(Some(summary.message.clone()));
                        popup.refresh(cx);
                        if summary.changed {
                            cx.emit(BranchesPopupEvent::BranchChanged {
                                head: summary.head,
                                files_changed: true,
                            });
                        } else {
                            cx.emit(BranchesPopupEvent::RemoteUpdated {
                                summary: summary.message,
                            });
                        }
                    }
                    Err(err) => {
                        popup.model.set_error(Some(err.to_string()), None);
                    }
                }
                cx.notify();
            });
        }));
    }

    fn scroll_selected_into_view(&self) {
        if let Some(selected) = self.model.selected_index() {
            self.list_scroll
                .scroll_to_item(selected, ScrollStrategy::Top);
        }
    }

    /// Handles keystrokes forwarded from capture-phase or direct key-down.
    pub(crate) fn handle_keystroke(
        &mut self,
        keystroke: &gpui::Keystroke,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let outcome = self.model.handle_key(keystroke);
        match outcome {
            BranchesKeyOutcome::Ignored => false,
            BranchesKeyOutcome::Consumed => {
                self.scroll_selected_into_view();
                cx.notify();
                true
            }
            BranchesKeyOutcome::Close => {
                cx.emit(BranchesPopupEvent::CloseRequested);
                true
            }
            BranchesKeyOutcome::ActivateSelected(row) => {
                self.activate_row(&row, cx);
                true
            }
            BranchesKeyOutcome::CreateBranch(name) => {
                self.create_branch(name, cx);
                true
            }
            BranchesKeyOutcome::DeleteBranch(name) => {
                self.delete_branch(name, false, cx);
                true
            }
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.handle_keystroke(&event.keystroke, window, cx) {
            cx.stop_propagation();
        }
    }

    fn activate_row(&mut self, row: &BranchesRow, cx: &mut Context<'_, Self>) {
        match row {
            BranchesRow::NewBranchAction => {
                self.model.enter_new_branch_mode();
                cx.notify();
            }
            BranchesRow::LocalBranch(info) => {
                self.checkout_local(info.name.clone(), cx);
            }
            BranchesRow::RemoteBranch(info) => {
                self.checkout_remote(info.name.clone(), cx);
            }
            BranchesRow::SectionHeader { .. } => {}
        }
    }

    fn render_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Stateful<Div>> {
        let theme = cx.global::<KovaTheme>().clone();
        let p = ControlPalette::from_theme(&theme);
        let active_bg = hsla_from_rgba(theme.surfaces().active);
        let selected = self.model.selected_index();
        let rows = self.model.rows();

        range
            .filter_map(|idx| rows.get(idx).map(|row| (idx, row)))
            .map(|(idx, row)| {
                Self::render_single_row(idx, row, selected == Some(idx), active_bg, &p, cx)
            })
            .collect()
    }

    fn render_single_row(
        idx: usize,
        row: &BranchesRow,
        is_selected: bool,
        active_bg: Hsla,
        p: &ControlPalette,
        cx: &Context<'_, Self>,
    ) -> Stateful<Div> {
        match row {
            BranchesRow::SectionHeader { title, count } => div()
                .id(SharedString::from(format!("branches-header-{idx}")))
                .h(px(ROW_HEIGHT_PX))
                .w_full()
                .px(px(12.0))
                .flex()
                .items_center()
                .text_size(px(11.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(p.text_muted)
                .child(format!("{title} ({count})")),
            BranchesRow::NewBranchAction => {
                let mut el = div()
                    .id(SharedString::from(format!("branches-row-{idx}")))
                    .h(px(ROW_HEIGHT_PX))
                    .w_full()
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .cursor_pointer()
                    .hover(|s| s.bg(p.hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.model.select_row(idx);
                        this.model.enter_new_branch_mode();
                        cx.notify();
                    }));
                if is_selected {
                    el = el.bg(active_bg);
                }
                el.child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(FontWeight::BOLD)
                        .text_color(p.primary.subtle_fg)
                        .child("+"),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(p.text)
                        .child("New Branch…"),
                )
            }
            BranchesRow::LocalBranch(info) | BranchesRow::RemoteBranch(info) => {
                Self::render_branch_row(idx, info, is_selected, active_bg, p, cx)
            }
        }
    }

    fn render_branch_row(
        idx: usize,
        info: &BranchInfo,
        is_selected: bool,
        active_bg: Hsla,
        p: &ControlPalette,
        cx: &Context<'_, Self>,
    ) -> Stateful<Div> {
        let is_local = matches!(info.kind, BranchKind::Local);
        let row_for_click = if is_local {
            BranchesRow::LocalBranch(info.clone())
        } else {
            BranchesRow::RemoteBranch(info.clone())
        };

        let mut el = div()
            .id(SharedString::from(format!("branches-row-{idx}")))
            .h(px(ROW_HEIGHT_PX))
            .w_full()
            .px(px(12.0))
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .cursor_pointer()
            .hover(|s| s.bg(p.hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.model.select_row(idx);
                this.activate_row(&row_for_click, cx);
            }));

        if is_selected {
            el = el.bg(active_bg);
        }

        let marker = if info.is_current { "✓" } else { " " };
        let marker_color = if info.is_current {
            p.primary.subtle_fg
        } else {
            p.text_muted
        };

        el.child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .overflow_hidden()
                .child(
                    div()
                        .w(px(12.0))
                        .flex_none()
                        .text_size(px(11.5))
                        .font_weight(FontWeight::BOLD)
                        .text_color(marker_color)
                        .child(marker),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(px(12.5))
                        .font_weight(if info.is_current {
                            FontWeight::SEMIBOLD
                        } else {
                            FontWeight::NORMAL
                        })
                        .text_color(p.text)
                        .child(info.name.clone()),
                ),
        )
        .child(Self::render_branch_meta(idx, info, is_local, p, cx))
    }

    fn render_branch_meta(
        idx: usize,
        info: &BranchInfo,
        is_local: bool,
        p: &ControlPalette,
        cx: &Context<'_, Self>,
    ) -> Div {
        let can_delete = is_local && !info.is_current;
        let ab_label = info.ahead_behind.and_then(|(ahead, behind)| {
            if ahead == 0 && behind == 0 {
                None
            } else {
                let mut parts = Vec::new();
                if ahead > 0 {
                    parts.push(format!("↑{ahead}"));
                }
                if behind > 0 {
                    parts.push(format!("↓{behind}"));
                }
                Some(parts.join(" "))
            }
        });

        div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .children(ab_label.map(|ab| {
                div()
                    .px(px(5.0))
                    .py(px(1.0))
                    .rounded(px(4.0))
                    .bg(p.input)
                    .text_size(px(10.5))
                    .text_color(p.primary.subtle_fg)
                    .child(ab)
            }))
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(p.text_muted)
                    .child(info.target_oid.clone()),
            )
            .when(can_delete, |right| {
                let del_name = info.name.clone();
                right.child(
                    div()
                        .id(SharedString::from(format!("branches-del-{idx}")))
                        .w(px(18.0))
                        .h(px(18.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .text_size(px(12.0))
                        .text_color(p.text_muted)
                        .hover(|s| s.bg(p.danger.subtle_bg).text_color(p.danger.subtle_fg))
                        .cursor_pointer()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.delete_branch(del_name.clone(), false, cx);
                        }))
                        .child("×"),
                )
            })
    }

    fn render_header(
        &self,
        p: &ControlPalette,
        border_subtle: Hsla,
        cx: &Context<'_, Self>,
    ) -> Div {
        let head_badge = self
            .model
            .branch_list()
            .map(|list| format!("⎇ {}", list.head.label()));
        let busy = self.model.busy().is_some();

        div()
            .h(px(38.0))
            .px(px(12.0))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(px(8.0))
            .border_b_1()
            .border_color(border_subtle)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w(px(0.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text)
                            .child("Git Branches"),
                    )
                    .children(head_badge.map(|label| {
                        div()
                            .px(px(6.0))
                            .py(px(1.0))
                            .rounded(px(4.0))
                            .bg(p.input)
                            .border_1()
                            .border_color(border_subtle)
                            .text_size(px(11.0))
                            .text_color(p.text_secondary)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(label)
                    })),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .when(self.repo.is_some(), |bar| {
                        bar.child(
                            Button::ghost(if busy { "…" } else { "Fetch" }, PluginVariant::Muted)
                                .render("branches-fetch", p)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.fetch(cx);
                                })),
                        )
                        .child(
                            Button::ghost("Pull", PluginVariant::Muted)
                                .render("branches-pull", p)
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.pull(cx);
                                })),
                        )
                    })
                    .child(
                        Button::ghost("×", PluginVariant::Muted)
                            .render("branches-close", p)
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                cx.emit(BranchesPopupEvent::CloseRequested);
                            })),
                    ),
            )
    }

    fn render_input_bar(
        input: &TextInputState,
        placeholder: &str,
        p: &ControlPalette,
        border_subtle: Hsla,
    ) -> Div {
        let (before, after) = input.split_at_cursor();
        let mut field = div()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .flex()
            .items_center()
            .text_size(px(12.5))
            .text_color(p.text)
            .child(before.to_owned())
            .child(div().w(px(1.5)).h(px(15.0)).bg(p.caret))
            .child(after.to_owned());
        if input.text().is_empty() {
            field = field.child(
                div()
                    .ml(px(4.0))
                    .text_size(px(12.5))
                    .text_color(p.text_muted)
                    .child(placeholder.to_owned()),
            );
        }

        div()
            .h(px(36.0))
            .px(px(12.0))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.0))
            .bg(p.input)
            .border_b_1()
            .border_color(border_subtle)
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(p.text_muted)
                    .child("⌕"),
            )
            .child(field)
    }

    fn render_new_branch_body(&self, p: &ControlPalette, cx: &Context<'_, Self>) -> Div {
        let base_label = self
            .model
            .branch_list()
            .map_or_else(|| "HEAD".to_owned(), |l| l.head.label().to_owned());
        let (before, after) = self.model.new_branch_input().split_at_cursor();
        let mut field = div()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .flex()
            .items_center()
            .text_size(px(12.5))
            .text_color(p.text)
            .child(before.to_owned())
            .child(div().w(px(1.5)).h(px(15.0)).bg(p.caret))
            .child(after.to_owned());
        if self.model.new_branch_input().text().is_empty() {
            field = field.child(
                div()
                    .ml(px(4.0))
                    .text_size(px(12.5))
                    .text_color(p.text_muted)
                    .child("New branch name…"),
            );
        }

        div()
            .p(px(12.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(p.text_secondary)
                    .child(format!("Create branch from `{base_label}` and checkout:")),
            )
            .child(
                div()
                    .h(px(32.0))
                    .px(px(10.0))
                    .rounded(px(6.0))
                    .bg(p.input)
                    .border_1()
                    .border_color(p.focus)
                    .flex()
                    .items_center()
                    .child(field),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        Button::ghost("Back", PluginVariant::Muted)
                            .render("branches-new-cancel", p)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.model.exit_new_branch_mode();
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("Create", PluginVariant::Primary)
                            .render("branches-new-submit", p)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                let name = this.model.new_branch_input().text().trim().to_owned();
                                if name.is_empty() {
                                    this.model
                                        .set_error(Some("Enter a branch name.".to_owned()), None);
                                    cx.notify();
                                } else {
                                    this.create_branch(name, cx);
                                }
                            })),
                    ),
            )
    }

    fn render_banner(&self, p: &ControlPalette, cx: &Context<'_, Self>) -> Option<Div> {
        if let Some(err) = self.model.error() {
            let unmerged = self.model.unmerged_branch().map(str::to_owned);
            return Some(
                div()
                    .mx(px(10.0))
                    .my(px(6.0))
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(5.0))
                    .bg(p.danger.subtle_bg)
                    .border_1()
                    .border_color(p.danger.border)
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(11.0))
                            .text_color(p.danger.subtle_fg)
                            .child(err.to_owned()),
                    )
                    .children(unmerged.map(|branch| {
                        Button::new("Force Delete", PluginVariant::Error)
                            .render("branches-force-delete", p)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.delete_branch(branch.clone(), true, cx);
                            }))
                    })),
            );
        }
        if let Some(msg) = self.model.busy().or(self.model.status_message()) {
            return Some(
                div()
                    .mx(px(10.0))
                    .my(px(4.0))
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .bg(p.input)
                    .text_size(px(11.0))
                    .text_color(p.text_secondary)
                    .child(msg.to_owned()),
            );
        }
        None
    }

    fn render_browse_body(
        &self,
        p: &ControlPalette,
        border_subtle: Hsla,
        cx: &mut Context<'_, Self>,
    ) -> Div {
        let row_count = self.model.rows().len();
        #[allow(
            clippy::cast_precision_loss,
            reason = "row count in branch list is far below 2^24"
        )]
        let list_h =
            (row_count as f32 * ROW_HEIGHT_PX + 8.0).clamp(LIST_MIN_HEIGHT_PX, LIST_MAX_HEIGHT_PX);
        let list_el = if row_count == 0 {
            div()
                .h(px(LIST_MIN_HEIGHT_PX))
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .text_color(p.text_muted)
                .child("No matching branches")
        } else {
            div().h(px(list_h)).w_full().child(
                uniform_list("branches-list", row_count, cx.processor(Self::render_rows))
                    .track_scroll(self.list_scroll.clone())
                    .size_full()
                    .py(px(4.0)),
            )
        };
        div()
            .flex()
            .flex_col()
            .child(Self::render_input_bar(
                self.model.filter(),
                "Filter branches…",
                p,
                border_subtle,
            ))
            .child(list_el)
    }

    /// Renders the modal Git Branches overlay centered within `viewport_px`.
    pub fn render_modal(
        &self,
        viewport_px: (f32, f32),
        cx: &mut Context<'_, Self>,
    ) -> Stateful<Div> {
        let theme = cx.global::<KovaTheme>().clone();
        let p = ControlPalette::from_theme(&theme);
        let scrim = hsla_from_rgba(theme.surfaces().overlay);
        let elevated = hsla_from_rgba(theme.surfaces().elevated);
        let border_strong = hsla_from_rgba(theme.borders().strong);
        let border_subtle = hsla_from_rgba(theme.borders().subtle);

        let modal_w = (viewport_px.0 - 48.0).clamp(320.0, POPUP_WIDTH_PX);
        let modal_max_h = (viewport_px.1 - 48.0).clamp(240.0, POPUP_MAX_HEIGHT_PX);

        let body = if self.repo.is_none() {
            div()
                .p(px(18.0))
                .text_size(px(12.5))
                .text_color(p.text_muted)
                .child("Open a Git repository to manage branches.")
        } else if self.model.mode() == BranchesMode::NewBranch {
            self.render_new_branch_body(&p, cx)
        } else {
            self.render_browse_body(&p, border_subtle, cx)
        };

        let footer_hints = match self.model.mode() {
            BranchesMode::Browse => "↑↓ Navigate · ↵ Checkout · Del Delete · Esc Close",
            BranchesMode::NewBranch => "↵ Create & Checkout · Esc Back",
        };

        let card = div()
            .id("branches-modal-card")
            .w(px(modal_w))
            .max_h(px(modal_max_h))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(8.0))
            .bg(elevated)
            .border_1()
            .border_color(border_strong)
            .shadow_lg()
            .key_context(BRANCHES_POPUP_KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &CloseBranchesPopup, _, cx| {
                cx.emit(BranchesPopupEvent::CloseRequested);
            }))
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
            })
            .child(self.render_header(&p, border_subtle, cx))
            .child(body)
            .children(self.render_banner(&p, cx))
            .child(
                div()
                    .h(px(26.0))
                    .px(px(12.0))
                    .flex()
                    .flex_none()
                    .items_center()
                    .border_t_1()
                    .border_color(border_subtle)
                    .text_size(px(11.0))
                    .text_color(p.text_muted)
                    .child(footer_hints),
            );

        div()
            .id("branches-modal-backdrop")
            .absolute()
            .inset_0()
            .bg(scrim)
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.emit(BranchesPopupEvent::CloseRequested);
                    cx.stop_propagation();
                }),
            )
            .child(card)
    }
}

impl Render for BranchesPopup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        self.render_modal((f32::from(viewport.width), f32::from(viewport.height)), cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ks(source: &str) -> gpui::Keystroke {
        gpui::Keystroke::parse(source).expect("valid keystroke")
    }

    fn ks_char(source: &str, key_char: &str) -> gpui::Keystroke {
        let mut keystroke = ks(source);
        keystroke.key_char = Some(key_char.to_owned());
        keystroke
    }

    fn local(name: &str, current: bool) -> BranchInfo {
        BranchInfo {
            name: name.to_owned(),
            full_ref: format!("refs/heads/{name}"),
            kind: BranchKind::Local,
            is_current: current,
            target_oid: "abc1234".to_owned(),
            upstream: None,
            ahead_behind: None,
        }
    }

    fn remote(remote: &str, short: &str) -> BranchInfo {
        BranchInfo {
            name: format!("{remote}/{short}"),
            full_ref: format!("refs/remotes/{remote}/{short}"),
            kind: BranchKind::Remote {
                remote: remote.to_owned(),
                short_name: short.to_owned(),
            },
            is_current: false,
            target_oid: "def5678".to_owned(),
            upstream: None,
            ahead_behind: None,
        }
    }

    fn sample_list() -> BranchList {
        BranchList {
            head: HeadState::Branch("main".to_owned()),
            local: vec![
                local("main", true),
                local("feat/ui", false),
                local("fix/bug", false),
            ],
            remote: vec![remote("origin", "main"), remote("origin", "release/1.0")],
        }
    }

    #[test]
    fn builds_sections_and_skips_headers_on_up_down_navigation() {
        let mut model = BranchesPopupModel::new();
        model.set_branch_list(sample_list());
        model.open(true);

        // Row 0: + New Branch..., Row 1: Local (3), Row 2: main, Row 3: feat/ui, Row 4: fix/bug,
        // Row 5: Remote (2), Row 6: origin/main, Row 7: origin/release/1.0
        assert_eq!(model.rows().len(), 8);
        assert_eq!(model.selected_index(), Some(0));
        assert!(matches!(
            model.selected_row(),
            Some(BranchesRow::NewBranchAction)
        ));

        // Down skips Local (3) header and lands on `main`.
        assert_eq!(model.handle_key(&ks("down")), BranchesKeyOutcome::Consumed);
        assert_eq!(model.selected_index(), Some(2));
        assert!(matches!(
            model.selected_row(),
            Some(BranchesRow::LocalBranch(b)) if b.name == "main"
        ));

        // Down through feat/ui -> fix/bug -> skips Remote (2) header -> origin/main.
        model.handle_key(&ks("down"));
        model.handle_key(&ks("down"));
        model.handle_key(&ks("down"));
        assert_eq!(model.selected_index(), Some(6));
        assert!(matches!(
            model.selected_row(),
            Some(BranchesRow::RemoteBranch(b)) if b.name == "origin/main"
        ));

        // Up skips Remote (2) header back to fix/bug.
        model.handle_key(&ks("up"));
        assert_eq!(model.selected_index(), Some(4));
        assert!(matches!(
            model.selected_row(),
            Some(BranchesRow::LocalBranch(b)) if b.name == "fix/bug"
        ));
    }

    #[test]
    fn typing_filters_branches_and_auto_selects_first_match() {
        let mut model = BranchesPopupModel::new();
        model.set_branch_list(sample_list());
        model.open(true);

        for ch in ["r", "e", "l"] {
            assert_eq!(
                model.handle_key(&ks_char(ch, ch)),
                BranchesKeyOutcome::Consumed
            );
        }
        assert_eq!(model.filter().text(), "rel");
        // + New Branch..., Remote (1), origin/release/1.0
        assert_eq!(model.rows().len(), 3);
        assert_eq!(model.selected_index(), Some(2));
        assert_eq!(
            model.handle_key(&ks("enter")),
            BranchesKeyOutcome::ActivateSelected(BranchesRow::RemoteBranch(remote(
                "origin",
                "release/1.0"
            )))
        );

        // Fuzzy subsequence match (`fui` -> `feat/ui`).
        model.set_filter("fui");
        assert!(matches!(
            model.selected_row(),
            Some(BranchesRow::LocalBranch(b)) if b.name == "feat/ui"
        ));
    }

    #[test]
    fn new_branch_mode_delete_and_escape_behave_cleanly() {
        let mut model = BranchesPopupModel::new();
        model.set_branch_list(sample_list());
        model.open(true);

        // Enter on + New Branch... enters NewBranch mode.
        assert_eq!(model.handle_key(&ks("enter")), BranchesKeyOutcome::Consumed);
        assert_eq!(model.mode(), BranchesMode::NewBranch);

        // Esc in NewBranch returns to Browse instead of closing popup.
        assert_eq!(
            model.handle_key(&ks("escape")),
            BranchesKeyOutcome::Consumed
        );
        assert_eq!(model.mode(), BranchesMode::Browse);

        // Re-enter NewBranch, type name, Enter emits CreateBranch.
        model.handle_key(&ks("enter"));
        for ch in ["f", "o", "o"] {
            model.handle_key(&ks_char(ch, ch));
        }
        assert_eq!(
            model.handle_key(&ks("enter")),
            BranchesKeyOutcome::CreateBranch("foo".to_owned())
        );

        // Back in Browse mode: Delete/Backspace on empty filter deletes non-current local branch,
        // never deletes current branch.
        model.exit_new_branch_mode();
        model.handle_key(&ks("down")); // main (current)
        assert_eq!(
            model.handle_key(&ks("backspace")),
            BranchesKeyOutcome::Ignored
        );
        model.handle_key(&ks("down")); // feat/ui (not current)
        assert_eq!(
            model.handle_key(&ks("delete")),
            BranchesKeyOutcome::DeleteBranch("feat/ui".to_owned())
        );

        // Esc in Browse closes popup.
        assert_eq!(model.handle_key(&ks("escape")), BranchesKeyOutcome::Close);
    }
}
