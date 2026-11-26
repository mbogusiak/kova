//! Persistence of the hibernation state (KOVA-038, FR-2).
//!
//! The state lives exclusively in `~/.cache/kova/workspaces/<hash>/hibernated_state.json`
//! (never inside the project repository) and is written atomically: a `.tmp.<pid>` sibling is
//! written, `fsync`ed and renamed over the target.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use kova_config::{KovaPaths, PathsError};
use serde::{Deserialize, Serialize};

use crate::error::LspDapError;
use crate::lsp::diagnostics::DiagnosticSnapshot;

/// Current schema version of [`HibernationStateSnapshot`].
pub const HIBERNATION_SCHEMA_VERSION: u32 = 1;
/// File name of the hibernation state inside the workspace cache directory.
pub const HIBERNATED_STATE_FILE: &str = "hibernated_state.json";

/// A document that was open in `jdt.ls` when the session was hibernated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HibernatedOpenDocument {
    /// Document URI.
    pub uri: String,
    /// Filesystem path of the document.
    pub file_path: PathBuf,
    /// LSP language id.
    pub language_id: String,
    /// Last buffer version synced to the server.
    pub version: u64,
}

/// Everything persisted when a session is hibernated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HibernationStateSnapshot {
    /// Schema version ([`HIBERNATION_SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Project root of the session.
    pub project_root: PathBuf,
    /// UNIX timestamp (seconds) of the hibernation.
    pub hibernated_at_unix_s: u64,
    /// Diagnostics at hibernation time.
    pub diagnostics: DiagnosticSnapshot,
    /// Documents open in the server at hibernation time.
    pub open_documents: Vec<HibernatedOpenDocument>,
}

/// Reads and writes `hibernated_state.json` in the workspace cache.
#[derive(Debug, Clone, Copy, Default)]
pub struct HibernationStore;

fn paths_error(project_root: &Path, err: &PathsError) -> LspDapError {
    LspDapError::Io {
        path: project_root.display().to_string(),
        message: err.to_string(),
    }
}

impl HibernationStore {
    /// `~/.cache/kova/workspaces/<hash>/hibernated_state.json` for `project_root`.
    ///
    /// # Errors
    /// [`LspDapError::Io`] when the project root cannot be canonicalised.
    pub fn snapshot_path(paths: &KovaPaths, project_root: &Path) -> Result<PathBuf, LspDapError> {
        Ok(paths
            .workspace_cache_dir(project_root)
            .map_err(|err| paths_error(project_root, &err))?
            .join(HIBERNATED_STATE_FILE))
    }

    /// Atomically writes `snapshot` for `snapshot.project_root` and returns the file path.
    ///
    /// # Errors
    /// [`LspDapError::Io`] on filesystem failures, [`LspDapError::SerializationError`] when the
    /// snapshot cannot be serialised.
    pub fn save_hibernated_state(
        paths: &KovaPaths,
        snapshot: &HibernationStateSnapshot,
    ) -> Result<PathBuf, LspDapError> {
        let root = &snapshot.project_root;
        let dir = paths
            .ensure_workspace_cache_dir(root)
            .map_err(|err| paths_error(root, &err))?;
        let target = dir.join(HIBERNATED_STATE_FILE);
        let tmp = dir.join(format!(
            ".{HIBERNATED_STATE_FILE}.tmp.{}",
            std::process::id()
        ));
        let bytes = serde_json::to_vec_pretty(snapshot)?;
        let write = || -> std::io::Result<()> {
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&tmp, &target)
        };
        if let Err(err) = write() {
            let _ignored = std::fs::remove_file(&tmp);
            return Err(LspDapError::io(target.display().to_string(), &err));
        }
        Ok(target)
    }

    /// Loads the persisted state; `Ok(None)` when the file is missing, corrupt or has an
    /// unsupported schema version (a stale file never blocks opening the project).
    ///
    /// # Errors
    /// [`LspDapError::Io`] when the project root cannot be canonicalised or the file exists but
    /// cannot be read.
    pub fn load_hibernated_state(
        paths: &KovaPaths,
        project_root: &Path,
    ) -> Result<Option<HibernationStateSnapshot>, LspDapError> {
        let path = Self::snapshot_path(paths, project_root)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(LspDapError::io(path.display().to_string(), &err)),
        };
        Ok(serde_json::from_slice::<HibernationStateSnapshot>(&bytes)
            .ok()
            .filter(|s| s.schema_version == HIBERNATION_SCHEMA_VERSION))
    }

    /// Removes the persisted state; returns whether a file was removed.
    ///
    /// # Errors
    /// [`LspDapError::Io`] when the file exists but cannot be removed.
    pub fn clear_hibernated_state(
        paths: &KovaPaths,
        project_root: &Path,
    ) -> Result<bool, LspDapError> {
        let path = Self::snapshot_path(paths, project_root)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(LspDapError::io(path.display().to_string(), &err)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use kova_buffer::{Utf16Point, Utf16Range};
    use serde_json::json;

    use super::*;
    use crate::lsp::diagnostics::{DiagnosticEntry, DiagnosticFileSnapshot, DiagnosticSeverity};

    fn fixture() -> (tempfile::TempDir, KovaPaths, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = KovaPaths::from_roots(
            tmp.path().join("config"),
            tmp.path().join("data"),
            tmp.path().join("cache"),
        )
        .unwrap();
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        (tmp, paths, project)
    }

    fn snapshot(project: &Path) -> HibernationStateSnapshot {
        HibernationStateSnapshot {
            schema_version: HIBERNATION_SCHEMA_VERSION,
            project_root: project.to_path_buf(),
            hibernated_at_unix_s: 1_700_000_000,
            diagnostics: DiagnosticSnapshot {
                project_root: project.to_path_buf(),
                files: vec![DiagnosticFileSnapshot {
                    uri: "file:///A.java".to_owned(),
                    version: Some(4),
                    diagnostics: vec![DiagnosticEntry {
                        range: Utf16Range::new(Utf16Point::new(0, 0), Utf16Point::new(0, 1)),
                        severity: DiagnosticSeverity::Error,
                        code: None,
                        source: Some("Java".to_owned()),
                        message: "boom".to_owned(),
                        tags: Vec::new(),
                        raw_lsp_diagnostic: json!({ "message": "boom", "severity": 1 }),
                    }],
                }],
            },
            open_documents: vec![HibernatedOpenDocument {
                uri: "file:///A.java".to_owned(),
                file_path: PathBuf::from("/A.java"),
                language_id: "java".to_owned(),
                version: 4,
            }],
        }
    }

    #[test]
    fn save_load_clear_round_trip_outside_project() {
        let (_tmp, paths, project) = fixture();
        assert_eq!(
            HibernationStore::load_hibernated_state(&paths, &project).unwrap(),
            None
        );
        let state = snapshot(&project);
        let path = HibernationStore::save_hibernated_state(&paths, &state).unwrap();
        assert_eq!(
            path,
            HibernationStore::snapshot_path(&paths, &project).unwrap()
        );
        assert!(path.starts_with(paths.workspaces_cache_dir()));
        assert_eq!(std::fs::read_dir(&project).unwrap().count(), 0);
        let siblings: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            siblings,
            vec![std::ffi::OsString::from(HIBERNATED_STATE_FILE)]
        );

        assert_eq!(
            HibernationStore::load_hibernated_state(&paths, &project).unwrap(),
            Some(state)
        );
        assert!(HibernationStore::clear_hibernated_state(&paths, &project).unwrap());
        assert!(!HibernationStore::clear_hibernated_state(&paths, &project).unwrap());
    }

    #[test]
    fn corrupt_or_unsupported_state_is_ignored() {
        let (_tmp, paths, project) = fixture();
        let path = HibernationStore::save_hibernated_state(&paths, &snapshot(&project)).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(
            HibernationStore::load_hibernated_state(&paths, &project).unwrap(),
            None
        );
        let mut future = snapshot(&project);
        future.schema_version = 99;
        std::fs::write(&path, serde_json::to_vec(&future).unwrap()).unwrap();
        assert_eq!(
            HibernationStore::load_hibernated_state(&paths, &project).unwrap(),
            None
        );
    }
}
