//! Narrow legacy compatibility projection; successful text remains byte-exact.
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::path::Path;
fn require(condition: bool, label: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(format!("golden: {label}"))
    }
}
fn keys(value: &Value, expected: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|m| m.len() == expected.len() && expected.iter().all(|k| m.contains_key(*k)))
}
fn regex(pattern: &str) -> regex::Regex {
    static CACHE: std::sync::LazyLock<
        std::sync::Mutex<std::collections::BTreeMap<String, regex::Regex>>,
    > = std::sync::LazyLock::new(Default::default);
    CACHE
        .lock()
        .expect("regex cache")
        .entry(pattern.into())
        .or_insert_with(|| regex::Regex::new(pattern).expect("fixed normalization expression"))
        .clone()
}
#[must_use]
pub fn normalize_string(text: &str, bundle: &str) -> String {
    let mut text = text.replace(bundle, "<BUNDLE>");
    text = regex(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})")
        .replace_all(&text, "<TS>")
        .into_owned();
    for pattern in [
        r#"("catalog_generation":\s*)\d+"#,
        r#"(\\"catalog_generation\\":\s*)\d+"#,
        r"(\bcatalog_generation:\s*)\d+",
        r"(catalog generation )\d+",
    ] {
        text = regex(pattern).replace_all(&text, "${1}<GEN>").into_owned();
    }
    for (pattern, replacement) in [
        (
            r#"("(?:sql_)?version": ?")\d+\.\d+\.\d+(?:-[^"]*)?(")"#,
            "${1}<VERSION>${2}",
        ),
        (
            r#"(\\"(?:sql_)?version\\": ?\\")\d+\.\d+\.\d+(?:-[^\\"]*)?(\\")"#,
            "${1}<VERSION>${2}",
        ),
        (
            r#"(\bsql_version: ?)\d+\.\d+\.\d+(?:-[^\s,"]*)?"#,
            "${1}<VERSION>",
        ),
    ] {
        text = regex(pattern).replace_all(&text, replacement).into_owned();
    }
    text = regex(r#"\{[^{}]*"path": "okf-workspace\.lock"[^{}]*\}"#)
        .replace_all(&text, |caps: &regex::Captures<'_>| {
            let text = regex(r#""bytes": \d+"#)
                .replace_all(&caps[0], "\"bytes\": <BYTES>")
                .into_owned();
            regex(r#""sha256": "[0-9a-f]{64}""#)
                .replace_all(&text, "\"sha256\": \"<SHA256>\"")
                .into_owned()
        })
        .into_owned();
    regex(r#""size_bytes": \d+"#)
        .replace_all(&text, "\"size_bytes\": <SIZE_BYTES>")
        .into_owned()
}
pub fn normalize(value: &Value, bundle: &str) -> Value {
    match value {
        Value::Object(map) => {
            let mut out: serde_json::Map<String, Value> = map
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if k == "catalog_generation" && v.is_number() {
                            json!("<GEN>")
                        } else {
                            normalize(v, bundle)
                        },
                    )
                })
                .collect();
            if out.get("name") == Some(&json!("pgokf-mcp"))
                && out.get("version").is_some_and(Value::is_string)
            {
                out.insert("version".into(), json!("<VERSION>"));
            }
            if out
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|p| p.ends_with("okf-workspace.lock"))
            {
                for (key, mask) in [("bytes", "<BYTES>"), ("sha256", "<SHA256>")] {
                    if out.contains_key(key) {
                        out.insert(key.into(), json!(mask));
                    }
                }
            }
            if out.get("size_bytes").is_some_and(Value::is_number) {
                out.insert("size_bytes".into(), json!("<SIZE_BYTES>"));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|v| normalize(v, bundle)).collect()),
        Value::String(s) => json!(normalize_string(s, bundle)),
        _ => value.clone(),
    }
}
/// # Errors
/// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
pub fn project_schema(current: &Value, frozen: &Value, path: &[String]) -> Result<Value, String> {
    let Some(old) = frozen.as_object() else {
        require(current == frozen, "schema value drift")?;
        return Ok(current.clone());
    };
    let new = current
        .as_object()
        .ok_or("golden: schema object required")?;
    let additions = [
        "$schema",
        "additionalProperties",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "pattern",
        "enum",
        "oneOf",
        "allOf",
    ];
    for key in new.keys().filter(|k| !old.contains_key(*k)) {
        let allowed = if path.last().is_some_and(|p| p == "properties") {
            path == ["properties"] && key == "cursor"
        } else {
            additions.contains(&key.as_str())
        };
        require(allowed, "unlisted schema addition")?;
    }
    let mut result = serde_json::Map::new();
    for (key, expected) in old {
        let mut child = path.to_vec();
        child.push(key.clone());
        result.insert(
            key.clone(),
            project_schema(
                new.get(key).ok_or("golden: removed schema field")?,
                expected,
                &child,
            )?,
        );
    }
    Ok(Value::Object(result))
}
/// # Errors
/// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
pub fn output_schema(schema: &Value) -> Result<(), String> {
    require(
        keys(
            schema,
            &[
                "$schema",
                "type",
                "properties",
                "required",
                "additionalProperties",
            ],
        ),
        "output schema fields",
    )?;
    require(
        schema["$schema"] == "https://json-schema.org/draft/2020-12/schema"
            && schema["type"] == "object"
            && schema["additionalProperties"] == false
            && schema["required"] == json!(["value"])
            && keys(&schema["properties"], &["value"]),
        "output object wrapper",
    )?;
    let domain = &schema["properties"]["value"];
    require(
        domain["$schema"] == schema["$schema"]
            && domain["oneOf"].as_array().is_some_and(|a| a.len() == 2),
        "output domain",
    )
}
fn tools(current: &mut Value, frozen: &mut Value, local: bool) -> Result<(), String> {
    let old = frozen.as_array_mut().ok_or("golden: tools array")?;
    if local {
        old.retain(|t| {
            ![
                "put_document",
                "delete_document",
                "create_content_bundle",
                "refresh_bundle",
                "set_bundle_state",
            ]
            .contains(&t["name"].as_str().unwrap_or_default())
        });
    }
    let new = current.as_array_mut().ok_or("golden: tools array")?;
    let traversal: Vec<_> = new
        .iter()
        .filter(|t| t["name"] == "traverse_concepts")
        .collect();
    require(traversal.len() == 1, "exactly one traversal tool")?;
    output_schema(&traversal[0]["outputSchema"])?;
    new.retain(|t| t["name"] != "traverse_concepts");
    require(new.len() == old.len(), "tools inventory")?;
    for (actual, expected) in new.iter_mut().zip(old) {
        require(actual["name"] == expected["name"], "tool ordering/name")?;
        require(
            keys(
                actual,
                &[
                    "name",
                    "description",
                    "inputSchema",
                    "title",
                    "annotations",
                    "outputSchema",
                ],
            ),
            "tool fields",
        )?;
        require(
            actual["title"].as_str().is_some_and(|s| !s.is_empty()),
            "tool title",
        )?;
        output_schema(&actual["outputSchema"])?;
        require(
            keys(
                &actual["annotations"],
                &[
                    "readOnlyHint",
                    "destructiveHint",
                    "idempotentHint",
                    "openWorldHint",
                ],
            ) && actual["annotations"]
                .as_object()
                .is_some_and(|a| a.values().all(Value::is_boolean)),
            "tool annotations",
        )?;
        require(
            actual["description"]
                == format!(
                    "{} Unknown arguments and wrong-typed optionals are refused.",
                    expected["description"].as_str().ok_or("description")?
                ),
            "tool description",
        )?;
        require(
            actual["inputSchema"]["additionalProperties"] == false,
            "tool input closure",
        )?;
        *actual = json!({"name":actual["name"],"description":expected["description"],"inputSchema":project_schema(&actual["inputSchema"],&expected["inputSchema"],&[])?});
    }
    Ok(())
}
fn evidence(meta: &Value, text: &Value, message: &Value) -> Result<(), String> {
    require(
        keys(meta, &["io.logicocean.pgokf/catalogEvidence"]),
        "evidence metadata",
    )?;
    let items = meta["io.logicocean.pgokf/catalogEvidence"]
        .as_array()
        .ok_or("golden: evidence array")?;
    for item in items {
        require(
            keys(
                item,
                &[
                    "bundle_id",
                    "concept_id",
                    "catalog_generation",
                    "sync_hash",
                    "freshness",
                    "uri",
                ],
            ),
            "evidence fields",
        )?;
        let bundle = item["bundle_id"]
            .as_u64()
            .filter(|n| *n > 0)
            .ok_or("golden: evidence bundle")?;
        require(
            item["catalog_generation"].as_u64().is_some_and(|n| n > 0),
            "evidence generation",
        )?;
        let concept = item["concept_id"]
            .as_str()
            .ok_or("golden: evidence identity")?;
        let rows = text
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![text.clone()]);
        require(
            rows.iter().any(|row| {
                row.get("concept_id").or_else(|| row.get("neighbor_id")) == Some(&json!(concept))
                    && row
                        .get("bundle_id")
                        .or_else(|| message.pointer("/params/arguments/bundle_id"))
                        .unwrap_or(&item["bundle_id"])
                        == &item["bundle_id"]
            }),
            "evidence outside result",
        )?;
        require(
            item["sync_hash"].is_null() || item["sync_hash"].is_string(),
            "evidence hash",
        )?;
        require(
            item["uri"]
                == format!(
                    "okf://catalog/bundles/{bundle}/concepts/{}",
                    URL_SAFE_NO_PAD.encode(concept)
                ),
            "evidence uri",
        )?;
        require(
            keys(&item["freshness"], &["state", "reasons"])
                && ["fresh", "stale", "reconciling", "unknown"]
                    .contains(&item["freshness"]["state"].as_str().unwrap_or_default())
                && item["freshness"]["reasons"]
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string)),
            "evidence freshness",
        )?;
    }
    Ok(())
}
#[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
/// # Errors
/// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
pub fn compare(
    name: &str,
    captured: &Value,
    committed: &Value,
    bundle: &Path,
) -> Result<(), String> {
    let mut actual = captured.clone();
    let mut frozen = committed.clone();
    let new = actual.as_array_mut().ok_or("golden: exchange array")?;
    let old = frozen.as_array_mut().ok_or("golden: exchange array")?;
    require(new.len() == old.len(), "exchange inventory")?;
    let bundle = bundle.to_str().ok_or("bundle path encoding")?;
    for (exchange, baseline) in new.iter_mut().zip(old) {
        require(
            normalize(&exchange["request"], bundle) == normalize(&baseline["request"], bundle),
            "request drift",
        )?;
        let message = exchange["request"].clone();
        let method = message["method"].as_str().unwrap_or_default();
        let current = &mut exchange["response"];
        let old = &mut baseline["response"];
        if !current.is_object() || !old.is_object() {
            continue;
        }
        if name == "stdio-pre-initialize" && method == "tools/call" {
            require(
                *current
                    == json!({"jsonrpc":"2.0","id":message["id"],"error":{"code":-32600,"message":"legacy stdio requires initialize then notifications/initialized"}}),
                "pre-initialize refusal",
            )?;
            *old = current.clone();
            continue;
        }
        let (Some(result), Some(previous)) = (current.get_mut("result"), old.get_mut("result"))
        else {
            continue;
        };
        if !result.is_object() || !previous.is_object() {
            continue;
        }
        require(result.get("resultType").is_none(), "modern envelope leaked")?;
        if method == "initialize" && result["protocolVersion"] == "2025-06-18" {
            require(
                result["capabilities"] == json!({"tools":{},"resources":{}}),
                "legacy capabilities",
            )?;
            result["capabilities"] = previous["capabilities"].clone();
        }
        if method == "tools/list" {
            tools(
                &mut result["tools"],
                &mut previous["tools"],
                name == "stdio-tools-list",
            )?;
        }
        if method == "tools/call" {
            require(
                result["isError"] == previous["isError"],
                "success/error channel drift",
            )?;
            let wrapper = result
                .as_object_mut()
                .ok_or("result")?
                .remove("structuredContent")
                .ok_or("golden: structured wrapper absent")?;
            require(keys(&wrapper, &["value"]), "structured wrapper")?;
            let structured = &wrapper["value"];
            require(
                result["content"].as_array().is_some_and(|a| a.len() == 1)
                    && result["content"][0]["type"] == "text",
                "legacy content",
            )?;
            let text: Value = crate::json::from_slice(
                result["content"][0]["text"]
                    .as_str()
                    .ok_or("legacy text")?
                    .as_bytes(),
            )?;
            if message["params"]["name"] == "no_such_tool" {
                require(
                    result["content"] == previous["content"],
                    "unknown tool text",
                )?;
            } else {
                require(*structured == text, "structured content differs")?;
            }
            if let Some(meta) = result.as_object_mut().ok_or("result")?.remove("_meta") {
                require(
                    result["isError"] == false
                        && [
                            "concept_search",
                            "find_similar",
                            "concept_neighbors",
                            "get_concept",
                            "get_skill",
                        ]
                        .contains(&message["params"]["name"].as_str().unwrap_or_default()),
                    "evidence call binding",
                )?;
                evidence(&meta, &text, &message)?;
            }
            if previous["isError"] == true {
                let code = match name {
                    "stdio-write-without-writer" => "writer_unavailable",
                    "stdio-tools-call-delete_document-refused" => "not_found",
                    "stdio-tools-call-create_content_bundle-refused" => "conflict",
                    "stdio-tools-call-put_document-refused"
                    | "stdio-tools-call-refresh_bundle-refused"
                    | "stdio-tools-call-set_bundle_state-refused"
                    | "http-admin-refusals" => "invalid_argument",
                    _ => return Err(format!("golden: unlisted error exception {name}")),
                };
                require(
                    keys(structured, &["code", "message"]) && structured["code"] == code,
                    "structured error",
                )?;
                let msg = structured["message"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or("golden: error message")?
                    .to_lowercase();
                require(
                    !["sqlstate", "postgresql", "select ", "from pgokf"]
                        .iter()
                        .any(|word| msg.contains(word)),
                    "error redaction",
                )?;
                result["content"] = previous["content"].clone();
            }
        }
    }
    require(
        normalize(&actual, bundle) == normalize(&frozen, bundle),
        &format!("fixture {name} drift"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn baseline() -> Value {
        crate::json::from_slice(include_bytes!(
            "../../../../tests/mcp-golden/http-reader-tools-call-concept_search.json"
        ))
        .unwrap()
    }
    fn current() -> Value {
        let mut v = baseline();
        let text = v[0]["response"]["result"]["content"][0]["text"]
            .as_str()
            .unwrap();
        let parsed: Value = crate::json::from_slice(text.as_bytes()).unwrap();
        v[0]["response"]["result"]["structuredContent"] = json!({"value":parsed});
        v
    }
    #[test]
    fn successful_text_and_envelope_are_pinned() {
        let frozen = baseline();
        let good = current();
        let path = Path::new("fixture-bundle");
        compare("sample", &good, &frozen, path).unwrap();
        for field in [
            "text",
            "id",
            "error",
            "extra",
            "unwrap",
            "wrapper_extra",
            "value",
        ] {
            let mut bad = good.clone();
            match field {
                "text" => {
                    let text = bad[0]["response"]["result"]["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    bad[0]["response"]["result"]["content"][0]["text"] = json!(text + " ");
                }
                "id" => bad[0]["response"]["id"] = json!(999),
                "error" => bad[0]["response"]["result"]["isError"] = json!(true),
                "extra" => bad[0]["response"]["result"]["extra"] = json!(true),
                "unwrap" => {
                    bad[0]["response"]["result"]["structuredContent"] =
                        good[0]["response"]["result"]["structuredContent"]["value"].clone();
                }
                "wrapper_extra" => {
                    bad[0]["response"]["result"]["structuredContent"]["extra"] = json!(true);
                }
                _ => bad[0]["response"]["result"]["structuredContent"]["value"] = json!([]),
            }
            assert!(compare("sample", &bad, &frozen, path).is_err(), "{field}");
        }
    }
    #[test]
    fn schema_removal_and_wrapper_mutations() {
        let frozen =
            json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]});
        let mut current = frozen.clone();
        current["additionalProperties"] = json!(false);
        assert_eq!(project_schema(&current, &frozen, &[]).unwrap(), frozen);
        for bad in [
            json!({"type":"array"}),
            json!({"type":"object","properties":{},"required":["query"]}),
            json!({"type":"object","properties":{"query":{"type":"integer"}},"required":["query"]}),
            json!({"type":"object","properties":{"query":{"type":"string"}},"required":[]}),
        ] {
            assert!(project_schema(&bad, &frozen, &[]).is_err());
        }
        let domain = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","oneOf":[{"type":"array"},{"type":"object"}]});
        let schema = json!({"$schema":domain["$schema"],"type":"object","additionalProperties":false,"properties":{"value":domain},"required":["value"]});
        output_schema(&schema).unwrap();
        for (field, value) in [
            ("type", json!("array")),
            ("additionalProperties", json!(true)),
            ("required", json!([])),
            ("properties", json!({"other":domain})),
        ] {
            let mut bad = schema.clone();
            bad[field] = value;
            assert!(output_schema(&bad).is_err());
        }
        assert!(output_schema(&domain).is_err());
    }
    #[test]
    fn evidence_identity_uri_and_metadata() {
        let mut good = current();
        let row = good[0]["response"]["result"]["structuredContent"]["value"][0].clone();
        let evidence = json!({"bundle_id":row["bundle_id"],"concept_id":row["concept_id"],"catalog_generation":1,"sync_hash":null,"freshness":{"state":"unknown","reasons":[]},"uri":format!("okf://catalog/bundles/{}/concepts/{}",row["bundle_id"],URL_SAFE_NO_PAD.encode(row["concept_id"].as_str().unwrap()))});
        good[0]["response"]["result"]["_meta"] =
            json!({"io.logicocean.pgokf/catalogEvidence":[evidence]});
        compare("sample", &good, &baseline(), Path::new("fixture-bundle")).unwrap();
        for (field, value) in [
            ("concept_id", json!("unrelated")),
            ("uri", json!("escape")),
            ("freshness", json!({"state":"fabricated","reasons":[]})),
        ] {
            let mut bad = good.clone();
            bad[0]["response"]["result"]["_meta"]["io.logicocean.pgokf/catalogEvidence"][0]
                [field] = value;
            assert!(compare("sample", &bad, &baseline(), Path::new("fixture-bundle")).is_err());
        }
        good[0]["response"]["result"]["_meta"]["unexpected"] = json!(true);
        assert!(compare("sample", &good, &baseline(), Path::new("fixture-bundle")).is_err());
    }
    #[test]
    fn normalization_preserves_nonvolatile_bytes() {
        let input = json!({"catalog_generation":42,"rank":0.123,"sync_hash":"abcd","modified_at":"2026-01-01T12:00:00Z","text":"unchanged 42", "name":"pgokf-mcp","version":"0.4.0-dev3"});
        let output = normalize(&input, "fixture-bundle");
        assert_eq!(output["catalog_generation"], "<GEN>");
        assert_eq!(output["modified_at"], "<TS>");
        assert_eq!(output["version"], "<VERSION>");
        for key in ["rank", "sync_hash", "text"] {
            assert_eq!(output[key], input[key]);
        }
    }
}
