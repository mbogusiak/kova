//! Headless *Recent Projects* store backing `File → Recent Projects` (`KOVA-049`).
//!
//! The list lives in `~/.cache/kova/recent_workspaces.json` (the [`RecentWorkspaces`] schema
//! shared with the session layer), so the menu and the workspace session never disagree.
//! The store performs plain blocking file I/O and has no GUI dependency: hosts call it from a
//! background thread and feed [`RecentProjectsStore::entries`] into their menu builder.

use std::path::{Path, PathBuf};

use kova_config::KovaPaths;

use super::session::{
    RecentWorkspaceEntry, RecentWorkspaces, load_recent_workspaces, save_recent_workspaces,
};
use super::{ProjectError, ProjectMetadata};

/// Maximum number of projects listed in the `Recent Projects` submenu.
pub const MAX_RECENT_PROJECTS_IN_MENU: usize = 10;

/// One entry of the `Recent Projects` submenu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentProject {
    /// Display name (final path component of `root`).
    pub name: String,
    /// Canonical project root.
    pub root: PathBuf,
}

impl From<&RecentWorkspaceEntry> for RecentProject {
    fn from(entry: &RecentWorkspaceEntry) -> Self {
        Self {
            name: entry.name.clone(),
            root: entry.root.clone(),
        }
    }
}

/// Persistent most-recent-first list of opened projects under the Kova cache directory.
#[derive(Debug, Clone)]
pub struct RecentProjectsStore {
    paths: KovaPaths,
    recent: RecentWorkspaces,
}

impl RecentProjectsStore {
    /// Loads the store from `paths.cache_dir()`.
    ///
    /// A missing, corrupted or unsupported file yields an empty list: the next
    /// [`Self::record`] rewrites it cleanly.
    #[must_use]
    pub fn load(paths: KovaPaths) -> Self {
        let recent = load_recent_workspaces(&paths).unwrap_or_default();
        Self { paths, recent }
    }

    /// Path of the backing JSON file.
    #[must_use]
    pub fn file_path(&self) -> PathBuf {
        self.paths.cache_dir().join("recent_workspaces.json")
    }

    /// All remembered projects, most recent first.
    #[must_use]
    pub fn entries(&self) -> Vec<RecentProject> {
        self.recent
            .entries
            .iter()
            .map(RecentProject::from)
            .collect()
    }

    /// Projects for the menu: most recent first, at most `limit`, directories that no longer
    /// exist are skipped.
    #[must_use]
    pub fn menu_entries(&self, limit: usize) -> Vec<RecentProject> {
        self.recent
            .entries
            .iter()
            .filter(|entry| entry.root.is_dir())
            .take(limit)
            .map(RecentProject::from)
            .collect()
    }

    /// Records `root` as the most recently opened project and persists the list atomically.
    ///
    /// Returns the canonical metadata of the recorded project.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError::NotFound`] / [`ProjectError::NotADirectory`] for an invalid
    /// `root`, or [`ProjectError::Io`] when the list cannot be written.
    pub fn record(&mut self, root: &Path) -> Result<ProjectMetadata, ProjectError> {
        let metadata = ProjectMetadata::inspect(root, &self.paths)?;
        self.recent
            .upsert(RecentWorkspaceEntry::from_metadata(&metadata));
        save_recent_workspaces(&self.paths, &self.recent)?;
        Ok(metadata)
    }

    /// Forgets `root` (e.g. a project whose directory vanished) and persists the list.
    ///
    /// Returns `true` when an entry was removed.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError::Io`] when the list cannot be written.
    pub fn remove(&mut self, root: &Path) -> Result<bool, ProjectError> {
        let before = self.recent.entries.len();
        self.recent.entries.retain(|entry| entry.root != root);
        if self.recent.entries.len() == before {
            return Ok(false);
        }
        save_recent_workspaces(&self.paths, &self.recent)?;
        Ok(true)
    }

    /// Empties the list (`Recent Projects → Clear Recent Projects`) and persists it.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError::Io`] when the list cannot be written.
    pub fn clear(&mut self) -> Result<(), ProjectError> {
        self.recent.entries.clear();
        save_recent_workspaces(&self.paths, &self.recent)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "test harness: failures must abort the test with context"
    )]

    use std::fs;

    use kova_config::KovaPaths;
    use tempfile::TempDir;

    use super::{MAX_RECENT_PROJECTS_IN_MENU, RecentProjectsStore};
    use crate::project::session::MAX_RECENT_WORKSPACES;

    fn isolated_paths(temp: &TempDir) -> KovaPaths {
        KovaPaths::from_roots(
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("cache"),
        )
        .expect("isolated temp paths are absolute")
    }

    fn project(temp: &TempDir, name: &str) -> std::path::PathBuf {
        let dir = temp.path().join("projects").join(name);
        fs::create_dir_all(&dir).expect("project dir");
        dir.canonicalize().expect("canonical")
    }

    #[test]
    fn empty_when_file_is_missing() {
        let temp = TempDir::new().expect("tempdir");
        let store = RecentProjectsStore::load(isolated_paths(&temp));
        assert_eq!(store.entries().len(), 0);
        assert!(!store.file_path().exists());
    }

    #[test]
    fn record_persists_and_reloads_most_recent_first() {
        let temp = TempDir::new().expect("tempdir");
        let alpha = project(&temp, "alpha");
        let beta = project(&temp, "beta");

        let mut store = RecentProjectsStore::load(isolated_paths(&temp));
        let meta = store.record(&alpha).expect("record alpha");
        assert_eq!(meta.name, "alpha");
        store.record(&beta).expect("record beta");
        assert!(store.file_path().is_file());

        let reloaded = RecentProjectsStore::load(isolated_paths(&temp));
        let names: Vec<_> = reloaded.entries().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["beta", "alpha"]);
        assert_eq!(reloaded.entries()[1].root, alpha);
    }

    #[test]
    fn reopening_promotes_without_duplicates() {
        let temp = TempDir::new().expect("tempdir");
        let alpha = project(&temp, "alpha");
        let beta = project(&temp, "beta");
        let mut store = RecentProjectsStore::load(isolated_paths(&temp));
        store.record(&alpha).expect("alpha");
        store.record(&beta).expect("beta");
        store.record(&alpha).expect("alpha again");

        let names: Vec<_> = store.entries().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["alpha", "beta"]);
    }

    #[test]
    fn list_is_capped() {
        let temp = TempDir::new().expect("tempdir");
        let mut store = RecentProjectsStore::load(isolated_paths(&temp));
        for i in 0..MAX_RECENT_WORKSPACES + 3 {
            store
                .record(&project(&temp, &format!("p{i}")))
                .expect("record");
        }
        assert_eq!(store.entries().len(), MAX_RECENT_WORKSPACES);
        assert_eq!(
            store.menu_entries(MAX_RECENT_PROJECTS_IN_MENU).len(),
            MAX_RECENT_PROJECTS_IN_MENU
        );
        assert_eq!(
            store.entries()[0].name,
            format!("p{}", MAX_RECENT_WORKSPACES + 2)
        );
    }

    #[test]
    fn menu_skips_deleted_directories_and_remove_forgets_them() {
        let temp = TempDir::new().expect("tempdir");
        let alpha = project(&temp, "alpha");
        let gone = project(&temp, "gone");
        let mut store = RecentProjectsStore::load(isolated_paths(&temp));
        store.record(&alpha).expect("alpha");
        store.record(&gone).expect("gone");
        fs::remove_dir_all(&gone).expect("delete project");

        let menu: Vec<_> = store.menu_entries(10).into_iter().map(|p| p.name).collect();
        assert_eq!(menu, ["alpha"]);
        assert_eq!(store.entries().len(), 2);

        assert!(store.remove(&gone).expect("remove"));
        assert!(!store.remove(&gone).expect("second remove is a no-op"));
        let reloaded = RecentProjectsStore::load(isolated_paths(&temp));
        assert_eq!(reloaded.entries().len(), 1);
    }

    #[test]
    fn corrupted_file_falls_back_to_empty_and_is_rewritten() {
        let temp = TempDir::new().expect("tempdir");
        let paths = isolated_paths(&temp);
        fs::create_dir_all(paths.cache_dir()).expect("cache dir");
        fs::write(paths.cache_dir().join("recent_workspaces.json"), "{ nope").expect("write");

        let mut store = RecentProjectsStore::load(paths.clone());
        assert_eq!(store.entries().len(), 0);
        store.record(&project(&temp, "alpha")).expect("record");
        assert_eq!(RecentProjectsStore::load(paths).entries().len(), 1);
    }

    #[test]
    fn clear_empties_persisted_list() {
        let temp = TempDir::new().expect("tempdir");
        let mut store = RecentProjectsStore::load(isolated_paths(&temp));
        store.record(&project(&temp, "alpha")).expect("record");
        store.clear().expect("clear");
        assert_eq!(store.entries().len(), 0);
        assert_eq!(
            RecentProjectsStore::load(isolated_paths(&temp))
                .entries()
                .len(),
            0
        );
    }

    #[test]
    fn record_rejects_missing_and_file_roots() {
        let temp = TempDir::new().expect("tempdir");
        let mut store = RecentProjectsStore::load(isolated_paths(&temp));
        assert!(store.record(&temp.path().join("missing")).is_err());
        let file = temp.path().join("file.txt");
        fs::write(&file, "x").expect("file");
        assert!(store.record(&file).is_err());
        assert_eq!(store.entries().len(), 0);
        assert!(!store.file_path().exists());
    }
}
