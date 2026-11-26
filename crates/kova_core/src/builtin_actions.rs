//! Canonical built-in IDE actions registered by `kova_core`.
//!
//! Covers navigation, text editing, multi-cursor, code folding, refactoring,
//! run/debug, tool windows, Omarchy focus/tiling, Git, and Vim toggle actions.

use std::sync::OnceLock;

use crate::action::{ActionCategory, ActionDescriptor, ActionError, ActionId, ActionRegistry};
use crate::context::{ContextExpr, FocusContext};

static BUILTIN_REGISTRY: OnceLock<ActionRegistry> = OnceLock::new();

/// String constants for all canonical built-in [`ActionId`]s so downstream
/// crates (`kova_ui`, Vim engine, keymap presets) avoid typo-prone raw strings.
pub mod ids {
    // --- Navigation & Search ---

    /// Open *Search Everywhere* modal.
    pub const NAVIGATION_SEARCH_EVERYWHERE: &str = "navigation.search_everywhere";
    /// Open *Find Action* command palette.
    pub const NAVIGATION_FIND_ACTION: &str = "navigation.find_action";
    /// Open *Go to Class* popup.
    pub const NAVIGATION_GO_TO_CLASS: &str = "navigation.go_to_class";
    /// Open *Go to File* popup.
    pub const NAVIGATION_GO_TO_FILE: &str = "navigation.go_to_file";
    /// Open *Go to Symbol* popup.
    pub const NAVIGATION_GO_TO_SYMBOL: &str = "navigation.go_to_symbol";
    /// Jump to a specific line and column in the active editor.
    pub const NAVIGATION_GO_TO_LINE: &str = "navigation.go_to_line";
    /// Jump to symbol declaration or usages.
    pub const NAVIGATION_GO_TO_DECLARATION: &str = "navigation.go_to_declaration";
    /// Jump to symbol implementation(s).
    pub const NAVIGATION_GO_TO_IMPLEMENTATION: &str = "navigation.go_to_implementation";
    /// Jump to super method or superclass.
    pub const NAVIGATION_GO_TO_SUPER: &str = "navigation.go_to_super";
    /// Find all usages of symbol under caret.
    pub const NAVIGATION_FIND_USAGES: &str = "navigation.find_usages";
    /// Navigate back in cursor history.
    pub const NAVIGATION_BACK: &str = "navigation.back";
    /// Navigate forward in cursor history.
    pub const NAVIGATION_FORWARD: &str = "navigation.forward";
    /// Open *Recent Files* popup.
    pub const NAVIGATION_RECENT_FILES: &str = "navigation.recent_files";
    /// Open *File Structure* popup for the active file.
    pub const NAVIGATION_FILE_STRUCTURE: &str = "navigation.file_structure";
    /// Jump to next highlighted error or warning.
    pub const NAVIGATION_NEXT_ERROR: &str = "navigation.next_error";
    /// Jump to previous highlighted error or warning.
    pub const NAVIGATION_PREV_ERROR: &str = "navigation.prev_error";
    /// Select next editor tab.
    pub const NAVIGATION_NEXT_TAB: &str = "navigation.next_tab";
    /// Select previous editor tab.
    pub const NAVIGATION_PREV_TAB: &str = "navigation.prev_tab";

    // --- Editor, Search & Multi-Cursor ---

    /// Save the active buffer or all dirty files.
    pub const EDITOR_SAVE: &str = "editor.save";
    /// Undo the last edit operation.
    pub const EDITOR_UNDO: &str = "editor.undo";
    /// Redo the last undone edit operation.
    pub const EDITOR_REDO: &str = "editor.redo";
    /// Cut selection (or current line) to clipboard.
    pub const EDITOR_CUT: &str = "editor.cut";
    /// Copy selection (or current line) to clipboard.
    pub const EDITOR_COPY: &str = "editor.copy";
    /// Paste from clipboard at carets.
    pub const EDITOR_PASTE: &str = "editor.paste";
    /// Select entire document.
    pub const EDITOR_SELECT_ALL: &str = "editor.select_all";
    /// Dismiss active inline popup, multi-cursor, or selection.
    pub const EDITOR_ESCAPE: &str = "editor.escape";
    /// Duplicate current line or selection.
    pub const EDITOR_DUPLICATE_LINE: &str = "editor.duplicate_line";
    /// Delete current line at caret.
    pub const EDITOR_DELETE_LINE: &str = "editor.delete_line";
    /// Toggle line comment for current line or selection.
    pub const EDITOR_TOGGLE_LINE_COMMENT: &str = "editor.toggle_line_comment";
    /// Toggle block comment around selection.
    pub const EDITOR_TOGGLE_BLOCK_COMMENT: &str = "editor.toggle_block_comment";
    /// Expand selection outward using syntax AST boundaries.
    pub const EDITOR_EXTEND_SELECTION: &str = "editor.extend_selection";
    /// Shrink selection back to previous scope.
    pub const EDITOR_SHRINK_SELECTION: &str = "editor.shrink_selection";
    /// Move current line or selection up by one line.
    pub const EDITOR_MOVE_LINE_UP: &str = "editor.move_line_up";
    /// Move current line or selection down by one line.
    pub const EDITOR_MOVE_LINE_DOWN: &str = "editor.move_line_down";
    /// Move current statement or block up.
    pub const EDITOR_MOVE_STATEMENT_UP: &str = "editor.move_statement_up";
    /// Move current statement or block down.
    pub const EDITOR_MOVE_STATEMENT_DOWN: &str = "editor.move_statement_down";
    /// Reformat active file or selection.
    pub const EDITOR_REFORMAT_CODE: &str = "editor.reformat_code";
    /// Optimize and sort imports in the active file.
    pub const EDITOR_OPTIMIZE_IMPORTS: &str = "editor.optimize_imports";
    /// Show context actions / quick fixes at caret.
    pub const EDITOR_SHOW_INTENTIONS: &str = "editor.show_intentions";
    /// Trigger basic code completion.
    pub const EDITOR_COMPLETE_BASIC: &str = "editor.complete_basic";
    /// Trigger type-aware smart code completion.
    pub const EDITOR_COMPLETE_SMART: &str = "editor.complete_smart";
    /// Show call parameter info tooltip.
    pub const EDITOR_PARAMETER_INFO: &str = "editor.parameter_info";
    /// Show quick documentation popup for symbol under caret.
    pub const EDITOR_QUICK_DOC: &str = "editor.quick_doc";
    /// Add next matching occurrence to multi-cursor selection.
    pub const EDITOR_ADD_NEXT_OCCURRENCE: &str = "editor.add_next_occurrence";
    /// Unselect last added occurrence in multi-cursor selection.
    pub const EDITOR_UNSELECT_OCCURRENCE: &str = "editor.unselect_occurrence";
    /// Select all occurrences of word/selection in file.
    pub const EDITOR_SELECT_ALL_OCCURRENCES: &str = "editor.select_all_occurrences";
    /// Clone caret one line above.
    pub const EDITOR_ADD_CURSOR_ABOVE: &str = "editor.add_cursor_above";
    /// Clone caret one line below.
    pub const EDITOR_ADD_CURSOR_BELOW: &str = "editor.add_cursor_below";
    /// Open inline find bar in active editor.
    pub const EDITOR_FIND: &str = "editor.find";
    /// Open inline replace bar in active editor.
    pub const EDITOR_REPLACE: &str = "editor.replace";
    /// Open workspace-wide *Find in Files* search.
    pub const EDITOR_FIND_IN_FILES: &str = "editor.find_in_files";
    /// Open workspace-wide *Replace in Files* search.
    pub const EDITOR_REPLACE_IN_FILES: &str = "editor.replace_in_files";
    /// Jump to next match in file search.
    pub const EDITOR_FIND_NEXT: &str = "editor.find_next";
    /// Jump to previous match in file search.
    pub const EDITOR_FIND_PREV: &str = "editor.find_prev";
    /// Collapse foldable region at caret.
    pub const EDITOR_FOLD_REGION: &str = "editor.fold_region";
    /// Expand foldable region at caret.
    pub const EDITOR_UNFOLD_REGION: &str = "editor.unfold_region";
    /// Collapse all foldable regions in file.
    pub const EDITOR_FOLD_ALL: &str = "editor.fold_all";
    /// Expand all foldable regions in file.
    pub const EDITOR_UNFOLD_ALL: &str = "editor.unfold_all";

    // --- Refactoring ---

    /// Rename symbol under caret across workspace.
    pub const REFACTOR_RENAME: &str = "refactor.rename";
    /// Extract expression into a local variable.
    pub const REFACTOR_EXTRACT_VARIABLE: &str = "refactor.extract_variable";
    /// Extract statements into a method/function.
    pub const REFACTOR_EXTRACT_METHOD: &str = "refactor.extract_method";
    /// Extract expression into a constant.
    pub const REFACTOR_EXTRACT_CONSTANT: &str = "refactor.extract_constant";
    /// Extract expression into a class field.
    pub const REFACTOR_EXTRACT_FIELD: &str = "refactor.extract_field";
    /// Inline variable, method, or constant.
    pub const REFACTOR_INLINE: &str = "refactor.inline";
    /// Change method/function signature.
    pub const REFACTOR_CHANGE_SIGNATURE: &str = "refactor.change_signature";

    // --- Run & Debug ---

    /// Run entry point or test at caret context.
    pub const RUN_RUN_CONTEXT: &str = "run.run_context";
    /// Re-run selected or last run configuration.
    pub const RUN_RUN_LAST: &str = "run.run_last";
    /// Debug entry point or test at caret context.
    pub const RUN_DEBUG_CONTEXT: &str = "run.debug_context";
    /// Re-debug selected or last run configuration.
    pub const RUN_DEBUG_LAST: &str = "run.debug_last";
    /// Stop active run or debug session.
    pub const RUN_STOP: &str = "run.stop";
    /// Toggle line breakpoint at current caret line.
    pub const DEBUG_TOGGLE_LINE_BREAKPOINT: &str = "debug.toggle_line_breakpoint";
    /// Step over current statement in debugger.
    pub const DEBUG_STEP_OVER: &str = "debug.step_over";
    /// Step into call in debugger.
    pub const DEBUG_STEP_INTO: &str = "debug.step_into";
    /// Step out of current frame in debugger.
    pub const DEBUG_STEP_OUT: &str = "debug.step_out";
    /// Resume execution in debugger.
    pub const DEBUG_RESUME: &str = "debug.resume";
    /// Evaluate expression in active debug frame.
    pub const DEBUG_EVALUATE_EXPRESSION: &str = "debug.evaluate_expression";

    // --- Tool Windows, View, Git & Vim ---

    /// Toggle or focus the *Project* tool window.
    pub const TOOL_WINDOW_PROJECT: &str = "tool_window.project";
    /// Toggle or focus the *Structure* tool window.
    pub const TOOL_WINDOW_STRUCTURE: &str = "tool_window.structure";
    /// Toggle or focus the *Run* tool window.
    pub const TOOL_WINDOW_RUN: &str = "tool_window.run";
    /// Toggle or focus the *Debug* tool window.
    pub const TOOL_WINDOW_DEBUG: &str = "tool_window.debug";
    /// Toggle or focus the *Problems* tool window.
    pub const TOOL_WINDOW_PROBLEMS: &str = "tool_window.problems";
    /// Toggle or focus the *Git* tool window.
    pub const TOOL_WINDOW_GIT: &str = "tool_window.git";
    /// Toggle or focus the *Terminal* tool window.
    pub const TOOL_WINDOW_TERMINAL: &str = "tool_window.terminal";
    /// Hide all tool window docks.
    pub const TOOL_WINDOW_HIDE_ALL: &str = "tool_window.hide_all";
    /// Toggle Omarchy distraction-free Focus/Tiling mode.
    pub const VIEW_TOGGLE_FOCUS_MODE: &str = "view.toggle_focus_mode";
    /// Open Settings / Preferences view.
    pub const VIEW_OPEN_SETTINGS: &str = "view.open_settings";
    /// Close active editor tab.
    pub const VIEW_CLOSE_TAB: &str = "view.close_tab";
    /// Open Git commit tool/dialog.
    pub const GIT_COMMIT: &str = "git.commit";
    /// Push committed changes to remote.
    pub const GIT_PUSH: &str = "git.push";
    /// Update / pull project changes from remote.
    pub const GIT_UPDATE_PROJECT: &str = "git.update_project";
    /// Rollback uncommitted changes in active selection or file.
    pub const GIT_ROLLBACK: &str = "git.rollback";
    /// Toggle modal Vim editing mode on or off.
    pub const VIM_TOGGLE: &str = "vim.toggle";

    // --- Focus / Tiling window management (KOVA-026) ---

    /// Toggle the dense Omarchy/Hyprland tiling layout (8 px margins, 6 px gaps, focus ring).
    pub const VIEW_TOGGLE_TILING_MODE: &str = "view.toggle_tiling_mode";
    /// Move keyboard focus to the island on the left of the focused one.
    pub const WINDOW_FOCUS_LEFT: &str = "window.focus_left";
    /// Move keyboard focus to the island on the right of the focused one.
    pub const WINDOW_FOCUS_RIGHT: &str = "window.focus_right";
    /// Move keyboard focus to the island above the focused one.
    pub const WINDOW_FOCUS_UP: &str = "window.focus_up";
    /// Move keyboard focus to the island below the focused one.
    pub const WINDOW_FOCUS_DOWN: &str = "window.focus_down";
    /// Move the active editor tab to the split on the left (creating it when missing).
    pub const WINDOW_MOVE_TAB_LEFT: &str = "window.move_tab_left";
    /// Move the active editor tab to the split on the right (creating it when missing).
    pub const WINDOW_MOVE_TAB_RIGHT: &str = "window.move_tab_right";
    /// Move the active editor tab to the split above (creating it when missing).
    pub const WINDOW_MOVE_TAB_UP: &str = "window.move_tab_up";
    /// Move the active editor tab to the split below (creating it when missing).
    pub const WINDOW_MOVE_TAB_DOWN: &str = "window.move_tab_down";
    /// Split the active editor pane side by side.
    pub const WINDOW_SPLIT_RIGHT: &str = "window.split_right";
    /// Split the active editor pane top / bottom.
    pub const WINDOW_SPLIT_DOWN: &str = "window.split_down";
    /// Move the nearest split divider of the active pane to the left.
    pub const WINDOW_RESIZE_LEFT: &str = "window.resize_left";
    /// Move the nearest split divider of the active pane to the right.
    pub const WINDOW_RESIZE_RIGHT: &str = "window.resize_right";
    /// Move the nearest split divider of the active pane up.
    pub const WINDOW_RESIZE_UP: &str = "window.resize_up";
    /// Move the nearest split divider of the active pane down.
    pub const WINDOW_RESIZE_DOWN: &str = "window.resize_down";
}

#[derive(Debug, Clone, Copy)]
enum WhenScope {
    Global,
    Workspace,
    Editor,
    EditorText,
    EditableText,
}

impl WhenScope {
    fn into_expr(self) -> Option<ContextExpr> {
        match self {
            Self::Global => None,
            Self::Workspace => Some(ContextExpr::focus(FocusContext::Workspace)),
            Self::Editor => Some(ContextExpr::focus(FocusContext::Editor)),
            Self::EditorText => Some(ContextExpr::focus(FocusContext::EditorText)),
            Self::EditableText => Some(ContextExpr::and(
                ContextExpr::focus(FocusContext::EditorText),
                ContextExpr::negated(ContextExpr::flag("editor_read_only")),
            )),
        }
    }
}

struct BuiltinActionSpec {
    id: &'static str,
    title: &'static str,
    category: ActionCategory,
    description: &'static str,
    scope: WhenScope,
}

const fn spec(
    id: &'static str,
    title: &'static str,
    category: ActionCategory,
    description: &'static str,
    scope: WhenScope,
) -> BuiltinActionSpec {
    BuiltinActionSpec {
        id,
        title,
        category,
        description,
        scope,
    }
}

const BUILTIN_SPECS: &[BuiltinActionSpec] = &[
    // Navigation & Search
    spec(
        ids::NAVIGATION_SEARCH_EVERYWHERE,
        "Search Everywhere",
        ActionCategory::Navigation,
        "Search classes, files, symbols, and IDE actions in one popup",
        WhenScope::Global,
    ),
    spec(
        ids::NAVIGATION_FIND_ACTION,
        "Find Action",
        ActionCategory::Navigation,
        "Search and execute any registered IDE action",
        WhenScope::Global,
    ),
    spec(
        ids::NAVIGATION_GO_TO_CLASS,
        "Go to Class",
        ActionCategory::Navigation,
        "Jump directly to a class, interface, enum, or record",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_GO_TO_FILE,
        "Go to File",
        ActionCategory::Navigation,
        "Open any file in the project by fuzzy path",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_GO_TO_SYMBOL,
        "Go to Symbol",
        ActionCategory::Navigation,
        "Jump directly to a method, field, or symbol",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_GO_TO_LINE,
        "Go to Line/Column",
        ActionCategory::Navigation,
        "Jump to a line and optional column in the active editor",
        WhenScope::Editor,
    ),
    spec(
        ids::NAVIGATION_GO_TO_DECLARATION,
        "Go to Declaration or Usages",
        ActionCategory::Navigation,
        "Navigate to symbol declaration or list usages",
        WhenScope::EditorText,
    ),
    spec(
        ids::NAVIGATION_GO_TO_IMPLEMENTATION,
        "Go to Implementation(s)",
        ActionCategory::Navigation,
        "Navigate to concrete implementations of an interface or method",
        WhenScope::EditorText,
    ),
    spec(
        ids::NAVIGATION_GO_TO_SUPER,
        "Go to Super Method/Class",
        ActionCategory::Navigation,
        "Navigate to the overridden method or superclass",
        WhenScope::EditorText,
    ),
    spec(
        ids::NAVIGATION_FIND_USAGES,
        "Find Usages",
        ActionCategory::Navigation,
        "Search all references to the symbol under caret",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_BACK,
        "Back",
        ActionCategory::Navigation,
        "Move back in navigation history",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_FORWARD,
        "Forward",
        ActionCategory::Navigation,
        "Move forward in navigation history",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_RECENT_FILES,
        "Recent Files",
        ActionCategory::Navigation,
        "Open popup listing recently visited files and tool windows",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_FILE_STRUCTURE,
        "File Structure",
        ActionCategory::Navigation,
        "Show outline popup of members in the active file",
        WhenScope::Editor,
    ),
    spec(
        ids::NAVIGATION_NEXT_ERROR,
        "Next Highlighted Error",
        ActionCategory::Navigation,
        "Jump to the next diagnostic error or warning",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_PREV_ERROR,
        "Previous Highlighted Error",
        ActionCategory::Navigation,
        "Jump to the previous diagnostic error or warning",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_NEXT_TAB,
        "Select Next Tab",
        ActionCategory::Navigation,
        "Focus the next open editor tab",
        WhenScope::Workspace,
    ),
    spec(
        ids::NAVIGATION_PREV_TAB,
        "Select Previous Tab",
        ActionCategory::Navigation,
        "Focus the previous open editor tab",
        WhenScope::Workspace,
    ),
    // Editor & Search
    spec(
        ids::EDITOR_SAVE,
        "Save All",
        ActionCategory::Editor,
        "Flush active buffer and modified files to disk",
        WhenScope::Workspace,
    ),
    spec(
        ids::EDITOR_UNDO,
        "Undo",
        ActionCategory::Editor,
        "Undo the previous edit",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_REDO,
        "Redo",
        ActionCategory::Editor,
        "Redo the previously undone edit",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_CUT,
        "Cut",
        ActionCategory::Editor,
        "Cut selected text or current line to clipboard",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_COPY,
        "Copy",
        ActionCategory::Editor,
        "Copy selected text or current line to clipboard",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_PASTE,
        "Paste",
        ActionCategory::Editor,
        "Insert clipboard contents at active carets",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_SELECT_ALL,
        "Select All",
        ActionCategory::Editor,
        "Select the entire editor buffer",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_ESCAPE,
        "Escape",
        ActionCategory::Editor,
        "Close inline bars, clear multi-cursors, or cancel selection",
        WhenScope::Editor,
    ),
    spec(
        ids::EDITOR_DUPLICATE_LINE,
        "Duplicate Line or Selection",
        ActionCategory::Editor,
        "Duplicate the current line or selected text",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_DELETE_LINE,
        "Delete Line",
        ActionCategory::Editor,
        "Delete the line(s) at active carets",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_TOGGLE_LINE_COMMENT,
        "Comment with Line Comment",
        ActionCategory::Editor,
        "Toggle line comments on current line or selection",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_TOGGLE_BLOCK_COMMENT,
        "Comment with Block Comment",
        ActionCategory::Editor,
        "Toggle block comment around active selection",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_EXTEND_SELECTION,
        "Extend Selection",
        ActionCategory::Editor,
        "Expand selection outward to enclosing syntax node",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_SHRINK_SELECTION,
        "Shrink Selection",
        ActionCategory::Editor,
        "Shrink selection inward to previous syntax scope",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_MOVE_LINE_UP,
        "Move Line Up",
        ActionCategory::Editor,
        "Swap current line or selection with the line above",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_MOVE_LINE_DOWN,
        "Move Line Down",
        ActionCategory::Editor,
        "Swap current line or selection with the line below",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_MOVE_STATEMENT_UP,
        "Move Statement Up",
        ActionCategory::Editor,
        "Move the enclosing statement or declaration upward",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_MOVE_STATEMENT_DOWN,
        "Move Statement Down",
        ActionCategory::Editor,
        "Move the enclosing statement or declaration downward",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_REFORMAT_CODE,
        "Reformat Code",
        ActionCategory::Editor,
        "Format active file or selection using configured formatter",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_OPTIMIZE_IMPORTS,
        "Optimize Imports",
        ActionCategory::Editor,
        "Remove unused imports and sort import declarations",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_SHOW_INTENTIONS,
        "Show Context Actions",
        ActionCategory::Editor,
        "Show LSP quick fixes and intention actions at caret",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_COMPLETE_BASIC,
        "Basic Completion",
        ActionCategory::Editor,
        "Trigger basic code completion popup",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_COMPLETE_SMART,
        "Type-Matching Completion",
        ActionCategory::Editor,
        "Trigger smart type-filtered code completion",
        WhenScope::EditableText,
    ),
    spec(
        ids::EDITOR_PARAMETER_INFO,
        "Parameter Info",
        ActionCategory::Editor,
        "Display signature parameter hints at call site",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_QUICK_DOC,
        "Quick Documentation",
        ActionCategory::Editor,
        "Show Javadoc/KDoc hover documentation popup",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_ADD_NEXT_OCCURRENCE,
        "Add Selection for Next Occurrence",
        ActionCategory::Editor,
        "Add next match of current word or selection as a cursor",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_UNSELECT_OCCURRENCE,
        "Unselect Occurrence",
        ActionCategory::Editor,
        "Remove the most recently added occurrence cursor",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_SELECT_ALL_OCCURRENCES,
        "Select All Occurrences",
        ActionCategory::Editor,
        "Create cursors for all matches of current word or selection",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_ADD_CURSOR_ABOVE,
        "Add Caret Above",
        ActionCategory::Editor,
        "Clone active caret onto the line above",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_ADD_CURSOR_BELOW,
        "Add Caret Below",
        ActionCategory::Editor,
        "Clone active caret onto the line below",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_FIND,
        "Find...",
        ActionCategory::Search,
        "Open inline find bar in the active editor",
        WhenScope::Editor,
    ),
    spec(
        ids::EDITOR_REPLACE,
        "Replace...",
        ActionCategory::Search,
        "Open inline find and replace bar in the active editor",
        WhenScope::Editor,
    ),
    spec(
        ids::EDITOR_FIND_IN_FILES,
        "Find in Files...",
        ActionCategory::Search,
        "Search text or regex across all workspace files",
        WhenScope::Workspace,
    ),
    spec(
        ids::EDITOR_REPLACE_IN_FILES,
        "Replace in Files...",
        ActionCategory::Search,
        "Search and replace across workspace files",
        WhenScope::Workspace,
    ),
    spec(
        ids::EDITOR_FIND_NEXT,
        "Find Next / Move to Next Occurrence",
        ActionCategory::Search,
        "Navigate to the next search match in the active editor",
        WhenScope::Editor,
    ),
    spec(
        ids::EDITOR_FIND_PREV,
        "Find Previous / Move to Previous Occurrence",
        ActionCategory::Search,
        "Navigate to the previous search match in the active editor",
        WhenScope::Editor,
    ),
    spec(
        ids::EDITOR_FOLD_REGION,
        "Collapse Region",
        ActionCategory::Editor,
        "Collapse the code fold region containing the caret",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_UNFOLD_REGION,
        "Expand Region",
        ActionCategory::Editor,
        "Expand the code fold region containing the caret",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_FOLD_ALL,
        "Collapse All",
        ActionCategory::Editor,
        "Collapse all foldable code regions in the active file",
        WhenScope::EditorText,
    ),
    spec(
        ids::EDITOR_UNFOLD_ALL,
        "Expand All",
        ActionCategory::Editor,
        "Expand all foldable code regions in the active file",
        WhenScope::EditorText,
    ),
    // Refactoring
    spec(
        ids::REFACTOR_RENAME,
        "Rename...",
        ActionCategory::Refactor,
        "Rename symbol or file across the project",
        WhenScope::Workspace,
    ),
    spec(
        ids::REFACTOR_EXTRACT_VARIABLE,
        "Introduce Variable...",
        ActionCategory::Refactor,
        "Extract selected expression into a local variable",
        WhenScope::EditableText,
    ),
    spec(
        ids::REFACTOR_EXTRACT_METHOD,
        "Extract Method...",
        ActionCategory::Refactor,
        "Extract selected statements into a method",
        WhenScope::EditableText,
    ),
    spec(
        ids::REFACTOR_EXTRACT_CONSTANT,
        "Introduce Constant...",
        ActionCategory::Refactor,
        "Extract selected expression into a constant",
        WhenScope::EditableText,
    ),
    spec(
        ids::REFACTOR_EXTRACT_FIELD,
        "Introduce Field...",
        ActionCategory::Refactor,
        "Extract selected expression into a class field",
        WhenScope::EditableText,
    ),
    spec(
        ids::REFACTOR_INLINE,
        "Inline...",
        ActionCategory::Refactor,
        "Inline variable, method, or constant usage",
        WhenScope::EditableText,
    ),
    spec(
        ids::REFACTOR_CHANGE_SIGNATURE,
        "Change Signature...",
        ActionCategory::Refactor,
        "Modify method parameters, return type, or visibility",
        WhenScope::EditableText,
    ),
    // Run & Debug
    spec(
        ids::RUN_RUN_CONTEXT,
        "Run Context Configuration",
        ActionCategory::RunDebug,
        "Run main entry point or test at caret context",
        WhenScope::Workspace,
    ),
    spec(
        ids::RUN_RUN_LAST,
        "Run Selected Configuration",
        ActionCategory::RunDebug,
        "Run the active or most recent run configuration",
        WhenScope::Workspace,
    ),
    spec(
        ids::RUN_DEBUG_CONTEXT,
        "Debug Context Configuration",
        ActionCategory::RunDebug,
        "Debug main entry point or test at caret context",
        WhenScope::Workspace,
    ),
    spec(
        ids::RUN_DEBUG_LAST,
        "Debug Selected Configuration",
        ActionCategory::RunDebug,
        "Debug the active or most recent run configuration",
        WhenScope::Workspace,
    ),
    spec(
        ids::RUN_STOP,
        "Stop",
        ActionCategory::RunDebug,
        "Terminate the active run or debug process",
        WhenScope::Workspace,
    ),
    spec(
        ids::DEBUG_TOGGLE_LINE_BREAKPOINT,
        "Toggle Line Breakpoint",
        ActionCategory::RunDebug,
        "Add or remove a debugger breakpoint on the current line",
        WhenScope::Editor,
    ),
    spec(
        ids::DEBUG_STEP_OVER,
        "Step Over",
        ActionCategory::RunDebug,
        "Step over the current line in the debugger",
        WhenScope::Workspace,
    ),
    spec(
        ids::DEBUG_STEP_INTO,
        "Step Into",
        ActionCategory::RunDebug,
        "Step into the called function in the debugger",
        WhenScope::Workspace,
    ),
    spec(
        ids::DEBUG_STEP_OUT,
        "Step Out",
        ActionCategory::RunDebug,
        "Step out of the current stack frame in the debugger",
        WhenScope::Workspace,
    ),
    spec(
        ids::DEBUG_RESUME,
        "Resume Program",
        ActionCategory::RunDebug,
        "Continue execution in the active debug session",
        WhenScope::Workspace,
    ),
    spec(
        ids::DEBUG_EVALUATE_EXPRESSION,
        "Evaluate Expression...",
        ActionCategory::RunDebug,
        "Evaluate an expression in the suspended debug frame",
        WhenScope::Workspace,
    ),
    // Tool Windows, View, Git & Vim
    spec(
        ids::TOOL_WINDOW_PROJECT,
        "Project",
        ActionCategory::ToolWindow,
        "Toggle or focus the Project explorer tool window",
        WhenScope::Workspace,
    ),
    spec(
        ids::TOOL_WINDOW_STRUCTURE,
        "Structure",
        ActionCategory::ToolWindow,
        "Toggle or focus the File Structure tool window",
        WhenScope::Workspace,
    ),
    spec(
        ids::TOOL_WINDOW_RUN,
        "Run",
        ActionCategory::ToolWindow,
        "Toggle or focus the Run output tool window",
        WhenScope::Workspace,
    ),
    spec(
        ids::TOOL_WINDOW_DEBUG,
        "Debug",
        ActionCategory::ToolWindow,
        "Toggle or focus the Debug tool window",
        WhenScope::Workspace,
    ),
    spec(
        ids::TOOL_WINDOW_PROBLEMS,
        "Problems",
        ActionCategory::ToolWindow,
        "Toggle or focus the Problems diagnostics tool window",
        WhenScope::Workspace,
    ),
    spec(
        ids::TOOL_WINDOW_GIT,
        "Git",
        ActionCategory::ToolWindow,
        "Toggle or focus the Git tool window",
        WhenScope::Workspace,
    ),
    spec(
        ids::TOOL_WINDOW_TERMINAL,
        "Terminal",
        ActionCategory::ToolWindow,
        "Toggle or focus the integrated PTY Terminal tool window",
        WhenScope::Workspace,
    ),
    spec(
        ids::TOOL_WINDOW_HIDE_ALL,
        "Hide All Tool Windows",
        ActionCategory::ToolWindow,
        "Collapse all side and bottom tool window docks",
        WhenScope::Workspace,
    ),
    spec(
        ids::VIEW_TOGGLE_FOCUS_MODE,
        "Toggle Focus Mode",
        ActionCategory::View,
        "Toggle Omarchy distraction-free focus/tiling mode",
        WhenScope::Workspace,
    ),
    spec(
        ids::VIEW_OPEN_SETTINGS,
        "Settings...",
        ActionCategory::View,
        "Open IDE Settings synchronized with settings.toml",
        WhenScope::Global,
    ),
    spec(
        ids::VIEW_CLOSE_TAB,
        "Close Tab",
        ActionCategory::View,
        "Close the active editor tab",
        WhenScope::Workspace,
    ),
    spec(
        ids::GIT_COMMIT,
        "Commit...",
        ActionCategory::Git,
        "Open Git commit view",
        WhenScope::Workspace,
    ),
    spec(
        ids::GIT_PUSH,
        "Push...",
        ActionCategory::Git,
        "Push local commits to configured Git remote",
        WhenScope::Workspace,
    ),
    spec(
        ids::GIT_UPDATE_PROJECT,
        "Update Project",
        ActionCategory::Git,
        "Fetch and update project from configured Git remote",
        WhenScope::Workspace,
    ),
    spec(
        ids::GIT_ROLLBACK,
        "Rollback",
        ActionCategory::Git,
        "Revert uncommitted changes in the current file or selection",
        WhenScope::Workspace,
    ),
    spec(
        ids::VIM_TOGGLE,
        "Toggle Vim Mode",
        ActionCategory::Vim,
        "Enable or disable modal Vim editing",
        WhenScope::Workspace,
    ),
    spec(
        ids::VIEW_TOGGLE_TILING_MODE,
        "Toggle Tiling Mode",
        ActionCategory::View,
        "Switch between Islands and dense Omarchy tiling layout",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_FOCUS_LEFT,
        "Focus Left Island",
        ActionCategory::View,
        "Move keyboard focus to the island on the left",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_FOCUS_RIGHT,
        "Focus Right Island",
        ActionCategory::View,
        "Move keyboard focus to the island on the right",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_FOCUS_UP,
        "Focus Island Above",
        ActionCategory::View,
        "Move keyboard focus to the island above",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_FOCUS_DOWN,
        "Focus Island Below",
        ActionCategory::View,
        "Move keyboard focus to the island below",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_MOVE_TAB_LEFT,
        "Move Tab to Left Split",
        ActionCategory::View,
        "Move the active tab to the split on the left",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_MOVE_TAB_RIGHT,
        "Move Tab to Right Split",
        ActionCategory::View,
        "Move the active tab to the split on the right",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_MOVE_TAB_UP,
        "Move Tab to Split Above",
        ActionCategory::View,
        "Move the active tab to the split above",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_MOVE_TAB_DOWN,
        "Move Tab to Split Below",
        ActionCategory::View,
        "Move the active tab to the split below",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_SPLIT_RIGHT,
        "Split Right",
        ActionCategory::View,
        "Split the active editor pane side by side",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_SPLIT_DOWN,
        "Split Down",
        ActionCategory::View,
        "Split the active editor pane top and bottom",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_RESIZE_LEFT,
        "Resize Split Left",
        ActionCategory::View,
        "Move the active split divider to the left",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_RESIZE_RIGHT,
        "Resize Split Right",
        ActionCategory::View,
        "Move the active split divider to the right",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_RESIZE_UP,
        "Resize Split Up",
        ActionCategory::View,
        "Move the active split divider up",
        WhenScope::Workspace,
    ),
    spec(
        ids::WINDOW_RESIZE_DOWN,
        "Resize Split Down",
        ActionCategory::View,
        "Move the active split divider down",
        WhenScope::Workspace,
    ),
];

/// Registers all built-in IDE actions into `registry`.
///
/// # Errors
///
/// Returns [`ActionError`] if any action ID is already present in `registry`
/// or fails validation.
pub fn register_builtin_actions(registry: &mut ActionRegistry) -> Result<(), ActionError> {
    for entry in BUILTIN_SPECS {
        let mut descriptor =
            ActionDescriptor::builtin(ActionId::from_static(entry.id), entry.title, entry.category)
                .with_description(entry.description);
        if let Some(when_expr) = entry.scope.into_expr() {
            descriptor = descriptor.with_when(when_expr);
        }
        registry.register(descriptor)?;
    }
    Ok(())
}

/// Returns a `&'static` reference to the cached built-in [`ActionRegistry`].
///
/// # Panics
///
/// Panics only if a compiled-in action specification is duplicated or invalid
/// (guarded by unit and integration tests).
#[must_use]
pub fn builtin_action_registry_ref() -> &'static ActionRegistry {
    BUILTIN_REGISTRY.get_or_init(|| {
        let mut registry = ActionRegistry::new();
        #[allow(
            clippy::expect_used,
            reason = "compiled-in built-in action catalog is validated by unit and audit tests"
        )]
        register_builtin_actions(&mut registry)
            .expect("compiled-in built-in action specs must be unique and valid");
        registry
    })
}

/// Returns a cloned copy of the built-in [`ActionRegistry`].
#[must_use]
pub fn builtin_action_registry() -> ActionRegistry {
    builtin_action_registry_ref().clone()
}

/// Returns all canonical built-in action identifier strings in registration order.
#[must_use]
pub fn builtin_action_ids() -> Vec<&'static str> {
    BUILTIN_SPECS.iter().map(|entry| entry.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextState;
    use crate::context::FocusStack;

    #[test]
    fn builtin_registry_contains_all_specs_without_duplicates() {
        let registry = builtin_action_registry();
        assert_eq!(registry.len(), BUILTIN_SPECS.len());
        assert!(registry.len() >= 85);
        for spec in BUILTIN_SPECS {
            let desc = registry.get_by_str(spec.id).expect("action registered");
            assert!(!desc.title.is_empty());
            assert!(desc.description.is_some());
        }
    }

    #[test]
    fn duplicate_registration_returns_error() {
        let mut registry = builtin_action_registry();
        let err = register_builtin_actions(&mut registry).expect_err("duplicate must fail");
        assert!(matches!(err, ActionError::DuplicateAction(_)));
    }

    #[test]
    fn editable_actions_respect_read_only_flag() {
        let registry = builtin_action_registry_ref();
        let stack = FocusStack::from_leaf(FocusContext::EditorText);

        let writable = ContextState::new();
        let writable_ids: Vec<&str> = registry
            .available_in(&stack, &writable)
            .into_iter()
            .map(|d| d.id.as_str())
            .collect();
        assert!(writable_ids.contains(&ids::EDITOR_DUPLICATE_LINE));
        assert!(writable_ids.contains(&ids::EDITOR_COPY));

        let read_only = ContextState::new().with_flag("editor_read_only", true);
        let ro_ids: Vec<&str> = registry
            .available_in(&stack, &read_only)
            .into_iter()
            .map(|d| d.id.as_str())
            .collect();
        assert!(!ro_ids.contains(&ids::EDITOR_DUPLICATE_LINE));
        assert!(ro_ids.contains(&ids::EDITOR_COPY));
        assert!(ro_ids.contains(&ids::NAVIGATION_SEARCH_EVERYWHERE));
    }
}
