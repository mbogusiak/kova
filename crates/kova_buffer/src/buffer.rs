//! Mutable text `Buffer` using copy-on-write `Arc<Rope>` mutations.

use std::ops::Range;
use std::sync::Arc;

use ropey::Rope;
use smallvec::{SmallVec, smallvec};

use crate::edit_log::{
    EditLog, EditLogConfig, LspTextChange, TreeSitterEdit, VersionChanges, VersionDelta,
    apply_single_rope_edit,
};
use crate::error::BufferError;
use crate::line_ending::LineEnding;
use crate::position::{BufferId, BufferVersion, TextEdit};
use crate::snapshot::BufferSnapshot;

/// Mutable text buffer backed by `Arc<Rope>` with lock-free $O(1)$ [`BufferSnapshot`]s.
///
/// When no external snapshots share the underlying `Arc<Rope>`, mutations update
/// the rope in place in $O(\log N)$ time. When background workers hold an older
/// [`BufferSnapshot`], `Arc::make_mut` forks only the root handle in $O(1)$ (internal
/// B-tree nodes remain structurally shared by `ropey`) without blocking readers.
#[derive(Debug, Clone)]
pub struct Buffer {
    id: BufferId,
    version: BufferVersion,
    rope: Arc<Rope>,
    line_ending: LineEnding,
    edit_log: EditLog,
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Buffer {
    /// Creates an empty LF buffer with `BufferId(0)` and `BufferVersion(0)`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: BufferId::new(0),
            version: BufferVersion::new(0),
            rope: Arc::new(Rope::new()),
            line_ending: LineEnding::Lf,
            edit_log: EditLog::default(),
        }
    }

    /// Creates a buffer initialized with `text`, auto-detecting its [`LineEnding`].
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self::with_id(BufferId::new(0), text)
    }

    /// Creates a buffer with an explicit [`BufferId`], auto-detecting its [`LineEnding`].
    #[must_use]
    pub fn with_id(id: BufferId, text: &str) -> Self {
        let rope = Rope::from_str(text);
        let line_ending = LineEnding::detect_rope(&rope);
        Self {
            id,
            version: BufferVersion::new(0),
            rope: Arc::new(rope),
            line_ending,
            edit_log: EditLog::default(),
        }
    }

    /// Creates a buffer with explicit [`BufferId`] and [`LineEnding`], normalizing
    /// `text` line endings to match `line_ending`.
    #[must_use]
    pub fn with_line_ending(id: BufferId, text: &str, line_ending: LineEnding) -> Self {
        let normalized = LineEnding::normalize_text(text, line_ending);
        Self {
            id,
            version: BufferVersion::new(0),
            rope: Arc::new(Rope::from_str(&normalized)),
            line_ending,
            edit_log: EditLog::default(),
        }
    }

    /// Creates a buffer initialized with `text` and a custom [`EditLogConfig`].
    #[must_use]
    pub fn with_edit_log_config(id: BufferId, text: &str, edit_log_config: EditLogConfig) -> Self {
        let rope = Rope::from_str(text);
        let line_ending = LineEnding::detect_rope(&rope);
        Self {
            id,
            version: BufferVersion::new(0),
            rope: Arc::new(rope),
            line_ending,
            edit_log: EditLog::new(edit_log_config),
        }
    }

    /// Returns the buffer identifier.
    #[must_use]
    pub const fn id(&self) -> BufferId {
        self.id
    }

    /// Returns the current revision number.
    #[must_use]
    pub const fn version(&self) -> BufferVersion {
        self.version
    }

    /// Returns the active [`LineEnding`] of this buffer.
    #[must_use]
    pub const fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    /// Updates the buffer's configured [`LineEnding`] without rewriting existing content.
    pub fn set_line_ending(&mut self, line_ending: LineEnding) {
        self.line_ending = line_ending;
    }

    /// Returns a reference to the buffer's incremental [`EditLog`].
    #[must_use]
    pub const fn edit_log(&self) -> &EditLog {
        &self.edit_log
    }

    /// Returns the number of [`VersionDelta`] transitions currently retained in the [`EditLog`].
    #[must_use]
    pub fn edit_log_len(&self) -> usize {
        self.edit_log.len()
    }

    /// Queries all incremental version transitions since `since`.
    #[must_use]
    pub fn changes_since(&self, since: BufferVersion) -> VersionChanges<'_> {
        self.edit_log.changes_since(since)
    }

    /// Collects sequential [`TreeSitterEdit`]s since `since`, or `None` if a full re-parse is required.
    #[must_use]
    pub fn tree_sitter_edits_since(&self, since: BufferVersion) -> Option<Vec<TreeSitterEdit>> {
        self.edit_log.collect_tree_sitter_edits(since)
    }

    /// Collects sequential [`LspTextChange`]s since `since`, or `None` if full sync is required.
    #[must_use]
    pub fn lsp_changes_since(&self, since: BufferVersion) -> Option<Vec<LspTextChange>> {
        self.edit_log.collect_lsp_changes(since)
    }

    /// Returns an immutable [`BufferSnapshot`] in $O(1)$ time.
    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        BufferSnapshot::new(
            self.id,
            self.version,
            Arc::clone(&self.rope),
            self.line_ending,
        )
    }

    /// Applies a single edit replacing `range` (in bytes) with `replacement`.
    ///
    /// Returns the resulting [`BufferVersion`]. A pure no-op (`range.is_empty()` and
    /// `replacement.is_empty()`) validates bounds and returns the current version
    /// without incrementing it or pushing to [`EditLog`].
    ///
    /// # Errors
    ///
    /// Returns [`BufferError::InvalidByteRange`], [`BufferError::NotCharBoundary`],
    /// or [`BufferError::SplitsCrLf`] if `range` is invalid.
    pub fn apply_edit(
        &mut self,
        range: Range<usize>,
        replacement: &str,
    ) -> Result<BufferVersion, BufferError> {
        {
            let snapshot = self.snapshot();
            validate_edit_range(&snapshot, &range)?;
        }
        if range.is_empty() && replacement.is_empty() {
            return Ok(self.version);
        }

        let rope = Arc::make_mut(&mut self.rope);
        let delta = apply_single_rope_edit(rope, &range, replacement);
        self.edit_log
            .push(VersionDelta::new(self.version, smallvec![delta]));

        self.version = self.version.next();
        Ok(self.version)
    }

    /// Atomically validates and applies a batch of non-overlapping [`TextEdit`]s
    /// whose ranges all refer to the pre-edit buffer state.
    ///
    /// Adjacent non-empty ranges (`prev.end == next.start`) are permitted; two empty
    /// insertions at the exact same offset or overlapping spans fail with
    /// [`BufferError::OverlappingEdits`] without modifying the buffer.
    ///
    /// Recorded [`EditDelta`](crate::edit_log::EditDelta)s are ordered from back to front so sequential consumers
    /// (Tree-sitter and LSP) never observe coordinate shifts from earlier cursors.
    ///
    /// # Errors
    ///
    /// Returns [`BufferError`] if any edit range is invalid or any two edits collide.
    pub fn apply_edits(&mut self, edits: &[TextEdit]) -> Result<BufferVersion, BufferError> {
        if edits.is_empty() {
            return Ok(self.version);
        }

        let mut indexed: Vec<(usize, &TextEdit)> = edits.iter().enumerate().collect();
        {
            let snapshot = self.snapshot();
            for &(_, edit) in &indexed {
                validate_edit_range(&snapshot, &edit.range)?;
            }
        }

        indexed.sort_by(|a, b| {
            a.1.range
                .start
                .cmp(&b.1.range.start)
                .then_with(|| a.1.range.end.cmp(&b.1.range.end))
                .then_with(|| a.0.cmp(&b.0))
        });

        for pair in indexed.windows(2) {
            let prev = &pair[0].1.range;
            let next = &pair[1].1.range;
            let same_empty_point = prev.is_empty() && next.is_empty() && prev.start == next.start;
            if next.start < prev.end || same_empty_point {
                return Err(BufferError::OverlappingEdits {
                    first_end: prev.end,
                    second_start: next.start,
                });
            }
        }

        let effective_count = indexed
            .iter()
            .filter(|(_, edit)| !edit.range.is_empty() || !edit.replacement.is_empty())
            .count();
        if effective_count == 0 {
            return Ok(self.version);
        }

        // Apply and log in descending document order (back-to-front) so every earlier
        // edit's pre-edit byte, Point, and Utf16Point coordinates remain exact.
        let rope = Arc::make_mut(&mut self.rope);
        let mut deltas: SmallVec<[_; 1]> = SmallVec::with_capacity(effective_count);
        for (_, edit) in indexed.into_iter().rev() {
            if edit.range.is_empty() && edit.replacement.is_empty() {
                continue;
            }
            deltas.push(apply_single_rope_edit(
                rope,
                &edit.range,
                edit.replacement.as_str(),
            ));
        }

        self.edit_log.push(VersionDelta::new(self.version, deltas));
        self.version = self.version.next();
        Ok(self.version)
    }
}

fn validate_edit_range(snapshot: &BufferSnapshot, range: &Range<usize>) -> Result<(), BufferError> {
    let len = snapshot.len_bytes();
    if range.start > range.end || range.end > len {
        return Err(BufferError::InvalidByteRange {
            start: range.start,
            end: range.end,
            len,
        });
    }
    snapshot.validate_byte_boundary(range.start)?;
    snapshot.validate_byte_boundary(range.end)?;
    Ok(())
}
