//! Incremental `rayon`-backed cache revalidation for [`JvmWorkspaceIndex`].

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use kova_config::KovaPaths;
use rayon::prelude::*;

use crate::bytecode::scan_jar_archive;
use crate::cache::codec::{
    ArtifactFingerprint, CacheLoadOutcome, CachedJarShard, CachedSourceEntry,
    CachedWorkspaceSnapshot, FileFingerprint, ScanConfigFingerprint,
};
use crate::cache::store::JvmIndexCacheStore;
use crate::classpath::ClasspathEntry;
use crate::discovery::{DiscoveredSourceFile, collect_project_sources};
use crate::error::JvmIndexError;
use crate::indexer::{IndexCancelToken, JvmWorkspaceIndex, WorkspaceIndexer};
use crate::model::ParsedClassUnit;
use crate::source::parse_source_file;

/// Difference between a cached workspace snapshot and the current filesystem state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexDelta {
    /// Project source files that were added or whose size/mtime changed.
    pub added_or_modified_sources: Vec<DiscoveredSourceFile>,
    /// Relative paths of project source files that were removed.
    pub removed_sources: Vec<PathBuf>,
    /// Classpath `.jar`/`.jmod` entries that were added or whose size/mtime changed.
    pub added_or_modified_jars: Vec<ClasspathEntry>,
    /// Paths of classpath `.jar`/`.jmod` entries that were removed.
    pub removed_jars: Vec<PathBuf>,
    /// Number of project source files unchanged since the cached snapshot.
    pub unchanged_sources: usize,
    /// Number of classpath archives unchanged since the cached snapshot.
    pub unchanged_jars: usize,
}

impl IndexDelta {
    /// Returns `true` when no source files or classpath archives were added, modified, or removed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added_or_modified_sources.is_empty()
            && self.removed_sources.is_empty()
            && self.added_or_modified_jars.is_empty()
            && self.removed_jars.is_empty()
    }
}

/// Result of [`load_and_revalidate`].
#[derive(Debug, Clone)]
pub struct RevalidatedIndexResult {
    /// Reconstructed and up-to-date workspace index.
    pub index: JvmWorkspaceIndex,
    /// `true` if `jvm_index.kidx` was loaded and matched the current scanner config.
    pub cache_hit: bool,
    /// Delta computed against the cached snapshot (or full scan delta on cache miss).
    pub delta: IndexDelta,
    /// Number of `.jar`/`.jmod` archives satisfied from shared `~/.cache/kova/jvm_jars/` shards.
    pub jar_shard_hits: usize,
    /// Number of `.jar`/`.jmod` archives that required bytecode scanning.
    pub jar_shard_misses: usize,
}

/// Computes the incremental delta between `cached` and the current sources/classpath.
#[must_use]
pub fn compute_index_delta(
    cached: &CachedWorkspaceSnapshot,
    current_sources: &[DiscoveredSourceFile],
    current_classpath: &[ClasspathEntry],
) -> IndexDelta {
    let cached_sources: HashMap<&Path, FileFingerprint> = cached
        .sources
        .iter()
        .map(|s| (s.rel_path.as_path(), s.fingerprint))
        .collect();
    let current_source_set: HashSet<&Path> = current_sources
        .iter()
        .map(|s| s.rel_path.as_path())
        .collect();

    let mut added_or_modified_sources = Vec::new();
    let mut unchanged_sources = 0usize;
    for src in current_sources {
        let fp = FileFingerprint {
            size_bytes: src.file_size,
            modified_unix_ns: src.modified_unix_ns,
        };
        if cached_sources.get(src.rel_path.as_path()) == Some(&fp) {
            unchanged_sources += 1;
        } else {
            added_or_modified_sources.push(src.clone());
        }
    }

    let mut removed_sources: Vec<PathBuf> = cached
        .sources
        .iter()
        .filter(|s| !current_source_set.contains(s.rel_path.as_path()))
        .map(|s| s.rel_path.clone())
        .collect();
    removed_sources.sort();

    let cached_jars: HashMap<&Path, FileFingerprint> = cached
        .classpath
        .iter()
        .map(|j| (j.path.as_path(), j.fingerprint))
        .collect();
    let current_jar_set: HashSet<&Path> =
        current_classpath.iter().map(|j| j.path.as_path()).collect();

    let mut added_or_modified_jars = Vec::new();
    let mut unchanged_jars = 0usize;
    for jar in current_classpath {
        let fp = FileFingerprint {
            size_bytes: jar.size_bytes,
            modified_unix_ns: jar.modified_unix_ns,
        };
        if cached_jars.get(jar.path.as_path()) == Some(&fp) {
            unchanged_jars += 1;
        } else {
            added_or_modified_jars.push(jar.clone());
        }
    }

    let mut removed_jars: Vec<PathBuf> = cached
        .classpath
        .iter()
        .filter(|j| !current_jar_set.contains(j.path.as_path()))
        .map(|j| j.path.clone())
        .collect();
    removed_jars.sort();

    IndexDelta {
        added_or_modified_sources,
        removed_sources,
        added_or_modified_jars,
        removed_jars,
        unchanged_sources,
        unchanged_jars,
    }
}

/// Loads `jvm_index.kidx` and shared `.kjar` shards, revalidates only changed files/archives
/// on `rayon`, and atomically persists the updated cache.
///
/// # Errors
///
/// Returns [`JvmIndexError::Cancelled`] if `cancel` is triggered, or [`JvmIndexError`] on
/// fatal discovery/parsing/I/O failures.
pub fn load_and_revalidate(
    paths: &KovaPaths,
    project_root: &Path,
    indexer: &WorkspaceIndexer,
    current_classpath: &[ClasspathEntry],
    cancel: &IndexCancelToken,
) -> Result<RevalidatedIndexResult, JvmIndexError> {
    if cancel.is_cancelled() {
        return Err(JvmIndexError::Cancelled);
    }

    let cfg = indexer.config();
    let config_fp = ScanConfigFingerprint::from(cfg);
    let current_sources = collect_project_sources(project_root, &cfg.discovery)?;

    let cached_snapshot = match JvmIndexCacheStore::load_workspace_cache(paths, project_root)? {
        CacheLoadOutcome::Hit(snap) if snap.config == config_fp => Some(snap),
        _ => None,
    };

    let (cache_hit, delta, mut cached_source_map) = if let Some(snap) = cached_snapshot {
        let delta = compute_index_delta(&snap, &current_sources, current_classpath);
        let unchanged_set: HashSet<PathBuf> = current_sources
            .iter()
            .filter(|s| {
                !delta
                    .added_or_modified_sources
                    .iter()
                    .any(|m| m.rel_path == s.rel_path)
            })
            .map(|s| s.rel_path.clone())
            .collect();
        let map: HashMap<PathBuf, CachedSourceEntry> = snap
            .sources
            .into_iter()
            .filter(|s| unchanged_set.contains(&s.rel_path))
            .map(|s| (s.rel_path.clone(), s))
            .collect();
        (true, delta, map)
    } else {
        let delta = IndexDelta {
            added_or_modified_sources: current_sources.clone(),
            removed_sources: Vec::new(),
            added_or_modified_jars: current_classpath.to_vec(),
            removed_jars: Vec::new(),
            unchanged_sources: 0,
            unchanged_jars: 0,
        };
        (false, delta, HashMap::new())
    };

    // Parse only added or modified project sources in parallel.
    let scanned_sources: Vec<Result<CachedSourceEntry, JvmIndexError>> = delta
        .added_or_modified_sources
        .par_iter()
        .map(|src| {
            if cancel.is_cancelled() {
                return Err(JvmIndexError::Cancelled);
            }
            let source_text =
                std::fs::read_to_string(&src.abs_path).map_err(|err| JvmIndexError::Io {
                    path: src.abs_path.display().to_string(),
                    message: err.to_string(),
                })?;
            let units = parse_source_file(&src.rel_path, &source_text, &cfg.source)?;
            Ok(CachedSourceEntry {
                rel_path: src.rel_path.clone(),
                fingerprint: FileFingerprint {
                    size_bytes: src.file_size,
                    modified_unix_ns: src.modified_unix_ns,
                },
                units,
            })
        })
        .collect();

    for res in scanned_sources {
        let entry = res?;
        cached_source_map.insert(entry.rel_path.clone(), entry);
    }

    // Resolve classpath archives via shared .kjar shards or scan on miss.
    let (jar_shard_hits, jar_shard_misses, classpath_units, classpath_fps) =
        revalidate_classpath_jars(paths, current_classpath, cfg, config_fp, cancel)?;

    let mut source_units_by_file = HashMap::with_capacity(cached_source_map.len());
    let mut snapshot_sources: Vec<CachedSourceEntry> = cached_source_map.into_values().collect();
    snapshot_sources.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    for entry in &snapshot_sources {
        source_units_by_file.insert(entry.rel_path.clone(), entry.units.clone());
    }

    if !cache_hit || !delta.is_empty() || jar_shard_misses > 0 {
        let snapshot = CachedWorkspaceSnapshot {
            config: config_fp,
            sources: snapshot_sources,
            classpath: classpath_fps,
        };
        JvmIndexCacheStore::save_workspace_cache(paths, project_root, &snapshot)?;
    }

    let index = JvmWorkspaceIndex::from_parts(
        source_units_by_file,
        classpath_units,
        current_sources.len(),
        current_classpath.len(),
    );

    Ok(RevalidatedIndexResult {
        index,
        cache_hit,
        delta,
        jar_shard_hits,
        jar_shard_misses,
    })
}

struct JarResolveOutcome {
    artifact: ArtifactFingerprint,
    units: Vec<ParsedClassUnit>,
    was_hit: bool,
}

fn revalidate_classpath_jars(
    paths: &KovaPaths,
    current_classpath: &[ClasspathEntry],
    cfg: &crate::indexer::IndexerConfig,
    config_fp: ScanConfigFingerprint,
    cancel: &IndexCancelToken,
) -> Result<(usize, usize, Vec<ParsedClassUnit>, Vec<ArtifactFingerprint>), JvmIndexError> {
    let jar_results: Vec<Result<JarResolveOutcome, JvmIndexError>> = current_classpath
        .par_iter()
        .map(|cp| {
            if cancel.is_cancelled() {
                return Err(JvmIndexError::Cancelled);
            }
            let artifact = ArtifactFingerprint {
                path: cp.path.clone(),
                fingerprint: FileFingerprint {
                    size_bytes: cp.size_bytes,
                    modified_unix_ns: cp.modified_unix_ns,
                },
            };
            if let CacheLoadOutcome::Hit(shard) =
                JvmIndexCacheStore::load_jar_shard(paths, &artifact)?
            {
                if shard.config == config_fp {
                    return Ok(JarResolveOutcome {
                        artifact,
                        units: shard.units,
                        was_hit: true,
                    });
                }
            }
            let scan = scan_jar_archive(&cp.path, &cfg.bytecode)?;
            let shard = CachedJarShard {
                artifact: artifact.clone(),
                config: config_fp,
                units: scan.units.clone(),
            };
            JvmIndexCacheStore::save_jar_shard(paths, &shard)?;
            Ok(JarResolveOutcome {
                artifact,
                units: scan.units,
                was_hit: false,
            })
        })
        .collect();

    let mut jar_shard_hits = 0usize;
    let mut jar_shard_misses = 0usize;
    let mut classpath_units = Vec::new();
    let mut classpath_fps = Vec::with_capacity(current_classpath.len());

    for res in jar_results {
        let outcome = res?;
        if outcome.was_hit {
            jar_shard_hits += 1;
        } else {
            jar_shard_misses += 1;
        }
        classpath_fps.push(outcome.artifact);
        classpath_units.extend(outcome.units);
    }

    Ok((
        jar_shard_hits,
        jar_shard_misses,
        classpath_units,
        classpath_fps,
    ))
}
