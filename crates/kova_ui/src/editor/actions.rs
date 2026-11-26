//! GPUI actions for [`crate::editor::EditorView`] (Phase 0 base, extensible by Phase 1 keymaps).
//!
//! Clipboard, Select All and Undo/Redo are dispatched only through these actions (bound by
//! [`bind_default_keys`]); navigation and editing keys are handled from the raw
//! `KeyDownEvent` until the Phase 1 `JetBrains` keymap takes over. Printable characters and
//! dead keys always arrive through `EntityInputHandler`.

use gpui::{App, KeyBinding};

/// Key context set on the editor root element; default bindings apply only inside it.
pub const EDITOR_KEY_CONTEXT: &str = "KovaEditor";

/// Key context set on the workspace shell root (window-wide commands such as `Save`).
pub const WORKSPACE_KEY_CONTEXT: &str = "KovaWorkspace";

gpui::actions!(
    kova_workspace,
    [
        /// Atomically saves the active document (`Cmd+S` / `Ctrl+S`, `KOVA-016`).
        Save,
    ]
);

gpui::actions!(
    kova_editor,
    [
        /// Copies all selections to the system clipboard.
        Copy,
        /// Copies all selections and deletes them as one undo step.
        Cut,
        /// Pastes the system clipboard at every cursor.
        Paste,
        /// Selects the whole buffer.
        SelectAll,
        /// Reverts the latest edit transaction.
        Undo,
        /// Re-applies the latest undone transaction.
        Redo,
        /// Moves every cursor one grapheme left.
        MoveLeft,
        /// Moves every cursor one grapheme right.
        MoveRight,
        /// Moves every cursor one line up.
        MoveUp,
        /// Moves every cursor one line down.
        MoveDown,
        /// Collapses multiple cursors or clears the selection.
        Escape,
    ]
);

/// Returns the Phase 0 platform bindings (`Cmd` on macOS, `Ctrl` elsewhere) for editor and
/// workspace actions.
#[must_use]
pub fn default_key_bindings() -> Vec<KeyBinding> {
    let ctx = Some(EDITOR_KEY_CONTEXT);
    let workspace = Some(WORKSPACE_KEY_CONTEXT);
    if cfg!(target_os = "macos") {
        vec![
            KeyBinding::new("cmd-s", Save, workspace),
            KeyBinding::new("cmd-c", Copy, ctx),
            KeyBinding::new("cmd-x", Cut, ctx),
            KeyBinding::new("cmd-v", Paste, ctx),
            KeyBinding::new("cmd-a", SelectAll, ctx),
            KeyBinding::new("cmd-z", Undo, ctx),
            KeyBinding::new("cmd-shift-z", Redo, ctx),
        ]
    } else {
        vec![
            KeyBinding::new("ctrl-s", Save, workspace),
            KeyBinding::new("ctrl-c", Copy, ctx),
            KeyBinding::new("ctrl-x", Cut, ctx),
            KeyBinding::new("ctrl-v", Paste, ctx),
            KeyBinding::new("ctrl-a", SelectAll, ctx),
            KeyBinding::new("ctrl-z", Undo, ctx),
            KeyBinding::new("ctrl-y", Redo, ctx),
            KeyBinding::new("ctrl-shift-z", Redo, ctx),
        ]
    }
}

/// Registers [`default_key_bindings`] on the application keymap.
pub fn bind_default_keys(cx: &mut App) {
    cx.bind_keys(default_key_bindings());
}
