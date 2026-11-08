//! Execute all frozen legacy transcripts against a disposable real cluster.
use super::{
    golden,
    live::{self, Cluster, Endpoint},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path};
fn fixtures(root: &Path) -> Result<BTreeMap<String, Value>, String> {
    let mut result = BTreeMap::new();
    for entry in std::fs::read_dir(root.join("tests/mcp-golden")).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.extension().is_some_and(|e| e == "json") {
            result.insert(
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or("fixture name")?
                    .into(),
                crate::json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)?,
            );
        }
    }
    if result.len() != 72 {
        return Err(format!(
            "expected 72 golden fixtures, found {}",
            result.len()
        ));
    }
    Ok(result)
}
fn lock(response: &Value) -> Result<String, String> {
    let text: Value = crate::json::from_slice(
        response["result"]["content"][0]["text"]
            .as_str()
            .ok_or("build text missing")?
            .as_bytes(),
    )?;
    text["contents"]
        .as_object()
        .ok_or("build contents missing")?
        .iter()
        .find(|(k, _)| k.ends_with("okf-workspace.lock"))
        .and_then(|(_, v)| v.as_str())
        .map(str::to_owned)
        .ok_or("build lock missing".into())
}
fn replace_lock(message: &mut Value, lock: Option<&str>) {
    if let Some(args) = message
        .pointer_mut("/params/arguments")
        .and_then(Value::as_object_mut)
        && args.contains_key("lock")
        && let Some(lock) = lock
    {
        args.insert("lock".into(), json!(lock));
    }
}
fn stdio_fixture(
    name: &str,
    expected: &BTreeMap<String, Value>,
    actual: &mut BTreeMap<String, Value>,
    service: &mut crate::service::Service,
    lock: Option<&str>,
) -> Result<(), String> {
    let mut rows = Vec::new();
    for row in expected
        .get(name)
        .ok_or(format!("missing fixture {name}"))?
        .as_array()
        .ok_or("fixture array")?
    {
        let mut message = row["request"].clone();
        replace_lock(&mut message, lock);
        let response = live::stdio_exchange(service, &message)?;
        rows.push(json!({"request":message,"response":response}));
    }
    actual.insert(name.into(), json!(rows));
    Ok(())
}
fn http_fixture(
    name: &str,
    expected: &BTreeMap<String, Value>,
    actual: &mut BTreeMap<String, Value>,
    endpoint: &mut Endpoint,
    lock: Option<&str>,
) -> Result<(), String> {
    let mut rows = Vec::new();
    for row in expected
        .get(name)
        .ok_or(format!("missing fixture {name}"))?
        .as_array()
        .ok_or("fixture array")?
    {
        let mut message = row["request"].clone();
        replace_lock(&mut message, lock);
        let auth = row["auth"].as_str().ok_or("fixture auth")?;
        let response =
            if ["GET", "DELETE"].contains(&message["method"].as_str().unwrap_or_default()) {
                endpoint.call(
                    message["method"].as_str().unwrap_or_default(),
                    Some(&live::token(auth)),
                    None,
                    &BTreeMap::new(),
                )?
            } else {
                endpoint.exchange(&message, auth, false)?
            };
        rows.push(json!({"request":message,"response":if response.value.is_null(){json!("")}else{response.value},"status":response.status,"auth":auth}));
    }
    actual.insert(name.into(), json!(rows));
    Ok(())
}
#[allow(clippy::too_many_lines)] // Keep each ordered protocol scenario together.
/// # Errors
/// Rejects invalid protocol data, missing prerequisites, or failed bounded I/O.
pub fn run(root: &Path, pg_config: &Path, binary: &Path, evidence: &Path) -> Result<usize, String> {
    super::live::begin_evidence(root, pg_config, binary, evidence)?;
    let expected = fixtures(root)?;
    let bundle = root
        .join("tests/mcp-golden/bundle")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let mut cluster = Cluster::start(root, pg_config, evidence)?;
    cluster.sql("postgres", "CREATE DATABASE golden")?;
    cluster.sql(
        "golden",
        "CREATE EXTENSION pgokf; SELECT pgokf.set_config('store_source','true');",
    )?;
    cluster.sql("golden",&format!("SELECT pgokf.register_bundle($path${}$path$,'golden-fs'); SELECT pgokf.register_bundle_content('golden-content',ARRAY['notes/seed.md'],ARRAY[convert_to($seed$---\ntype: Reference\ntitle: Golden seed\n---\n\n# Golden seed\n\nSeeded into the content bundle by the golden MCP harness.\n$seed$,'UTF8')]);",bundle.display()))?;
    for role in live::ROLES {
        cluster.sql("golden",&format!("INSERT INTO pgokf_web.mcp_tokens(name,role,tenant,digest,created_by) VALUES('golden-{role}','{role}',NULL,'{}','golden-harness');",live::digest(live::token(role).as_bytes())))?;
    }
    let dsn = cluster.dsn("golden", "postgres");
    let mut actual = BTreeMap::new();
    let mut session = live::stdio(binary, &dsn, false, &evidence.join("stdio-read.json"))?;
    for name in [
        "stdio-initialize",
        "stdio-ping",
        "stdio-tools-list",
        "stdio-tools-call-concept_search",
        "stdio-tools-call-find_similar",
        "stdio-tools-call-concept_neighbors",
        "stdio-tools-call-get_concept",
        "stdio-tools-call-get_skill",
        "stdio-tools-call-list_plugin_targets",
        "stdio-tools-call-build_workspace_plugin",
    ] {
        stdio_fixture(name, &expected, &mut actual, &mut session, None)?;
    }
    let stdio_lock = lock(&actual["stdio-tools-call-build_workspace_plugin"][0]["response"])?;
    for name in [
        "stdio-tools-call-check_workspace_plugin_freshness",
        "stdio-write-without-writer",
        "stdio-protocol-errors",
    ] {
        stdio_fixture(
            name,
            &expected,
            &mut actual,
            &mut session,
            Some(&stdio_lock),
        )?;
    }
    session.stop()?;
    let mut session = live::stdio(binary, &dsn, false, &evidence.join("stdio-early.json"))?;
    stdio_fixture(
        "stdio-pre-initialize",
        &expected,
        &mut actual,
        &mut session,
        None,
    )?;
    session.stop()?;
    let mut session = live::stdio(binary, &dsn, true, &evidence.join("stdio-write.json"))?;
    live::stdio_exchange(
        &mut session,
        &live::request(0, "initialize", json!({"protocolVersion":"2025-06-18"})),
    )?;
    live::stdio_exchange(
        &mut session,
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )?;
    for tool in [
        "list_bundles",
        "put_document",
        "put_document-refused",
        "delete_document",
        "delete_document-refused",
        "create_content_bundle",
        "create_content_bundle-refused",
        "refresh_bundle",
        "refresh_bundle-refused",
        "set_bundle_state",
        "set_bundle_state-refused",
    ] {
        stdio_fixture(
            &format!("stdio-tools-call-{tool}"),
            &expected,
            &mut actual,
            &mut session,
            None,
        )?;
    }
    session.stop()?;
    let mut endpoint = Endpoint::start(
        binary,
        &dsn,
        true,
        None,
        BTreeMap::new(),
        &evidence.join("http.json"),
    )?;
    for name in [
        "http-protocol",
        "http-initialize",
        "http-admin-tools-call-build_workspace_plugin",
    ] {
        http_fixture(name, &expected, &mut actual, &mut endpoint, None)?;
    }
    let http_lock = lock(&actual["http-admin-tools-call-build_workspace_plugin"][0]["response"])?;
    for role in live::ROLES {
        http_fixture(
            &format!("http-{role}-tools-list"),
            &expected,
            &mut actual,
            &mut endpoint,
            None,
        )?;
        for tool in [
            "concept_search",
            "find_similar",
            "concept_neighbors",
            "get_concept",
            "get_skill",
            "check_workspace_plugin_freshness",
            "list_plugin_targets",
            "build_workspace_plugin",
            "list_bundles",
        ] {
            let name = format!("http-{role}-tools-call-{tool}");
            if expected.contains_key(&name) && !actual.contains_key(&name) {
                http_fixture(
                    &name,
                    &expected,
                    &mut actual,
                    &mut endpoint,
                    Some(&http_lock),
                )?;
            }
        }
    }
    for role in live::ROLES {
        http_fixture(
            &format!("http-{role}-refusals"),
            &expected,
            &mut actual,
            &mut endpoint,
            None,
        )?;
    }
    for name in [
        "http-writer-tools-call-put_document",
        "http-writer-tools-call-delete_document",
        "http-admin-tools-call-create_content_bundle",
        "http-admin-tools-call-refresh_bundle",
        "http-admin-tools-call-set_bundle_state",
    ] {
        http_fixture(name, &expected, &mut actual, &mut endpoint, None)?;
    }
    endpoint.stop()?;
    cluster.stop()?;
    crate::owned_docker::publish(&evidence.join("capture.json"), &actual)?;
    if actual.keys().ne(expected.keys()) {
        return Err("golden fixture coverage differs".into());
    }
    let mut failures = Vec::new();
    for (name, rows) in &actual {
        if let Err(error) = golden::compare(name, rows, &expected[name], &bundle) {
            failures.push(error);
        }
    }
    crate::owned_docker::publish(
        &evidence.join("results.json"),
        &json!({"fixtures":actual.len(),"failures":failures}),
    )?;
    if !failures.is_empty() {
        return Err(failures.join("\n"));
    }
    Ok(actual.len())
}
