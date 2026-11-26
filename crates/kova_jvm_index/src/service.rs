//! Non-blocking background indexing service with two-phase readiness and budget reporting.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use kova_config::KovaPaths;
use kova_core::perf::{
    BudgetKind, BudgetReport, budgets::GO_TO_CLASS_READY_MAX, check_duration_budget,
    check_rss_budget, current_process_rss_bytes,
};

use crate::cache::{JvmCacheLayout, load_and_revalidate};
use crate::classpath::{ClasspathDiscoveryConfig, discover_classpath};
use crate::error::JvmIndexError;
use crate::indexer::{IndexCancelToken, IndexerConfig, JvmWorkspaceIndex, WorkspaceIndexer};
use crate::snapshot::JvmIndexSnapshot;

/// Readiness state machine for [`JvmIndexService`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexPhase {
    /// Service is idle before any project has been opened.
    Idle,
    /// Loading persisted binary cache from disk.
    LoadingCache,
    /// Actively scanning project sources and classpath archives.
    Indexing {
        /// Number of project source files scanned so far.
        scanned_sources: usize,
        /// Total number of discovered project source files.
        total_sources: usize,
        /// Number of `.jar`/`.jmod` archives scanned so far.
        scanned_archives: usize,
        /// Total number of discovered `.jar`/`.jmod` archives.
        total_archives: usize,
    },
    /// Index snapshot is ready for *Go to Class* queries.
    Ready {
        /// `true` if the snapshot was served from a warm binary cache.
        warm_cache: bool,
        /// Wall-clock duration from project open until readiness.
        ready_elapsed: Duration,
        /// Total number of indexed types.
        type_count: usize,
        /// Total number of indexed members.
        member_count: usize,
    },
    /// Indexing was cancelled before completion.
    Cancelled,
    /// Indexing failed with an error message.
    Failed(String),
}

/// Performance budget report captured when [`JvmIndexService`] reaches [`IndexPhase::Ready`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexReadyMetrics {
    /// Wall-clock duration from project open until readiness.
    pub elapsed: Duration,
    /// Evaluation against [`GO_TO_CLASS_READY_MAX`] (`< 1.5 s`).
    pub duration_report: BudgetReport,
    /// Evaluation against [`kova_core::perf::budgets::MAX_PROCESS_RSS_BYTES`] (`< 150 MiB`),
    /// if OS memory statistics are available.
    pub rss_report: Option<BudgetReport>,
    /// `true` if served from warm binary cache.
    pub warm_cache: bool,
    /// Number of indexed types.
    pub type_count: usize,
    /// Number of indexed members.
    pub member_count: usize,
}

/// Callback invoked on the indexing thread each time a new snapshot is published.
///
/// It receives the phase that was current when the snapshot was published: an
/// [`IndexPhase::Indexing`] phase for the preliminary sources-only snapshot and
/// [`IndexPhase::Ready`] for the complete one.
pub type IndexUpdateCallback = Arc<dyn Fn(&IndexPhase) + Send + Sync>;

/// Handle to an active background indexing job spawned by [`JvmIndexService::start_project_indexing`].
pub struct IndexJobHandle {
    job_id: u64,
    cancel_token: IndexCancelToken,
    join_handle: Option<JoinHandle<()>>,
}

impl fmt::Debug for IndexJobHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexJobHandle")
            .field("job_id", &self.job_id)
            .field("cancelled", &self.cancel_token.is_cancelled())
            .field("running", &self.join_handle.is_some())
            .finish()
    }
}

impl IndexJobHandle {
    /// Returns the monotonic identifier of this job.
    #[must_use]
    pub const fn job_id(&self) -> u64 {
        self.job_id
    }

    /// Signals cooperative cancellation to the background indexing job.
    pub fn cancel(&self) {
        self.cancel_token.cancel();
    }

    /// Waits for the background indexing thread to finish.
    ///
    /// Returns `false` if the thread could not be spawned or panicked.
    #[must_use = "`false` means the indexing thread was not spawned or panicked"]
    pub fn join(mut self) -> bool {
        self.join_handle
            .take()
            .is_some_and(|handle| handle.join().is_ok())
    }
}

/// Mutable service state; every write is gated on `active_job` so a superseded
/// job can never overwrite the results of a newer one.
#[derive(Debug)]
struct ServiceState {
    active_job: u64,
    active_cancel: Option<IndexCancelToken>,
    workspace_index: Option<JvmWorkspaceIndex>,
    phase: IndexPhase,
    last_metrics: Option<IndexReadyMetrics>,
}

/// Thread-safe JVM index service for 120 FPS UI integration.
///
/// [`Self::snapshot`] only takes a read lock long enough to clone an [`Arc`]; all
/// indexing work runs outside of it.
pub struct JvmIndexService {
    indexer: WorkspaceIndexer,
    snapshot: RwLock<Arc<JvmIndexSnapshot>>,
    state: Mutex<ServiceState>,
}

impl fmt::Debug for JvmIndexService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JvmIndexService")
            .field("phase", &self.phase())
            .field("type_count", &self.snapshot().type_count())
            .finish_non_exhaustive()
    }
}

impl JvmIndexService {
    /// Creates a new [`JvmIndexService`] with the given [`IndexerConfig`].
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError`] if the underlying `rayon` thread pool cannot be initialized.
    pub fn new(indexer_config: IndexerConfig) -> Result<Self, JvmIndexError> {
        let indexer = WorkspaceIndexer::new(indexer_config)?;
        Ok(Self {
            indexer,
            snapshot: RwLock::new(Arc::new(JvmIndexSnapshot::empty())),
            state: Mutex::new(ServiceState {
                active_job: 0,
                active_cancel: None,
                workspace_index: None,
                phase: IndexPhase::Idle,
                last_metrics: None,
            }),
        })
    }

    /// Returns the current immutable [`JvmIndexSnapshot`] without performing any I/O.
    #[must_use]
    pub fn snapshot(&self) -> Arc<JvmIndexSnapshot> {
        match self.snapshot.read() {
            Ok(guard) => Arc::clone(&guard),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Returns the current [`IndexPhase`].
    #[must_use]
    pub fn phase(&self) -> IndexPhase {
        self.lock_state().phase.clone()
    }

    /// Returns the most recent [`IndexReadyMetrics`], if indexing has completed at least once.
    #[must_use]
    pub fn last_metrics(&self) -> Option<IndexReadyMetrics> {
        self.lock_state().last_metrics.clone()
    }

    /// Synchronously opens a project, loading from warm binary cache if valid or
    /// running a full parallel scan otherwise, and records [`IndexReadyMetrics`].
    ///
    /// Supersedes (and cancels) any running background job.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError`] if discovery, parsing, or indexing fails, or
    /// [`JvmIndexError::Cancelled`] if a newer job superseded this one.
    pub fn open_project_sync(
        &self,
        project_root: &Path,
        paths: &KovaPaths,
        classpath_cfg: &ClasspathDiscoveryConfig,
    ) -> Result<IndexReadyMetrics, JvmIndexError> {
        let cancel = IndexCancelToken::new();
        let job = self.begin_job(&cancel);
        self.run_job(
            &JobSpec {
                job,
                project_root,
                paths,
                classpath_cfg,
                cancel: &cancel,
                two_phase: false,
            },
            None,
        )
    }

    /// Launches non-blocking two-phase project indexing on a background thread.
    ///
    /// Without a warm cache, a sources-only snapshot is published first (phase
    /// stays [`IndexPhase::Indexing`]); the complete snapshot with `.jar`/`.jmod`
    /// classes follows as [`IndexPhase::Ready`]. With a warm cache the revalidated
    /// cache is published directly. `on_update` runs after each publication.
    ///
    /// Cancels any previously active job; a superseded job never overwrites the
    /// snapshot, phase or metrics of a newer one.
    pub fn start_project_indexing(
        self: &Arc<Self>,
        project_root: PathBuf,
        paths: KovaPaths,
        classpath_cfg: ClasspathDiscoveryConfig,
        on_update: Option<IndexUpdateCallback>,
    ) -> IndexJobHandle {
        let cancel_token = IndexCancelToken::new();
        let job = self.begin_job(&cancel_token);
        let service = Arc::clone(self);
        let thread_token = cancel_token.clone();

        let spawned = std::thread::Builder::new()
            .name(format!("kova-jvm-index-job-{job}"))
            .spawn(move || {
                // Errors are already recorded in `phase()`.
                let _outcome = service.run_job(
                    &JobSpec {
                        job,
                        project_root: &project_root,
                        paths: &paths,
                        classpath_cfg: &classpath_cfg,
                        cancel: &thread_token,
                        two_phase: true,
                    },
                    on_update.as_ref(),
                );
            });

        let join_handle = match spawned {
            Ok(handle) => Some(handle),
            Err(err) => {
                self.commit(job, None, |state| {
                    state.phase =
                        IndexPhase::Failed(format!("cannot spawn indexing thread: {err}"));
                });
                None
            }
        };

        IndexJobHandle {
            job_id: job,
            cancel_token,
            join_handle,
        }
    }

    /// Incrementally re-indexes a single modified `.java`/`.kt`/`.kts` file and
    /// atomically swaps the published [`JvmIndexSnapshot`].
    ///
    /// Search names of unchanged types are shared with the previous snapshot, so
    /// no library type is re-converted for `nucleo-matcher`.
    ///
    /// # Errors
    ///
    /// Returns [`JvmIndexError`] if parsing the modified source file fails.
    pub fn on_file_saved_or_changed(
        &self,
        _project_root: &Path,
        rel_path: &Path,
        content: &str,
    ) -> Result<(), JvmIndexError> {
        let mut state = self.lock_state();
        let index = state
            .workspace_index
            .get_or_insert_with(JvmWorkspaceIndex::default);
        index.reindex_source_file(rel_path, content, &self.indexer.config().source)?;
        let previous = self.snapshot();
        let snap = Arc::new(JvmIndexSnapshot::from_workspace_index_reusing(
            index, &previous,
        ));
        self.publish_snapshot(snap);
        Ok(())
    }

    /// Cancels the currently active background indexing job, if any.
    pub fn cancel_active_job(&self) {
        if let Some(token) = &self.lock_state().active_cancel {
            token.cancel();
        }
    }

    fn begin_job(&self, cancel: &IndexCancelToken) -> u64 {
        let mut state = self.lock_state();
        if let Some(previous) = state.active_cancel.replace(cancel.clone()) {
            previous.cancel();
        }
        state.active_job += 1;
        state.active_job
    }

    fn run_job(
        &self,
        spec: &JobSpec<'_>,
        on_update: Option<&IndexUpdateCallback>,
    ) -> Result<IndexReadyMetrics, JvmIndexError> {
        let result = self.run_job_inner(spec, on_update);
        if let Err(err) = &result {
            let phase = match err {
                JvmIndexError::Cancelled => IndexPhase::Cancelled,
                other => IndexPhase::Failed(other.to_string()),
            };
            self.commit(spec.job, None, |state| state.phase = phase);
        }
        result
    }

    fn run_job_inner(
        &self,
        spec: &JobSpec<'_>,
        on_update: Option<&IndexUpdateCallback>,
    ) -> Result<IndexReadyMetrics, JvmIndexError> {
        let start = Instant::now();
        let job = spec.job;
        let warm_cache_present =
            JvmCacheLayout::workspace_index_file(spec.paths, spec.project_root)
                .is_ok_and(|file| file.is_file());

        self.commit_or_cancel(job, spec.cancel, None, |state| {
            state.phase = IndexPhase::LoadingCache;
        })?;

        let classpath = discover_classpath(spec.project_root, spec.classpath_cfg)?;
        let total_archives = classpath.len();
        self.commit_or_cancel(job, spec.cancel, None, |state| {
            state.phase = IndexPhase::Indexing {
                scanned_sources: 0,
                total_sources: 0,
                scanned_archives: 0,
                total_archives,
            };
        })?;

        if spec.two_phase && !warm_cache_present {
            // Phase 1: project sources only, so Go to Class works before libraries are scanned.
            let sources_only = self
                .indexer
                .index_project(spec.project_root, &[], spec.cancel)?;
            let scanned = sources_only.stats().source_files_scanned;
            let snap = Arc::new(JvmIndexSnapshot::from_workspace_index_reusing(
                &sources_only,
                &self.snapshot(),
            ));
            let phase = IndexPhase::Indexing {
                scanned_sources: scanned,
                total_sources: scanned,
                scanned_archives: 0,
                total_archives,
            };
            let notify_phase = phase.clone();
            self.commit_or_cancel(job, spec.cancel, Some(snap), |state| {
                state.workspace_index = Some(sources_only);
                state.phase = phase;
            })?;
            if let Some(callback) = on_update {
                callback(&notify_phase);
            }
        }

        // Phase 2: warm cache revalidation or full parallel scan of sources and archives.
        let revalidated = load_and_revalidate(
            spec.paths,
            spec.project_root,
            &self.indexer,
            &classpath,
            spec.cancel,
        )?;
        let warm_cache = revalidated.cache_hit;
        let snap = Arc::new(JvmIndexSnapshot::from_workspace_index_reusing(
            &revalidated.index,
            &self.snapshot(),
        ));
        let type_count = snap.type_count();
        let member_count = snap.member_count();
        let elapsed = start.elapsed();
        let metrics = build_metrics(elapsed, warm_cache, type_count, member_count);
        let phase = IndexPhase::Ready {
            warm_cache,
            ready_elapsed: elapsed,
            type_count,
            member_count,
        };
        let notify_phase = phase.clone();
        let stored_metrics = metrics.clone();
        self.commit_or_cancel(job, spec.cancel, Some(snap), |state| {
            state.workspace_index = Some(revalidated.index);
            state.last_metrics = Some(stored_metrics);
            state.phase = phase;
        })?;
        if let Some(callback) = on_update {
            callback(&notify_phase);
        }
        Ok(metrics)
    }

    /// Applies `update` (and publishes `snapshot`) only if `job` is still the
    /// active job and has not been cancelled.
    fn commit_or_cancel(
        &self,
        job: u64,
        cancel: &IndexCancelToken,
        snapshot: Option<Arc<JvmIndexSnapshot>>,
        update: impl FnOnce(&mut ServiceState),
    ) -> Result<(), JvmIndexError> {
        if cancel.is_cancelled() || !self.commit(job, snapshot, update) {
            return Err(JvmIndexError::Cancelled);
        }
        Ok(())
    }

    /// Applies `update` and publishes `snapshot` atomically under the state lock,
    /// only if `job` is still the active job. Returns `false` for a superseded job.
    fn commit(
        &self,
        job: u64,
        snapshot: Option<Arc<JvmIndexSnapshot>>,
        update: impl FnOnce(&mut ServiceState),
    ) -> bool {
        let mut state = self.lock_state();
        if state.active_job != job {
            return false;
        }
        update(&mut state);
        if let Some(snap) = snapshot {
            self.publish_snapshot(snap);
        }
        true
    }

    fn lock_state(&self) -> MutexGuard<'_, ServiceState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn publish_snapshot(&self, snap: Arc<JvmIndexSnapshot>) {
        match self.snapshot.write() {
            Ok(mut guard) => *guard = snap,
            Err(poisoned) => *poisoned.into_inner() = snap,
        }
    }
}

/// Inputs of one indexing run.
struct JobSpec<'a> {
    job: u64,
    project_root: &'a Path,
    paths: &'a KovaPaths,
    classpath_cfg: &'a ClasspathDiscoveryConfig,
    cancel: &'a IndexCancelToken,
    two_phase: bool,
}

fn build_metrics(
    elapsed: Duration,
    warm_cache: bool,
    type_count: usize,
    member_count: usize,
) -> IndexReadyMetrics {
    let duration_report =
        check_duration_budget(BudgetKind::GoToClassReady, elapsed, GO_TO_CLASS_READY_MAX);
    let rss_report = current_process_rss_bytes().ok().map(check_rss_budget);

    IndexReadyMetrics {
        elapsed,
        duration_report,
        rss_report,
        warm_cache,
        type_count,
        member_count,
    }
}
