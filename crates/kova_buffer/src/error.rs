//! Error types returned by `kova_buffer` operations.

use thiserror::Error;

/// Errors that can occur when querying coordinates or applying edits to a buffer.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BufferError {
    /// A byte offset exceeds the length of the buffer.
    #[error("byte offset {byte} is out of bounds for buffer of length {len}")]
    ByteOffsetOutOfBounds {
        /// Requested byte offset.
        byte: usize,
        /// Total byte length of the buffer.
        len: usize,
    },

    /// A Unicode scalar (`char`) index exceeds the length of the buffer.
    #[error("char offset {char_idx} is out of bounds for buffer of {len} chars")]
    CharOffsetOutOfBounds {
        /// Requested char index.
        char_idx: usize,
        /// Total char count of the buffer.
        len: usize,
    },

    /// A UTF-16 code unit index exceeds the length of the buffer.
    #[error("UTF-16 offset {utf16_idx} is out of bounds for buffer of {len} code units")]
    Utf16OffsetOutOfBounds {
        /// Requested UTF-16 code unit offset.
        utf16_idx: usize,
        /// Total UTF-16 code unit length of the buffer.
        len: usize,
    },

    /// A 0-based line index exceeds the number of lines in the buffer.
    #[error("row {row} is out of bounds for buffer with {line_count} lines")]
    RowOutOfBounds {
        /// Requested 0-based row.
        row: usize,
        /// Total number of lines in the buffer.
        line_count: usize,
    },

    /// A column offset exceeds the content length of the specified line (excluding line ending).
    #[error("column {column} on row {row} exceeds line content length {max_column}")]
    ColumnOutOfBounds {
        /// 0-based row index.
        row: usize,
        /// Requested column offset.
        column: usize,
        /// Maximum valid column on that row (excluding trailing `\n` or `\r\n`).
        max_column: usize,
    },

    /// A byte offset does not lie on a valid UTF-8 character boundary.
    #[error("byte offset {byte} does not lie on a UTF-8 character boundary")]
    NotCharBoundary {
        /// Byte offset inside a multi-byte UTF-8 sequence.
        byte: usize,
    },

    /// A byte offset lies strictly between `\r` and `\n` in a CRLF sequence.
    #[error("byte offset {byte} splits an atomic CRLF (\\r\\n) line ending")]
    SplitsCrLf {
        /// Byte offset pointing at `\n` inside `\r\n`.
        byte: usize,
    },

    /// A byte range has `start > end` or `end > len`.
    #[error("invalid byte range {start}..{end} for buffer of length {len}")]
    InvalidByteRange {
        /// Range start byte offset.
        start: usize,
        /// Range end byte offset.
        end: usize,
        /// Total byte length of the buffer.
        len: usize,
    },

    /// Two edits in a single batch overlap or duplicate an insertion at the same byte offset.
    #[error(
        "overlapping edits in batch: previous ended at {first_end}, next started at {second_start}"
    )]
    OverlappingEdits {
        /// End byte of the earlier edit in sorted order.
        first_end: usize,
        /// Start byte of the following edit in sorted order.
        second_start: usize,
    },

    /// The number of per-cursor replacement texts does not match the number of active cursors.
    #[error("cursor count mismatch: expected {expected} texts, got {actual}")]
    CursorCountMismatch {
        /// Number of selections in the [`SelectionSet`](crate::SelectionSet).
        expected: usize,
        /// Number of replacement strings supplied.
        actual: usize,
    },
}

/// Errors returned by [`History`](crate::History) undo/redo and transaction operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HistoryError {
    /// Underlying text buffer validation or mutation failed.
    #[error(transparent)]
    Buffer(#[from] BufferError),

    /// [`History::end_group`](crate::History::end_group) was called without an active group.
    #[error("end_group called without a matching begin_group")]
    UnmatchedEndGroup,

    /// History navigation (`undo`, `redo`, `redo_branch`) was attempted while a group is open.
    #[error("cannot navigate history while an explicit edit group is open")]
    GroupStillOpen,

    /// Requested child branch index is out of bounds for the current node.
    #[error("invalid branch index {index}: current node has {available} child branch(es)")]
    InvalidBranchIndex {
        /// Requested 0-based child branch index.
        index: usize,
        /// Number of child branches available at the current node.
        available: usize,
    },
}
