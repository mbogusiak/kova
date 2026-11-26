//! Git presentation layer (KOVA-043, KOVA-060): gutter diff markers, inline blame, the
//! `Cmd+K` commit panel, and the Git Branches popup. All `gix` work runs in `kova_tools` on
//! the background executor.

pub mod branches_popup;
pub mod commit_panel;
pub mod gutter;

use gpui::{App, KeyBinding};
use kova_core::builtin_actions::ids;
use kova_core::keymap::presets::jetbrains_keymap_layer_ref;
use kova_core::{KeyChord, KeyStep, Platform};

pub use branches_popup::{
    BranchesKeyOutcome, BranchesMode, BranchesPopup, BranchesPopupEvent, BranchesPopupModel,
    BranchesRow, UnsavedFilesCheck,
};
pub use commit_panel::{CheckState, CommitPanel, CommitPanelEvent, CommitPanelModel, StageOp};
pub use gutter::EditorGitState;

/// Key context set on the workspace shell root; workspace-wide bindings apply inside it.
pub const WORKSPACE_KEY_CONTEXT: &str = "KovaWorkspace";

/// Key context set on the commit panel root.
pub const COMMIT_PANEL_KEY_CONTEXT: &str = "KovaCommitPanel";

/// Key context set on the Git Branches popup card.
pub const BRANCHES_POPUP_KEY_CONTEXT: &str = "KovaBranchesPopup";

gpui::actions!(
    kova_git,
    [
        /// Opens or closes the commit panel (`git.commit`, `Cmd+K` / `Ctrl+K`).
        ToggleCommitPanel,
        /// Closes the commit panel.
        CloseCommitPanel,
        /// Commits the staged changes with the typed message.
        CommitStaged,
        /// Opens or closes the Git Branches popup (`git.branches`).
        ToggleBranchesPopup,
        /// Closes the Git Branches popup.
        CloseBranchesPopup,
    ]
);

/// Converts a single-stroke `kova_core` chord (`cmd+k`) to GPUI keystroke syntax (`cmd-k`).
#[must_use]
pub fn gpui_keystroke(chord: &KeyChord) -> Option<String> {
    match chord.steps() {
        [KeyStep::Stroke(stroke)] => Some(stroke.to_string().replace('+', "-")),
        _ => None,
    }
}

/// Returns the GPUI keystroke for `git.commit`, taken from the built-in `JetBrains`
/// preset of `platform` in `kova_core` (so the two never drift).
#[must_use]
pub fn commit_panel_keystroke(platform: Platform) -> String {
    jetbrains_keymap_layer_ref(platform)
        .chord_for_action(ids::GIT_COMMIT)
        .and_then(gpui_keystroke)
        .unwrap_or_else(|| match platform {
            Platform::MacOs => "cmd-k".to_owned(),
            Platform::Linux => "ctrl-k".to_owned(),
        })
}

/// Returns the GPUI keystroke for `git.branches`, taken from the built-in `JetBrains`
/// preset of `platform` in `kova_core`.
#[must_use]
pub fn branches_popup_keystroke(platform: Platform) -> String {
    jetbrains_keymap_layer_ref(platform)
        .chord_for_action(ids::GIT_BRANCHES)
        .and_then(gpui_keystroke)
        .unwrap_or_else(|| match platform {
            Platform::MacOs => "ctrl-v".to_owned(),
            Platform::Linux => "alt-`".to_owned(),
        })
}

/// Returns the default Git bindings for the host platform.
#[must_use]
pub fn default_key_bindings() -> Vec<KeyBinding> {
    let platform = Platform::current();
    let commit_key = if platform == Platform::MacOs {
        "cmd-enter"
    } else {
        "ctrl-enter"
    };
    vec![
        KeyBinding::new(
            &commit_panel_keystroke(platform),
            ToggleCommitPanel,
            Some(WORKSPACE_KEY_CONTEXT),
        ),
        KeyBinding::new(
            &branches_popup_keystroke(platform),
            ToggleBranchesPopup,
            Some(WORKSPACE_KEY_CONTEXT),
        ),
        KeyBinding::new("escape", CloseCommitPanel, Some(COMMIT_PANEL_KEY_CONTEXT)),
        KeyBinding::new(commit_key, CommitStaged, Some(COMMIT_PANEL_KEY_CONTEXT)),
        KeyBinding::new(
            "escape",
            CloseBranchesPopup,
            Some(BRANCHES_POPUP_KEY_CONTEXT),
        ),
    ]
}

/// Registers [`default_key_bindings`] on the application keymap.
pub fn bind_default_keys(cx: &mut App) {
    cx.bind_keys(default_key_bindings());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_panel_shortcut_mirrors_kova_core_preset() {
        assert_eq!(commit_panel_keystroke(Platform::MacOs), "cmd-k");
        assert_eq!(commit_panel_keystroke(Platform::Linux), "ctrl-k");
        assert_eq!(branches_popup_keystroke(Platform::MacOs), "ctrl-v");
        assert_eq!(branches_popup_keystroke(Platform::Linux), "alt-`");
    }

    #[test]
    fn converts_only_single_strokes() {
        let chord = KeyChord::parse("cmd+shift+k", Platform::MacOs).expect("valid chord");
        assert_eq!(gpui_keystroke(&chord).as_deref(), Some("shift-cmd-k"));
        let sequence = KeyChord::parse("ctrl+k ctrl+c", Platform::Linux).expect("valid chord");
        assert_eq!(gpui_keystroke(&sequence), None);
    }
}
