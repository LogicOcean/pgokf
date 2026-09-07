// SPDX-License-Identifier: AGPL-3.0-only
//! Minimal, hand-rolled JSON-RPC 2.0 types, shared by both transports.
//!
//! Over stdio, MCP frames each message as a single line of JSON terminated
//! by a newline (no `Content-Length` header); over HTTP one message is one
//! request body. Either way the messages are the same, so this module models
//! just the request and response shapes the server needs, plus the standard
//! error codes, and the server pulls in no heavy MCP SDK.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The JSON was not valid JSON.
pub const PARSE_ERROR: i64 = -32700;
/// The JSON parsed but is not a JSON-RPC request.
pub const INVALID_REQUEST: i64 = -32600;
/// The method is not implemented.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// The parameters were structurally invalid for the method.
pub const INVALID_PARAMS: i64 = -32602;

/// An inbound JSON-RPC request or notification.
///
/// A notification is a request with **no** `id` at all; the server does not
/// reply to one. An explicit `"id": null` is a request (a discouraged one,
/// but a request), so the two are told apart rather than conflated: a
/// client that sent one and is waiting for an answer gets it.
#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default, deserialize_with = "read_id")]
    pub id: Id,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// The `id` member of a request. Whether a message is a notification turns
/// on whether the member is there **at all**, so an absent one and one that
/// is present and `null` are different things and are modelled as such.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Id {
    /// No `id` member: a notification, never answered.
    #[default]
    Absent,
    /// `"id": null` — discouraged by JSON-RPC 2.0, but still a request.
    Null,
    /// A string or a number, echoed back verbatim.
    Given(Value),
}

/// Read the `id` member that is present. An absent one never reaches here:
/// `#[serde(default)]` gives [`Id::Absent`] for that.
fn read_id<'de, D>(deserializer: D) -> Result<Id, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Value>::deserialize(deserializer)?.map_or(Id::Null, Id::Given))
}

impl Request {
    /// Whether this is a notification (no `id` member), which must not be
    /// answered.
    #[must_use]
    pub fn is_notification(&self) -> bool {
        self.id == Id::Absent
    }

    /// The id to echo in the reply. JSON-RPC 2.0 allows a string, a number,
    /// or null; [`Request::has_valid_id`] refuses anything else.
    #[must_use]
    pub fn reply_id(&self) -> Value {
        match &self.id {
            Id::Absent | Id::Null => Value::Null,
            Id::Given(id) => id.clone(),
        }
    }

    /// Whether the `id` is one JSON-RPC 2.0 allows: a string, a number, or
    /// null. An array or an object is not a request this server answers.
    #[must_use]
    pub fn has_valid_id(&self) -> bool {
        match &self.id {
            Id::Absent | Id::Null => true,
            Id::Given(id) => id.is_string() || id.is_number(),
        }
    }
}

/// Parse one JSON-RPC message, or the response to send instead of one.
///
/// Text that is not JSON is a parse error; text that is JSON but not a
/// request — a batch, a scalar, no `method`, an id that is not a string, a
/// number, or null — is an *invalid request*, which is the code JSON-RPC
/// reserves for it and which both transports return.
///
/// # Errors
///
/// The response to send back, ready to serialize.
pub fn parse_request(text: &str) -> Result<Request, Response> {
    let value: Value = serde_json::from_str(text).map_err(|error| {
        Response::error(Value::Null, PARSE_ERROR, format!("invalid JSON: {error}"))
    })?;
    if value.is_array() {
        return Err(Response::error(
            Value::Null,
            INVALID_REQUEST,
            "this server answers one request at a time; JSON-RPC batches are not supported",
        ));
    }
    let request: Request = serde_json::from_value(value).map_err(|error| {
        Response::error(
            Value::Null,
            INVALID_REQUEST,
            format!("not a JSON-RPC request: {error}"),
        )
    })?;
    if !request.has_valid_id() {
        return Err(Response::error(
            Value::Null,
            INVALID_REQUEST,
            "the id must be a string, a number, or null",
        ));
    }
    Ok(request)
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
    fn an_explicit_null_id_is_a_request_not_a_notification() {
        // Arrange
        let request: Request =
            serde_json::from_value(json!({"id": null, "method": "ping"})).expect("parses");

        // Act & Assert
        assert!(!request.is_notification(), "the member is present");
        assert_eq!(request.reply_id(), Value::Null);
        assert!(request.has_valid_id());
    }

    #[test]
    fn an_id_that_is_not_a_string_number_or_null_is_refused() {
        // Arrange
        let structured: Request =
            serde_json::from_value(json!({"id": {"a": 1}, "method": "ping"})).expect("parses");
        let numbered: Request =
            serde_json::from_value(json!({"id": 7, "method": "ping"})).expect("parses");

        // Act & Assert
        assert!(!structured.has_valid_id());
        assert!(numbered.has_valid_id());
        assert_eq!(numbered.reply_id(), json!(7));
    }

    #[test]
    fn a_message_that_is_not_a_request_says_which_kind_of_wrong_it_is() {
        // Arrange / Act
        let code = |text: &str| {
            parse_request(text)
                .err()
                .map(|r| r.error.expect("error").code)
        };

        // Assert
        assert!(parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#).is_ok());
        assert_eq!(code("not json"), Some(PARSE_ERROR));
        assert_eq!(code("[]"), Some(INVALID_REQUEST), "no batches");
        assert_eq!(code(r#""hello""#), Some(INVALID_REQUEST));
        assert_eq!(code(r#"{"id":1}"#), Some(INVALID_REQUEST), "no method");
        assert_eq!(
            code(r#"{"id":{"a":1},"method":"ping"}"#),
            Some(INVALID_REQUEST),
            "a structured id"
        );
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
