//! Run the pinned official Node clients; all verdicts and result checks are Rust.
use super::{Native, Value, ensure, json, live};
use crate::process::CommandSpec;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
const CORE: &str = "@modelcontextprotocol/conformance@0.2.0-alpha.11";
const SKILLS_COMMIT: &str = "7169291ec0b68eb370fddcd9947313ab0d5e4156";
const AUTH_ADAPTER: &str = r"import {createRequire,syncBuiltinESMExports} from 'node:module';
import http from 'node:http';
const originalRequest=http.request;
http.request=function(options,...rest){if(options&&typeof options==='object'&&!(options instanceof URL)&&new URL(`${options.protocol??'http:'}//${options.hostname??options.host}:${options.port??80}`).origin===process.env.PGOKF_CONFORMANCE_ORIGIN){options={...options,headers:{...options.headers,Authorization:`Bearer ${process.env.PGOKF_CONFORMANCE_BEARER}`}};}return originalRequest.call(this,options,...rest);};
syncBuiltinESMExports();
const require=createRequire(process.env.PGOKF_CONFORMANCE_PACKAGE);
const {Dispatcher,getGlobalDispatcher,setGlobalDispatcher}=require('undici');
const delegate=getGlobalDispatcher();
setGlobalDispatcher(new class extends Dispatcher {dispatch(options,handler){if(String(options.origin)===process.env.PGOKF_CONFORMANCE_ORIGIN){const headers=Array.isArray(options.headers)?[...options.headers]:Object.entries(options.headers??{}).flat();headers.push('authorization',`Bearer ${process.env.PGOKF_CONFORMANCE_BEARER}`);options={...options,headers};}return delegate.dispatch(options,handler);}});
const original=globalThis.fetch;
globalThis.fetch=(input,init={})=>{const url=typeof input==='string'||input instanceof URL?String(input):input.url;if(new URL(url).origin===process.env.PGOKF_CONFORMANCE_ORIGIN){const headers=new Headers(init.headers??(input instanceof Request?input.headers:undefined));headers.set('Authorization',`Bearer ${process.env.PGOKF_CONFORMANCE_BEARER}`);return original(input,{...init,headers});}return original(input,init);};";
// This adapter calls SDK APIs and emits observations. It makes no verification decision.
const SDK_ADAPTER: &str = r"const {Client,StreamableHTTPClientTransport}=await import(process.env.PGOKF_SDK_MODULE);
for(const mode of [{pin:'2026-07-28'},'legacy']){const client=new Client({name:'pgokf-verifier',version:'1'},{capabilities:{},versionNegotiation:{mode}});const transport=new StreamableHTTPClientTransport(new URL(process.env.PGOKF_CONFORMANCE_ORIGIN+'/mcp'),{requestInit:{headers:{Authorization:`Bearer ${process.env.PGOKF_CONFORMANCE_BEARER}`}}});try{await client.connect(transport);const discovery=mode==='legacy'?null:await client.discover();const tools=await client.listTools();const resources=await client.listResources();const templates=await client.listResourceTemplates();const content=await client.readResource({uri:'okf://catalog/manifest'});const result=await client.callTool({name:'get_concept',arguments:{bundle_id:1,concept_id:'runbooks/failover'}});const arrayResult=await client.callTool({name:'concept_search',arguments:{query:'failover',limit:2}});console.log(JSON.stringify({mode,discovery,tools,resources,templates,content,result,arrayResult}));}finally{await client.close();}}";
fn command(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    log: &Path,
) -> Result<Vec<u8>, String> {
    let mut c = CommandSpec::new(program);
    c.args = args.iter().map(Into::into).collect();
    c.cwd = cwd.map(Path::to_owned);
    c.timeout = Duration::from_mins(10);
    let output = c.run()?;
    std::fs::write(
        log,
        [output.stdout.as_slice(), output.stderr.as_slice()].concat(),
    )
    .map_err(|e| e.to_string())?;
    ensure(
        output.status.success(),
        &format!(
            "external command {program} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
    )?;
    Ok(output.stdout)
}
fn package(root: &Path, name: &str, version: &str) -> Result<(), String> {
    let meta: Value = crate::json::from_slice(
        &std::fs::read(root.join("package.json")).map_err(|e| e.to_string())?,
    )?;
    ensure(
        meta["name"] == name && meta["version"] == version,
        "external package identity",
    )
}
#[derive(Default, serde::Serialize, Debug, PartialEq)]
struct CheckTally {
    success: usize,
    warning: usize,
    skipped: usize,
    identities: std::collections::BTreeMap<String, String>,
}
fn checks(root: &Path) -> Result<CheckTally, String> {
    let mut tally = CheckTally::default();
    for entry in std::fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            let t = checks(&entry.path())?;
            tally.success += t.success;
            tally.warning += t.warning;
            tally.skipped += t.skipped;
            for (identity, status) in t.identities {
                ensure(
                    tally.identities.insert(identity, status).is_none(),
                    "duplicate official check identity",
                )?;
            }
        } else if entry.file_name() == "checks.json" {
            let rows: Vec<Value> =
                crate::json::from_slice(&std::fs::read(entry.path()).map_err(|e| e.to_string())?)?;
            ensure(!rows.is_empty(), "official scenario emitted no checks")?;
            for row in rows {
                let id = row["id"].as_str().ok_or("official check ID")?;
                let name = row["name"].as_str().ok_or("official check name")?;
                ensure(
                    !id.trim().is_empty()
                        && !name.trim().is_empty()
                        && !id.contains(['\n', '\r'])
                        && !name.contains(['\n', '\r']),
                    "empty or noncanonical official identity",
                )?;
                let status = row["status"].as_str().ok_or("official check status")?;
                ensure(
                    tally
                        .identities
                        .insert(format!("{id}\n{name}"), status.into())
                        .is_none(),
                    "duplicate official check identity",
                )?;
                match row["status"].as_str() {
                    Some("SUCCESS") => tally.success += 1,
                    Some("WARNING") => tally.warning += 1,
                    Some("SKIPPED") => tally.skipped += 1,
                    _ => return Err(format!("official failed/unknown check: {}", row["id"])),
                }
            }
        }
    }
    Ok(tally)
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Expected {
    id: String,
    name: String,
    status: String,
}
fn expected_checks(scenario: &str) -> Result<CheckTally, String> {
    let (success, warning, skipped) = match scenario {
        "tools-list" => (3, 0, 0),
        "resources-list" | "dns-rebinding-protection" => (2, 0, 0),
        "sep-2164-resource-not-found" => (3, 1, 0),
        "http-header-validation" => (14, 0, 0),
        "sep-2640-skills-enumeration" => (32, 0, 0),
        "sep-2640-skills-manifest" => (4, 2, 0),
        "sep-2640-skills-directory" => (1, 0, 6),
        _ => return Err("unknown selected scenario".into()),
    };
    let profiles: std::collections::BTreeMap<String, Vec<Expected>> =
        crate::json::from_slice(include_bytes!("official_checks.json"))?;
    let rows = profiles
        .get(scenario)
        .ok_or("unprofiled official scenario")?;
    let identities = rows
        .iter()
        .map(|r| (format!("{}\n{}", r.id, r.name), r.status.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    ensure(
        identities.len() == rows.len(),
        "duplicate expected official identity",
    )?;
    Ok(CheckTally {
        success,
        warning,
        skipped,
        identities,
    })
}
fn archive_digest(bytes: &[u8], expected: &str) -> Result<String, String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha512};
    let digest = STANDARD.encode(Sha512::digest(bytes));
    ensure(digest == expected, "official registry archive SHA512")?;
    Ok(digest)
}
fn validate_checks(scenario: &str, observed: &CheckTally) -> Result<(), String> {
    ensure(
        *observed == expected_checks(scenario)?,
        "official exact check identities/status profile",
    )
}
fn verify_archive(
    package_spec: &str,
    core: &Path,
    integrity: &str,
    tag: &str,
    evidence: &Path,
) -> Result<(), String> {
    let packed = command(
        "npm",
        &["pack", "--ignore-scripts", "--json", package_spec],
        Some(evidence),
        &evidence.join(format!("{tag}-pack.json")),
    )?;
    let rows: Vec<Value> = crate::json::from_slice(&packed)?;
    ensure(rows.len() == 1, "one pinned archive")?;
    let filename = rows[0]["filename"].as_str().ok_or("archive filename")?;
    ensure(
        Path::new(filename).components().count() == 1,
        "archive basename",
    )?;
    let archive = evidence.join(filename);
    let digest = archive_digest(
        &std::fs::read(&archive).map_err(|e| e.to_string())?,
        integrity,
    )?;
    let unpacked = evidence.join(format!("{tag}-archive"));
    std::fs::create_dir(&unpacked).map_err(|e| e.to_string())?;
    command(
        "tar",
        &[
            "-xzf",
            archive.to_str().ok_or("archive path")?,
            "-C",
            unpacked.to_str().ok_or("archive destination")?,
        ],
        None,
        &evidence.join(format!("{tag}-unpack.log")),
    )?;
    let expected = unpacked.join("package");
    compare_tree(&expected, core)?;
    crate::owned_docker::publish(
        &evidence.join(format!("{tag}-integrity.json")),
        &json!({"sha512":digest,"executable_and_requirements_equal":true}),
    )
}
fn compare_tree(expected: &Path, actual: &Path) -> Result<(), String> {
    if expected.is_file() {
        return ensure(
            std::fs::read(expected).map_err(|e| e.to_string())?
                == std::fs::read(actual).map_err(|e| e.to_string())?,
            "pinned official file differs",
        );
    }
    let names = |path: &Path| -> Result<std::collections::BTreeSet<std::ffi::OsString>, String> {
        std::fs::read_dir(path)
            .map_err(|e| e.to_string())?
            .map(|e| e.map(|e| e.file_name()).map_err(|e| e.to_string()))
            .collect()
    };
    let expected_names = names(expected)?;
    let mut actual_names = names(actual)?;
    if !expected_names.contains(std::ffi::OsStr::new("node_modules")) {
        actual_names.remove(std::ffi::OsStr::new("node_modules"));
    }
    ensure(
        expected_names == actual_names,
        "pinned official directory differs",
    )?;
    for name in expected_names {
        compare_tree(&expected.join(&name), &actual.join(&name))?;
    }
    Ok(())
}
#[allow(clippy::too_many_lines)] // Keep pinned installation and authentication ordering together.
fn pinned_clients(evidence: &Path) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), String> {
    let clients = if let Some(path) = std::env::var_os("PGOKF_MCP_EXTERNAL_CLIENTS") {
        PathBuf::from(path)
    } else {
        let path = evidence.join("clients");
        std::fs::create_dir(&path).map_err(|e| e.to_string())?;
        command(
            "npm",
            &[
                "install",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--save-exact",
                CORE,
                "@modelcontextprotocol/inspector@2.7.0",
                "@modelcontextprotocol/client@2.0.0",
            ],
            Some(&path),
            &evidence.join("npm-clients.log"),
        )?;
        path.join("node_modules/@modelcontextprotocol")
    };
    let core = clients.join("conformance");
    let inspector = clients.join("inspector");
    let sdk = clients.join("client");
    package(&core, "@modelcontextprotocol/conformance", "0.2.0-alpha.11")?;
    package(&inspector, "@modelcontextprotocol/inspector", "2.7.0")?;
    package(&sdk, "@modelcontextprotocol/client", "2.0.0")?;
    let metadata = command(
        "npm",
        &["view", CORE, "gitHead", "--json"],
        None,
        &evidence.join("core-provenance.json"),
    )?;
    let head: String = crate::json::from_slice(&metadata)?;
    ensure(
        head == "c321dd32035556e6769d3724a8ee97d87c3faaac",
        "official core commit pin",
    )?;
    verify_archive(
        CORE,
        &core,
        "imPK9tx5gQsL6ZKQq4MrsyDYfSaIwpRmX6+ogjbeAXs9LGvxkBxWcY7KcS7TvwaBk/ZiVWl6b/naF4q83UwDRA==",
        "core",
        evidence,
    )?;
    verify_archive(
        "@modelcontextprotocol/client@2.0.0",
        &sdk,
        "8f1OghQ2rjzIOfqgUCP+8GiUWqRs89njoWLNqAe8kWmDePv3s1fZXseej+QXemssEuuOvLLmLO/kqM3IQHtISw==",
        "sdk",
        evidence,
    )?;
    verify_archive(
        "@modelcontextprotocol/inspector@2.7.0",
        &inspector,
        "V1SqfR+m3NWMkEe2i3v2GMm00pZtAl/JISAHQzYnOZElEuwuLLkU8vTWBYg5HMCYTgpvuMmzb+MZz98HjdcGuw==",
        "inspector",
        evidence,
    )?;
    let skills = if let Some(path) = std::env::var_os("PGOKF_MCP_SKILLS_SOURCE") {
        PathBuf::from(path)
    } else {
        let path = evidence.join("skills-source");
        std::fs::create_dir(&path).map_err(|e| e.to_string())?;
        command(
            "git",
            &["init", "--quiet"],
            Some(&path),
            &evidence.join("skills-init.log"),
        )?;
        command(
            "git",
            &[
                "fetch",
                "--depth=1",
                "https://github.com/modelcontextprotocol/conformance.git",
                SKILLS_COMMIT,
            ],
            Some(&path),
            &evidence.join("skills-fetch.log"),
        )?;
        command(
            "git",
            &["checkout", "--quiet", "--detach", "FETCH_HEAD"],
            Some(&path),
            &evidence.join("skills-checkout.log"),
        )?;
        path
    };
    let head = command(
        "git",
        &["rev-parse", "HEAD"],
        Some(&skills),
        &evidence.join("skills-head.log"),
    )?;
    ensure(
        String::from_utf8_lossy(&head).trim() == SKILLS_COMMIT,
        "Skills exact source pin",
    )?;
    let dirty = command(
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
        Some(&skills),
        &evidence.join("skills-status.log"),
    )?;
    ensure(dirty.is_empty(), "modified official Skills source")?;
    command(
        "npm",
        &["ci", "--ignore-scripts", "--no-audit", "--no-fund"],
        Some(&skills),
        &evidence.join("skills-install.log"),
    )?;
    command(
        "npm",
        &["run", "build"],
        Some(&skills),
        &evidence.join("skills-build.log"),
    )?;
    Ok((core, skills, inspector, sdk))
}
#[allow(clippy::too_many_lines)] // Keep external gate ordering and cleanup reviewable together.
pub(super) fn run(
    root: &Path,
    pg_config: &Path,
    binary: &Path,
    evidence: &Path,
) -> Result<(), String> {
    live::begin_evidence(root, pg_config, binary, evidence)?;
    let evidence = evidence.canonicalize().map_err(|e| e.to_string())?;
    let (core, skills, inspector, sdk) = pinned_clients(&evidence)?;
    let auth = evidence.join("auth-adapter.mjs");
    let sdk_adapter = evidence.join("sdk-adapter.mjs");
    std::fs::write(&auth, AUTH_ADAPTER).map_err(|e| e.to_string())?;
    std::fs::write(&sdk_adapter, SDK_ADAPTER).map_err(|e| e.to_string())?;
    let mut native = Native::setup(root, pg_config, binary, &evidence, true)?;
    native.endpoint.stop()?;
    native.endpoint = live::Endpoint::start_same_origin(
        binary,
        &native.cluster.dsn(super::DB, "mcp_contract"),
        &evidence.join("official-http.json"),
    )?;
    let policy: Value = crate::json::from_slice(
        &std::fs::read(root.join("spec/mcp-conformance-applicability.json"))
            .map_err(|e| e.to_string())?,
    )?;
    let discovery = native.success("server/discover", json!({}))?;
    ensure(
        discovery["capabilities"] == policy["capabilities"],
        "official applicability matches live capabilities",
    )?;
    let mut receipts = Vec::new();
    for (scenario, runner) in [
        ("tools-list", &core),
        ("resources-list", &core),
        ("sep-2164-resource-not-found", &core),
        ("dns-rebinding-protection", &core),
        ("http-header-validation", &core),
        ("sep-2640-skills-enumeration", &skills),
        ("sep-2640-skills-manifest", &skills),
        ("sep-2640-skills-directory", &skills),
    ] {
        let out = evidence.join(scenario);
        std::fs::create_dir(&out).map_err(|e| e.to_string())?;
        let mut c = CommandSpec::new("node");
        c.args = vec![
            "--import".into(),
            auth.as_os_str().into(),
            runner.join("dist/index.js").into_os_string(),
            "server".into(),
            "--url".into(),
            format!("{}/mcp", native.endpoint.origin).into(),
            "--scenario".into(),
            scenario.into(),
            "--output-dir".into(),
            out.as_os_str().into(),
            "--spec-version".into(),
            "2026-07-28".into(),
            "--verbose".into(),
        ];
        if runner == &skills {
            c.args.push("--force".into());
        }
        c.env.insert(
            "PGOKF_CONFORMANCE_PACKAGE".into(),
            core.join("package.json").into_os_string(),
        );
        c.env.insert(
            "PGOKF_CONFORMANCE_ORIGIN".into(),
            native.endpoint.origin.clone().into(),
        );
        c.env.insert(
            "PGOKF_CONFORMANCE_BEARER".into(),
            live::token("reader").into(),
        );
        c.timeout = Duration::from_mins(2);
        let output = c.run()?;
        std::fs::write(out.join("stdout.log"), &output.stdout).map_err(|e| e.to_string())?;
        std::fs::write(out.join("stderr.log"), &output.stderr).map_err(|e| e.to_string())?;
        ensure(
            output.status.success(),
            &format!("official scenario {scenario} exit {}", output.status),
        )?;
        let passed = checks(&out)?;
        validate_checks(scenario, &passed)?;
        receipts.push(json!({"scenario":scenario,"checks":passed}));
    }
    let mut c = CommandSpec::new("node");
    c.args = vec![sdk_adapter.as_os_str().into()];
    c.env.insert(
        "PGOKF_SDK_MODULE".into(),
        sdk.join("dist/index.mjs").into_os_string(),
    );
    c.env.insert(
        "PGOKF_CONFORMANCE_PACKAGE".into(),
        core.join("package.json").into_os_string(),
    );
    c.env.insert(
        "PGOKF_CONFORMANCE_ORIGIN".into(),
        native.endpoint.origin.clone().into(),
    );
    c.env.insert(
        "PGOKF_CONFORMANCE_BEARER".into(),
        live::token("reader").into(),
    );
    let output = c.checked()?;
    std::fs::write(evidence.join("sdk.jsonl"), &output.stdout).map_err(|e| e.to_string())?;
    let text = std::str::from_utf8(&output.stdout).map_err(|e| e.to_string())?;
    let observations = text
        .lines()
        .map(|l| crate::json::from_slice::<Value>(l.as_bytes()))
        .collect::<Result<Vec<_>, _>>()?;
    ensure(observations.len() == 2, "both SDK protocol modes")?;
    for observation in observations {
        let legacy = observation["mode"] == "legacy";
        ensure(
            observation["tools"]["tools"]
                .as_array()
                .is_some_and(|a| a.len() == 7)
                && observation["resources"]["resources"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty())
                && observation["templates"]["resourceTemplates"]
                    .as_array()
                    .is_some_and(|a| a.len() == 5)
                && observation["content"]["contents"]
                    .as_array()
                    .is_some_and(|a| a.len() == 1)
                && observation["result"]["isError"] == false
                && observation["arrayResult"]["isError"] == false,
            "SDK successful exact API surface",
        )?;
        ensure(
            if legacy {
                observation["arrayResult"]["structuredContent"]["value"].is_array()
            } else {
                observation["discovery"]["supportedVersions"] == json!(["2026-07-28"])
                    && observation["arrayResult"]["structuredContent"].is_array()
            },
            "SDK dialect projection",
        )?;
        receipts.push(json!({"sdk_mode":observation["mode"],"passed":true}));
    }
    for era in ["legacy", "modern"] {
        for method in [
            "tools/list",
            "resources/list",
            "resources/templates/list",
            "resources/read",
            "tools/call",
        ] {
            let mut c = CommandSpec::new("node");
            c.args = vec![
                inspector
                    .join("clients/cli/build/index.js")
                    .into_os_string(),
                "--server-url".into(),
                format!("{}/mcp", native.endpoint.origin).into(),
                "--transport".into(),
                "http".into(),
                "--protocol-era".into(),
                era.into(),
                "--method".into(),
                method.into(),
                "--header".into(),
                format!("Authorization: Bearer {}", live::token("reader")).into(),
            ];
            if method == "resources/read" {
                c.args
                    .extend(["--uri".into(), "okf://catalog/manifest".into()]);
            }
            if method == "tools/call" {
                c.args.extend([
                    "--tool-name".into(),
                    "get_concept".into(),
                    "--tool-arg".into(),
                    "bundle_id=1".into(),
                    "concept_id=services/widget".into(),
                ]);
            }
            let output = c.checked()?;
            let value: Value = crate::json::from_slice(&output.stdout)?;
            ensure(
                value.is_object()
                    && value.get("error").is_none()
                    && value.get("isError") != Some(&json!(true)),
                "Inspector response",
            )?;
            let (field, minimum) = match method {
                "tools/list" => ("tools", 7),
                "resources/list" => ("resources", 1),
                "resources/templates/list" => ("resourceTemplates", 5),
                "resources/read" => ("contents", 1),
                _ => ("content", 1),
            };
            ensure(
                value[field].as_array().is_some_and(|a| a.len() >= minimum),
                "Inspector result inventory/content",
            )?;
            ensure(
                value[field]
                    .as_array()
                    .is_some_and(|rows| rows.iter().all(Value::is_object)),
                "Inspector typed result members",
            )?;
            std::fs::write(
                evidence.join(format!("inspector-{era}-{}.json", method.replace('/', "-"))),
                &output.stdout,
            )
            .map_err(|e| e.to_string())?;
            receipts.push(json!({"inspector_era":era,"method":method,"passed":true}));
        }
    }
    native.readonly.stop()?;
    native.endpoint.stop()?;
    native.cluster.stop()?;
    crate::owned_docker::publish(
        &evidence.join("external-report.json"),
        &json!({"passed":true,"receipts":receipts}),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_results_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checks.json");
        for status in ["FAILURE", "UNKNOWN"] {
            std::fs::write(
                &path,
                serde_json::to_vec(&json!([{"id":"check","name":"Check","status":status}]))
                    .unwrap(),
            )
            .unwrap();
            assert!(checks(dir.path()).is_err());
        }
        std::fs::write(&path, b"[]").unwrap();
        assert!(checks(dir.path()).is_err());
        let profiles: Value =
            crate::json::from_slice(include_bytes!("official_checks.json")).unwrap();
        let good = &profiles["tools-list"];
        std::fs::write(&path, serde_json::to_vec(good).unwrap()).unwrap();
        assert_eq!(
            checks(dir.path()).unwrap(),
            expected_checks("tools-list").unwrap()
        );
        let mut replaced = good.clone();
        replaced[0]["id"] = json!("replacement");
        std::fs::write(&path, serde_json::to_vec(&replaced).unwrap()).unwrap();
        assert!(validate_checks("tools-list", &checks(dir.path()).unwrap()).is_err());
        for field in ["id", "name"] {
            let mut bad = good.clone();
            bad[0][field] = json!("");
            std::fs::write(&path, serde_json::to_vec(&bad).unwrap()).unwrap();
            assert!(checks(dir.path()).is_err());
        }
        let mut duplicated = good.clone();
        duplicated.as_array_mut().unwrap().push(good[0].clone());
        std::fs::write(&path, serde_json::to_vec(&duplicated).unwrap()).unwrap();
        assert!(checks(dir.path()).is_err());
        let mut status = profiles["sep-2640-skills-directory"].clone();
        status[0]["status"] = json!("SKIPPED");
        status[1]["status"] = json!("SUCCESS");
        std::fs::write(&path, serde_json::to_vec(&status).unwrap()).unwrap();
        assert!(
            validate_checks("sep-2640-skills-directory", &checks(dir.path()).unwrap()).is_err()
        );
        assert!(expected_checks("unknown").is_err());
    }
    #[test]
    fn registry_archive_bytes_are_authenticated() {
        archive_digest(b"abc","3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw==").unwrap();
        assert!(archive_digest(b"changed","3a81oZNherrMQXNJriBBMRLm+k6JqX6iCp7u5ktV05ohkpkqJ0/BqDa6PCOj/uu9RU1EI2Q86A4qmslPpUyknw==").is_err());
    }
    #[test]
    fn official_tree_detects_changed_missing_and_extra_files() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("index.js"), b"official").unwrap();
        std::fs::write(b.path().join("index.js"), b"official").unwrap();
        compare_tree(a.path(), b.path()).unwrap();
        std::fs::write(b.path().join("index.js"), b"patched").unwrap();
        assert!(compare_tree(a.path(), b.path()).is_err());
        std::fs::write(b.path().join("index.js"), b"official").unwrap();
        std::fs::write(b.path().join("extra"), b"x").unwrap();
        assert!(compare_tree(a.path(), b.path()).is_err());
    }
}
