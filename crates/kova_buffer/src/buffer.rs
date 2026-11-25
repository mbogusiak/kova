//! Mutable text `Buffer` using copy-on-write `Arc<Rope>` mutations.

use std::ops::Range;
use std::sync::Arc;

use ropey::Rope;

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
    /// without incrementing it.
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
        let snapshot = self.snapshot();
        validate_edit_range(&snapshot, &range)?;
        if range.is_empty() && replacement.is_empty() {
            return Ok(self.version);
        }

        let start_char = snapshot.rope().byte_to_char(range.start);
        let end_char = snapshot.rope().byte_to_char(range.end);

        let rope = Arc::make_mut(&mut self.rope);
        if start_char != end_char {
            rope.remove(start_char..end_char);
        }
        if !replacement.is_empty() {
            rope.insert(start_char, replacement);
        }

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
    /// # Errors
    ///
    /// Returns [`BufferError`] if any edit range is invalid or any two edits collide.
    pub fn apply_edits(&mut self, edits: &[TextEdit]) -> Result<BufferVersion, BufferError> {
        if edits.is_empty() {
            return Ok(self.version);
        }

        let snapshot = self.snapshot();
        let mut indexed: Vec<(usize, &TextEdit)> = edits.iter().enumerate().collect();

        for &(_, edit) in &indexed {
            validate_edit_range(&snapshot, &edit.range)?;
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

        let has_effective_change = indexed
            .iter()
            .any(|(_, edit)| !edit.range.is_empty() || !edit.replacement.is_empty());
        if !has_effective_change {
            return Ok(self.version);
        }

        // Convert all byte bounds to char indices before mutating the rope, then apply
        // from back to front so earlier indices stay invariant.
        let mut char_edits: Vec<(usize, usize, &str)> = Vec::with_capacity(indexed.len());
        for (_, edit) in indexed {
            if edit.range.is_empty() && edit.replacement.is_empty() {
                continue;
            }
            let start_char = snapshot.rope().byte_to_char(edit.range.start);
            let end_char = snapshot.rope().byte_to_char(edit.range.end);
            char_edits.push((start_char, end_char, edit.replacement.as_str()));
        }

        let rope = Arc::make_mut(&mut self.rope);
        for (start_char, end_char, replacement) in char_edits.into_iter().rev() {
            if start_char != end_char {
                rope.remove(start_char..end_char);
            }
            if !replacement.is_empty() {
                rope.insert(start_char, replacement);
            }
        }

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
