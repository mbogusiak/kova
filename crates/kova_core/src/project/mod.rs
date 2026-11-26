//! Headless project workspace model, `.gitignore`-aware scanner, and Omarchy session storage.
//!
//! In accordance with Kova's Omarchy architecture, inspecting and scanning a user project
//! repository never creates hidden directories (such as `.idea/` or `.kova/`) inside the
//! project root. Ephemeral workspace state is persisted exclusively under
//! `~/.cache/kova/workspaces/<hash>/`.

pub mod active;
pub mod recent;
pub mod scanner;
pub mod session;

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use kova_config::{KovaPaths, PathsError};
use serde::{Deserialize, Serialize};

pub use active::{ActiveProject, ProjectGeneration, ProjectPhase};
pub use recent::{MAX_RECENT_PROJECTS_IN_MENU, RecentProject, RecentProjectsStore};
pub use scanner::{
    DEFAULT_MAX_ENTRIES, ProjectFileEntry, ProjectNode, ProjectNodeId, ProjectNodeKind,
    ProjectScanOptions, ProjectTree, scan_project,
};
pub use session::{
    MAX_RECENT_FILES, MAX_RECENT_WORKSPACES, OpenTabState, RecentWorkspaceEntry, RecentWorkspaces,
    SESSION_SCHEMA_VERSION, SessionLayoutState, WorkspaceSession, load_recent_workspaces,
    load_workspace_session, load_workspace_session_or_default, record_recent_workspace,
    save_recent_workspaces, save_workspace_session,
};

const GRADLE_MARKERS: [&str; 5] = [
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradlew",
];

const MAVEN_MARKER: &str = "pom.xml";

/// Errors returned while inspecting a project, scanning its tree, or persisting session state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectError {
    /// Target project directory does not exist.
    #[error("project directory does not exist: `{path}`")]
    NotFound {
        /// Path that was not found.
        path: PathBuf,
    },
    /// Target path exists on disk, but is not a directory.
    #[error("project path is not a directory: `{path}`")]
    NotADirectory {
        /// Path that is not a directory.
        path: PathBuf,
    },
    /// Filesystem I/O failure during project inspection or session persistence.
    #[error("I/O error at `{path}`: {message}")]
    Io {
        /// Filesystem path associated with the failure.
        path: PathBuf,
        /// Underlying OS error description.
        message: String,
    },
    /// Failure resolving Omarchy paths or workspace hashes via [`KovaPaths`].
    #[error(transparent)]
    Paths(#[from] PathsError),
    /// Non-recoverable failure while walking the project directory tree.
    #[error("failed to walk project tree{}: {message}", path.as_ref().map_or_else(String::new, |p| format!(" at `{}`", p.display())))]
    Walk {
        /// Optional path where traversal failed.
        path: Option<PathBuf>,
        /// Underlying walker error message.
        message: String,
    },
    /// `session.json` or `recent_workspaces.json` contains malformed JSON.
    #[error("corrupted session file `{path}`: {message}")]
    CorruptedSession {
        /// Path to the corrupted JSON file.
        path: PathBuf,
        /// Deserialization error message.
        message: String,
    },
    /// Session file declares a `schema_version` unsupported by this version of Kova.
    #[error("unsupported session schema version {version} in `{path}`")]
    UnsupportedSessionVersion {
        /// Path to the session file.
        path: PathBuf,
        /// Unsupported schema version found in the file.
        version: u32,
    },
    /// Relative path violates project containment rules (absolute, `..`, `.`, empty, or NUL).
    #[error("invalid project-relative path `{path}`: {reason}")]
    InvalidProjectPath {
        /// Rejected path value.
        path: PathBuf,
        /// Explanation of why the path was rejected.
        reason: String,
    },
}

/// Detected JVM build system at the root of a project workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildSystemKind {
    /// Gradle build detected (`build.gradle`, `build.gradle.kts`, `settings.gradle*`, or `gradlew`).
    Gradle,
    /// Maven build detected (`pom.xml`).
    Maven,
    /// Both Gradle and Maven build markers exist at the project root.
    Mixed,
    /// Neither Gradle nor Maven markers were detected at the project root.
    Unknown,
}

impl BuildSystemKind {
    /// Detects the build system at `root` using $O(1)$ root-level marker checks.
    #[must_use]
    pub fn detect(root: &Path) -> Self {
        let has_gradle = GRADLE_MARKERS
            .iter()
            .any(|marker| root.join(marker).is_file());
        let has_maven = root.join(MAVEN_MARKER).is_file();

        match (has_gradle, has_maven) {
            (true, true) => Self::Mixed,
            (true, false) => Self::Gradle,
            (false, true) => Self::Maven,
            (false, false) => Self::Unknown,
        }
    }
}

/// Read-only descriptor for a Kova project workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMetadata {
    /// Canonicalized filesystem root of the project.
    pub root: PathBuf,
    /// Display name derived from the final path component of `root`.
    pub name: String,
    /// Deterministic 32-character hex `SHA-256` hash of `root`.
    pub workspace_hash: String,
    /// Ephemeral Omarchy cache directory (`~/.cache/kova/workspaces/<hash>`).
    pub cache_dir: PathBuf,
    /// Path to the optional explicit `<project_root>/kova.toml` file.
    pub kova_toml_path: PathBuf,
    /// Whether `<project_root>/kova.toml` currently exists as a regular file.
    pub has_kova_toml: bool,
    /// Build system detected at the project root.
    pub build_system: BuildSystemKind,
}

impl ProjectMetadata {
    /// Inspects `root` and computes Omarchy metadata without creating any files or directories.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError::NotFound`] if `root` does not exist,
    /// [`ProjectError::NotADirectory`] if `root` is not a directory,
    /// [`ProjectError::Io`] if canonicalization or metadata lookup fails, or
    /// [`ProjectError::Paths`] if workspace hash resolution fails.
    pub fn inspect(root: &Path, paths: &KovaPaths) -> Result<Self, ProjectError> {
        let canonical_root = ensure_canonical_dir(root)?;
        let name = project_display_name(&canonical_root);
        let workspace_hash = paths.workspace_hash(&canonical_root)?;
        let cache_dir = paths.workspace_cache_dir(&canonical_root)?;
        let kova_toml_path = KovaPaths::project_config_file(&canonical_root);
        let has_kova_toml = kova_toml_path.is_file();
        let build_system = BuildSystemKind::detect(&canonical_root);

        Ok(Self {
            root: canonical_root,
            name,
            workspace_hash,
            cache_dir,
            kova_toml_path,
            has_kova_toml,
            build_system,
        })
    }
}

/// High-level opened workspace combining metadata, scanned file tree, and session state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Project metadata and Omarchy cache locations.
    pub metadata: ProjectMetadata,
    /// Scanned project file tree respecting `.gitignore`.
    pub tree: ProjectTree,
    /// Sanitized workspace session restored from cache (or initialized empty).
    pub session: WorkspaceSession,
    /// Optional diagnostic warning produced if a corrupted session file fell back to defaults.
    pub session_warning: Option<String>,
}

impl Project {
    /// Opens a project using default [`ProjectScanOptions`], restoring its sanitized session.
    ///
    /// Never creates files or directories inside `root`.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`ProjectMetadata::inspect`] or [`scan_project`].
    pub fn open(root: &Path, paths: &KovaPaths) -> Result<Self, ProjectError> {
        Self::open_with_options(root, paths, &ProjectScanOptions::default())
    }

    /// Opens a project with explicit [`ProjectScanOptions`], restoring its sanitized session.
    ///
    /// Never creates files or directories inside `root`.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`ProjectMetadata::inspect`] or [`scan_project`].
    pub fn open_with_options(
        root: &Path,
        paths: &KovaPaths,
        options: &ProjectScanOptions,
    ) -> Result<Self, ProjectError> {
        let metadata = ProjectMetadata::inspect(root, paths)?;
        let tree = scan_project(&metadata.root, options)?;
        let (session, session_warning) = load_workspace_session_or_default(paths, &metadata);

        Ok(Self {
            metadata,
            tree,
            session,
            session_warning,
        })
    }

    /// Re-scans the entire project tree with `options`.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`scan_project`].
    pub fn rescan(&mut self, options: &ProjectScanOptions) -> Result<(), ProjectError> {
        self.tree = scan_project(&self.metadata.root, options)?;
        Ok(())
    }

    /// Re-scans a single subtree `dir_rel_path` relative to the project root.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`ProjectTree::rescan_subtree`].
    pub fn rescan_subtree(
        &mut self,
        dir_rel_path: &Path,
        options: &ProjectScanOptions,
    ) -> Result<(), ProjectError> {
        self.tree
            .rescan_subtree(&self.metadata.root, dir_rel_path, options)
    }

    /// Persists the current [`WorkspaceSession`] atomically into Omarchy cache storage.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`save_workspace_session`].
    pub fn save_session(&self, paths: &KovaPaths) -> Result<PathBuf, ProjectError> {
        save_workspace_session(paths, &self.session)
    }
}

pub(crate) fn ensure_canonical_dir(root: &Path) -> Result<PathBuf, ProjectError> {
    match std::fs::metadata(root) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(ProjectError::NotADirectory {
                    path: root.to_path_buf(),
                });
            }
        }
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(ProjectError::NotFound {
                path: root.to_path_buf(),
            });
        }
        Err(err) => {
            return Err(ProjectError::Io {
                path: root.to_path_buf(),
                message: err.to_string(),
            });
        }
    }

    let canonical = std::fs::canonicalize(root).map_err(|err| ProjectError::Io {
        path: root.to_path_buf(),
        message: err.to_string(),
    })?;

    if !canonical.is_dir() {
        return Err(ProjectError::NotADirectory { path: canonical });
    }

    Ok(canonical)
}

/// Display name of a project root: its final path component (`kova-project` for `/`).
#[must_use]
pub fn project_display_name(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "kova-project".to_owned())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests assert project inspection outcomes"
)]
mod tests {
    use std::path::PathBuf;

    use kova_config::KovaPaths;
    use tempfile::TempDir;

    use super::{BuildSystemKind, Project, ProjectError, ProjectMetadata};

    fn isolated_paths(temp: &TempDir) -> KovaPaths {
        KovaPaths::from_roots(
            temp.path().join("config"),
            temp.path().join("data"),
            temp.path().join("cache"),
        )
        .expect("isolated temp paths are absolute")
    }

    #[test]
    fn detects_gradle_maven_mixed_and_unknown_projects() {
        let temp = TempDir::new().expect("temp dir");

        let unknown_dir = temp.path().join("plain");
        std::fs::create_dir_all(&unknown_dir).expect("plain dir");
        assert_eq!(
            BuildSystemKind::detect(&unknown_dir),
            BuildSystemKind::Unknown
        );

        let gradle_dir = temp.path().join("gradle-app");
        std::fs::create_dir_all(&gradle_dir).expect("gradle dir");
        std::fs::write(gradle_dir.join("build.gradle.kts"), "").expect("build.gradle.kts");
        assert_eq!(
            BuildSystemKind::detect(&gradle_dir),
            BuildSystemKind::Gradle
        );

        let maven_dir = temp.path().join("maven-app");
        std::fs::create_dir_all(&maven_dir).expect("maven dir");
        std::fs::write(maven_dir.join("pom.xml"), "<project/>").expect("pom.xml");
        assert_eq!(BuildSystemKind::detect(&maven_dir), BuildSystemKind::Maven);

        let mixed_dir = temp.path().join("mixed-app");
        std::fs::create_dir_all(&mixed_dir).expect("mixed dir");
        std::fs::write(mixed_dir.join("gradlew"), "").expect("gradlew");
        std::fs::write(mixed_dir.join("pom.xml"), "<project/>").expect("pom.xml");
        assert_eq!(BuildSystemKind::detect(&mixed_dir), BuildSystemKind::Mixed);
    }

    #[test]
    fn inspect_populates_metadata_without_creating_files() {
        let omarchy = TempDir::new().expect("omarchy dir");
        let project = TempDir::new().expect("project dir");
        let paths = isolated_paths(&omarchy);

        let root = project.path().join("my-service");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::write(root.join("build.gradle.kts"), "").expect("gradle file");
        std::fs::write(root.join("kova.toml"), "schema = 1\n").expect("kova.toml");

        let meta = ProjectMetadata::inspect(&root, &paths).expect("inspect succeeds");
        assert_eq!(meta.name, "my-service");
        assert_eq!(meta.build_system, BuildSystemKind::Gradle);
        assert!(meta.has_kova_toml);
        assert_eq!(meta.kova_toml_path, meta.root.join("kova.toml"));
        assert_eq!(meta.workspace_hash.len(), 32);
        assert_eq!(
            meta.cache_dir,
            paths.workspaces_cache_dir().join(&meta.workspace_hash)
        );
        assert!(
            !meta.cache_dir.exists(),
            "inspect must be strictly read-only"
        );
        assert!(!meta.root.join(".idea").exists());
        assert!(!meta.root.join(".kova").exists());
    }

    #[test]
    fn inspect_rejects_missing_and_file_paths() {
        let omarchy = TempDir::new().expect("omarchy dir");
        let project = TempDir::new().expect("project dir");
        let paths = isolated_paths(&omarchy);

        let missing = project.path().join("does-not-exist");
        assert!(matches!(
            ProjectMetadata::inspect(&missing, &paths),
            Err(ProjectError::NotFound { .. })
        ));

        let file_path = project.path().join("file.txt");
        std::fs::write(&file_path, "hello").expect("write file");
        assert!(matches!(
            ProjectMetadata::inspect(&file_path, &paths),
            Err(ProjectError::NotADirectory { .. })
        ));
    }

    #[test]
    fn project_open_and_save_session_leave_project_root_clean() {
        let omarchy = TempDir::new().expect("omarchy dir");
        let project_dir = TempDir::new().expect("project dir");
        let paths = isolated_paths(&omarchy);

        std::fs::create_dir_all(project_dir.path().join("src")).expect("src");
        std::fs::write(project_dir.path().join("pom.xml"), "<project/>").expect("pom");
        std::fs::write(project_dir.path().join("src/App.java"), "class App {}").expect("App.java");

        let mut project = Project::open(project_dir.path(), &paths).expect("open project");
        assert_eq!(project.metadata.build_system, BuildSystemKind::Maven);
        assert_eq!(project.tree.file_count(), 2);
        assert_eq!(project.session.open_tabs.len(), 0);

        project
            .session
            .open_or_focus_tab(crate::project::OpenTabState::new(PathBuf::from(
                "src/App.java",
            )));
        let saved = project.save_session(&paths).expect("save session");
        assert!(saved.is_file());
        assert!(saved.starts_with(paths.workspaces_cache_dir()));
        assert!(!project_dir.path().join(".idea").exists());
        assert!(!project_dir.path().join(".kova").exists());
    }
}
