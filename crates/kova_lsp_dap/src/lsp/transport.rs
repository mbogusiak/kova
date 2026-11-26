//! JSON-RPC 2.0 message model and `Content-Length` framing (LSP base protocol).
//!
//! Frames look like `Content-Length: <n>\r\n[other headers]\r\n\r\n<n bytes of UTF-8 JSON>`.
//! The reader accepts lower-case header names, bare `\n` line endings, unknown headers
//! (such as `Content-Type`) and arbitrarily fragmented input.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::LspDapError;

/// Hard upper bound for a single message body (64 MiB).
pub const MAX_LSP_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Upper bound for the header section of a single frame (protects against garbage streams).
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Logical stream name used in [`LspDapError::Io`].
const STREAM_NAME: &str = "lsp-stream";

/// JSON-RPC protocol version string.
pub const JSONRPC_VERSION: &str = "2.0";

/// JSON-RPC request identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    /// Numeric identifier (used by Kova for all outgoing requests).
    Number(i64),
    /// String identifier (used by some servers, e.g. for server-to-client requests).
    Text(String),
}

/// JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Error code (e.g. `-32601` for "method not found").
    pub code: i32,
    /// Short description.
    pub message: String,
    /// Optional structured payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// `MethodNotFound` error code.
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// `InvalidParams` error code.
    pub const INVALID_PARAMS: i32 = -32602;
    /// `InternalError` error code.
    pub const INTERNAL_ERROR: i32 = -32603;
    /// LSP `RequestCancelled` error code.
    pub const REQUEST_CANCELLED: i32 = -32800;

    /// Creates an error without a `data` payload.
    #[must_use]
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// JSON-RPC request (carries an `id` and expects a response).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request identifier.
    pub id: JsonRpcId,
    /// Method name.
    pub method: String,
    /// Parameters (omitted when `null`).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl JsonRpcRequest {
    /// Creates a `2.0` request.
    #[must_use]
    pub fn new(id: JsonRpcId, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC response (exactly one of `result` / `error` is meaningful).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Identifier of the answered request.
    pub id: JsonRpcId,
    /// Successful result (`None` is equivalent to `null`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Error object, if the request failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// Creates a successful response (`result` is always serialized, even when `null`).
    #[must_use]
    pub fn success(id: JsonRpcId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Creates an error response.
    #[must_use]
    pub fn failure(id: JsonRpcId, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// JSON-RPC notification (no `id`, no response).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Method name.
    pub method: String,
    /// Parameters (omitted when `null`).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

impl JsonRpcNotification {
    /// Creates a `2.0` notification.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            method: method.into(),
            params,
        }
    }
}

/// Any decoded JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum IncomingMessage {
    /// Request from the peer (expects a response).
    Request(JsonRpcRequest),
    /// Response to one of our requests.
    Response(JsonRpcResponse),
    /// Notification from the peer.
    Notification(JsonRpcNotification),
}

/// Alias kept for symmetry with the DAP/BSP bridges (E9).
pub type JsonRpcMessage = IncomingMessage;

impl IncomingMessage {
    /// Classifies a raw JSON frame by the presence of `method` / `id` / `result` / `error`.
    ///
    /// # Errors
    /// Returns [`LspDapError::SerializationError`] for invalid JSON and
    /// [`LspDapError::FramingError`] for JSON that is not a JSON-RPC message.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, LspDapError> {
        let value: Value = serde_json::from_slice(bytes)?;
        let Some(object) = value.as_object() else {
            return Err(LspDapError::framing("JSON-RPC message must be an object"));
        };
        let has_id = object.get("id").is_some_and(|id| !id.is_null());
        if object.contains_key("method") {
            if has_id {
                Ok(Self::Request(serde_json::from_value(value)?))
            } else {
                Ok(Self::Notification(serde_json::from_value(value)?))
            }
        } else if has_id && (object.contains_key("result") || object.contains_key("error")) {
            Ok(Self::Response(serde_json::from_value(value)?))
        } else {
            Err(LspDapError::framing(
                "message is neither a request, a response nor a notification",
            ))
        }
    }
}

/// Reads one framed message body.
///
/// Returns `Ok(None)` on a clean EOF before the first header byte.
///
/// # Errors
/// Returns [`LspDapError::FramingError`] for malformed or oversized headers, a missing or
/// invalid `Content-Length`, bodies larger than [`MAX_LSP_MESSAGE_BYTES`] and truncated
/// frames, and [`LspDapError::Io`] for other I/O failures.
pub async fn read_lsp_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Vec<u8>>, LspDapError> {
    let mut content_length: Option<usize> = None;
    let mut header_bytes = 0_usize;
    let mut headers_seen = false;
    let mut line = Vec::new();

    loop {
        line.clear();
        let budget = MAX_HEADER_BYTES
            .saturating_sub(header_bytes)
            .saturating_add(1);
        let limit = u64::try_from(budget).unwrap_or(u64::MAX);
        let read = (&mut *reader)
            .take(limit)
            .read_until(b'\n', &mut line)
            .await
            .map_err(|err| LspDapError::io(STREAM_NAME, &err))?;
        if read == 0 {
            if header_bytes == 0 {
                return Ok(None);
            }
            return Err(LspDapError::framing("stream ended inside frame headers"));
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(LspDapError::framing(format!(
                "frame headers exceed {MAX_HEADER_BYTES} bytes"
            )));
        }
        if line.last() != Some(&b'\n') {
            return Err(LspDapError::framing("stream ended inside frame headers"));
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| LspDapError::framing("frame header is not valid UTF-8"))?
            .trim_end_matches(['\r', '\n']);
        if text.is_empty() {
            if headers_seen {
                break;
            }
            // Tolerate stray blank lines between frames.
            continue;
        }
        headers_seen = true;
        let Some((name, value)) = text.split_once(':') else {
            return Err(LspDapError::framing(format!(
                "malformed header line `{text}`"
            )));
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let len = value.trim().parse::<usize>().map_err(|_| {
                LspDapError::framing(format!("invalid Content-Length `{}`", value.trim()))
            })?;
            content_length = Some(len);
        }
    }

    let len =
        content_length.ok_or_else(|| LspDapError::framing("missing Content-Length header"))?;
    if len > MAX_LSP_MESSAGE_BYTES {
        return Err(LspDapError::framing(format!(
            "message of {len} bytes exceeds the {MAX_LSP_MESSAGE_BYTES} byte limit"
        )));
    }
    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body).await.map_err(|err| {
        if err.kind() == std::io::ErrorKind::UnexpectedEof {
            LspDapError::framing(format!("truncated frame: expected {len} body bytes"))
        } else {
            LspDapError::io(STREAM_NAME, &err)
        }
    })?;
    Ok(Some(body))
}

/// Writes one framed message (`Content-Length` header + body) and flushes the writer.
///
/// # Errors
/// Returns [`LspDapError::FramingError`] for payloads above [`MAX_LSP_MESSAGE_BYTES`],
/// [`LspDapError::TransportClosed`] when the peer closed the stream and
/// [`LspDapError::Io`] for other I/O failures.
pub async fn write_lsp_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), LspDapError> {
    if payload.len() > MAX_LSP_MESSAGE_BYTES {
        return Err(LspDapError::framing(format!(
            "message of {} bytes exceeds the {MAX_LSP_MESSAGE_BYTES} byte limit",
            payload.len()
        )));
    }
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    let mut frame = Vec::with_capacity(header.len() + payload.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(payload);
    let map = |err: std::io::Error| {
        if err.kind() == std::io::ErrorKind::BrokenPipe {
            LspDapError::TransportClosed
        } else {
            LspDapError::io(STREAM_NAME, &err)
        }
    };
    writer.write_all(&frame).await.map_err(map)?;
    writer.flush().await.map_err(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    fn frame(body: &str) -> Vec<u8> {
        format!("Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
    }

    #[tokio::test]
    async fn reads_frames_split_into_tiny_chunks() {
        let body = r#"{"jsonrpc":"2.0","method":"x","params":{"t":"zażółć 🦀"}}"#;
        let mut bytes = frame(body);
        bytes.extend(frame(r#"{"jsonrpc":"2.0","id":1,"result":null}"#));
        let mut reader = BufReader::with_capacity(3, bytes.as_slice());
        let first = read_lsp_frame(&mut reader).await;
        assert_eq!(first, Ok(Some(body.as_bytes().to_vec())));
        let second = read_lsp_frame(&mut reader).await;
        assert!(matches!(second, Ok(Some(_))));
        assert_eq!(read_lsp_frame(&mut reader).await, Ok(None));
    }

    #[tokio::test]
    async fn accepts_lowercase_and_extra_headers_and_bare_newlines() {
        let body = r#"{"a":1}"#;
        let raw = format!(
            "content-type: application/vscode-jsonrpc; charset=utf-8\ncontent-length: {}\n\n{body}",
            body.len()
        );
        let mut reader = BufReader::new(raw.as_bytes());
        assert_eq!(
            read_lsp_frame(&mut reader).await,
            Ok(Some(body.as_bytes().to_vec()))
        );
    }

    #[tokio::test]
    async fn rejects_oversized_and_truncated_frames() {
        let raw = format!("Content-Length: {}\r\n\r\n", MAX_LSP_MESSAGE_BYTES + 1);
        let mut reader = BufReader::new(raw.as_bytes());
        assert!(matches!(
            read_lsp_frame(&mut reader).await,
            Err(LspDapError::FramingError { .. })
        ));

        let raw = "Content-Length: 10\r\n\r\n{\"a\"";
        let mut reader = BufReader::new(raw.as_bytes());
        assert!(matches!(
            read_lsp_frame(&mut reader).await,
            Err(LspDapError::FramingError { .. })
        ));

        let raw = "Content-Len";
        let mut reader = BufReader::new(raw.as_bytes());
        assert!(matches!(
            read_lsp_frame(&mut reader).await,
            Err(LspDapError::FramingError { .. })
        ));
    }

    #[tokio::test]
    async fn rejects_missing_content_length_and_huge_headers() {
        let raw = "Content-Type: x\r\n\r\n{}";
        let mut reader = BufReader::new(raw.as_bytes());
        assert!(matches!(
            read_lsp_frame(&mut reader).await,
            Err(LspDapError::FramingError { .. })
        ));

        let raw = format!("X-Junk: {}\r\n", "a".repeat(MAX_HEADER_BYTES));
        let mut reader = BufReader::new(raw.as_bytes());
        assert!(matches!(
            read_lsp_frame(&mut reader).await,
            Err(LspDapError::FramingError { .. })
        ));
    }

    #[tokio::test]
    async fn write_then_read_round_trips_multibyte_payload() {
        let payload = r#"{"text":"Gęślą jaźń 😀"}"#.as_bytes();
        let mut out = Vec::new();
        assert_eq!(write_lsp_frame(&mut out, payload).await, Ok(()));
        let header = format!("Content-Length: {}\r\n\r\n", payload.len());
        assert!(out.starts_with(header.as_bytes()));
        let mut reader = BufReader::new(out.as_slice());
        assert_eq!(
            read_lsp_frame(&mut reader).await,
            Ok(Some(payload.to_vec()))
        );
    }

    #[test]
    fn classifies_messages() {
        let req = IncomingMessage::from_slice(br#"{"jsonrpc":"2.0","id":"a","method":"m"}"#);
        assert!(
            matches!(req, Ok(IncomingMessage::Request(r)) if r.id == JsonRpcId::Text("a".into()))
        );
        let note = IncomingMessage::from_slice(br#"{"jsonrpc":"2.0","method":"m","params":[1]}"#);
        assert!(matches!(note, Ok(IncomingMessage::Notification(_))));
        let resp = IncomingMessage::from_slice(br#"{"jsonrpc":"2.0","id":7,"result":null}"#);
        assert!(matches!(resp, Ok(IncomingMessage::Response(r)) if r.id == JsonRpcId::Number(7)));
        let err = IncomingMessage::from_slice(
            br#"{"jsonrpc":"2.0","id":7,"error":{"code":-1,"message":"boom"}}"#,
        );
        assert!(matches!(err, Ok(IncomingMessage::Response(r)) if r.error.is_some()));
        assert!(IncomingMessage::from_slice(b"[]").is_err());
        assert!(IncomingMessage::from_slice(b"{").is_err());
    }

    #[test]
    fn success_response_always_serializes_result() {
        let resp = JsonRpcResponse::success(JsonRpcId::Number(1), Value::Null);
        let json = serde_json::to_string(&resp).unwrap_or_default();
        assert_eq!(json, r#"{"jsonrpc":"2.0","id":1,"result":null}"#);
    }
}
