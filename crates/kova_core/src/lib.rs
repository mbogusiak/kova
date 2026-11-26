//! `kova_core` — project model, Actions, `JetBrains` keymap and modal Vim engine.
//!
//! This crate belongs to the **Headless Core**: it must compile and pass its
//! tests without any windowing or GPU dependency (enforced by KOVA-003).

pub mod action;
pub mod builtin_actions;
pub mod completion;
pub mod context;
pub mod keymap;
pub mod markdown;
pub mod navigation;
pub mod perf;
pub mod project;
pub mod vim;

pub use action::{
    ActionCategory, ActionDescriptor, ActionError, ActionId, ActionRegistry, ActionSource,
};
pub use builtin_actions::{
    builtin_action_ids, builtin_action_registry, builtin_action_registry_ref,
    register_builtin_actions,
};
pub use completion::{
    CompletionContext, CompletionProposal, CompletionProposalKind, CompletionSeparator,
    CompletionSource, CompletionTextEdit, CompletionTriggerKind, LocalCompletionResult,
    RankedProposal, ReceiverAccessKind, ReceiverTarget, extract_completion_context,
    filter_and_rank_proposals, resolve_local_completion, score_completion_match,
    should_auto_trigger_completion,
};
pub use context::{ContextExpr, ContextExprError, ContextState, FocusContext, FocusStack};
pub use keymap::{
    BindingRule, BindingTarget, KeyChord, KeyCode, KeyDispatch, KeyStep, KeyStroke,
    KeymapDiagnostic, KeymapDiagnosticKind, KeymapLayer, KeymapLayerKind, KeymapParseError,
    KeymapPresetMode, KeymapResolver, KeymapSession, ModifierTap, ModifierTapDetector, Modifiers,
    Platform, build_effective_keymap, jetbrains_keymap_layer, jetbrains_keymap_layer_ref, presets,
};
pub use markdown::{
    InlineSpan, ListItem, ListMarkerKind, MarkdownBlock, MarkdownDocument, MarkdownViewMode,
    TableAlignment, TableBlock, is_markdown_path, parse_markdown,
};
pub use navigation::{
    NavLocation, NavigationHistory, ResolvedLocation, SourceLanguageKind, SymbolQuery,
    extract_symbol_query, navigable_identifier_at_byte, resolve_local_definition,
};
pub use project::{
    ActiveProject, BuildSystemKind, DEFAULT_MAX_ENTRIES, MAX_RECENT_FILES,
    MAX_RECENT_PROJECTS_IN_MENU, MAX_RECENT_WORKSPACES, OpenTabState, Project, ProjectError,
    ProjectFileEntry, ProjectGeneration, ProjectMetadata, ProjectNode, ProjectNodeId,
    ProjectNodeKind, ProjectPhase, ProjectScanOptions, ProjectTree, RecentProject,
    RecentProjectsStore, RecentWorkspaceEntry, RecentWorkspaces, SESSION_SCHEMA_VERSION,
    SessionLayoutState, WorkspaceSession, load_recent_workspaces, load_workspace_session,
    load_workspace_session_or_default, project_display_name, record_recent_workspace,
    save_recent_workspaces, save_workspace_session, scan_project,
};

pub use vim::{
    KeyConflictPolicy, Register, VimCursorShape, VimEngine, VimKey, VimMode, VimOutcome, VimStatus,
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
