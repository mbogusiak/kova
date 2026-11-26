//! Asynchronous DAP client: `Content-Length` framing, `seq` / `request_seq` correlation,
//! the adapter event stream and answers to reverse requests (`KOVA-041`).
//!
//! The client is generic over any `AsyncRead` / `AsyncWrite` pair, so the same code path
//! drives an in-memory mock (`tokio::io::duplex`) in tests and the TCP socket opened to
//! `java-debug` in the IDE ([`DapClient::connect_tcp`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::dap::protocol::{DapEvent, DapMessage, DapRequest, DapResponse};
use crate::error::LspDapError;
use crate::lsp::{read_lsp_frame, write_lsp_frame};

/// Default timeout of a single DAP request.
pub const DEFAULT_DAP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

type PendingMap = HashMap<i64, oneshot::Sender<DapResponse>>;

#[derive(Debug)]
struct Inner {
    next_seq: AtomicI64,
    pending: Mutex<PendingMap>,
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    closed: AtomicBool,
}

impl Inner {
    fn next_seq(&self) -> i64 {
        self.next_seq.fetch_add(1, Ordering::Relaxed)
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, PendingMap> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn send(&self, message: &DapMessage) -> Result<(), LspDapError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(LspDapError::TransportClosed);
        }
        self.outgoing
            .send(message.to_vec()?)
            .map_err(|_| LspDapError::TransportClosed)
    }
}

/// Handle to a running DAP connection (cheap to clone).
#[derive(Debug, Clone)]
pub struct DapClient {
    inner: Arc<Inner>,
    timeout: Duration,
}

/// A request that was sent but whose response has not been awaited yet.
///
/// Needed for `launch` / `attach`: `java-debug` answers them only after
/// `configurationDone`, so the session must keep configuring before awaiting the response.
#[derive(Debug)]
pub struct PendingRequest {
    command: String,
    seq: i64,
    rx: oneshot::Receiver<DapResponse>,
    inner: Arc<Inner>,
}

impl PendingRequest {
    /// `seq` of the request.
    #[must_use]
    pub const fn seq(&self) -> i64 {
        self.seq
    }

    /// Waits for the response body.
    ///
    /// # Errors
    /// [`LspDapError::RequestTimeout`] when `timeout` elapses, [`LspDapError::TransportClosed`]
    /// when the adapter disconnects, and [`LspDapError::DapRequestFailed`] for a
    /// `success: false` response.
    pub async fn wait(self, timeout: Duration) -> Result<Value, LspDapError> {
        let Self {
            command,
            seq,
            rx,
            inner,
        } = self;
        let response = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return Err(LspDapError::TransportClosed),
            Err(_) => {
                inner.pending().remove(&seq);
                return Err(LspDapError::RequestTimeout {
                    method: command,
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                });
            }
        };
        if response.success {
            Ok(response.body)
        } else {
            Err(LspDapError::DapRequestFailed {
                message: response.failure_message(),
                command,
            })
        }
    }
}

impl DapClient {
    /// Starts reader and writer tasks over `reader` / `writer` on the current `tokio`
    /// runtime and returns the client with the adapter event stream.
    ///
    /// The event stream ends when the adapter closes the connection.
    pub fn spawn<R, W>(reader: R, writer: W) -> (Self, mpsc::UnboundedReceiver<DapEvent>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<DapEvent>();
        let inner = Arc::new(Inner {
            next_seq: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            outgoing: out_tx,
            closed: AtomicBool::new(false),
        });
        tokio::spawn(write_loop(writer, out_rx));
        tokio::spawn(read_loop(reader, Arc::clone(&inner), event_tx));
        (
            Self {
                inner,
                timeout: DEFAULT_DAP_REQUEST_TIMEOUT,
            },
            event_rx,
        )
    }

    /// Connects to an adapter listening on `host:port` (the port returned by the `jdt.ls`
    /// command `vscode.java.startDebugSession`).
    ///
    /// # Errors
    /// Returns [`LspDapError::Io`] when the connection cannot be established.
    pub async fn connect_tcp(
        host: &str,
        port: u16,
    ) -> Result<(Self, mpsc::UnboundedReceiver<DapEvent>), LspDapError> {
        let stream = tokio::net::TcpStream::connect((host, port))
            .await
            .map_err(|err| LspDapError::io(format!("dap://{host}:{port}"), &err))?;
        stream.set_nodelay(true).ok();
        let (read, write) = stream.into_split();
        Ok(Self::spawn(read, write))
    }

    /// Overrides the default request timeout.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Default request timeout.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Whether the adapter connection is closed.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Sends a request without waiting for its response.
    ///
    /// # Errors
    /// [`LspDapError::TransportClosed`] when the connection is closed.
    pub fn start_request(
        &self,
        command: &str,
        arguments: Value,
    ) -> Result<PendingRequest, LspDapError> {
        let seq = self.inner.next_seq();
        let (tx, rx) = oneshot::channel();
        self.inner.pending().insert(seq, tx);
        let message = DapMessage::Request(DapRequest {
            seq,
            command: command.to_owned(),
            arguments,
        });
        if let Err(err) = self.inner.send(&message) {
            self.inner.pending().remove(&seq);
            return Err(err);
        }
        Ok(PendingRequest {
            command: command.to_owned(),
            seq,
            rx,
            inner: Arc::clone(&self.inner),
        })
    }

    /// Sends a request and waits for its response body with the default timeout.
    ///
    /// # Errors
    /// See [`PendingRequest::wait`].
    pub async fn request(&self, command: &str, arguments: Value) -> Result<Value, LspDapError> {
        self.start_request(command, arguments)?
            .wait(self.timeout)
            .await
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) {
    while let Some(bytes) = rx.recv().await {
        if write_lsp_frame(&mut writer, &bytes).await.is_err() {
            break;
        }
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    reader: R,
    inner: Arc<Inner>,
    events: mpsc::UnboundedSender<DapEvent>,
) {
    let mut reader = BufReader::new(reader);
    while let Ok(Some(frame)) = read_lsp_frame(&mut reader).await {
        // A malformed frame is skipped; the adapter may still recover.
        let Ok(message) = DapMessage::from_slice(&frame) else {
            continue;
        };
        match message {
            DapMessage::Response(response) => {
                let waiter = inner.pending().remove(&response.request_seq);
                if let Some(waiter) = waiter {
                    drop(waiter.send(response));
                }
            }
            DapMessage::Event(event) => {
                drop(events.send(event));
            }
            DapMessage::Request(request) => answer_reverse_request(&inner, &request),
        }
    }
    inner.closed.store(true, Ordering::Release);
    // Dropping the senders wakes every waiter with `TransportClosed`.
    inner.pending().clear();
}

/// Reverse requests (`runInTerminal`, `startDebugging`) are not supported: Kova runs the
/// debuggee with `console: internalConsole`, so the adapter gets an explicit failure
/// instead of waiting forever.
fn answer_reverse_request(inner: &Inner, request: &DapRequest) {
    let response = DapMessage::Response(DapResponse {
        seq: inner.next_seq(),
        request_seq: request.seq,
        success: false,
        command: request.command.clone(),
        message: Some(format!("`{}` is not supported by Kova", request.command)),
        body: json!({}),
    });
    drop(inner.send(&response));
}
