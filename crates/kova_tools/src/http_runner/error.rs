//! Error type shared by the `.http` runner (parsing, variables, execution).

use thiserror::Error;

/// Failure raised while resolving or executing an `.http` request.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HttpRunnerError {
    /// No request starts at or spans the given 0-based line.
    #[error("no HTTP request at line {line}")]
    NoRequestAtLine {
        /// 0-based line that was clicked.
        line: usize,
    },
    /// A `{{name}}` placeholder has no value in any variable source.
    #[error("unresolved variable '{{{{{name}}}}}'")]
    UnresolvedVariable {
        /// Variable name without braces.
        name: String,
    },
    /// The resolved URL is not a valid absolute `http`/`https` URL.
    #[error("invalid URL '{url}': {message}")]
    InvalidUrl {
        /// URL after variable substitution.
        url: String,
        /// Parser diagnostic.
        message: String,
    },
    /// A header name or value is not valid HTTP.
    #[error("invalid header '{name}'")]
    InvalidHeader {
        /// Header name as written in the request.
        name: String,
    },
    /// The HTTP method token is not valid.
    #[error("invalid HTTP method '{method}'")]
    InvalidMethod {
        /// Method token as written in the request.
        method: String,
    },
    /// An environment file (`http-client.env.json` / `kova.toml`) could not be read or parsed.
    #[error("cannot load HTTP environment from '{path}': {message}")]
    Environment {
        /// Path of the offending file.
        path: String,
        /// Underlying diagnostic.
        message: String,
    },
    /// The requested environment name is not declared in any environment source.
    #[error("unknown HTTP environment '{name}'")]
    UnknownEnvironment {
        /// Requested environment name.
        name: String,
    },
    /// The request did not complete before the configured timeout.
    #[error("request timed out after {timeout_ms} ms")]
    Timeout {
        /// Configured timeout in milliseconds.
        timeout_ms: u64,
    },
    /// The request was cancelled by the user.
    #[error("request cancelled")]
    Cancelled,
    /// Transport-level failure (DNS, TLS, connection reset, too many redirects, ...).
    #[error("request failed: {message}")]
    Transport {
        /// Underlying diagnostic.
        message: String,
    },
    /// The HTTP client or its runtime could not be initialized.
    #[error("HTTP client initialization failed: {message}")]
    Client {
        /// Underlying diagnostic.
        message: String,
    },
}
