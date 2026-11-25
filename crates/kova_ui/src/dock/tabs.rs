//! Editor tab group model (`TabGroupModel`) with MRU history and dirty badges.

use std::path::{Path, PathBuf};

use crate::dock::split::PaneId;

/// Number of characters after which tab titles are compacted when many tabs are open.
pub const COMPACT_TITLE_MAX_CHARS: usize = 14;
/// Tab count above which titles are compacted (`> 10` open files).
pub const COMPACT_TITLES_THRESHOLD: usize = 10;

/// Unique identifier for an open editor tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

/// Metadata for a single tab in an editor pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabItem {
    /// Unique tab identifier.
    pub id: TabId,
    /// Display title (usually file name).
    pub title: String,
    /// Optional backing file path.
    pub path: Option<PathBuf>,
    /// Whether the document has unsaved modifications (`●`).
    pub is_dirty: bool,
    /// Language label shown in the status bar (e.g. `"Java"`, `"TOML"`).
    pub language_label: String,
}

impl TabItem {
    /// Returns the title shown on the tab, compacted with `…` when `compact` is set.
    #[must_use]
    pub fn display_title(&self, compact: bool) -> String {
        if !compact || self.title.chars().count() <= COMPACT_TITLE_MAX_CHARS {
            return self.title.clone();
        }
        let mut out: String = self
            .title
            .chars()
            .take(COMPACT_TITLE_MAX_CHARS.saturating_sub(1))
            .collect();
        out.push('…');
        out
    }
}

/// Manages open tabs, active selection and MRU history for a single editor [`PaneId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabGroupModel {
    pane_id: PaneId,
    tabs: Vec<TabItem>,
    active_tab: Option<TabId>,
    /// Most-recently-used order, oldest first; the last entry is the active tab.
    mru: Vec<TabId>,
    next_tab_id: u64,
}

impl TabGroupModel {
    /// Creates an empty [`TabGroupModel`] for `pane_id`.
    #[must_use]
    pub const fn new(pane_id: PaneId) -> Self {
        Self {
            pane_id,
            tabs: Vec::new(),
            active_tab: None,
            mru: Vec::new(),
            next_tab_id: 1,
        }
    }

    /// Returns the owning editor pane.
    #[must_use]
    pub const fn pane_id(&self) -> PaneId {
        self.pane_id
    }

    /// Returns the ordered list of open tabs.
    #[must_use]
    pub fn tabs(&self) -> &[TabItem] {
        &self.tabs
    }

    /// Returns the id of the active tab, if any.
    #[must_use]
    pub const fn active_tab_id(&self) -> Option<TabId> {
        self.active_tab
    }

    /// Returns `true` when titles should be compacted (more than [`COMPACT_TITLES_THRESHOLD`] tabs).
    #[must_use]
    pub fn compact_titles(&self) -> bool {
        self.tabs.len() > COMPACT_TITLES_THRESHOLD
    }

    /// Returns the tab showing `path`, if one is open in this group.
    #[must_use]
    pub fn find_by_path(&self, path: &Path) -> Option<&TabItem> {
        self.tabs
            .iter()
            .find(|tab| tab.path.as_deref() == Some(path))
    }

    /// Opens a tab for `path` (or focuses the existing tab if already open) and returns its [`TabId`].
    pub fn open_or_focus(
        &mut self,
        path: Option<&Path>,
        title: impl Into<String>,
        language_label: impl Into<String>,
    ) -> TabId {
        if let Some(target_path) = path
            && let Some(existing) = self.find_by_path(target_path)
        {
            let id = existing.id;
            self.activate(id);
            return id;
        }

        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        self.tabs.push(TabItem {
            id,
            title: title.into(),
            path: path.map(Path::to_path_buf),
            is_dirty: false,
            language_label: language_label.into(),
        });
        self.activate(id);
        id
    }

    /// Closes `id`; if it was active, re-activates the most recently used remaining tab.
    pub fn close_tab(&mut self, id: TabId) -> bool {
        let Some(pos) = self.tabs.iter().position(|tab| tab.id == id) else {
            return false;
        };
        self.tabs.remove(pos);
        self.mru.retain(|tab| *tab != id);

        if self.active_tab == Some(id) {
            self.active_tab = self.mru.last().copied();
        }
        true
    }

    /// Selects `id` as the active tab if it belongs to this group.
    pub fn select_tab(&mut self, id: TabId) -> bool {
        if self.tabs.iter().any(|tab| tab.id == id) {
            self.activate(id);
            true
        } else {
            false
        }
    }

    /// Cycles selection to the next tab (wrapping around).
    pub fn next_tab(&mut self) {
        self.cycle(true);
    }

    /// Cycles selection to the previous tab (wrapping around).
    pub fn prev_tab(&mut self) {
        self.cycle(false);
    }

    fn cycle(&mut self, forward: bool) {
        let len = self.tabs.len();
        if len <= 1 {
            return;
        }
        let Some(pos) = self
            .active_tab
            .and_then(|active| self.tabs.iter().position(|tab| tab.id == active))
        else {
            return;
        };
        let target = if forward {
            (pos + 1) % len
        } else {
            (pos + len - 1) % len
        };
        if let Some(tab) = self.tabs.get(target) {
            let id = tab.id;
            self.activate(id);
        }
    }

    fn activate(&mut self, id: TabId) {
        self.mru.retain(|tab| *tab != id);
        self.mru.push(id);
        self.active_tab = Some(id);
    }

    /// Updates the dirty indicator (`●`) for `id`.
    pub fn set_dirty(&mut self, id: TabId, dirty: bool) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            tab.is_dirty = dirty;
        }
    }

    /// Returns the currently active [`TabItem`], if any.
    #[must_use]
    pub fn active_tab(&self) -> Option<&TabItem> {
        let active = self.active_tab?;
        self.tabs.iter().find(|tab| tab.id == active)
    }

    /// Returns the number of open tabs in this group.
    #[must_use]
    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_focus_cycle_and_close_tabs() {
        let mut group = TabGroupModel::new(PaneId(1));
        let t1 = group.open_or_focus(Some(Path::new("OrderApp.java")), "OrderApp.java", "Java");
        let t2 = group.open_or_focus(Some(Path::new("Order.java")), "Order.java", "Java");
        assert_eq!(group.tab_count(), 2);
        assert_eq!(group.active_tab_id(), Some(t2));

        // Opening the same path focuses the existing tab instead of duplicating.
        let t1_again =
            group.open_or_focus(Some(Path::new("OrderApp.java")), "OrderApp.java", "Java");
        assert_eq!(t1_again, t1);
        assert_eq!(group.tab_count(), 2);
        assert_eq!(group.active_tab_id(), Some(t1));

        group.next_tab();
        assert_eq!(group.active_tab_id(), Some(t2));
        group.prev_tab();
        assert_eq!(group.active_tab_id(), Some(t1));

        group.set_dirty(t1, true);
        assert!(group.active_tab().is_some_and(|t| t.is_dirty));

        assert!(group.close_tab(t1));
        assert_eq!(group.active_tab_id(), Some(t2));
        assert!(group.close_tab(t2));
        assert_eq!(group.active_tab_id(), None);
        assert!(!group.close_tab(t2));
    }

    #[test]
    fn closing_active_tab_returns_to_most_recently_used() {
        let mut group = TabGroupModel::new(PaneId(1));
        let a = group.open_or_focus(Some(Path::new("A.java")), "A.java", "Java");
        let b = group.open_or_focus(Some(Path::new("B.java")), "B.java", "Java");
        let c = group.open_or_focus(Some(Path::new("C.java")), "C.java", "Java");
        // MRU: b, c, a  (a is active, b was used before c? -> select order a, c)
        assert!(group.select_tab(a));
        assert!(group.select_tab(c));
        assert!(group.select_tab(a));
        // Closing `a` must return to `c` (MRU), not the adjacent `b`.
        assert!(group.close_tab(a));
        assert_eq!(group.active_tab_id(), Some(c));
        // Closing an inactive tab keeps the active tab.
        assert!(group.close_tab(b));
        assert_eq!(group.active_tab_id(), Some(c));
    }

    #[test]
    fn compacts_titles_beyond_ten_tabs() {
        let mut group = TabGroupModel::new(PaneId(1));
        for i in 0..11 {
            let name = format!("VeryLongControllerName{i}.java");
            group.open_or_focus(Some(Path::new(&name)), name.clone(), "Java");
        }
        assert!(group.compact_titles());
        let title = group
            .active_tab()
            .map(|t| t.display_title(group.compact_titles()))
            .unwrap_or_default();
        assert_eq!(title.chars().count(), COMPACT_TITLE_MAX_CHARS);
        assert!(title.ends_with('…'));
    }
}
