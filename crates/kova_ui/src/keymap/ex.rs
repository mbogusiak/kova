//! Vim Ex command line (`:` / `/` / `?`) shown in the status bar and its command parser
//! (`ExCommandParser`, `KOVA-026` FR-4.3).
//!
//! `kova_core`'s Vim engine has no command-line mode, so the line editor and the parser live
//! here and map Ex commands onto workspace shell actions. Both are headless.

use crate::dock::SplitAxis;

/// Parsed Ex command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExCommand {
    /// `:w` — save the active document.
    Write,
    /// `:q` — close the active tab (refused while the document is dirty).
    Quit,
    /// `:q!` — close the active tab, discarding the warning.
    ForceQuit,
    /// `:wq` / `:x` — save, then close the active tab.
    WriteQuit,
    /// `:sp` (`Vertical`, panes stacked) / `:vsp` (`Horizontal`, side by side).
    Split(SplitAxis),
    /// `:<n>` — jump to 1-based line `n`.
    GoToLine(usize),
    /// `:<n>:<c>` — jump to 1-based line `n` and 1-based column `c`.
    GoToLineColumn(usize, usize),
    /// `:noh` — clear search highlighting.
    NoHighlight,
    /// `:only` — Focus Mode (hide every dock).
    Only,
}

/// Why an Ex command line could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExParseError {
    /// Nothing but whitespace after `:`.
    Empty,
    /// Unknown or unsupported command (`E492: Not an editor command`).
    NotAnEditorCommand(String),
}

impl std::fmt::Display for ExParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("E471: Argument required"),
            Self::NotAnEditorCommand(cmd) => write!(f, "E492: Not an editor command: {cmd}"),
        }
    }
}

impl std::error::Error for ExParseError {}

/// Parses the text typed after `:` (without the prefix).
///
/// # Errors
///
/// Returns [`ExParseError::Empty`] for a blank line and
/// [`ExParseError::NotAnEditorCommand`] for anything this shell does not support.
pub fn parse_ex_command(input: &str) -> Result<ExCommand, ExParseError> {
    let cmd = input.trim();
    if cmd.is_empty() {
        return Err(ExParseError::Empty);
    }
    if cmd.bytes().all(|b| b.is_ascii_digit()) {
        return cmd
            .parse::<usize>()
            .map(|line| ExCommand::GoToLine(line.max(1)))
            .map_err(|_| ExParseError::NotAnEditorCommand(cmd.to_owned()));
    }
    if let Some((line_str, col_str)) = cmd.split_once(':')
        && !line_str.is_empty()
        && !col_str.is_empty()
        && line_str.bytes().all(|b| b.is_ascii_digit())
        && col_str.bytes().all(|b| b.is_ascii_digit())
    {
        return match (line_str.parse::<usize>(), col_str.parse::<usize>()) {
            (Ok(line), Ok(col)) => Ok(ExCommand::GoToLineColumn(line.max(1), col.max(1))),
            _ => Err(ExParseError::NotAnEditorCommand(cmd.to_owned())),
        };
    }
    Ok(match cmd {
        "w" | "write" => ExCommand::Write,
        "q" | "quit" | "clo" | "close" => ExCommand::Quit,
        "q!" | "quit!" => ExCommand::ForceQuit,
        "wq" | "x" | "xit" | "exit" => ExCommand::WriteQuit,
        "sp" | "split" => ExCommand::Split(SplitAxis::Vertical),
        "vs" | "vsp" | "vsplit" => ExCommand::Split(SplitAxis::Horizontal),
        "noh" | "nohl" | "nohlsearch" => ExCommand::NoHighlight,
        "on" | "only" => ExCommand::Only,
        other => return Err(ExParseError::NotAnEditorCommand(other.to_owned())),
    })
}

/// Edit result of one key in the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExLineEvent {
    /// The text or caret changed; keep the line open.
    Edited,
    /// `Enter`: the line closed and returned its prefix and text.
    Submitted {
        /// `:` / `/` / `?`.
        prefix: char,
        /// Text typed after the prefix.
        text: String,
    },
    /// `Escape`, or `Backspace` on an empty line: the line closed without running anything.
    Cancelled,
    /// The key has no meaning in the command line (ignored but consumed).
    Ignored,
}

/// One-line editor for the Ex / search prompt rendered in the status bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExCommandLine {
    prefix: char,
    text: String,
    /// Caret position in characters.
    cursor_col: usize,
}

impl ExCommandLine {
    /// Opens an empty line with `prefix` (`:` / `/` / `?`).
    #[must_use]
    pub const fn new(prefix: char) -> Self {
        Self {
            prefix,
            text: String::new(),
            cursor_col: 0,
        }
    }

    /// Prompt prefix.
    #[must_use]
    pub const fn prefix(&self) -> char {
        self.prefix
    }

    /// Text typed after the prefix.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Caret column in characters (`0 ..= text.chars().count()`).
    #[must_use]
    pub const fn cursor_col(&self) -> usize {
        self.cursor_col
    }

    /// Text before and after the caret, for rendering the caret between them.
    #[must_use]
    pub fn split_at_cursor(&self) -> (&str, &str) {
        self.text.split_at(self.byte_at(self.cursor_col))
    }

    fn byte_at(&self, col: usize) -> usize {
        self.text
            .char_indices()
            .nth(col)
            .map_or(self.text.len(), |(i, _)| i)
    }

    /// Inserts typed text (IME commits included) at the caret.
    pub fn insert(&mut self, text: &str) -> ExLineEvent {
        let printable: String = text.chars().filter(|c| !c.is_control()).collect();
        if printable.is_empty() {
            return ExLineEvent::Ignored;
        }
        let at = self.byte_at(self.cursor_col);
        self.text.insert_str(at, &printable);
        self.cursor_col += printable.chars().count();
        ExLineEvent::Edited
    }

    /// Handles a named key (`enter`, `escape`, `backspace`, `delete`, `left`, `right`,
    /// `home`, `end`) as reported by GPUI.
    pub fn handle_named_key(&mut self, key: &str) -> ExLineEvent {
        let len = self.text.chars().count();
        match key {
            "enter" => ExLineEvent::Submitted {
                prefix: self.prefix,
                text: std::mem::take(&mut self.text),
            },
            "escape" => ExLineEvent::Cancelled,
            "backspace" if self.text.is_empty() => ExLineEvent::Cancelled,
            "backspace" if self.cursor_col > 0 => {
                let start = self.byte_at(self.cursor_col - 1);
                let end = self.byte_at(self.cursor_col);
                self.text.replace_range(start..end, "");
                self.cursor_col -= 1;
                ExLineEvent::Edited
            }
            "delete" if self.cursor_col < len => {
                let start = self.byte_at(self.cursor_col);
                let end = self.byte_at(self.cursor_col + 1);
                self.text.replace_range(start..end, "");
                ExLineEvent::Edited
            }
            "left" if self.cursor_col > 0 => {
                self.cursor_col -= 1;
                ExLineEvent::Edited
            }
            "right" if self.cursor_col < len => {
                self.cursor_col += 1;
                ExLineEvent::Edited
            }
            "home" => {
                self.cursor_col = 0;
                ExLineEvent::Edited
            }
            "end" => {
                self.cursor_col = len;
                ExLineEvent::Edited
            }
            _ => ExLineEvent::Ignored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_ex_commands() {
        assert_eq!(parse_ex_command("w"), Ok(ExCommand::Write));
        assert_eq!(parse_ex_command(" q "), Ok(ExCommand::Quit));
        assert_eq!(parse_ex_command("q!"), Ok(ExCommand::ForceQuit));
        assert_eq!(parse_ex_command("wq"), Ok(ExCommand::WriteQuit));
        assert_eq!(parse_ex_command("x"), Ok(ExCommand::WriteQuit));
        assert_eq!(
            parse_ex_command("sp"),
            Ok(ExCommand::Split(SplitAxis::Vertical))
        );
        assert_eq!(
            parse_ex_command("vsp"),
            Ok(ExCommand::Split(SplitAxis::Horizontal))
        );
        assert_eq!(parse_ex_command("42"), Ok(ExCommand::GoToLine(42)));
        assert_eq!(parse_ex_command("0"), Ok(ExCommand::GoToLine(1)));
        assert_eq!(
            parse_ex_command("42:10"),
            Ok(ExCommand::GoToLineColumn(42, 10))
        );
        assert_eq!(parse_ex_command("0:0"), Ok(ExCommand::GoToLineColumn(1, 1)));
        assert!(matches!(
            parse_ex_command("42:"),
            Err(ExParseError::NotAnEditorCommand(_))
        ));
        assert!(matches!(
            parse_ex_command(":10"),
            Err(ExParseError::NotAnEditorCommand(_))
        ));
        assert_eq!(parse_ex_command("noh"), Ok(ExCommand::NoHighlight));
        assert_eq!(parse_ex_command("only"), Ok(ExCommand::Only));
        assert_eq!(parse_ex_command("  "), Err(ExParseError::Empty));
        let err = parse_ex_command("frobnicate").unwrap_err();
        assert_eq!(err.to_string(), "E492: Not an editor command: frobnicate");
        assert!(matches!(
            parse_ex_command("99999999999999999999999"),
            Err(ExParseError::NotAnEditorCommand(_))
        ));
    }

    #[test]
    fn command_line_edits_unicode_and_submits() {
        let mut line = ExCommandLine::new(':');
        assert_eq!(line.prefix(), ':');
        assert_eq!(line.insert("vś"), ExLineEvent::Edited);
        assert_eq!(line.handle_named_key("left"), ExLineEvent::Edited);
        assert_eq!(line.insert("p"), ExLineEvent::Edited);
        assert_eq!(line.text(), "vpś");
        assert_eq!(line.split_at_cursor(), ("vp", "ś"));
        assert_eq!(line.handle_named_key("delete"), ExLineEvent::Edited);
        assert_eq!(line.text(), "vp");
        assert_eq!(line.handle_named_key("home"), ExLineEvent::Edited);
        assert_eq!(line.insert("s"), ExLineEvent::Edited);
        assert_eq!(line.handle_named_key("end"), ExLineEvent::Edited);
        assert_eq!(line.cursor_col(), 3);
        assert_eq!(line.handle_named_key("backspace"), ExLineEvent::Edited);
        assert_eq!(line.insert("\u{7}"), ExLineEvent::Ignored);
        assert_eq!(line.handle_named_key("f5"), ExLineEvent::Ignored);
        assert_eq!(
            line.handle_named_key("enter"),
            ExLineEvent::Submitted {
                prefix: ':',
                text: "sv".to_owned()
            }
        );
    }

    #[test]
    fn command_line_cancels_on_escape_and_empty_backspace() {
        let mut line = ExCommandLine::new('/');
        assert_eq!(line.handle_named_key("backspace"), ExLineEvent::Cancelled);
        line.insert("x");
        assert_eq!(line.handle_named_key("escape"), ExLineEvent::Cancelled);
    }
}
