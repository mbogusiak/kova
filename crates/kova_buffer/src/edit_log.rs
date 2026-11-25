//! Incremental `EditLog` recording byte/point deltas for Tree-sitter and UTF-16 deltas for LSP.
//!
//! All types in this module are pure headless value types with zero dependencies on
//! `tree-sitter` or `lsp-types`, mapping 1:1 to `tree_sitter::InputEdit` and
//! `lsp_types::TextDocumentContentChangeEvent` (`TextDocumentSyncKind::Incremental`).

use std::collections::VecDeque;
use std::ops::Range;

use ropey::Rope;
use smallvec::SmallVec;

use crate::position::{BufferVersion, Point, Utf16Point, Utf16Range};

/// Default maximum number of version transitions retained in an [`EditLog`].
pub const DEFAULT_MAX_EDIT_LOG_VERSIONS: usize = 1024;

/// Default maximum total replacement text bytes retained in an [`EditLog`] (`4 MiB`).
pub const DEFAULT_MAX_EDIT_LOG_BYTES: usize = 4 * 1024 * 1024;

/// Byte- and point-based edit descriptor mapping 1:1 to `tree_sitter::InputEdit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TreeSitterEdit {
    /// Byte offset where the edit begins.
    pub start_byte: usize,
    /// Byte offset where the replaced range ended before the edit.
    pub old_end_byte: usize,
    /// Byte offset where the replacement text ends after the edit.
    pub new_end_byte: usize,
    /// Zero-based `(row, byte_column)` where the edit begins.
    pub start_point: Point,
    /// Zero-based `(row, byte_column)` where the replaced range ended before the edit.
    pub old_end_point: Point,
    /// Zero-based `(row, byte_column)` where the replacement text ends after the edit.
    pub new_end_point: Point,
}

/// Incremental UTF-16 text change mapping 1:1 to LSP `TextDocumentContentChangeEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LspTextChange {
    /// Half-open UTF-16 range `[start, end)` in document coordinates prior to this step.
    pub range: Utf16Range,
    /// Length of `range` in UTF-16 code units (saturated to `u32::MAX`).
    pub range_length: u32,
    /// Replacement UTF-8 text for `range`.
    pub text: String,
}

/// Combined Tree-sitter and LSP delta for a single atomic step within a version transition.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EditDelta {
    /// Byte/point delta for incremental `tree_sitter::Tree::edit`.
    pub tree_sitter: TreeSitterEdit,
    /// UTF-16 delta for incremental LSP `textDocument/didChange`.
    pub lsp: LspTextChange,
}

/// Complete record of all sequential edits transitioning a buffer from `from_version` to `to_version`.
///
/// Multi-cursor edits are stored in descending buffer order (back-to-front) so that
/// sequential consumers (Tree-sitter `Tree::edit` and LSP `contentChanges[]`) can replay
/// every [`EditDelta`] in order without coordinate shift between cursors on the same line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionDelta {
    /// Buffer revision immediately prior to this change.
    pub from_version: BufferVersion,
    /// Buffer revision immediately after this change (`from_version + 1`).
    pub to_version: BufferVersion,
    /// Ordered list of sequential [`EditDelta`]s comprising this transition.
    pub edits: SmallVec<[EditDelta; 1]>,
    /// Sum of `lsp.text.len()` across all edits in this transition.
    pub byte_cost: usize,
}

impl VersionDelta {
    /// Builds a [`VersionDelta`] for `from_version -> from_version.next()`, summing `byte_cost`.
    #[must_use]
    pub fn new(from_version: BufferVersion, edits: SmallVec<[EditDelta; 1]>) -> Self {
        let byte_cost = edits.iter().fold(0usize, |acc, delta| {
            acc.saturating_add(delta.lsp.text.len())
        });
        Self {
            from_version,
            to_version: from_version.next(),
            edits,
            byte_cost,
        }
    }
}

/// Retention bounds for an [`EditLog`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditLogConfig {
    /// Maximum number of [`VersionDelta`] entries kept before FIFO eviction. Defaults to `1024`.
    pub max_versions: usize,
    /// Maximum total `byte_cost` kept across all retained entries. Defaults to `4 MiB`.
    pub max_retained_bytes: usize,
}

impl Default for EditLogConfig {
    fn default() -> Self {
        Self {
            max_versions: DEFAULT_MAX_EDIT_LOG_VERSIONS,
            max_retained_bytes: DEFAULT_MAX_EDIT_LOG_BYTES,
        }
    }
}

/// Result of querying an [`EditLog`] for all deltas since a consumer's snapshot version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionChanges<'a> {
    /// Consumer is already at the buffer's current version (`since == current_version`).
    UpToDate,
    /// Contiguous sequence of deltas covering `from_version -> current_version`.
    Deltas {
        /// Starting version requested by the consumer.
        from_version: BufferVersion,
        /// Current buffer version at the time of the query.
        current_version: BufferVersion,
        /// Ordered version transitions `[from_version -> from_version + 1, ..., current_version]`.
        deltas: Vec<&'a VersionDelta>,
    },
    /// Requested version is ahead of the buffer or was evicted from the retention window.
    FullResyncRequired {
        /// Version supplied by the consumer.
        requested_version: BufferVersion,
        /// Earliest `from_version` still available in the log (or `current_version` if empty).
        oldest_available_version: BufferVersion,
        /// Current buffer version.
        current_version: BufferVersion,
    },
}

impl VersionChanges<'_> {
    /// Flattens the changes into sequential [`TreeSitterEdit`]s.
    ///
    /// Returns `Some(Vec::new())` when [`Self::UpToDate`], `Some(edits)` when [`Self::Deltas`],
    /// or `None` when [`Self::FullResyncRequired`] (indicating a full re-parse is needed).
    #[must_use]
    pub fn collect_tree_sitter_edits(&self) -> Option<Vec<TreeSitterEdit>> {
        match self {
            Self::UpToDate => Some(Vec::new()),
            Self::Deltas { deltas, .. } => Some(
                deltas
                    .iter()
                    .flat_map(|vd| vd.edits.iter().map(|e| e.tree_sitter))
                    .collect(),
            ),
            Self::FullResyncRequired { .. } => None,
        }
    }

    /// Flattens the changes into sequential [`LspTextChange`]s.
    ///
    /// Returns `Some(Vec::new())` when [`Self::UpToDate`], `Some(changes)` when [`Self::Deltas`],
    /// or `None` when [`Self::FullResyncRequired`] (indicating full document sync is needed).
    #[must_use]
    pub fn collect_lsp_changes(&self) -> Option<Vec<LspTextChange>> {
        match self {
            Self::UpToDate => Some(Vec::new()),
            Self::Deltas { deltas, .. } => Some(
                deltas
                    .iter()
                    .flat_map(|vd| vd.edits.iter().map(|e| e.lsp.clone()))
                    .collect(),
            ),
            Self::FullResyncRequired { .. } => None,
        }
    }
}

/// Bounded FIFO retention log of incremental [`VersionDelta`]s for asynchronous consumers.
#[derive(Debug, Clone)]
pub struct EditLog {
    config: EditLogConfig,
    deltas: VecDeque<VersionDelta>,
    current_version: BufferVersion,
    oldest_available_version: BufferVersion,
    retained_bytes: usize,
}

impl Default for EditLog {
    fn default() -> Self {
        Self::new(EditLogConfig::default())
    }
}

impl EditLog {
    /// Creates an empty [`EditLog`] starting at `BufferVersion(0)`.
    #[must_use]
    pub fn new(config: EditLogConfig) -> Self {
        Self {
            config,
            deltas: VecDeque::new(),
            current_version: BufferVersion::new(0),
            oldest_available_version: BufferVersion::new(0),
            retained_bytes: 0,
        }
    }

    /// Returns the active retention configuration.
    #[must_use]
    pub const fn config(&self) -> EditLogConfig {
        self.config
    }

    /// Returns the number of [`VersionDelta`] records currently retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.deltas.len()
    }

    /// Returns `true` if no deltas are currently retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.deltas.is_empty()
    }

    /// Returns the total replacement byte cost currently retained in the log.
    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Returns the latest [`BufferVersion`] observed by this log.
    #[must_use]
    pub const fn current_version(&self) -> BufferVersion {
        self.current_version
    }

    /// Returns the oldest version from which incremental deltas can still be served.
    #[must_use]
    pub const fn oldest_available_version(&self) -> BufferVersion {
        self.oldest_available_version
    }

    /// Records a [`VersionDelta`] and enforces `max_versions` and `max_retained_bytes` bounds.
    ///
    /// If `max_versions == 0` or `delta.byte_cost > max_retained_bytes`, all retained deltas
    /// are evicted and `oldest_available_version` advances to `delta.to_version` so callers
    /// receive [`VersionChanges::FullResyncRequired`] without exceeding memory limits.
    pub fn push(&mut self, delta: VersionDelta) {
        self.current_version = delta.to_version;

        if self.config.max_versions == 0 || delta.byte_cost > self.config.max_retained_bytes {
            self.deltas.clear();
            self.retained_bytes = 0;
            self.oldest_available_version = self.current_version;
            return;
        }

        self.retained_bytes = self.retained_bytes.saturating_add(delta.byte_cost);
        self.deltas.push_back(delta);

        while self.deltas.len() > self.config.max_versions
            || self.retained_bytes > self.config.max_retained_bytes
        {
            let Some(evicted) = self.deltas.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(evicted.byte_cost);
        }

        self.oldest_available_version = self
            .deltas
            .front()
            .map_or(self.current_version, |front| front.from_version);
    }

    /// Queries all version transitions from `since` up to [`Self::current_version`].
    #[must_use]
    pub fn changes_since(&self, since: BufferVersion) -> VersionChanges<'_> {
        if since == self.current_version {
            return VersionChanges::UpToDate;
        }

        if since > self.current_version || since < self.oldest_available_version {
            return VersionChanges::FullResyncRequired {
                requested_version: since,
                oldest_available_version: self.oldest_available_version,
                current_version: self.current_version,
            };
        }

        let offset = usize::try_from(
            since
                .as_u64()
                .saturating_sub(self.oldest_available_version.as_u64()),
        )
        .unwrap_or(usize::MAX);

        if offset >= self.deltas.len() {
            return VersionChanges::FullResyncRequired {
                requested_version: since,
                oldest_available_version: self.oldest_available_version,
                current_version: self.current_version,
            };
        }

        let deltas: Vec<&VersionDelta> = self.deltas.range(offset..).collect();
        VersionChanges::Deltas {
            from_version: since,
            current_version: self.current_version,
            deltas,
        }
    }

    /// Collects sequential [`TreeSitterEdit`]s since `since`, or `None` if full re-parse is needed.
    #[must_use]
    pub fn collect_tree_sitter_edits(&self, since: BufferVersion) -> Option<Vec<TreeSitterEdit>> {
        self.changes_since(since).collect_tree_sitter_edits()
    }

    /// Collects sequential [`LspTextChange`]s since `since`, or `None` if full sync is needed.
    #[must_use]
    pub fn collect_lsp_changes(&self, since: BufferVersion) -> Option<Vec<LspTextChange>> {
        self.changes_since(since).collect_lsp_changes()
    }
}

/// Applies a single validated `range -> replacement` edit to `rope`, returning its [`EditDelta`].
///
/// The caller must ensure `range` lies on valid UTF-8 char boundaries and does not split `\r\n`.
pub(crate) fn apply_single_rope_edit(
    rope: &mut Rope,
    range: &Range<usize>,
    replacement: &str,
) -> EditDelta {
    let start_byte = range.start;
    let old_end_byte = range.end;

    let (start_char, start_point, start_utf16, start_utf16_cu) =
        coordinates_at_byte(rope, start_byte);

    let (old_end_char, old_end_point, old_end_utf16, range_length) = if old_end_byte == start_byte {
        (start_char, start_point, start_utf16, 0)
    } else {
        let (end_char, end_point, end_utf16, end_utf16_cu) =
            coordinates_at_byte(rope, old_end_byte);
        let utf16_len = end_utf16_cu.saturating_sub(start_utf16_cu);
        let range_len = u32::try_from(utf16_len).unwrap_or(u32::MAX);
        (end_char, end_point, end_utf16, range_len)
    };

    if start_char != old_end_char {
        rope.remove(start_char..old_end_char);
    }
    if !replacement.is_empty() {
        rope.insert(start_char, replacement);
    }

    let new_end_byte = start_byte + replacement.len();
    let new_end_point = if replacement.is_empty() {
        start_point
    } else {
        let row = rope.byte_to_line(new_end_byte);
        let line_byte = rope.line_to_byte(row);
        Point::new(row, new_end_byte - line_byte)
    };

    EditDelta {
        tree_sitter: TreeSitterEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_point,
            old_end_point,
            new_end_point,
        },
        lsp: LspTextChange {
            range: Utf16Range::new(start_utf16, old_end_utf16),
            range_length,
            text: replacement.to_owned(),
        },
    }
}

fn coordinates_at_byte(rope: &Rope, byte: usize) -> (usize, Point, Utf16Point, usize) {
    let char_idx = rope.byte_to_char(byte);
    let row = rope.char_to_line(char_idx);
    let line_byte = rope.line_to_byte(row);
    let point = Point::new(row, byte - line_byte);

    let utf16_cu = rope.char_to_utf16_cu(char_idx);
    let line_char = rope.line_to_char(row);
    let line_utf16 = rope.char_to_utf16_cu(line_char);
    let utf16_point = Utf16Point::new(row, utf16_cu - line_utf16);

    (char_idx, point, utf16_point, utf16_cu)
}
