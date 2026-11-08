//! Live modern MCP conformance against a disposable tenant-scoped `PostgreSQL` catalog.
use super::{
    live::{self, Cluster, Endpoint, Response},
    wire::WireValidator,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
mod controls;
mod external;
mod semantic_rules;
mod semantics;
mod surface;
const DB: &str = "mcp_contract";
const PREFIX: &str = "io.logicocean.pgokf/";
fn ensure(predicate: bool, label: &str) -> Result<(), String> {
    if predicate {
        Ok(())
    } else {
        Err(format!("native: {label}"))
    }
}
/// # Errors
/// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
pub fn bytes(content: &Value) -> Result<Vec<u8>, String> {
    match (content.get("text"), content.get("blob")) {
        (Some(Value::String(s)), None) => Ok(s.as_bytes().to_vec()),
        (None, Some(Value::String(s))) => STANDARD.decode(s).map_err(|e| e.to_string()),
        _ => Err("resource must have exactly one text/blob value".into()),
    }
}
fn key(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(value)
}
fn skill_uri() -> String {
    format!(
        "skill://pgokf/b/1/c/{}/deploy/SKILL.md",
        key("skills/deploy/SKILL")
    )
}
fn frontmatter(source: &str) -> Result<Value, String> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(source).map_err(|e| e.to_string())?;
    serde_json::to_value(yaml).map_err(|e| e.to_string())
}
fn sql_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}
struct Native {
    cluster: Cluster,
    endpoint: Endpoint,
    readonly: Endpoint,
    wire: WireValidator,
    root: PathBuf,
    evidence: PathBuf,
    binary: PathBuf,
    foreign_id: i64,
    package: Vec<u8>,
    assertions: usize,
    exchanges: usize,
    tests: Vec<String>,
    observed: BTreeMap<String, Value>,
    receipts: crate::mcp_coverage::Receipts,
    inventory: crate::mcp_coverage::Inventory,
    mutation: Option<Mutation>,
}
impl Native {
    fn setup(
        root: &Path,
        pg_config: &Path,
        binary: &Path,
        evidence: &Path,
        unique_names: bool,
    ) -> Result<Self, String> {
        let cache = std::env::var_os("PGOKF_MCP_SCHEMA_DIR").map(PathBuf::from);
        let wire = WireValidator::load(root, cache.as_deref())?;
        let mut cluster = Cluster::start(root, pg_config, evidence)?;
        cluster.sql("postgres", &format!("CREATE DATABASE {DB}"))?;
        cluster.sql(
            DB,
            "CREATE EXTENSION pgokf; SELECT pgokf.set_config('store_source','true');",
        )?;
        let bundle = root
            .join("tests/mcp-golden/bundle")
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let package =
            std::fs::read(bundle.join("skills/deploy/SKILL.md")).map_err(|e| e.to_string())?;
        cluster.sql(DB,&format!("SET pgokf.tenant='contract-a'; SELECT pgokf.register_bundle({},'contract-seed'); SELECT pgokf.register_bundle_content('package-'||n,ARRAY['skills/deploy/SKILL.md','skills/deploy/assets/payload.bin'],ARRAY[convert_to({},'UTF8'),decode('00ff80','hex')]) FROM generate_series(1,54) n;",sql_literal(bundle.to_str().ok_or("bundle path")?),if unique_names { format!("replace({}, 'name: deploy', 'name: deploy-'||n)",sql_literal(std::str::from_utf8(&package).map_err(|e|e.to_string())?)) } else { sql_literal(std::str::from_utf8(&package).map_err(|e|e.to_string())?) }))?;
        let foreign=cluster.sql(DB,"SET pgokf.tenant='contract-b'; SELECT pgokf.register_bundle_content('foreign',ARRAY['foreign.md'],ARRAY[convert_to(E'---\ntype: Reference\ntitle: Foreign\n---\nforeign\n','UTF8')]); SELECT id FROM pgokf.bundles WHERE name='foreign';")?;
        let foreign_id = foreign
            .lines()
            .last()
            .ok_or("foreign bundle")?
            .parse()
            .map_err(|_| "foreign id")?;
        for role in ["reader", "builder", "writer", "admin", "foreign"] {
            let tenant = if role == "foreign" {
                "contract-b"
            } else {
                "contract-a"
            };
            let effective = if role == "foreign" { "reader" } else { role };
            cluster.sql(DB,&format!("INSERT INTO pgokf_web.mcp_tokens(name,role,tenant,digest,created_by) VALUES('contract-{role}','{effective}','{tenant}','{}','contract');",live::digest(live::token(role).as_bytes())))?;
        }
        cluster.sql(DB,"SET pgokf.tenant='contract-a'; DO $body$ DECLARE token bigint; generation bigint; BEGIN SELECT catalog_generation INTO generation FROM pgokf.bundles WHERE id=1; SELECT fencing_token INTO token FROM pgokf.issue_publication_fence(1,'contract',1,generation); PERFORM pgokf.replace_relationships('contract',1,1,generation,token,'[{\"source_concept_id\":\"runbooks/failover\",\"relation_type\":\"old:removed\",\"target_concept_id\":\"skills/deploy/SKILL\"}]'::jsonb); SELECT fencing_token INTO token FROM pgokf.issue_publication_fence(1,'contract',2,generation); PERFORM pgokf.replace_relationships('contract',1,2,generation,token,'[{\"source_concept_id\":\"runbooks/failover\",\"relation_type\":\"docs:references\",\"target_concept_id\":\"services/widget\"},{\"source_concept_id\":\"services/widget\",\"relation_type\":\"code:calls\",\"target_concept_id\":\"runbooks/failover-appendix\"}]'::jsonb); END $body$; CREATE ROLE mcp_contract LOGIN; GRANT pgokf_admin TO mcp_contract;")?;
        let dsn = cluster.dsn(DB, "mcp_contract");
        let env = BTreeMap::from([(
            "OKF_MCP_CURSOR_KEY".into(),
            STANDARD.encode((0u8..32).collect::<Vec<_>>()).into(),
        )]);
        let endpoint = Endpoint::start(
            binary,
            &dsn,
            true,
            Some("contract-a"),
            env,
            &evidence.join("http.json"),
        )?;
        let readonly = Endpoint::start(
            binary,
            &dsn,
            false,
            Some("contract-a"),
            BTreeMap::new(),
            &evidence.join("readonly.json"),
        )?;
        Ok(Self {
            cluster,
            endpoint,
            readonly,
            wire,
            root: root.into(),
            evidence: evidence.into(),
            binary: binary.into(),
            foreign_id,
            package,
            assertions: 0,
            exchanges: 0,
            tests: Vec::new(),
            observed: BTreeMap::new(),
            receipts: crate::mcp_coverage::Receipts::default(),
            mutation: None,
            inventory: crate::mcp_coverage::Inventory {
                tools: vec![],
                resources: vec![],
                templates: vec![],
                skills: vec![],
                members: vec![],
                classes: vec![],
            },
        })
    }
    fn check(&mut self, condition: bool, label: &str) -> Result<(), String> {
        ensure(condition, label)?;
        self.assertions += 1;
        Ok(())
    }
    fn exchange(
        &mut self,
        message: &Value,
        role: &str,
        readonly: bool,
        changes: &BTreeMap<String, Option<String>>,
    ) -> Result<Response, String> {
        let mut headers = live::headers(message);
        for (key, value) in changes {
            if let Some(value) = value {
                headers.insert(key.clone(), value.clone());
            } else {
                headers.remove(key);
            }
        }
        let response = if readonly {
            &mut self.readonly
        } else {
            &mut self.endpoint
        }
        .call(
            "POST",
            Some(&live::token(role)),
            Some(&message.to_string()),
            &headers,
        )?;
        self.exchanges += 1;
        if ![401, 403, 405].contains(&response.status) {
            self.wire.exchange(message, &response.value)?;
        }
        Ok(response)
    }
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    fn success_at(
        &mut self,
        method: &str,
        params: Value,
        role: &str,
        readonly: bool,
    ) -> Result<Value, String> {
        let response = self.exchange(
            &live::modern(method, params.clone()),
            role,
            readonly,
            &BTreeMap::new(),
        )?;
        self.check(
            response.status == 200 && response.value.get("error").is_none(),
            &format!("{method} success: {}", response.value),
        )?;
        self.check(
            response
                .headers
                .get("mcp-protocol-version")
                .is_some_and(|v| v == "2026-07-28")
                && response
                    .headers
                    .get("content-type")
                    .is_some_and(|v| v.starts_with("application/json")),
            "response HTTP headers",
        )?;
        let value = response.value["result"].clone();
        self.check(
            value["resultType"] == "complete"
                && value["_meta"]["io.modelcontextprotocol/serverInfo"]["name"] == "pgokf-mcp",
            "complete server metadata",
        )?;
        if method != "tools/call" {
            let uri = params["uri"].as_str().unwrap_or_default();
            let ttl = if method == "server/discover" {
                3_600_000
            } else if method == "resources/read"
                && (uri.contains("/documents/") || uri.starts_with("skill://"))
            {
                300_000
            } else {
                30000
            };

            self.cache_receipts(method, &value, ttl)?;
        }
        if method == "resources/read" {
            let content = &value["contents"][0];
            let raw = bytes(content)?;
            self.check(
                content["uri"] == params["uri"]
                    && content["mimeType"].as_str().is_some_and(|s| !s.is_empty()),
                "resource identity and MIME",
            )?;
            self.check(
                content["_meta"][format!("{PREFIX}sha256")]
                    == format!("sha256:{}", live::digest(&raw))
                    && content["_meta"][format!("{PREFIX}byteSize")].as_u64()
                        == Some(raw.len() as u64),
                "resource exact size and hash",
            )?;
            let generation = &content["_meta"][format!("{PREFIX}catalogGeneration")];
            if params["uri"] == "okf://catalog/manifest" {
                let catalog: Value = crate::json::from_slice(&raw)?;
                self.check(
                    catalog["schema"] == "pgokf.catalog.v1"
                        && catalog["catalog_generation"] == *generation
                        && generation.as_str().is_some_and(|s| {
                            s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
                        }),
                    "catalog generation fingerprint",
                )?;
            } else {
                self.check(
                    generation.as_u64().is_some_and(|n| n > 0)
                        && ["fresh", "stale", "unknown"].contains(
                            &content["_meta"][format!("{PREFIX}freshness")]["state"]
                                .as_str()
                                .unwrap_or_default(),
                        ),
                    "resource generation/freshness",
                )?;
            }
        }
        self.receipts.assert(
            format!("method:{method}"),
            value["resultType"] == "complete",
        )?;
        if method == "resources/read" {
            let uri = params["uri"].as_str().ok_or("URI")?;
            let class = if uri == "okf://catalog/manifest" {
                "catalog"
            } else if uri.starts_with("skill://") && uri.ends_with("/SKILL.md") {
                "skill_manifest"
            } else if uri.starts_with("skill://") {
                "skill_member"
            } else if uri.contains("/documents/") {
                "document"
            } else if uri.contains("/concepts/") {
                "concept"
            } else {
                "bundle"
            };
            self.receipts
                .assert(format!("class:{class}"), value["contents"][0]["uri"] == uri)?;
        }
        self.observed.entry(method.into()).or_insert_with(
            || json!({"request":live::modern(method,params),"response":response.value}),
        );
        Ok(value)
    }
    fn success(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.success_at(method, params, "reader", false)
    }
    fn error_at(
        &mut self,
        method: &str,
        params: Value,
        role: &str,
        readonly: bool,
        code: i64,
        status: u16,
    ) -> Result<Value, String> {
        let response = self.exchange(
            &live::modern(method, params),
            role,
            readonly,
            &BTreeMap::new(),
        )?;
        self.check(
            response.status == status
                && response.value["error"]["code"] == code
                && response.value.get("result").is_none(),
            &format!("expected {method} error {code}: {}", response.value),
        )?;
        Ok(response.value["error"].clone())
    }
    fn error(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.error_at(method, params, "reader", false, -32602, 400)
    }
    fn pages(&mut self, method: &str, field: &str) -> Result<(Vec<Value>, Vec<String>), String> {
        let mut result = self.success(method, json!({}))?;
        let mut rows = result[field].as_array().ok_or("list field")?.clone();
        let mut cursors = Vec::new();
        while let Some(cursor) = result["nextCursor"].as_str() {
            let cursor = cursor.to_owned();
            self.check(
                !cursors.contains(&cursor) && cursors.len() < 100,
                "bounded unique cursors",
            )?;
            cursors.push(cursor.clone());
            result = self.success(method, json!({"cursor":cursor}))?;
            rows.extend(result[field].as_array().ok_or("list field")?.clone());
        }
        Ok((rows, cursors))
    }
    #[allow(clippy::needless_pass_by_value)] // Callers construct owned JSON request values.
    fn tool(&mut self, name: &str, args: Value) -> Result<Value, String> {
        self.success("tools/call", json!({"name":name,"arguments":args}))
    }
    fn scenario(
        &mut self,
        name: &str,
        run: fn(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        if self.mutation.is_some_and(|m| m.skips(name)) {
            return Ok(());
        }
        let before = self.assertions;
        run(self).map_err(|e| format!("{name}: {e}"))?;
        self.check(self.assertions > before, "empty scenario")?;
        for original in original_tests(name) {
            self.receipts
                .assert(format!("test:{original}"), self.assertions > before)?;
        }
        self.tests.push(name.into());
        Ok(())
    }
    fn role_matrix(&mut self) -> Result<(), String> {
        for (role, count) in live::ROLES.into_iter().zip([7, 9, 12, 15]) {
            let discovery = self.success_at("server/discover", json!({}), role, false)?;
            self.check(discovery["supportedVersions"]==json!(["2026-07-28"])&&discovery["capabilities"]==json!({"tools":{"listChanged":false},"resources":{},"extensions":{"io.modelcontextprotocol/skills":{}}}),"discovery")?;
            let result = self.success_at("tools/list", json!({}), role, false)?;
            let tools = result["tools"].as_array().ok_or("tools array")?;
            self.check(
                tools.len() == count
                    && tools
                        .iter()
                        .map(|t| t["name"].as_str())
                        .collect::<BTreeSet<_>>()
                        .len()
                        == count,
                "role tool inventory",
            )?;
            for tool in tools {
                self.check(
                    tool["inputSchema"]["additionalProperties"] == false
                        && tool.get("outputSchema").is_some()
                        && tool["inputSchema"]["properties"]
                            .get("output_dir")
                            .is_none(),
                    "closed remote tool schema",
                )?;
            }
        }
        let tools = self.success_at("tools/list", json!({}), "admin", true)?;
        self.check(
            tools["tools"].as_array().is_some_and(|a| a.len() == 10),
            "readonly admin tools",
        )?;
        for (name, args) in [
            ("create_content_bundle", json!({"name":"unavailable"})),
            (
                "put_document",
                json!({"bundle_id":1,"path":"blocked.md","text":"blocked"}),
            ),
            (
                "delete_document",
                json!({"bundle_id":1,"path":"services/widget.md"}),
            ),
            ("refresh_bundle", json!({"bundle_id":1})),
            (
                "set_bundle_state",
                json!({"bundle_id":1,"state":"disabled"}),
            ),
        ] {
            let result = self.success_at(
                "tools/call",
                json!({"name":name,"arguments":args}),
                "admin",
                true,
            )?;
            self.check(
                result["isError"] == true
                    && result["structuredContent"]["code"] == "writer_unavailable",
                "readonly call refusal",
            )?;
        }
        Ok(())
    }
    fn header_controls(&mut self) -> Result<(), String> {
        for changes in [
            BTreeMap::from([("Accept".into(), None)]),
            BTreeMap::from([("MCP-Protocol-Version".into(), None)]),
            BTreeMap::from([("Mcp-Method".into(), None)]),
            BTreeMap::from([("MCP-Protocol-Version".into(), Some("2025-06-18".into()))]),
            BTreeMap::from([("Mcp-Method".into(), Some("resources/list".into()))]),
            BTreeMap::from([("Accept".into(), Some("text/event-stream".into()))]),
        ] {
            let r = self.exchange(
                &live::modern("tools/list", json!({})),
                "reader",
                false,
                &changes,
            )?;
            self.check(
                r.status == 400 && r.value["error"]["code"] == -32020,
                "required header refusal",
            )?;
        }
        let message = live::modern(
            "tools/call",
            json!({"name":"get_concept","arguments":{"bundle_id":1,"concept_id":"services/widget"}}),
        );
        for value in [None, Some("get_skill".into())] {
            let r = self.exchange(
                &message,
                "reader",
                false,
                &BTreeMap::from([("Mcp-Name".into(), value)]),
            )?;
            self.check(
                r.status == 400 && r.value["error"]["code"] == -32020,
                "name header binding",
            )?;
        }
        let r = self.exchange(
            &message,
            "reader",
            false,
            &BTreeMap::from([(
                "Mcp-Name".into(),
                Some(format!("=?base64?{}?=", STANDARD.encode("get_concept"))),
            )]),
        )?;
        self.check(
            r.status == 200 && r.value["result"]["isError"] == false,
            "encoded name header",
        )
    }
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    fn resource_inventory(&mut self) -> Result<(), String> {
        let (resources, cursors) = self.pages("resources/list", "resources")?;
        self.check(
            resources.len() == 56
                && !cursors.is_empty()
                && resources
                    .iter()
                    .map(|r| r["uri"].as_str())
                    .collect::<BTreeSet<_>>()
                    .len()
                    == 56
                && resources[0]["uri"] == "okf://catalog/manifest",
            "resource pages",
        )?;
        let templates = self.success("resources/templates/list", json!({}))?;
        self.check(
            templates["resourceTemplates"]
                .as_array()
                .is_some_and(|a| a.len() == 5),
            "template inventory",
        )?;
        self.inventory.resources = resources
            .iter()
            .map(|r| r["uri"].as_str().unwrap_or_default().to_owned())
            .collect();
        self.inventory.templates = templates["resourceTemplates"]
            .as_array()
            .ok_or("templates")?
            .iter()
            .map(|r| r["uriTemplate"].as_str().unwrap_or_default().to_owned())
            .collect();
        self.inventory.classes = [
            "bundle",
            "catalog",
            "concept",
            "document",
            "skill_manifest",
            "skill_member",
        ]
        .map(str::to_owned)
        .into();
        for resource in resources {
            let uri = resource["uri"].clone();
            let result = self.success("resources/read", json!({"uri":uri}))?;
            self.check(
                result["contents"].as_array().is_some_and(|a| a.len() == 1)
                    && result["contents"][0]["mimeType"] == resource["mimeType"],
                "every advertised resource",
            )?;
            let value: Value = crate::json::from_slice(&bytes(&result["contents"][0])?)?;
            self.check(value.is_object(), "catalog resource JSON")?;
            self.receipts.assert(
                format!(
                    "resource:{}",
                    resource["uri"].as_str().ok_or("resource URI")?
                ),
                value.is_object(),
            )?;
        }
        let (skills, cursors) = self.pages("skills/list", "skills")?;
        self.check(
            skills.len() == 55
                && !cursors.is_empty()
                && skills
                    .iter()
                    .map(|s| s["uri"].as_str())
                    .collect::<BTreeSet<_>>()
                    .len()
                    == 55,
            "skill pages",
        )?;
        self.inventory.skills = skills
            .iter()
            .map(|s| s["uri"].as_str().unwrap_or_default().to_owned())
            .collect();
        let mut binary_count = 0;
        for skill in &skills {
            let fetched = self.success("skills/get", json!({"uri":skill["uri"]}))?;
            self.check(
                fetched["skill"] == *skill
                    && fetched.get("nextCursor").is_none()
                    && skill["frontmatter"]["name"] == "deploy",
                "complete skill manifest",
            )?;
            let members = skill["resources"].as_array().ok_or("skill members")?;
            self.check(
                members.len() <= 512
                    && members
                        .iter()
                        .map(|m| m["size"].as_u64().unwrap_or(u64::MAX))
                        .sum::<u64>()
                        <= 16 * 1024 * 1024
                    && members.iter().any(|m| m["uri"] == skill["uri"]),
                "skill size and self inclusion",
            )?;
            for member in members {
                let result = self.success("resources/read", json!({"uri":member["uri"]}))?;
                let content = &result["contents"][0];
                let raw = bytes(content)?;
                self.check(
                    member["size"].as_u64() == Some(raw.len() as u64)
                        && member["digest"] == format!("sha256:{}", live::digest(&raw)),
                    "every member digest/size",
                )?;
                let uri = member["uri"].as_str().ok_or("member uri")?;
                let suffix = uri.split_once("/deploy/").ok_or("skill path")?.1;
                let expected = if suffix == "assets/payload.bin" {
                    binary_count += 1;
                    self.check(content.get("blob").is_some(), "binary encoding")?;
                    vec![0, 255, 128]
                } else {
                    std::fs::read(
                        self.root
                            .join("tests/mcp-golden/bundle/skills/deploy")
                            .join(suffix),
                    )
                    .map_err(|e| e.to_string())?
                };
                self.check(raw == expected, "every exact member byte")?;
                self.receipts
                    .assert(format!("member:{uri}"), raw == expected)?;
                self.inventory.members.push(uri.into());
                if member["uri"] == skill["uri"] {
                    self.check(
                        raw == self.package && content["mimeType"] == "text/markdown",
                        "skill manifest bytes",
                    )?;
                }
            }
        }
        for skill in &skills {
            self.receipts.assert(
                format!("skill:{}", skill["uri"].as_str().ok_or("skill URI")?),
                skill["resources"].is_array(),
            )?;
        }
        self.check(binary_count == 54, "binary resource count")?;
        let package = self.tool(
            "get_skill",
            json!({"bundle_id":1,"concept_id":"skills/deploy/SKILL"}),
        )?;
        self.check(
            package["structuredContent"]["skill_md"]
                .as_str()
                .is_some_and(|s| s.as_bytes() == self.package)
                && package["structuredContent"]["agent_skill"] == skills[0]["frontmatter"],
            "skill tool projection",
        )?;
        Ok(())
    }
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    fn lifecycle(&mut self) -> Result<(), String> {
        for version in ["2024-11-05", "2025-03-26", "2025-06-18"] {
            let message = live::request(
                7,
                "initialize",
                json!({"protocolVersion":version,"capabilities":{},"clientInfo":{"name":"surface","version":"1"}}),
            );
            let response = self.endpoint.exchange(&message, "reader", false)?;
            self.wire
                .legacy_exchange(&message, &response.value, version)?;
            self.check(
                response.status == 200 && response.value["result"]["protocolVersion"] == version,
                "legacy revision",
            )?;
            self.receipts.assert(
                "method:initialize",
                response.value["result"]["protocolVersion"] == version,
            )?;
        }
        for (method, notification) in [("ping", false), ("notifications/initialized", true)] {
            let mut message = live::request(7, method, json!({}));
            if notification {
                message.as_object_mut().ok_or("message")?.remove("id");
            }
            let r = self.endpoint.exchange(&message, "reader", false)?;
            self.check(
                r.status == if notification { 202 } else { 200 }
                    && if notification {
                        r.value.is_null()
                    } else {
                        r.value["id"] == 7
                    },
                "legacy lifecycle",
            )?;
            self.receipts.assert(
                format!("method:{method}"),
                r.status == if notification { 202 } else { 200 },
            )?;
        }
        for id in [json!(0), json!(-1), json!(i64::MAX), json!("native-id")] {
            let mut message = live::modern("tools/list", json!({}));
            message["id"] = id.clone();
            let r = self.exchange(&message, "reader", false, &BTreeMap::new())?;
            self.check(r.status == 200 && r.value["id"] == id, "typed request IDs")?;
        }
        for (field, value) in [
            ("jsonrpc", json!("1.0")),
            ("id", json!(true)),
            ("id", json!({})),
            ("id", Value::Null),
            ("id", json!(1.5)),
            ("id", json!([])),
        ] {
            let mut message = live::modern("tools/list", json!({}));
            message[field] = value;
            let r = self.exchange(&message, "reader", false, &BTreeMap::new())?;
            self.check(
                r.status == 400
                    && r.value["error"]["code"] == -32600
                    && r.value.get("id").is_none(),
                "invalid envelope",
            )?;
        }
        for body in ["[{}]", "[]", "not json"] {
            let r = self.endpoint.call(
                "POST",
                Some(&live::token("reader")),
                Some(body),
                &live::headers(&live::modern("tools/list", json!({}))),
            )?;
            self.wire.exchange(&Value::Null, &r.value)?;
            self.check(
                r.status == 400
                    && r.value["error"]["code"] == if body == "not json" { -32700 } else { -32600 },
                "uncorrelatable malformed body",
            )?;
        }
        for field in [
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientCapabilities",
        ] {
            let mut message = live::modern("server/discover", json!({}));
            message["params"]["_meta"]
                .as_object_mut()
                .ok_or("meta")?
                .remove(field);
            let r = self.exchange(&message, "reader", false, &BTreeMap::new())?;
            self.check(
                r.status == 400 && r.value["error"]["code"] == -32602,
                "required request metadata",
            )?;
        }
        for value in [Value::Null, json!(false), json!([]), json!("capabilities")] {
            let mut message = live::modern("server/discover", json!({}));
            message["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"] = value;
            let r = self.exchange(&message, "reader", false, &BTreeMap::new())?;
            self.check(
                r.status == 400 && r.value["error"]["code"] == -32602,
                "capabilities type",
            )?;
        }
        for method in [
            "initialize",
            "ping",
            "notifications/initialized",
            "prompts/list",
            "prompts/get",
            "completion/complete",
            "tasks/list",
            "tasks/get",
            "tasks/result",
            "tasks/cancel",
            "tasks/update",
            "logging/setLevel",
            "resources/subscribe",
            "resources/unsubscribe",
            "subscriptions/listen",
            "resources/directory/read",
            "sampling/createMessage",
            "elicitation/create",
            "roots/list",
            "unknown/method",
        ] {
            self.error_at(method, json!({}), "reader", false, -32601, 404)?;
        }
        for changes in [
            BTreeMap::from([("Mcp-Method".into(), Some(" tools/list ".into()))]),
            BTreeMap::from([
                ("Mcp-Method".into(), None),
                ("mcp-method".into(), Some("tools/list".into())),
            ]),
        ] {
            let r = self.exchange(
                &live::modern("tools/list", json!({})),
                "reader",
                false,
                &changes,
            )?;
            self.check(r.status == 200, "header case/whitespace")?;
        }
        let message = live::modern("server/discover", json!({}));
        let mut headers = live::headers(&message);
        headers.insert("Origin".into(), "https://hostile.invalid".into());
        let r = self.endpoint.call(
            "POST",
            Some(&live::token("reader")),
            Some(&message.to_string()),
            &headers,
        )?;
        self.check(r.status == 403, "DNS rebinding")?;
        for method in ["GET", "DELETE"] {
            let r =
                self.endpoint
                    .call(method, Some(&live::token("reader")), None, &BTreeMap::new())?;
            self.check(r.status == 405, "HTTP transport method")?;
        }
        let mut notification = live::modern("tools/list", json!({}));
        notification.as_object_mut().ok_or("message")?.remove("id");
        let r = self.exchange(&notification, "reader", false, &BTreeMap::new())?;
        self.check(r.status == 202 && r.value.is_null(), "modern notification")?;
        let r = self.endpoint.call(
            "POST",
            Some(&live::token("unknown")),
            Some("not json"),
            &BTreeMap::new(),
        )?;
        self.check(r.status == 401, "auth before body")?;
        let mut future = live::modern("server/discover", json!({}));
        future["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"] = json!("future");
        let r = self.exchange(
            &future,
            "reader",
            false,
            &BTreeMap::from([("MCP-Protocol-Version".into(), Some("future".into()))]),
        )?;
        self.check(
            r.status == 400
                && r.value["error"]["code"] == -32022
                && r.value["error"]["data"]
                    == json!({"requested":"future","supported":["2026-07-28"]}),
            "unsupported protocol version",
        )?;
        Ok(())
    }
    fn refusals_and_legacy(&mut self) -> Result<(), String> {
        self.error("tools/call", json!({"name":"unknown","arguments":{}}))?;
        for arguments in [
            json!({"query":"widget","unknown":true}),
            json!({"query":"widget","limit":0}),
            json!({"query":"widget","limit":501}),
            json!({"query":"widget","tags":[false]}),
            json!({"query":"widget","status":false}),
        ] {
            let result = self.tool("concept_search", arguments)?;
            self.check(
                result["isError"] == true
                    && result["structuredContent"]["code"] == "invalid_argument",
                "domain refusal",
            )?;
        }
        for role in ["reader", "builder", "writer"] {
            self.error_at(
                "tools/call",
                json!({"name":"create_content_bundle","arguments":{"name":"refused"}}),
                role,
                false,
                -32602,
                400,
            )?;
        }
        for (method, field) in [
            ("resources/list", "resources"),
            ("resources/templates/list", "resourceTemplates"),
        ] {
            let message = live::request(1, method, json!({}));
            let r = self.endpoint.exchange(&message, "reader", false)?;
            self.wire
                .legacy_exchange(&message, &r.value, "2025-06-18")?;
            self.check(
                r.status == 200
                    && r.value["result"][field]
                        .as_array()
                        .is_some_and(|a| !a.is_empty())
                    && r.value["result"].get("resultType").is_none(),
                "legacy resources",
            )?;
        }
        for method in ["skills/list", "skills/get"] {
            let r = self.endpoint.exchange(
                &live::request(1, method, json!({"uri":skill_uri()})),
                "reader",
                false,
            )?;
            self.check(
                r.status == 200 && r.value["error"]["code"] == -32601,
                "skills modern-only",
            )?;
        }
        let r=self.endpoint.exchange(&live::request(1,"tools/call",json!({"name":"get_skill","arguments":{"bundle_id":1,"concept_id":"skills/deploy/SKILL"}})),"reader",false)?;
        let text: Value = crate::json::from_slice(
            r.value["result"]["content"][0]["text"]
                .as_str()
                .ok_or("legacy content")?
                .as_bytes(),
        )?;
        self.check(
            r.status == 200
                && r.value["result"]["isError"] == false
                && r.value["result"]["structuredContent"]
                    .as_object()
                    .is_some_and(|m| m.len() == 1)
                && r.value["result"]["structuredContent"]["value"] == text
                && text["skill_md"]
                    .as_str()
                    .is_some_and(|s| s.as_bytes() == self.package),
            "legacy skill fallback",
        )?;
        Ok(())
    }
    fn exact_and_tenant(&mut self) -> Result<(), String> {
        let uri = format!(
            "okf://catalog/bundles/1/documents/{}",
            key("services/widget.md")
        );
        let result = self.success("resources/read", json!({"uri":uri}))?;
        self.check(
            bytes(&result["contents"][0])?
                == std::fs::read(self.root.join("tests/mcp-golden/bundle/services/widget.md"))
                    .map_err(|e| e.to_string())?,
            "exact document bytes",
        )?;
        let prefix = skill_uri().trim_end_matches("SKILL.md").to_owned();
        let mut uris = vec![
            "file:///etc/passwd".into(),
            "okf://foreign/manifest".into(),
            "okf://catalog/bundles/01".into(),
            "okf://catalog/bundles/0".into(),
            "okf://catalog/bundles/-1".into(),
            "okf://catalog/bundles/+1".into(),
            "okf://catalog/bundles/9223372036854775808".into(),
            "okf://catalog/bundles/1/".into(),
            "okf://catalog/bundles/1?tenant=other".into(),
            "okf://catalog/bundles/1#fragment".into(),
            "okf://catalog/bundles/1/documents/..".into(),
            format!(
                "okf://catalog/bundles/1/concepts/{}=",
                key("services/widget")
            ),
        ];
        for suffix in [
            "../SKILL.md",
            "./SKILL.md",
            "/SKILL.md",
            "scripts//check.sh",
            "%2F",
            "%2f",
            "%5C",
            "%5c",
            "%00",
            "%41",
            "%252F",
            "a\\b",
            "unknown",
            "%2e%2e/SKILL.md",
            "scripts%2Fcheck.sh",
            "%252e%252e/SKILL.md",
        ] {
            uris.push(format!("{prefix}{suffix}"));
        }
        uris.push(format!("{prefix}{}", "a".repeat(2050)));
        for uri in uris {
            self.error("resources/read", json!({"uri":uri}))?;
        }
        for role in live::ROLES {
            let foreign = self.error_at(
                "resources/read",
                json!({"uri":format!("okf://catalog/bundles/{}",self.foreign_id)}),
                role,
                false,
                -32602,
                400,
            )?;
            let missing = self.error_at(
                "resources/read",
                json!({"uri":"okf://catalog/bundles/999999"}),
                role,
                false,
                -32602,
                400,
            )?;
            self.check(foreign == missing, "cross tenant indistinguishable")?;
        }
        let message = live::modern("resources/list", json!({}));
        for role in ["foreign", "unknown", "none"] {
            let r = self.endpoint.exchange(&message, role, true)?;
            self.check(
                [401, 403].contains(&r.status),
                "tenant endpoint authentication",
            )?;
        }
        Ok(())
    }
    fn linked_results(&mut self) -> Result<(), String> {
        for (name, args) in [
            ("concept_search", json!({"query":"widget","limit":3})),
            (
                "find_similar",
                json!({"bundle_id":1,"concept_id":"runbooks/failover","limit":3}),
            ),
            (
                "concept_neighbors",
                json!({"bundle_id":1,"concept_id":"runbooks/failover"}),
            ),
            (
                "get_concept",
                json!({"bundle_id":1,"concept_id":"services/widget"}),
            ),
            (
                "traverse_concepts",
                json!({"bundle_id":1,"concept_id":"runbooks/failover","graph":"relationships"}),
            ),
            (
                "get_skill",
                json!({"bundle_id":1,"concept_id":"skills/deploy/SKILL"}),
            ),
        ] {
            let result = self.tool(name, args)?;
            self.check(result["isError"] == false, "linked tool success")?;
            let structured = &result["structuredContent"];
            let rows = structured
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![structured.clone()]);
            self.check(!rows.is_empty(), "linked tool rows")?;
            if name != "traverse_concepts" {
                let reduced = rows
                    .iter()
                    .map(|r| {
                        let mut r = r.clone();
                        r.as_object_mut().expect("typed row").remove("uri");
                        r
                    })
                    .collect::<Vec<_>>();
                let original = if structured.is_array() {
                    json!(reduced)
                } else {
                    reduced[0].clone()
                };
                let text: Value = crate::json::from_slice(
                    result["content"][0]["text"]
                        .as_str()
                        .ok_or("tool text")?
                        .as_bytes(),
                )?;
                self.check(text == original, "legacy text projection")?;
            }
            let links = result["content"]
                .as_array()
                .ok_or("tool content")?
                .iter()
                .filter(|r| r["type"] == "resource_link")
                .map(|r| r["uri"].as_str().unwrap_or_default())
                .collect::<BTreeSet<_>>();
            self.check(
                links
                    == rows
                        .iter()
                        .map(|r| r["uri"].as_str().unwrap_or_default())
                        .collect(),
                "canonical resource links",
            )?;
            for row in rows {
                let expected = if name == "get_skill" {
                    skill_uri()
                } else {
                    format!(
                        "okf://catalog/bundles/{}/concepts/{}",
                        row.get("bundle_id").unwrap_or(&json!(1)),
                        key(row
                            .get("concept_id")
                            .or_else(|| row.get("neighbor_id"))
                            .and_then(Value::as_str)
                            .ok_or("row identity")?)
                    )
                };
                self.check(row["uri"] == expected, "canonical resource URI")?;
                self.success("resources/read", json!({"uri":expected}))?;
            }
        }
        Ok(())
    }
    fn integrity_and_redaction(&mut self) -> Result<(), String> {
        self.cluster.sql(
            DB,
            "SET pgokf.tenant='contract-a'; SELECT pgokf.set_bundle_enabled(1,false);",
        )?;
        let missing = self.error(
            "resources/read",
            json!({"uri":"okf://catalog/bundles/999999"}),
        )?;
        let hidden = self.error("resources/read", json!({"uri":"okf://catalog/bundles/1"}))?;
        self.check(hidden == missing, "hidden bundle non-disclosure")?;
        self.error("skills/get", json!({"uri":skill_uri()}))?;
        self.cluster.sql(
            DB,
            "SET pgokf.tenant='contract-a'; SELECT pgokf.set_bundle_enabled(1,true);",
        )?;
        for (column, replacement) in [
            ("skill_md_sha256", "repeat('0',64)"),
            ("skill_md_byte_size", "skill_md_byte_size+1"),
        ] {
            let original = self.cluster.sql(
                DB,
                &format!("SELECT {column} FROM pgokf.skills WHERE bundle_id=1;"),
            )?;
            self.cluster.sql(
                DB,
                &format!("UPDATE pgokf.skills SET {column}={replacement} WHERE bundle_id=1;"),
            )?;
            self.error("resources/read", json!({"uri":skill_uri()}))?;
            self.cluster.sql(
                DB,
                &format!(
                    "UPDATE pgokf.skills SET {column}={} WHERE bundle_id=1;",
                    sql_literal(&original)
                ),
            )?;
        }
        let definition = self.cluster.sql(
            DB,
            "SELECT pg_get_functiondef('pgokf.get_skill(bigint,text)'::regprocedure);",
        )?;
        for (state, code) in [
            ("22023", "invalid_argument"),
            ("42501", "permission_denied"),
            ("23505", "conflict"),
            ("XX000", "internal"),
            ("P0001", "internal"),
        ] {
            self.cluster.sql(DB,&format!("CREATE OR REPLACE FUNCTION pgokf.get_skill(bundle_id bigint,concept_id text) RETURNS pgokf.skill_result LANGUAGE plpgsql AS $fn$ BEGIN RAISE EXCEPTION USING ERRCODE='{state}', MESSAGE='synthetic-sensitive-detail SELECT secret FROM hidden'; END $fn$;"))?;
            let result = self.tool(
                "get_skill",
                json!({"bundle_id":1,"concept_id":"skills/deploy/SKILL"}),
            )?;
            self.check(
                result["isError"] == true
                    && result["structuredContent"]["code"] == code
                    && !result.to_string().contains("synthetic-sensitive-detail")
                    && !result.to_string().contains("SELECT secret"),
                "database error classification and redaction",
            )?;
        }
        self.cluster.sql(DB, &definition)?;
        Ok(())
    }
    fn audit_and_revocation(&mut self) -> Result<(), String> {
        let before = self.cluster.sql(
            DB,
            "SELECT coalesce(max(id),0) FROM pgokf_private.access_log;",
        )?;
        let prefix = skill_uri().trim_end_matches("SKILL.md").to_owned();
        for member in ["SKILL.md", "scripts/check.sh", "references/guide.md"] {
            self.success("resources/read", json!({"uri":format!("{prefix}{member}")}))?;
        }
        self.success("resources/read",json!({"uri":format!("okf://catalog/bundles/1/documents/{}",key("services/widget.md"))}))?;
        let events=self.cluster.sql(DB,&format!("SELECT op FROM pgokf_private.access_log WHERE id>{before} AND tenant_id='contract-a' ORDER BY id;"))?;
        self.check(
            events.lines().collect::<Vec<_>>()
                == [
                    "get_skill",
                    "get_script",
                    "get_reference",
                    "get_concept_source",
                ],
            "real access audit events",
        )?;
        let digest = live::digest(live::token("admin").as_bytes());
        self.cluster.sql(
            DB,
            &format!("UPDATE pgokf_web.mcp_tokens SET role='reader' WHERE digest='{digest}';"),
        )?;
        let tools = self.success_at("tools/list", json!({}), "admin", false)?;
        self.check(
            tools["tools"].as_array().is_some_and(|a| a.len() == 7),
            "immediate role downgrade",
        )?;
        self.cluster.sql(
            DB,
            &format!("UPDATE pgokf_web.mcp_tokens SET role='admin' WHERE digest='{digest}';"),
        )?;
        let token = format!("pgokf_{}", "R".repeat(43));
        let digest = live::digest(token.as_bytes());
        self.cluster.sql(DB,&format!("INSERT INTO pgokf_web.mcp_tokens(name,role,tenant,digest,created_by) VALUES('revoked','reader','contract-a','{digest}','contract');"))?;
        let message = live::modern("server/discover", json!({}));
        let r = self.endpoint.call(
            "POST",
            Some(&token),
            Some(&message.to_string()),
            &live::headers(&message),
        )?;
        self.check(r.status == 200, "token before revocation")?;
        self.cluster.sql(
            DB,
            &format!("DELETE FROM pgokf_web.mcp_tokens WHERE digest='{digest}';"),
        )?;
        let r = self.endpoint.call(
            "POST",
            Some(&token),
            Some(&message.to_string()),
            &live::headers(&message),
        )?;
        self.check(r.status == 401, "immediate revocation")
    }
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    fn cursors(&mut self) -> Result<(), String> {
        let mut original = Vec::new();
        for method in ["resources/list", "skills/list", "concept_search"] {
            let result = if method == "concept_search" {
                self.tool(method, json!({"query":"widget","limit":1}))?
            } else {
                self.success(method, json!({}))?
            };
            let cursor = if method == "concept_search" {
                result["_meta"][format!("{PREFIX}nextCursor")].as_str()
            } else {
                result["nextCursor"].as_str()
            }
            .ok_or("expected pagination cursor")?
            .to_owned();
            let packed = URL_SAFE_NO_PAD.decode(&cursor).map_err(|e| e.to_string())?;
            let payload: Value = crate::json::from_slice(
                packed
                    .get(..packed.len().checked_sub(32).ok_or("cursor length")?)
                    .ok_or("cursor payload")?,
            )?;
            for (field, value) in [
                ("t", json!("contract-b")),
                ("p", json!("another-principal")),
                ("m", json!("other/list")),
                ("f", json!("different-filter")),
                ("exp", json!(1)),
                ("v", json!(999)),
            ] {
                let mut modified = payload.clone();
                modified[field] = value;
                let forged = sign_cursor(&modified)?;
                if method == "concept_search" {
                    let result =
                        self.tool(method, json!({"query":"widget","limit":1,"cursor":forged}))?;
                    self.check(
                        result["isError"] == true
                            && result["structuredContent"]["code"] == "invalid_cursor",
                        &format!("search cursor {field}"),
                    )?;
                    self.receipts.assert(
                        format!("cursor:{method}:{field}"),
                        result["isError"] == true
                            && result["structuredContent"]["code"] == "invalid_cursor",
                    )?;
                } else {
                    let error = self.error(method, json!({"cursor":forged}))?;
                    self.check(
                        error["data"]["code"] == "invalid_cursor",
                        &format!("{method} cursor {field}"),
                    )?;
                    self.receipts.assert(
                        format!("cursor:{method}:{field}"),
                        error["data"]["code"] == "invalid_cursor",
                    )?;
                }
            }
            original.push((method, cursor));
        }
        let cursor = &original[0].1;
        let mut tampered = cursor.clone();
        let last = tampered.pop().ok_or("cursor empty")?;
        tampered.push(if last == 'A' { 'B' } else { 'A' });
        for (method, cursor, role) in [
            ("resources/list", tampered.as_str(), "reader"),
            ("skills/list", cursor, "reader"),
            ("resources/list", cursor, "builder"),
            ("resources/list", "", "reader"),
        ] {
            let error =
                self.error_at(method, json!({"cursor":cursor}), role, false, -32602, 400)?;
            self.check(
                error["data"]["code"] == "invalid_cursor",
                "cursor MAC/method/role binding",
            )?;
        }
        let first = self.tool("concept_search", json!({"query":"widget","limit":1}))?;
        let cursor = first["_meta"][format!("{PREFIX}nextCursor")].clone();
        let second = self.tool(
            "concept_search",
            json!({"query":"widget","limit":1,"cursor":cursor}),
        )?;
        self.check(
            first["structuredContent"] != second["structuredContent"],
            "search page advances",
        )?;
        let drift = self.tool(
            "concept_search",
            json!({"query":"failover","limit":1,"cursor":cursor}),
        )?;
        self.check(
            drift["isError"] == true && drift["structuredContent"]["code"] == "invalid_cursor",
            "search filter binding",
        )?;
        let before =
            self.success("resources/read", json!({"uri":"okf://catalog/manifest"}))?["contents"][0]
                ["_meta"][format!("{PREFIX}catalogGeneration")]
            .clone();
        self.cluster.sql(
            DB,
            "SET pgokf.tenant='contract-a'; SELECT pgokf.set_bundle_enabled(1,false);",
        )?;
        for (method, cursor) in original {
            if method == "concept_search" {
                let result =
                    self.tool(method, json!({"query":"widget","limit":1,"cursor":cursor}))?;
                self.check(
                    result["isError"] == true
                        && result["structuredContent"]["code"] == "stale_cursor",
                    "search generation binding",
                )?;
                self.receipts.assert(
                    format!("cursor:{method}:generation"),
                    result["isError"] == true
                        && result["structuredContent"]["code"] == "stale_cursor",
                )?;
            } else {
                let error = self.error(method, json!({"cursor":cursor}))?;
                self.check(
                    error["data"]["code"] == "stale_cursor",
                    "list generation binding",
                )?;
                self.receipts.assert(
                    format!("cursor:{method}:generation"),
                    error["data"]["code"] == "stale_cursor",
                )?;
            }
        }
        let after =
            self.success("resources/read", json!({"uri":"okf://catalog/manifest"}))?["contents"][0]
                ["_meta"][format!("{PREFIX}catalogGeneration")]
            .clone();
        self.check(before != after, "manifest invalidation")?;
        self.cluster.sql(
            DB,
            "SET pgokf.tenant='contract-a'; SELECT pgokf.set_bundle_enabled(1,true);",
        )?;
        let token = format!("pgokf_{}", "P".repeat(43));
        let digest = live::digest(token.as_bytes());
        self.cluster.sql(DB,&format!("INSERT INTO pgokf_web.mcp_tokens(name,role,tenant,digest,created_by) VALUES('second-reader','reader','contract-a','{digest}','surface');"))?;
        let cursor = self.success("resources/list", json!({}))?["nextCursor"].clone();
        let message = live::modern("resources/list", json!({"cursor":cursor}));
        let r = self.endpoint.call(
            "POST",
            Some(&token),
            Some(&message.to_string()),
            &live::headers(&message),
        )?;
        self.check(
            r.status == 400 && r.value["error"]["data"]["code"] == "invalid_cursor",
            "cursor distinct principal",
        )?;
        self.cluster.sql(
            DB,
            &format!("DELETE FROM pgokf_web.mcp_tokens WHERE digest='{digest}';"),
        )?;
        Ok(())
    }
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    fn graphs_and_freshness(&mut self) -> Result<(), String> {
        for graph in ["links", "relationships"] {
            let result = self.tool(
                "traverse_concepts",
                json!({"bundle_id":1,"concept_id":"runbooks/failover","graph":graph}),
            )?;
            let rows = result["structuredContent"].as_array().ok_or("graph rows")?;
            self.check(
                result["isError"] == false && !rows.is_empty(),
                "both graph surfaces",
            )?;
            for row in rows {
                self.check(
                    row.get("uri").is_some()
                        && row.get("freshness").is_some()
                        && row["path"].as_array().is_some_and(|p| {
                            p.iter().all(|s| {
                                s.as_object().is_some_and(|o| {
                                    o.len() == 2
                                        && o.contains_key("bundle_id")
                                        && o.contains_key("concept_id")
                                })
                            })
                        }),
                    "typed graph path",
                )?;
            }
        }
        for (concept, options, expected) in [
            (
                "runbooks/failover",
                json!({}),
                vec!["services/widget", "runbooks/failover-appendix"],
            ),
            (
                "services/widget",
                json!({"direction":"inbound"}),
                vec!["runbooks/failover"],
            ),
            (
                "services/widget",
                json!({"direction":"both"}),
                vec!["runbooks/failover", "runbooks/failover-appendix"],
            ),
            (
                "runbooks/failover",
                json!({"relation_types":["docs:references"]}),
                vec!["services/widget"],
            ),
            (
                "runbooks/failover",
                json!({"relation_types":["absent:relation"]}),
                vec![],
            ),
        ] {
            let mut args = json!({"bundle_id":1,"concept_id":concept,"graph":"relationships"});
            args.as_object_mut()
                .ok_or("args")?
                .extend(options.as_object().ok_or("options")?.clone());
            let result = self.tool("traverse_concepts", args)?;
            let rows = result["structuredContent"].as_array().ok_or("graph rows")?;
            self.check(
                rows.iter()
                    .map(|r| r["concept_id"].as_str().unwrap_or_default())
                    .collect::<BTreeSet<_>>()
                    == expected.into_iter().collect()
                    && rows.iter().all(|r| r["relation_type"] != "old:removed"),
                "relationship direction/filter/replacement",
            )?;
        }
        self.cluster.sql(DB,"SET pgokf.tenant='contract-a'; SELECT pgokf.mark_stale(1,ARRAY['source_advanced'],'contract','contract-r2');")?;
        for (name, args) in [
            ("concept_search", json!({"query":"widget"})),
            ("find_similar", json!({"concept_id":"services/widget"})),
            (
                "concept_neighbors",
                json!({"concept_id":"runbooks/failover"}),
            ),
            ("get_concept", json!({"concept_id":"services/widget"})),
            ("get_skill", json!({"concept_id":"skills/deploy/SKILL"})),
        ] {
            let mut args = args;
            args["bundle_id"] = json!(1);
            let result = self.tool(name, args)?;
            self.check(result["isError"] == false, "stale tool success")?;
            let evidence = result["_meta"][format!("{PREFIX}catalogEvidence")]
                .as_array()
                .ok_or("catalog evidence")?;
            self.check(
                !evidence.is_empty()
                    && evidence.iter().all(|r| {
                        r["freshness"]["state"] == "stale"
                            && r["freshness"]["reasons"]
                                .as_array()
                                .is_some_and(|a| a.contains(&json!("source_advanced")))
                            && r["catalog_generation"].as_u64().is_some_and(|n| n > 0)
                            && r["uri"]
                                .as_str()
                                .is_some_and(|s| s.starts_with("okf://catalog/bundles/1/concepts/"))
                    }),
                "stale evidence across projections",
            )?;
        }
        let skill = self.success("skills/list", json!({}))?["skills"][0].clone();
        self.check(
            skill["_meta"][format!("{PREFIX}freshness")]["state"] == "stale",
            "stale skill descriptor",
        )?;
        let fetched = self.success("skills/get", json!({"uri":skill["uri"]}))?;
        self.check(fetched["skill"] == skill, "stale skill exact projection")?;
        let mut uris = skill["resources"]
            .as_array()
            .ok_or("members")?
            .iter()
            .map(|r| r["uri"].clone())
            .collect::<Vec<_>>();
        uris.extend([
            json!(format!(
                "okf://catalog/bundles/1/concepts/{}",
                key("services/widget")
            )),
            json!(format!(
                "okf://catalog/bundles/1/documents/{}",
                key("services/widget.md")
            )),
        ]);
        for uri in uris {
            let content =
                self.success("resources/read", json!({"uri":uri}))?["contents"][0].clone();
            self.check(
                content["_meta"][format!("{PREFIX}freshness")]["state"] == "stale",
                "stale resource projection",
            )?;
        }
        let restored=self.cluster.sql(DB,"SET pgokf.tenant='contract-a'; SELECT pgokf.mark_fresh(1,b.catalog_generation,f.dependency_invalidation_epoch,'contract-r2') FROM pgokf.bundles b JOIN pgokf.bundle_freshness f ON f.bundle_id=b.id WHERE b.id=1;")?;
        self.check(restored.lines().last() == Some("t"), "freshness restored")
    }
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    fn opaque_identities(&mut self) -> Result<(), String> {
        let names = [
            "plain",
            "odd%20name",
            "odd%2fname",
            "odd%25name",
            "odd%",
            "odd%%20name",
            "資料%2f🙂",
        ];
        let doc = "---\ntype: Reference\ntitle: Opaqueprobe\n---\nOpaqueprobe exact bytes\r\n";
        let package = std::str::from_utf8(&self.package)
            .map_err(|e| e.to_string())?
            .to_owned();
        let mut paths = Vec::new();
        let mut bodies = Vec::new();
        for name in names {
            paths.push(sql_literal(&format!("{name}.md")));
            bodies.push(format!("convert_to({},'UTF8')", sql_literal(doc)));
        }
        for name in names {
            paths.push(sql_literal(&format!("{name}/deploy/SKILL.md")));
            bodies.push(format!("convert_to({},'UTF8')", sql_literal(&package)));
        }
        let output=self.cluster.sql(DB,&format!("SET pgokf.tenant='contract-a'; SELECT pgokf.register_bundle_content('opaque-identities',ARRAY[{}],ARRAY[{}]); SELECT id FROM pgokf.bundles WHERE name='opaque-identities';",paths.join(","),bodies.join(",")))?;
        let bundle: i64 = output
            .lines()
            .last()
            .ok_or("opaque bundle")?
            .parse()
            .map_err(|_| "opaque id")?;
        for name in names {
            for (path, expected) in [
                (format!("{name}.md"), doc.as_bytes()),
                (format!("{name}/deploy/SKILL.md"), package.as_bytes()),
            ] {
                let uri = format!("okf://catalog/bundles/{bundle}/documents/{}", key(&path));
                let result = self.success("resources/read", json!({"uri":uri}))?;
                self.check(
                    bytes(&result["contents"][0])? == expected,
                    "opaque exact document",
                )?;
            }
            for tool in [
                "get_concept",
                "find_similar",
                "concept_neighbors",
                "traverse_concepts",
                "get_skill",
            ] {
                let concept = if tool == "get_skill" {
                    format!("{name}/deploy/SKILL")
                } else {
                    name.into()
                };
                let params =
                    json!({"name":tool,"arguments":{"bundle_id":bundle,"concept_id":concept}});
                for modern in [true, false] {
                    let message = if modern {
                        live::modern("tools/call", params.clone())
                    } else {
                        live::request(1, "tools/call", params.clone())
                    };
                    let r = self.endpoint.exchange(&message, "reader", modern)?;
                    if modern {
                        self.wire.exchange(&message, &r.value)?;
                    } else {
                        self.wire
                            .legacy_exchange(&message, &r.value, "2025-06-18")?;
                    }
                    let result = &r.value["result"];
                    self.check(
                        r.status == 200 && result["isError"] == false,
                        "opaque tool identity",
                    )?;
                    if !modern {
                        let text: Value = crate::json::from_slice(
                            result["content"][0]["text"]
                                .as_str()
                                .ok_or("text")?
                                .as_bytes(),
                        )?;
                        self.check(
                            result["structuredContent"]["value"] == text,
                            "opaque legacy wrapper",
                        )?;
                    }
                    for item in result["content"].as_array().ok_or("content")? {
                        if item["type"] == "resource_link" {
                            self.success("resources/read", json!({"uri":item["uri"]}))?;
                        }
                    }
                    if modern && tool == "get_concept" {
                        self.check(
                            result["structuredContent"][0]["concept_id"] == name
                                && result["structuredContent"][0]["uri"]
                                    == format!(
                                        "okf://catalog/bundles/{bundle}/concepts/{}",
                                        key(name)
                                    ),
                            "opaque concept projection",
                        )?;
                    }
                }
            }
        }
        for modern in [true, false] {
            let params =
                json!({"name":"concept_search","arguments":{"query":"Opaqueprobe","limit":100}});
            let message = if modern {
                live::modern("tools/call", params)
            } else {
                live::request(1, "tools/call", params)
            };
            let r = self.endpoint.exchange(&message, "reader", modern)?;
            let rows: Value = crate::json::from_slice(
                r.value["result"]["content"][0]["text"]
                    .as_str()
                    .ok_or("text")?
                    .as_bytes(),
            )?;
            let rows = rows.as_array().ok_or("search rows")?;
            self.check(
                rows.iter()
                    .map(|r| r["concept_id"].as_str().unwrap_or_default())
                    .collect::<BTreeSet<_>>()
                    == names.into_iter().collect()
                    && rows.iter().all(|r| r["bundle_id"] == bundle),
                "opaque search identities",
            )?;
        }
        self.cluster.sql(
            DB,
            &format!(
                "SET pgokf.tenant='contract-a'; SELECT pgokf.set_bundle_enabled({bundle},false);"
            ),
        )?;
        Ok(())
    }
    #[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
    fn complete_tools(&mut self) -> Result<(), String> {
        let listing = self.success_at("tools/list", json!({}), "admin", false)?;
        let definitions = listing["tools"]
            .as_array()
            .ok_or("tool list")?
            .iter()
            .map(|t| Ok((t["name"].as_str().ok_or("tool name")?.to_owned(), t.clone())))
            .collect::<Result<BTreeMap<_, _>, String>>()?;
        let refresh_path = self.evidence.join("refresh-bundle");
        copy_tree(&self.root.join("tests/mcp-golden/bundle"), &refresh_path)?;
        let registered=self.cluster.sql(DB,&format!("SET pgokf.tenant='contract-a'; SELECT pgokf.register_bundle({},'surface-refresh'); SELECT id FROM pgokf.bundles WHERE name='surface-refresh';",sql_literal(refresh_path.canonicalize().map_err(|e|e.to_string())?.to_str().ok_or("refresh path")?)))?;
        let refresh_id: i64 = registered
            .lines()
            .last()
            .ok_or("refresh id")?
            .parse()
            .map_err(|_| "refresh id")?;
        let build_args = json!({"target":"claude-code","bundle_ids":[1],"limit":10});
        let built = self.success_at(
            "tools/call",
            json!({"name":"build_workspace_plugin","arguments":build_args}),
            "admin",
            false,
        )?;
        self.check(built["isError"] == false, "build success")?;
        let lock = built["structuredContent"]["contents"]
            .as_object()
            .ok_or("build contents")?
            .iter()
            .find(|(p, _)| p.ends_with("okf-workspace.lock"))
            .map(|(_, v)| v.clone())
            .ok_or("build lock")?;
        let calls = vec![
            ("concept_search", json!({"query":"failover","limit":10})),
            (
                "find_similar",
                json!({"bundle_id":1,"concept_id":"runbooks/failover","limit":5}),
            ),
            (
                "concept_neighbors",
                json!({"bundle_id":1,"concept_id":"runbooks/failover","max_hops":2}),
            ),
            (
                "get_concept",
                json!({"bundle_id":1,"concept_id":"services/widget"}),
            ),
            (
                "get_skill",
                json!({"bundle_id":1,"concept_id":"skills/deploy/SKILL"}),
            ),
            (
                "traverse_concepts",
                json!({"bundle_id":1,"concept_id":"runbooks/failover"}),
            ),
            ("list_plugin_targets", json!({})),
            ("build_workspace_plugin", build_args),
            ("check_workspace_plugin_freshness", json!({"lock":lock})),
            ("list_bundles", json!({})),
            ("create_content_bundle", json!({"name":"surface-write"})),
            (
                "put_document",
                json!({"bundle":"surface-write","path":"surface.md","text":"---\ntype: Runbook\ntitle: Surface\n---\nSurface\n"}),
            ),
            (
                "delete_document",
                json!({"bundle":"surface-write","path":"surface.md"}),
            ),
            ("refresh_bundle", json!({"bundle_id":refresh_id})),
            (
                "set_bundle_state",
                json!({"bundle":"surface-write","state":"enabled"}),
            ),
        ];
        self.check(
            definitions
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>()
                == calls.iter().map(|(n, _)| *n).collect(),
            "complete tool inventory",
        )?;
        self.inventory.tools = definitions.keys().cloned().collect();
        for (name, args) in &calls {
            let definition = &definitions[*name];
            self.schema(&definition["inputSchema"], args, &format!("{name}:input"))?;
            let result = self.success_at(
                "tools/call",
                json!({"name":name,"arguments":args}),
                "admin",
                false,
            )?;
            self.check(
                result["isError"] == false,
                &format!("complete {name} success: {result}"),
            )?;
            self.receipts
                .assert(format!("tool:{name}:success"), result["isError"] == false)?;
            self.schema(
                &definition["outputSchema"],
                &result["structuredContent"],
                &format!("{name}:output"),
            )?;
            let mut invalid = args.clone();
            invalid["unadvertised_argument"] = json!(true);
            self.check(
                checked_schema(&definition["inputSchema"], &invalid).is_err(),
                "input schema rejects unknown fields",
            )?;
            let result = self.success_at(
                "tools/call",
                json!({"name":name,"arguments":invalid}),
                "admin",
                false,
            )?;
            self.check(
                result["isError"] == true
                    && result["structuredContent"]["code"] == "invalid_argument",
                "every tool domain refusal",
            )?;
            self.receipts.assert(
                format!("tool:{name}:error"),
                result["isError"] == true
                    && result["structuredContent"]["code"] == "invalid_argument",
            )?;
            self.schema(
                &definition["outputSchema"],
                &result["structuredContent"],
                &format!("{name}:error"),
            )?;
        }
        let writer_tools = [
            "create_content_bundle",
            "put_document",
            "delete_document",
            "refresh_bundle",
            "set_bundle_state",
        ];
        for (role, tier) in live::ROLES.into_iter().zip(0..4) {
            let permitted = calls
                .iter()
                .filter(|(name, _)| {
                    let minimum = match *name {
                        "list_plugin_targets" | "build_workspace_plugin" => 1,
                        "list_bundles" | "put_document" | "delete_document" => 2,
                        "create_content_bundle" | "refresh_bundle" | "set_bundle_state" => 3,
                        _ => 0,
                    };
                    minimum <= tier
                })
                .map(|(n, _)| *n)
                .collect::<BTreeSet<_>>();
            for readonly in [false, true] {
                let listing = self.success_at("tools/list", json!({}), role, readonly)?;
                let listed = listing["tools"]
                    .as_array()
                    .ok_or("tool list")?
                    .iter()
                    .map(|t| t["name"].as_str().unwrap_or_default())
                    .collect::<BTreeSet<_>>();
                self.check(
                    listed
                        == permitted
                            .iter()
                            .copied()
                            .filter(|n| !readonly || !writer_tools.contains(n))
                            .collect(),
                    "complete role/writer discovery",
                )?;
                for (name, args) in &calls {
                    if permitted.contains(name) {
                        let unavailable = readonly && writer_tools.contains(name);
                        let mut args = args.clone();
                        if !unavailable {
                            args["unadvertised_argument"] = json!(true);
                        }
                        let result = self.success_at(
                            "tools/call",
                            json!({"name":name,"arguments":args}),
                            role,
                            readonly,
                        )?;
                        self.check(
                            result["isError"] == true
                                && result["structuredContent"]["code"]
                                    == if unavailable {
                                        "writer_unavailable"
                                    } else {
                                        "invalid_argument"
                                    },
                            "complete role/writer call matrix",
                        )?;
                        self.receipts.assert(
                            format!(
                                "role:{role}:{}:{name}",
                                if readonly { "False" } else { "True" }
                            ),
                            result["isError"] == true
                                && result["structuredContent"]["code"]
                                    == if unavailable {
                                        "writer_unavailable"
                                    } else {
                                        "invalid_argument"
                                    },
                        )?;
                    } else {
                        let error = self.error_at(
                            "tools/call",
                            json!({"name":name,"arguments":args}),
                            role,
                            readonly,
                            -32602,
                            400,
                        )?;
                        self.receipts.assert(
                            format!(
                                "role:{role}:{}:{name}",
                                if readonly { "False" } else { "True" }
                            ),
                            error["code"] == -32602,
                        )?;
                    }
                }
            }
        }
        self.cluster.sql(DB,"SET pgokf.tenant='contract-a'; SELECT pgokf.set_bundle_enabled(id,false) FROM pgokf.bundles WHERE name IN ('surface-write','surface-refresh');")?;
        Ok(())
    }
    fn schema(&mut self, schema: &Value, value: &Value, label: &str) -> Result<(), String> {
        if self.mutation == Some(Mutation::Schemas) {
            return Ok(());
        }
        self.receipts.assert(
            format!("schema:{label}"),
            checked_schema(schema, value).is_ok(),
        )
    }
    fn run_all(&mut self) -> Result<(), String> {
        for (name, run) in [
            (
                "requirement-tools",
                Self::requirement_tools as fn(&mut Self) -> Result<(), String>,
            ),
            ("requirement-resources", Self::requirement_resources),
            ("requirement-skills", Self::requirement_skills),
            ("requirement-stateless", Self::requirement_stateless),
            ("requirement-headers", Self::requirement_headers),
            ("requirement-wire", Self::requirement_wire),
            (
                "discovery-role-writer",
                Self::role_matrix as fn(&mut Self) -> Result<(), String>,
            ),
            ("required-headers", Self::header_controls),
            ("lifecycle", Self::lifecycle),
            ("resources-skills-every-member", Self::resource_inventory),
            ("domain-and-legacy-refusals", Self::refusals_and_legacy),
            ("exact-uri-tenant", Self::exact_and_tenant),
            ("canonical-linked-results", Self::linked_results),
            ("cursor-bindings", Self::cursors),
            ("graphs-and-freshness", Self::graphs_and_freshness),
            ("integrity-and-redaction", Self::integrity_and_redaction),
            ("audit-and-revocation", Self::audit_and_revocation),
            ("opaque-identities", Self::opaque_identities),
            ("complete-tool-role-schema-matrix", Self::complete_tools),
            ("wire-corruption-controls", Self::all_wire_controls),
            ("tenant-surface", Self::tenant_surface),
            ("freshness-catalog-build", Self::freshness_build),
            ("cache-corruption-controls", Self::cache_controls),
        ] {
            self.scenario(name, run)?;
        }
        Ok(())
    }
}
fn sign_cursor(payload: &Value) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut inner = vec![0x36u8; 64];
    let mut outer = vec![0x5cu8; 64];
    for i in 0..32 {
        inner[i] ^= u8::try_from(i).map_err(|e| e.to_string())?;
        outer[i] ^= u8::try_from(i).map_err(|e| e.to_string())?;
    }
    let raw = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    inner.extend_from_slice(&raw);
    outer.extend_from_slice(&Sha256::digest(inner));
    let mut signed = raw;
    signed.extend_from_slice(&Sha256::digest(outer));
    Ok(URL_SAFE_NO_PAD.encode(signed))
}
fn checked_schema(schema: &Value, value: &Value) -> Result<(), String> {
    fn refs(value: &Value) -> Result<(), String> {
        match value {
            Value::Object(map) => {
                for (k, v) in map {
                    if ["$ref", "$dynamicRef"].contains(&k.as_str()) {
                        ensure(
                            v.as_str().is_some_and(|s| s.starts_with('#')),
                            "external schema reference",
                        )?;
                    }
                    refs(v)?;
                }
            }
            Value::Array(a) => {
                for v in a {
                    refs(v)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    refs(schema)?;
    jsonschema::meta::validate(schema).map_err(|e| e.to_string())?;
    jsonschema::validator_for(schema)
        .map_err(|e| e.to_string())?
        .validate(value)
        .map_err(|e| e.to_string())
}
/// # Errors
/// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
pub fn run(root: &Path, pg_config: &Path, binary: &Path, evidence: &Path) -> Result<(), String> {
    run_mutated(root, pg_config, binary, evidence, None)
}
/// Execute one probe-removal control; success remains forbidden if coverage is absent.
/// # Errors
/// Any setup, protocol, predicate, or coverage closure failure is returned.
pub fn run_mutated(
    root: &Path,
    pg_config: &Path,
    binary: &Path,
    evidence: &Path,
    mutation: Option<Mutation>,
) -> Result<(), String> {
    live::begin_evidence(root, pg_config, binary, evidence)?;
    let mut native = Native::setup(root, pg_config, binary, evidence, false)?;
    native.mutation = mutation;
    let result = native.run_all().and_then(|()| {
        let ledger: crate::mcp_coverage::Ledger = crate::json::from_slice(
            &std::fs::read(root.join("spec/mcp-native-coverage.json"))
                .map_err(|e| e.to_string())?,
        )?;
        let policy: crate::mcp_coverage::Policy = crate::json::from_slice(
            &std::fs::read(root.join("spec/mcp-conformance-applicability.json"))
                .map_err(|e| e.to_string())?,
        )?;
        crate::mcp_coverage::close(&native.inventory, &native.receipts, &ledger, &policy)
    });
    let report = json!({"passed":result.is_ok(),"tests":native.tests,"assertions":native.assertions,"exchanges":native.exchanges,"error":result.as_ref().err(),"coverage_closed":result.is_ok(),"mutation":mutation.map(|m|format!("{m:?}")),"receipts":native.receipts.counts()});
    crate::owned_docker::publish(&evidence.join("report.json"), &report)?;
    crate::owned_docker::publish(&evidence.join("wire-exchanges.json"), &native.observed)?;
    native.readonly.stop()?;
    native.endpoint.stop()?;
    native.cluster.stop()?;
    result
}

fn original_tests(group: &str) -> &'static [&'static str] {
    match group {
        "discovery-role-writer" => &["test_discovery_and_role_availability_matrix"],
        "required-headers" => &["test_required_headers_and_mismatch"],
        "lifecycle" => &[
            "test_complete_lifecycle",
            "test_metadata_version_and_removed_methods",
        ],
        "resources-skills-every-member" => &[
            "test_complete_resources",
            "test_resource_pages_templates_and_manifest",
            "test_skill_pages_get_and_every_file_digest",
        ],
        "domain-and-legacy-refusals" => &[
            "test_schema_and_role_refusals",
            "test_skills_extension_is_modern_only_with_legacy_tool_fallback",
        ],
        "exact-uri-tenant" => &[
            "test_document_exact_bytes_and_resource_pointer_non_dereference",
            "test_cross_tenant_and_missing_are_indistinguishable",
        ],
        "canonical-linked-results" => &["test_modern_tool_results_link_canonical_resources"],
        "cursor-bindings" => &[
            "test_complete_cursor_bindings",
            "test_cursor_tamper_cross_method_role_and_generation",
            "test_search_cursor_filters_and_graph_results",
        ],
        "graphs-and-freshness" => &[
            "test_both_graph_surfaces_return_typed_nodes",
            "test_relationship_direction_filter_generation_and_freshness",
            "test_freshness_evidence_across_tool_projections",
        ],
        "integrity-and-redaction" => &[
            "test_hidden_bundle_and_skill_integrity_fail_closed",
            "test_real_database_errors_are_classified_and_redacted",
        ],
        "audit-and-revocation" => &[
            "test_exact_reads_leave_real_audit_events",
            "test_token_role_change_and_revocation_are_immediate",
        ],
        "opaque-identities" => &["test_opaque_catalog_percent_identifiers"],
        "complete-tool-role-schema-matrix" => &["test_complete_tools"],
        "wire-corruption-controls" => &["test_wire_corruption_controls"],
        "cache-corruption-controls" => &["test_cache_corruption_controls"],
        "tenant-surface" => &[
            "test_complete_uri_and_tenant",
            "test_notifications_auth_before_body_and_non_ascii_name",
        ],
        "freshness-catalog-build" => &["test_freshness_catalog_and_build"],
        "requirement-tools" => &["test_requirement_tool_semantics"],
        "requirement-resources" => &["test_requirement_resource_semantics"],
        "requirement-skills" => &["test_requirement_skills_semantics"],
        "requirement-stateless" => &["test_requirement_stateless_semantics"],
        "requirement-headers" => &["test_requirement_header_semantics"],
        "requirement-wire" => &["test_requirement_wire_semantics"],
        _ => &[],
    }
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    std::fs::create_dir(target).map_err(|e| e.to_string())?;
    let mut entries = std::fs::read_dir(source)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        let dest = target.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), dest).map_err(|e| e.to_string())?;
        } else {
            return Err("fixture symlink refused".into());
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation {
    ToolCalls,
    ResourceReads,
    Lifecycle,
    Headers,
    Schemas,
    RoleTenant,
    CacheScope,
    CachePositive,
    CacheTtl,
}
impl Mutation {
    /// # Errors
    /// Rejects unknown probe-removal names.
    pub fn parse(name: &str) -> Result<Self, String> {
        Ok(match name {
            "tool-calls" => Self::ToolCalls,
            "resource-reads" => Self::ResourceReads,
            "lifecycle" => Self::Lifecycle,
            "headers" => Self::Headers,
            "schemas" => Self::Schemas,
            "role-tenant" => Self::RoleTenant,
            "cache-scope" => Self::CacheScope,
            "cache-positive" => Self::CachePositive,
            "cache-ttl" => Self::CacheTtl,
            _ => return Err("unknown MCP mutation".into()),
        })
    }
    fn skips(self, name: &str) -> bool {
        matches!(
            (self, name),
            (Self::ToolCalls, "complete-tool-role-schema-matrix")
                | (Self::ResourceReads, "resources-skills-every-member")
                | (Self::Lifecycle, "lifecycle")
                | (Self::Headers, "required-headers")
                | (Self::RoleTenant, "tenant-surface")
        )
    }
}

/// Run pinned official conformance, SDK, and Inspector gates against an owned cluster.
/// # Errors
/// Fails on missing tools, source identity mismatch, or any external/client check failure.
pub fn external_run(
    root: &Path,
    pg_config: &Path,
    binary: &Path,
    evidence: &Path,
) -> Result<(), String> {
    external::run(root, pg_config, binary, evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frontmatter_rejects_duplicate_mapping_keys() {
        assert_eq!(
            frontmatter("name: deploy\nmetadata: {team: ops}\n").unwrap()["name"],
            "deploy"
        );
        for source in [
            "name: one\nname: two\n",
            "metadata: {team: one, team: two}\n",
        ] {
            assert!(frontmatter(source).is_err());
        }
    }
    #[test]
    fn content_encoding_is_exact() {
        assert_eq!(bytes(&json!({"text":"é"})).unwrap(), "é".as_bytes());
        assert_eq!(bytes(&json!({"blob":"AP+A"})).unwrap(), [0, 255, 128]);
        for bad in [
            json!({}),
            json!({"text":7}),
            json!({"blob":"%%%"}),
            json!({"text":"x","blob":"eA=="}),
        ] {
            assert!(bytes(&bad).is_err());
        }
    }
    #[test]
    fn tool_schema_enforces_keywords_and_local_refs() {
        let schema = json!({"type":"object","properties":{"count":{"type":"integer","minimum":1}},"required":["count"],"additionalProperties":false});
        checked_schema(&schema, &json!({"count":1})).unwrap();
        for bad in [
            json!({}),
            json!({"count":0}),
            json!({"count":"1"}),
            json!({"count":1,"extra":true}),
        ] {
            assert!(checked_schema(&schema, &bad).is_err());
        }
        for invalid in [
            json!({"type":"not-a-type"}),
            json!({"$defs":{"unused":{"$ref":"https://example.invalid/schema"}}}),
        ] {
            assert!(checked_schema(&invalid, &json!({})).is_err());
        }
        assert!(
            checked_schema(
                &json!({"x-unused":{"$ref":"https://example.invalid/schema"}}),
                &json!({})
            )
            .is_err()
        );
        assert!(ensure(false, "control").is_err());
    }
}
