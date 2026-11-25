//! Parallel `rayon`-backed workspace indexer for JVM sources and `.jar`/`.jmod` archives.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;

use crate::bytecode::{BytecodeScanOptions, scan_jar_archive};
use crate::classpath::ClasspathEntry;
use crate::discovery::{SourceDiscoveryOptions, collect_project_sources};
use crate::error::JvmIndexError;
use crate::model::{JvmSourceOrigin, ParsedClassUnit};
use crate::source::{SourceScanOptions, parse_source_file};

/// Configuration for [`WorkspaceIndexer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexerConfig {
    /// Number of worker threads in the dedicated `rayon` pool.
    pub worker_threads: usize,
    /// Options for `.jar`/`.jmod` bytecode scanning.
    pub bytecode: BytecodeScanOptions,
    /// Options for `.java`/`.kt`/`.kts` source scanning.
    pub source: SourceScanOptions,
    /// Options for `.gitignore`-aware source file discovery.
    pub discovery: SourceDiscoveryOptions,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, NonZeroUsize::get);
        Self {
            worker_threads: (cores / 2).clamp(1, 6),
            bytecode: BytecodeScanOptions::default(),
            source: SourceScanOptions::default(),
            discovery: SourceDiscoveryOptions::default(),
        }
    }
}

/// Cooperative cancellation token checked before each file or archive task.
#[derive(Debug, Clone, Default)]
pub struct IndexCancelToken {
    cancelled: Arc<AtomicBool>,
}

impl IndexCancelToken {
    /// Creates a new uncancelled token.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Signals cancellation to any active indexing run sharing this token.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns `true` if [`Self::cancel`] has been called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Summary statistics for a completed [`JvmWorkspaceIndex`] build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexStats {
    /// Number of project source files parsed.
    pub source_files_scanned: usize,
    /// Number of `.jar`/`.jmod` archives scanned.
    pub archives_scanned: usize,
    /// Total number of deduplicated types in the index.
    pub total_types: usize,
}

/// Aggregated, deterministically ordered JVM symbol index for a workspace.
///
/// Maintains project source units separately from classpath units so that
/// [`Self::reindex_source_file`] and [`Self::remove_source_file`] can update
/// project types in $O(\text{symbols in file})$ while preserving `ProjectSource > JarEntry`
/// precedence.
#[derive(Debug, Clone, Default)]
pub struct JvmWorkspaceIndex {
    source_units_by_file: HashMap<PathBuf, Vec<ParsedClassUnit>>,
    classpath_units: Vec<ParsedClassUnit>,
    merged_units: Vec<ParsedClassUnit>,
    fqcn_index: HashMap<String, usize>,
    stats: IndexStats,
}

impl JvmWorkspaceIndex {
    fn from_parts(
        source_units_by_file: HashMap<PathBuf, Vec<ParsedClassUnit>>,
        classpath_units: Vec<ParsedClassUnit>,
        source_files_scanned: usize,
        archives_scanned: usize,
    ) -> Self {
        let mut index = Self {
            source_units_by_file,
            classpath_units,
            merged_units: Vec::new(),
            fqcn_index: HashMap::new(),
            stats: IndexStats {
                source_files_scanned,
                archives_scanned,
                total_types: 0,
            },
        };
        index.rebuild_merged();
        index
    }

    fn rebuild_merged(&mut self) {
        let mut by_fqcn: HashMap<String, ParsedClassUnit> = HashMap::new();

        for unit in &self.classpath_units {
            by_fqcn
                .entry(unit.ty.fqcn.clone())
                .or_insert_with(|| unit.clone());
        }

        // ProjectSource always takes precedence over JarEntry/ClassFile.
        for units in self.source_units_by_file.values() {
            for unit in units {
                by_fqcn.insert(unit.ty.fqcn.clone(), unit.clone());
            }
        }

        let mut merged: Vec<ParsedClassUnit> = by_fqcn.into_values().collect();
        merged.sort_by(|a, b| {
            a.ty.fqcn
                .cmp(&b.ty.fqcn)
                .then_with(|| origin_rank(&a.ty.origin).cmp(&origin_rank(&b.ty.origin)))
        });

        let mut fqcn_index = HashMap::with_capacity(merged.len());
        for (idx, unit) in merged.iter().enumerate() {
            fqcn_index.insert(unit.ty.fqcn.clone(), idx);
        }

        self.stats.total_types = merged.len();
        self.merged_units = merged;
        self.fqcn_index = fqcn_index;
    }

    /// Returns all indexed types in deterministic FQCN order.
    #[must_use]
    pub fn types(&self) -> &[ParsedClassUnit] {
        &self.merged_units
    }

    /// Returns summary statistics for this index.
    #[must_use]
    pub const fn stats(&self) -> &IndexStats {
        &self.stats
    }

    /// Looks up an indexed type by its fully qualified class name.
    #[must_use]
    pub fn find_by_fqcn(&self, fqcn: &str) -> Option<&ParsedClassUnit> {
        self.fqcn_index
            .get(fqcn)
            .and_then(|&idx| self.merged_units.get(idx))
    }

    /// Incrementally re-parses a single project source file and updates the index.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError`] if parsing fails.
    pub fn reindex_source_file(
        &mut self,
        rel_path: &Path,
        source: &str,
        options: &SourceScanOptions,
    ) -> Result<(), JvmIndexError> {
        let units = parse_source_file(rel_path, source, options)?;
        if units.is_empty() {
            self.source_units_by_file.remove(rel_path);
        } else {
            self.source_units_by_file
                .insert(rel_path.to_path_buf(), units);
        }
        self.stats.source_files_scanned = self.source_units_by_file.len();
        self.rebuild_merged();
        Ok(())
    }

    /// Removes all symbols originating from `rel_path`, restoring any shadowed classpath types.
    pub fn remove_source_file(&mut self, rel_path: &Path) {
        self.source_units_by_file.remove(rel_path);
        self.stats.source_files_scanned = self.source_units_by_file.len();
        self.rebuild_merged();
    }
}

const fn origin_rank(origin: &JvmSourceOrigin) -> u8 {
    match origin {
        JvmSourceOrigin::ProjectSource { .. } => 0,
        JvmSourceOrigin::ClassFile { .. } => 1,
        JvmSourceOrigin::JarEntry { .. } | JvmSourceOrigin::JmodEntry { .. } => 2,
    }
}

/// Multi-threaded workspace indexer running on a bounded `rayon::ThreadPool`.
#[derive(Debug)]
pub struct WorkspaceIndexer {
    config: IndexerConfig,
    pool: rayon::ThreadPool,
}

impl WorkspaceIndexer {
    /// Creates a new [`WorkspaceIndexer`] with a dedicated `rayon` thread pool.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError::Io`] if the `rayon` thread pool cannot be constructed.
    pub fn new(config: IndexerConfig) -> Result<Self, JvmIndexError> {
        let threads = config.worker_threads.max(1);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("kova-jvm-index-{i}"))
            .build()
            .map_err(|err| JvmIndexError::Io {
                path: String::from("<rayon-thread-pool>"),
                message: err.to_string(),
            })?;
        Ok(Self { config, pool })
    }

    /// Indexes all project sources under `project_root` and all `classpath` archives in parallel.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError::Cancelled`] if `cancel` is triggered, or [`JvmIndexError`]
    /// on fatal discovery/archive errors.
    pub fn index_project(
        &self,
        project_root: &Path,
        classpath: &[ClasspathEntry],
        cancel: &IndexCancelToken,
    ) -> Result<JvmWorkspaceIndex, JvmIndexError> {
        if cancel.is_cancelled() {
            return Err(JvmIndexError::Cancelled);
        }

        let sources = collect_project_sources(project_root, &self.config.discovery)?;
        let source_opts = self.config.source;
        let bytecode_opts = &self.config.bytecode;

        let (source_res, archive_res) = self.pool.install(|| {
            rayon::join(
                || {
                    sources
                        .par_iter()
                        .map(
                            |file| -> Result<(PathBuf, Vec<ParsedClassUnit>), JvmIndexError> {
                                if cancel.is_cancelled() {
                                    return Err(JvmIndexError::Cancelled);
                                }
                                let content =
                                    std::fs::read_to_string(&file.abs_path).map_err(|err| {
                                        JvmIndexError::Io {
                                            path: file.abs_path.display().to_string(),
                                            message: err.to_string(),
                                        }
                                    })?;
                                let units =
                                    parse_source_file(&file.rel_path, &content, &source_opts)?;
                                Ok((file.rel_path.clone(), units))
                            },
                        )
                        .collect::<Result<Vec<_>, _>>()
                },
                || {
                    classpath
                        .par_iter()
                        .map(|entry| -> Result<Vec<ParsedClassUnit>, JvmIndexError> {
                            if cancel.is_cancelled() {
                                return Err(JvmIndexError::Cancelled);
                            }
                            let scanned = scan_jar_archive(&entry.path, bytecode_opts)?;
                            Ok(scanned.units)
                        })
                        .collect::<Result<Vec<_>, _>>()
                },
            )
        });

        let source_pairs = source_res?;
        let archive_batches = archive_res?;

        let source_files_scanned = source_pairs.len();
        let archives_scanned = archive_batches.len();

        let mut source_units_by_file = HashMap::with_capacity(source_files_scanned);
        for (rel_path, units) in source_pairs {
            if !units.is_empty() {
                source_units_by_file.insert(rel_path, units);
            }
        }

        let classpath_units: Vec<ParsedClassUnit> = archive_batches.into_iter().flatten().collect();

        Ok(JvmWorkspaceIndex::from_parts(
            source_units_by_file,
            classpath_units,
            source_files_scanned,
            archives_scanned,
        ))
    }
}
