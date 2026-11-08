// SPDX-License-Identifier: AGPL-3.0-only
//! Stateless MCP 2026-07-28 validation and result decoration.

use crate::rpc::{INVALID_PARAMS, INVALID_REQUEST, Id, METHOD_NOT_FOUND, Request, Response};
use axum::http::{HeaderMap, StatusCode};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub const VERSION: &str = "2026-07-28";
pub const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
pub const CAPABILITIES_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
pub const SERVER_INFO_KEY: &str = "io.modelcontextprotocol/serverInfo";
pub const HEADER_MISMATCH: i64 = -32020;
pub const UNSUPPORTED_VERSION: i64 = -32022;

/// Classify a JSON value that was too malformed to deserialize as a request.
#[must_use]
pub fn is_modern_value(value: &Value) -> bool {
    value.get("method").and_then(Value::as_str) == Some("server/discover")
        || value
            .get("params")
            .and_then(|params| params.get("_meta"))
            .is_some_and(|meta| meta.get(VERSION_KEY).is_some())
}

/// Modern error envelopes omit an unreadable id; legacy keeps `id: null`.
pub fn normalize_error_id(response: &mut Response) {
    if response.error.is_some() && response.id.as_ref().is_some_and(Value::is_null) {
        response.id = None;
    }
}

#[must_use]
pub fn is_modern(request: &Request) -> bool {
    request.method == "server/discover"
        || request
            .params
            .get("_meta")
            .is_some_and(|meta| meta.get(VERSION_KEY).is_some())
}

/// # Errors
/// Returns the JSON-RPC refusal for an invalid modern request.
pub fn validate(request: &Request) -> Result<(), Response> {
    if request.jsonrpc.as_ref().and_then(Value::as_str) != Some("2.0")
        || !matches!(&request.id, Id::Absent | Id::Given(Value::String(_)))
            && !matches!(&request.id, Id::Given(value) if value.is_i64() || value.is_u64())
    {
        return Err(Response::error(
            Value::Null,
            INVALID_REQUEST,
            "modern requests require jsonrpc 2.0 and a string or integer id",
        ));
    }
    let meta = request
        .params
        .get("_meta")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_meta(request))?;
    let version = meta
        .get(VERSION_KEY)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_meta(request))?;
    if version != VERSION {
        return Err(Response::error_data(
            request.reply_id(),
            UNSUPPORTED_VERSION,
            "unsupported protocol version",
            json!({"requested":version,"supported":[VERSION]}),
        ));
    }
    if !meta.get(CAPABILITIES_KEY).is_some_and(Value::is_object) {
        return Err(invalid_meta(request));
    }
    if matches!(
        request.method.as_str(),
        "initialize" | "ping" | "notifications/initialized"
    ) {
        return Err(Response::error(
            request.reply_id(),
            METHOD_NOT_FOUND,
            format!("method not available in MCP {VERSION}"),
        ));
    }
    Ok(())
}

fn invalid_meta(request: &Request) -> Response {
    Response::error(
        request.reply_id(),
        INVALID_PARAMS,
        "modern requests require protocolVersion and object clientCapabilities in params._meta",
    )
}

#[must_use]
pub fn discover(resources: bool, skills: bool) -> Value {
    let mut capabilities = json!({"tools":{"listChanged":false}});
    if resources {
        capabilities["resources"] = json!({});
    }
    if resources && skills {
        capabilities["extensions"] = json!({"io.modelcontextprotocol/skills":{}});
    }
    json!({"supportedVersions":[VERSION],"capabilities":capabilities,"instructions":"Read catalog metadata and retained bytes; invoke advertised catalog tools."})
}

pub fn decorate(method: &str, response: &mut Response) {
    normalize_error_id(response);
    let Some(result) = response.result.as_mut().and_then(Value::as_object_mut) else {
        return;
    };
    result.insert("resultType".into(), json!("complete"));
    let meta = result.entry("_meta").or_insert_with(|| json!({}));
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(
            SERVER_INFO_KEY.into(),
            json!({"name":"pgokf-mcp","version":env!("CARGO_PKG_VERSION")}),
        );
    }
    let cache = match method {
        "server/discover" => Some((3_600_000, "public")),
        "resources/read" => {
            let exact = result
                .get("contents")
                .and_then(Value::as_array)
                .is_some_and(|contents| {
                    !contents.is_empty()
                        && contents.iter().all(|item| {
                            item.get("uri").and_then(Value::as_str).is_some_and(|uri| {
                                uri.starts_with("skill://") || uri.contains("/documents/")
                            })
                        })
                });
            Some((if exact { 300_000 } else { 30_000 }, "private"))
        }
        "tools/list"
        | "resources/list"
        | "resources/templates/list"
        | "skills/list"
        | "skills/get" => Some((30_000, "private")),
        _ => None,
    };
    if let Some((ttl, scope)) = cache {
        result.insert("ttlMs".into(), json!(ttl));
        result.insert("cacheScope".into(), json!(scope));
    }
}

/// Validate transport mirrors before interpreting a supplied protocol version.
/// Missing body metadata remains a parameter error, never supplied by a header.
///
/// # Errors
/// Returns header mismatch before unsupported version when the mirrors differ.
pub fn validate_http(headers: &HeaderMap, request: &Request) -> Result<(), Response> {
    if request
        .params
        .get("_meta")
        .and_then(|meta| meta.get(VERSION_KEY))
        .and_then(Value::as_str)
        .is_some()
    {
        validate_headers(headers, request)?;
    }
    validate(request)?;
    validate_headers(headers, request)
}

/// # Errors
/// Returns the JSON-RPC refusal for an invalid modern request.
pub fn validate_headers(headers: &HeaderMap, request: &Request) -> Result<(), Response> {
    let mismatch = || {
        Response::error(
            request.reply_id(),
            HEADER_MISMATCH,
            "missing or mismatched MCP request header",
        )
    };
    let version = one_header(headers, "mcp-protocol-version").ok_or_else(mismatch)?;
    if Some(version)
        != request
            .params
            .get("_meta")
            .and_then(|meta| meta.get(VERSION_KEY))
            .and_then(Value::as_str)
        || one_header(headers, "mcp-method") != Some(request.method.as_str())
        || !one_header(headers, "accept").is_some_and(accepts_json)
    {
        return Err(mismatch());
    }
    let target = match request.method.as_str() {
        "tools/call" => Some("name"),
        "resources/read" => Some("uri"),
        _ => None,
    };
    if let Some(target) = target {
        let body = request
            .params
            .get(target)
            .and_then(Value::as_str)
            .ok_or_else(mismatch)?;
        let header = one_header(headers, "mcp-name").ok_or_else(mismatch)?;
        let name = decode_name(header).ok_or_else(mismatch)?;
        if name != body {
            return Err(mismatch());
        }
    }
    Ok(())
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    Some(value)
}
fn decode_name(value: &str) -> Option<String> {
    if let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|v| v.strip_suffix("?="))
    {
        return String::from_utf8(STANDARD.decode(encoded).ok()?).ok();
    }
    if value.trim() != value || !value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return None;
    }
    Some(value.into())
}
fn accepts_json(value: &str) -> bool {
    let mut exact = None;
    let mut wildcard = None;
    for range in value.split(',') {
        let mut fields = range.split(';').map(str::trim);
        let media = fields.next().unwrap_or_default();
        let mut quality = 1.0_f64;
        for parameter in fields {
            if let Some((name, value)) = parameter.split_once('=')
                && name.trim().eq_ignore_ascii_case("q")
            {
                quality = value.trim().parse().unwrap_or(0.0);
            }
        }
        let permitted = quality > 0.0 && quality <= 1.0;
        if media.eq_ignore_ascii_case("application/json") {
            exact = Some(permitted);
        } else if media == "*/*" {
            wildcard = Some(permitted);
        }
    }
    exact.or(wildcard).unwrap_or(false)
}
#[must_use]
pub fn http_status(response: &Response) -> StatusCode {
    match response.error.as_ref().map(|error| error.code) {
        Some(METHOD_NOT_FOUND) => StatusCode::NOT_FOUND,
        Some(_) => StatusCode::BAD_REQUEST,
        None => StatusCode::OK,
    }
}

#[derive(Default)]
pub struct LegacyLifecycle {
    initialized: bool,
    ready: bool,
}
impl LegacyLifecycle {
    /// # Errors
    /// Returns an invalid-request error for legacy work before initialization.
    pub fn admit(&mut self, request: &Request) -> Result<(), Response> {
        if is_modern(request) {
            return Ok(());
        }
        match request.method.as_str() {
            "initialize" => {
                self.initialized = true;
                self.ready = false;
                Ok(())
            }
            "notifications/initialized" if self.initialized && request.is_notification() => {
                self.ready = true;
                Ok(())
            }
            "ping" => Ok(()),
            _ if self.ready => Ok(()),
            _ => Err(Response::error(
                request.reply_id(),
                INVALID_REQUEST,
                "legacy stdio requires initialize then notifications/initialized",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{Id, parse_request};
    use serde_json::json;

    fn request(method: &str) -> Request {
        serde_json::from_value(
            json!({"jsonrpc":"2.0", "id":1,"method":method,"params":{"_meta":{
                VERSION_KEY: VERSION, CAPABILITIES_KEY: {}
            }}}),
        )
        .unwrap()
    }
    fn headers(method: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("mcp-protocol-version", VERSION.parse().unwrap());
        headers.insert("mcp-method", method.parse().unwrap());
        headers.insert(
            "accept",
            "application/json, text/event-stream".parse().unwrap(),
        );
        headers
    }
    #[test]
    fn modern_error_omits_unknown_id_and_preserves_readable_id() {
        let mut invalid = Response::error(Value::Null, INVALID_REQUEST, "invalid modern id");
        normalize_error_id(&mut invalid);
        assert!(serde_json::to_value(&invalid).unwrap().get("id").is_none());
        let mut known = Response::error(json!(42), INVALID_PARAMS, "invalid parameters");
        normalize_error_id(&mut known);
        assert_eq!(serde_json::to_value(&known).unwrap()["id"], 42);
        assert!(is_modern_value(
            &json!({"method":"server/discover","id":{}})
        ));
    }
    #[test]
    fn era_is_classified_only_from_this_request() {
        assert!(is_modern(&request("tools/list")));
        assert!(is_modern(
            &parse_request(r#"{"id":1,"method":"server/discover"}"#).unwrap()
        ));
        assert!(!is_modern(
            &parse_request(r#"{"id":1,"method":"tools/list"}"#).unwrap()
        ));
    }
    #[test]
    fn metadata_version_and_ids_fail_closed() {
        let original = request("tools/list");
        assert!(validate(&original).is_ok());
        let mut r = request("tools/list");
        r.id = Id::Null;
        assert_eq!(
            validate(&r).unwrap_err().error.unwrap().code,
            crate::rpc::INVALID_REQUEST
        );
        r.id = Id::Given(json!(1.5));
        assert!(validate(&r).is_err());
        r.id = Id::Given(json!("one"));
        assert!(validate(&r).is_ok());
        r.jsonrpc = None;
        assert!(validate(&r).is_err());
        let mut r = request("tools/list");
        r.params["_meta"][VERSION_KEY] = json!("future");
        let error = validate(&r).unwrap_err().error.unwrap();
        assert_eq!(error.code, UNSUPPORTED_VERSION);
        assert_eq!(
            error.data.unwrap(),
            json!({"supported":[VERSION],"requested":"future"})
        );
        let mut r = request("tools/list");
        r.params["_meta"]
            .as_object_mut()
            .unwrap()
            .remove(CAPABILITIES_KEY);
        assert_eq!(
            validate(&r).unwrap_err().error.unwrap().code,
            crate::rpc::INVALID_PARAMS
        );
    }
    #[test]
    fn http_mirror_mismatch_precedes_unsupported_body_version() {
        let mut r = request("tools/list");
        r.params["_meta"][VERSION_KEY] = json!("2099-01-01");
        let mut h = headers("tools/list");
        assert_eq!(
            validate_http(&h, &r).unwrap_err().error.unwrap().code,
            HEADER_MISMATCH
        );
        h.insert("mcp-protocol-version", "2099-01-01".parse().unwrap());
        assert_eq!(
            validate_http(&h, &r).unwrap_err().error.unwrap().code,
            UNSUPPORTED_VERSION
        );
        r.params["_meta"]
            .as_object_mut()
            .unwrap()
            .remove(VERSION_KEY);
        assert_eq!(
            validate_http(&h, &r).unwrap_err().error.unwrap().code,
            INVALID_PARAMS
        );
    }
    #[test]
    fn header_mirrors_and_encoding() {
        let mut r = request("resources/read");
        r.params["uri"] = json!("okf://catalog/資料");
        let mut h = headers("resources/read");
        h.insert(
            "mcp-name",
            format!("=?base64?{}?=", STANDARD.encode("okf://catalog/資料"))
                .parse()
                .unwrap(),
        );
        assert!(validate_headers(&h, &r).is_ok());
        for name in ["mcp-method", "mcp-name", "mcp-protocol-version", "accept"] {
            let mut changed = h.clone();
            changed.remove(name);
            assert_eq!(
                validate_headers(&changed, &r)
                    .unwrap_err()
                    .error
                    .unwrap()
                    .code,
                HEADER_MISMATCH
            );
        }
        h.insert("mcp-name", "=?base64?invalid!?=".parse().unwrap());
        assert!(validate_headers(&h, &r).is_err());
        h.insert("mcp-name", "other".parse().unwrap());
        assert!(validate_headers(&h, &r).is_err());
    }
    #[test]
    fn accept_respects_quality_and_json_specificity() {
        for value in [
            "application/json",
            "*/*",
            "application/json;q=0.1",
            "text/event-stream, application/json",
        ] {
            assert!(accepts_json(value), "{value}");
        }
        for value in [
            "",
            "text/event-stream",
            "application/json;q=0",
            "application/json;q=0, */*;q=1",
            "application/json;q=bogus",
            "application/json;q=2",
        ] {
            assert!(!accepts_json(value), "{value}");
        }
    }
    #[test]
    fn envelope_and_capabilities_are_honest() {
        let mut response = Response::success(json!(1), discover(false, false));
        decorate("server/discover", &mut response);
        let result = response.result.unwrap();
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["cacheScope"], "public");
        assert_eq!(result["ttlMs"], 3_600_000);
        assert!(result["capabilities"].get("resources").is_none());
        assert!(result["capabilities"].get("extensions").is_none());
        assert_eq!(result["_meta"][SERVER_INFO_KEY]["name"], "pgokf-mcp");
        let mut r = Response::success(json!(1), json!({"tools":[]}));
        decorate("tools/list", &mut r);
        assert_eq!(r.result.unwrap()["cacheScope"], "private");
        let mut r = Response::success(json!(1), json!({}));
        decorate("tools/call", &mut r);
        assert!(r.result.unwrap().get("ttlMs").is_none());
    }
    #[test]
    fn sentinel_literals_are_decoded_once_and_duplicate_mirrors_refused() {
        let mut r = request("tools/call");
        r.params["name"] = json!("=?base64?literal?=");
        let mut h = headers("tools/call");
        h.insert(
            "mcp-name",
            format!("=?base64?{}?=", STANDARD.encode("=?base64?literal?="))
                .parse()
                .unwrap(),
        );
        assert!(validate_headers(&h, &r).is_ok());
        h.insert("mcp-name", "=?base64?literal?=".parse().unwrap());
        assert!(validate_headers(&h, &r).is_err());
        let r = request("tools/list");
        let mut h = headers("tools/list");
        h.append("mcp-method", "tools/list".parse().unwrap());
        assert!(validate_headers(&h, &r).is_err());
    }
    #[test]
    fn removed_modern_methods_are_unknown_and_cache_ttls_distinguish_exact_bytes() {
        for method in ["initialize", "ping", "notifications/initialized"] {
            let response = validate(&request(method)).unwrap_err();
            assert_eq!(http_status(&response), StatusCode::NOT_FOUND);
            assert!(response.error.unwrap().message.contains(VERSION));
        }
        for (uri, ttl) in [
            ("skill://pgokf/b/1/c/YQ/s/SKILL.md", 300_000),
            ("okf://catalog/bundles/1/documents/YQ", 300_000),
            ("okf://catalog/manifest", 30_000),
        ] {
            let mut response =
                Response::success(json!(1), json!({"contents":[{"uri":uri,"text":"body"}]}));
            decorate("resources/read", &mut response);
            assert_eq!(response.result.unwrap()["ttlMs"], ttl);
        }
    }
    #[test]
    fn legacy_lifecycle_is_separate_from_modern() {
        let mut state = LegacyLifecycle::default();
        let legacy =
            |method: &str| serde_json::from_value(json!({"id":1,"method":method})).unwrap();
        assert!(state.admit(&legacy("tools/list")).is_err());
        assert!(state.admit(&request("tools/list")).is_ok());
        assert!(state.admit(&legacy("tools/list")).is_err());
        assert!(state.admit(&legacy("initialize")).is_ok());
        assert!(state.admit(&legacy("initialize")).is_ok());
        assert!(state.admit(&legacy("tools/list")).is_err());
        let initialized =
            serde_json::from_value(json!({"method":"notifications/initialized"})).unwrap();
        assert!(state.admit(&initialized).is_ok());
        assert!(state.admit(&legacy("tools/list")).is_ok());
    }
}
