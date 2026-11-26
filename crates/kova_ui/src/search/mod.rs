//! *Search Everywhere* modal (`Shift+Shift`, `View → Search Everywhere`, `KOVA-050`).
//!
//! Splits headless fuzzy ranking ([`engine`]) and keyboard/state management
//! ([`modal::SearchEverywhereState`]) from the GPUI overlay renderer
//! ([`modal::SearchEverywhereModal`]).

pub mod engine;
pub mod modal;

use std::path::PathBuf;

pub use engine::{
    MAX_SEARCH_HITS, SearchEngine, char_indices_to_byte_ranges, parse_query_and_line,
};
pub use modal::{SearchEverywhereModal, SearchEverywhereState, SearchKeyOutcome};

use crate::keymap::ShellAction;

/// Scope tabs across the top of *Search Everywhere*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchScope {
    /// Unified results across classes, files, symbols, and actions.
    #[default]
    All,
    /// Project classes, interfaces, enums, records, and annotations.
    Classes,
    /// Project and open files.
    Files,
    /// Symbols in the active document outline.
    Symbols,
    /// Executable IDE actions from the built-in action registry.
    Actions,
}

impl SearchScope {
    /// Ordered tabs shown in the modal header.
    pub const ALL: [Self; 5] = [
        Self::All,
        Self::Classes,
        Self::Files,
        Self::Symbols,
        Self::Actions,
    ];

    /// Human-readable tab caption.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Classes => "Classes",
            Self::Files => "Files",
            Self::Symbols => "Symbols",
            Self::Actions => "Actions",
        }
    }

    /// Advances to the next tab (`Tab`).
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::All => Self::Classes,
            Self::Classes => Self::Files,
            Self::Files => Self::Symbols,
            Self::Symbols => Self::Actions,
            Self::Actions => Self::All,
        }
    }

    /// Moves to the previous tab (`Shift+Tab`).
    #[must_use]
    pub const fn prev(self) -> Self {
        match self {
            Self::All => Self::Actions,
            Self::Classes => Self::All,
            Self::Files => Self::Classes,
            Self::Symbols => Self::Files,
            Self::Actions => Self::Symbols,
        }
    }

    /// Returns `true` when `kind` is visible in this scope.
    #[must_use]
    pub const fn includes(self, kind: SearchEntryKind) -> bool {
        match self {
            Self::All => true,
            Self::Classes => matches!(kind, SearchEntryKind::Class),
            Self::Files => matches!(kind, SearchEntryKind::File),
            Self::Symbols => matches!(kind, SearchEntryKind::Symbol),
            Self::Actions => matches!(kind, SearchEntryKind::Action),
        }
    }
}

/// Kind of candidate indexed by *Search Everywhere*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SearchEntryKind {
    /// Class, interface, enum, record, or annotation.
    Class,
    /// Project or open file.
    File,
    /// Symbol in the active document (`Structure`).
    Symbol,
    /// Executable IDE action.
    Action,
}

impl SearchEntryKind {
    /// Short one-glyph badge shown on the left of a result row.
    #[must_use]
    pub const fn default_icon(self) -> &'static str {
        match self {
            Self::Class => "C",
            Self::File => "F",
            Self::Symbol => "S",
            Self::Action => "A",
        }
    }
}

/// Action executed when the user activates a *Search Everywhere* hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchTarget {
    /// Open a file (optionally jumping to a 1-based line).
    File {
        /// Project-relative (or absolute external) path.
        path: PathBuf,
        /// Optional 1-based target line.
        line: Option<usize>,
    },
    /// Jump to a symbol in the active or target file.
    Symbol {
        /// Optional document key when the symbol comes from a file path.
        path: Option<PathBuf>,
        /// 1-based line number.
        line: usize,
        /// 1-based column number.
        col: usize,
    },
    /// Run a shell command with restored workspace focus.
    Action(ShellAction),
}

/// Candidate item fed into [`SearchEngine`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchEntry {
    /// Entry category (`Class`, `File`, `Symbol`, `Action`).
    pub kind: SearchEntryKind,
    /// Primary label displayed and highlighted in the result row.
    pub title: String,
    /// Secondary metadata (parent directory, kind + line, or action category).
    pub subtitle: String,
    /// Full string matched for fuzzy search (e.g. relative file path for `Files`).
    pub match_text: String,
    /// Short one-glyph icon badge.
    pub icon: &'static str,
    /// Optional keyboard shortcut hint (e.g. `"cmd+,"`).
    pub shortcut_hint: Option<String>,
    /// Target executed on `Enter` or click.
    pub target: SearchTarget,
}

/// Ranked result returned by [`SearchEngine::search`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Matched candidate entry (with `:line` suffix applied when applicable).
    pub entry: SearchEntry,
    /// Ranking score (higher is better).
    pub score: u32,
    /// Sorted 0-based Unicode character indices in `entry.title` matched by the query.
    pub matched_char_indices: Vec<u32>,
    /// Optional 1-based line parsed from a `:line` query suffix.
    pub target_line: Option<usize>,
}
