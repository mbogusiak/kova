//! Native `.http` request runner (`KOVA-042`): parsing, `{{var}}` substitution,
//! asynchronous execution and response formatting — fully headless.
//!
//! Typical flow (the UI only renders the outcome):
//! 1. [`HttpFile::parse`] the editor text;
//! 2. [`prepare_request_at_line`] for the gutter `▶` line, with [`HttpEnvironments`];
//! 3. [`HttpExecutor::spawn`] the resolved request and await [`HttpRequestHandle::result`];
//! 4. format the [`HttpResponseData`] with the [`response`] helpers.

pub mod env;
pub mod error;
pub mod executor;
pub mod parser;
pub mod response;

pub use env::{
    ENV_FILE_NAME, HttpEnvironments, HttpVariables, PRIVATE_ENV_FILE_NAME, ResolvedHttpRequest,
    discover_project_root,
};
pub use error::HttpRunnerError;
pub use executor::{
    HttpCancelToken, HttpExecutor, HttpExecutorConfig, HttpRequestHandle, HttpResponseData,
};
pub use parser::{HttpFile, HttpHeader, HttpRequestSpec, SUPPORTED_METHODS, parse_http_file};
pub use response::{
    HttpBodyKind, HttpBodyPreview, HttpStatusClass, PRETTY_PRINT_LIMIT_BYTES, body_preview,
    detect_body_kind, format_duration_ms, format_headers, format_raw, format_size, format_status,
    pretty_print_json,
};

/// Finds the request at 0-based `line` in `file` and substitutes its variables.
///
/// Variables come from `environments` (selected by `environment`, see
/// [`HttpEnvironments::variables_for`]) overlaid with the file-level `@name = value`
/// declarations of `file`.
///
/// # Errors
///
/// Returns [`HttpRunnerError::NoRequestAtLine`], [`HttpRunnerError::UnknownEnvironment`] or
/// [`HttpRunnerError::UnresolvedVariable`].
pub fn prepare_request_at_line(
    file: &HttpFile,
    line: usize,
    environments: &HttpEnvironments,
    environment: Option<&str>,
) -> Result<(HttpRequestSpec, ResolvedHttpRequest), HttpRunnerError> {
    let spec = file
        .request_at_line(line)
        .ok_or(HttpRunnerError::NoRequestAtLine { line })?;
    let mut variables = environments.variables_for(environment)?;
    variables.extend(file.variables.clone());
    let resolved = variables.resolve(spec)?;
    Ok((spec.clone(), resolved))
}
