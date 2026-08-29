// SPDX-License-Identifier: MIT
//! Minimal, hand-rolled JSON-RPC 2.0 types for the MCP stdio transport.
//!
//! MCP frames each message as a single line of JSON terminated by a newline
//! (no `Content-Length` header). This module models just the request and
//! response shapes the server needs, plus the standard error codes, so the
//! server pulls in no heavy MCP SDK.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The JSON was not valid JSON.
pub const PARSE_ERROR: i64 = -32700;
/// The method is not implemented.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The parameters were structurally invalid for the method.
pub const INVALID_PARAMS: i64 = -32602;

/// An inbound JSON-RPC request or notification.
///
/// A notification is a request with no `id`; the server does not reply to one.
#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// Whether this is a notification (no `id`), which must not be answered.
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// A JSON-RPC response. Exactly one of `result` / `error` is populated.
#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// A success response echoing `id` with `result`.
    #[must_use]
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error response echoing `id` with a code and message.
    #[must_use]
    pub fn error(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_with_id_is_not_a_notification() {
        // Arrange
        let request: Request =
            serde_json::from_value(json!({"id": 1, "method": "tools/list"})).expect("parses");

        // Act & Assert
        assert!(!request.is_notification());
    }

    #[test]
    fn request_without_id_is_a_notification() {
        // Arrange
        let request: Request =
            serde_json::from_value(json!({"method": "notifications/initialized"})).expect("parses");

        // Act & Assert
        assert!(request.is_notification());
    }

    #[test]
    fn success_response_serializes_without_an_error_field() {
        // Arrange
        let response = Response::success(json!(7), json!({"ok": true}));

        // Act
        let serialized = serde_json::to_value(&response).expect("serializes");

        // Assert: result present, error omitted, id echoed.
        assert_eq!(serialized["jsonrpc"], "2.0");
        assert_eq!(serialized["id"], json!(7));
        assert_eq!(serialized["result"], json!({"ok": true}));
        assert!(serialized.get("error").is_none());
    }
}
