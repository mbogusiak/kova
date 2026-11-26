//! Integrated PTY terminal in the bottom dock (`KOVA-039`).
//!
//! * [`terminal_view`] — one terminal session rendered from the `kova_tools` grid snapshot
//!   with the `kova_theme` `[terminal]` palette, keyboard / mouse / clipboard input and resize.
//! * [`terminal_dock`] — the multi-tab container (new, close, rename) and its headless model.
//!
//! The PTY itself, the emulator and the Omakase `JAVA_HOME` injection live in the headless
//! `kova_tools::terminal` core.

pub mod terminal_dock;
pub mod terminal_view;

pub use terminal_dock::{TerminalDock, TerminalTabId, TerminalTabs};
pub use terminal_view::{TerminalLaunch, TerminalView, TerminalViewEvent};

/// Terminal-level shortcut resolved before a keystroke is forwarded to the shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalShortcut {
    /// `Alt+F12`: hide the terminal and return to the editor.
    Hide,
    /// `Cmd+T` / `Ctrl+Shift+T`: open a new terminal tab.
    NewTab,
    /// `Cmd+W` / `Ctrl+Shift+W`: close the active terminal tab.
    CloseTab,
    /// `Cmd+K` / `Ctrl+Shift+K`: clear the screen and scrollback.
    Clear,
    /// `Cmd+C` / `Ctrl+Shift+C`: copy the selection.
    Copy,
    /// `Cmd+V` / `Ctrl+Shift+V`: paste the clipboard.
    Paste,
}

/// Maps a keystroke to a terminal shortcut for the given platform (`macos` = `Cmd` based).
///
/// Every other keystroke without `Cmd` is forwarded to the shell, so `Ctrl+C`, `Ctrl+W`,
/// `Esc` and friends reach the program instead of the IDE keymap.
#[must_use]
pub fn terminal_shortcut(keystroke: &gpui::Keystroke, macos: bool) -> Option<TerminalShortcut> {
    let m = &keystroke.modifiers;
    let key = keystroke.key.as_str();
    if key == "f12" && m.alt && !m.platform && !m.control && !m.shift {
        return Some(TerminalShortcut::Hide);
    }
    let chord = if macos {
        m.platform && !m.control && !m.alt && !m.shift
    } else {
        m.control && m.shift && !m.alt && !m.platform
    };
    if !chord {
        return None;
    }
    match key {
        "t" => Some(TerminalShortcut::NewTab),
        "w" => Some(TerminalShortcut::CloseTab),
        "k" => Some(TerminalShortcut::Clear),
        "c" => Some(TerminalShortcut::Copy),
        "v" => Some(TerminalShortcut::Paste),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(source: &str) -> gpui::Keystroke {
        gpui::Keystroke::parse(source).unwrap_or_else(|_| gpui::Keystroke {
            modifiers: gpui::Modifiers::default(),
            key: String::new(),
            key_char: None,
        })
    }

    #[test]
    fn macos_shortcuts_use_cmd() {
        assert_eq!(
            terminal_shortcut(&stroke("alt-f12"), true),
            Some(TerminalShortcut::Hide)
        );
        assert_eq!(
            terminal_shortcut(&stroke("cmd-t"), true),
            Some(TerminalShortcut::NewTab)
        );
        assert_eq!(
            terminal_shortcut(&stroke("cmd-k"), true),
            Some(TerminalShortcut::Clear)
        );
        assert_eq!(
            terminal_shortcut(&stroke("cmd-w"), true),
            Some(TerminalShortcut::CloseTab)
        );
        assert_eq!(
            terminal_shortcut(&stroke("cmd-v"), true),
            Some(TerminalShortcut::Paste)
        );
        assert_eq!(
            terminal_shortcut(&stroke("ctrl-c"), true),
            None,
            "Ctrl+C reaches the shell"
        );
        assert_eq!(terminal_shortcut(&stroke("ctrl-w"), true), None);
    }

    #[test]
    fn linux_shortcuts_use_ctrl_shift() {
        assert_eq!(
            terminal_shortcut(&stroke("ctrl-shift-t"), false),
            Some(TerminalShortcut::NewTab)
        );
        assert_eq!(
            terminal_shortcut(&stroke("ctrl-shift-c"), false),
            Some(TerminalShortcut::Copy)
        );
        assert_eq!(terminal_shortcut(&stroke("ctrl-t"), false), None);
        assert_eq!(terminal_shortcut(&stroke("ctrl-c"), false), None);
    }
}
