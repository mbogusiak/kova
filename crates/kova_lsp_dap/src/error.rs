//! Error hierarchy shared by the LSP, DAP and BSP bridges.

/// Errors produced by the language-server / debug-adapter bridge.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LspDapError {
    /// The server process could not be spawned.
    #[error("failed to spawn language server `{program}`: {reason}")]
    SpawnFailed {
        /// Executable that failed to start.
        program: String,
        /// Underlying OS error.
        reason: String,
    },

    /// The underlying stream or session was closed.
    #[error("LSP/DAP transport closed unexpectedly")]
    TransportClosed,

    /// A frame violated the `Content-Length` framing rules.
    #[error("invalid JSON-RPC frame: {reason}")]
    FramingError {
        /// Human readable description of the violation.
        reason: String,
    },

    /// A payload could not be (de)serialized.
    #[error("JSON serialization/deserialization failed: {reason}")]
    SerializationError {
        /// Underlying `serde_json` error.
        reason: String,
    },

    /// The peer answered with a JSON-RPC error object.
    #[error("JSON-RPC error ({code}): {message}")]
    ProtocolError {
        /// JSON-RPC error code.
        code: i32,
        /// Error message sent by the peer.
        message: String,
    },

    /// A request did not receive a response in time (a `$/cancelRequest` was sent).
    #[error("request `{method}` timed out after {timeout_ms} ms")]
    RequestTimeout {
        /// Request method.
        method: String,
        /// Timeout that elapsed, in milliseconds.
        timeout_ms: u64,
    },

    /// A request was cancelled by the peer or the caller.
    #[error("request `{method}` was cancelled")]
    RequestCancelled {
        /// Request method.
        method: String,
    },

    /// The operation is not allowed in the current lifecycle state.
    #[error("invalid LSP lifecycle state `{current}`, expected `{expected}`")]
    InvalidState {
        /// Current state.
        current: String,
        /// Expected state(s).
        expected: String,
    },

    /// The session is hibernated and must be woken up first (KOVA-038).
    #[error("session for `{project_root}` is currently hibernated")]
    SessionHibernated {
        /// Project root of the hibernated session.
        project_root: String,
    },

    /// A path or URI could not be converted.
    #[error("invalid document URI `{uri}`: {reason}")]
    InvalidUri {
        /// Offending URI or path.
        uri: String,
        /// Why the conversion failed.
        reason: String,
    },

    /// Vineflower decompilation failed (KOVA-037).
    #[error("decompilation failed for `{target}`: {reason}")]
    DecompilationFailed {
        /// Class or jar that failed to decompile.
        target: String,
        /// Underlying failure.
        reason: String,
    },

    /// A caller-supplied argument was rejected before contacting the server (KOVA-037).
    #[error("invalid argument: {reason}")]
    InvalidArgument {
        /// Why the argument was rejected.
        reason: String,
    },

    /// A modification targeted a read-only document such as `jdt://` or the decompiled-source
    /// cache (KOVA-037).
    #[error("document `{uri}` is read-only")]
    ReadOnlyDocument {
        /// URI of the read-only document.
        uri: String,
    },

    /// A `WorkspaceEdit` could not be normalised (e.g. overlapping ranges) (KOVA-037).
    #[error("invalid workspace edit: {reason}")]
    InvalidWorkspaceEdit {
        /// Why the edit was rejected.
        reason: String,
    },

    /// An I/O operation failed.
    #[error("I/O error at `{path}`: {message}")]
    Io {
        /// Path or logical stream name.
        path: String,
        /// Underlying OS error.
        message: String,
    },
}

impl LspDapError {
    /// Builds an [`LspDapError::Io`] for a logical stream or file path.
    pub(crate) fn io(path: impl Into<String>, err: &std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            message: err.to_string(),
        }
    }

    /// Builds an [`LspDapError::FramingError`].
    pub(crate) fn framing(reason: impl Into<String>) -> Self {
        Self::FramingError {
            reason: reason.into(),
        }
    }
}

impl From<serde_json::Error> for LspDapError {
    fn from(err: serde_json::Error) -> Self {
        Self::SerializationError {
            reason: err.to_string(),
        }
    }
}
