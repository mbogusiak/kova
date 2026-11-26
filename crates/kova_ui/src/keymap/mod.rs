//! `JetBrains` keymap bridge (`KeyDispatchLayer`): GPUI keystrokes → `kova_core` chords → shell actions.
//!
//! The binding tables and chord rules live in `kova_core` ([`KeymapResolver`] built from the
//! `JetBrains` preset plus an optional user `keymap.toml`); this module only
//!
//! 1. converts a [`gpui::Keystroke`] into a headless [`KeyStroke`] without allocating,
//! 2. keeps the IME path intact ([`is_text_composition`]): `Option`/`AltGr` letters that type
//!    national characters (`ąęćłńóśźż`) are never treated as shortcuts while text is edited,
//! 3. runs the multi-key chord session with a `1000 ms` timeout guarded by an epoch counter,
//! 4. maps the resolved [`ActionId`] onto a typed [`ShellAction`], and
//! 5. decides the order in which the Ex command line, the chord session, the Vim engine, the
//!    keymap and the editor see a key ([`key_route`]).
//!
//! Every function here is pure or owns plain data, so the whole dispatch is unit-tested
//! without a window.

pub mod ex;

use std::time::Instant;

use kova_core::builtin_actions::{builtin_action_registry_ref, ids};
use kova_core::keymap::chord::DEFAULT_CHORD_TIMEOUT;
use kova_core::{
    ActionId, ContextState, FocusContext, FocusStack, KeyCode, KeyDispatch, KeyStep, KeyStroke,
    KeymapResolver, KeymapSession, ModifierTap, Modifiers, Platform, build_effective_keymap,
    jetbrains_keymap_layer,
};

pub use ex::{ExCommand, ExCommandLine, ExLineEvent, ExParseError, parse_ex_command};

use crate::dock::{Direction, DockToolId, FocusModeVariant, SplitAxis};
use crate::search::SearchScope;

/// Chord prefix timeout (`1000 ms`, `KOVA-026` FR-3).
pub const CHORD_TIMEOUT: std::time::Duration = DEFAULT_CHORD_TIMEOUT;

/// Context flag set while modal Vim editing is enabled (used by `when = "vim_enabled"` rules).
pub const VIM_ENABLED_FLAG: &str = "vim_enabled";

/// Maps a GPUI key name onto a headless [`KeyCode`].
fn key_code(key: &str) -> Option<KeyCode> {
    Some(match key {
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "delete" => KeyCode::Delete,
        "escape" => KeyCode::Escape,
        "space" => KeyCode::Space,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        _ => {
            if let Some(num) = key.strip_prefix('f')
                && let Ok(n) = num.parse::<u8>()
                && (1..=24).contains(&n)
            {
                return Some(KeyCode::F(n));
            }
            let mut chars = key.chars();
            let ch = chars.next()?;
            if chars.next().is_some() || ch.is_control() || ch.is_whitespace() {
                return None;
            }
            KeyCode::normalized_char(ch)
        }
    })
}

/// Maps a shifted US-layout symbol back to its base key (`{` → `[`), as macOS reports
/// `cmd-{` for `Cmd+Shift+[` while keymaps are written as `cmd+shift+[`.
const fn unshift_us_symbol(ch: char) -> Option<char> {
    Some(match ch {
        '{' => '[',
        '}' => ']',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        '|' => '\\',
        '_' => '-',
        '+' => '=',
        '~' => '`',
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        _ => return None,
    })
}

const fn core_modifiers(m: gpui::Modifiers) -> Modifiers {
    Modifiers {
        ctrl: m.control,
        alt: m.alt,
        shift: m.shift,
        meta: m.platform,
    }
}

/// Converts a GPUI keystroke into the [`KeyStroke`] used for keymap resolution.
///
/// Shifted symbols reported without the shift flag (`cmd-{`) are normalised to
/// `cmd+shift+[` so they match the canonical preset notation. Returns `None` for keys
/// without a keymap representation (bare modifiers, unknown named keys).
#[must_use]
pub fn shortcut_stroke(keystroke: &gpui::Keystroke) -> Option<KeyStroke> {
    let mut modifiers = core_modifiers(keystroke.modifiers);
    let mut code = key_code(&keystroke.key)?;
    if let KeyCode::Char(ch) = code
        && let Some(base) = unshift_us_symbol(ch)
        && (modifiers.ctrl || modifiers.meta || modifiers.alt)
    {
        code = KeyCode::Char(base);
        modifiers.shift = true;
    }
    Some(KeyStroke::new(modifiers, code))
}

/// Converts a GPUI keystroke into the [`KeyStroke`] fed to the Vim engine.
///
/// Printable keys typed without `Ctrl`/`Alt`/`Cmd` use the typed character (`key_char`), so
/// `Shift+;` reaches Vim as `:` and `Shift+4` as `$` on every layout; uppercase letters carry
/// the shift flag that [`kova_core::VimKey::from_stroke`] expects.
#[must_use]
pub fn vim_stroke(keystroke: &gpui::Keystroke) -> Option<KeyStroke> {
    let m = &keystroke.modifiers;
    if !m.control
        && !m.alt
        && !m.platform
        && let Some(text) = keystroke.key_char.as_deref()
    {
        let mut chars = text.chars();
        if let (Some(ch), None) = (chars.next(), chars.next())
            && !ch.is_control()
            && ch != ' '
        {
            let modifiers = if ch.is_ascii_uppercase() {
                Modifiers::SHIFT
            } else {
                Modifiers::NONE
            };
            return Some(KeyStroke::new(modifiers, KeyCode::Char(ch)));
        }
    }
    let modifiers = core_modifiers(*m);
    key_code(&keystroke.key).map(|code| KeyStroke::new(modifiers, code))
}

/// Returns `true` when `keystroke` types text through the IME / keyboard layout and must not
/// be resolved as a shortcut while text is being edited (Vim off or Vim `INSERT`).
///
/// - **macOS:** `Option` (+`Shift`) with a printable character key and without `Cmd`/`Ctrl` is
///   a character modifier (`Option+a` → `ą` on Polish Pro, `ß`, `¬`, dead keys).
/// - **Linux:** a printable non-ASCII `key_char` without `Super` comes from `AltGr` (or a
///   national layout) and is text.
///
/// Named keys (`Alt+Up`, `Alt+F12`, `Alt+Enter`) are never text.
#[must_use]
pub fn is_text_composition(keystroke: &gpui::Keystroke, platform: Platform) -> bool {
    let m = &keystroke.modifiers;
    if m.platform || keystroke.key.chars().nth(1).is_some() {
        return false;
    }
    let Some(text) = keystroke.key_char.as_deref() else {
        return false;
    };
    let printable = !text.is_empty() && text.chars().all(|c| !c.is_control());
    if !printable {
        return false;
    }
    match platform {
        Platform::MacOs => m.alt && !m.control,
        Platform::Linux => !text.is_ascii(),
    }
}

/// Typed shell command resolved from a keymap [`ActionId`] (or an Ex command).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellAction {
    /// Toggle (open + focus, or close) a tool window.
    ToggleTool(DockToolId),
    /// Toggle Focus Mode (`tool_window.hide_all` or `view.toggle_focus_mode`).
    ToggleFocusMode(FocusModeVariant),
    /// Switch between Islands and Tiling density.
    ToggleTilingMode,
    /// Enable or disable modal Vim editing.
    ToggleVimMode,
    /// Move keyboard focus to the neighbouring island.
    FocusDirection(Direction),
    /// Move the active tab to the neighbouring split (or a new one).
    MoveTabToSplit(Direction),
    /// Move the active pane's split divider.
    ResizeSplit(Direction),
    /// Split the active pane.
    Split(SplitAxis),
    /// Close the active tab.
    CloseActiveTab,
    /// Select the next (`true`) or previous tab.
    CycleTab(bool),
    /// Save the active document.
    Save,
    /// Duplicate the current line(s) or selection.
    DuplicateLine,
    /// Toggle a line comment on every selected line.
    ToggleLineComment,
    /// Add a selection for the next occurrence of the word under the caret.
    AddNextOccurrence,
    /// Open *Search Everywhere* (`Shift Shift`).
    OpenSearchEverywhere,
    /// Open *Search Everywhere* pre-focused on a specific scope tab (`Cmd+O`, `Cmd+Shift+O`, `Cmd+Alt+O`, `Cmd+Shift+A`).
    OpenSearchScope(SearchScope),
    /// Open *Recent Files* popup (`Cmd+E` / `Ctrl+E`).
    OpenRecentFiles,
    /// Open inline *Go to Line/Column* prompt (`Cmd+L` / `Ctrl+G`).
    GoToLine,
    /// Jump to the next problem (`F2`).
    NextProblem,
    /// Jump to the previous problem (`Shift+F2`).
    PrevProblem,
    /// Open *Settings* (`Cmd+,`).
    OpenSettings,
    /// Open or close the Git *Commit* panel (`Cmd+K` / `Ctrl+K`).
    ToggleCommitPanel,
    /// Jump to symbol declaration at the active caret (`Cmd+B` / `Ctrl+B`).
    GoToDefinition,
    /// Step backward in navigation history (`Cmd+[` / `Ctrl+Alt+Left`).
    NavigateBack,
    /// Step forward in navigation history (`Cmd+]` / `Ctrl+Alt+Right`).
    NavigateForward,
    /// Delete the line(s) at active carets (`Cmd+Backspace` / `Ctrl+Y`).
    DeleteLine,
    /// Toggle block comment around selection or caret (`Cmd+Alt+/` / `Ctrl+Shift+/`).
    ToggleBlockComment,
    /// Move current line or selection up (`true`) or down (`false`) (`Alt+Shift+Up/Down`).
    MoveLine(bool),
    /// Extend selection outward (`Alt+Up` / `Ctrl+W`).
    ExtendSelection,
    /// Shrink selection inward (`Alt+Down` / `Ctrl+Shift+W`).
    ShrinkSelection,
    /// Reformat active file (`Cmd+Alt+L` / `Ctrl+Alt+L`).
    ReformatCode,
    /// Unselect the last added multi-cursor occurrence (`Ctrl+Shift+G` / `Alt+Shift+J`).
    UnselectOccurrence,
    /// Select all occurrences of word or selection (`Ctrl+Cmd+G` / `Ctrl+Alt+Shift+J`).
    SelectAllOccurrences,
    /// Editor escape: close inline find bar or clear multi-cursors/selection (`Esc` when Vim is off).
    EditorEscape,
    /// Trigger basic code completion (`Ctrl+Space`).
    CompleteBasic,
    /// Collapse foldable region at caret (`Cmd+-` / `Ctrl+-`).
    FoldRegion,
    /// Expand foldable region at caret (`Cmd+=` / `Ctrl+=`).
    UnfoldRegion,
    /// Collapse all foldable regions (`Cmd+Shift+-` / `Ctrl+Shift+-`).
    FoldAll,
    /// Expand all foldable regions (`Cmd+Shift+=` / `Ctrl+Shift+=`).
    UnfoldAll,
    /// Open inline Find bar (`Cmd+F` / `Ctrl+F`).
    Find,
    /// Open inline Replace bar (`Cmd+R` / `Ctrl+R`).
    Replace,
    /// Jump to next find match (`Cmd+G` / `F3`).
    FindNext,
    /// Jump to previous find match (`Cmd+Shift+G` / `Shift+F3`).
    FindPrev,
    /// Open workspace-wide *Find in Files* modal (`Cmd+Shift+F` / `Ctrl+Shift+F`).
    FindInFiles,
    /// Run entry point at caret context (`Ctrl+Shift+R` / `Ctrl+Shift+F10`).
    RunContext,
    /// Re-run last executed configuration or entry point (`Ctrl+R` / `Shift+F10`).
    RunLast,
    /// JVM debugger command (`KOVA-041`).
    Debug(DebugShellAction),
}

/// Debugger commands bound by the `JetBrains` keymap (`KOVA-041`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DebugShellAction {
    /// *Toggle Line Breakpoint* (`Ctrl+F8` / `Cmd+F8`).
    ToggleBreakpoint,
    /// *Step Over* (`F8`).
    StepOver,
    /// *Step Into* (`F7`).
    StepInto,
    /// *Step Out* (`Shift+F8`).
    StepOut,
    /// *Resume Program* (`F9` / `Cmd+Alt+R`).
    Resume,
    /// *Evaluate Expression* (`Alt+F8`).
    EvaluateExpression,
    /// *Debug* the entry point at the caret (`Ctrl+Shift+F9` / `Ctrl+Shift+D`).
    DebugContext,
    /// *Stop* (`Ctrl+F2` / `Cmd+F2`).
    Stop,
}

impl ShellAction {
    /// Maps a keymap action onto a shell command; `None` for actions handled elsewhere
    /// (clipboard, undo, caret motions bound as GPUI actions by `KOVA-015`).
    #[must_use]
    pub fn from_action_id(action: &ActionId) -> Option<Self> {
        Some(match action.as_str() {
            ids::TOOL_WINDOW_PROJECT => Self::ToggleTool(DockToolId::Project),
            ids::TOOL_WINDOW_STRUCTURE | ids::NAVIGATION_FILE_STRUCTURE => {
                Self::ToggleTool(DockToolId::Structure)
            }
            ids::TOOL_WINDOW_RUN => Self::ToggleTool(DockToolId::Run),
            ids::TOOL_WINDOW_PROBLEMS => Self::ToggleTool(DockToolId::Problems),
            ids::TOOL_WINDOW_GIT => Self::ToggleTool(DockToolId::Commit),
            ids::TOOL_WINDOW_TERMINAL => Self::ToggleTool(DockToolId::Terminal),
            ids::TOOL_WINDOW_DEBUG => Self::ToggleTool(DockToolId::Debug),
            ids::TOOL_WINDOW_HIDE_ALL => Self::ToggleFocusMode(FocusModeVariant::HideDocks),
            ids::VIEW_TOGGLE_FOCUS_MODE => Self::ToggleFocusMode(FocusModeVariant::Zen),
            ids::VIEW_TOGGLE_TILING_MODE => Self::ToggleTilingMode,
            ids::VIM_TOGGLE => Self::ToggleVimMode,
            ids::WINDOW_FOCUS_LEFT => Self::FocusDirection(Direction::Left),
            ids::WINDOW_FOCUS_RIGHT => Self::FocusDirection(Direction::Right),
            ids::WINDOW_FOCUS_UP => Self::FocusDirection(Direction::Up),
            ids::WINDOW_FOCUS_DOWN => Self::FocusDirection(Direction::Down),
            ids::WINDOW_MOVE_TAB_LEFT => Self::MoveTabToSplit(Direction::Left),
            ids::WINDOW_MOVE_TAB_RIGHT => Self::MoveTabToSplit(Direction::Right),
            ids::WINDOW_MOVE_TAB_UP => Self::MoveTabToSplit(Direction::Up),
            ids::WINDOW_MOVE_TAB_DOWN => Self::MoveTabToSplit(Direction::Down),
            ids::WINDOW_RESIZE_LEFT => Self::ResizeSplit(Direction::Left),
            ids::WINDOW_RESIZE_RIGHT => Self::ResizeSplit(Direction::Right),
            ids::WINDOW_RESIZE_UP => Self::ResizeSplit(Direction::Up),
            ids::WINDOW_RESIZE_DOWN => Self::ResizeSplit(Direction::Down),
            ids::WINDOW_SPLIT_RIGHT => Self::Split(SplitAxis::Horizontal),
            ids::WINDOW_SPLIT_DOWN => Self::Split(SplitAxis::Vertical),
            ids::VIEW_CLOSE_TAB => Self::CloseActiveTab,
            ids::NAVIGATION_NEXT_TAB => Self::CycleTab(true),
            ids::NAVIGATION_PREV_TAB => Self::CycleTab(false),
            ids::NAVIGATION_GO_TO_CLASS => Self::OpenSearchScope(SearchScope::Classes),
            ids::NAVIGATION_GO_TO_FILE => Self::OpenSearchScope(SearchScope::Files),
            ids::NAVIGATION_GO_TO_SYMBOL => Self::OpenSearchScope(SearchScope::Symbols),
            ids::NAVIGATION_FIND_ACTION => Self::OpenSearchScope(SearchScope::Actions),
            ids::NAVIGATION_RECENT_FILES => Self::OpenRecentFiles,
            ids::NAVIGATION_GO_TO_LINE => Self::GoToLine,
            ids::NAVIGATION_NEXT_ERROR => Self::NextProblem,
            ids::NAVIGATION_PREV_ERROR => Self::PrevProblem,
            ids::NAVIGATION_GO_TO_DECLARATION => Self::GoToDefinition,
            ids::NAVIGATION_BACK => Self::NavigateBack,
            ids::NAVIGATION_FORWARD => Self::NavigateForward,
            ids::EDITOR_SAVE => Self::Save,
            ids::EDITOR_ESCAPE => Self::EditorEscape,
            ids::EDITOR_COMPLETE_BASIC => Self::CompleteBasic,
            ids::EDITOR_DUPLICATE_LINE => Self::DuplicateLine,
            ids::EDITOR_DELETE_LINE => Self::DeleteLine,
            ids::EDITOR_TOGGLE_LINE_COMMENT => Self::ToggleLineComment,
            ids::EDITOR_TOGGLE_BLOCK_COMMENT => Self::ToggleBlockComment,
            ids::EDITOR_MOVE_LINE_UP => Self::MoveLine(true),
            ids::EDITOR_MOVE_LINE_DOWN => Self::MoveLine(false),
            ids::EDITOR_EXTEND_SELECTION => Self::ExtendSelection,
            ids::EDITOR_SHRINK_SELECTION => Self::ShrinkSelection,
            ids::EDITOR_REFORMAT_CODE => Self::ReformatCode,
            ids::EDITOR_ADD_NEXT_OCCURRENCE => Self::AddNextOccurrence,
            ids::EDITOR_UNSELECT_OCCURRENCE => Self::UnselectOccurrence,
            ids::EDITOR_SELECT_ALL_OCCURRENCES => Self::SelectAllOccurrences,
            ids::EDITOR_FOLD_REGION => Self::FoldRegion,
            ids::EDITOR_UNFOLD_REGION => Self::UnfoldRegion,
            ids::EDITOR_FOLD_ALL => Self::FoldAll,
            ids::EDITOR_UNFOLD_ALL => Self::UnfoldAll,
            ids::EDITOR_FIND => Self::Find,
            ids::EDITOR_REPLACE => Self::Replace,
            ids::EDITOR_FIND_NEXT => Self::FindNext,
            ids::EDITOR_FIND_PREV => Self::FindPrev,
            ids::EDITOR_FIND_IN_FILES => Self::FindInFiles,
            ids::NAVIGATION_SEARCH_EVERYWHERE => Self::OpenSearchEverywhere,
            ids::VIEW_OPEN_SETTINGS => Self::OpenSettings,
            ids::GIT_COMMIT => Self::ToggleCommitPanel,
            ids::RUN_RUN_CONTEXT => Self::RunContext,
            ids::RUN_RUN_LAST => Self::RunLast,
            ids::DEBUG_TOGGLE_LINE_BREAKPOINT => Self::Debug(DebugShellAction::ToggleBreakpoint),
            ids::DEBUG_STEP_OVER => Self::Debug(DebugShellAction::StepOver),
            ids::DEBUG_STEP_INTO => Self::Debug(DebugShellAction::StepInto),
            ids::DEBUG_STEP_OUT => Self::Debug(DebugShellAction::StepOut),
            ids::DEBUG_RESUME => Self::Debug(DebugShellAction::Resume),
            ids::DEBUG_EVALUATE_EXPRESSION => Self::Debug(DebugShellAction::EvaluateExpression),
            ids::RUN_DEBUG_CONTEXT | ids::RUN_DEBUG_LAST => {
                Self::Debug(DebugShellAction::DebugContext)
            }
            ids::RUN_STOP => Self::Debug(DebugShellAction::Stop),
            _ => return None,
        })
    }
}

/// Focus scope and flags of the key being dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchContext {
    /// Innermost focus context (e.g. [`FocusContext::EditorVimNormal`]).
    pub leaf: FocusContext,
    /// Whether modal Vim editing is enabled.
    pub vim_enabled: bool,
    /// Whether the focused editor buffer is read-only (e.g. decompiled class).
    pub editor_read_only: bool,
}

/// Outcome of feeding one key to the [`KeyDispatchLayer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    /// A binding resolved to a shell command; the key is consumed.
    Action(ShellAction),
    /// A binding resolved to an action owned by another layer (e.g. `editor.copy` handled by
    /// GPUI actions); the key must propagate untouched.
    Delegated(ActionId),
    /// The key extended a chord prefix; the key is consumed and `epoch` identifies the
    /// timeout that may clear it.
    Pending {
        /// Status bar hint, e.g. `"ctrl+w …"`.
        hint: String,
        /// Epoch to pass to [`KeyDispatchLayer::expire`] when the `1000 ms` timer fires.
        epoch: u64,
    },
    /// `Escape` cancelled a pending chord; the key is consumed.
    Cancelled,
    /// No binding matched; the key must propagate (editor, IME).
    Unhandled,
}

/// Stateful keymap dispatcher: [`KeymapResolver`] + [`KeymapSession`] + chord timeout epoch.
#[derive(Debug)]
pub struct KeyDispatchLayer {
    resolver: KeymapResolver,
    session: KeymapSession,
    epoch: u64,
    cached_leaf: FocusContext,
    cached_stack: FocusStack,
    state_vim_on: ContextState,
    state_vim_off: ContextState,
    state_vim_on_ro: ContextState,
    state_vim_off_ro: ContextState,
    prev_shift: bool,
    prev_capslock: bool,
}

impl KeyDispatchLayer {
    /// Creates a dispatcher over the built-in `JetBrains` preset for `platform`.
    #[must_use]
    pub fn new(platform: Platform) -> Self {
        let resolver = build_effective_keymap(platform, None).unwrap_or_else(|_| {
            KeymapResolver::from_layers(platform, [jetbrains_keymap_layer(platform)])
        });
        Self::with_resolver(resolver)
    }

    /// Creates a dispatcher over an already composed resolver (preset + user `keymap.toml`).
    #[must_use]
    pub fn with_resolver(resolver: KeymapResolver) -> Self {
        Self {
            resolver,
            session: KeymapSession::new().with_chord_timeout(CHORD_TIMEOUT),
            epoch: 0,
            cached_leaf: FocusContext::Workspace,
            cached_stack: FocusStack::from_leaf(FocusContext::Workspace),
            state_vim_on: ContextState::new().with_flag(VIM_ENABLED_FLAG, true),
            state_vim_off: ContextState::new(),
            state_vim_on_ro: ContextState::new()
                .with_flag(VIM_ENABLED_FLAG, true)
                .with_flag("editor_read_only", true),
            state_vim_off_ro: ContextState::new().with_flag("editor_read_only", true),
            prev_shift: false,
            prev_capslock: false,
        }
    }

    /// Target platform of the underlying resolver.
    #[must_use]
    pub const fn platform(&self) -> Platform {
        self.resolver.platform()
    }

    /// Whether a chord prefix is waiting for its next key.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.session.pending_steps().is_empty()
    }

    /// Status bar hint for the pending chord prefix (`"ctrl+w …"`), if any.
    #[must_use]
    pub fn pending_hint(&self) -> Option<String> {
        let steps = self.session.pending_steps();
        (!steps.is_empty()).then(|| format_hint(steps))
    }

    /// Clears any pending chord prefix (focus change, modal open).
    pub fn reset(&mut self) {
        self.session.reset();
        self.epoch = self.epoch.wrapping_add(1);
    }

    /// Cancels any in-progress modifier double-tap sequence (`Shift+Shift`), e.g. when a
    /// normal key press is handled outside [`Self::dispatch`] (Vim, IME, terminal, modals).
    pub const fn cancel_modifier_tap(&mut self) {
        self.session.cancel_modifier_tap();
    }

    /// Updates tracked modifier flags while cancelling any double-tap candidate (for example
    /// when a blocking modal ignores modifier-only shortcuts).
    pub const fn sync_modifiers_and_cancel(
        &mut self,
        modifiers: gpui::Modifiers,
        capslock_on: bool,
    ) {
        self.session.cancel_modifier_tap();
        self.prev_shift = modifiers.shift;
        self.prev_capslock = capslock_on;
    }

    /// Clears the pending prefix when the timer for `epoch` fires and no newer key arrived.
    /// Returns `true` when a prefix was cleared.
    pub fn expire(&mut self, epoch: u64) -> bool {
        if epoch != self.epoch || !self.has_pending() {
            return false;
        }
        self.session.reset();
        true
    }

    /// Feeds one key-down stroke and resolves it against the keymap.
    pub fn dispatch(
        &mut self,
        stroke: KeyStroke,
        ctx: DispatchContext,
        now: Instant,
    ) -> DispatchOutcome {
        self.update_cached_context(ctx.leaf);
        let had_pending = self.has_pending();
        let state = match (ctx.vim_enabled, ctx.editor_read_only) {
            (true, false) => &self.state_vim_on,
            (false, false) => &self.state_vim_off,
            (true, true) => &self.state_vim_on_ro,
            (false, true) => &self.state_vim_off_ro,
        };
        let result =
            self.session
                .on_key_down(&self.resolver, stroke, &self.cached_stack, state, now);
        self.epoch = self.epoch.wrapping_add(1);
        let cancelled_escape = had_pending && stroke == KeyStroke::unmodified(KeyCode::Escape);
        self.map_dispatch_result(result, cancelled_escape, state)
    }

    /// Feeds a [`gpui::ModifiersChangedEvent`] (macOS `NSFlagsChanged` / X11 modifier edge)
    /// and resolves modifier double-taps such as `Shift+Shift` → [`ShellAction::OpenSearchEverywhere`].
    ///
    /// Any concurrent non-Shift modifier (`control`, `alt`, `platform`, `function`) or a
    /// `CapsLock` toggle cancels the tap detector and keeps `prev_shift` in sync so releasing
    /// `Cmd+Shift` in either order never triggers a false double-tap.
    pub fn dispatch_modifiers_changed(
        &mut self,
        modifiers: gpui::Modifiers,
        capslock_on: bool,
        ctx: DispatchContext,
        now: Instant,
    ) -> DispatchOutcome {
        let capslock_toggled = capslock_on != self.prev_capslock;
        self.prev_capslock = capslock_on;

        if modifiers.control
            || modifiers.alt
            || modifiers.platform
            || modifiers.function
            || capslock_toggled
        {
            self.session.cancel_modifier_tap();
            self.prev_shift = modifiers.shift;
            return DispatchOutcome::Unhandled;
        }

        if modifiers.shift == self.prev_shift {
            return DispatchOutcome::Unhandled;
        }
        self.prev_shift = modifiers.shift;

        self.update_cached_context(ctx.leaf);
        let state = match (ctx.vim_enabled, ctx.editor_read_only) {
            (true, false) => &self.state_vim_on,
            (false, false) => &self.state_vim_off,
            (true, true) => &self.state_vim_on_ro,
            (false, true) => &self.state_vim_off_ro,
        };
        let result = self.session.on_modifier_edge(
            &self.resolver,
            ModifierTap::Shift,
            modifiers.shift,
            &self.cached_stack,
            state,
            now,
        );
        if !matches!(result, KeyDispatch::NotMatched) {
            self.epoch = self.epoch.wrapping_add(1);
        }
        self.map_dispatch_result(result, false, state)
    }

    fn update_cached_context(&mut self, leaf: FocusContext) {
        if self.cached_leaf != leaf {
            self.cached_leaf = leaf;
            self.cached_stack = FocusStack::from_leaf(leaf);
        }
    }

    fn map_dispatch_result(
        &self,
        result: KeyDispatch,
        cancelled_escape: bool,
        state: &ContextState,
    ) -> DispatchOutcome {
        match result {
            KeyDispatch::Matched { action, .. } => {
                if state.editor_read_only
                    && builtin_action_registry_ref()
                        .get(&action)
                        .and_then(|desc| desc.default_when.as_ref())
                        .is_some_and(|expr| !expr.matches(&self.cached_stack, state))
                {
                    return DispatchOutcome::Unhandled;
                }
                ShellAction::from_action_id(&action)
                    .map_or(DispatchOutcome::Delegated(action), DispatchOutcome::Action)
            }
            KeyDispatch::Pending { prefix, .. } => DispatchOutcome::Pending {
                hint: format_hint(&prefix),
                epoch: self.epoch,
            },
            KeyDispatch::NotMatched if cancelled_escape => DispatchOutcome::Cancelled,
            KeyDispatch::NotMatched => DispatchOutcome::Unhandled,
        }
    }
}

fn format_hint(steps: &[KeyStep]) -> String {
    let mut hint = String::with_capacity(16);
    for step in steps {
        if !hint.is_empty() {
            hint.push(' ');
        }
        hint.push_str(&step.to_string());
    }
    hint.push_str(" …");
    hint
}

/// First handler that sees a key-down event in the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRoute {
    /// The Vim Ex command line (`:` / `/` / `?`) owns every key.
    CommandLine,
    /// A chord prefix is pending: the keymap must see the key first (`Ctrl+W` → `h`).
    ChordContinuation,
    /// Vim `NORMAL`/`VISUAL` owns the key; keys Vim passes through go to the keymap.
    VimThenKeymap,
    /// The key types text (IME / `Option` / `AltGr`): skip shortcuts entirely.
    TextInput,
    /// Resolve against the keymap; unmatched keys continue to the editor and the IME.
    Keymap,
}

/// Flags describing the focused editor at key-down time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "four independent routing conditions sampled per key event"
)]
pub struct RouteFlags {
    /// The Ex command line is open.
    pub command_line_open: bool,
    /// The keymap has a pending chord prefix.
    pub chord_pending: bool,
    /// Vim is enabled, the editor is focused and the mode is not `INSERT`.
    pub vim_owns_keys: bool,
    /// [`is_text_composition`] holds and the editor accepts text (Vim off or `INSERT`).
    pub text_composition: bool,
}

/// Decides the dispatch order for one key (`Ex line → chord → Vim → IME guard → keymap`).
#[must_use]
pub const fn key_route(flags: RouteFlags) -> KeyRoute {
    if flags.command_line_open {
        KeyRoute::CommandLine
    } else if flags.chord_pending {
        KeyRoute::ChordContinuation
    } else if flags.vim_owns_keys {
        KeyRoute::VimThenKeymap
    } else if flags.text_composition {
        KeyRoute::TextInput
    } else {
        KeyRoute::Keymap
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn ks(source: &str) -> gpui::Keystroke {
        gpui::Keystroke::parse(source).expect("valid keystroke")
    }

    fn ks_char(source: &str, key_char: &str) -> gpui::Keystroke {
        let mut keystroke = ks(source);
        keystroke.key_char = Some(key_char.to_owned());
        keystroke
    }

    fn ctx(leaf: FocusContext, vim_enabled: bool) -> DispatchContext {
        DispatchContext {
            leaf,
            vim_enabled,
            editor_read_only: false,
        }
    }

    fn press(
        layer: &mut KeyDispatchLayer,
        source: &str,
        leaf: FocusContext,
        vim: bool,
        now: Instant,
    ) -> DispatchOutcome {
        let stroke = shortcut_stroke(&ks(source)).expect("mappable keystroke");
        layer.dispatch(stroke, ctx(leaf, vim), now)
    }

    #[test]
    fn gpui_keystrokes_map_to_core_strokes() {
        let stroke = shortcut_stroke(&ks("cmd-shift-f12")).unwrap();
        assert_eq!(stroke.to_string(), "shift+cmd+f12");
        assert_eq!(
            shortcut_stroke(&ks("ctrl-alt-left")).unwrap().to_string(),
            "ctrl+alt+left"
        );
        // macOS reports `cmd-{` for Cmd+Shift+[.
        assert_eq!(
            shortcut_stroke(&ks("cmd-{")).unwrap().to_string(),
            "shift+cmd+["
        );
        assert_eq!(shortcut_stroke(&ks("cmd-/")).unwrap().to_string(), "cmd+/");
        assert_eq!(shortcut_stroke(&ks("cmd-,")).unwrap().to_string(), "cmd+,");
        assert!(shortcut_stroke(&ks("a")).is_some());
    }

    #[test]
    fn vim_strokes_use_the_typed_character() {
        // `Shift+;` arrives as key ":" (macOS) or key ";" + key_char ":" (Linux).
        let colon = vim_stroke(&ks_char("shift-;", ":")).unwrap();
        assert_eq!(colon, KeyStroke::unmodified(KeyCode::Char(':')));
        let dollar = vim_stroke(&ks_char("shift-4", "$")).unwrap();
        assert_eq!(dollar, KeyStroke::unmodified(KeyCode::Char('$')));
        let upper = vim_stroke(&ks_char("shift-a", "A")).unwrap();
        assert_eq!(upper, KeyStroke::new(Modifiers::SHIFT, KeyCode::Char('a')));
        let esc = vim_stroke(&ks("escape")).unwrap();
        assert_eq!(esc, KeyStroke::unmodified(KeyCode::Escape));
        let ctrl_w = vim_stroke(&ks("ctrl-w")).unwrap();
        assert_eq!(ctrl_w, KeyStroke::new(Modifiers::CTRL, KeyCode::Char('w')));
    }

    #[test]
    fn option_and_altgr_letters_are_text_not_shortcuts() {
        // macOS Polish Pro: Option+a → ą, Option+Shift+z → Ż.
        assert!(is_text_composition(&ks_char("alt-a", "ą"), Platform::MacOs));
        assert!(is_text_composition(
            &ks_char("alt-shift-z", "Ż"),
            Platform::MacOs
        ));
        // Option + named key or with Cmd/Ctrl stays a shortcut.
        assert!(!is_text_composition(
            &ks_char("alt-up", "\u{f700}"),
            Platform::MacOs
        ));
        assert!(!is_text_composition(&ks("alt-f12"), Platform::MacOs));
        assert!(!is_text_composition(&ks("cmd-alt-l"), Platform::MacOs));
        assert!(!is_text_composition(&ks("ctrl-alt-t"), Platform::MacOs));
        assert!(!is_text_composition(&ks_char("a", "a"), Platform::MacOs));
        // Linux AltGr: non-ASCII text; plain Alt+1 / Alt+J remain shortcuts.
        assert!(is_text_composition(
            &ks_char("alt-ctrl-a", "ą"),
            Platform::Linux
        ));
        assert!(is_text_composition(&ks_char("alt-s", "ś"), Platform::Linux));
        assert!(!is_text_composition(
            &ks_char("alt-1", "1"),
            Platform::Linux
        ));
        assert!(!is_text_composition(
            &ks_char("alt-j", "j"),
            Platform::Linux
        ));
        assert!(!is_text_composition(
            &ks_char("super-alt-left", "x"),
            Platform::Linux
        ));
    }

    #[test]
    fn jetbrains_shell_shortcuts_resolve_per_platform() {
        let now = Instant::now();
        let mut mac = KeyDispatchLayer::new(Platform::MacOs);
        let editor = FocusContext::EditorText;
        assert_eq!(
            press(&mut mac, "cmd-shift-f12", editor, false, now),
            DispatchOutcome::Action(ShellAction::ToggleFocusMode(FocusModeVariant::HideDocks))
        );
        assert_eq!(
            press(&mut mac, "ctrl-alt-z", editor, false, now),
            DispatchOutcome::Action(ShellAction::ToggleFocusMode(FocusModeVariant::Zen))
        );
        assert_eq!(
            press(&mut mac, "cmd-1", editor, false, now),
            DispatchOutcome::Action(ShellAction::ToggleTool(DockToolId::Project))
        );
        assert_eq!(
            press(&mut mac, "cmd-w", editor, false, now),
            DispatchOutcome::Action(ShellAction::CloseActiveTab)
        );
        assert_eq!(
            press(&mut mac, "cmd-d", editor, false, now),
            DispatchOutcome::Action(ShellAction::DuplicateLine)
        );
        assert_eq!(
            press(&mut mac, "cmd-/", editor, false, now),
            DispatchOutcome::Action(ShellAction::ToggleLineComment)
        );
        assert_eq!(
            press(&mut mac, "ctrl-g", editor, false, now),
            DispatchOutcome::Action(ShellAction::AddNextOccurrence)
        );
        assert_eq!(
            press(&mut mac, "cmd-s", editor, false, now),
            DispatchOutcome::Action(ShellAction::Save)
        );
        assert_eq!(
            press(&mut mac, "cmd-k", editor, false, now),
            DispatchOutcome::Action(ShellAction::ToggleCommitPanel)
        );
        assert_eq!(
            press(&mut mac, "ctrl-alt-t", editor, false, now),
            DispatchOutcome::Action(ShellAction::ToggleTilingMode)
        );
        assert_eq!(
            press(&mut mac, "ctrl-alt-right", editor, false, now),
            DispatchOutcome::Action(ShellAction::FocusDirection(Direction::Right))
        );
        assert_eq!(
            press(&mut mac, "ctrl-alt-shift-left", editor, false, now),
            DispatchOutcome::Action(ShellAction::MoveTabToSplit(Direction::Left))
        );
        assert_eq!(
            press(&mut mac, "ctrl-alt-cmd-up", editor, false, now),
            DispatchOutcome::Action(ShellAction::ResizeSplit(Direction::Up))
        );
        // Clipboard stays with the KOVA-015 GPUI actions.
        assert!(matches!(
            press(&mut mac, "cmd-c", editor, false, now),
            DispatchOutcome::Delegated(_)
        ));
        assert_eq!(
            press(&mut mac, "x", editor, false, now),
            DispatchOutcome::Unhandled
        );

        let mut linux = KeyDispatchLayer::new(Platform::Linux);
        assert_eq!(
            press(&mut linux, "ctrl-shift-f12", editor, false, now),
            DispatchOutcome::Action(ShellAction::ToggleFocusMode(FocusModeVariant::HideDocks))
        );
        assert_eq!(
            press(&mut linux, "alt-1", editor, false, now),
            DispatchOutcome::Action(ShellAction::ToggleTool(DockToolId::Project))
        );
        assert_eq!(
            press(&mut linux, "alt-j", editor, false, now),
            DispatchOutcome::Action(ShellAction::AddNextOccurrence)
        );
        assert_eq!(
            press(&mut linux, "ctrl-f4", editor, false, now),
            DispatchOutcome::Action(ShellAction::CloseActiveTab)
        );
        assert_eq!(
            press(&mut linux, "super-alt-down", editor, false, now),
            DispatchOutcome::Action(ShellAction::FocusDirection(Direction::Down))
        );
        assert_eq!(
            press(&mut linux, "ctrl-k", editor, false, now),
            DispatchOutcome::Action(ShellAction::ToggleCommitPanel)
        );
        assert_eq!(linux.platform(), Platform::Linux);
    }

    #[test]
    fn vim_ctrl_w_chord_navigates_and_times_out() {
        let t0 = Instant::now();
        for platform in [Platform::MacOs, Platform::Linux] {
            let mut layer = KeyDispatchLayer::new(platform);
            let normal = FocusContext::EditorVimNormal;
            let DispatchOutcome::Pending { hint, epoch } =
                press(&mut layer, "ctrl-w", normal, true, t0)
            else {
                panic!("ctrl+w must start a chord in Vim NORMAL on {platform}");
            };
            assert_eq!(hint, "ctrl+w …");
            assert!(layer.has_pending());
            assert_eq!(layer.pending_hint().as_deref(), Some("ctrl+w …"));
            assert_eq!(
                press(
                    &mut layer,
                    "l",
                    normal,
                    true,
                    t0 + Duration::from_millis(200)
                ),
                DispatchOutcome::Action(ShellAction::FocusDirection(Direction::Right))
            );
            // Stale timer from the first prefix never clears anything.
            assert!(!layer.expire(epoch));

            // Second prefix, then the timer fires: prefix cleared, `h` is a plain key again.
            let DispatchOutcome::Pending { epoch, .. } =
                press(&mut layer, "ctrl-w", normal, true, t0)
            else {
                panic!("second ctrl+w must be pending");
            };
            assert!(layer.expire(epoch));
            assert!(!layer.has_pending());
            assert_eq!(
                press(&mut layer, "h", normal, true, t0),
                DispatchOutcome::Unhandled
            );

            // Escape cancels a pending prefix.
            assert!(matches!(
                press(&mut layer, "ctrl-w", normal, true, t0),
                DispatchOutcome::Pending { .. }
            ));
            assert_eq!(
                press(&mut layer, "escape", normal, true, t0),
                DispatchOutcome::Cancelled
            );
            // Session-level timeout also applies without the timer.
            assert!(matches!(
                press(&mut layer, "ctrl-w", normal, true, t0),
                DispatchOutcome::Pending { .. }
            ));
            assert_eq!(
                press(
                    &mut layer,
                    "v",
                    normal,
                    true,
                    t0 + Duration::from_millis(1500)
                ),
                DispatchOutcome::Unhandled
            );
            layer.reset();
            assert!(!layer.has_pending());
        }
    }

    #[test]
    fn key_route_orders_command_line_chord_vim_ime_keymap() {
        let all = RouteFlags {
            command_line_open: true,
            chord_pending: true,
            vim_owns_keys: true,
            text_composition: true,
        };
        assert_eq!(key_route(all), KeyRoute::CommandLine);
        let chord = RouteFlags {
            command_line_open: false,
            ..all
        };
        assert_eq!(key_route(chord), KeyRoute::ChordContinuation);
        let vim = RouteFlags {
            chord_pending: false,
            ..chord
        };
        assert_eq!(key_route(vim), KeyRoute::VimThenKeymap);
        let text = RouteFlags {
            vim_owns_keys: false,
            ..vim
        };
        assert_eq!(key_route(text), KeyRoute::TextInput);
        assert_eq!(key_route(RouteFlags::default()), KeyRoute::Keymap);
    }

    #[test]
    fn jetbrains_debugger_keys_dispatch_debug_actions() {
        let now = Instant::now();
        let cases: [(&str, DebugShellAction); 5] = [
            ("f7", DebugShellAction::StepInto),
            ("f8", DebugShellAction::StepOver),
            ("shift-f8", DebugShellAction::StepOut),
            ("f9", DebugShellAction::Resume),
            ("alt-f8", DebugShellAction::EvaluateExpression),
        ];
        let mut linux = KeyDispatchLayer::new(Platform::Linux);
        for (key, action) in cases {
            for leaf in [FocusContext::Workspace, FocusContext::EditorText] {
                assert_eq!(
                    press(&mut linux, key, leaf, false, now),
                    DispatchOutcome::Action(ShellAction::Debug(action)),
                    "{key} in {leaf:?}"
                );
            }
        }
        assert_eq!(
            press(&mut linux, "ctrl-f8", FocusContext::EditorText, false, now),
            DispatchOutcome::Action(ShellAction::Debug(DebugShellAction::ToggleBreakpoint))
        );
        assert_eq!(
            press(
                &mut linux,
                "ctrl-shift-f9",
                FocusContext::Workspace,
                false,
                now
            ),
            DispatchOutcome::Action(ShellAction::Debug(DebugShellAction::DebugContext))
        );
        // macOS: same stepping keys; *Resume* is `Cmd+Alt+R` in the JetBrains macOS keymap.
        let mut mac = KeyDispatchLayer::new(Platform::MacOs);
        for (key, action) in [
            ("f7", DebugShellAction::StepInto),
            ("f8", DebugShellAction::StepOver),
            ("shift-f8", DebugShellAction::StepOut),
            ("alt-f8", DebugShellAction::EvaluateExpression),
            ("cmd-alt-r", DebugShellAction::Resume),
        ] {
            assert_eq!(
                press(&mut mac, key, FocusContext::Workspace, false, now),
                DispatchOutcome::Action(ShellAction::Debug(action)),
                "{key}"
            );
        }
        assert_eq!(
            press(&mut mac, "cmd-f8", FocusContext::EditorText, false, now),
            DispatchOutcome::Action(ShellAction::Debug(DebugShellAction::ToggleBreakpoint))
        );
    }

    #[test]
    fn every_window_action_maps_to_a_shell_action() {
        for id in [
            ids::VIEW_TOGGLE_TILING_MODE,
            ids::WINDOW_FOCUS_LEFT,
            ids::WINDOW_FOCUS_RIGHT,
            ids::WINDOW_FOCUS_UP,
            ids::WINDOW_FOCUS_DOWN,
            ids::WINDOW_MOVE_TAB_LEFT,
            ids::WINDOW_MOVE_TAB_RIGHT,
            ids::WINDOW_MOVE_TAB_UP,
            ids::WINDOW_MOVE_TAB_DOWN,
            ids::WINDOW_SPLIT_RIGHT,
            ids::WINDOW_SPLIT_DOWN,
            ids::WINDOW_RESIZE_LEFT,
            ids::WINDOW_RESIZE_RIGHT,
            ids::WINDOW_RESIZE_UP,
            ids::WINDOW_RESIZE_DOWN,
            ids::VIM_TOGGLE,
            ids::TOOL_WINDOW_HIDE_ALL,
            ids::GIT_COMMIT,
            ids::NAVIGATION_GO_TO_DECLARATION,
            ids::NAVIGATION_BACK,
            ids::NAVIGATION_FORWARD,
        ] {
            assert!(
                ShellAction::from_action_id(&ActionId::from_static(id)).is_some(),
                "{id} must map to a ShellAction"
            );
        }
        assert_eq!(
            ShellAction::from_action_id(&ActionId::from_static(ids::EDITOR_COPY)),
            None
        );
    }

    #[test]
    fn double_shift_triggers_via_modifiers_changed_in_editor_vim_terminal_and_tree() {
        for leaf in [
            FocusContext::EditorText,
            FocusContext::EditorVimNormal,
            FocusContext::Terminal,
            FocusContext::ProjectTree,
        ] {
            let mut layer = KeyDispatchLayer::new(Platform::MacOs);
            let ctx = ctx(leaf, leaf == FocusContext::EditorVimNormal);
            let t0 = Instant::now();

            assert_eq!(
                layer.dispatch_modifiers_changed(gpui::Modifiers::shift(), false, ctx, t0),
                DispatchOutcome::Unhandled
            );
            assert_eq!(
                layer.dispatch_modifiers_changed(
                    gpui::Modifiers::none(),
                    false,
                    ctx,
                    t0 + Duration::from_millis(50),
                ),
                DispatchOutcome::Unhandled
            );
            assert_eq!(
                layer.dispatch_modifiers_changed(
                    gpui::Modifiers::shift(),
                    false,
                    ctx,
                    t0 + Duration::from_millis(120),
                ),
                DispatchOutcome::Unhandled
            );
            assert_eq!(
                layer.dispatch_modifiers_changed(
                    gpui::Modifiers::none(),
                    false,
                    ctx,
                    t0 + Duration::from_millis(180),
                ),
                DispatchOutcome::Action(ShellAction::OpenSearchEverywhere),
                "double shift must trigger Search Everywhere in {leaf:?}"
            );
        }
    }

    #[test]
    fn double_shift_cancelled_by_key_down_in_editor_and_vim() {
        let t0 = Instant::now();
        for (leaf, vim) in [
            (FocusContext::EditorText, false),
            (FocusContext::EditorVimNormal, true),
        ] {
            let mut layer = KeyDispatchLayer::new(Platform::MacOs);
            let c = ctx(leaf, vim);
            layer.dispatch_modifiers_changed(gpui::Modifiers::shift(), false, c, t0);
            layer.cancel_modifier_tap();
            layer.dispatch_modifiers_changed(
                gpui::Modifiers::none(),
                false,
                c,
                t0 + Duration::from_millis(20),
            );
            layer.dispatch_modifiers_changed(
                gpui::Modifiers::shift(),
                false,
                c,
                t0 + Duration::from_millis(60),
            );
            assert_eq!(
                layer.dispatch_modifiers_changed(
                    gpui::Modifiers::none(),
                    false,
                    c,
                    t0 + Duration::from_millis(100),
                ),
                DispatchOutcome::Unhandled
            );
        }
    }

    #[test]
    fn double_shift_cancelled_by_other_modifiers_capslock_and_timeout() {
        let t0 = Instant::now();
        let editor_ctx = ctx(FocusContext::EditorText, false);

        // Cmd+Shift released in reverse order (Cmd released first, then Shift)
        let mut layer = KeyDispatchLayer::new(Platform::MacOs);
        layer.dispatch_modifiers_changed(gpui::Modifiers::shift(), false, editor_ctx, t0);
        layer.dispatch_modifiers_changed(
            gpui::Modifiers::command_shift(),
            false,
            editor_ctx,
            t0 + Duration::from_millis(20),
        );
        layer.dispatch_modifiers_changed(
            gpui::Modifiers::shift(),
            false,
            editor_ctx,
            t0 + Duration::from_millis(40),
        );
        layer.dispatch_modifiers_changed(
            gpui::Modifiers::none(),
            false,
            editor_ctx,
            t0 + Duration::from_millis(60),
        );
        layer.dispatch_modifiers_changed(
            gpui::Modifiers::shift(),
            false,
            editor_ctx,
            t0 + Duration::from_millis(90),
        );
        assert_eq!(
            layer.dispatch_modifiers_changed(
                gpui::Modifiers::none(),
                false,
                editor_ctx,
                t0 + Duration::from_millis(120),
            ),
            DispatchOutcome::Unhandled
        );

        // Fn+Shift, Alt+Shift, Ctrl+Shift, or CapsLock toggle cancels
        let fn_shift = gpui::Modifiers {
            function: true,
            ..gpui::Modifiers::shift()
        };
        let alt_shift = gpui::Modifiers::alt() | gpui::Modifiers::shift();
        for (other, caps) in [
            (fn_shift, false),
            (alt_shift, false),
            (gpui::Modifiers::control_shift(), false),
            (gpui::Modifiers::shift(), true),
        ] {
            let mut layer = KeyDispatchLayer::new(Platform::MacOs);
            layer.dispatch_modifiers_changed(gpui::Modifiers::shift(), false, editor_ctx, t0);
            layer.dispatch_modifiers_changed(
                other,
                caps,
                editor_ctx,
                t0 + Duration::from_millis(20),
            );
            layer.dispatch_modifiers_changed(
                gpui::Modifiers::none(),
                caps,
                editor_ctx,
                t0 + Duration::from_millis(40),
            );
            layer.dispatch_modifiers_changed(
                gpui::Modifiers::shift(),
                caps,
                editor_ctx,
                t0 + Duration::from_millis(80),
            );
            assert_eq!(
                layer.dispatch_modifiers_changed(
                    gpui::Modifiers::none(),
                    caps,
                    editor_ctx,
                    t0 + Duration::from_millis(110),
                ),
                DispatchOutcome::Unhandled
            );
        }
    }

    #[test]
    fn double_shift_expires_after_400ms_timeout() {
        let t0 = Instant::now();
        let editor_ctx = ctx(FocusContext::EditorText, false);
        let mut layer = KeyDispatchLayer::new(Platform::MacOs);
        layer.dispatch_modifiers_changed(gpui::Modifiers::shift(), false, editor_ctx, t0);
        layer.dispatch_modifiers_changed(
            gpui::Modifiers::none(),
            false,
            editor_ctx,
            t0 + Duration::from_millis(40),
        );
        layer.dispatch_modifiers_changed(
            gpui::Modifiers::shift(),
            false,
            editor_ctx,
            t0 + Duration::from_millis(460),
        );
        assert_eq!(
            layer.dispatch_modifiers_changed(
                gpui::Modifiers::none(),
                false,
                editor_ctx,
                t0 + Duration::from_millis(500),
            ),
            DispatchOutcome::Unhandled
        );
    }

    #[test]
    fn navigation_shortcuts_and_read_only_context_dispatch() {
        let now = Instant::now();
        let mut mac = KeyDispatchLayer::new(Platform::MacOs);
        assert_eq!(
            press(&mut mac, "cmd-b", FocusContext::EditorText, false, now),
            DispatchOutcome::Action(ShellAction::GoToDefinition)
        );
        assert_eq!(
            press(&mut mac, "cmd-[", FocusContext::EditorText, false, now),
            DispatchOutcome::Action(ShellAction::NavigateBack)
        );
        assert_eq!(
            press(&mut mac, "cmd-]", FocusContext::EditorText, false, now),
            DispatchOutcome::Action(ShellAction::NavigateForward)
        );

        let mut linux = KeyDispatchLayer::new(Platform::Linux);
        assert_eq!(
            press(&mut linux, "ctrl-b", FocusContext::EditorText, false, now),
            DispatchOutcome::Action(ShellAction::GoToDefinition)
        );
        assert_eq!(
            press(
                &mut linux,
                "ctrl-alt-left",
                FocusContext::Workspace,
                false,
                now
            ),
            DispatchOutcome::Action(ShellAction::NavigateBack)
        );
        assert_eq!(
            press(
                &mut linux,
                "ctrl-alt-right",
                FocusContext::Workspace,
                false,
                now
            ),
            DispatchOutcome::Action(ShellAction::NavigateForward)
        );

        // Read-only editors still allow GoToDefinition, while EditableText actions like DuplicateLine do not match.
        let ro_ctx = DispatchContext {
            leaf: FocusContext::EditorText,
            vim_enabled: false,
            editor_read_only: true,
        };
        assert_eq!(
            mac.dispatch(shortcut_stroke(&ks("cmd-b")).unwrap(), ro_ctx, now),
            DispatchOutcome::Action(ShellAction::GoToDefinition)
        );
        assert_eq!(
            mac.dispatch(shortcut_stroke(&ks("cmd-d")).unwrap(), ro_ctx, now),
            DispatchOutcome::Unhandled
        );
    }

    #[test]
    fn intellij_shortcuts_resolve_on_macos() {
        let now = Instant::now();
        let editor = FocusContext::EditorText;
        let mut mac = KeyDispatchLayer::new(Platform::MacOs);
        let mac_cases = [
            ("cmd-o", ShellAction::OpenSearchScope(SearchScope::Classes)),
            (
                "cmd-shift-o",
                ShellAction::OpenSearchScope(SearchScope::Files),
            ),
            (
                "cmd-alt-o",
                ShellAction::OpenSearchScope(SearchScope::Symbols),
            ),
            (
                "cmd-shift-a",
                ShellAction::OpenSearchScope(SearchScope::Actions),
            ),
            ("cmd-e", ShellAction::OpenRecentFiles),
            ("cmd-l", ShellAction::GoToLine),
            ("cmd-f12", ShellAction::ToggleTool(DockToolId::Structure)),
            ("f2", ShellAction::NextProblem),
            ("shift-f2", ShellAction::PrevProblem),
            ("cmd-backspace", ShellAction::DeleteLine),
            ("cmd-alt-/", ShellAction::ToggleBlockComment),
            ("alt-shift-up", ShellAction::MoveLine(true)),
            ("alt-shift-down", ShellAction::MoveLine(false)),
            ("alt-up", ShellAction::ExtendSelection),
            ("alt-down", ShellAction::ShrinkSelection),
            ("cmd--", ShellAction::FoldRegion),
            ("cmd-=", ShellAction::UnfoldRegion),
            ("cmd-shift--", ShellAction::FoldAll),
            ("cmd-shift-=", ShellAction::UnfoldAll),
            ("cmd-alt-l", ShellAction::ReformatCode),
            ("ctrl-g", ShellAction::AddNextOccurrence),
            ("ctrl-shift-g", ShellAction::UnselectOccurrence),
            ("ctrl-cmd-g", ShellAction::SelectAllOccurrences),
            ("cmd-f", ShellAction::Find),
            ("cmd-r", ShellAction::Replace),
            ("cmd-g", ShellAction::FindNext),
            ("cmd-shift-g", ShellAction::FindPrev),
            ("cmd-shift-f", ShellAction::FindInFiles),
            ("ctrl-shift-r", ShellAction::RunContext),
            ("ctrl-r", ShellAction::RunLast),
            ("escape", ShellAction::EditorEscape),
        ];
        for (shortcut, expected) in mac_cases {
            assert_eq!(
                press(&mut mac, shortcut, editor, false, now),
                DispatchOutcome::Action(expected),
                "macOS {shortcut}"
            );
        }
        // In Vim NORMAL, `Ctrl+R` is passed through to Vim Redo while `editor.escape` is disabled.
        assert_eq!(
            press(&mut mac, "escape", FocusContext::EditorVimNormal, true, now),
            DispatchOutcome::Unhandled
        );
    }

    #[test]
    fn intellij_shortcuts_resolve_on_linux() {
        let now = Instant::now();
        let editor = FocusContext::EditorText;
        let mut linux = KeyDispatchLayer::new(Platform::Linux);
        let linux_cases = [
            ("ctrl-n", ShellAction::OpenSearchScope(SearchScope::Classes)),
            (
                "ctrl-shift-n",
                ShellAction::OpenSearchScope(SearchScope::Files),
            ),
            (
                "ctrl-alt-shift-n",
                ShellAction::OpenSearchScope(SearchScope::Symbols),
            ),
            (
                "ctrl-shift-a",
                ShellAction::OpenSearchScope(SearchScope::Actions),
            ),
            ("ctrl-e", ShellAction::OpenRecentFiles),
            ("ctrl-g", ShellAction::GoToLine),
            ("ctrl-f12", ShellAction::ToggleTool(DockToolId::Structure)),
            ("f2", ShellAction::NextProblem),
            ("shift-f2", ShellAction::PrevProblem),
            ("ctrl-y", ShellAction::DeleteLine),
            ("ctrl-shift-/", ShellAction::ToggleBlockComment),
            ("alt-shift-up", ShellAction::MoveLine(true)),
            ("alt-shift-down", ShellAction::MoveLine(false)),
            ("ctrl-w", ShellAction::ExtendSelection),
            ("ctrl-shift-w", ShellAction::ShrinkSelection),
            ("ctrl--", ShellAction::FoldRegion),
            ("ctrl-=", ShellAction::UnfoldRegion),
            ("ctrl-shift--", ShellAction::FoldAll),
            ("ctrl-shift-=", ShellAction::UnfoldAll),
            ("ctrl-alt-l", ShellAction::ReformatCode),
            ("alt-j", ShellAction::AddNextOccurrence),
            ("alt-shift-j", ShellAction::UnselectOccurrence),
            ("ctrl-alt-shift-j", ShellAction::SelectAllOccurrences),
            ("ctrl-f", ShellAction::Find),
            ("ctrl-r", ShellAction::Replace),
            ("f3", ShellAction::FindNext),
            ("shift-f3", ShellAction::FindPrev),
            ("ctrl-shift-f", ShellAction::FindInFiles),
            ("ctrl-shift-f10", ShellAction::RunContext),
            ("shift-f10", ShellAction::RunLast),
            ("escape", ShellAction::EditorEscape),
        ];
        for (shortcut, expected) in linux_cases {
            assert_eq!(
                press(&mut linux, shortcut, editor, false, now),
                DispatchOutcome::Action(expected),
                "Linux {shortcut}"
            );
        }
    }

    #[test]
    fn intellij_shortcuts_respect_read_only_context() {
        let now = Instant::now();
        let mut mac = KeyDispatchLayer::new(Platform::MacOs);
        let ro_ctx = DispatchContext {
            leaf: FocusContext::EditorText,
            vim_enabled: false,
            editor_read_only: true,
        };
        for shortcut in [
            "alt-up",
            "alt-down",
            "cmd--",
            "cmd-=",
            "cmd-f",
            "cmd-g",
            "ctrl-shift-g",
            "ctrl-cmd-g",
        ] {
            assert!(
                matches!(
                    mac.dispatch(shortcut_stroke(&ks(shortcut)).unwrap(), ro_ctx, now),
                    DispatchOutcome::Action(_)
                ),
                "read-only should allow {shortcut}"
            );
        }
        for shortcut in [
            "cmd-backspace",
            "cmd-alt-/",
            "alt-shift-up",
            "alt-shift-down",
            "cmd-alt-l",
            "ctrl-space",
        ] {
            assert_eq!(
                mac.dispatch(shortcut_stroke(&ks(shortcut)).unwrap(), ro_ctx, now),
                DispatchOutcome::Unhandled,
                "read-only must block {shortcut}"
            );
        }
    }

    #[test]
    fn ctrl_space_resolves_to_complete_basic_on_macos_and_linux() {
        let now = Instant::now();
        for platform in [Platform::MacOs, Platform::Linux] {
            let mut layer = KeyDispatchLayer::new(platform);
            assert_eq!(
                press(
                    &mut layer,
                    "ctrl-space",
                    FocusContext::EditorText,
                    false,
                    now
                ),
                DispatchOutcome::Action(ShellAction::CompleteBasic),
                "{platform:?} editable"
            );
            assert_eq!(
                press(
                    &mut layer,
                    "ctrl-space",
                    FocusContext::EditorVimInsert,
                    true,
                    now
                ),
                DispatchOutcome::Action(ShellAction::CompleteBasic),
                "{platform:?} vim insert"
            );
        }
    }
}
