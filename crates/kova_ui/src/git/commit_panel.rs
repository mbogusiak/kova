//! `Cmd+K` Commit panel (`CommitPanel`, headless [`CommitPanelModel`]): changed files list
//! with stage/unstage checkboxes, commit message editor, *Commit* button, and selected file
//! unified diff preview.
//!
//! All `gix` operations (`status_cancellable`, `file_diff`, `stage`, `unstage`, `commit`)
//! run on GPUI's background executor so the UI thread never blocks.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{
    ClickEvent, Context, Div, EventEmitter, FocusHandle, Font, FontFallbacks, FontFeatures,
    FontStyle, FontWeight, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, ParentElement as _, Render, ScrollStrategy, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, UniformListScrollHandle, Window, div, px,
    uniform_list,
};
use kova_theme::PluginVariant;
use kova_tools::git::{
    ChangeKind, CommitSummary, DiffLine, DiffLineKind, FileStatus, GitRepository,
};

use super::{COMMIT_PANEL_KEY_CONTEXT, CloseCommitPanel, CommitStaged};
use crate::controls::{Button, ControlPalette};
use crate::theme::{KovaTheme, hsla_from_rgba};

const MODAL_MAX_WIDTH_PX: f32 = 880.0;
const MODAL_MAX_HEIGHT_PX: f32 = 580.0;
const MODAL_MARGIN_PX: f32 = 36.0;
const LEFT_COLUMN_WIDTH_PX: f32 = 320.0;
const FILE_ROW_HEIGHT_PX: f32 = 26.0;
const DIFF_ROW_HEIGHT_PX: f32 = 19.0;
const MONO_FONT: &str = "JetBrains Mono";

/// Checkbox state of a changed file in the Commit panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckState {
    /// No staged changes for this path.
    Unchecked,
    /// All working-tree changes are staged.
    Checked,
    /// Staged changes exist alongside newer unstaged changes.
    Partial,
}

impl CheckState {
    /// Derives the checkbox state from a repository [`FileStatus`].
    #[must_use]
    pub const fn from_status(status: &FileStatus) -> Self {
        match (status.staged.is_some(), status.unstaged.is_some()) {
            (true, false) => Self::Checked,
            (true, true) => Self::Partial,
            (false, _) => Self::Unchecked,
        }
    }

    /// Compact ASCII glyph rendered inside the checkbox square.
    #[must_use]
    pub const fn glyph(self) -> &'static str {
        match self {
            Self::Unchecked => "",
            Self::Checked => "✓",
            Self::Partial => "–",
        }
    }
}

/// Index mutation requested by toggling a file checkbox or bulk stage button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageOp {
    /// Stage the working-tree version of the given repository-relative path.
    Stage(String),
    /// Reset the index entry of the given repository-relative path back to `HEAD`.
    Unstage(String),
}

impl StageOp {
    /// Returns the repository-relative path targeted by this operation.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Stage(path) | Self::Unstage(path) => path,
        }
    }
}

/// Events emitted by [`CommitPanel`] to [`crate::shell::WorkspaceShell`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitPanelEvent {
    /// User pressed `Escape` or clicked Close (`×`).
    CloseRequested,
    /// A commit succeeded on `HEAD`.
    Committed(CommitSummary),
    /// Repository branch or changed-file count refreshed.
    StatusUpdated {
        /// Current branch or short detached SHA.
        branch: String,
        /// Number of changed files in the working tree / index.
        changed_count: usize,
    },
    /// User requested opening a changed file in the editor.
    OpenFile(String),
}

/// Keyboard focus target inside the Commit panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommitFocus {
    /// Changed files list.
    Files,
    /// Multi-line commit message box.
    #[default]
    Message,
}

/// Headless multi-line commit message editor (character-indexed caret).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommitMessageState {
    text: String,
    cursor_char: usize,
}

impl CommitMessageState {
    /// Creates an empty message editor.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current commit message text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Caret position in Unicode scalar values.
    #[must_use]
    pub const fn cursor_char(&self) -> usize {
        self.cursor_char
    }

    /// Replaces the message text and moves the caret to the end.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor_char = self.text.chars().count();
    }

    /// Clears the message text.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor_char = 0;
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map_or(self.text.len(), |(idx, _)| idx)
    }

    /// Inserts typed text (filtering control characters other than `\n`).
    pub fn insert(&mut self, text: &str) -> bool {
        let filtered: String = text
            .chars()
            .filter(|&c| c == '\n' || !c.is_control())
            .collect();
        if filtered.is_empty() {
            return false;
        }
        let at = self.byte_at(self.cursor_char);
        self.text.insert_str(at, &filtered);
        self.cursor_char += filtered.chars().count();
        true
    }

    /// Handles a named navigation or editing key (`enter`, `backspace`, `delete`, `left`,
    /// `right`, `home`, `end`).
    pub fn handle_named_key(&mut self, key: &str) -> bool {
        let len = self.text.chars().count();
        match key {
            "enter" => self.insert("\n"),
            "backspace" if self.cursor_char > 0 => {
                let start = self.byte_at(self.cursor_char - 1);
                let end = self.byte_at(self.cursor_char);
                self.text.replace_range(start..end, "");
                self.cursor_char -= 1;
                true
            }
            "delete" if self.cursor_char < len => {
                let start = self.byte_at(self.cursor_char);
                let end = self.byte_at(self.cursor_char + 1);
                self.text.replace_range(start..end, "");
                true
            }
            "left" if self.cursor_char > 0 => {
                self.cursor_char -= 1;
                true
            }
            "right" if self.cursor_char < len => {
                self.cursor_char += 1;
                true
            }
            "home" => {
                self.cursor_char = 0;
                true
            }
            "end" => {
                self.cursor_char = len;
                true
            }
            _ => false,
        }
    }

    /// Returns `(before_caret, after_caret)` slices for rendering.
    #[must_use]
    pub fn split_at_cursor(&self) -> (&str, &str) {
        self.text.split_at(self.byte_at(self.cursor_char))
    }
}

/// Pure headless state model of the Commit panel (`Cmd+K`).
#[derive(Debug, Clone, Default)]
pub struct CommitPanelModel {
    branch: String,
    entries: Vec<FileStatus>,
    selected: usize,
    diff_path: Option<String>,
    diff_lines: Arc<[DiffLine]>,
    message: CommitMessageState,
    focus: CommitFocus,
    busy: bool,
    error: Option<String>,
}

impl CommitPanelModel {
    /// Creates an empty Commit panel model.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current branch label.
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Changed files reported by `gix` status.
    #[must_use]
    pub fn entries(&self) -> &[FileStatus] {
        &self.entries
    }

    /// Index of the selected changed file.
    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.selected
    }

    /// Currently selected file status, if any.
    #[must_use]
    pub fn selected_file(&self) -> Option<&FileStatus> {
        self.entries.get(self.selected)
    }

    /// Repository-relative path of the selected file.
    #[must_use]
    pub fn selected_path(&self) -> Option<&str> {
        self.selected_file().map(|f| f.path.as_str())
    }

    /// Path whose diff is currently loaded in [`Self::diff_lines`].
    #[must_use]
    pub fn diff_path(&self) -> Option<&str> {
        self.diff_path.as_deref()
    }

    /// Unified diff preview lines for the selected file.
    #[must_use]
    pub fn diff_lines(&self) -> &[DiffLine] {
        &self.diff_lines
    }

    /// Commit message editor state.
    #[must_use]
    pub const fn message(&self) -> &CommitMessageState {
        &self.message
    }

    /// Mutable commit message editor state.
    pub const fn message_mut(&mut self) -> &mut CommitMessageState {
        &mut self.message
    }

    /// Current keyboard focus region inside the panel.
    #[must_use]
    pub const fn focus(&self) -> CommitFocus {
        self.focus
    }

    /// Sets keyboard focus region inside the panel.
    pub const fn set_focus(&mut self, focus: CommitFocus) {
        self.focus = focus;
    }

    /// Whether a background git operation is running.
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        self.busy
    }

    /// Sets the busy flag.
    pub const fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    /// Last error message shown in the panel, if any.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Sets or clears the error message.
    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    /// Replaces the branch label and changed file list, keeping selection anchored on the
    /// previously selected path when it still exists.
    pub fn set_status(&mut self, branch: String, entries: Vec<FileStatus>) {
        let prev_path = self.selected_path().map(str::to_owned);
        self.branch = branch;
        self.entries = entries;
        if self.entries.is_empty() {
            self.selected = 0;
            self.diff_path = None;
            self.diff_lines = Arc::from(Vec::new());
            return;
        }
        if let Some(prev) = prev_path
            && let Some(idx) = self.entries.iter().position(|e| e.path == prev)
        {
            self.selected = idx;
        } else {
            self.selected = self.selected.min(self.entries.len().saturating_sub(1));
        }
        if self.diff_path.as_deref() != self.selected_path() {
            self.diff_path = None;
            self.diff_lines = Arc::from(Vec::new());
        }
    }

    /// Selects entry `index` and returns its path when valid.
    pub fn select_index(&mut self, index: usize) -> Option<&str> {
        if index >= self.entries.len() {
            return None;
        }
        self.selected = index;
        if self.diff_path.as_deref() != self.selected_path() {
            self.diff_path = None;
            self.diff_lines = Arc::from(Vec::new());
        }
        self.selected_path()
    }

    /// Moves the file selection up (`down = false`) or down (`down = true`).
    pub fn move_selection(&mut self, down: bool) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        let next = if down {
            (self.selected + 1).min(self.entries.len() - 1)
        } else {
            self.selected.saturating_sub(1)
        };
        self.select_index(next)
    }

    /// Checkbox state of entry `index`.
    #[must_use]
    pub fn check_state(&self, index: usize) -> Option<CheckState> {
        self.entries.get(index).map(CheckState::from_status)
    }

    /// Number of files with staged changes.
    #[must_use]
    pub fn staged_count(&self) -> usize {
        self.entries.iter().filter(|e| e.staged.is_some()).count()
    }

    /// Whether a commit can currently be submitted (not busy, >= 1 staged file, non-empty
    /// message).
    #[must_use]
    pub fn can_commit(&self) -> bool {
        !self.busy && self.staged_count() > 0 && !self.message.text().trim().is_empty()
    }

    /// Returns the stage/unstage operation needed to toggle entry `index`.
    #[must_use]
    pub fn toggle_op_at(&self, index: usize) -> Option<StageOp> {
        let entry = self.entries.get(index)?;
        Some(match CheckState::from_status(entry) {
            CheckState::Checked => StageOp::Unstage(entry.path.clone()),
            CheckState::Unchecked | CheckState::Partial => StageOp::Stage(entry.path.clone()),
        })
    }

    /// Operations needed to stage all files that still have unstaged changes.
    #[must_use]
    pub fn stage_all_ops(&self) -> Vec<StageOp> {
        self.entries
            .iter()
            .filter(|e| e.unstaged.is_some())
            .map(|e| StageOp::Stage(e.path.clone()))
            .collect()
    }

    /// Operations needed to unstage all files that currently have staged changes.
    #[must_use]
    pub fn unstage_all_ops(&self) -> Vec<StageOp> {
        self.entries
            .iter()
            .filter(|e| e.staged.is_some())
            .map(|e| StageOp::Unstage(e.path.clone()))
            .collect()
    }

    /// Stores the diff preview for `path` (if `path` still matches the selected file).
    pub fn set_diff(&mut self, path: String, lines: Vec<DiffLine>) {
        if self.selected_path() == Some(path.as_str()) {
            self.diff_path = Some(path);
            self.diff_lines = Arc::from(lines);
        }
    }

    /// Clears the loaded diff preview.
    pub fn clear_diff(&mut self) {
        self.diff_path = None;
        self.diff_lines = Arc::from(Vec::new());
    }
}

/// Interactive `Cmd+K` Commit panel GPUI entity.
pub struct CommitPanel {
    repo: Option<Arc<GitRepository>>,
    model: CommitPanelModel,
    focus_handle: FocusHandle,
    list_scroll: UniformListScrollHandle,
    diff_scroll: UniformListScrollHandle,
    refresh_epoch: u64,
    diff_epoch: u64,
    refresh_cancel: Option<Arc<AtomicBool>>,
    refresh_task: Option<Task<()>>,
    diff_task: Option<Task<()>>,
    op_task: Option<Task<()>>,
}

impl std::fmt::Debug for CommitPanel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommitPanel")
            .field("has_repo", &self.repo.is_some())
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl EventEmitter<CommitPanelEvent> for CommitPanel {}

impl CommitPanel {
    /// Creates a new Commit panel entity.
    #[must_use]
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        Self {
            repo: None,
            model: CommitPanelModel::new(),
            focus_handle: cx.focus_handle(),
            list_scroll: UniformListScrollHandle::new(),
            diff_scroll: UniformListScrollHandle::new(),
            refresh_epoch: 0,
            diff_epoch: 0,
            refresh_cancel: None,
            refresh_task: None,
            diff_task: None,
            op_task: None,
        }
    }

    /// Returns the panel focus handle.
    #[must_use]
    pub const fn focus_handle(&self) -> &FocusHandle {
        &self.focus_handle
    }

    /// Returns the underlying headless model.
    #[must_use]
    pub const fn model(&self) -> &CommitPanelModel {
        &self.model
    }

    /// Focuses the commit message field (default when `Cmd+K` opens).
    pub fn focus_message(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.model.set_focus(CommitFocus::Message);
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Attaches a discovered repository (or clears it when `None`) and schedules a refresh.
    pub fn attach_repo(&mut self, repo: Option<Arc<GitRepository>>, cx: &mut Context<'_, Self>) {
        self.repo = repo;
        if self.repo.is_none() {
            self.cancel_refresh();
            self.model.set_status(String::new(), Vec::new());
            self.model.set_error(None);
            cx.notify();
            return;
        }
        self.refresh(cx);
    }

    fn cancel_refresh(&mut self) {
        if let Some(cancel) = self.refresh_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.refresh_task = None;
    }

    /// Cancels any stale status scan and computes branch + working-tree status on the
    /// background executor.
    pub fn refresh(&mut self, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.cancel_refresh();
        self.refresh_epoch = self.refresh_epoch.wrapping_add(1);
        let epoch = self.refresh_epoch;
        let cancel = Arc::new(AtomicBool::new(false));
        self.refresh_cancel = Some(Arc::clone(&cancel));

        let scan = cx.background_executor().spawn(async move {
            let head = repo.head()?;
            let entries = repo.status_cancellable(&cancel)?;
            Ok::<_, kova_tools::git::GitError>((head.label().to_owned(), entries))
        });

        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = scan.await;
            let _ignored = this.update(cx, |panel, cx| {
                if panel.refresh_epoch != epoch {
                    return;
                }
                panel.refresh_cancel = None;
                match result {
                    Ok((branch, entries)) => {
                        let changed_count = entries.len();
                        panel.model.set_status(branch.clone(), entries);
                        panel.model.set_error(None);
                        panel.load_selected_diff(cx);
                        cx.emit(CommitPanelEvent::StatusUpdated {
                            branch,
                            changed_count,
                        });
                    }
                    Err(kova_tools::git::GitError::Cancelled) => {}
                    Err(err) => {
                        panel.model.set_error(Some(err.to_string()));
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Loads the unified diff preview for the currently selected file on the background
    /// executor.
    pub fn load_selected_diff(&mut self, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            self.model.clear_diff();
            return;
        };
        let Some(path) = self.model.selected_path().map(str::to_owned) else {
            self.model.clear_diff();
            return;
        };
        self.diff_epoch = self.diff_epoch.wrapping_add(1);
        let epoch = self.diff_epoch;
        let path_for_task = path.clone();
        let diff_job = cx
            .background_executor()
            .spawn(async move { repo.file_diff(&path_for_task) });

        self.diff_task = Some(cx.spawn(async move |this, cx| {
            let result = diff_job.await;
            let _ignored = this.update(cx, |panel, cx| {
                if panel.diff_epoch != epoch {
                    return;
                }
                match result {
                    Ok(lines) => panel.model.set_diff(path, lines),
                    Err(err) => panel.model.set_error(Some(err.to_string())),
                }
                cx.notify();
            });
        }));
    }

    /// Selects row `index` in the changed files list and loads its diff preview.
    pub fn select_row(&mut self, index: usize, cx: &mut Context<'_, Self>) {
        if self.model.select_index(index).is_some() {
            self.list_scroll.scroll_to_item(index, ScrollStrategy::Top);
            self.load_selected_diff(cx);
            cx.notify();
        }
    }

    /// Toggles the stage checkbox of entry `index` off the UI thread.
    pub fn toggle_row(&mut self, index: usize, cx: &mut Context<'_, Self>) {
        if let Some(op) = self.model.toggle_op_at(index) {
            self.apply_stage_ops(vec![op], cx);
        }
    }

    /// Applies a single [`StageOp`] on the background executor.
    pub fn apply_stage_op(&mut self, op: StageOp, cx: &mut Context<'_, Self>) {
        self.apply_stage_ops(vec![op], cx);
    }

    /// Applies one or more [`StageOp`]s sequentially on the background executor, then
    /// refreshes status.
    pub fn apply_stage_ops(&mut self, ops: Vec<StageOp>, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if ops.is_empty() || self.model.is_busy() {
            return;
        }
        self.model.set_busy(true);
        self.model.set_error(None);
        cx.notify();

        let job = cx.background_executor().spawn(async move {
            for op in ops {
                match op {
                    StageOp::Stage(path) => repo.stage(&path)?,
                    StageOp::Unstage(path) => repo.unstage(&path)?,
                }
            }
            Ok::<_, kova_tools::git::GitError>(())
        });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ignored = this.update(cx, |panel, cx| {
                panel.model.set_busy(false);
                if let Err(err) = result {
                    panel.model.set_error(Some(err.to_string()));
                }
                panel.refresh(cx);
                cx.notify();
            });
        }));
    }

    /// Stages all unstaged changes.
    pub fn stage_all(&mut self, cx: &mut Context<'_, Self>) {
        let ops = self.model.stage_all_ops();
        self.apply_stage_ops(ops, cx);
    }

    /// Unstages all staged changes.
    pub fn unstage_all(&mut self, cx: &mut Context<'_, Self>) {
        let ops = self.model.unstage_all_ops();
        self.apply_stage_ops(ops, cx);
    }

    /// Commits staged changes with the current commit message on the background executor.
    pub fn commit_staged(&mut self, cx: &mut Context<'_, Self>) {
        let Some(repo) = self.repo.clone() else {
            self.model
                .set_error(Some("No Git repository open".to_owned()));
            cx.notify();
            return;
        };
        if self.model.is_busy() {
            return;
        }
        let message = self.model.message().text().trim().to_owned();
        if message.is_empty() {
            self.model
                .set_error(Some("Enter a commit message".to_owned()));
            cx.notify();
            return;
        }
        if self.model.staged_count() == 0 {
            self.model
                .set_error(Some("Select at least one file to commit".to_owned()));
            cx.notify();
            return;
        }
        self.model.set_busy(true);
        self.model.set_error(None);
        cx.notify();

        let job = cx
            .background_executor()
            .spawn(async move { repo.commit(&message, None) });

        self.op_task = Some(cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ignored = this.update(cx, |panel, cx| {
                panel.model.set_busy(false);
                match result {
                    Ok(summary) => {
                        panel.model.message_mut().clear();
                        panel.model.set_error(None);
                        panel.refresh(cx);
                        cx.emit(CommitPanelEvent::Committed(summary));
                    }
                    Err(err) => {
                        panel.model.set_error(Some(err.to_string()));
                    }
                }
                cx.notify();
            });
        }));
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let keystroke = &event.keystroke;
        let mods = keystroke.modifiers;
        let key = keystroke.key.as_str();

        if key == "escape" {
            cx.emit(CommitPanelEvent::CloseRequested);
            cx.stop_propagation();
            return;
        }
        if (mods.platform || mods.control) && key == "enter" {
            self.commit_staged(cx);
            cx.stop_propagation();
            return;
        }
        if key == "tab" {
            let next = match self.model.focus() {
                CommitFocus::Message => CommitFocus::Files,
                CommitFocus::Files => CommitFocus::Message,
            };
            self.model.set_focus(next);
            cx.stop_propagation();
            cx.notify();
            return;
        }

        if mods.platform || mods.control || mods.alt {
            return;
        }

        let handled = match self.model.focus() {
            CommitFocus::Files => self.handle_files_key(key, cx),
            CommitFocus::Message => {
                let text = keystroke.key_char.as_deref();
                let edited = match text {
                    Some(t)
                        if !t.is_empty() && key != "enter" && !t.chars().any(char::is_control) =>
                    {
                        self.model.message_mut().insert(t)
                    }
                    _ => self.model.message_mut().handle_named_key(key),
                };
                if edited {
                    self.model.set_error(None);
                }
                edited
            }
        };
        if handled {
            cx.stop_propagation();
            cx.notify();
        }
    }

    fn handle_files_key(&mut self, key: &str, cx: &mut Context<'_, Self>) -> bool {
        match key {
            "up" | "k" => {
                if self.model.move_selection(false).is_some() {
                    let selected = self.model.selected_index();
                    self.list_scroll
                        .scroll_to_item(selected, ScrollStrategy::Top);
                    self.load_selected_diff(cx);
                    return true;
                }
                false
            }
            "down" | "j" => {
                if self.model.move_selection(true).is_some() {
                    let selected = self.model.selected_index();
                    self.list_scroll
                        .scroll_to_item(selected, ScrollStrategy::Top);
                    self.load_selected_diff(cx);
                    return true;
                }
                false
            }
            "space" => {
                let selected = self.model.selected_index();
                self.toggle_row(selected, cx);
                true
            }
            "enter" => {
                if let Some(path) = self.model.selected_path().map(str::to_owned) {
                    cx.emit(CommitPanelEvent::OpenFile(path));
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn change_color(kind: ChangeKind, theme: &KovaTheme) -> Hsla {
        match kind {
            ChangeKind::Added | ChangeKind::Untracked => hsla_from_rgba(theme.gutter().git_added),
            ChangeKind::Modified | ChangeKind::Renamed => {
                hsla_from_rgba(theme.gutter().git_modified)
            }
            ChangeKind::Deleted | ChangeKind::Conflicted => {
                hsla_from_rgba(theme.gutter().git_deleted)
            }
        }
    }

    fn render_file_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<gpui::Stateful<Div>> {
        let theme = cx.global::<KovaTheme>().clone();
        let p = ControlPalette::from_theme(&theme);
        let active_bg = hsla_from_rgba(theme.surfaces().active);
        let selected = self.model.selected_index();
        let entries = self.model.entries();

        range
            .filter_map(|idx| entries.get(idx).map(|entry| (idx, entry)))
            .map(|(idx, entry)| {
                let check = CheckState::from_status(entry);
                let is_selected = idx == selected;
                let kind = entry.display_kind();
                let kind_color = Self::change_color(kind, &theme);
                let path_label = entry.path.clone();
                let (box_bg, box_border) = match check {
                    CheckState::Checked | CheckState::Partial => {
                        (p.primary.background, p.primary.border)
                    }
                    CheckState::Unchecked => (p.input, p.border),
                };

                div()
                    .id(SharedString::from(format!("commit-file-{idx}")))
                    .h(px(FILE_ROW_HEIGHT_PX))
                    .w_full()
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(4.0))
                    .when_bg(is_selected, active_bg)
                    .hover(|s| s.bg(p.hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.model.set_focus(CommitFocus::Files);
                        this.select_row(idx, cx);
                    }))
                    .child(
                        div()
                            .id(SharedString::from(format!("commit-check-{idx}")))
                            .size(px(15.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(3.0))
                            .border_1()
                            .border_color(box_border)
                            .bg(box_bg)
                            .text_size(px(10.0))
                            .font_weight(FontWeight::BOLD)
                            .text_color(p.primary.foreground)
                            .cursor_pointer()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.toggle_row(idx, cx);
                            }))
                            .child(check.glyph()),
                    )
                    .child(
                        div()
                            .w(px(12.0))
                            .flex_none()
                            .text_size(px(11.0))
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
                            .text_color(p.text)
                            .child(path_label),
                    )
            })
            .collect()
    }

    fn render_diff_rows(
        &mut self,
        range: Range<usize>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Vec<Div> {
        let theme = cx.global::<KovaTheme>().clone();
        let text_primary = hsla_from_rgba(theme.editor().foreground);
        let text_muted = hsla_from_rgba(theme.text().muted);
        let added_color = hsla_from_rgba(theme.gutter().git_added);
        let mut added_tint = added_color;
        added_tint.a = 0.14;
        let removed_color = hsla_from_rgba(theme.gutter().git_deleted);
        let mut removed_tint = removed_color;
        removed_tint.a = 0.16;
        let font = mono_font();
        let lines = self.model.diff_lines();

        range
            .filter_map(|idx| lines.get(idx).map(|line| (idx, line)))
            .map(|(idx, line)| {
                let (prefix, bg, prefix_fg, text_fg) = match line.kind {
                    DiffLineKind::Context => {
                        (" ", gpui::transparent_black(), text_muted, text_primary)
                    }
                    DiffLineKind::Added => ("+", added_tint, added_color, text_primary),
                    DiffLineKind::Removed => ("-", removed_tint, removed_color, text_primary),
                };
                div()
                    .h(px(DIFF_ROW_HEIGHT_PX))
                    .w_full()
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .bg(bg)
                    .font(font.clone())
                    .text_size(px(12.0))
                    .child(
                        div()
                            .w(px(28.0))
                            .flex_none()
                            .text_size(px(10.0))
                            .text_color(text_muted)
                            .child(format!("{}", idx + 1)),
                    )
                    .child(
                        div()
                            .w(px(12.0))
                            .flex_none()
                            .font_weight(FontWeight::BOLD)
                            .text_color(prefix_fg)
                            .child(prefix),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_color(text_fg)
                            .child(line.text.clone()),
                    )
            })
            .collect()
    }

    fn render_message_box(
        &self,
        p: &ControlPalette,
        cx: &Context<'_, Self>,
    ) -> gpui::Stateful<Div> {
        let focused = self.model.focus() == CommitFocus::Message;
        let msg = self.model.message();
        let border = if focused { p.focus } else { p.border };

        let content = if msg.text().is_empty() && !focused {
            div()
                .text_size(px(12.0))
                .text_color(p.text_muted)
                .child("Commit message (Cmd+Enter to commit)")
        } else {
            let (before, after) = msg.split_at_cursor();
            let before_lines: Vec<&str> = before.split('\n').collect();
            let after_lines: Vec<&str> = after.split('\n').collect();
            let split_idx = before_lines.len().saturating_sub(1);
            let mut lines_col = div().w_full().flex().flex_col().gap(px(2.0));

            for (i, &line) in before_lines.iter().enumerate() {
                if i < split_idx {
                    lines_col = lines_col.child(
                        div()
                            .min_h(px(16.0))
                            .text_size(px(12.0))
                            .text_color(p.text)
                            .child(line.to_owned()),
                    );
                } else {
                    let tail = after_lines.first().copied().unwrap_or_default();
                    let mut row = div()
                        .min_h(px(16.0))
                        .flex()
                        .items_center()
                        .text_size(px(12.0))
                        .text_color(p.text)
                        .child(line.to_owned());
                    if focused {
                        row = row.child(div().w(px(1.5)).h(px(14.0)).bg(p.caret));
                    }
                    row = row.child(tail.to_owned());
                    lines_col = lines_col.child(row);
                }
            }
            for &line in after_lines.iter().skip(1) {
                lines_col = lines_col.child(
                    div()
                        .min_h(px(16.0))
                        .text_size(px(12.0))
                        .text_color(p.text)
                        .child(line.to_owned()),
                );
            }
            lines_col
        };

        div()
            .id("commit-message-box")
            .w_full()
            .h(px(92.0))
            .p(px(8.0))
            .overflow_hidden()
            .rounded(px(6.0))
            .bg(p.input)
            .border_1()
            .border_color(border)
            .cursor_text()
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.focus_message(window, cx);
            }))
            .child(content)
    }

    /// Renders the modal Commit overlay centered within `viewport_px`.
    pub fn render_modal(
        &self,
        viewport_px: (f32, f32),
        cx: &mut Context<'_, Self>,
    ) -> gpui::Stateful<Div> {
        let theme = cx.global::<KovaTheme>().clone();
        let p = ControlPalette::from_theme(&theme);
        let scrim = hsla_from_rgba(theme.surfaces().overlay);
        let elevated = hsla_from_rgba(theme.surfaces().elevated);
        let island = hsla_from_rgba(theme.surfaces().island);
        let border_strong = hsla_from_rgba(theme.borders().strong);
        let border_subtle = hsla_from_rgba(theme.borders().subtle);

        let modal_w = (viewport_px.0 - MODAL_MARGIN_PX * 2.0).clamp(480.0, MODAL_MAX_WIDTH_PX);
        let modal_h = (viewport_px.1 - MODAL_MARGIN_PX * 2.0).clamp(320.0, MODAL_MAX_HEIGHT_PX);

        let card = div()
            .id("commit-modal-card")
            .w(px(modal_w))
            .h(px(modal_h))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(10.0))
            .bg(elevated)
            .border_1()
            .border_color(border_strong)
            .key_context(COMMIT_PANEL_KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &CloseCommitPanel, _, cx| {
                cx.emit(CommitPanelEvent::CloseRequested);
            }))
            .on_action(cx.listener(|this, _: &CommitStaged, _, cx| {
                this.commit_staged(cx);
            }))
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
            })
            .child(self.render_modal_header(&p, border_subtle, cx))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .min_h(px(0.0))
                    .flex()
                    .flex_row()
                    .child(self.render_changes_column(&p, island, border_subtle, cx))
                    .child(self.render_diff_column(&p, border_subtle, cx)),
            );

        div()
            .id("commit-modal-backdrop")
            .absolute()
            .inset_0()
            .bg(scrim)
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &MouseDownEvent, _, cx| {
                    cx.emit(CommitPanelEvent::CloseRequested);
                    cx.stop_propagation();
                }),
            )
            .child(card)
    }

    fn render_modal_header(
        &self,
        p: &ControlPalette,
        border_subtle: Hsla,
        cx: &Context<'_, Self>,
    ) -> Div {
        let branch_label = if self.model.branch().is_empty() {
            "No repository".to_owned()
        } else {
            format!("⎇ {}", self.model.branch())
        };
        div()
            .h(px(38.0))
            .px(px(14.0))
            .flex()
            .flex_none()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(border_subtle)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text)
                            .child("Commit Changes"),
                    )
                    .child(
                        div()
                            .px(px(8.0))
                            .py(px(2.0))
                            .rounded(px(4.0))
                            .bg(p.input)
                            .border_1()
                            .border_color(border_subtle)
                            .text_size(px(11.0))
                            .text_color(p.text_secondary)
                            .child(branch_label),
                    ),
            )
            .child(
                Button::ghost("Esc ×", PluginVariant::Muted)
                    .render("commit-close", p)
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                        cx.emit(CommitPanelEvent::CloseRequested);
                    })),
            )
    }

    fn render_changes_column(
        &self,
        p: &ControlPalette,
        island: Hsla,
        border_subtle: Hsla,
        cx: &mut Context<'_, Self>,
    ) -> Div {
        let total_count = self.model.entries().len();

        div()
            .w(px(LEFT_COLUMN_WIDTH_PX))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(border_subtle)
            .child(
                div()
                    .h(px(30.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(border_subtle)
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text_secondary)
                            .child(format!("Changes ({total_count})")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .child(
                                Button::ghost("Stage All", PluginVariant::Muted)
                                    .render("commit-stage-all", p)
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.stage_all(cx);
                                    })),
                            )
                            .child(
                                Button::ghost("Unstage All", PluginVariant::Muted)
                                    .render("commit-unstage-all", p)
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.unstage_all(cx);
                                    })),
                            ),
                    ),
            )
            .child(if total_count == 0 {
                div()
                    .flex_1()
                    .p(px(12.0))
                    .text_size(px(12.0))
                    .text_color(p.text_muted)
                    .child(if self.repo.is_some() {
                        "Working tree clean — nothing to commit."
                    } else {
                        "Open a Git repository to view changes."
                    })
                    .into_any_element()
            } else {
                uniform_list(
                    "commit-files-list",
                    total_count,
                    cx.processor(Self::render_file_rows),
                )
                .track_scroll(self.list_scroll.clone())
                .flex_1()
                .w_full()
                .p(px(4.0))
                .into_any_element()
            })
            .child(self.render_commit_footer(p, island, border_subtle, cx))
    }

    fn render_commit_footer(
        &self,
        p: &ControlPalette,
        island: Hsla,
        border_subtle: Hsla,
        cx: &Context<'_, Self>,
    ) -> Div {
        let staged_count = self.model.staged_count();
        let commit_shortcut = if cfg!(target_os = "macos") {
            "⌘↵ Commit"
        } else {
            "Ctrl+↵ Commit"
        };
        let commit_btn_variant = if self.model.can_commit() {
            PluginVariant::Primary
        } else {
            PluginVariant::Muted
        };
        div()
            .p(px(10.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .border_t_1()
            .border_color(border_subtle)
            .bg(island)
            .child(self.render_message_box(p, cx))
            .children(self.model.error().map(|err| {
                div()
                    .px(px(8.0))
                    .py(px(4.0))
                    .rounded(px(4.0))
                    .bg(p.danger.subtle_bg)
                    .border_1()
                    .border_color(p.danger.border)
                    .text_size(px(11.0))
                    .text_color(p.danger.subtle_fg)
                    .child(err.to_owned())
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(p.text_muted)
                            .child(format!("{staged_count} staged")),
                    )
                    .child(
                        Button::new(commit_shortcut, commit_btn_variant)
                            .render("commit-submit", p)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.commit_staged(cx);
                            })),
                    ),
            )
    }

    fn render_diff_column(
        &self,
        p: &ControlPalette,
        border_subtle: Hsla,
        cx: &mut Context<'_, Self>,
    ) -> Div {
        let diff_title = self
            .model
            .selected_path()
            .map_or_else(|| "Diff Preview".to_owned(), str::to_owned);
        let diff_len = self.model.diff_lines().len();

        div()
            .flex_1()
            .h_full()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(30.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(border_subtle)
                    .child(
                        div()
                            .text_size(px(11.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(p.text_secondary)
                            .child(diff_title),
                    )
                    .child(div().text_size(px(11.0)).text_color(p.text_muted).child(
                        if diff_len == 0 {
                            String::new()
                        } else {
                            format!("{diff_len} lines")
                        },
                    )),
            )
            .child(if diff_len == 0 {
                div()
                    .flex_1()
                    .p(px(16.0))
                    .text_size(px(12.0))
                    .text_color(p.text_muted)
                    .child(if self.model.selected_file().is_some() {
                        "No textual diff available."
                    } else {
                        "Select a changed file to preview its diff."
                    })
                    .into_any_element()
            } else {
                uniform_list(
                    "commit-diff-preview",
                    diff_len,
                    cx.processor(Self::render_diff_rows),
                )
                .track_scroll(self.diff_scroll.clone())
                .flex_1()
                .w_full()
                .py(px(4.0))
                .into_any_element()
            })
    }
}

impl Render for CommitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        self.render_modal((f32::from(viewport.width), f32::from(viewport.height)), cx)
    }
}

trait DivExt {
    fn when_bg(self, cond: bool, color: Hsla) -> Self;
}

impl DivExt for gpui::Stateful<Div> {
    fn when_bg(self, cond: bool, color: Hsla) -> Self {
        if cond { self.bg(color) } else { self }
    }
}

fn mono_font() -> Font {
    Font {
        family: SharedString::from(MONO_FONT),
        features: FontFeatures::default(),
        fallbacks: Some(FontFallbacks::from_fonts(vec![
            "SF Mono".to_owned(),
            "Menlo".to_owned(),
            "ui-monospace".to_owned(),
            "monospace".to_owned(),
        ])),
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(path: &str, staged: Option<ChangeKind>, unstaged: Option<ChangeKind>) -> FileStatus {
        FileStatus {
            path: path.to_owned(),
            staged,
            unstaged,
        }
    }

    #[test]
    fn check_state_and_toggle_ops_reflect_stage_status() {
        let mut model = CommitPanelModel::new();
        model.set_status(
            "main".to_owned(),
            vec![
                status("a.java", None, Some(ChangeKind::Modified)),
                status("b.java", Some(ChangeKind::Added), None),
                status(
                    "c.java",
                    Some(ChangeKind::Modified),
                    Some(ChangeKind::Modified),
                ),
            ],
        );

        assert_eq!(model.check_state(0), Some(CheckState::Unchecked));
        assert_eq!(model.check_state(1), Some(CheckState::Checked));
        assert_eq!(model.check_state(2), Some(CheckState::Partial));
        assert_eq!(model.staged_count(), 2);

        assert_eq!(
            model.toggle_op_at(0),
            Some(StageOp::Stage("a.java".to_owned()))
        );
        assert_eq!(
            model.toggle_op_at(1),
            Some(StageOp::Unstage("b.java".to_owned()))
        );
        assert_eq!(
            model.toggle_op_at(2),
            Some(StageOp::Stage("c.java".to_owned()))
        );
        assert_eq!(
            model.stage_all_ops(),
            vec![
                StageOp::Stage("a.java".to_owned()),
                StageOp::Stage("c.java".to_owned()),
            ]
        );
        assert_eq!(
            model.unstage_all_ops(),
            vec![
                StageOp::Unstage("b.java".to_owned()),
                StageOp::Unstage("c.java".to_owned()),
            ]
        );
    }

    #[test]
    fn commit_readiness_requires_staged_file_and_non_empty_message() {
        let mut model = CommitPanelModel::new();
        model.set_status(
            "feat/git".to_owned(),
            vec![status("a.txt", None, Some(ChangeKind::Untracked))],
        );
        assert!(!model.can_commit());

        model.message_mut().set_text("Initial commit");
        assert!(!model.can_commit(), "no staged file yet");

        model.set_status(
            "feat/git".to_owned(),
            vec![status("a.txt", Some(ChangeKind::Added), None)],
        );
        assert!(model.can_commit());

        model.message_mut().set_text("   \n  ");
        assert!(!model.can_commit(), "blank message rejected");

        model.message_mut().set_text("Add a.txt");
        model.set_busy(true);
        assert!(!model.can_commit(), "busy blocks double commit");
    }

    #[test]
    fn selection_preserves_path_across_refresh_and_tracks_diff() {
        let mut model = CommitPanelModel::new();
        model.set_status(
            "main".to_owned(),
            vec![
                status("a.txt", None, Some(ChangeKind::Modified)),
                status("b.txt", Some(ChangeKind::Modified), None),
            ],
        );
        assert_eq!(model.select_index(1), Some("b.txt"));
        model.set_diff(
            "b.txt".to_owned(),
            vec![DiffLine {
                kind: DiffLineKind::Added,
                text: "hello".to_owned(),
            }],
        );
        assert_eq!(model.diff_path(), Some("b.txt"));
        assert_eq!(model.diff_lines().len(), 1);

        // Stale diff for another file is ignored.
        model.set_diff("a.txt".to_owned(), Vec::new());
        assert_eq!(model.diff_path(), Some("b.txt"));

        // Refresh reorders/inserts above `b.txt`, keeping `b.txt` selected and diff intact.
        model.set_status(
            "main".to_owned(),
            vec![
                status("0.txt", None, Some(ChangeKind::Untracked)),
                status("a.txt", None, Some(ChangeKind::Modified)),
                status("b.txt", Some(ChangeKind::Modified), None),
            ],
        );
        assert_eq!(model.selected_index(), 2);
        assert_eq!(model.selected_path(), Some("b.txt"));
        assert_eq!(model.diff_lines().len(), 1);

        assert_eq!(model.move_selection(false), Some("a.txt"));
        assert_eq!(model.diff_lines(), []);
    }

    #[test]
    fn commit_message_state_supports_multiline_and_unicode_editing() {
        let mut msg = CommitMessageState::new();
        assert!(msg.insert("Fix Żółć"));
        assert!(msg.handle_named_key("enter"));
        assert!(msg.insert("Body"));
        assert_eq!(msg.text(), "Fix Żółć\nBody");

        msg.handle_named_key("left");
        assert!(msg.handle_named_key("backspace"));
        assert_eq!(msg.text(), "Fix Żółć\nBoy");
        msg.handle_named_key("end");
        msg.handle_named_key("left");
        assert!(msg.handle_named_key("delete"));
        assert_eq!(msg.text(), "Fix Żółć\nBo");
        msg.clear();
        assert_eq!(msg.text(), "");
        assert!(!msg.handle_named_key("backspace"));
    }
}
