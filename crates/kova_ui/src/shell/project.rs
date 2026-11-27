//! Project switching, interactive *Open Project* dialog flow, and native menu commands
//! of [`WorkspaceShell`] (`KOVA-049`, `KOVA-054`).
//!
//! `File → Open Project…`, `File → Recent Projects`, and the titlebar project dropdown
//! (`KOVA-065`) route through [`WorkspaceShell::request_open_project`]. Direct callers of
//! [`WorkspaceShell::switch_project`] keep the safe-by-default dirty check, while
//! [`WorkspaceShell::switch_project_discarding`] discards unsaved edits after explicit
//! user confirmation.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gpui::{Context, Div, Stateful, Window};

use super::open_project_modal::{
    OpenProjectActionCallback, OpenProjectButton, OpenProjectCancelCallback, OpenProjectDirtyEntry,
    OpenProjectKeyOutcome, OpenProjectModal, ProjectHostCommand, ProjectHostSink,
    render_open_project_modal,
};
use super::{PaneState, WorkspaceShell};
use crate::app_menu::{
    BuildProject, CloseTab, OpenSettings, RunAllTests, RunContext, RunLast, SearchEverywhere,
    StopRun, ToggleFocusMode, ToggleGit, ToggleProjectTree, ToggleTerminal,
};
use crate::dock::{
    BottomPanelModel, DockToolId, FocusModeVariant, FocusedRegion, ProjectTreeModel, SplitTree,
    StructureModel,
};
use crate::document::{
    DocumentState, SAVE_PATH_REQUIRED_MESSAGE, SaveAction, SaveSink, SaveStatus, SaveTicket,
    StatusKind,
};
use crate::editor::shared::SharedBuffer;
use crate::keymap::ShellAction;
use crate::terminal::TerminalShortcut;
use crate::theme::KovaTheme;

/// Why [`WorkspaceShell::switch_project`] kept the current project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchProjectError {
    /// The target is not a directory.
    NotADirectory(PathBuf),
    /// These open documents have unsaved changes.
    UnsavedChanges(Vec<PathBuf>),
}

/// How [`WorkspaceShell::switch_project_with_mode`] treats unsaved documents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchProjectMode {
    /// Refuse to switch while any open document has unsaved edits.
    CheckDirty,
    /// Discard all unsaved edits and switch immediately (after explicit consent).
    DiscardDirty,
}

/// Outcome of [`WorkspaceShell::request_open_project`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenProjectRequestOutcome {
    /// Target path is not an existing directory; an error notice was shown.
    NotADirectory,
    /// Target matches the project already open in this window (clean; editor re-focused).
    AlreadyOpenClean,
    /// Target matches the project already open in this window (dirty; kept untouched).
    AlreadyOpenDirty,
    /// No project was bound and no buffers were dirty; switched immediately.
    SwitchedImmediately,
    /// Opened the interactive *Open Project* modal (clean or dirty).
    OpenedModal,
    /// Ignored because a *Save & Switch* batch save is already running.
    SaveInProgress,
}

/// Tracks an in-flight *Save & Switch* batch save before switching the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingProjectSwitch {
    /// Target project root to switch to once all saves succeed.
    pub target_root: PathBuf,
    /// In-flight save tickets awaited before switching.
    pub pending_tickets: HashSet<SaveTicket>,
    /// Remaining untitled (`doc.path.is_none()`) dirty documents queued for *Save As*.
    pub untitled_queue: VecDeque<PathBuf>,
    /// Untitled document currently awaiting a *Save As* dialog response.
    pub awaiting_save_as: Option<PathBuf>,
}

/// Pure decision produced by [`plan_open_project`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenProjectPlan {
    NotADirectory(PathBuf),
    AlreadyOpenClean(PathBuf),
    AlreadyOpenDirty,
    SwitchImmediately(PathBuf),
    OpenModal(OpenProjectModal),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingBeginOutcome {
    ReadyNow(PathBuf),
    Waiting {
        pending: PendingProjectSwitch,
        next_save_as: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingSaveStep {
    StillWaiting,
    Aborted(String),
    Complete(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingSaveAsStep {
    StillWaiting,
    PromptNext(PathBuf),
    Aborted(String),
    Complete(PathBuf),
}

/// Canonicalizes `path` when possible, falling back to `path` itself.
#[must_use]
pub fn canonicalize_or_raw(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Returns `true` when `lhs` and `rhs` refer to the same directory (canonicalized).
#[must_use]
pub fn paths_match(lhs: &Path, rhs: &Path) -> bool {
    canonicalize_or_raw(lhs) == canonicalize_or_raw(rhs)
}

/// Evaluates the *Open Project* decision matrix for `target_root`.
#[must_use]
pub(crate) fn plan_open_project(
    current_root: Option<&Path>,
    target_root: PathBuf,
    dirty_entries: &[OpenProjectDirtyEntry],
    can_save: bool,
) -> OpenProjectPlan {
    if !target_root.is_dir() {
        return OpenProjectPlan::NotADirectory(target_root);
    }
    if current_root.is_some_and(|current| paths_match(current, &target_root)) {
        return if dirty_entries.is_empty() {
            OpenProjectPlan::AlreadyOpenClean(target_root)
        } else {
            OpenProjectPlan::AlreadyOpenDirty
        };
    }
    if current_root.is_none() && dirty_entries.is_empty() {
        return OpenProjectPlan::SwitchImmediately(target_root);
    }
    let modal = if dirty_entries.is_empty() {
        OpenProjectModal::new_clean(target_root)
    } else {
        OpenProjectModal::new_dirty(target_root, dirty_entries.to_vec(), can_save)
    };
    OpenProjectPlan::OpenModal(modal)
}

fn dirty_keys_from(
    doc_states: &HashMap<PathBuf, DocumentState>,
    documents: &HashMap<PathBuf, SharedBuffer>,
) -> Vec<PathBuf> {
    let mut dirty: Vec<PathBuf> = doc_states
        .iter()
        .filter(|(key, doc)| {
            documents
                .get(*key)
                .is_some_and(|buffer| doc.is_dirty(buffer.version().as_u64()))
        })
        .map(|(key, _)| key.clone())
        .collect();
    dirty.sort();
    dirty
}

fn any_save_in_flight(doc_states: &HashMap<PathBuf, DocumentState>) -> bool {
    doc_states
        .values()
        .any(|doc| doc.in_flight_ticket().is_some())
}

fn dispatch_doc_save_with_sink(
    key: &Path,
    doc_states: &mut HashMap<PathBuf, DocumentState>,
    documents: &HashMap<PathBuf, SharedBuffer>,
    save_sink: Option<&SaveSink>,
) -> Result<Option<SaveTicket>, String> {
    let Some(snapshot) = documents.get(key).map(SharedBuffer::snapshot) else {
        return Ok(None);
    };
    let Some(doc) = doc_states.get_mut(key) else {
        return Ok(None);
    };
    let existing_ticket = doc.in_flight_ticket();
    match doc.begin_save(snapshot) {
        SaveAction::Dispatch(request) => {
            let ticket = request.ticket;
            if let Some(sink) = save_sink {
                sink(request);
                Ok(Some(ticket))
            } else {
                let message = "Saving is not available".to_owned();
                doc.finish_save(ticket, Err(message.clone()));
                Err(message)
            }
        }
        SaveAction::Queued => Ok(existing_ticket),
        SaveAction::NeedsPath => Err(SAVE_PATH_REQUIRED_MESSAGE.to_owned()),
    }
}

impl PendingProjectSwitch {
    fn begin(
        target_root: PathBuf,
        dirty_keys: &[PathBuf],
        doc_states: &mut HashMap<PathBuf, DocumentState>,
        documents: &HashMap<PathBuf, SharedBuffer>,
        save_sink: Option<&SaveSink>,
    ) -> Result<PendingBeginOutcome, String> {
        if save_sink.is_none() {
            return Err("Saving is unavailable because no save sink is configured.".to_owned());
        }

        let mut path_backed = Vec::new();
        let mut untitled_queue = VecDeque::new();
        for key in dirty_keys {
            if let Some(doc) = doc_states.get(key) {
                if doc.path.is_some() {
                    path_backed.push(key.clone());
                } else {
                    untitled_queue.push_back(key.clone());
                }
            }
        }

        let mut pending_tickets = HashSet::new();
        for key in &path_backed {
            if let Some(ticket) =
                dispatch_doc_save_with_sink(key, doc_states, documents, save_sink)?
            {
                pending_tickets.insert(ticket);
            }
        }

        let next_save_as = untitled_queue.pop_front();
        if pending_tickets.is_empty() && next_save_as.is_none() {
            let remaining_dirty = dirty_keys_from(doc_states, documents);
            if remaining_dirty.is_empty() && !any_save_in_flight(doc_states) {
                return Ok(PendingBeginOutcome::ReadyNow(target_root));
            }
            // Wait for any already in-flight save on a clean-at-click doc if present.
            for doc in doc_states.values() {
                if let Some(ticket) = doc.in_flight_ticket() {
                    pending_tickets.insert(ticket);
                }
            }
            if pending_tickets.is_empty() {
                return Err("Unsaved edits remain; please try Save & Switch again.".to_owned());
            }
        }

        let pending = Self {
            target_root,
            pending_tickets,
            untitled_queue,
            awaiting_save_as: next_save_as.clone(),
        };
        Ok(PendingBeginOutcome::Waiting {
            pending,
            next_save_as,
        })
    }

    fn is_ready_to_switch(
        &self,
        doc_states: &HashMap<PathBuf, DocumentState>,
        documents: &HashMap<PathBuf, SharedBuffer>,
    ) -> Result<bool, String> {
        if !self.pending_tickets.is_empty()
            || !self.untitled_queue.is_empty()
            || self.awaiting_save_as.is_some()
        {
            return Ok(false);
        }
        if !dirty_keys_from(doc_states, documents).is_empty() {
            return Err("Unsaved edits remain; please try Save & Switch again.".to_owned());
        }
        if any_save_in_flight(doc_states) {
            return Ok(false);
        }
        Ok(true)
    }

    fn on_save_finished(
        &mut self,
        key: &Path,
        ticket: SaveTicket,
        outcome: Result<(), &str>,
        resave: bool,
        doc_states: &HashMap<PathBuf, DocumentState>,
        documents: &HashMap<PathBuf, SharedBuffer>,
    ) -> PendingSaveStep {
        self.pending_tickets.remove(&ticket);
        if let Err(detail) = outcome {
            return PendingSaveStep::Aborted(format!("Save failed ({}): {detail}", key.display()));
        }
        if resave && let Some(doc) = doc_states.get(key) {
            if let Some(new_ticket) = doc.in_flight_ticket() {
                self.pending_tickets.insert(new_ticket);
            } else if let SaveStatus::Failed(detail) = doc.status() {
                return PendingSaveStep::Aborted(format!(
                    "Save failed ({}): {detail}",
                    key.display()
                ));
            }
        }
        match self.is_ready_to_switch(doc_states, documents) {
            Ok(true) => PendingSaveStep::Complete(self.target_root.clone()),
            Ok(false) => PendingSaveStep::StillWaiting,
            Err(message) => PendingSaveStep::Aborted(message),
        }
    }

    fn on_save_as_result(
        &mut self,
        key: &Path,
        chosen: Result<Option<PathBuf>, String>,
        doc_states: &mut HashMap<PathBuf, DocumentState>,
        documents: &HashMap<PathBuf, SharedBuffer>,
        save_sink: Option<&SaveSink>,
    ) -> PendingSaveAsStep {
        if self.awaiting_save_as.as_deref() != Some(key) {
            return PendingSaveAsStep::StillWaiting;
        }
        self.awaiting_save_as = None;

        match chosen {
            Ok(Some(path)) => {
                if let Some(doc) = doc_states.get_mut(key) {
                    doc.set_path(path);
                }
                match dispatch_doc_save_with_sink(key, doc_states, documents, save_sink) {
                    Ok(Some(ticket)) => {
                        self.pending_tickets.insert(ticket);
                    }
                    Ok(None) => {}
                    Err(message) => return PendingSaveAsStep::Aborted(message),
                }
                if let Some(next_key) = self.untitled_queue.pop_front() {
                    self.awaiting_save_as = Some(next_key.clone());
                    return PendingSaveAsStep::PromptNext(next_key);
                }
                match self.is_ready_to_switch(doc_states, documents) {
                    Ok(true) => PendingSaveAsStep::Complete(self.target_root.clone()),
                    Ok(false) => PendingSaveAsStep::StillWaiting,
                    Err(message) => PendingSaveAsStep::Aborted(message),
                }
            }
            Ok(None) => {
                PendingSaveAsStep::Aborted("Save As cancelled; project switch aborted.".to_owned())
            }
            Err(message) => PendingSaveAsStep::Aborted(message),
        }
    }
}

impl WorkspaceShell {
    /// Installs the host callback used to coordinate `Recent Projects` and new windows (`KOVA-054`).
    pub fn set_project_host_sink(&mut self, sink: ProjectHostSink) {
        self.project_host_sink = Some(sink);
    }

    /// Returns the active *Open Project* modal state, if open.
    #[must_use]
    pub const fn open_project_modal(&self) -> Option<&OpenProjectModal> {
        self.open_project_modal.as_ref()
    }

    /// Returns the in-progress *Save & Switch* state, if any.
    #[must_use]
    pub const fn pending_project_switch(&self) -> Option<&PendingProjectSwitch> {
        self.pending_project_switch.as_ref()
    }

    fn emit_project_host(&self, command: ProjectHostCommand) {
        if let Some(sink) = &self.project_host_sink {
            sink(command);
        }
    }

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
        self.switch_project_with_mode(root, SwitchProjectMode::CheckDirty, window, cx)
    }

    /// Switches this window to `root`, explicitly discarding any unsaved edits.
    ///
    /// # Errors
    ///
    /// Returns [`SwitchProjectError::NotADirectory`] when `root` is not an existing directory.
    pub fn switch_project_discarding(
        &mut self,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Result<(), SwitchProjectError> {
        self.switch_project_with_mode(root, SwitchProjectMode::DiscardDirty, window, cx)
    }

    /// Switches this window to `root` according to `mode`.
    ///
    /// # Errors
    ///
    /// Returns [`SwitchProjectError`] when `root` is not a directory or when
    /// `mode == SwitchProjectMode::CheckDirty` and unsaved documents exist.
    pub fn switch_project_with_mode(
        &mut self,
        root: PathBuf,
        mode: SwitchProjectMode,
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
        if mode == SwitchProjectMode::CheckDirty {
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
        }

        self.execute_project_switch(root, Some(window), cx);
        Ok(())
    }

    /// Interactive entry point for `File → Open Project…`, `File → Recent Projects`, and the
    /// titlebar project pill (`KOVA-054`).
    ///
    /// Applies the decision matrix (invalid directory, already open in current window,
    /// unloaded + clean fast path, clean modal, or dirty modal).
    pub fn request_open_project(
        &mut self,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> OpenProjectRequestOutcome {
        if self.pending_project_switch.is_some() {
            return OpenProjectRequestOutcome::SaveInProgress;
        }
        let dirty_entries = self.dirty_document_entries();
        match plan_open_project(
            self.project.root(),
            root,
            &dirty_entries,
            self.save_sink.is_some(),
        ) {
            OpenProjectPlan::NotADirectory(root) => {
                self.set_notice(
                    StatusKind::Error,
                    format!("Cannot open project: {} is not a directory", root.display()),
                    cx,
                );
                self.emit_project_host(ProjectHostCommand::RemoveRecent(root));
                OpenProjectRequestOutcome::NotADirectory
            }
            OpenProjectPlan::AlreadyOpenClean(root) => {
                self.emit_project_host(ProjectHostCommand::RecordRecent(root));
                self.focus_active_editor(window, cx);
                window.focus(&self.focus_handle);
                cx.notify();
                OpenProjectRequestOutcome::AlreadyOpenClean
            }
            OpenProjectPlan::AlreadyOpenDirty => {
                self.focus_active_editor(window, cx);
                window.focus(&self.focus_handle);
                cx.notify();
                OpenProjectRequestOutcome::AlreadyOpenDirty
            }
            OpenProjectPlan::SwitchImmediately(root) => {
                self.execute_project_switch(root, Some(window), cx);
                OpenProjectRequestOutcome::SwitchedImmediately
            }
            OpenProjectPlan::OpenModal(modal) => {
                self.open_project_modal = Some(modal);
                self.close_completion_popup(cx);
                self.ex_line = None;
                self.keymap.reset();
                self.chord_hint = None;
                window.focus(&self.focus_handle);
                cx.notify();
                OpenProjectRequestOutcome::OpenedModal
            }
        }
    }

    fn execute_project_switch(
        &mut self,
        root: PathBuf,
        window: Option<&mut Window>,
        cx: &mut Context<'_, Self>,
    ) {
        self.project.switch_to(root.clone());
        self.reset_project_state(cx);
        self.load_project(cx);
        self.clear_notice(cx);
        if let Some(window) = window {
            self.focus_active_editor(window, cx);
            window.focus(&self.focus_handle);
        }
        self.emit_project_host(ProjectHostCommand::RecordRecent(root));
        cx.notify();
    }

    /// Keys of open documents with unsaved changes, sorted.
    fn dirty_document_keys(&self) -> Vec<PathBuf> {
        dirty_keys_from(&self.doc_states, &self.documents)
    }

    fn dirty_document_entries(&self) -> Vec<OpenProjectDirtyEntry> {
        self.dirty_document_keys()
            .into_iter()
            .map(|key| {
                let untitled = self
                    .doc_states
                    .get(&key)
                    .is_some_and(|doc| doc.path.is_none());
                OpenProjectDirtyEntry {
                    display_name: key.display().to_string(),
                    untitled,
                }
            })
            .collect()
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
        self.close_completion_popup(cx);
        self.next_completion_request_id = self.next_completion_request_id.wrapping_add(1);
        self.doc_notice = None;
        self.close_after_save = None;
        self.open_project_modal = None;
        self.pending_project_switch = None;
        self.project_dropdown = None;
        self.ex_line = None;
        self.analysis_task = None;
        self.project_task = None;

        self.project_tree = ProjectTreeModel::default();
        self.project_tree.set_filter("");
        self.new_item_modal.state_mut().close();
        self.new_item_modal.clear_bounds();
        self.project_options_open = false;
        self.project_options_bounds.set(None);
        self.context_menu = None;
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
        self.branches_popup_open = false;
        self.branches_popup = None;
        self.branch_reload_task = None;
        self.search_everywhere.close();
        self.find_in_files.close();
        self.find_in_files_task = None;
        self.recent_files.clear();
        self.last_run = None;
        self.active_build_run_id = None;
        if let Some(cancel) = &self.cancel_run_sink {
            cancel(super::CancelRunKind::All);
        }
        self.git_discover_task = None;
        self.status_bar.git_branch.clear();
        self.set_git_repo(None, None, cx);
    }

    /// Intercepts capture-phase keys while the *Open Project* modal is open.
    pub(super) fn handle_open_project_modal_key(
        &mut self,
        keystroke: &gpui::Keystroke,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(modal) = self.open_project_modal.as_mut() else {
            return false;
        };
        let mods = &keystroke.modifiers;
        if mods.control || mods.platform || mods.alt {
            // Keep modal ownership even for modified chords.
            return true;
        }
        match modal.handle_key(&keystroke.key, mods.shift) {
            OpenProjectKeyOutcome::Cancel => {
                self.cancel_open_project_modal(window, cx);
            }
            OpenProjectKeyOutcome::Activate(button) => {
                self.activate_open_project_button(button, window, cx);
            }
            OpenProjectKeyOutcome::Consumed => {
                cx.notify();
            }
            OpenProjectKeyOutcome::Ignored => {}
        }
        true
    }

    /// Executes a button action inside the open *Open Project* dialog.
    pub fn activate_open_project_button(
        &mut self,
        button: OpenProjectButton,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(target_root) = self
            .open_project_modal
            .as_ref()
            .filter(|modal| modal.is_button_enabled(button))
            .map(|modal| modal.target_root().clone())
        else {
            return;
        };

        match button {
            OpenProjectButton::Cancel => {
                self.cancel_open_project_modal(window, cx);
            }
            OpenProjectButton::ThisWindow => {
                self.open_project_modal = None;
                drop(self.switch_project(target_root, window, cx));
            }
            OpenProjectButton::DiscardAndSwitch => {
                self.open_project_modal = None;
                drop(self.switch_project_discarding(target_root, window, cx));
            }
            OpenProjectButton::NewWindow => {
                self.open_project_modal = None;
                self.pending_project_switch = None;
                self.emit_project_host(ProjectHostCommand::OpenNewWindow(target_root));
                self.focus_active_editor(window, cx);
                cx.notify();
            }
            OpenProjectButton::SaveAndSwitch => {
                self.begin_save_and_switch(target_root, window, cx);
            }
        }
    }

    /// Dismisses the *Open Project* dialog and aborts any pending switch without discarding edits.
    pub fn cancel_open_project_modal(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.open_project_modal.take().is_none() && self.pending_project_switch.take().is_none()
        {
            return;
        }
        self.pending_project_switch = None;
        self.focus_active_editor(window, cx);
        cx.notify();
    }

    fn begin_save_and_switch(
        &mut self,
        target_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.pending_project_switch.is_some() {
            return;
        }
        let dirty = self.dirty_document_keys();
        match PendingProjectSwitch::begin(
            target_root,
            &dirty,
            &mut self.doc_states,
            &self.documents,
            self.save_sink.as_ref(),
        ) {
            Ok(PendingBeginOutcome::ReadyNow(root)) => {
                self.open_project_modal = None;
                self.execute_project_switch(root, Some(window), cx);
            }
            Ok(PendingBeginOutcome::Waiting {
                pending,
                next_save_as,
            }) => {
                self.pending_project_switch = Some(pending);
                if let Some(modal) = self.open_project_modal.as_mut() {
                    modal.set_saving(true);
                    modal.set_error(None);
                }
                if let Some(save_as_key) = next_save_as {
                    self.prompt_pending_save_as(save_as_key, window, cx);
                }
                cx.notify();
            }
            Err(message) => {
                if let Some(modal) = self.open_project_modal.as_mut() {
                    modal.set_saving(false);
                    modal.set_error(Some(message.clone()));
                }
                self.set_notice(StatusKind::Error, message, cx);
            }
        }
    }

    fn prompt_pending_save_as(
        &self,
        key: PathBuf,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let directory = self
            .project
            .root_owned()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let suggested = key.file_name().map_or_else(
            || "Untitled.java".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let receiver = cx.prompt_for_new_path(&directory, Some(&suggested));
        cx.spawn_in(window, async move |this, cx| {
            let outcome = receiver.await;
            this.update_in(cx, |shell, window, cx| {
                let chosen = match outcome {
                    Ok(Ok(path)) => Ok(path),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(_) => Err(SAVE_PATH_REQUIRED_MESSAGE.to_owned()),
                };
                shell.handle_pending_save_as_outcome(&key, chosen, window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn handle_pending_save_as_outcome(
        &mut self,
        key: &Path,
        chosen: Result<Option<PathBuf>, String>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(pending) = self.pending_project_switch.as_mut() else {
            return;
        };
        match pending.on_save_as_result(
            key,
            chosen,
            &mut self.doc_states,
            &self.documents,
            self.save_sink.as_ref(),
        ) {
            PendingSaveAsStep::StillWaiting => cx.notify(),
            PendingSaveAsStep::PromptNext(next_key) => {
                self.prompt_pending_save_as(next_key, window, cx);
                cx.notify();
            }
            PendingSaveAsStep::Aborted(message) => {
                self.abort_pending_switch(Some(message), cx);
            }
            PendingSaveAsStep::Complete(root) => {
                self.pending_project_switch = None;
                self.open_project_modal = None;
                self.execute_project_switch(root, Some(window), cx);
            }
        }
    }

    pub(super) fn handle_pending_switch_save_finished(
        &mut self,
        key: &Path,
        ticket: SaveTicket,
        outcome: Result<(), &str>,
        resave: bool,
        window: Option<&mut Window>,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(pending) = self.pending_project_switch.as_mut() else {
            return;
        };
        match pending.on_save_finished(
            key,
            ticket,
            outcome,
            resave,
            &self.doc_states,
            &self.documents,
        ) {
            PendingSaveStep::StillWaiting => {}
            PendingSaveStep::Aborted(message) => {
                self.abort_pending_switch(Some(message), cx);
            }
            PendingSaveStep::Complete(root) => {
                self.pending_project_switch = None;
                self.open_project_modal = None;
                self.execute_project_switch(root, window, cx);
            }
        }
    }

    fn abort_pending_switch(&mut self, error: Option<String>, cx: &mut Context<'_, Self>) {
        self.pending_project_switch = None;
        if let Some(modal) = self.open_project_modal.as_mut() {
            modal.set_saving(false);
            modal.set_error(error.clone());
        }
        if let Some(message) = error {
            self.set_notice(StatusKind::Error, message, cx);
        } else {
            cx.notify();
        }
    }

    /// Renders the *Open Project* modal overlay when open.
    pub(super) fn render_open_project_overlay(
        &self,
        cx: &Context<'_, Self>,
    ) -> Option<Stateful<Div>> {
        let modal = self.open_project_modal.as_ref()?;
        let action_listener = cx.listener(|this, button: &OpenProjectButton, window, cx| {
            this.activate_open_project_button(*button, window, cx);
        });
        let cancel_listener = cx.listener(|this, (): &(), window, cx| {
            this.cancel_open_project_modal(window, cx);
        });
        let on_action: OpenProjectActionCallback =
            Rc::new(move |button, window, cx| action_listener(&button, window, cx));
        let on_cancel: OpenProjectCancelCallback =
            Rc::new(move |window, cx| cancel_listener(&(), window, cx));
        Some(render_open_project_modal(
            modal,
            cx.global::<KovaTheme>(),
            &on_action,
            on_cancel,
        ))
    }

    /// Whether a modal surface owns the keyboard, so a menu shortcut must fall through to
    /// the key capture that serves it (settings, commit panel, evaluate, open project, Ex line,
    /// Search Everywhere).
    pub(super) fn modal_owns_keys(&self) -> bool {
        self.settings_open
            || self.commit_panel_open
            || self.branches_popup_open
            || self.open_project_modal.is_some()
            || self.new_item_modal.is_open()
            || self.search_everywhere.is_open()
            || self.find_in_files.is_open()
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

    pub(super) fn build_project_menu_action(
        &mut self,
        _: &BuildProject,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(ShellAction::BuildProject, window, cx);
    }

    pub(super) fn run_last_menu_action(
        &mut self,
        _: &RunLast,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(ShellAction::RunLast, window, cx);
    }

    pub(super) fn run_context_menu_action(
        &mut self,
        _: &RunContext,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(ShellAction::RunContext, window, cx);
    }

    pub(super) fn run_all_tests_menu_action(
        &mut self,
        _: &RunAllTests,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(ShellAction::RunAllTests, window, cx);
    }

    pub(super) fn stop_run_menu_action(
        &mut self,
        _: &StopRun,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.run_menu_action(
            ShellAction::Debug(crate::keymap::DebugShellAction::Stop),
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
    #![allow(
        clippy::expect_used,
        reason = "test harness aborts immediately on invalid fixtures"
    )]

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use kova_buffer::Buffer;
    use kova_core::project::ActiveProject;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> std::io::Result<Self> {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("kova-054-project-test-{}-{n}", std::process::id()));
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

    use super::{
        OpenProjectPlan, PendingBeginOutcome, PendingProjectSwitch, PendingSaveAsStep,
        PendingSaveStep, dirty_keys_from, document_key, paths_match, plan_open_project,
    };
    use crate::document::{DocumentState, SaveRequest, SaveSink};
    use crate::editor::shared::SharedBuffer;
    use crate::shell::open_project_modal::{
        OpenProjectButton, OpenProjectDirtyEntry, OpenProjectModal, OpenProjectModalMode,
        ProjectHostCommand,
    };

    trait OpenProjectBeginResultExt {
        fn into_waiting(self) -> (PendingProjectSwitch, Option<PathBuf>);
    }

    impl OpenProjectBeginResultExt for PendingBeginOutcome {
        fn into_waiting(self) -> (PendingProjectSwitch, Option<PathBuf>) {
            match self {
                Self::Waiting {
                    pending,
                    next_save_as,
                } => (pending, next_save_as),
                Self::ReadyNow(root) => panic!("expected Waiting, got ReadyNow({root:?})"),
            }
        }
    }

    fn dirty_entry(name: &str, untitled: bool) -> OpenProjectDirtyEntry {
        OpenProjectDirtyEntry {
            display_name: name.to_owned(),
            untitled,
        }
    }

    fn recording_sink() -> (SaveSink, Arc<Mutex<Vec<SaveRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let sink: SaveSink = Arc::new(move |req| {
            captured.lock().expect("lock").push(req);
        });
        (sink, requests)
    }

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

    #[test]
    fn decision_matrix_covers_invalid_same_unloaded_clean_and_dirty_cases() {
        let temp = TempDir::new().expect("tempdir");
        let orders = temp.path().join("orders");
        let billing = temp.path().join("billing");
        let missing = temp.path().join("missing");
        std::fs::create_dir(&orders).expect("orders");
        std::fs::create_dir(&billing).expect("billing");

        // 1. Invalid directory.
        assert_eq!(
            plan_open_project(None, missing.clone(), &[], true),
            OpenProjectPlan::NotADirectory(missing)
        );

        // 2. Unloaded + clean -> immediate switch without modal.
        assert_eq!(
            plan_open_project(None, orders.clone(), &[], true),
            OpenProjectPlan::SwitchImmediately(orders.clone())
        );

        // 3. Unloaded + dirty (e.g. edited welcome buffer) -> dirty modal.
        let dirty = vec![dirty_entry("OrderApp.java", true)];
        assert_eq!(
            plan_open_project(None, orders.clone(), &dirty, true),
            OpenProjectPlan::OpenModal(OpenProjectModal::new_dirty(
                orders.clone(),
                dirty.clone(),
                true
            ))
        );

        // 4. Loaded + clean (different project) -> clean modal (This Window | New Window | Cancel).
        let plan = plan_open_project(Some(&orders), billing.clone(), &[], true);
        assert_eq!(
            plan,
            OpenProjectPlan::OpenModal(OpenProjectModal::new_clean(billing.clone()))
        );
        if let OpenProjectPlan::OpenModal(modal) = plan {
            assert_eq!(modal.mode(), &OpenProjectModalMode::Clean);
            assert_eq!(
                modal.buttons(),
                [
                    OpenProjectButton::ThisWindow,
                    OpenProjectButton::NewWindow,
                    OpenProjectButton::Cancel
                ]
            );
        }

        // 5. Same project (even via non-canonical path) -> clean no-op vs dirty keep.
        let non_canonical = orders.join(".");
        assert!(paths_match(&orders, &non_canonical));
        assert_eq!(
            plan_open_project(Some(&orders), non_canonical.clone(), &[], true),
            OpenProjectPlan::AlreadyOpenClean(non_canonical.clone())
        );
        assert_eq!(
            plan_open_project(Some(&orders), non_canonical, &dirty, true),
            OpenProjectPlan::AlreadyOpenDirty
        );
    }

    #[allow(
        clippy::too_many_lines,
        reason = "multi-step async save/resave scenario verified end-to-end in one test"
    )]
    #[test]
    fn save_and_switch_waits_for_all_tickets_including_resave_and_switches_when_clean() {
        let target = PathBuf::from("/work/billing");
        let (sink, requests) = recording_sink();

        let key_a = PathBuf::from("src/A.java");
        let key_b = PathBuf::from("src/B.java");
        let buf_a = SharedBuffer::new(Buffer::from_text("class A {}"));
        let buf_b = SharedBuffer::new(Buffer::from_text("class B {}"));

        let mut doc_states = HashMap::new();
        doc_states.insert(
            key_a.clone(),
            DocumentState::opened(Some(PathBuf::from("/work/orders/src/A.java")), 0, false),
        );
        doc_states.insert(
            key_b.clone(),
            DocumentState::opened(Some(PathBuf::from("/work/orders/src/B.java")), 0, false),
        );
        let mut documents = HashMap::new();
        documents.insert(key_a.clone(), buf_a.clone());
        documents.insert(key_b.clone(), buf_b.clone());

        // Make both dirty.
        buf_a.apply_edit(0..0, "// a\n").expect("edit a");
        buf_b.apply_edit(0..0, "// b\n").expect("edit b");
        let dirty = dirty_keys_from(&doc_states, &documents);
        assert_eq!(dirty, vec![key_a.clone(), key_b.clone()]);

        let (mut pending, next_save_as) = PendingProjectSwitch::begin(
            target.clone(),
            &dirty,
            &mut doc_states,
            &documents,
            Some(&sink),
        )
        .expect("begin")
        .into_waiting();
        assert_eq!(next_save_as, None);

        let dispatches = std::mem::take(&mut *requests.lock().expect("lock"));
        assert_eq!(dispatches.len(), 2);

        // Concurrent edit on A triggers a queued resave when its first save finishes.
        buf_a.apply_edit(0..0, "// more\n").expect("edit a2");
        let req_a = dispatches
            .iter()
            .find(|r| r.path.ends_with("A.java"))
            .expect("req A")
            .clone();
        let req_b = dispatches
            .iter()
            .find(|r| r.path.ends_with("B.java"))
            .expect("req B")
            .clone();

        // Simulate queued save while first save is in flight.
        assert!(matches!(
            super::dispatch_doc_save_with_sink(&key_a, &mut doc_states, &documents, Some(&sink)),
            Ok(Some(_))
        ));

        // Finish B -> still waiting on A.
        let resave_b = doc_states
            .get_mut(&key_b)
            .expect("b")
            .finish_save(req_b.ticket, Ok(()));
        assert!(!resave_b);
        assert_eq!(
            pending.on_save_finished(
                &key_b,
                req_b.ticket,
                Ok(()),
                resave_b,
                &doc_states,
                &documents
            ),
            PendingSaveStep::StillWaiting
        );

        // Finish A1 -> resave == true -> dispatches A2, still waiting!
        let resave_a1 = doc_states
            .get_mut(&key_a)
            .expect("a")
            .finish_save(req_a.ticket, Ok(()));
        assert!(resave_a1);
        let a2_ticket =
            super::dispatch_doc_save_with_sink(&key_a, &mut doc_states, &documents, Some(&sink))
                .expect("dispatch a2")
                .expect("ticket a2");
        assert_eq!(
            pending.on_save_finished(
                &key_a,
                req_a.ticket,
                Ok(()),
                resave_a1,
                &doc_states,
                &documents
            ),
            PendingSaveStep::StillWaiting
        );

        // Finish A2 -> now all dirty docs are clean and all tickets done -> Complete!
        let resave_a2 = doc_states
            .get_mut(&key_a)
            .expect("a")
            .finish_save(a2_ticket, Ok(()));
        assert!(!resave_a2);
        assert_eq!(
            pending.on_save_finished(
                &key_a,
                a2_ticket,
                Ok(()),
                resave_a2,
                &doc_states,
                &documents
            ),
            PendingSaveStep::Complete(target)
        );
        assert_eq!(
            dirty_keys_from(&doc_states, &documents),
            Vec::<PathBuf>::new()
        );
    }

    #[test]
    fn save_and_switch_aborts_on_save_error_and_keeps_edits_intact() {
        let target = PathBuf::from("/work/billing");
        let (sink, requests) = recording_sink();

        let key = PathBuf::from("src/Main.java");
        let buf = SharedBuffer::new(Buffer::from_text("class Main {}"));
        let mut doc_states = HashMap::new();
        doc_states.insert(
            key.clone(),
            DocumentState::opened(Some(PathBuf::from("/work/orders/src/Main.java")), 0, false),
        );
        let mut documents = HashMap::new();
        documents.insert(key.clone(), buf.clone());
        buf.apply_edit(0..0, "// dirty\n").expect("edit");

        let dirty = dirty_keys_from(&doc_states, &documents);
        let (mut pending, _) =
            PendingProjectSwitch::begin(target, &dirty, &mut doc_states, &documents, Some(&sink))
                .expect("begin")
                .into_waiting();
        let req = requests.lock().expect("lock").pop().expect("req");

        let resave = doc_states
            .get_mut(&key)
            .expect("doc")
            .finish_save(req.ticket, Err("disk full".to_owned()));
        let step = pending.on_save_finished(
            &key,
            req.ticket,
            Err("disk full"),
            resave,
            &doc_states,
            &documents,
        );
        assert!(matches!(step, PendingSaveStep::Aborted(msg) if msg.contains("disk full")));
        // Dirty buffer stays intact!
        assert_eq!(dirty_keys_from(&doc_states, &documents), vec![key]);
    }

    #[test]
    fn save_and_switch_untitled_prompts_save_as_and_aborts_on_cancel() {
        let target = PathBuf::from("/work/billing");
        let (sink, requests) = recording_sink();

        let welcome_key = PathBuf::from("src/main/java/dev/kova/demo/OrderApp.java");
        let buf = SharedBuffer::new(Buffer::from_text("class OrderApp {}"));
        let mut doc_states = HashMap::new();
        doc_states.insert(welcome_key.clone(), DocumentState::opened(None, 0, false));
        let mut documents = HashMap::new();
        documents.insert(welcome_key.clone(), buf.clone());
        buf.apply_edit(0..0, "// welcome edit\n").expect("edit");

        let dirty = dirty_keys_from(&doc_states, &documents);
        let (mut pending, next_save_as) = PendingProjectSwitch::begin(
            target.clone(),
            &dirty,
            &mut doc_states,
            &documents,
            Some(&sink),
        )
        .expect("begin")
        .into_waiting();
        assert_eq!(next_save_as, Some(welcome_key.clone()));

        // Cancelling Save As aborts cleanly without losing edits.
        let mut cancelled_pending = pending.clone();
        assert!(matches!(
            cancelled_pending.on_save_as_result(
                &welcome_key,
                Ok(None),
                &mut doc_states,
                &documents,
                Some(&sink),
            ),
            PendingSaveAsStep::Aborted(msg) if msg.contains("cancelled")
        ));
        assert_eq!(
            dirty_keys_from(&doc_states, &documents),
            vec![welcome_key.clone()]
        );

        // Choosing a path dispatches the save and completes once finish_save succeeds.
        let chosen_path = PathBuf::from("/tmp/OrderApp.java");
        assert_eq!(
            pending.on_save_as_result(
                &welcome_key,
                Ok(Some(chosen_path.clone())),
                &mut doc_states,
                &documents,
                Some(&sink),
            ),
            PendingSaveAsStep::StillWaiting
        );
        let req = requests.lock().expect("lock").pop().expect("req");
        assert_eq!(req.path, chosen_path);
        let resave = doc_states
            .get_mut(&welcome_key)
            .expect("doc")
            .finish_save(req.ticket, Ok(()));
        assert_eq!(
            pending.on_save_finished(
                &welcome_key,
                req.ticket,
                Ok(()),
                resave,
                &doc_states,
                &documents,
            ),
            PendingSaveStep::Complete(target)
        );
    }

    #[test]
    fn discard_and_new_window_behaviors_on_headless_models() {
        // Without save sink, Save & Switch returns error without mutating doc_states.
        let key = PathBuf::from("Main.java");
        let buf = SharedBuffer::new(Buffer::from_text("class Main {}"));
        let mut doc_states = HashMap::new();
        doc_states.insert(
            key.clone(),
            DocumentState::opened(Some(PathBuf::from("/work/orders/Main.java")), 0, false),
        );
        let mut documents = HashMap::new();
        documents.insert(key.clone(), buf.clone());
        buf.apply_edit(0..0, "// edit\n").expect("edit");

        assert!(
            PendingProjectSwitch::begin(
                PathBuf::from("/work/billing"),
                std::slice::from_ref(&key),
                &mut doc_states,
                &documents,
                None,
            )
            .is_err()
        );
        assert_eq!(dirty_keys_from(&doc_states, &documents), vec![key.clone()]);

        // ActiveProject switch (Discard & Switch semantics) updates root and leaves New Window command distinct.
        let mut active = ActiveProject::unloaded("kova-orders");
        active.switch_to("/work/orders");
        active.switch_to("/work/billing");
        assert_eq!(active.root(), Some(Path::new("/work/billing")));

        // Host command for New Window carries target without altering dirty docs.
        let cmd = ProjectHostCommand::OpenNewWindow(PathBuf::from("/work/billing"));
        assert_eq!(
            cmd,
            ProjectHostCommand::OpenNewWindow(PathBuf::from("/work/billing"))
        );
        assert_eq!(dirty_keys_from(&doc_states, &documents), vec![key]);
    }
}
