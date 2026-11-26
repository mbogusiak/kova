//! Debug Adapter Protocol message model (`KOVA-041`).
//!
//! DAP reuses the LSP base protocol framing (`Content-Length: <n>\r\n\r\n<json>`, see
//! [`crate::lsp::read_lsp_frame`]) but **not** JSON-RPC 2.0: every message carries a
//! monotonically increasing `seq` and a `type` of `request`, `response` or `event`, and a
//! response points back at its request through `request_seq`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::LspDapError;

/// A request (client → adapter, or a reverse request such as `runInTerminal`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DapRequest {
    /// Sequence number of this message.
    pub seq: i64,
    /// Command name (`initialize`, `setBreakpoints`, …).
    pub command: String,
    /// Command arguments (omitted when `null`).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub arguments: Value,
}

/// A response to a [`DapRequest`] (DAP spells `request_seq` in snake case).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DapResponse {
    /// Sequence number of this message.
    pub seq: i64,
    /// `seq` of the answered request.
    pub request_seq: i64,
    /// Whether the request succeeded.
    pub success: bool,
    /// Command of the answered request.
    pub command: String,
    /// Error text (or a short reason such as `cancelled`) when `success` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Response payload.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub body: Value,
}

impl DapResponse {
    /// Human readable failure reason: `body.error.format`, then `message`, then a default.
    #[must_use]
    pub fn failure_message(&self) -> String {
        self.body
            .pointer("/error/format")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| self.message.clone())
            .unwrap_or_else(|| format!("`{}` failed", self.command))
    }
}

/// An adapter event (`initialized`, `stopped`, `output`, …).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DapEvent {
    /// Sequence number of this message.
    pub seq: i64,
    /// Event name.
    pub event: String,
    /// Event payload.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub body: Value,
}

impl DapEvent {
    /// Creates an event (used by tests and adapters written in Rust).
    #[must_use]
    pub fn new(seq: i64, event: impl Into<String>, body: Value) -> Self {
        Self {
            seq,
            event: event.into(),
            body,
        }
    }
}

/// Any DAP protocol message, tagged by its `type` field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DapMessage {
    /// `"type": "request"`.
    Request(DapRequest),
    /// `"type": "response"`.
    Response(DapResponse),
    /// `"type": "event"`.
    Event(DapEvent),
}

impl DapMessage {
    /// Decodes one frame body.
    ///
    /// # Errors
    /// Returns [`LspDapError::SerializationError`] for invalid JSON or an unknown `type`.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, LspDapError> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// Encodes this message as a frame body.
    ///
    /// # Errors
    /// Returns [`LspDapError::SerializationError`] if serialization fails.
    pub fn to_vec(&self) -> Result<Vec<u8>, LspDapError> {
        Ok(serde_json::to_vec(self)?)
    }

    /// `seq` of the message.
    #[must_use]
    pub const fn seq(&self) -> i64 {
        match self {
            Self::Request(r) => r.seq,
            Self::Response(r) => r.seq,
            Self::Event(e) => e.seq,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "test assertions")]

    use serde_json::json;

    use super::*;

    #[test]
    fn decodes_all_three_message_types() {
        let req = DapMessage::from_slice(
            br#"{"seq":1,"type":"request","command":"runInTerminal","arguments":{"kind":"integrated"}}"#,
        )
        .unwrap();
        assert!(matches!(req, DapMessage::Request(ref r) if r.command == "runInTerminal"));

        let resp = DapMessage::from_slice(
            br#"{"seq":2,"type":"response","request_seq":7,"success":false,"command":"evaluate","message":"boom"}"#,
        )
        .unwrap();
        let DapMessage::Response(resp) = resp else {
            panic!("expected response");
        };
        assert_eq!(resp.request_seq, 7);
        assert_eq!(resp.failure_message(), "boom");

        let event = DapMessage::from_slice(
            br#"{"seq":3,"type":"event","event":"stopped","body":{"threadId":1}}"#,
        )
        .unwrap();
        assert_eq!(event.seq(), 3);
        assert!(DapMessage::from_slice(br#"{"seq":4,"type":"bogus"}"#).is_err());
    }

    #[test]
    fn encodes_request_without_null_arguments() {
        let msg = DapMessage::Request(DapRequest {
            seq: 9,
            command: "threads".to_owned(),
            arguments: Value::Null,
        });
        let value: Value = serde_json::from_slice(&msg.to_vec().unwrap()).unwrap();
        assert_eq!(
            value,
            json!({"seq": 9, "type": "request", "command": "threads"})
        );
    }

    #[test]
    fn failure_message_prefers_structured_error() {
        let resp = DapResponse {
            seq: 1,
            request_seq: 1,
            success: false,
            command: "evaluate".to_owned(),
            message: Some("generic".to_owned()),
            body: json!({"error": {"id": 1, "format": "Cannot evaluate `x`"}}),
        };
        assert_eq!(resp.failure_message(), "Cannot evaluate `x`");
    }
}
