//! Headless code navigation primitives: history (`Back`/`Forward`) and fast Level-1
//! workspace symbol resolution for Java, Kotlin, and Rust.
//!
//! This module lives in `kova_core` with zero UI, GPU, or async-runtime dependencies.

pub mod resolver;

use std::path::PathBuf;

pub use resolver::{
    ExternalTargetCandidates, ImportEntry, ResolvedLocation, SourceLanguageKind, SymbolQuery,
    extract_symbol_query, navigable_identifier_at_byte, resolve_external_fqcn_candidates,
    resolve_local_definition,
};

/// Default maximum number of entries kept in [`NavigationHistory`] `back` / `forward` stacks.
pub const DEFAULT_MAX_NAV_HISTORY: usize = 128;

/// One 1-based cursor location in a source file or decompiled document.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NavLocation {
    /// Document path (project-relative for workspace files, or absolute for external/cache files).
    pub path: PathBuf,
    /// 1-based line number.
    pub line: usize,
    /// 1-based character column.
    pub column: usize,
}

impl NavLocation {
    /// Creates a new [`NavLocation`] with 1-based `line` and `column` (clamped to `>= 1`).
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, line: usize, column: usize) -> Self {
        Self {
            path: path.into(),
            line: line.max(1),
            column: column.max(1),
        }
    }
}

/// Bounded Back/Forward cursor navigation history (`Cmd+[` / `Cmd+]`, `Ctrl+Alt+Left/Right`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavigationHistory {
    max_entries: usize,
    back: Vec<NavLocation>,
    forward: Vec<NavLocation>,
}

impl Default for NavigationHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl NavigationHistory {
    /// Creates an empty [`NavigationHistory`] with capacity [`DEFAULT_MAX_NAV_HISTORY`].
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_entries: DEFAULT_MAX_NAV_HISTORY,
            back: Vec::new(),
            forward: Vec::new(),
        }
    }

    /// Creates an empty [`NavigationHistory`] retaining at most `max_entries` per stack.
    #[must_use]
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            back: Vec::new(),
            forward: Vec::new(),
        }
    }

    /// Records `origin` before jumping to a new destination, clearing the `forward` stack.
    ///
    /// Consecutive identical locations on top of `back` are deduplicated.
    pub fn push(&mut self, origin: NavLocation) {
        self.forward.clear();
        if self.back.last() == Some(&origin) {
            return;
        }
        self.back.push(origin);
        trim_oldest(&mut self.back, self.max_entries);
    }

    /// Steps backward from `current`, pushing `current` onto `forward`.
    pub fn go_back(&mut self, current: NavLocation) -> Option<NavLocation> {
        let target = self.back.pop()?;
        if self.forward.last() != Some(&current) {
            self.forward.push(current);
            trim_oldest(&mut self.forward, self.max_entries);
        }
        Some(target)
    }

    /// Steps forward from `current`, pushing `current` onto `back`.
    pub fn go_forward(&mut self, current: NavLocation) -> Option<NavLocation> {
        let target = self.forward.pop()?;
        if self.back.last() != Some(&current) {
            self.back.push(current);
            trim_oldest(&mut self.back, self.max_entries);
        }
        Some(target)
    }

    /// Clears both `back` and `forward` stacks (e.g. on project switch).
    pub fn clear(&mut self) {
        self.back.clear();
        self.forward.clear();
    }

    /// Returns `true` when at least one backward location is available.
    #[must_use]
    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Returns `true` when at least one forward location is available.
    #[must_use]
    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    /// Number of entries in the `back` stack.
    #[must_use]
    pub fn back_len(&self) -> usize {
        self.back.len()
    }

    /// Number of entries in the `forward` stack.
    #[must_use]
    pub fn forward_len(&self) -> usize {
        self.forward.len()
    }
}

fn trim_oldest(stack: &mut Vec<NavLocation>, max_entries: usize) {
    if stack.len() > max_entries {
        let excess = stack.len() - max_entries;
        stack.drain(..excess);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_push_back_forward_and_reset_forward() {
        let mut hist = NavigationHistory::new();
        assert!(!hist.can_go_back());
        assert!(!hist.can_go_forward());

        let a = NavLocation::new("src/A.java", 10, 5);
        let b = NavLocation::new("src/B.java", 20, 3);
        let c = NavLocation::new("src/C.java", 30, 1);

        // Push origin A before jumping to B, then B before jumping to C.
        hist.push(a.clone());
        hist.push(a.clone()); // consecutive duplicate ignored
        assert_eq!(hist.back_len(), 1);
        hist.push(b.clone());
        assert_eq!(hist.back_len(), 2);

        // Step back from C -> B, then B -> A.
        assert_eq!(hist.go_back(c.clone()), Some(b.clone()));
        assert!(hist.can_go_forward());
        assert_eq!(hist.go_back(b.clone()), Some(a.clone()));
        assert!(!hist.can_go_back());
        assert_eq!(hist.forward_len(), 2);

        // Step forward from A -> B.
        assert_eq!(hist.go_forward(a.clone()), Some(b.clone()));
        assert_eq!(hist.back_len(), 1);
        assert_eq!(hist.forward_len(), 1);

        // New jump from B clears forward stack.
        hist.push(b.clone());
        assert!(!hist.can_go_forward());
        assert_eq!(hist.back_len(), 2);

        hist.clear();
        assert!(!hist.can_go_back());
        assert!(!hist.can_go_forward());
    }

    #[test]
    fn history_enforces_capacity_limit() {
        let mut hist = NavigationHistory::with_capacity(3);
        for line in 1..=5 {
            hist.push(NavLocation::new("src/A.java", line, 1));
        }
        assert_eq!(hist.back_len(), 3);
        assert_eq!(
            hist.go_back(NavLocation::new("src/A.java", 99, 1)),
            Some(NavLocation::new("src/A.java", 5, 1))
        );
        assert_eq!(
            hist.go_back(NavLocation::new("src/A.java", 5, 1)),
            Some(NavLocation::new("src/A.java", 4, 1))
        );
        assert_eq!(
            hist.go_back(NavLocation::new("src/A.java", 4, 1)),
            Some(NavLocation::new("src/A.java", 3, 1))
        );
        assert_eq!(hist.go_back(NavLocation::new("src/A.java", 3, 1)), None);
    }
}
