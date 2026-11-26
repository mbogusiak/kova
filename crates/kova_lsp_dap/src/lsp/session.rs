//! JSON-RPC session multiplexer: request/response routing, timeouts, `$/cancelRequest`
//! and server-to-client requests.
//!
//! [`LspSession::from_streams`] spawns two `tokio` tasks:
//! - the **writer task** drains a bounded `mpsc` queue of serialized messages into the stream,
//! - the **reader task** decodes frames and routes responses to waiting callers (`oneshot`),
//!   notifications to a `broadcast` channel (plus an optional synchronous hook) and
//!   server requests to a [`ServerRequestHandler`] running in its own task, so a slow handler
//!   never blocks the reading of further frames (no `initialize` / `workspace/configuration`
//!   deadlock with `jdt.ls`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, BufReader};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::error::LspDapError;
use crate::lsp::transport::{
    IncomingMessage, JsonRpcError, JsonRpcId, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
    read_lsp_frame, write_lsp_frame,
};

/// Boxed future returned by [`ServerRequestHandler::handle`].
pub type ServerRequestFuture = Pin<Box<dyn Future<Output = Result<Value, JsonRpcError>> + Send>>;

/// Synchronous hook invoked by the reader task for every notification, before broadcasting.
///
/// Must be cheap and non-blocking; used by [`crate::lsp::JdtlsClient`] to update its state
/// without the risk of lagging behind a `broadcast` channel.
pub type NotificationHook = Arc<dyn Fn(&JsonRpcNotification) + Send + Sync>;

/// Answers requests sent by the server to the client (e.g. `workspace/configuration`).
pub trait ServerRequestHandler: Send + Sync + 'static {
    /// Handles one server request; the returned value becomes the JSON-RPC `result`.
    fn handle(&self, method: &str, params: Value) -> ServerRequestFuture;
}

type PendingMap = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, LspDapError>>>>>;

/// Default capacity of the outgoing message queue.
const DEFAULT_OUTGOING_CAPACITY: usize = 256;
/// Default capacity of the notification broadcast channel.
const DEFAULT_NOTIFICATION_CAPACITY: usize = 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Construction options for [`LspSession::from_streams_with_options`].
#[derive(Clone)]
pub struct SessionOptions {
    /// Handler for server-to-client requests (`MethodNotFound` is answered when absent).
    pub server_handler: Option<Arc<dyn ServerRequestHandler>>,
    /// Synchronous hook run for every incoming notification.
    pub notification_hook: Option<NotificationHook>,
    /// Bounded capacity of the outgoing queue.
    pub outgoing_capacity: usize,
    /// Capacity of the notification broadcast channel.
    pub notification_capacity: usize,
}

impl Default for SessionOptions {
    fn default() -> Self {
        Self {
            server_handler: None,
            notification_hook: None,
            outgoing_capacity: DEFAULT_OUTGOING_CAPACITY,
            notification_capacity: DEFAULT_NOTIFICATION_CAPACITY,
        }
    }
}

impl fmt::Debug for SessionOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionOptions")
            .field("server_handler", &self.server_handler.is_some())
            .field("notification_hook", &self.notification_hook.is_some())
            .field("outgoing_capacity", &self.outgoing_capacity)
            .field("notification_capacity", &self.notification_capacity)
            .finish()
    }
}

struct SessionInner {
    outgoing: mpsc::Sender<Vec<u8>>,
    next_id: AtomicI64,
    pending: PendingMap,
    notifications: broadcast::Sender<JsonRpcNotification>,
    closed: watch::Receiver<bool>,
    reader_task: JoinHandle<()>,
    writer_task: JoinHandle<()>,
}

impl Drop for SessionInner {
    fn drop(&mut self) {
        self.reader_task.abort();
        self.writer_task.abort();
    }
}

/// Bidirectional JSON-RPC session over an arbitrary async byte stream.
///
/// Cloning is cheap; the background tasks stop when the last clone is dropped.
#[derive(Clone)]
pub struct LspSession {
    inner: Arc<SessionInner>,
}

impl fmt::Debug for LspSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LspSession")
            .field("pending_requests", &self.pending_requests())
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl LspSession {
    /// Starts a session on `reader` / `writer` (e.g. `ChildStdout` / `ChildStdin` or the halves
    /// of `tokio::io::duplex`). Must be called from within a `tokio` runtime.
    pub fn from_streams<R, W>(
        reader: R,
        writer: W,
        server_handler: Option<Arc<dyn ServerRequestHandler>>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self::from_streams_with_options(
            reader,
            writer,
            SessionOptions {
                server_handler,
                ..SessionOptions::default()
            },
        )
    }

    /// Starts a session with explicit [`SessionOptions`]. Must be called from within a `tokio`
    /// runtime.
    pub fn from_streams_with_options<R, W>(reader: R, writer: W, options: SessionOptions) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (outgoing, outgoing_rx) = mpsc::channel(options.outgoing_capacity.max(1));
        let (notifications, _) = broadcast::channel(options.notification_capacity.max(1));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (closed_tx, closed) = watch::channel(false);
        let closed_tx = Arc::new(closed_tx);

        let writer_task = tokio::spawn(writer_loop(writer, outgoing_rx, Arc::clone(&closed_tx)));
        let reader_task = tokio::spawn(reader_loop(ReaderContext {
            reader: BufReader::new(reader),
            outgoing: outgoing.clone(),
            pending: Arc::clone(&pending),
            notifications: notifications.clone(),
            hook: options.notification_hook,
            handler: options.server_handler,
            closed: closed_tx,
        }));

        Self {
            inner: Arc::new(SessionInner {
                outgoing,
                next_id: AtomicI64::new(1),
                pending,
                notifications,
                closed,
                reader_task,
                writer_task,
            }),
        }
    }

    /// Sends a request and waits for its typed result.
    ///
    /// On timeout, or when the returned future is dropped before completion, the pending entry
    /// is removed and a best-effort `$/cancelRequest` is sent.
    ///
    /// # Errors
    /// [`LspDapError::RequestTimeout`], [`LspDapError::TransportClosed`],
    /// [`LspDapError::ProtocolError`] (error response), [`LspDapError::RequestCancelled`]
    /// (server answered `RequestCancelled`) or [`LspDapError::SerializationError`].
    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
        timeout: Duration,
    ) -> Result<R, LspDapError> {
        let params = serde_json::to_value(params)?;
        let value = self.request_value(method, params, timeout).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Untyped variant of [`Self::request`].
    ///
    /// # Errors
    /// Same as [`Self::request`].
    pub async fn request_value(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, LspDapError> {
        if self.is_closed() {
            return Err(LspDapError::TransportClosed);
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let payload =
            serde_json::to_vec(&JsonRpcRequest::new(JsonRpcId::Number(id), method, params))?;
        let (tx, rx) = oneshot::channel();
        lock(&self.inner.pending).insert(id, tx);
        let mut guard = PendingGuard {
            id,
            pending: Arc::clone(&self.inner.pending),
            outgoing: self.inner.outgoing.clone(),
            send_cancel: false,
            armed: true,
        };
        // The reader may have drained `pending` between the check above and the insert.
        if self.is_closed() {
            return Err(LspDapError::TransportClosed);
        }
        self.inner
            .outgoing
            .send(payload)
            .await
            .map_err(|_| LspDapError::TransportClosed)?;
        guard.send_cancel = true;

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => {
                guard.armed = false;
                result.map_err(|err| match err {
                    LspDapError::ProtocolError { code, .. }
                        if code == JsonRpcError::REQUEST_CANCELLED =>
                    {
                        LspDapError::RequestCancelled {
                            method: method.to_owned(),
                        }
                    }
                    other => other,
                })
            }
            Ok(Err(_)) => {
                guard.armed = false;
                Err(LspDapError::TransportClosed)
            }
            Err(_) => {
                drop(guard);
                Err(LspDapError::RequestTimeout {
                    method: method.to_owned(),
                    timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                })
            }
        }
    }

    /// Sends a notification.
    ///
    /// # Errors
    /// [`LspDapError::SerializationError`] or [`LspDapError::TransportClosed`].
    pub async fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<(), LspDapError> {
        let params = serde_json::to_value(params)?;
        let payload = serde_json::to_vec(&JsonRpcNotification::new(method, params))?;
        self.inner
            .outgoing
            .send(payload)
            .await
            .map_err(|_| LspDapError::TransportClosed)
    }

    /// Subscribes to incoming notifications (only messages received after this call).
    #[must_use]
    pub fn subscribe_notifications(&self) -> broadcast::Receiver<JsonRpcNotification> {
        self.inner.notifications.subscribe()
    }

    /// Number of requests still waiting for a response.
    #[must_use]
    pub fn pending_requests(&self) -> usize {
        lock(&self.inner.pending).len()
    }

    /// Whether the transport has been closed (EOF, framing or I/O error).
    #[must_use]
    pub fn is_closed(&self) -> bool {
        *self.inner.closed.borrow()
    }

    /// Watch channel flipping to `true` once the transport is closed.
    #[must_use]
    pub fn closed_watch(&self) -> watch::Receiver<bool> {
        self.inner.closed.clone()
    }
}

/// RAII guard removing a pending entry and sending `$/cancelRequest` when a request is abandoned.
struct PendingGuard {
    id: i64,
    pending: PendingMap,
    outgoing: mpsc::Sender<Vec<u8>>,
    send_cancel: bool,
    armed: bool,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let was_pending = lock(&self.pending).remove(&self.id).is_some();
        if was_pending && self.send_cancel {
            let cancel = JsonRpcNotification::new("$/cancelRequest", json!({ "id": self.id }));
            if let Ok(payload) = serde_json::to_vec(&cancel) {
                // Best effort: a full queue or closed transport makes cancellation moot.
                let _ignored = self.outgoing.try_send(payload);
            }
        }
    }
}

async fn writer_loop<W: AsyncWrite + Unpin>(
    mut writer: W,
    mut outgoing: mpsc::Receiver<Vec<u8>>,
    closed: Arc<watch::Sender<bool>>,
) {
    while let Some(payload) = outgoing.recv().await {
        if write_lsp_frame(&mut writer, &payload).await.is_err() {
            closed.send_replace(true);
            break;
        }
    }
}

struct ReaderContext<R> {
    reader: BufReader<R>,
    outgoing: mpsc::Sender<Vec<u8>>,
    pending: PendingMap,
    notifications: broadcast::Sender<JsonRpcNotification>,
    hook: Option<NotificationHook>,
    handler: Option<Arc<dyn ServerRequestHandler>>,
    closed: Arc<watch::Sender<bool>>,
}

async fn reader_loop<R: AsyncRead + Unpin>(mut ctx: ReaderContext<R>) {
    loop {
        let Ok(Some(frame)) = read_lsp_frame(&mut ctx.reader).await else {
            break;
        };
        // Malformed JSON inside a well-framed message is skipped, not fatal.
        let Ok(message) = IncomingMessage::from_slice(&frame) else {
            continue;
        };
        match message {
            IncomingMessage::Response(response) => route_response(&ctx.pending, response),
            IncomingMessage::Notification(notification) => {
                if let Some(hook) = &ctx.hook {
                    hook(&notification);
                }
                // No subscribers is fine.
                let _ignored = ctx.notifications.send(notification);
            }
            IncomingMessage::Request(request) => {
                let handler = ctx.handler.clone();
                let outgoing = ctx.outgoing.clone();
                tokio::spawn(async move {
                    let response = match handler {
                        Some(handler) => {
                            match handler.handle(&request.method, request.params).await {
                                Ok(result) => JsonRpcResponse::success(request.id, result),
                                Err(error) => JsonRpcResponse::failure(request.id, error),
                            }
                        }
                        None => JsonRpcResponse::failure(
                            request.id,
                            JsonRpcError::new(
                                JsonRpcError::METHOD_NOT_FOUND,
                                format!("unhandled method `{}`", request.method),
                            ),
                        ),
                    };
                    if let Ok(payload) = serde_json::to_vec(&response) {
                        let _ignored = outgoing.send(payload).await;
                    }
                });
            }
        }
    }
    ctx.closed.send_replace(true);
    let drained: Vec<_> = lock(&ctx.pending).drain().map(|(_, tx)| tx).collect();
    for tx in drained {
        let _ignored = tx.send(Err(LspDapError::TransportClosed));
    }
}

fn route_response(pending: &PendingMap, response: JsonRpcResponse) {
    let JsonRpcId::Number(id) = response.id else {
        return;
    };
    let Some(tx) = lock(pending).remove(&id) else {
        return;
    };
    let result = match response.error {
        Some(error) => Err(LspDapError::ProtocolError {
            code: error.code,
            message: error.message,
        }),
        None => Ok(response.result.unwrap_or(Value::Null)),
    };
    // The caller may have given up in the meantime.
    let _ignored = tx.send(result);
}

/// Default answers to the server requests issued by Eclipse `jdt.ls`.
///
/// - `workspace/configuration`: values looked up (by dotted `section`) in a settings object,
///   `null` for unknown sections,
/// - `client/registerCapability` / `client/unregisterCapability`: accepted and tracked,
/// - `window/workDoneProgress/create`: accepted, token tracked,
/// - `workspace/applyEdit`: delegated to a registered handler (KOVA-037) or answered with
///   `{"applied": false}`,
/// - `window/showMessageRequest`: answered with `null` (no action chosen).
pub struct DefaultServerRequestHandler {
    settings: Mutex<Value>,
    registrations: Mutex<BTreeMap<String, String>>,
    progress_tokens: Mutex<BTreeSet<String>>,
    apply_edit: Mutex<Option<Arc<dyn ServerRequestHandler>>>,
}

impl fmt::Debug for DefaultServerRequestHandler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DefaultServerRequestHandler")
            .field("registrations", &*lock(&self.registrations))
            .field("progress_tokens", &*lock(&self.progress_tokens))
            .field("apply_edit_handler", &lock(&self.apply_edit).is_some())
            .finish_non_exhaustive()
    }
}

impl DefaultServerRequestHandler {
    /// Creates a handler answering `workspace/configuration` from `settings`
    /// (a JSON object such as `{"java": {...}, "kova": {...}}`).
    #[must_use]
    pub fn new(settings: Value) -> Self {
        Self {
            settings: Mutex::new(settings),
            registrations: Mutex::new(BTreeMap::new()),
            progress_tokens: Mutex::new(BTreeSet::new()),
            apply_edit: Mutex::new(None),
        }
    }

    /// Replaces the settings served to `workspace/configuration`.
    pub fn set_settings(&self, settings: Value) {
        *lock(&self.settings) = settings;
    }

    /// Registers the handler that applies server-initiated `workspace/applyEdit` requests.
    pub fn set_apply_edit_handler(&self, handler: Arc<dyn ServerRequestHandler>) {
        *lock(&self.apply_edit) = Some(handler);
    }

    /// Dynamic registrations currently active, as `(registration id, method)` pairs.
    #[must_use]
    pub fn registrations(&self) -> Vec<(String, String)> {
        lock(&self.registrations)
            .iter()
            .map(|(id, method)| (id.clone(), method.clone()))
            .collect()
    }

    /// Work-done progress tokens created by the server.
    #[must_use]
    pub fn progress_tokens(&self) -> Vec<String> {
        lock(&self.progress_tokens).iter().cloned().collect()
    }

    /// Resolves a dotted configuration `section` (`None` returns the whole settings object).
    #[must_use]
    pub fn configuration_value(&self, section: Option<&str>) -> Value {
        let settings = lock(&self.settings);
        let Some(section) = section.filter(|s| !s.is_empty()) else {
            return settings.clone();
        };
        section
            .split('.')
            .try_fold(&*settings, |node, key| node.get(key))
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn handle_configuration(&self, params: &Value) -> Value {
        let items = params
            .get("items")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        Value::Array(
            items
                .iter()
                .map(|item| self.configuration_value(item.get("section").and_then(Value::as_str)))
                .collect(),
        )
    }

    fn handle_register(&self, params: &Value) {
        let Some(list) = params.get("registrations").and_then(Value::as_array) else {
            return;
        };
        let mut registrations = lock(&self.registrations);
        for registration in list {
            if let (Some(id), Some(method)) = (
                registration.get("id").and_then(Value::as_str),
                registration.get("method").and_then(Value::as_str),
            ) {
                registrations.insert(id.to_owned(), method.to_owned());
            }
        }
    }

    fn handle_unregister(&self, params: &Value) {
        // The LSP specification spells the field `unregisterations`.
        let Some(list) = params
            .get("unregisterations")
            .or_else(|| params.get("unregistrations"))
            .and_then(Value::as_array)
        else {
            return;
        };
        let mut registrations = lock(&self.registrations);
        for registration in list {
            if let Some(id) = registration.get("id").and_then(Value::as_str) {
                registrations.remove(id);
            }
        }
    }

    fn handle_progress_create(&self, params: &Value) {
        if let Some(token) = params.get("token").and_then(token_to_string) {
            lock(&self.progress_tokens).insert(token);
        }
    }
}

/// Converts a `ProgressToken` (`string | integer`) to a string.
pub(crate) fn token_to_string(token: &Value) -> Option<String> {
    match token {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

impl ServerRequestHandler for DefaultServerRequestHandler {
    fn handle(&self, method: &str, params: Value) -> ServerRequestFuture {
        let result = match method {
            "workspace/configuration" => Ok(self.handle_configuration(&params)),
            "client/registerCapability" => {
                self.handle_register(&params);
                Ok(Value::Null)
            }
            "client/unregisterCapability" => {
                self.handle_unregister(&params);
                Ok(Value::Null)
            }
            "window/workDoneProgress/create" => {
                self.handle_progress_create(&params);
                Ok(Value::Null)
            }
            "window/showMessageRequest" => Ok(Value::Null),
            "workspace/applyEdit" => {
                let delegate = lock(&self.apply_edit).clone();
                if let Some(delegate) = delegate {
                    return delegate.handle(method, params);
                }
                Ok(json!({
                    "applied": false,
                    "failureReason": "workspace/applyEdit is not supported yet",
                }))
            }
            other => Err(JsonRpcError::new(
                JsonRpcError::METHOD_NOT_FOUND,
                format!("unhandled method `{other}`"),
            )),
        };
        Box::pin(std::future::ready(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_resolves_dotted_sections() {
        let handler = DefaultServerRequestHandler::new(json!({
            "java": { "format": { "enabled": true } },
        }));
        assert_eq!(
            handler.configuration_value(Some("java.format.enabled")),
            json!(true)
        );
        assert_eq!(
            handler.configuration_value(Some("java.missing")),
            Value::Null
        );
        assert_eq!(handler.configuration_value(Some("unknown")), Value::Null);
        assert_eq!(
            handler.configuration_value(None),
            json!({ "java": { "format": { "enabled": true } } })
        );
    }

    #[tokio::test]
    async fn tracks_registrations_and_progress_tokens() {
        let handler = Arc::new(DefaultServerRequestHandler::new(json!({})));
        let register = json!({ "registrations": [
            { "id": "r1", "method": "workspace/didChangeWatchedFiles" },
            { "id": "r2", "method": "textDocument/formatting" },
        ]});
        assert_eq!(
            handler.handle("client/registerCapability", register).await,
            Ok(Value::Null)
        );
        let unregister = json!({ "unregisterations": [{ "id": "r1", "method": "x" }] });
        assert_eq!(
            handler
                .handle("client/unregisterCapability", unregister)
                .await,
            Ok(Value::Null)
        );
        assert_eq!(
            handler.registrations(),
            vec![("r2".to_owned(), "textDocument/formatting".to_owned())]
        );
        let create = json!({ "token": 42 });
        assert_eq!(
            handler
                .handle("window/workDoneProgress/create", create)
                .await,
            Ok(Value::Null)
        );
        assert_eq!(handler.progress_tokens(), vec!["42".to_owned()]);
        let unknown = handler.handle("x/unknown", Value::Null).await;
        assert!(matches!(unknown, Err(e) if e.code == JsonRpcError::METHOD_NOT_FOUND));
        let apply = handler.handle("workspace/applyEdit", json!({})).await;
        assert!(matches!(apply, Ok(v) if v["applied"] == json!(false)));
    }
}
