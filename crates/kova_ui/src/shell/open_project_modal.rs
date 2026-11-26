//! Interactive *Open Project* modal dialog (`KOVA-054`).
//!
//! Presented when the user triggers `File → Open Project…`, `File → Recent Projects`, or the
//! titlebar project pill while a project is already open or unsaved edits exist:
//! - Clean mode: **This Window** | **New Window** | **Cancel**
//! - Dirty mode: **Save & Switch** | **Discard & Switch** | **New Window** | **Cancel**
//!
//! [`OpenProjectModal`] is pure headless state (mode, dirty file preview, focused button,
//! saving/error state, and keyboard navigation) rendered as a centered GPUI overlay by
//! [`render_open_project_modal`].

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, ClickEvent, Div, FontWeight, InteractiveElement as _, MouseButton, MouseDownEvent,
    ParentElement as _, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _,
    Window, div, px,
};
use kova_core::project::project_display_name;
use kova_theme::PluginVariant;

use crate::controls::{Badge, Button, ControlPalette};
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Maximum number of dirty file rows listed in [`OpenProjectModal`] before summarizing.
pub const MAX_DIRTY_PREVIEW_ITEMS: usize = 5;

const MODAL_WIDTH_PX: f32 = 480.0;

/// Host-level command emitted by [`crate::WorkspaceShell`] for multi-window and
/// `Recent Projects` coordination (`KOVA-054`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectHostCommand {
    /// Record `root` in the persisted `Recent Projects` list.
    RecordRecent(PathBuf),
    /// Forget a vanished or invalid `root` from `Recent Projects`.
    RemoveRecent(PathBuf),
    /// Open `root` in a separate workspace window, leaving the current window untouched.
    OpenNewWindow(PathBuf),
}

/// Callback delivering [`ProjectHostCommand`]s to the application host (`kova_app`).
pub type ProjectHostSink = Arc<dyn Fn(ProjectHostCommand) + Send + Sync>;

/// Button inside [`OpenProjectModal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenProjectButton {
    /// Replace the project in the current window (clean mode).
    ThisWindow,
    /// Save all dirty documents, then switch the current window to the target project.
    SaveAndSwitch,
    /// Discard all unsaved edits and switch the current window to the target project.
    DiscardAndSwitch,
    /// Open the target project in a new window, leaving the current window untouched.
    NewWindow,
    /// Close the modal without switching or discarding edits.
    Cancel,
}

impl OpenProjectButton {
    /// Human-readable button caption.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ThisWindow => "This Window",
            Self::SaveAndSwitch => "Save & Switch",
            Self::DiscardAndSwitch => "Discard & Switch",
            Self::NewWindow => "New Window",
            Self::Cancel => "Cancel",
        }
    }

    /// Stable GPUI element identifier for this button.
    #[must_use]
    pub const fn element_id(self) -> &'static str {
        match self {
            Self::ThisWindow => "open-project-this-window",
            Self::SaveAndSwitch => "open-project-save-switch",
            Self::DiscardAndSwitch => "open-project-discard-switch",
            Self::NewWindow => "open-project-new-window",
            Self::Cancel => "open-project-cancel",
        }
    }
}

/// One unsaved open document displayed in dirty mode of [`OpenProjectModal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenProjectDirtyEntry {
    /// Display path or buffer key shown to the user.
    pub display_name: String,
    /// `true` when the document has no file path on disk (`doc.path.is_none()`).
    pub untitled: bool,
}

/// Variant of [`OpenProjectModal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenProjectModalMode {
    /// A project is already open in the window, with no unsaved edits.
    Clean,
    /// One or more open documents have unsaved edits.
    Dirty {
        /// Sorted list of dirty documents.
        dirty_files: Vec<OpenProjectDirtyEntry>,
        /// Whether *Save & Switch* is available (requires a configured `SaveSink`).
        can_save: bool,
    },
}

/// Result of feeding one key to [`OpenProjectModal::handle_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenProjectKeyOutcome {
    /// Focus moved between dialog buttons.
    Consumed,
    /// `Enter` activated an enabled button.
    Activate(OpenProjectButton),
    /// `Escape` requested dismissing the modal.
    Cancel,
    /// Key has no special binding in the dialog (still swallowed by the modal).
    Ignored,
}

/// Headless state and keyboard controller of the *Open Project* dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenProjectModal {
    target_root: PathBuf,
    target_name: String,
    mode: OpenProjectModalMode,
    focused_index: usize,
    saving: bool,
    error: Option<String>,
}

const CLEAN_BUTTONS: &[OpenProjectButton] = &[
    OpenProjectButton::ThisWindow,
    OpenProjectButton::NewWindow,
    OpenProjectButton::Cancel,
];

const DIRTY_BUTTONS: &[OpenProjectButton] = &[
    OpenProjectButton::SaveAndSwitch,
    OpenProjectButton::DiscardAndSwitch,
    OpenProjectButton::NewWindow,
    OpenProjectButton::Cancel,
];

impl OpenProjectModal {
    /// Opens the dialog in clean mode (*This Window* | *New Window* | *Cancel*).
    #[must_use]
    pub fn new_clean(target_root: PathBuf) -> Self {
        let target_name = project_display_name(&target_root);
        Self {
            target_root,
            target_name,
            mode: OpenProjectModalMode::Clean,
            focused_index: 0,
            saving: false,
            error: None,
        }
    }

    /// Opens the dialog in dirty mode (*Save & Switch* | *Discard & Switch* | *New Window* | *Cancel*).
    #[must_use]
    pub fn new_dirty(
        target_root: PathBuf,
        dirty_files: Vec<OpenProjectDirtyEntry>,
        can_save: bool,
    ) -> Self {
        let target_name = project_display_name(&target_root);
        let focused_index = usize::from(!can_save);
        Self {
            target_root,
            target_name,
            mode: OpenProjectModalMode::Dirty {
                dirty_files,
                can_save,
            },
            focused_index,
            saving: false,
            error: None,
        }
    }

    /// Target project root directory.
    #[must_use]
    pub const fn target_root(&self) -> &PathBuf {
        &self.target_root
    }

    /// Display name of the target project.
    #[must_use]
    pub fn target_name(&self) -> &str {
        &self.target_name
    }

    /// Current modal mode (clean vs dirty).
    #[must_use]
    pub const fn mode(&self) -> &OpenProjectModalMode {
        &self.mode
    }

    /// Whether a batch *Save & Switch* is currently in progress.
    #[must_use]
    pub const fn is_saving(&self) -> bool {
        self.saving
    }

    /// Optional error notice shown inside the modal (e.g. after a failed save).
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Updates the in-progress saving flag.
    pub const fn set_saving(&mut self, saving: bool) {
        self.saving = saving;
    }

    /// Sets or clears the modal error banner.
    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }

    /// Ordered button list for the current mode.
    #[must_use]
    pub const fn buttons(&self) -> &'static [OpenProjectButton] {
        match self.mode {
            OpenProjectModalMode::Clean => CLEAN_BUTTONS,
            OpenProjectModalMode::Dirty { .. } => DIRTY_BUTTONS,
        }
    }

    /// Currently focused button.
    #[must_use]
    pub fn focused_button(&self) -> OpenProjectButton {
        let buttons = self.buttons();
        buttons[self.focused_index.min(buttons.len() - 1)]
    }

    /// Whether `button` can currently be activated.
    #[must_use]
    pub const fn is_button_enabled(&self, button: OpenProjectButton) -> bool {
        if self.saving {
            return matches!(button, OpenProjectButton::Cancel);
        }
        match button {
            OpenProjectButton::SaveAndSwitch => {
                matches!(
                    self.mode,
                    OpenProjectModalMode::Dirty { can_save: true, .. }
                )
            }
            OpenProjectButton::ThisWindow
            | OpenProjectButton::DiscardAndSwitch
            | OpenProjectButton::NewWindow
            | OpenProjectButton::Cancel => true,
        }
    }

    /// Whether any dirty entry has no path on disk (`doc.path.is_none()`).
    #[must_use]
    pub fn has_untitled_dirty(&self) -> bool {
        match &self.mode {
            OpenProjectModalMode::Clean => false,
            OpenProjectModalMode::Dirty { dirty_files, .. } => {
                dirty_files.iter().any(|entry| entry.untitled)
            }
        }
    }

    /// Returns `(visible_entries, overflow_count)` capped at [`MAX_DIRTY_PREVIEW_ITEMS`].
    #[must_use]
    pub fn dirty_preview(&self) -> (&[OpenProjectDirtyEntry], usize) {
        match &self.mode {
            OpenProjectModalMode::Clean => (&[], 0),
            OpenProjectModalMode::Dirty { dirty_files, .. } => {
                let shown = dirty_files.len().min(MAX_DIRTY_PREVIEW_ITEMS);
                (&dirty_files[..shown], dirty_files.len() - shown)
            }
        }
    }

    /// Handles one keystroke (`escape`, `tab`, `left`, `right`, `enter`).
    pub fn handle_key(&mut self, key: &str, shift: bool) -> OpenProjectKeyOutcome {
        match key {
            "escape" => OpenProjectKeyOutcome::Cancel,
            "tab" => {
                self.cycle_focus(!shift);
                OpenProjectKeyOutcome::Consumed
            }
            "right" | "down" => {
                self.cycle_focus(true);
                OpenProjectKeyOutcome::Consumed
            }
            "left" | "up" => {
                self.cycle_focus(false);
                OpenProjectKeyOutcome::Consumed
            }
            "enter" => {
                let button = self.focused_button();
                if self.is_button_enabled(button) {
                    OpenProjectKeyOutcome::Activate(button)
                } else {
                    OpenProjectKeyOutcome::Ignored
                }
            }
            _ => OpenProjectKeyOutcome::Ignored,
        }
    }

    fn cycle_focus(&mut self, forward: bool) {
        let buttons = self.buttons();
        let len = buttons.len();
        if len == 0 {
            return;
        }
        for step in 1..=len {
            let next = if forward {
                (self.focused_index + step) % len
            } else {
                (self.focused_index + len - (step % len)) % len
            };
            if self.is_button_enabled(buttons[next]) {
                self.focused_index = next;
                return;
            }
        }
    }
}

/// Callback invoked when a button inside [`OpenProjectModal`] is clicked.
pub(super) type OpenProjectActionCallback = Rc<dyn Fn(OpenProjectButton, &mut Window, &mut App)>;

/// Callback invoked when the modal backdrop is clicked.
pub(super) type OpenProjectCancelCallback = Rc<dyn Fn(&mut Window, &mut App)>;

/// Renders [`OpenProjectModal`] as a centered GPUI modal overlay.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "declarative modal layout; keeping header, dirty preview, banners, and footer together is clearer"
)]
pub(super) fn render_open_project_modal(
    modal: &OpenProjectModal,
    theme: &KovaTheme,
    on_action: &OpenProjectActionCallback,
    on_cancel: OpenProjectCancelCallback,
) -> Stateful<Div> {
    let cp = ControlPalette::from_theme(theme);
    let elevated = hsla_from_rgba(theme.surfaces().elevated);
    let border_strong = hsla_from_rgba(theme.borders().island);
    let border_subtle = hsla_from_rgba(theme.borders().subtle);
    let mut backdrop = hsla_from_rgba(theme.surfaces().overlay);
    backdrop.a = backdrop.a.min(0.55);

    let (title, prompt) = match modal.mode() {
        OpenProjectModalMode::Clean => (
            "Open Project",
            format!("Open project '{}'?", modal.target_name()),
        ),
        OpenProjectModalMode::Dirty { .. } => (
            "Open Project — Unsaved Changes",
            format!(
                "You have unsaved changes. Save before opening '{}'?",
                modal.target_name()
            ),
        ),
    };

    let mut card = div()
        .id("open-project-card")
        .w(px(MODAL_WIDTH_PX))
        .max_w_full()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .p(px(16.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(border_strong)
        .bg(elevated)
        .shadow_lg()
        .occlude()
        // Clicks inside the dialog must never dismiss via the backdrop.
        .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
            cx.stop_propagation();
        })
        .on_click(|_: &ClickEvent, _, cx| cx.stop_propagation())
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(px(14.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(cp.text)
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(cp.text_secondary)
                        .child(prompt),
                )
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(cp.text_muted)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(modal.target_root().display().to_string()),
                ),
        );

    if let OpenProjectModalMode::Dirty { can_save, .. } = modal.mode() {
        let (preview, overflow) = modal.dirty_preview();
        let mut list = div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .p(px(8.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(border_subtle)
            .bg(cp.input);

        for entry in preview {
            let mut row = div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .text_size(px(11.5))
                .text_color(cp.text)
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(entry.display_name.clone()),
                );
            if entry.untitled {
                row = row.child(Badge::new("untitled", PluginVariant::Warning).render(&cp));
            }
            list = list.child(row);
        }
        if overflow > 0 {
            list = list.child(
                div()
                    .text_size(px(11.0))
                    .text_color(cp.text_muted)
                    .child(format!("+{overflow} more")),
            );
        }
        card = card.child(list);

        if modal.has_untitled_dirty() {
            card = card.child(
                div()
                    .text_size(px(11.5))
                    .text_color(cp.text_secondary)
                    .child("Untitled buffers will prompt for Save As before switching."),
            );
        }
        if !can_save {
            card = card.child(
                div()
                    .px(px(8.0))
                    .py(px(6.0))
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(cp.warning.border)
                    .bg(cp.warning.subtle_bg)
                    .text_size(px(11.5))
                    .text_color(cp.warning.subtle_fg)
                    .child("Save & Switch is unavailable because no save sink is configured."),
            );
        }
    }

    if modal.is_saving() {
        card = card.child(
            div()
                .text_size(px(11.5))
                .text_color(cp.primary.subtle_fg)
                .child("Saving modified documents…"),
        );
    }

    if let Some(error) = modal.error() {
        card = card.child(
            div()
                .px(px(8.0))
                .py(px(6.0))
                .rounded(px(5.0))
                .border_1()
                .border_color(cp.danger.border)
                .bg(cp.danger.subtle_bg)
                .text_size(px(11.5))
                .text_color(cp.danger.subtle_fg)
                .child(error.to_owned()),
        );
    }

    let focused = modal.focused_button();
    let mut actions = div().flex().items_center().justify_end().gap(px(6.0));
    for &button in modal.buttons() {
        let enabled = modal.is_button_enabled(button);
        let is_focused = button == focused;
        let label: SharedString = if button == OpenProjectButton::SaveAndSwitch && modal.is_saving()
        {
            "Saving…".into()
        } else {
            button.label().into()
        };
        let btn_widget = match button {
            OpenProjectButton::ThisWindow | OpenProjectButton::SaveAndSwitch => Button::new(
                label,
                if enabled {
                    PluginVariant::Primary
                } else {
                    PluginVariant::Muted
                },
            ),
            OpenProjectButton::DiscardAndSwitch => Button::new(label, PluginVariant::Error),
            OpenProjectButton::NewWindow => Button::new(label, PluginVariant::Muted),
            OpenProjectButton::Cancel => Button::ghost(label, PluginVariant::Muted),
        };
        let action_cb = Rc::clone(on_action);
        let btn_el = btn_widget
            .render(button.element_id(), &cp)
            .when(!enabled, |el| el.opacity(0.45).cursor_default())
            .when(enabled, move |el| {
                el.on_click(move |_: &ClickEvent, window, cx| action_cb(button, window, cx))
            });

        actions = actions.child(
            div()
                .p(px(2.0))
                .rounded(px(7.0))
                .border_1()
                .border_color(if is_focused && enabled {
                    cp.focus
                } else {
                    gpui::transparent_black()
                })
                .child(btn_el),
        );
    }
    card = card.child(actions);

    div()
        .id("open-project-overlay")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .bg(backdrop)
        .child(
            div()
                .id("open-project-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .on_mouse_down(MouseButton::Left, move |_: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    on_cancel(window, cx);
                }),
        )
        .child(card)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirty_entry(name: &str, untitled: bool) -> OpenProjectDirtyEntry {
        OpenProjectDirtyEntry {
            display_name: name.to_owned(),
            untitled,
        }
    }

    #[test]
    fn clean_modal_buttons_and_keyboard_navigation() {
        let mut modal = OpenProjectModal::new_clean(PathBuf::from("/work/billing"));
        assert_eq!(modal.target_name(), "billing");
        assert_eq!(
            modal.buttons(),
            [
                OpenProjectButton::ThisWindow,
                OpenProjectButton::NewWindow,
                OpenProjectButton::Cancel,
            ]
        );
        assert_eq!(modal.focused_button(), OpenProjectButton::ThisWindow);

        assert_eq!(
            modal.handle_key("tab", false),
            OpenProjectKeyOutcome::Consumed
        );
        assert_eq!(modal.focused_button(), OpenProjectButton::NewWindow);

        assert_eq!(
            modal.handle_key("right", false),
            OpenProjectKeyOutcome::Consumed
        );
        assert_eq!(modal.focused_button(), OpenProjectButton::Cancel);

        // Wrap forward.
        assert_eq!(
            modal.handle_key("tab", false),
            OpenProjectKeyOutcome::Consumed
        );
        assert_eq!(modal.focused_button(), OpenProjectButton::ThisWindow);

        // Shift+Tab / Left wraps backward.
        assert_eq!(
            modal.handle_key("left", false),
            OpenProjectKeyOutcome::Consumed
        );
        assert_eq!(modal.focused_button(), OpenProjectButton::Cancel);

        assert_eq!(
            modal.handle_key("tab", true),
            OpenProjectKeyOutcome::Consumed
        );
        assert_eq!(modal.focused_button(), OpenProjectButton::NewWindow);

        assert_eq!(
            modal.handle_key("enter", false),
            OpenProjectKeyOutcome::Activate(OpenProjectButton::NewWindow)
        );
        assert_eq!(
            modal.handle_key("escape", false),
            OpenProjectKeyOutcome::Cancel
        );
        assert_eq!(modal.handle_key("a", false), OpenProjectKeyOutcome::Ignored);
    }

    #[test]
    fn dirty_modal_buttons_truncation_and_untitled_detection() {
        let entries: Vec<_> = (1..=8)
            .map(|i| dirty_entry(&format!("src/File{i}.java"), i == 1))
            .collect();
        let mut modal = OpenProjectModal::new_dirty(PathBuf::from("/work/billing"), entries, true);
        assert_eq!(
            modal.buttons(),
            [
                OpenProjectButton::SaveAndSwitch,
                OpenProjectButton::DiscardAndSwitch,
                OpenProjectButton::NewWindow,
                OpenProjectButton::Cancel,
            ]
        );
        assert!(modal.has_untitled_dirty());
        let (preview, overflow) = modal.dirty_preview();
        assert_eq!(preview.len(), MAX_DIRTY_PREVIEW_ITEMS);
        assert_eq!(overflow, 3);
        assert_eq!(modal.focused_button(), OpenProjectButton::SaveAndSwitch);

        assert_eq!(
            modal.handle_key("enter", false),
            OpenProjectKeyOutcome::Activate(OpenProjectButton::SaveAndSwitch)
        );

        // While saving, non-Cancel buttons are disabled.
        modal.set_saving(true);
        assert!(!modal.is_button_enabled(OpenProjectButton::SaveAndSwitch));
        assert!(!modal.is_button_enabled(OpenProjectButton::DiscardAndSwitch));
        assert!(!modal.is_button_enabled(OpenProjectButton::NewWindow));
        assert!(modal.is_button_enabled(OpenProjectButton::Cancel));
        assert_eq!(
            modal.handle_key("enter", false),
            OpenProjectKeyOutcome::Ignored
        );
        assert_eq!(
            modal.handle_key("tab", false),
            OpenProjectKeyOutcome::Consumed
        );
        assert_eq!(modal.focused_button(), OpenProjectButton::Cancel);
        assert_eq!(
            modal.handle_key("enter", false),
            OpenProjectKeyOutcome::Activate(OpenProjectButton::Cancel)
        );
    }

    #[test]
    fn dirty_modal_without_save_sink_skips_disabled_save_button() {
        let mut modal = OpenProjectModal::new_dirty(
            PathBuf::from("/work/billing"),
            vec![dirty_entry("Main.java", false)],
            false,
        );
        assert!(!modal.has_untitled_dirty());
        assert!(!modal.is_button_enabled(OpenProjectButton::SaveAndSwitch));
        assert_eq!(modal.focused_button(), OpenProjectButton::DiscardAndSwitch);

        // Cycling backward from DiscardAndSwitch skips SaveAndSwitch -> Cancel.
        modal.handle_key("left", false);
        assert_eq!(modal.focused_button(), OpenProjectButton::Cancel);

        // Cycling forward wraps around SaveAndSwitch -> DiscardAndSwitch.
        modal.handle_key("right", false);
        assert_eq!(modal.focused_button(), OpenProjectButton::DiscardAndSwitch);
    }
}
