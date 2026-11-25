//! Atomic Omarchy session persistence in `~/.cache/kova/workspaces/<hash>/session.json`.

use std::collections::{BTreeSet, HashSet};
use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use kova_config::KovaPaths;
use serde::{Deserialize, Serialize};

use super::{BuildSystemKind, ProjectError, ProjectMetadata};

/// Current JSON schema version for [`WorkspaceSession`] and [`RecentWorkspaces`].
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// Maximum number of recent files retained per workspace session (`Cmd+E` / `Ctrl+E`).
pub const MAX_RECENT_FILES: usize = 64;

/// Maximum number of recently opened workspaces retained in `recent_workspaces.json`.
pub const MAX_RECENT_WORKSPACES: usize = 32;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Saved state of a single open editor tab inside a [`WorkspaceSession`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenTabState {
    /// Path relative to `project_root`.
    pub rel_path: PathBuf,
    /// Zero-based cursor line index.
    #[serde(default)]
    pub cursor_line: usize,
    /// Zero-based cursor column offset.
    #[serde(default)]
    pub cursor_column: usize,
    /// Zero-based first visible line at the top of the viewport.
    #[serde(default)]
    pub scroll_top_line: usize,
    /// Whether the tab is pinned in the tab bar.
    #[serde(default)]
    pub pinned: bool,
}

impl OpenTabState {
    /// Creates an [`OpenTabState`] positioned at line 0, column 0, unpinned.
    #[must_use]
    pub const fn new(rel_path: PathBuf) -> Self {
        Self {
            rel_path,
            cursor_line: 0,
            cursor_column: 0,
            scroll_top_line: 0,
            pinned: false,
        }
    }
}

/// Dock panel and Focus Mode layout persisted across IDE restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLayoutState {
    /// Whether distraction-free Focus Mode is active.
    #[serde(default)]
    pub focus_mode: bool,
    /// Whether the left tool dock is visible.
    #[serde(default = "default_true")]
    pub left_sidebar_visible: bool,
    /// Active left tool window identifier (for example `"project"` or `"structure"`).
    #[serde(default = "default_left_tool")]
    pub active_left_tool: Option<String>,
    /// Whether the bottom tool dock is visible.
    #[serde(default)]
    pub bottom_panel_visible: bool,
    /// Active bottom tool window identifier (`"terminal"`, `"run"`, `"debug"`, `"problems"`, `"git"`).
    #[serde(default)]
    pub active_bottom_tool: Option<String>,
}

const fn default_true() -> bool {
    true
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "serde default function for Option<String> must return Option<String>"
)]
fn default_left_tool() -> Option<String> {
    Some("project".to_owned())
}

impl Default for SessionLayoutState {
    fn default() -> Self {
        Self {
            focus_mode: false,
            left_sidebar_visible: true,
            active_left_tool: default_left_tool(),
            bottom_panel_visible: false,
            active_bottom_tool: None,
        }
    }
}

/// Ephemeral workspace session stored under `~/.cache/kova/workspaces/<hash>/session.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSession {
    /// Schema version (`SESSION_SCHEMA_VERSION`).
    pub schema_version: u32,
    /// Canonical path of the project workspace root.
    pub project_root: PathBuf,
    /// Deterministic 32-character hex `SHA-256` workspace hash.
    pub workspace_hash: String,
    /// Last opened timestamp in milliseconds since Unix epoch.
    #[serde(default)]
    pub last_opened_unix_ms: u64,
    /// Ordered list of open editor tabs.
    #[serde(default)]
    pub open_tabs: Vec<OpenTabState>,
    /// Index of the active tab within `open_tabs`.
    #[serde(default)]
    pub active_tab_index: Option<usize>,
    /// Project-relative directory paths currently expanded in the *Project* tree.
    #[serde(default)]
    pub expanded_dirs: BTreeSet<PathBuf>,
    /// Most recently opened project-relative files (newest first, capped at 64).
    #[serde(default)]
    pub recent_files: Vec<PathBuf>,
    /// Saved dock/panel visibility and Focus Mode layout.
    #[serde(default)]
    pub layout: SessionLayoutState,
}

impl WorkspaceSession {
    /// Creates an empty [`WorkspaceSession`] initialized from [`ProjectMetadata`].
    #[must_use]
    pub fn new(metadata: &ProjectMetadata) -> Self {
        Self::empty(metadata.root.clone(), metadata.workspace_hash.clone())
    }

    /// Creates an empty [`WorkspaceSession`] for `project_root` and `workspace_hash`.
    #[must_use]
    pub fn empty(project_root: PathBuf, workspace_hash: String) -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            project_root,
            workspace_hash,
            last_opened_unix_ms: current_unix_ms(),
            open_tabs: Vec::new(),
            active_tab_index: None,
            expanded_dirs: BTreeSet::new(),
            recent_files: Vec::new(),
            layout: SessionLayoutState::default(),
        }
    }

    /// Opens or updates a tab, focuses it, and records it at the front of `recent_files`.
    pub fn open_or_focus_tab(&mut self, tab: OpenTabState) {
        let rel_path = tab.rel_path.clone();
        if let Some(idx) = self
            .open_tabs
            .iter()
            .position(|existing| existing.rel_path == rel_path)
        {
            self.open_tabs[idx] = tab;
            self.active_tab_index = Some(idx);
        } else {
            self.open_tabs.push(tab);
            self.active_tab_index = Some(self.open_tabs.len() - 1);
        }
        self.record_recent_file(rel_path);
    }

    /// Moves `rel_path` to the front of `recent_files`, deduplicating and capping at [`MAX_RECENT_FILES`].
    pub fn record_recent_file(&mut self, rel_path: PathBuf) {
        self.recent_files.retain(|existing| existing != &rel_path);
        self.recent_files.insert(0, rel_path);
        self.recent_files.truncate(MAX_RECENT_FILES);
    }

    /// Validates and cleans all stored paths against `project_root`.
    ///
    /// Rejects any path that is empty, absolute, contains `.`, `..`, NUL bytes, or resolves
    /// through a symlink outside canonical `project_root`. Also prunes entries that no longer
    /// exist on disk, deduplicates tabs and recent files, caps `recent_files` at 64 entries,
    /// and keeps `active_tab_index` in bounds.
    pub fn sanitize_against_project(&mut self, project_root: &Path) {
        let canonical_root = std::fs::canonicalize(project_root).ok();

        let previous_active_path = self
            .active_tab_index
            .and_then(|idx| self.open_tabs.get(idx))
            .map(|tab| tab.rel_path.clone());
        let had_active_index = self.active_tab_index.is_some();

        let mut seen_tabs = HashSet::with_capacity(self.open_tabs.len());
        let mut sanitized_tabs = Vec::with_capacity(self.open_tabs.len());
        for tab in self.open_tabs.drain(..) {
            if canonical_root
                .as_deref()
                .is_some_and(|root| resolves_inside_project_file(root, &tab.rel_path))
                && seen_tabs.insert(tab.rel_path.clone())
            {
                sanitized_tabs.push(tab);
            }
        }
        self.open_tabs = sanitized_tabs;

        self.active_tab_index = if self.open_tabs.is_empty() {
            None
        } else if let Some(prev_path) = previous_active_path {
            self.open_tabs
                .iter()
                .position(|tab| tab.rel_path == prev_path)
                .or(Some(0))
        } else if had_active_index {
            Some(0)
        } else {
            None
        };

        self.expanded_dirs.retain(|dir| {
            canonical_root
                .as_deref()
                .is_some_and(|root| resolves_inside_project_dir(root, dir))
        });

        let mut seen_recent = HashSet::with_capacity(self.recent_files.len());
        let mut sanitized_recent =
            Vec::with_capacity(self.recent_files.len().min(MAX_RECENT_FILES));
        for rel_path in self.recent_files.drain(..) {
            if sanitized_recent.len() >= MAX_RECENT_FILES {
                break;
            }
            if canonical_root
                .as_deref()
                .is_some_and(|root| resolves_inside_project_file(root, &rel_path))
                && seen_recent.insert(rel_path.clone())
            {
                sanitized_recent.push(rel_path);
            }
        }
        self.recent_files = sanitized_recent;
    }
}

/// Summary entry for a recently opened workspace stored in `~/.cache/kova/recent_workspaces.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentWorkspaceEntry {
    /// Display name of the project workspace.
    pub name: String,
    /// Canonical root path of the project workspace.
    pub root: PathBuf,
    /// Deterministic 32-character hex `SHA-256` workspace hash.
    pub workspace_hash: String,
    /// Build system detected when the workspace was last opened.
    pub build_system: BuildSystemKind,
    /// Last opened timestamp in milliseconds since Unix epoch.
    pub last_opened_unix_ms: u64,
}

impl RecentWorkspaceEntry {
    /// Builds a [`RecentWorkspaceEntry`] from [`ProjectMetadata`] using the current timestamp.
    #[must_use]
    pub fn from_metadata(metadata: &ProjectMetadata) -> Self {
        Self {
            name: metadata.name.clone(),
            root: metadata.root.clone(),
            workspace_hash: metadata.workspace_hash.clone(),
            build_system: metadata.build_system,
            last_opened_unix_ms: current_unix_ms(),
        }
    }
}

/// Global list of recently opened workspaces stored under `~/.cache/kova/recent_workspaces.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentWorkspaces {
    /// Schema version (`SESSION_SCHEMA_VERSION`).
    pub schema_version: u32,
    /// Recent workspaces ordered most-recent first (capped at [`MAX_RECENT_WORKSPACES`]).
    #[serde(default)]
    pub entries: Vec<RecentWorkspaceEntry>,
}

impl Default for RecentWorkspaces {
    fn default() -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }
}

impl RecentWorkspaces {
    /// Inserts or promotes `entry` to the front of the list and enforces [`MAX_RECENT_WORKSPACES`].
    pub fn upsert(&mut self, entry: RecentWorkspaceEntry) {
        self.entries.retain(|existing| {
            existing.workspace_hash != entry.workspace_hash && existing.root != entry.root
        });
        self.entries.insert(0, entry);
        self.entries.truncate(MAX_RECENT_WORKSPACES);
    }
}

/// Atomically writes `session` to `~/.cache/kova/workspaces/<hash>/session.json`.
///
/// Creates only the workspace cache directory via [`KovaPaths::ensure_workspace_cache_dir`] and
/// never touches `session.project_root`.
///
/// # Errors
///
/// Returns [`ProjectError::Paths`] if the cache directory cannot be resolved or created, or
/// [`ProjectError::Io`] if JSON serialization or atomic file replacement fails.
pub fn save_workspace_session(
    paths: &KovaPaths,
    session: &WorkspaceSession,
) -> Result<PathBuf, ProjectError> {
    let cache_dir = paths.ensure_workspace_cache_dir(&session.project_root)?;
    write_json_atomic(&cache_dir, "session.json", "session", session)
}

/// Loads and sanitizes `session.json` for `project_root` from `~/.cache/kova/workspaces/<hash>/`.
///
/// Returns `Ok(None)` when `session.json` does not exist yet (without creating any files).
///
/// # Errors
///
/// Returns [`ProjectError::Paths`] if `project_root` cannot be resolved,
/// [`ProjectError::Io`] on unreadable files, [`ProjectError::CorruptedSession`] on malformed
/// `JSON`, or [`ProjectError::UnsupportedSessionVersion`] when `schema_version` is unknown.
pub fn load_workspace_session(
    paths: &KovaPaths,
    project_root: &Path,
) -> Result<Option<WorkspaceSession>, ProjectError> {
    let cache_dir = paths.workspace_cache_dir(project_root)?;
    let session_path = cache_dir.join("session.json");
    let Some(mut session) = read_json_optional::<WorkspaceSession>(&session_path)? else {
        return Ok(None);
    };

    if session.schema_version != SESSION_SCHEMA_VERSION {
        return Err(ProjectError::UnsupportedSessionVersion {
            path: session_path,
            version: session.schema_version,
        });
    }

    session.sanitize_against_project(project_root);
    Ok(Some(session))
}

/// Loads `session.json` if valid, or falls back to a clean [`WorkspaceSession`] with a diagnostic message.
///
/// Never fails or panics when `session.json` is missing, corrupted, or uses an unknown schema version.
#[must_use]
pub fn load_workspace_session_or_default(
    paths: &KovaPaths,
    metadata: &ProjectMetadata,
) -> (WorkspaceSession, Option<String>) {
    match load_workspace_session(paths, &metadata.root) {
        Ok(Some(session)) => (session, None),
        Ok(None) => (WorkspaceSession::new(metadata), None),
        Err(err) => (WorkspaceSession::new(metadata), Some(err.to_string())),
    }
}

/// Loads `~/.cache/kova/recent_workspaces.json`, returning an empty list if absent.
///
/// # Errors
///
/// Returns [`ProjectError::Io`], [`ProjectError::CorruptedSession`], or
/// [`ProjectError::UnsupportedSessionVersion`] if an existing file cannot be parsed.
pub fn load_recent_workspaces(paths: &KovaPaths) -> Result<RecentWorkspaces, ProjectError> {
    let file_path = paths.cache_dir().join("recent_workspaces.json");
    let Some(recent) = read_json_optional::<RecentWorkspaces>(&file_path)? else {
        return Ok(RecentWorkspaces::default());
    };

    if recent.schema_version != SESSION_SCHEMA_VERSION {
        return Err(ProjectError::UnsupportedSessionVersion {
            path: file_path,
            version: recent.schema_version,
        });
    }

    Ok(recent)
}

/// Atomically persists `recent` to `~/.cache/kova/recent_workspaces.json`.
///
/// # Errors
///
/// Returns [`ProjectError::Io`] if creating `paths.cache_dir()` or writing the file fails.
pub fn save_recent_workspaces(
    paths: &KovaPaths,
    recent: &RecentWorkspaces,
) -> Result<PathBuf, ProjectError> {
    let cache_dir = paths.cache_dir();
    std::fs::create_dir_all(cache_dir).map_err(|err| ProjectError::Io {
        path: cache_dir.to_path_buf(),
        message: err.to_string(),
    })?;
    write_json_atomic(
        cache_dir,
        "recent_workspaces.json",
        "recent_workspaces",
        recent,
    )
}

/// Loads, updates, and atomically saves `recent_workspaces.json` for `metadata`.
///
/// If the existing `recent_workspaces.json` is corrupted or uses an unsupported version, it is
/// replaced cleanly with a fresh list containing `metadata`.
///
/// # Errors
///
/// Propagates I/O errors from [`save_recent_workspaces`].
pub fn record_recent_workspace(
    paths: &KovaPaths,
    metadata: &ProjectMetadata,
) -> Result<RecentWorkspaces, ProjectError> {
    let mut recent = load_recent_workspaces(paths).unwrap_or_default();
    recent.upsert(RecentWorkspaceEntry::from_metadata(metadata));
    save_recent_workspaces(paths, &recent)?;
    Ok(recent)
}

/// Returns `true` when `path` is a non-empty relative path composed strictly of `Normal` components
/// with no NUL bytes.
#[must_use]
pub(crate) fn is_safe_relative_path(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.is_empty() || bytes.contains(&0) || path.is_absolute() {
        return false;
    }

    let mut has_normal = false;
    for component in path.components() {
        match component {
            Component::Normal(_) => has_normal = true,
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => return false,
        }
    }
    has_normal
}

fn resolves_inside_project_file(canonical_root: &Path, rel_path: &Path) -> bool {
    resolves_inside_project_with(canonical_root, rel_path, Path::is_file)
}

fn resolves_inside_project_dir(canonical_root: &Path, rel_path: &Path) -> bool {
    resolves_inside_project_with(canonical_root, rel_path, Path::is_dir)
}

fn resolves_inside_project_with<F>(canonical_root: &Path, rel_path: &Path, predicate: F) -> bool
where
    F: FnOnce(&Path) -> bool,
{
    if !is_safe_relative_path(rel_path) {
        return false;
    }
    let joined = canonical_root.join(rel_path);
    let Ok(canonical_target) = std::fs::canonicalize(&joined) else {
        return false;
    };
    canonical_target.starts_with(canonical_root) && predicate(&canonical_target)
}

fn read_json_optional<T>(path: &Path) -> Result<Option<T>, ProjectError>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ProjectError::Io {
                path: path.to_path_buf(),
                message: err.to_string(),
            });
        }
    };

    serde_json::from_slice::<T>(&bytes)
        .map(Some)
        .map_err(|err| ProjectError::CorruptedSession {
            path: path.to_path_buf(),
            message: err.to_string(),
        })
}

fn write_json_atomic<T>(
    dir: &Path,
    target_name: &str,
    tmp_prefix: &str,
    value: &T,
) -> Result<PathBuf, ProjectError>
where
    T: Serialize,
{
    let target_path = dir.join(target_name);
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| ProjectError::Io {
        path: target_path.clone(),
        message: err.to_string(),
    })?;

    let pid = std::process::id();
    let (tmp_path, mut file) = loop {
        let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = dir.join(format!(".{tmp_prefix}.tmp.{pid}.{seq}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {}
            Err(err) => {
                return Err(ProjectError::Io {
                    path: candidate,
                    message: err.to_string(),
                });
            }
        }
    };

    let write_res = (|| -> Result<(), std::io::Error> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();

    drop(file);

    if let Err(err) = write_res {
        drop(std::fs::remove_file(&tmp_path));
        return Err(ProjectError::Io {
            path: tmp_path,
            message: err.to_string(),
        });
    }

    if let Err(err) = std::fs::rename(&tmp_path, &target_path) {
        drop(std::fs::remove_file(&tmp_path));
        return Err(ProjectError::Io {
            path: target_path,
            message: err.to_string(),
        });
    }

    Ok(target_path)
}

fn current_unix_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |dur| dur.as_millis());
    u64::try_from(millis).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests assert session sanitization and atomic persistence"
)]
mod tests {
    use std::path::{Path, PathBuf};

    use kova_config::KovaPaths;
    use tempfile::TempDir;

    use super::{
        MAX_RECENT_FILES, OpenTabState, SESSION_SCHEMA_VERSION, WorkspaceSession,
        is_safe_relative_path, load_workspace_session, load_workspace_session_or_default,
        save_workspace_session,
    };
    use crate::project::{ProjectError, ProjectMetadata};

    fn isolated_paths(temp: &TempDir) -> KovaPaths {
        KovaPaths::from_roots(
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("cache"),
        )
        .expect("isolated temp paths are absolute")
    }

    #[test]
    fn safe_relative_path_rejects_traversal_absolute_dot_and_nul() {
        assert!(is_safe_relative_path(Path::new("src/Main.java")));
        assert!(!is_safe_relative_path(Path::new("")));
        assert!(!is_safe_relative_path(Path::new(".")));
        assert!(!is_safe_relative_path(Path::new("./src/Main.java")));
        assert!(!is_safe_relative_path(Path::new("../etc/passwd")));
        assert!(!is_safe_relative_path(Path::new("src/../../etc/passwd")));
        assert!(!is_safe_relative_path(Path::new("/etc/passwd")));
        assert!(!is_safe_relative_path(Path::new("src\0Main.java")));
    }

    #[test]
    fn sanitize_filters_invalid_paths_dedups_and_fixes_active_tab() {
        let project = TempDir::new().expect("project dir");
        std::fs::create_dir_all(project.path().join("src")).expect("src dir");
        std::fs::create_dir_all(project.path().join("docs")).expect("docs dir");
        std::fs::write(project.path().join("src/A.java"), "class A {}").expect("A.java");
        std::fs::write(project.path().join("src/B.java"), "class B {}").expect("B.java");

        let mut session = WorkspaceSession::empty(project.path().to_path_buf(), "abc".to_owned());
        session.open_tabs = vec![
            OpenTabState::new(PathBuf::from("../../etc/passwd")),
            OpenTabState::new(PathBuf::from("src/Deleted.java")),
            OpenTabState::new(PathBuf::from("src/A.java")),
            OpenTabState::new(PathBuf::from("src/A.java")),
            OpenTabState::new(PathBuf::from("src/B.java")),
        ];
        // Point active_tab_index at the deleted file so sanitization falls back to index 0 (A.java).
        session.active_tab_index = Some(1);
        session.expanded_dirs.insert(PathBuf::from("src"));
        session.expanded_dirs.insert(PathBuf::from("../outside"));
        session.expanded_dirs.insert(PathBuf::from("missing_dir"));

        for idx in 0..80 {
            let rel = if idx % 2 == 0 {
                PathBuf::from("src/A.java")
            } else {
                PathBuf::from("src/B.java")
            };
            session.recent_files.push(rel);
        }

        session.sanitize_against_project(project.path());

        assert_eq!(session.open_tabs.len(), 2);
        assert_eq!(session.open_tabs[0].rel_path, PathBuf::from("src/A.java"));
        assert_eq!(session.open_tabs[1].rel_path, PathBuf::from("src/B.java"));
        assert_eq!(session.active_tab_index, Some(0));
        assert!(session.expanded_dirs.contains(Path::new("src")));
        assert_eq!(session.expanded_dirs.len(), 1);
        assert!(session.recent_files.len() <= MAX_RECENT_FILES);
        assert_eq!(
            session.recent_files,
            vec![PathBuf::from("src/A.java"), PathBuf::from("src/B.java")]
        );
    }

    #[test]
    fn save_and_load_roundtrip_and_fallback_on_corruption() {
        let omarchy = TempDir::new().expect("omarchy dir");
        let project = TempDir::new().expect("project dir");
        let paths = isolated_paths(&omarchy);

        std::fs::create_dir_all(project.path().join("src")).expect("src dir");
        std::fs::write(project.path().join("src/App.java"), "class App {}").expect("App.java");

        let meta = ProjectMetadata::inspect(project.path(), &paths).expect("inspect");
        assert!(
            load_workspace_session(&paths, &meta.root)
                .expect("missing session returns Ok(None)")
                .is_none()
        );

        let mut session = WorkspaceSession::new(&meta);
        session.open_or_focus_tab(OpenTabState {
            rel_path: PathBuf::from("src/App.java"),
            cursor_line: 12,
            cursor_column: 4,
            scroll_top_line: 3,
            pinned: true,
        });
        session.expanded_dirs.insert(PathBuf::from("src"));

        let saved_path = save_workspace_session(&paths, &session).expect("save session");
        assert_eq!(saved_path, meta.cache_dir.join("session.json"));

        let loaded = load_workspace_session(&paths, &meta.root)
            .expect("load session")
            .expect("session exists");
        assert_eq!(loaded.schema_version, SESSION_SCHEMA_VERSION);
        assert_eq!(loaded.open_tabs, session.open_tabs);
        assert_eq!(loaded.active_tab_index, Some(0));
        assert_eq!(loaded.expanded_dirs, session.expanded_dirs);

        // Corrupt session.json and verify explicit error + safe fallback helper.
        std::fs::write(&saved_path, "{ not valid json").expect("corrupt json");
        assert!(matches!(
            load_workspace_session(&paths, &meta.root),
            Err(ProjectError::CorruptedSession { .. })
        ));
        let (fallback, warning) = load_workspace_session_or_default(&paths, &meta);
        assert_eq!(fallback.open_tabs.len(), 0);
        assert!(warning.is_some());
    }
}
