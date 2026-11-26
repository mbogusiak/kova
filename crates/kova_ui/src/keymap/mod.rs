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

use kova_core::builtin_actions::ids;
use kova_core::keymap::chord::DEFAULT_CHORD_TIMEOUT;
use kova_core::{
    ActionId, ContextState, FocusContext, FocusStack, KeyCode, KeyDispatch, KeyStep, KeyStroke,
    KeymapResolver, KeymapSession, Modifiers, Platform, build_effective_keymap,
    jetbrains_keymap_layer,
};

pub use ex::{ExCommand, ExCommandLine, ExLineEvent, ExParseError, parse_ex_command};

use crate::dock::{Direction, DockToolId, FocusModeVariant, SplitAxis};

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
    /// Open *Settings* (`Cmd+,`).
    OpenSettings,
}

impl ShellAction {
    /// Maps a keymap action onto a shell command; `None` for actions handled elsewhere
    /// (clipboard, undo, caret motions bound as GPUI actions by `KOVA-015`).
    #[must_use]
    pub fn from_action_id(action: &ActionId) -> Option<Self> {
        Some(match action.as_str() {
            ids::TOOL_WINDOW_PROJECT => Self::ToggleTool(DockToolId::Project),
            ids::TOOL_WINDOW_STRUCTURE => Self::ToggleTool(DockToolId::Structure),
            ids::TOOL_WINDOW_RUN => Self::ToggleTool(DockToolId::Run),
            ids::TOOL_WINDOW_PROBLEMS => Self::ToggleTool(DockToolId::Problems),
            ids::TOOL_WINDOW_GIT => Self::ToggleTool(DockToolId::Commit),
            ids::TOOL_WINDOW_TERMINAL => Self::ToggleTool(DockToolId::Terminal),
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
            ids::EDITOR_SAVE => Self::Save,
            ids::EDITOR_DUPLICATE_LINE => Self::DuplicateLine,
            ids::EDITOR_TOGGLE_LINE_COMMENT => Self::ToggleLineComment,
            ids::EDITOR_ADD_NEXT_OCCURRENCE => Self::AddNextOccurrence,
            ids::NAVIGATION_SEARCH_EVERYWHERE => Self::OpenSearchEverywhere,
            ids::VIEW_OPEN_SETTINGS => Self::OpenSettings,
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
        if self.cached_leaf != ctx.leaf {
            self.cached_leaf = ctx.leaf;
            self.cached_stack = FocusStack::from_leaf(ctx.leaf);
        }
        let had_pending = self.has_pending();
        let state = if ctx.vim_enabled {
            &self.state_vim_on
        } else {
            &self.state_vim_off
        };
        let result =
            self.session
                .on_key_down(&self.resolver, stroke, &self.cached_stack, state, now);
        self.epoch = self.epoch.wrapping_add(1);
        match result {
            KeyDispatch::Matched { action, .. } => ShellAction::from_action_id(&action)
                .map_or(DispatchOutcome::Delegated(action), DispatchOutcome::Action),
            KeyDispatch::Pending { prefix, .. } => DispatchOutcome::Pending {
                hint: format_hint(&prefix),
                epoch: self.epoch,
            },
            KeyDispatch::NotMatched
                if had_pending && stroke == KeyStroke::unmodified(KeyCode::Escape) =>
            {
                DispatchOutcome::Cancelled
            }
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
        DispatchContext { leaf, vim_enabled }
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
}
