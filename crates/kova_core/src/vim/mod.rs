//! Headless modal Vim engine (IdeaVim-style) for the Kova editor.
//!
//! [`VimEngine`] owns a [`kova_buffer::Buffer`] together with a byte-offset
//! cursor and executes Vim commands (modes, counts, motions, operators, text
//! objects, registers, dot-repeat and undo/redo) entirely in memory, without
//! any UI or GPU dependency.
//!
//! Keys that Vim does not own (IDE shortcuts such as `Alt+Enter` or `Cmd+/`,
//! or `Ctrl` chords assigned to the IDE by [`KeyConflictPolicy`]) are reported
//! as [`VimOutcome::PassThrough`] so the caller can dispatch IDE actions.

mod engine;
mod motion;

pub use engine::{Register, VimEngine, VimStatus};

use std::collections::HashMap;

use crate::context::FocusContext;
use crate::keymap::{KeyCode, KeyStroke};

/// Modal editing mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VimMode {
    /// Command mode (default).
    #[default]
    Normal,
    /// Text insertion.
    Insert,
    /// Characterwise visual selection.
    Visual,
    /// Linewise visual selection.
    VisualLine,
}

impl VimMode {
    /// Focus context to use for keymap resolution in this mode.
    #[must_use]
    pub const fn focus_context(self) -> FocusContext {
        match self {
            Self::Normal => FocusContext::EditorVimNormal,
            Self::Insert => FocusContext::EditorVimInsert,
            Self::Visual => FocusContext::EditorVimVisual,
            Self::VisualLine => FocusContext::EditorVimVisualLine,
        }
    }

    /// Whether a visual selection is active.
    #[must_use]
    pub const fn is_visual(self) -> bool {
        matches!(self, Self::Visual | Self::VisualLine)
    }
}

/// Shape of the text cursor for the current Vim state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VimCursorShape {
    /// Filled block (Normal, Visual).
    Block,
    /// Thin vertical bar (Insert).
    Bar,
    /// Underline (operator pending).
    Underline,
}

/// Who owns a conflicting `Ctrl` chord (analogue of `sethandler` in `.ideavimrc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeyConflictPolicy {
    /// The Vim engine handles the chord.
    #[default]
    Vim,
    /// The chord is left to the IDE keymap.
    Ide,
}

/// Result of feeding a key to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VimOutcome {
    /// The engine consumed the key.
    Handled,
    /// The key is not owned by Vim; dispatch it to the IDE keymap.
    PassThrough,
    /// `gd` in Normal mode: trigger IDE *Go to Definition* at the current cursor.
    GoToDefinition,
}

/// Normalized key as understood by the Vim state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VimKey {
    /// Printable character (case already applied).
    Char(char),
    /// `Ctrl` + lowercase letter.
    Ctrl(char),
    /// Escape.
    Escape,
    /// Enter.
    Enter,
    /// Backspace.
    Backspace,
    /// Forward delete.
    Delete,
    /// Tab.
    Tab,
    /// Cursor left.
    Left,
    /// Cursor right.
    Right,
    /// Cursor up.
    Up,
    /// Cursor down.
    Down,
    /// Home.
    Home,
    /// End.
    End,
}

impl VimKey {
    /// Converts a [`KeyStroke`]; returns `None` for chords Vim never owns
    /// (`Alt`/`Cmd` combinations, function keys, ...).
    #[must_use]
    pub fn from_stroke(stroke: KeyStroke) -> Option<Self> {
        let m = stroke.modifiers;
        if m.alt || m.meta {
            return None;
        }
        Some(match stroke.key {
            KeyCode::Char(c) if m.ctrl => {
                if c == '[' {
                    Self::Escape
                } else {
                    Self::Ctrl(c.to_ascii_lowercase())
                }
            }
            KeyCode::Char(c) if m.shift => Self::Char(c.to_ascii_uppercase()),
            KeyCode::Char(c) => Self::Char(c),
            KeyCode::Space if !m.ctrl => Self::Char(' '),
            KeyCode::Enter => Self::Enter,
            KeyCode::Tab => Self::Tab,
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Delete => Self::Delete,
            KeyCode::Escape => Self::Escape,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::PageDown => Self::Ctrl('f'),
            KeyCode::PageUp => Self::Ctrl('b'),
            _ => return None,
        })
    }

    /// Converts a plain character as typed in tests: `\u{1b}` is Escape,
    /// `\n` Enter, `\u{8}` Backspace, `\t` Tab, `\u{1}..=\u{1a}` `Ctrl+letter`.
    #[must_use]
    pub fn from_char(c: char) -> Self {
        match c {
            '\u{1b}' => Self::Escape,
            '\n' | '\r' => Self::Enter,
            '\u{8}' | '\u{7f}' => Self::Backspace,
            '\t' => Self::Tab,
            '\u{1}'..='\u{1a}' => {
                let letter = char::from_u32(u32::from('a') + u32::from(c) - 1).unwrap_or(c);
                Self::Ctrl(letter)
            }
            _ => Self::Char(c),
        }
    }
}

/// Per-chord conflict policy table.
#[derive(Debug, Clone, Default)]
pub(crate) struct PolicyTable {
    default: KeyConflictPolicy,
    overrides: HashMap<char, KeyConflictPolicy>,
}

impl PolicyTable {
    pub(crate) fn get(&self, c: char) -> KeyConflictPolicy {
        self.overrides.get(&c).copied().unwrap_or(self.default)
    }

    pub(crate) fn set_default(&mut self, p: KeyConflictPolicy) {
        self.default = p;
    }

    pub(crate) fn set(&mut self, c: char, p: KeyConflictPolicy) {
        self.overrides.insert(c, p);
    }
}

#[cfg(test)]
mod tests;
