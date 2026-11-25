//! `kova_core` — project model, Actions, `JetBrains` keymap and modal Vim engine.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod action;
pub mod builtin_actions;
pub mod context;
pub mod keymap;
pub mod perf;

pub use action::{
    ActionCategory, ActionDescriptor, ActionError, ActionId, ActionRegistry, ActionSource,
};
pub use builtin_actions::{
    builtin_action_ids, builtin_action_registry, builtin_action_registry_ref,
    register_builtin_actions,
};
pub use context::{ContextExpr, ContextExprError, ContextState, FocusContext, FocusStack};
pub use keymap::{
    BindingRule, BindingTarget, KeyChord, KeyCode, KeyDispatch, KeyStep, KeyStroke,
    KeymapDiagnostic, KeymapDiagnosticKind, KeymapLayer, KeymapLayerKind, KeymapParseError,
    KeymapPresetMode, KeymapResolver, KeymapSession, ModifierTap, ModifierTapDetector, Modifiers,
    Platform, build_effective_keymap, jetbrains_keymap_layer, jetbrains_keymap_layer_ref, presets,
};

/// Name of this crate as declared in its `Cargo.toml`.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::CRATE_NAME;

    #[test]
    fn crate_name_matches_manifest() {
        assert_eq!(CRATE_NAME, "kova_core");
    }

    #[test]
    fn inherits_workspace_license() {
        assert_eq!(env!("CARGO_PKG_LICENSE"), "MIT OR Apache-2.0");
    }
}
