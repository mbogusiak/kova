//! Incremental Tree-sitter parsing engine on a dedicated `rayon` worker pool.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use kova_buffer::{BufferId, BufferSnapshot, BufferVersion, EditLog, VersionChanges};
use kova_theme::SyntaxLanguage;

use crate::error::SyntaxError;
use crate::input::parse_rope_with;
use crate::registry::{LanguageEntry, LanguageRegistry};
use crate::tree::{SyntaxEdit, SyntaxTree};

/// Default per-job parse timeout (`250 ms`).
pub const DEFAULT_PARSE_TIMEOUT: Duration = Duration::from_millis(250);

/// Maximum default number of `rayon` syntax worker threads to protect the `< 150 MiB` RSS budget.
const MAX_DEFAULT_WORKER_THREADS: usize = 4;

thread_local! {
    static THREAD_LOCAL_PARSER: RefCell<Option<tree_sitter::Parser>> = const { RefCell::new(None) };
}

/// Configuration for [`SyntaxEngine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxEngineConfig {
    /// Number of dedicated `rayon` worker threads (`1..=4` by default).
    pub worker_threads: usize,
    /// Optional wall-clock timeout enforced inside Tree-sitter's `progress_callback`.
    pub parse_timeout: Option<Duration>,
    /// Whether worker threads reuse a `thread_local!` `tree_sitter::Parser` instance across jobs.
    pub reuse_thread_local_parsers: bool,
}

impl Default for SyntaxEngineConfig {
    fn default() -> Self {
        let cpus = std::thread::available_parallelism().map_or(2, NonZeroUsize::get);
        Self {
            worker_threads: (cpus / 2).clamp(1, MAX_DEFAULT_WORKER_THREADS),
            parse_timeout: Some(DEFAULT_PARSE_TIMEOUT),
            reuse_thread_local_parsers: true,
        }
    }
}

/// Input payload for asynchronous [`SyntaxEngine::schedule_parse`].
#[derive(Debug, Clone)]
pub struct ParseRequest {
    /// Target grammar language.
    pub language: SyntaxLanguage,
    /// Immutable buffer snapshot to parse (cloned in $O(1)$ via `Arc<Rope>`).
    pub snapshot: BufferSnapshot,
    /// Optional base syntax tree and sequential edits for incremental re-parsing.
    pub previous: Option<(SyntaxTree, Vec<SyntaxEdit>)>,
}

impl ParseRequest {
    /// Creates a full (non-incremental) [`ParseRequest`] for `snapshot`.
    #[must_use]
    pub const fn full(language: SyntaxLanguage, snapshot: BufferSnapshot) -> Self {
        Self {
            language,
            snapshot,
            previous: None,
        }
    }

    /// Creates an incremental [`ParseRequest`] anchored on `previous` and `edits`.
    #[must_use]
    pub const fn incremental(
        language: SyntaxLanguage,
        snapshot: BufferSnapshot,
        previous: SyntaxTree,
        edits: Vec<SyntaxEdit>,
    ) -> Self {
        Self {
            language,
            snapshot,
            previous: Some((previous, edits)),
        }
    }

    /// Builds an incremental [`ParseRequest`] from [`EditLog`] deltas when `previous` is compatible
    /// and its revision is still retained in `edit_log`, falling back to a full parse otherwise.
    #[must_use]
    pub fn from_edit_log(
        language: SyntaxLanguage,
        snapshot: BufferSnapshot,
        previous: Option<&SyntaxTree>,
        edit_log: &EditLog,
    ) -> Self {
        let Some(prev) = previous else {
            return Self::full(language, snapshot);
        };

        if prev.buffer_id() != snapshot.id() || prev.language() != language {
            return Self::full(language, snapshot);
        }

        if prev.version() == snapshot.version() {
            return Self::incremental(language, snapshot, prev.clone(), Vec::new());
        }

        match edit_log.changes_since(prev.version()) {
            VersionChanges::Deltas {
                current_version, ..
            } if current_version == snapshot.version() => {
                if let Some(edits) =
                    SyntaxEdit::from_version_changes(&edit_log.changes_since(prev.version()))
                {
                    Self::incremental(language, snapshot, prev.clone(), edits)
                } else {
                    Self::full(language, snapshot)
                }
            }
            VersionChanges::UpToDate
            | VersionChanges::Deltas { .. }
            | VersionChanges::FullResyncRequired { .. } => Self::full(language, snapshot),
        }
    }
}

/// Handle returned by [`SyntaxEngine::schedule_parse`] for inspecting or cancelling a queued job.
#[derive(Debug, Clone)]
pub struct ParseJobHandle {
    buffer_id: BufferId,
    target_version: BufferVersion,
    cancelled: Arc<AtomicBool>,
    latest_version: Arc<AtomicU64>,
}

impl ParseJobHandle {
    /// Returns the buffer identifier associated with this parse job.
    #[must_use]
    pub const fn buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    /// Returns the snapshot revision targeted by this parse job.
    #[must_use]
    pub const fn target_version(&self) -> BufferVersion {
        self.target_version
    }

    /// Requests cancellation of this parse job; checked before and during Tree-sitter execution.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns `true` if [`Self::cancel`] was called or a newer version was scheduled for the buffer.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self.latest_version.load(Ordering::Acquire) > self.target_version.as_u64()
    }
}

#[derive(Debug, Clone, Copy)]
enum AbortReason {
    Cancelled(BufferVersion),
    TimedOut(u64),
}

/// Incremental Tree-sitter parsing engine backed by a bounded `rayon` thread pool.
pub struct SyntaxEngine {
    config: SyntaxEngineConfig,
    registry: LanguageRegistry,
    pool: rayon::ThreadPool,
    latest_versions: Mutex<HashMap<BufferId, Arc<AtomicU64>>>,
}

impl fmt::Debug for SyntaxEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SyntaxEngine")
            .field("config", &self.config)
            .field("registered_languages", &self.registry.len())
            .field("worker_threads", &self.pool.current_num_threads())
            .finish_non_exhaustive()
    }
}

impl SyntaxEngine {
    /// Creates a new [`SyntaxEngine`] with its own bounded `rayon::ThreadPool`.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::ThreadPoolInit`] if `config.worker_threads == 0` or the OS fails
    /// to spawn the worker threads.
    pub fn new(
        registry: LanguageRegistry,
        config: SyntaxEngineConfig,
    ) -> Result<Self, SyntaxError> {
        if config.worker_threads == 0 {
            return Err(SyntaxError::ThreadPoolInit {
                reason: String::from("worker_threads must be at least 1"),
            });
        }

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(config.worker_threads)
            .thread_name(|idx| format!("kova-syntax-{idx}"))
            .build()
            .map_err(|err| SyntaxError::ThreadPoolInit {
                reason: err.to_string(),
            })?;

        Ok(Self {
            config,
            registry,
            pool,
            latest_versions: Mutex::new(HashMap::new()),
        })
    }

    /// Returns the engine configuration.
    #[must_use]
    pub const fn config(&self) -> SyntaxEngineConfig {
        self.config
    }

    /// Returns a reference to the language registry.
    #[must_use]
    pub const fn registry(&self) -> &LanguageRegistry {
        &self.registry
    }

    /// Returns the number of worker threads in the dedicated `rayon` pool.
    #[must_use]
    pub fn worker_thread_count(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// Synchronously parses `snapshot` on the calling thread, reusing `previous` incrementally
    /// when `buffer_id` and `language` match.
    ///
    /// Returns `Ok(prev.clone())` in $O(1)$ without invoking Tree-sitter when `prev.version() ==
    /// snapshot.version()` and `edits` is empty.
    ///
    /// # Errors
    ///
    /// Returns [`SyntaxError::UnsupportedLanguage`] if `language` is not registered,
    /// [`SyntaxError::LanguageLoadFailed`] if the grammar ABI is rejected,
    /// [`SyntaxError::ParseTimedOut`] if parsing exceeds `config.parse_timeout`, or
    /// [`SyntaxError::ParseFailed`] if Tree-sitter fails to produce a tree.
    pub fn parse_sync(
        &self,
        language: SyntaxLanguage,
        snapshot: &BufferSnapshot,
        previous: Option<(&SyntaxTree, &[SyntaxEdit])>,
    ) -> Result<SyntaxTree, SyntaxError> {
        let entry = self
            .registry
            .get_entry(language)
            .ok_or(SyntaxError::UnsupportedLanguage { language })?;

        let owned_prev = previous.map(|(tree, edits)| (tree.clone(), edits.to_vec()));
        execute_parse(
            &entry,
            snapshot,
            owned_prev.as_ref().map(|(t, e)| (t, e.as_slice())),
            self.config,
            None,
        )
    }

    /// Synchronously parses `snapshot` using incremental deltas from `edit_log` when `previous`
    /// is compatible and its revision is retained in `edit_log`, falling back to a full parse
    /// otherwise.
    ///
    /// # Errors
    ///
    /// Propagates any [`SyntaxError`] from [`Self::parse_sync`].
    pub fn parse_with_edit_log(
        &self,
        language: SyntaxLanguage,
        snapshot: &BufferSnapshot,
        previous: Option<&SyntaxTree>,
        edit_log: &EditLog,
    ) -> Result<SyntaxTree, SyntaxError> {
        let request = ParseRequest::from_edit_log(language, snapshot.clone(), previous, edit_log);
        let prev_ref = request
            .previous
            .as_ref()
            .map(|(tree, edits)| (tree, edits.as_slice()));
        self.parse_sync(request.language, &request.snapshot, prev_ref)
    }

    /// Schedules an asynchronous parse on the dedicated `rayon` pool using *latest-wins*
    /// coalescing per [`BufferId`].
    ///
    /// Returns immediately in $O(1)$ time (`<< 5 ms`) with a [`ParseJobHandle`]. Older queued or
    /// in-flight parses for the same buffer are cancelled via Tree-sitter's `progress_callback`
    /// and deliver `Err(SyntaxError::ParseCancelled { .. })` to `on_complete`.
    pub fn schedule_parse(
        &self,
        request: ParseRequest,
        on_complete: impl FnOnce(Result<SyntaxTree, SyntaxError>) + Send + 'static,
    ) -> ParseJobHandle {
        let buffer_id = request.snapshot.id();
        let target_version = request.snapshot.version();
        let latest_version = self.bump_latest_version(buffer_id, target_version);
        let cancelled = Arc::new(AtomicBool::new(false));

        let handle = ParseJobHandle {
            buffer_id,
            target_version,
            cancelled: Arc::clone(&cancelled),
            latest_version: Arc::clone(&latest_version),
        };

        let entry = self.registry.get_entry(request.language);
        let config = self.config;
        let cancel_flag = Arc::clone(&cancelled);

        self.pool.spawn(move || {
            let latest_raw = latest_version.load(Ordering::Acquire);
            if cancel_flag.load(Ordering::Acquire) || latest_raw > target_version.as_u64() {
                on_complete(Err(SyntaxError::ParseCancelled {
                    requested: target_version,
                    latest: BufferVersion::new(latest_raw),
                }));
                return;
            }

            let Some(entry) = entry else {
                on_complete(Err(SyntaxError::UnsupportedLanguage {
                    language: request.language,
                }));
                return;
            };

            let prev_ref = request
                .previous
                .as_ref()
                .map(|(tree, edits)| (tree, edits.as_slice()));

            let cancel_ctx = CancelContext {
                target_version,
                cancelled: &cancel_flag,
                latest_version: &latest_version,
            };

            let result = execute_parse(
                &entry,
                &request.snapshot,
                prev_ref,
                config,
                Some(&cancel_ctx),
            );
            on_complete(result);
        });

        handle
    }

    fn bump_latest_version(
        &self,
        buffer_id: BufferId,
        target_version: BufferVersion,
    ) -> Arc<AtomicU64> {
        let mut guard = self
            .latest_versions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let counter = guard
            .entry(buffer_id)
            .or_insert_with(|| Arc::new(AtomicU64::new(target_version.as_u64())));
        counter.fetch_max(target_version.as_u64(), Ordering::AcqRel);
        Arc::clone(counter)
    }
}

struct CancelContext<'a> {
    target_version: BufferVersion,
    cancelled: &'a AtomicBool,
    latest_version: &'a AtomicU64,
}

impl CancelContext<'_> {
    fn check_cancelled(&self) -> Option<BufferVersion> {
        let latest = self.latest_version.load(Ordering::Acquire);
        if self.cancelled.load(Ordering::Acquire) || latest > self.target_version.as_u64() {
            Some(BufferVersion::new(latest))
        } else {
            None
        }
    }
}

fn execute_parse(
    entry: &LanguageEntry,
    snapshot: &BufferSnapshot,
    previous: Option<(&SyntaxTree, &[SyntaxEdit])>,
    config: SyntaxEngineConfig,
    cancel: Option<&CancelContext<'_>>,
) -> Result<SyntaxTree, SyntaxError> {
    if let Some(ctx) = cancel
        && let Some(latest) = ctx.check_cancelled()
    {
        return Err(SyntaxError::ParseCancelled {
            requested: snapshot.version(),
            latest,
        });
    }

    let compatible_prev = previous.filter(|(prev, _)| {
        prev.buffer_id() == snapshot.id() && prev.language() == entry.language()
    });

    if let Some((prev, edits)) = compatible_prev
        && prev.version() == snapshot.version()
        && edits.is_empty()
    {
        return Ok(prev.clone());
    }

    let (edited_old_tree, incremental) = match compatible_prev {
        Some((prev, edits)) => (Some(prev.with_edits(edits)), true),
        None => (None, false),
    };

    let started = Instant::now();
    let mut abort_reason: Option<AbortReason> = None;

    let tree_opt = with_parser(config.reuse_thread_local_parsers, |parser| {
        parser.set_language(entry.ts_language()).map_err(|err| {
            SyntaxError::LanguageLoadFailed {
                language: entry.language(),
                reason: err.to_string(),
            }
        })?;

        let mut progress = |_state: &tree_sitter::ParseState| -> ControlFlow<()> {
            if let Some(ctx) = cancel
                && let Some(latest) = ctx.check_cancelled()
            {
                abort_reason = Some(AbortReason::Cancelled(latest));
                return ControlFlow::Break(());
            }
            if let Some(timeout) = config.parse_timeout
                && started.elapsed() >= timeout
            {
                let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                abort_reason = Some(AbortReason::TimedOut(elapsed_ms));
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        };

        let parsed = parse_rope_with(
            parser,
            snapshot.rope(),
            edited_old_tree.as_ref(),
            &mut progress,
        );
        if parsed.is_none() {
            // Ensure an aborted parser never leaks partial state into subsequent parses on this thread.
            parser.reset();
        }
        Ok(parsed)
    })?;

    let elapsed = started.elapsed();

    let Some(tree) = tree_opt else {
        return match abort_reason {
            Some(AbortReason::Cancelled(latest)) => Err(SyntaxError::ParseCancelled {
                requested: snapshot.version(),
                latest,
            }),
            Some(AbortReason::TimedOut(elapsed_ms)) => {
                Err(SyntaxError::ParseTimedOut { elapsed_ms })
            }
            None => {
                if let Some(ctx) = cancel
                    && let Some(latest) = ctx.check_cancelled()
                {
                    return Err(SyntaxError::ParseCancelled {
                        requested: snapshot.version(),
                        latest,
                    });
                }
                Err(SyntaxError::ParseFailed)
            }
        };
    };

    if let Some(ctx) = cancel
        && let Some(latest) = ctx.check_cancelled()
    {
        return Err(SyntaxError::ParseCancelled {
            requested: snapshot.version(),
            latest,
        });
    }

    Ok(SyntaxTree::new(
        snapshot.id(),
        snapshot.version(),
        entry.language(),
        tree,
        elapsed,
        incremental,
    ))
}

fn with_parser<T>(
    reuse_thread_local: bool,
    f: impl FnOnce(&mut tree_sitter::Parser) -> Result<T, SyntaxError>,
) -> Result<T, SyntaxError> {
    if !reuse_thread_local {
        let mut parser = tree_sitter::Parser::new();
        return f(&mut parser);
    }

    THREAD_LOCAL_PARSER.with(|cell| {
        let mut guard = cell.borrow_mut();
        let parser = guard.get_or_insert_with(tree_sitter::Parser::new);
        f(parser)
    })
}

/// Thread-safe store retaining the latest [`SyntaxTree`] per [`BufferId`] with version monotonicity.
#[derive(Debug, Default)]
pub struct SyntaxTreeStore {
    trees: RwLock<HashMap<BufferId, SyntaxTree>>,
}

impl SyntaxTreeStore {
    /// Creates an empty [`SyntaxTreeStore`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a cloned [`SyntaxTree`] for `buffer_id` in $O(1)$ time, if stored.
    #[must_use]
    pub fn get(&self, buffer_id: BufferId) -> Option<SyntaxTree> {
        let guard = self
            .trees
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.get(&buffer_id).cloned()
    }

    /// Inserts `tree` only if no newer (or identical-version/same-language) tree is already stored.
    ///
    /// Returns `true` when `tree` was accepted, or `false` if a newer revision already won the race.
    pub fn insert_if_newer(&self, tree: SyntaxTree) -> bool {
        let mut guard = self
            .trees
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = guard.get(&tree.buffer_id())
            && (existing.version() > tree.version()
                || (existing.version() == tree.version() && existing.language() == tree.language()))
        {
            return false;
        }
        guard.insert(tree.buffer_id(), tree);
        true
    }

    /// Removes and returns the stored [`SyntaxTree`] for `buffer_id`, if present.
    pub fn remove(&self, buffer_id: BufferId) -> Option<SyntaxTree> {
        let mut guard = self
            .trees
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.remove(&buffer_id)
    }

    /// Returns the number of buffers currently tracked in the store.
    #[must_use]
    pub fn len(&self) -> usize {
        let guard = self
            .trees
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.len()
    }

    /// Returns `true` if the store contains no syntax trees.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes all stored syntax trees.
    pub fn clear(&self) {
        let mut guard = self
            .trees
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clear();
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "unit tests assert deterministic synchronous and incremental parsing"
)]
mod tests {
    use kova_buffer::{Buffer, EditLogConfig, TextEdit};

    use super::*;

    fn json_engine(config: SyntaxEngineConfig) -> SyntaxEngine {
        let mut registry = LanguageRegistry::new();
        registry
            .register(SyntaxLanguage::Json, tree_sitter_json::LANGUAGE.into())
            .unwrap();
        SyntaxEngine::new(registry, config).unwrap()
    }

    #[test]
    fn parse_sync_incremental_edit_matches_full_reparse() {
        let engine = json_engine(SyntaxEngineConfig::default());
        let mut buffer = Buffer::with_id(
            BufferId::new(42),
            "{\n  \"name\": \"kova\",\n  \"items\": [1, 2, 3]\n}\n",
        );
        let snap0 = buffer.snapshot();
        let tree0 = engine
            .parse_sync(SyntaxLanguage::Json, &snap0, None)
            .unwrap();
        assert!(!tree0.was_incremental());
        assert!(!tree0.has_error());

        // Same version with empty edits takes the O(1) no-op fast path.
        let noop = engine
            .parse_sync(SyntaxLanguage::Json, &snap0, Some((&tree0, &[])))
            .unwrap();
        assert_eq!(noop.version(), tree0.version());
        assert_eq!(noop.root_node().to_sexp(), tree0.root_node().to_sexp());

        // Multi-cursor / multi-line edit tracked by EditLog.
        let initial = snap0.text();
        let name_start = initial.find("kova").unwrap();
        let two_start = initial.find('2').unwrap();
        buffer
            .apply_edits(&[
                TextEdit::new(name_start..(name_start + 4), "kova-ide"),
                TextEdit::new(two_start..(two_start + 1), "10,\n    20"),
            ])
            .unwrap();
        let snap1 = buffer.snapshot();

        let tree1_inc = engine
            .parse_with_edit_log(
                SyntaxLanguage::Json,
                &snap1,
                Some(&tree0),
                buffer.edit_log(),
            )
            .unwrap();
        assert!(tree1_inc.was_incremental());
        assert_eq!(tree1_inc.version(), snap1.version());
        assert!(!tree1_inc.has_error());

        let tree1_full = engine
            .parse_sync(SyntaxLanguage::Json, &snap1, None)
            .unwrap();
        assert!(!tree1_full.was_incremental());
        assert_eq!(
            tree1_inc.root_node().to_sexp(),
            tree1_full.root_node().to_sexp()
        );
    }

    #[test]
    fn edit_log_eviction_falls_back_to_full_reparse_cleanly() {
        let engine = json_engine(SyntaxEngineConfig::default());
        let mut buffer = Buffer::with_edit_log_config(
            BufferId::new(7),
            "{\"v\": 0}",
            EditLogConfig {
                max_versions: 1,
                max_retained_bytes: 1024,
            },
        );
        let snap0 = buffer.snapshot();
        let tree0 = engine
            .parse_sync(SyntaxLanguage::Json, &snap0, None)
            .unwrap();

        buffer.apply_edit(6..7, "1").unwrap();
        buffer.apply_edit(6..7, "2").unwrap();
        let snap2 = buffer.snapshot();

        // Version 0 -> 1 was evicted, so `parse_with_edit_log` safely falls back to full parse.
        let tree2 = engine
            .parse_with_edit_log(
                SyntaxLanguage::Json,
                &snap2,
                Some(&tree0),
                buffer.edit_log(),
            )
            .unwrap();
        assert!(!tree2.was_incremental());
        assert_eq!(tree2.version(), snap2.version());
        assert!(!tree2.has_error());
    }

    #[test]
    fn syntax_tree_store_rejects_stale_versions() {
        let engine = json_engine(SyntaxEngineConfig::default());
        let mut buffer = Buffer::with_id(BufferId::new(9), "{\"k\": 1}");
        let snap0 = buffer.snapshot();
        let tree0 = engine
            .parse_sync(SyntaxLanguage::Json, &snap0, None)
            .unwrap();

        buffer.apply_edit(6..7, "2").unwrap();
        let snap1 = buffer.snapshot();
        let tree1 = engine
            .parse_sync(SyntaxLanguage::Json, &snap1, None)
            .unwrap();

        let store = SyntaxTreeStore::new();
        assert!(store.is_empty());
        assert!(store.insert_if_newer(tree1));
        assert!(!store.insert_if_newer(tree0));
        assert_eq!(
            store.get(BufferId::new(9)).unwrap().version(),
            BufferVersion::new(1)
        );
    }

    #[test]
    fn parse_timeout_aborts_and_resets_thread_local_parser() {
        let engine = json_engine(SyntaxEngineConfig {
            worker_threads: 1,
            parse_timeout: Some(Duration::ZERO),
            reuse_thread_local_parsers: true,
        });

        let mut text = String::from("[");
        for i in 0..500 {
            if i > 0 {
                text.push(',');
            }
            text.push_str(&i.to_string());
        }
        text.push(']');

        let buffer = Buffer::from_text(&text);
        let snap = buffer.snapshot();
        let err = engine
            .parse_sync(SyntaxLanguage::Json, &snap, None)
            .unwrap_err();
        assert!(matches!(err, SyntaxError::ParseTimedOut { .. }));

        // Subsequent parse on a non-zero timeout engine must succeed cleanly.
        let healthy_engine = json_engine(SyntaxEngineConfig::default());
        let ok_tree = healthy_engine
            .parse_sync(SyntaxLanguage::Json, &snap, None)
            .unwrap();
        assert!(!ok_tree.has_error());
    }
}
