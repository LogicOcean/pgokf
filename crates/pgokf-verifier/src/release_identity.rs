//! Source and distribution identities, authenticated formula tuples and release intent.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const LAST_PUBLISHED: &str = "0.3.1";
pub const PUBLISHED_DIGEST: &str =
    "194441d1b4d6bd5cf5f22a39a3b6c923e9a68d7692202ff7c4bb05109df812e5";
/// SHA256 of the authenticated immutable `v0.4.0` GitHub tag archive consumed by
/// the post-tag formula (tag object `daa61f66`, commit `a471b1fb`).
pub const PUBLISHED_0_4_0_DIGEST: &str =
    "54f3d3e575cc3d66b1a58f28838ceb5f99cdbeb9f33c3b823f334fab54effed8";
/// SHA256 of the authenticated immutable `v0.5.0` GitHub tag archive consumed by
/// the post-tag formula (tag object `fb8522a8`, commit `36307da`).
pub const PUBLISHED_0_5_0_DIGEST: &str =
    "8e0beb9c543147e9f7c690b3b6f904709c3249060ac3928c30c7d94d25b0cf0b";
/// SHA256 of the authenticated immutable `v0.6.0` GitHub tag archive consumed by
/// the post-tag formula (tag object `137609e8`, commit `5d16ef7`).
pub const PUBLISHED_0_6_0_DIGEST: &str =
    "f92041aa98d22a63a73405afe4d554b1d1ee9b755a0b7f45ad3b4b1966e4f7f1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceManifest {
    pub version: String,
    pub distribution_version: String,
    pub packages: BTreeSet<String>,
}

fn read(root: &Path, path: &str) -> Result<String, String> {
    std::fs::read_to_string(root.join(path)).map_err(|e| format!("{path}: {e}"))
}
fn toml(source: &str) -> Result<toml::Value, String> {
    toml::from_str(source).map_err(|e| e.to_string())
}
fn at<'a>(value: &'a toml::Value, keys: &[&str]) -> Option<&'a toml::Value> {
    keys.iter().try_fold(value, |v, k| v.get(*k))
}
fn text<'a>(value: &'a toml::Value, keys: &[&str]) -> Option<&'a str> {
    at(value, keys)?.as_str()
}

/// Validate every local manifest and exact internal pin, including target sections.
/// TOML and JSON duplicate keys are rejected before identity comparison.
/// # Errors
/// Missing, inconsistent, duplicate or unpinned identities fail.
pub fn source_identity(root: &Path) -> Result<SourceManifest, String> {
    source_identity_version(root, env!("CARGO_PKG_VERSION"))
}

pub(crate) fn source_identity_version(
    root: &Path,
    expected: &str,
) -> Result<SourceManifest, String> {
    let version = crate::release::compatibility_identity(
        &read(root, "crates/extension/pgokf.control")?,
        expected,
    )?;
    let workspace = toml(&read(root, "Cargo.toml")?)?;
    if text(&workspace, &["workspace", "package", "version"]) != Some(&version) {
        return Err("workspace/control mismatch".into());
    }
    check_dependencies(&workspace, &version)?;
    let members = at(&workspace, &["workspace", "members"])
        .and_then(toml::Value::as_array)
        .ok_or("workspace members missing")?;
    let mut declared = BTreeSet::new();
    for member in members {
        let member = member.as_str().ok_or("literal workspace member required")?;
        if !declared.insert(member.to_owned()) {
            return Err("duplicate workspace member".into());
        }
    }
    let published = if version.contains('-') {
        LAST_PUBLISHED
    } else {
        &version
    };
    let meta: serde_json::Value = crate::json::from_slice(read(root, "META.json")?.as_bytes())?;
    if meta["version"] != published
        || meta["provides"]["pgokf"]["version"] != published
        || meta["provides"]["pgokf"]["file"]
            != format!("crates/extension/sql/pgokf--{published}.sql")
    {
        return Err("published META identity mismatch".into());
    }
    let lock = toml(&read(root, "Cargo.lock")?)?;
    let entries = lock
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or("lock packages missing")?;
    if expected == env!("CARGO_PKG_VERSION")
        && entries
            .iter()
            .any(|p| text(p, &["name"]) == Some("serde_cbor"))
    {
        return Err("serde_cbor reintroduced (RUSTSEC-2021-0127)".into());
    }
    let mut packages = BTreeSet::new();
    let mut discovered = BTreeSet::new();
    let mut dirs = std::fs::read_dir(root.join("crates"))
        .map_err(|e| e.to_string())?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    dirs.sort();
    for dir in dirs {
        let path = dir.join("Cargo.toml");
        if !path.is_file() {
            continue;
        }
        discovered.insert(format!(
            "crates/{}",
            dir.file_name()
                .and_then(|s| s.to_str())
                .ok_or("invalid crate name")?
        ));
        let manifest = toml(&std::fs::read_to_string(path).map_err(|e| e.to_string())?)?;
        let name = text(&manifest, &["package", "name"]).ok_or("package name missing")?;
        if !packages.insert(name.to_owned()) {
            return Err(format!("duplicate local package {name}"));
        }
        let declaration = at(&manifest, &["package", "version"]);
        if declaration.and_then(toml::Value::as_str) != Some(&version)
            && declaration
                .and_then(|v| v.get("workspace"))
                .and_then(toml::Value::as_bool)
                != Some(true)
        {
            return Err(format!("{name}: package/control mismatch"));
        }
        let locked: Vec<_> = entries
            .iter()
            .filter(|p| text(p, &["name"]) == Some(name))
            .collect();
        if locked.len() != 1 || text(locked[0], &["version"]) != Some(&version) {
            return Err(format!("{name}: unique matching lock identity required"));
        }
        check_dependencies(&manifest, &version)?;
    }
    if packages.is_empty() {
        return Err("no local packages".into());
    }
    if declared != discovered {
        return Err("workspace members differ from local package inventory".into());
    }
    Ok(SourceManifest {
        version: version.clone(),
        distribution_version: published.to_owned(),
        packages,
    })
}
fn check_dependencies(value: &toml::Value, version: &str) -> Result<(), String> {
    if let Some(table) = value.as_table() {
        for (key, child) in table {
            if matches!(
                key.as_str(),
                "dependencies" | "dev-dependencies" | "build-dependencies"
            ) {
                for (name, dep) in child.as_table().ok_or("invalid dependency table")? {
                    if dep.get("path").is_some()
                        && text(dep, &["version"]) != Some(&format!("={version}"))
                    {
                        return Err(format!(
                            "{name}: internal path dependency requires exact ={version}"
                        ));
                    }
                }
            } else {
                check_dependencies(child, version)?;
            }
        }
    }
    Ok(())
}

/// Distribution files retain the last published identity during development.
/// # Errors
/// Missing or stale distribution declarations fail.
pub fn distribution_identity(root: &Path, identity: &SourceManifest) -> Result<(), String> {
    let version = &identity.distribution_version;
    let issue = crate::workflow::parse(&read(root, ".github/ISSUE_TEMPLATE/bug_report.yml")?)?;
    let entries: Vec<_> = issue["body"]
        .as_array()
        .ok_or("issue template body missing")?
        .iter()
        .filter(|item| item["id"] == "pgokf-version")
        .collect();
    if entries.len() != 1 || entries[0]["attributes"]["placeholder"] != *version {
        return Err("issue template active version mismatch".into());
    }
    for path in [
        "packaging/docker/Dockerfile",
        "packaging/docker/Dockerfile.companions",
    ] {
        let source = read(root, path)?;
        let declarations: Vec<_> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("ARG PGOKF_VERSION="))
            .collect();
        if declarations != [format!("\"{version}\"").as_str()] {
            return Err(format!("{path}: active image version mismatch"));
        }
    }
    let env = read(root, "deploy/compose/.env.example")?;
    for (key, expected) in [
        (
            "PGOKF_IMAGE",
            format!("ghcr.io/logicocean/pgokf:{version}-pg18"),
        ),
        (
            "PGOKF_COMPANIONS_IMAGE",
            format!("ghcr.io/logicocean/pgokf-companions:{version}"),
        ),
    ] {
        let values: Vec<_> = env
            .lines()
            .filter_map(|line| line.trim().strip_prefix(&format!("{key}=")))
            .collect();
        if values != [expected.as_str()] {
            return Err(format!("Compose {key} identity mismatch"));
        }
    }
    let spec = read(root, "packaging/rpm/pgokf.spec")?;
    let versions: Vec<_> = spec
        .lines()
        .filter_map(|l| l.strip_prefix("Version:"))
        .map(str::trim)
        .collect();
    if versions != [version] {
        return Err("RPM version mismatch".into());
    }
    let install = format!("crates/extension/sql/pgokf--{version}.sql");
    if !root.join(install).is_file() {
        return Err("published install script missing".into());
    }
    for path in [
        "packaging/docker/Dockerfile",
        "packaging/docker/Dockerfile.companions",
    ] {
        let source = read(root, path)?;
        for token in source.split_whitespace() {
            if let Some((_, tag)) = token
                .split_once("pgokf:")
                .or_else(|| token.split_once("pgokf-companions:"))
                && tag.starts_with(|c: char| c.is_ascii_digit())
                && tag.trim_matches(['\'', '"', '`', ',']).split("-pg").next() != Some(version)
            {
                return Err(format!("{path}: stale image example"));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormulaTuple {
    pub version: String,
    pub sha256: String,
    pub assertions: Vec<String>,
}
/// Read the formula's narrow, literal release declarations; reject duplicates.
/// The executable Ruby API is separately exercised by the live policy command.
/// # Errors
/// Dynamic, missing or duplicated release declarations fail.
pub fn formula_tuple(source: &str) -> Result<FormulaTuple, String> {
    fn declarations(source: &str, key: &str) -> Result<Vec<String>, String> {
        source
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix(key))
            .map(|tail| {
                let quoted = tail
                    .trim_start()
                    .strip_prefix('"')
                    .ok_or("literal formula string required")?;
                let end = quoted.find('"').ok_or("unclosed formula string")?;
                if quoted[..end].contains(['\\', '#']) {
                    return Err("dynamic formula string forbidden".into());
                }
                let remainder = quoted[end + 1..].trim();
                if key == "assert_match " {
                    if !matches!(remainder, ", control.read" | ", output") {
                        return Err("formula assertion target changed".into());
                    }
                } else if !remainder.is_empty() && !remainder.starts_with('#') {
                    return Err("executable formula declaration suffix forbidden".into());
                }
                Ok(quoted[..end].to_owned())
            })
            .collect()
    }
    let urls = declarations(source, "url ")?;
    let digests = declarations(source, "sha256 ")?;
    if urls.len() != 1 || digests.len() != 1 {
        return Err("unique formula url/digest required".into());
    }
    let version = urls[0]
        .strip_prefix("https://github.com/LogicOcean/pgokf/archive/refs/tags/v")
        .and_then(|s| s.strip_suffix(".tar.gz"))
        .ok_or("versioned archive required")?;
    let mut assertions: Vec<String> = declarations(source, "assert_match ")?
        .into_iter()
        .map(|s| {
            s.strip_prefix("default_version = '")
                .and_then(|s| s.strip_suffix('\''))
                .unwrap_or(&s)
                .to_owned()
        })
        .collect();
    let control = declarations(source, "expected_control_version = ")?;
    let extension = declarations(source, "expected_extension_version = ")?;
    if !control.is_empty() || !extension.is_empty() {
        if !assertions.is_empty() || control.len() != 1 || extension.len() != 1 {
            return Err("unique Formula observation expectations required".into());
        }
        assertions.extend(control);
        assertions.extend(extension);
    }
    Ok(FormulaTuple {
        version: version.into(),
        sha256: digests[0].clone(),
        assertions,
    })
}
/// # Errors
/// Retired releases, unauthenticated digests or mixed assertions fail.
pub fn authenticate_formula(
    tuple: &FormulaTuple,
    records: &BTreeMap<String, String>,
) -> Result<(), String> {
    if tuple.version == "0.3.0" {
        return Err("retired unpublished release".into());
    }
    if tuple.sha256.len() != 64
        || !tuple
            .sha256
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        || records.get(&tuple.version) != Some(&tuple.sha256)
    {
        return Err("formula requires authenticated release digest".into());
    }
    if tuple.assertions.len() != 2 || tuple.assertions.iter().any(|v| v != &tuple.version) {
        return Err("formula assertions disagree with release tuple".into());
    }
    if records
        .iter()
        .any(|(v, d)| v != &tuple.version && d == &tuple.sha256)
    {
        return Err("digest reused across releases".into());
    }
    Ok(())
}
#[must_use]
pub fn published_digests() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("0.2.0".into(), PUBLISHED_DIGEST.into()),
        ("0.4.0".into(), PUBLISHED_0_4_0_DIGEST.into()),
        ("0.5.0".into(), PUBLISHED_0_5_0_DIGEST.into()),
        ("0.6.0".into(), PUBLISHED_0_6_0_DIGEST.into()),
    ])
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseIntent {
    pub version: String,
    pub publish: bool,
    pub source_ref: String,
}
/// Resolve immutable tag intent. Tag resolution is supplied by a bounded Git adapter.
/// # Errors
/// Historical identity, dirty source, invalid OIDs and mismatched tag commits fail.
pub fn release_intent(
    version: &str,
    head: &str,
    clean: bool,
    event_name: &str,
    _event_ref: &str,
    dispatch: &str,
    resolve: impl FnOnce(&str) -> Result<String, String>,
) -> Result<ReleaseIntent, String> {
    if version != env!("CARGO_PKG_VERSION") {
        return Err("historical/retired source forbidden".into());
    }
    if !clean {
        return Err("tracked source differs from HEAD".into());
    }
    if head.len() != 40 || !head.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err("invalid commit identity".into());
    }
    if event_name != "workflow_dispatch" && !dispatch.is_empty() {
        return Err("release tag input is accepted only for workflow_dispatch".into());
    }
    let tag = if event_name == "workflow_dispatch" && !dispatch.is_empty() {
        Some(format!("refs/tags/{dispatch}"))
    } else {
        None
    };
    if let Some(tag) = &tag {
        if tag != &format!("refs/tags/v{version}") {
            return Err("tag does not match current version".into());
        }
        if resolve(tag)? != head {
            return Err("HEAD is not the tag commit".into());
        }
    }
    Ok(ReleaseIntent {
        version: version.into(),
        publish: tag.is_some(),
        source_ref: head.into(),
    })
}
