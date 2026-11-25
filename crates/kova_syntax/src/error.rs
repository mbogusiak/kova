//! Error types for the `kova_syntax` incremental Tree-sitter subsystem.

use kova_buffer::{BufferError, BufferId, BufferVersion};
use kova_theme::SyntaxLanguage;

/// Errors produced during language registration, query compilation, or parsing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SyntaxError {
    /// Requested language is not registered in [`crate::LanguageRegistry`].
    #[error("language {language:?} is not registered in LanguageRegistry")]
    UnsupportedLanguage {
        /// Language that was requested.
        language: SyntaxLanguage,
    },

    /// Tree-sitter rejected the supplied grammar (for example due to an ABI mismatch).
    #[error("invalid tree-sitter language ABI or incompatible grammar for {language:?}: {reason}")]
    LanguageLoadFailed {
        /// Language whose grammar failed to load.
        language: SyntaxLanguage,
        /// Human-readable reason returned by Tree-sitter.
        reason: String,
    },

    /// Tree-sitter query compilation failed.
    #[error("invalid query for {language:?} ({kind}): {message}")]
    InvalidQuery {
        /// Language the query targets.
        language: SyntaxLanguage,
        /// Query category (such as `"highlights"` or `"locals"`).
        kind: &'static str,
        /// Compiler diagnostic from Tree-sitter.
        message: String,
    },

    /// Parsing was cancelled explicitly or superseded by a newer buffer version.
    #[error(
        "parse cancelled or superseded by newer buffer version (requested {requested:?}, latest {latest:?})"
    )]
    ParseCancelled {
        /// Snapshot version the cancelled job targeted.
        requested: BufferVersion,
        /// Latest version observed for the buffer at cancellation time.
        latest: BufferVersion,
    },

    /// Parsing exceeded the configured per-job timeout.
    #[error("parse timed out after {elapsed_ms} ms")]
    ParseTimedOut {
        /// Elapsed wall-clock duration in milliseconds before aborting.
        elapsed_ms: u64,
    },

    /// Tree-sitter returned `None` without an explicit cancellation or timeout signal.
    #[error("parser returned no syntax tree (internal timeout or cancellation)")]
    ParseFailed,

    /// Failed to construct the dedicated `rayon` worker thread pool.
    #[error("failed to initialize syntax worker thread pool: {reason}")]
    ThreadPoolInit {
        /// Error message from `rayon::ThreadPoolBuilder`.
        reason: String,
    },

    /// Snapshot version did not match the expected revision.
    #[error(
        "snapshot version mismatch for buffer {buffer_id:?}: expected {expected:?}, got {actual:?}"
    )]
    VersionMismatch {
        /// Identifier of the mismatched buffer.
        buffer_id: BufferId,
        /// Expected buffer revision.
        expected: BufferVersion,
        /// Actual snapshot revision.
        actual: BufferVersion,
    },

    /// Byte range fell outside the bounds of the target snapshot.
    #[error("byte range {start}..{end} is out of bounds for snapshot of length {len}")]
    InvalidByteRange {
        /// Inclusive start byte offset.
        start: usize,
        /// Exclusive end byte offset.
        end: usize,
        /// Total byte length of the snapshot.
        len: usize,
    },

    /// Error propagated from `kova_buffer`.
    #[error(transparent)]
    Buffer(#[from] BufferError),
}
