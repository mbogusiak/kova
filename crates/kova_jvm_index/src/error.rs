//! Error types for JVM bytecode parsing, signature decoding, and index operations.

use thiserror::Error;

/// Errors produced by `kova_jvm_index` when parsing classfiles, scanning archives,
/// decoding JVM descriptors/signatures, or accessing the index cache.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JvmIndexError {
    /// A `.class` byte stream is malformed or violates the JVM classfile format.
    #[error("invalid classfile `{origin}`: {reason}")]
    InvalidClassFile {
        /// Human-readable origin of the classfile (file path or archive entry).
        origin: String,
        /// Detailed reason why parsing failed.
        reason: String,
    },

    /// An archive (`.jar`, `.zip`, or `.jmod`) could not be opened or parsed.
    #[error("failed to read archive `{path}`: {reason}")]
    InvalidArchive {
        /// Path to the archive on disk.
        path: String,
        /// Underlying archive reader failure message.
        reason: String,
    },

    /// Reading or decompressing a specific entry inside an archive failed.
    #[error("failed to read archive entry `{entry}` in `{archive}`: {reason}")]
    ArchiveEntryRead {
        /// Path to the archive on disk.
        archive: String,
        /// Entry path inside the archive.
        entry: String,
        /// Detailed failure reason (e.g., CRC mismatch or size limit exceeded).
        reason: String,
    },

    /// A JVM field/method descriptor or generic `Signature` attribute is malformed or too deep.
    #[error("invalid JVM descriptor or signature `{raw}`: {reason}")]
    InvalidDescriptor {
        /// Raw descriptor or signature string that failed to parse.
        raw: String,
        /// Detailed syntax or depth-limit failure reason.
        reason: String,
    },

    /// An operating-system I/O operation failed.
    #[error("I/O error at `{path}`: {message}")]
    Io {
        /// Path involved in the I/O operation.
        path: String,
        /// Formatted OS error message.
        message: String,
    },

    /// Reading or writing the binary index cache failed.
    #[error("cache error at `{path}`: {reason}")]
    Cache {
        /// Path to the cache file or directory.
        path: String,
        /// Detailed cache failure reason.
        reason: String,
    },

    /// Indexing was cancelled before completion.
    #[error("indexing cancelled")]
    Cancelled,
}
