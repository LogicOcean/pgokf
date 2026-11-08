//! Pinned upstream wire schemas and the explicitly local Skills projection.
use crate::{json, process::CommandSpec};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    file: String,
    url: String,
    sha256: String,
    commit: String,
    #[serde(default)]
    representation: Option<String>,
}
fn descriptors(sources: &BTreeMap<String, Source>) -> Result<(), String> {
    let expected = ["2024-11-05", "2025-03-26", "2025-06-18", "core", "skills"];
    if sources.keys().map(String::as_str).ne(expected) {
        return Err("wire-schema: exact source keys required".into());
    }
    let mut files = std::collections::BTreeSet::new();
    for (name, source) in sources {
        if source.file.is_empty()
            || source.file.len() > 128
            || source.file == "."
            || source.file == ".."
            || !source
                .file
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || !files.insert(&source.file)
        {
            return Err("wire-schema: canonical unique source filename required".into());
        }
        let hex = |s: &str, n| {
            s.len() == n
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if !hex(&source.sha256, 64) || !hex(&source.commit, 40) {
            return Err("wire-schema: hash/commit shape".into());
        }
        let Some(url) = source.url.strip_prefix("https://") else {
            return Err("wire-schema: HTTPS source required".into());
        };
        let Some((host, path)) = url.split_once('/') else {
            return Err("wire-schema: source URL path".into());
        };
        if host != "raw.githubusercontent.com"
            || !path.starts_with("modelcontextprotocol/")
            || !path.split('/').any(|s| s == source.commit)
            || source.url.len() > 2048
            || source
                .url
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b"?#\\".contains(&b))
        {
            return Err("wire-schema: canonical pinned source URL".into());
        }
        if (name == "skills")
            != source
                .representation
                .as_ref()
                .is_some_and(|r| !r.is_empty())
        {
            return Err("wire-schema: Skills representation required only for Skills".into());
        }
    }
    Ok(())
}
pub struct WireValidator {
    core: Value,
    legacy: BTreeMap<String, Value>,
    validators: std::cell::RefCell<BTreeMap<String, jsonschema::Validator>>,
}
fn reference(name: &str) -> Value {
    json!({"$ref": format!("#/$defs/{name}")})
}
#[allow(clippy::needless_pass_by_value)] // Callers construct owned JSON request values.
fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object", "properties":properties, "required":required})
}
fn stem(method: &str) -> Option<&'static str> {
    Some(match method {
        "server/discover" => "Discover",
        "tools/list" => "ListTools",
        "tools/call" => "CallTool",
        "resources/list" => "ListResources",
        "resources/templates/list" => "ListResourceTemplates",
        "resources/read" => "ReadResource",
        "skills/list" => "ListSkills",
        "skills/get" => "GetSkill",
        _ => return None,
    })
}
impl WireValidator {
    /// Read pinned schemas from a cache, or retrieve them with a bounded HTTPS command.
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn load(root: &Path, cache: Option<&Path>) -> Result<Self, String> {
        let sources: BTreeMap<String, Source> = json::from_slice(
            &std::fs::read(root.join("spec/mcp-wire-sources.json")).map_err(|e| e.to_string())?,
        )?;
        descriptors(&sources)?;
        let mut schemas = BTreeMap::new();
        for (name, source) in sources {
            let bytes =
                if let Some(path) = cache.map(|p| p.join(&source.file)).filter(|p| p.is_file()) {
                    std::fs::read(path).map_err(|e| e.to_string())?
                } else {
                    let mut cmd = CommandSpec::new("curl");
                    cmd.args = [
                        "--fail",
                        "--silent",
                        "--show-error",
                        "--proto",
                        "=https",
                        "--max-time",
                        "30",
                        &source.url,
                    ]
                    .map(Into::into)
                    .into();
                    cmd.checked()?.stdout
                };
            if super::live::digest(&bytes) != source.sha256 {
                return Err(format!("wire-schema: source hash mismatch: {name}"));
            }
            if name != "skills" {
                schemas.insert(name, json::from_slice::<Value>(&bytes)?);
            }
        }
        let mut core = schemas.remove("core").ok_or("missing core schema")?;
        jsonschema::meta::validate(&core).map_err(|e| e.to_string())?;
        let defs = core
            .get_mut("$defs")
            .and_then(Value::as_object_mut)
            .ok_or("missing definitions")?;
        defs.insert("SkillResource".into(), object(json!({"uri":{"type":"string"},"digest":{"type":"string"},"size":{"type":"number"}}), &["uri","digest","size"]));
        defs.insert("Skill".into(), object(json!({"uri":{"type":"string"},"frontmatter":object(json!({"name":{"type":"string"},"description":{"type":"string"}}), &["name","description"]),"resources":{"anyOf":[{"const":"dynamic"},{"type":"array","items":reference("SkillResource")}]},"_meta":reference("MetaObject")}), &["uri","frontmatter","resources"]));
        for (method, stem, field, base, shape, parent) in [
            (
                "skills/list",
                "ListSkills",
                "skills",
                "ListResourcesRequest",
                json!({"type":"array","items":reference("Skill")}),
                "PaginatedResult",
            ),
            (
                "skills/get",
                "GetSkill",
                "skill",
                "ReadResourceRequest",
                reference("Skill"),
                "Result",
            ),
        ] {
            let mut request = defs.get(base).ok_or("missing base request")?.clone();
            request["properties"]["method"] = json!({"const":method});
            defs.insert(format!("{stem}Request"), request);
            defs.insert(format!("{stem}Result"),json!({"allOf":[reference("CacheableResult"),reference(parent),object(json!({field:shape}), &[field])]}));
            defs.insert(format!("{stem}ResultResponse"),json!({"allOf":[reference("JSONRPCResultResponse"),object(json!({"result":reference(&format!("{stem}Result"))}), &["result"])]}));
        }
        Ok(Self {
            core,
            legacy: schemas,
            validators: std::cell::RefCell::new(BTreeMap::new()),
        })
    }
    fn validate_schema(schema: &Value, definition: &str, value: &Value) -> Result<(), String> {
        let key = if schema.get("$defs").is_some() {
            "$defs"
        } else {
            "definitions"
        };
        let mut root = json!({key:schema[key],"$ref":format!("#/{key}/{definition}")});
        if let Some(draft) = schema.get("$schema") {
            root["$schema"] = draft.clone();
        }
        let validator = jsonschema::validator_for(&root)
            .map_err(|e| format!("wire-schema:{definition}: {e}"))?;
        validator
            .validate(value)
            .map_err(|e| format!("wire-schema:{definition}: {e}"))
    }
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn validate(&self, definition: &str, value: &Value) -> Result<(), String> {
        let mut validators = self.validators.borrow_mut();
        if !validators.contains_key(definition) {
            let root = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","$defs":self.core["$defs"],"$ref":format!("#/$defs/{definition}")});
            validators.insert(
                definition.into(),
                jsonschema::validator_for(&root)
                    .map_err(|e| format!("wire-schema:{definition}: {e}"))?,
            );
        }
        validators
            .get(definition)
            .ok_or("missing validator")?
            .validate(value)
            .map_err(|e| format!("wire-schema:{definition}: {e}"))
    }
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn legacy_exchange(
        &self,
        request: &Value,
        response: &Value,
        version: &str,
    ) -> Result<(), String> {
        let schema = self
            .legacy
            .get(version)
            .ok_or("wire-schema: unknown legacy version")?;
        let validate = |name: &str, value: &Value| Self::validate_schema(schema, name, value);
        if response.is_null() {
            return validate("JSONRPCNotification", request);
        }
        if response.get("error").is_some() {
            validate("JSONRPCError", response)?;
            if response.get("result").is_some() {
                return Err("wire-schema: legacy error and result".into());
            }
            return Ok(());
        }
        let method = request["method"]
            .as_str()
            .ok_or("wire-schema: missing method")?;
        let stem = stem(method)
            .or(match method {
                "initialize" => Some("Initialize"),
                "ping" => Some(""),
                _ => None,
            })
            .ok_or("wire-schema: unbound legacy method")?;
        validate("JSONRPCRequest", request)?;
        validate("JSONRPCResponse", response)?;
        if !stem.is_empty() {
            validate(&format!("{stem}Request"), request)?;
        }
        validate(
            &if stem.is_empty() {
                "EmptyResult".into()
            } else {
                format!("{stem}Result")
            },
            &response["result"],
        )?;
        correlate(request, response)
    }
    /// # Errors
    /// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
    pub fn exchange(&self, request: &Value, response: &Value) -> Result<(), String> {
        if response.is_null() {
            return if request.get("id").is_some() {
                Err("wire-schema: missing response".into())
            } else {
                Ok(())
            };
        }
        metadata(response)?;
        if let Some(error) = response.get("error") {
            self.validate("JSONRPCErrorResponse", response)?;
            let definition = self.core["$defs"]
                .as_object()
                .ok_or("missing definitions")?
                .iter()
                .find(|(_, v)| v.pointer("/properties/code/const") == error.get("code"))
                .map_or("Error", |(k, _)| k.as_str());
            self.validate(definition, error)?;
            if response.get("result").is_some() {
                return Err("wire-schema: error and result are mutually exclusive".into());
            }
            if matches!(error["code"].as_i64(), Some(-32600 | -32700)) {
                if response.get("id").is_some() {
                    return Err("wire-schema: uncorrelatable error has id".into());
                }
            } else {
                correlate(request, response)?;
            }
            return Ok(());
        }
        let method = request["method"]
            .as_str()
            .ok_or("wire-schema: missing method")?;
        let stem = stem(method).ok_or("wire-schema: unbound method")?;
        self.validate(&format!("{stem}Request"), request)?;
        self.validate(&format!("{stem}ResultResponse"), response)?;
        self.validate(&format!("{stem}Result"), &response["result"])?;
        correlate(request, response)?;
        if response["result"]["resultType"] != "complete" {
            return Err("wire-schema: unadvertised result type".into());
        }
        if method == "tools/list" {
            for tool in response["result"]["tools"]
                .as_array()
                .ok_or("missing tools")?
            {
                for field in ["inputSchema", "outputSchema"] {
                    if let Some(schema) = tool.get(field) {
                        jsonschema::meta::validate(schema)
                            .map_err(|e| format!("wire-schema:Tool:{field}: {e}"))?;
                    }
                }
            }
        }
        Ok(())
    }
}
fn correlate(request: &Value, response: &Value) -> Result<(), String> {
    if request.get("id").is_none() || request.get("id") != response.get("id") {
        return Err("wire-schema: id correlation".into());
    }
    Ok(())
}
/// # Errors
/// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
pub fn metadata(value: &Value) -> Result<(), String> {
    match value {
        Value::Object(map) => {
            if let Some(meta) = map.get("_meta") {
                let meta = meta
                    .as_object()
                    .ok_or("wire-schema: metadata object required")?;
                let grammar=regex::Regex::new(r"^(?:[A-Za-z](?:[A-Za-z0-9-]*[A-Za-z0-9])?(?:\.[A-Za-z](?:[A-Za-z0-9-]*[A-Za-z0-9])?)*/)?(?:[A-Za-z0-9](?:[A-Za-z0-9_.-]*[A-Za-z0-9])?)?$").map_err(|e|e.to_string())?;
                if meta.keys().any(|key| !grammar.is_match(key)) {
                    return Err("wire-schema: metadata key grammar".into());
                }
            }
            for (key, child) in map {
                if ![
                    "_meta",
                    "structuredContent",
                    "arguments",
                    "inputSchema",
                    "outputSchema",
                    "frontmatter",
                ]
                .contains(&key.as_str())
                {
                    metadata(child)?;
                }
            }
        }
        Value::Array(a) => {
            for child in a {
                metadata(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_controls_precede_cache_or_network() {
        let good: Value =
            crate::json::from_slice(include_bytes!("../../../../spec/mcp-wire-sources.json"))
                .unwrap();
        let parse = |v: &Value| -> Result<(), String> {
            let sources: BTreeMap<String, Source> =
                crate::json::from_slice(&serde_json::to_vec(v).unwrap())?;
            descriptors(&sources)
        };
        parse(&good).unwrap();
        for (field, value) in [
            ("file", json!("../outside")),
            ("file", json!("/tmp/cache")),
            ("file", json!(".")),
            ("url", json!("http://raw.githubusercontent.com/x")),
            ("url", json!("https://evil@raw.githubusercontent.com/x")),
            ("sha256", json!("f")),
            ("commit", json!("ABC")),
            ("unknown", json!(true)),
            ("representation", json!("not Skills")),
        ] {
            let mut bad = good.clone();
            bad["core"][field] = value;
            assert!(parse(&bad).is_err(), "{field}");
        }
        let mut bad = good.clone();
        bad["core"]["file"] = bad["skills"]["file"].clone();
        assert!(parse(&bad).is_err());
        let mut bad = good.clone();
        let item = bad.as_object_mut().unwrap().remove("core").unwrap();
        bad["different"] = item;
        assert!(parse(&bad).is_err());
    }
    #[test]
    fn source_integrity_and_schema_validation() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("spec");
        std::fs::create_dir(&spec).unwrap();
        let schema = json!({"$schema":"https://json-schema.org/draft/2020-12/schema","$defs":{"ListResourcesRequest":{},"ReadResourceRequest":{},"Probe":{"type":"integer","minimum":2}}});
        let bytes = serde_json::to_vec(&schema).unwrap();
        let mut sources = serde_json::Map::new();
        for name in ["core", "skills", "2024-11-05", "2025-03-26", "2025-06-18"] {
            let file = format!("{name}.json");
            std::fs::write(dir.path().join(&file), &bytes).unwrap();
            sources.insert(name.into(),json!({"file":file,"url":"https://raw.githubusercontent.com/modelcontextprotocol/test/0000000000000000000000000000000000000000/schema","commit":"0000000000000000000000000000000000000000","representation":if name=="skills"{json!("local")}else{Value::Null},"sha256":super::super::live::digest(&bytes)}));
        }
        let pins = spec.join("mcp-wire-sources.json");
        std::fs::write(&pins, serde_json::to_vec(&sources).unwrap()).unwrap();
        let validator = WireValidator::load(dir.path(), Some(dir.path())).unwrap();
        validator.validate("Probe", &json!(2)).unwrap();
        assert!(validator.validate("Probe", &json!(1)).is_err());
        assert!(WireValidator::validate_schema(&schema, "Probe", &json!("2")).is_err());
        std::fs::write(dir.path().join("skills.json"), b"changed source").unwrap();
        assert!(WireValidator::load(dir.path(), Some(dir.path())).is_err());
        sources.remove("skills");
        std::fs::write(&pins, serde_json::to_vec(&sources).unwrap()).unwrap();
        assert!(WireValidator::load(dir.path(), Some(dir.path())).is_err());
    }
    #[test]
    fn metadata_domain_boundaries() {
        for key in ["simple", "io.example/name", "io.example/", ""] {
            assert!(metadata(&json!({"_meta":{key:1}})).is_ok());
        }
        for key in ["bad/name/extra", "a./x", "-x/y", "x/-bad"] {
            assert!(metadata(&json!({"nested":[{"_meta":{key:1}}]})).is_err());
        }
        assert!(metadata(&json!({"structuredContent":{"_meta":[]}})).is_ok());
        assert!(metadata(&json!({"_meta":[]})).is_err());
    }
    #[test]
    fn typed_correlation() {
        for id in [json!(true), json!("1"), json!([]), Value::Null] {
            assert!(correlate(&json!({"id":1}), &json!({"id":id})).is_err());
        }
        assert!(correlate(&json!({"id":1}), &json!({"id":1})).is_ok());
    }
}
