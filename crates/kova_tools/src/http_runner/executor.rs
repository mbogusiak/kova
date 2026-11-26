//! Asynchronous HTTP execution (`reqwest` + `rustls`) with timeouts, redirects and cancellation.
//!
//! [`HttpExecutor::execute`] is a plain `async fn` that must run on a Tokio runtime.
//! [`HttpExecutor::spawn`] owns that runtime on a dedicated background thread and returns an
//! [`HttpRequestHandle`] whose result future only awaits a Tokio `oneshot` channel, so it can be
//! awaited from any executor (including GPUI's) without blocking the UI thread.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use tokio::sync::{oneshot, watch};

use crate::http_runner::env::ResolvedHttpRequest;
use crate::http_runner::error::HttpRunnerError;
use crate::http_runner::parser::HttpHeader;

/// Tunables of [`HttpExecutor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpExecutorConfig {
    /// Total request timeout (connect + headers + body).
    pub timeout: Duration,
    /// TCP/TLS connect timeout.
    pub connect_timeout: Duration,
    /// Maximum number of redirects followed (`0` disables redirects).
    pub max_redirects: usize,
    /// Upper bound of body bytes kept in memory; the rest is discarded and `truncated` is set.
    pub max_body_bytes: usize,
}

impl Default for HttpExecutorConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_redirects: 10,
            max_body_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Response metadata and raw body collected by [`HttpExecutor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponseData {
    /// HTTP status code (e.g. `200`).
    pub status: u16,
    /// Canonical reason phrase (e.g. `OK`), when known.
    pub reason: Option<String>,
    /// Protocol version label (e.g. `HTTP/1.1`).
    pub http_version: String,
    /// URL after following redirects.
    pub final_url: String,
    /// Wall-clock duration from sending the request until the body was read.
    pub duration_ms: u64,
    /// Number of body bytes received (including bytes discarded beyond `max_body_bytes`).
    pub size_bytes: u64,
    /// Response headers in wire order.
    pub headers: Vec<HttpHeader>,
    /// Raw body bytes (at most `max_body_bytes`).
    pub body: Vec<u8>,
    /// `true` when the body exceeded `max_body_bytes` and was cut.
    pub truncated: bool,
}

impl HttpResponseData {
    /// Returns the first header named `name` (case-insensitive).
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    /// Returns the `Content-Type` header value, if present.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.header("content-type")
    }
}

/// Cloneable cancellation signal for an in-flight request.
#[derive(Debug, Clone)]
pub struct HttpCancelToken {
    tx: Arc<watch::Sender<bool>>,
}

impl Default for HttpCancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpCancelToken {
    /// Creates a token in the not-cancelled state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(false).0),
        }
    }

    /// Requests cancellation of every request observing this token.
    pub fn cancel(&self) {
        self.tx.send_replace(true);
    }

    /// Returns `true` once [`Self::cancel`] was called.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.tx.borrow()
    }

    /// Resolves once the token is cancelled.
    pub async fn cancelled(&self) {
        let mut rx = self.tx.subscribe();
        // `wait_for` only fails when the sender is dropped, which cannot happen while `self` lives.
        let _ignored = rx.wait_for(|cancelled| *cancelled).await.map(|_| ());
    }
}

type ResponseResult = Result<HttpResponseData, HttpRunnerError>;

/// Handle to a request running on a background thread (see [`HttpExecutor::spawn`]).
#[derive(Debug)]
pub struct HttpRequestHandle {
    cancel: HttpCancelToken,
    result: oneshot::Receiver<ResponseResult>,
}

impl HttpRequestHandle {
    /// Cancels the request; its result becomes [`HttpRunnerError::Cancelled`].
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// Returns a token that can cancel the request after the handle is consumed.
    #[must_use]
    pub fn cancel_token(&self) -> HttpCancelToken {
        self.cancel.clone()
    }

    /// Awaits the result. Runtime-agnostic: safe to await from any executor.
    ///
    /// # Errors
    ///
    /// Returns the request failure, or [`HttpRunnerError::Cancelled`] if the worker vanished.
    pub async fn result(self) -> ResponseResult {
        self.result.await.unwrap_or(Err(HttpRunnerError::Cancelled))
    }

    /// Blocks the current thread until the result is available (never call on the UI thread).
    ///
    /// # Errors
    ///
    /// Same as [`Self::result`].
    pub fn blocking_result(self) -> ResponseResult {
        self.result
            .blocking_recv()
            .unwrap_or(Err(HttpRunnerError::Cancelled))
    }
}

/// Reusable HTTP client for `.http` requests.
#[derive(Debug, Clone)]
pub struct HttpExecutor {
    client: reqwest::Client,
    config: HttpExecutorConfig,
}

impl HttpExecutor {
    /// Builds an executor with `config`.
    ///
    /// # Errors
    ///
    /// Returns [`HttpRunnerError::Client`] if the TLS backend cannot be initialized.
    pub fn new(config: HttpExecutorConfig) -> Result<Self, HttpRunnerError> {
        let redirect = if config.max_redirects == 0 {
            Policy::none()
        } else {
            Policy::limited(config.max_redirects)
        };
        let client = reqwest::Client::builder()
            .user_agent(concat!("Kova-HTTP-Runner/", env!("CARGO_PKG_VERSION")))
            .redirect(redirect)
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(|e| HttpRunnerError::Client {
                message: e.to_string(),
            })?;
        Ok(Self { client, config })
    }

    /// Returns the active configuration.
    #[must_use]
    pub const fn config(&self) -> &HttpExecutorConfig {
        &self.config
    }

    /// Executes `request` on the current Tokio runtime, honoring `cancel` and the timeout.
    ///
    /// # Errors
    ///
    /// Returns validation errors (`InvalidUrl`, `InvalidMethod`, `InvalidHeader`), `Timeout`,
    /// `Cancelled` or `Transport`.
    pub async fn execute(
        &self,
        request: &ResolvedHttpRequest,
        cancel: &HttpCancelToken,
    ) -> ResponseResult {
        let builder = self.build_request(request)?;
        if cancel.is_cancelled() {
            return Err(HttpRunnerError::Cancelled);
        }
        let timeout = self.config.timeout;
        let run = self.send_and_collect(builder);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(HttpRunnerError::Cancelled),
            res = tokio::time::timeout(timeout, run) => res.unwrap_or_else(|_| {
                Err(HttpRunnerError::Timeout {
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                })
            }),
        }
    }

    /// Runs `request` on a dedicated background thread with its own Tokio runtime.
    #[must_use]
    pub fn spawn(&self, request: ResolvedHttpRequest) -> HttpRequestHandle {
        let (tx, rx) = oneshot::channel();
        let cancel = HttpCancelToken::new();
        let worker_cancel = cancel.clone();
        let executor = self.clone();
        let spawned = std::thread::Builder::new()
            .name("kova-http-runner".to_owned())
            .spawn(move || {
                let result = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt.block_on(executor.execute(&request, &worker_cancel)),
                    Err(e) => Err(HttpRunnerError::Client {
                        message: e.to_string(),
                    }),
                };
                // The receiver may have been dropped (UI closed); nothing left to report.
                let _ignored = tx.send(result);
            });
        if let Err(e) = spawned {
            let (tx, rx) = oneshot::channel();
            let _ignored = tx.send(Err(HttpRunnerError::Client {
                message: e.to_string(),
            }));
            return HttpRequestHandle { cancel, result: rx };
        }
        HttpRequestHandle { cancel, result: rx }
    }

    fn build_request(
        &self,
        request: &ResolvedHttpRequest,
    ) -> Result<reqwest::RequestBuilder, HttpRunnerError> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| {
            HttpRunnerError::InvalidMethod {
                method: request.method.clone(),
            }
        })?;
        let url = reqwest::Url::parse(&request.url).map_err(|e| HttpRunnerError::InvalidUrl {
            url: request.url.clone(),
            message: e.to_string(),
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(HttpRunnerError::InvalidUrl {
                url: request.url.clone(),
                message: format!("unsupported scheme '{}'", url.scheme()),
            });
        }
        let mut builder = self.client.request(method, url);
        for header in &request.headers {
            let invalid = || HttpRunnerError::InvalidHeader {
                name: header.name.clone(),
            };
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| invalid())?;
            let value = HeaderValue::from_str(&header.value).map_err(|_| invalid())?;
            builder = builder.header(name, value);
        }
        if let Some(body) = &request.body {
            builder = builder.body(body.clone());
        }
        Ok(builder)
    }

    async fn send_and_collect(&self, builder: reqwest::RequestBuilder) -> ResponseResult {
        let started = Instant::now();
        let mut response = builder.send().await.map_err(transport)?;

        let status = response.status();
        let http_version = format!("{:?}", response.version());
        let final_url = response.url().to_string();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                HttpHeader::new(name.as_str(), String::from_utf8_lossy(value.as_bytes()))
            })
            .collect();

        let limit = self.config.max_body_bytes;
        let mut body = Vec::new();
        let mut size_bytes = 0u64;
        let mut truncated = false;
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            size_bytes += chunk.len() as u64;
            let room = limit.saturating_sub(body.len());
            if chunk.len() > room {
                truncated = true;
            }
            body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }

        Ok(HttpResponseData {
            status: status.as_u16(),
            reason: status.canonical_reason().map(str::to_owned),
            http_version,
            final_url,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            size_bytes,
            headers,
            body,
            truncated,
        })
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "used as a map_err adapter taking the error by value"
)]
fn transport(err: reqwest::Error) -> HttpRunnerError {
    HttpRunnerError::Transport {
        message: error_chain(&err),
    }
}

fn error_chain(err: &dyn std::error::Error) -> String {
    let mut message = err.to_string();
    let mut source = err.source();
    while let Some(inner) = source {
        message.push_str(": ");
        message.push_str(&inner.to_string());
        source = inner.source();
    }
    message
}
