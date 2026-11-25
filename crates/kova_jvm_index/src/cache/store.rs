//! Atomic filesystem store for `KIDX` workspace snapshots and `KJAR` shared shards.

use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use kova_config::KovaPaths;

use crate::cache::codec::{
    ArtifactFingerprint, CacheLoadOutcome, CacheMissReason, CachedJarShard,
    CachedWorkspaceSnapshot, HEADER_SIZE, MAX_CACHE_FILE_BYTES, decode_jar_shard,
    decode_workspace_snapshot, encode_jar_shard, encode_workspace_snapshot,
};
use crate::error::JvmIndexError;

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Deterministic cache path layout under `KovaPaths` (Omarchy-compliant, zero files in `project_root`).
#[derive(Debug)]
pub struct JvmCacheLayout;

impl JvmCacheLayout {
    /// Returns `~/.cache/kova/workspaces/<hash>/jvm_index.kidx`.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError::Cache`] if `KovaPaths::workspace_cache_dir` fails.
    pub fn workspace_index_file(
        paths: &KovaPaths,
        project_root: &Path,
    ) -> Result<PathBuf, JvmIndexError> {
        let dir = paths
            .workspace_cache_dir(project_root)
            .map_err(|err| JvmIndexError::Cache {
                path: project_root.display().to_string(),
                reason: err.to_string(),
            })?;
        Ok(dir.join("jvm_index.kidx"))
    }

    /// Returns `~/.cache/kova/jvm_jars`.
    #[must_use]
    pub fn shared_jars_dir(paths: &KovaPaths) -> PathBuf {
        paths.cache_dir().join("jvm_jars")
    }

    /// Returns `~/.cache/kova/jvm_jars/<sha256_prefix>.kjar` for `fingerprint`.
    #[must_use]
    pub fn jar_shard_file(paths: &KovaPaths, fingerprint: &ArtifactFingerprint) -> PathBuf {
        Self::shared_jars_dir(paths).join(format!("{}.kjar", fingerprint.cache_key_hex()))
    }
}

/// Atomic reader/writer for `jvm_index.kidx` and shared `.kjar` shards.
#[derive(Debug)]
pub struct JvmIndexCacheStore;

impl JvmIndexCacheStore {
    /// Atomically writes `snapshot` to `~/.cache/kova/workspaces/<hash>/jvm_index.kidx`.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError::Cache`] or [`JvmIndexError::Io`] if encoding, directory
    /// creation, writing, syncing, or renaming fails.
    pub fn save_workspace_cache(
        paths: &KovaPaths,
        project_root: &Path,
        snapshot: &CachedWorkspaceSnapshot,
    ) -> Result<PathBuf, JvmIndexError> {
        let dir = paths
            .ensure_workspace_cache_dir(project_root)
            .map_err(|err| JvmIndexError::Cache {
                path: project_root.display().to_string(),
                reason: err.to_string(),
            })?;
        let target = dir.join("jvm_index.kidx");
        let bytes = encode_workspace_snapshot(snapshot)?;
        write_atomic(&target, &bytes)?;
        Ok(target)
    }

    /// Loads `~/.cache/kova/workspaces/<hash>/jvm_index.kidx` if present and valid.
    ///
    /// Missing, truncated, version-mismatched, or checksum-invalid files return
    /// `Ok(CacheLoadOutcome::Miss(...))` without failing.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError::Cache`] if the workspace cache path cannot be resolved,
    /// or [`JvmIndexError::Io`] on unexpected filesystem errors other than `NotFound`.
    pub fn load_workspace_cache(
        paths: &KovaPaths,
        project_root: &Path,
    ) -> Result<CacheLoadOutcome<CachedWorkspaceSnapshot>, JvmIndexError> {
        let target = JvmCacheLayout::workspace_index_file(paths, project_root)?;
        match read_bounded(&target)? {
            ReadOutcome::NotFound => Ok(CacheLoadOutcome::Miss(CacheMissReason::NotFound)),
            ReadOutcome::Oversized(len) => Ok(CacheLoadOutcome::Miss(
                CacheMissReason::CorruptedPayload(format!("cache file size {len} exceeds limit")),
            )),
            ReadOutcome::Bytes(bytes) => Ok(decode_workspace_snapshot(&bytes)),
        }
    }

    /// Atomically writes a shared `.jar`/`.jmod` shard to `~/.cache/kova/jvm_jars/<hash>.kjar`.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError::Cache`] or [`JvmIndexError::Io`] on failure.
    pub fn save_jar_shard(
        paths: &KovaPaths,
        shard: &CachedJarShard,
    ) -> Result<PathBuf, JvmIndexError> {
        let dir = JvmCacheLayout::shared_jars_dir(paths);
        fs::create_dir_all(&dir).map_err(|err| JvmIndexError::Io {
            path: dir.display().to_string(),
            message: err.to_string(),
        })?;
        let target = JvmCacheLayout::jar_shard_file(paths, &shard.artifact);
        let bytes = encode_jar_shard(shard)?;
        write_atomic(&target, &bytes)?;
        Ok(target)
    }

    /// Loads a shared `.jar`/`.jmod` shard and verifies its [`ArtifactFingerprint`].
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError::Io`] on unexpected filesystem errors other than `NotFound`.
    pub fn load_jar_shard(
        paths: &KovaPaths,
        fingerprint: &ArtifactFingerprint,
    ) -> Result<CacheLoadOutcome<CachedJarShard>, JvmIndexError> {
        let target = JvmCacheLayout::jar_shard_file(paths, fingerprint);
        match read_bounded(&target)? {
            ReadOutcome::NotFound => Ok(CacheLoadOutcome::Miss(CacheMissReason::NotFound)),
            ReadOutcome::Oversized(len) => Ok(CacheLoadOutcome::Miss(
                CacheMissReason::CorruptedPayload(format!("shard file size {len} exceeds limit")),
            )),
            ReadOutcome::Bytes(bytes) => match decode_jar_shard(&bytes) {
                CacheLoadOutcome::Hit(shard) if shard.artifact == *fingerprint => {
                    Ok(CacheLoadOutcome::Hit(shard))
                }
                CacheLoadOutcome::Hit(_) => {
                    Ok(CacheLoadOutcome::Miss(CacheMissReason::CorruptedPayload(
                        String::from("jar shard artifact fingerprint mismatch"),
                    )))
                }
                CacheLoadOutcome::Miss(reason) => Ok(CacheLoadOutcome::Miss(reason)),
            },
        }
    }

    /// Removes `jvm_index.kidx` for `project_root` if it exists.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError::Cache`] or [`JvmIndexError::Io`] if removal fails for a reason
    /// other than `NotFound`.
    pub fn clear_workspace_cache(
        paths: &KovaPaths,
        project_root: &Path,
    ) -> Result<(), JvmIndexError> {
        let target = JvmCacheLayout::workspace_index_file(paths, project_root)?;
        match fs::remove_file(&target) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(JvmIndexError::Io {
                path: target.display().to_string(),
                message: err.to_string(),
            }),
        }
    }
}

enum ReadOutcome {
    NotFound,
    Oversized(u64),
    Bytes(Vec<u8>),
}

fn read_bounded(path: &Path) -> Result<ReadOutcome, JvmIndexError> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(ReadOutcome::NotFound),
        Err(err) => {
            return Err(JvmIndexError::Io {
                path: path.display().to_string(),
                message: err.to_string(),
            });
        }
    };

    let max_total = (HEADER_SIZE as u64).saturating_add(MAX_CACHE_FILE_BYTES);
    if meta.len() > max_total {
        return Ok(ReadOutcome::Oversized(meta.len()));
    }

    match fs::read(path) {
        Ok(bytes) => Ok(ReadOutcome::Bytes(bytes)),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(ReadOutcome::NotFound),
        Err(err) => Err(JvmIndexError::Io {
            path: path.display().to_string(),
            message: err.to_string(),
        }),
    }
}

fn write_atomic(target: &Path, bytes: &[u8]) -> Result<(), JvmIndexError> {
    let parent = target.parent().ok_or_else(|| JvmIndexError::Cache {
        path: target.display().to_string(),
        reason: String::from("target path has no parent directory"),
    })?;
    let file_name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("cache");
    let pid = std::process::id();
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.tmp.{pid}.{seq}"));

    let write_res = (|| -> std::io::Result<()> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp_path, target)?;
        Ok(())
    })();

    if let Err(err) = write_res {
        drop(fs::remove_file(&tmp_path));
        return Err(JvmIndexError::Io {
            path: target.display().to_string(),
            message: err.to_string(),
        });
    }

    Ok(())
}
