//! Typed error hierarchy for `kova_toolchain`.

use thiserror::Error;

/// Errors returned by JDK discovery, archive verification/extraction, and Temurin provisioning.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ToolchainError {
    /// A Java version string or JDK requirement specification could not be parsed.
    #[error("invalid Java version or requirement `{raw}`: {reason}")]
    InvalidJavaVersion {
        /// Raw input string that failed parsing.
        raw: String,
        /// Human-readable reason explaining the parse failure.
        reason: String,
    },

    /// No installed or downloadable JDK matched the requested requirement.
    #[error("no matching JDK found for requirement `{requirement}`")]
    JdkNotFound {
        /// Requirement description that could not be satisfied.
        requirement: String,
    },

    /// A discovered JDK has a major version below the required minimum runtime version.
    #[error("JDK at `{path}` has version {found_major}, expected >= {min_major}")]
    UnsupportedJdkRuntime {
        /// Path to the JDK home directory.
        path: String,
        /// Major Java version found at `path`.
        found_major: u16,
        /// Minimum required major Java version.
        min_major: u16,
    },

    /// Downloading an artifact or querying a remote metadata endpoint failed.
    #[error("failed to download artifact `{url}`: {reason}")]
    DownloadFailed {
        /// URL that failed to download.
        url: String,
        /// Underlying failure reason.
        reason: String,
    },

    /// SHA-256 checksum verification failed for a downloaded artifact.
    #[error("SHA-256 checksum mismatch for `{artifact}`: expected {expected}, got {actual}")]
    ChecksumMismatch {
        /// Artifact path or identifier.
        artifact: String,
        /// Expected lowercase hex SHA-256 digest.
        expected: String,
        /// Actual computed lowercase hex SHA-256 digest.
        actual: String,
    },

    /// Extracting an archive failed due to corruption or I/O errors.
    #[error("archive extraction failed for `{archive}`: {reason}")]
    ArchiveExtraction {
        /// Archive path.
        archive: String,
        /// Failure reason.
        reason: String,
    },

    /// An archive entry attempted path traversal (Zip-Slip / Tar-Slip) or an escaping symlink.
    #[error("unsafe archive entry path blocked in `{archive}`: `{entry}`")]
    PathTraversalBlocked {
        /// Archive path.
        archive: String,
        /// Offending entry path or link target.
        entry: String,
    },

    /// A required Omakase tool is missing from its expected installation path.
    #[error("required Omakase tool `{tool}` is not installed at `{expected_path}`")]
    ToolNotInstalled {
        /// Tool name.
        tool: String,
        /// Expected installation path.
        expected_path: String,
    },

    /// An Omakase tool is configured or installed in an unusable way (missing manifest
    /// entry, ambiguous launcher, missing platform configuration directory, ...).
    #[error("invalid Omakase tool `{tool}` installation: {reason}")]
    InvalidToolInstallation {
        /// Tool name.
        tool: String,
        /// Human-readable description of the problem.
        reason: String,
    },

    /// An external tool process exited with a non-zero status or failed to start.
    #[error("external tool `{program}` failed ({status}): {stderr}")]
    ProcessFailed {
        /// Program name or path.
        program: String,
        /// Exit status or failure description.
        status: String,
        /// Captured standard error output.
        stderr: String,
    },

    /// A filesystem I/O operation failed.
    #[error("I/O error at `{path}`: {message}")]
    Io {
        /// Path associated with the I/O operation.
        path: String,
        /// Error message.
        message: String,
    },

    /// The toolchain operation was cancelled by the caller.
    #[error("toolchain operation cancelled")]
    Cancelled,
}

impl ToolchainError {
    /// Constructs a [`ToolchainError::Io`] from a path and [`std::io::Error`].
    #[must_use]
    pub fn io(path: impl Into<String>, err: &std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            message: err.to_string(),
        }
    }
}
