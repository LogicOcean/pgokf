// SPDX-License-Identifier: AGPL-3.0-only
//! Semantic substitution campaign over isolated source and extension prefixes.
use crate::{
    json,
    process::CommandSpec,
    upgrade::{Options, read},
};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::{
    collections::BTreeSet,
    fs,
    io::{Read as _, Seek as _, Write as _},
    path::{Path, PathBuf},
    time::Duration,
};
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    path: String,
    before: Option<String>,
    after: Option<String>,
    #[serde(default)]
    copy_from: Option<String>,
    #[serde(default)]
    delete: bool,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mutation {
    name: String,
    #[serde(default)]
    unit_test: Option<String>,
    #[serde(default)]
    failure_contains: Option<String>,
    #[serde(default)]
    probe: Option<String>,
    edits: Vec<Edit>,
    operator: Option<String>,
    legacy_diagnostic: Option<String>,
    legacy_source_detector: Option<String>,
}
#[derive(Debug, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "Independent compilation, baseline, kill and restoration evidence facts"
)]
struct Receipt {
    name: String,
    compiled: bool,
    baseline_passed: bool,
    killed: bool,
    diagnostic: String,
    source_findings: Vec<String>,
    restored_green: bool,
    command: Vec<String>,
    binding: Option<BuildBinding>,
}

#[derive(Debug, Clone, Serialize)]
struct BuildBinding {
    source_sha256: String,
    executable: String,
    executable_sha256: String,
}
fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
fn bind_artifact(
    stdout: &[u8],
    source: &Path,
    target: &Path,
    test: bool,
) -> Result<BuildBinding, String> {
    let mut executables = BTreeSet::new();
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let value: serde_json::Value = json::from_slice(line)?;
        if value["reason"] == "compiler-artifact"
            && value["profile"]["test"].as_bool() == Some(test)
            && let Some(executable) = value["executable"].as_str()
        {
            executables.insert(executable.to_owned());
        }
    }
    if executables.len() != 1 {
        return Err("exactly one compiled executable required".into());
    }
    let executable = executables
        .into_iter()
        .next()
        .ok_or("compiled executable missing")?;
    let real_executable = fs::canonicalize(&executable).map_err(|e| e.to_string())?;
    let real_target = fs::canonicalize(target).map_err(|e| e.to_string())?;
    if !real_executable.starts_with(real_target) {
        return Err("compiled executable is outside the owned target".into());
    }
    Ok(BuildBinding {
        source_sha256: digest(
            &serde_json::to_vec(&snapshot_manifest(source, source)?).map_err(|e| e.to_string())?,
        ),
        executable_sha256: digest(&fs::read(&executable).map_err(|e| e.to_string())?),
        executable,
    })
}
fn verify_binding(binding: &BuildBinding, source: &Path) -> Result<(), String> {
    if binding.executable_sha256
        != digest(&fs::read(&binding.executable).map_err(|e| e.to_string())?)
        || binding.source_sha256
            != digest(
                &serde_json::to_vec(&snapshot_manifest(source, source)?)
                    .map_err(|e| e.to_string())?,
            )
    {
        return Err("source or compiled executable identity changed".into());
    }
    Ok(())
}
fn libtest_counts(output: &str) -> Option<(usize, usize, usize)> {
    let summaries = output
        .lines()
        .filter(|line| line.starts_with("test result: "))
        .collect::<Vec<_>>();
    if summaries.len() != 1 {
        return None;
    }
    let fields = summaries[0]
        .split_once(". ")?
        .1
        .split("; ")
        .collect::<Vec<_>>();
    Some((
        fields.first()?.strip_suffix(" passed")?.parse().ok()?,
        fields.get(1)?.strip_suffix(" failed")?.parse().ok()?,
        fields.get(2)?.strip_suffix(" ignored")?.parse().ok()?,
    ))
}
fn intended_assertion(case: &Mutation, success: bool, output: &str) -> bool {
    let Some(test) = case.unit_test.as_deref() else {
        return false;
    };
    let Some(marker) = case
        .failure_contains
        .as_deref()
        .filter(|marker| marker.starts_with("PGOKF_ASSERT:"))
    else {
        return false;
    };
    !success
        && libtest_counts(output) == Some((0, 1, 0))
        && output
            .lines()
            .filter(|line| *line == format!("test {test} ... FAILED"))
            .count()
            == 1
        && output.contains(&format!("thread '{test}'"))
        && output.contains("panicked at")
        && output.contains(marker)
        && output
            .lines()
            .filter(|line| line.starts_with("test ") && line.contains(" ... "))
            .count()
            == 1
}
fn source_policy_only(case: &Mutation) -> bool {
    case.unit_test.is_none()
        && case.legacy_diagnostic.is_none()
        && case.legacy_source_detector.is_some()
}
fn baseline_assertions(selected: &BTreeSet<&str>, success: bool, output: &str) -> bool {
    success
        && libtest_counts(output).is_some_and(|(passed, failed, ignored)| {
            passed > 0
                && passed
                    == output
                        .lines()
                        .filter(|line| line.starts_with("test ") && line.ends_with(" ... ok"))
                        .count()
                && failed == 0
                && ignored == 0
        })
        && selected.iter().all(|test| {
            output
                .lines()
                .filter(|line| *line == format!("test {test} ... ok"))
                .count()
                == 1
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ReceiptFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}
impl ReceiptFileIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Result<Self, String> {
        if !metadata.file_type().is_file() {
            return Err("infrastructure receipt is not a regular file".into());
        }
        #[cfg(unix)]
        {
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Err("infrastructure receipt identity requires Unix".into())
        }
    }
    fn from_path(path: &Path) -> Result<Self, String> {
        let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        Self::from_metadata(&metadata)
    }
    fn matches(&self, path: &Path) -> Result<bool, String> {
        let actual = Self::from_path(path)?;
        #[cfg(unix)]
        {
            Ok(self.device == actual.device && self.inode == actual.inode)
        }
        #[cfg(not(unix))]
        {
            let _ = actual;
            Ok(false)
        }
    }
    #[cfg(unix)]
    fn device_string(self) -> String {
        self.device.to_string()
    }
    #[cfg(unix)]
    fn inode_string(self) -> String {
        self.inode.to_string()
    }
}

struct PreparedInfrastructureReceipt {
    file: fs::File,
    identity: ReceiptFileIdentity,
}
impl PreparedInfrastructureReceipt {
    fn identity(file: &fs::File) -> Result<ReceiptFileIdentity, String> {
        ReceiptFileIdentity::from_metadata(&file.metadata().map_err(|error| error.to_string())?)
    }
    fn ensure_identity(&self, path: &Path) -> Result<(), String> {
        if !self.identity.matches(path)? || Self::identity(&self.file)? != self.identity {
            return Err("infrastructure receipt identity differs".into());
        }
        Ok(())
    }
    fn authenticated_bytes(&mut self, path: &Path) -> Result<Vec<u8>, String> {
        self.authenticated_bytes_with_after_read(path, || {})
    }
    fn authenticated_bytes_with_after_read(
        &mut self,
        path: &Path,
        after_read: impl FnOnce(),
    ) -> Result<Vec<u8>, String> {
        self.ensure_identity(path)?;
        self.file
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        self.file
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        after_read();
        self.ensure_identity(path)?;
        Ok(bytes)
    }
}

fn prepare_infrastructure_receipt(
    path: &Path,
    campaign_id: &str,
    case_id: &str,
    nonce: &str,
) -> Result<PreparedInfrastructureReceipt, String> {
    let value = serde_json::json!({
        "format_version": 3,
        "state": "intent",
        "nonce": nonce,
        "campaign_id": campaign_id,
        "case_id": case_id,
    });
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    let identity = PreparedInfrastructureReceipt::identity(&file)?;
    Ok(PreparedInfrastructureReceipt { file, identity })
}

fn intended_phase(case: &Mutation) -> &'static str {
    if case.probe.as_deref() == Some("catalog-controls") {
        "catalog-controls"
    } else if case.name == "row-type dependents unchecked" {
        "exceptions"
    } else if case.name.starts_with("fixture ") {
        "manifest"
    } else if matches!(
        case.name.as_str(),
        "bypass guards" | "weaken guard ACL" | "weaken guard dependents"
    ) {
        "refusal"
    } else {
        "reconciled"
    }
}

fn intended_oracle(case: &Mutation) -> Result<&'static str, String> {
    let oracle = match case.name.as_str() {
        "omit drop semantic" => "reconciled.retired-semantic-absent",
        "omit drop hybrid" => "reconciled.retired-hybrid-absent",
        "omit drop bm25" => "reconciled.retired-bm25-absent",
        "omit helper" => "reconciled.current-helper-present",
        "omit helper owner"
        | "weaken search revoke"
        | "weaken search grant"
        | "weaken helper revoke"
        | "skip search comments"
        | "skip package comments"
        | "skip initial privileges" => "reconciled.function-metadata-equal",
        "bypass guards" | "weaken guard ACL" => "refusal.update-rejected",
        "weaken guard dependents" => "refusal.diagnostic-matched",
        "skip web comment schema"
        | "skip web comment users table"
        | "skip web comment users.name"
        | "skip web comment users.role"
        | "skip web comment users.password_hash"
        | "skip web comment users.created_at"
        | "skip web comment users.updated_at"
        | "reword web comment"
        | "third column-order drift" => "reconciled.catalog-equal-with-declared-order",
        "fixture without the column order"
        | "fixture without the sessions order"
        | "fixture without the stale comments"
        | "fixture of the rejected dev2 candidate" => "manifest.live-drift-exact",
        "undeclared users order" => "reconciled.users-declared-order",
        "row-type dependents unchecked" => "exceptions.rowtype-dependents-rejected",
        "row-type function in an edge" => "reconciled.rowtype-dependents-empty",
        "relation grant validation removed" => "catalog-controls.shared-forgery-rejected",
        "column grant validation removed" => "catalog-controls.column-grant-forgery-rejected",
        "tenant policy validation removed" => "catalog-controls.tenant-policy-forgery-rejected",
        _ => {
            return Err(format!(
                "live mutation lacks an exact semantic oracle: {}",
                case.name
            ));
        }
    };
    Ok(oracle)
}

struct LiveCommandProof<'a> {
    receipt: &'a Path,
    nonce: &'a str,
    campaign_id: &'a str,
    case_id: &'a str,
    expected_phase: &'a str,
    expected_oracle: &'a str,
}

fn live_command_killed(
    status_success: bool,
    output: &str,
    diagnostic: &str,
    receipt: &mut PreparedInfrastructureReceipt,
    expected: &LiveCommandProof<'_>,
) -> bool {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct InfrastructureReceipt {
        format_version: u32,
        state: String,
        nonce: String,
        campaign_id: String,
        case_id: String,
        startup_succeeded: bool,
        semantic_phase: Option<String>,
        semantic_oracle: Option<String>,
        semantic_error: Option<String>,
        service_running_before_cleanup: bool,
        cleanup_succeeded: bool,
    }
    let proof = receipt
        .authenticated_bytes(expected.receipt)
        .ok()
        .and_then(|bytes| json::from_slice::<InfrastructureReceipt>(&bytes).ok());
    !status_success
        && output.contains(diagnostic)
        && proof.is_some_and(|proof| {
            proof.format_version == 3
                && proof.state == "complete"
                && proof.nonce == expected.nonce
                && proof.campaign_id == expected.campaign_id
                && proof.case_id == expected.case_id
                && proof.startup_succeeded
                && proof.semantic_phase.as_deref() == Some(expected.expected_phase)
                && proof.semantic_oracle.as_deref() == Some(expected.expected_oracle)
                && proof
                    .semantic_error
                    .is_some_and(|error| error.contains(diagnostic))
                && proof.service_running_before_cleanup
                && proof.cleanup_succeeded
        })
}
fn command_record(command: &CommandSpec) -> Vec<String> {
    std::iter::once(command.program.as_os_str())
        .chain(command.args.iter().map(std::ffi::OsString::as_os_str))
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
}
fn expected_source(m: &Mutation) -> &'static [&'static str] {
    match m.name.as_str() {
        "bypass guards" | "cascade" => &["reviewed edge", "guard", "CASCADE"],
        "rewrite dev1 in place" | "rewrite dev2 in place" => &["reviewed edge", "upgrade path"],
        "widen the exception to another table"
        | "widen the runtime indexes"
        | "allow-list expansion" => &["catalog exceptions changed"],
        "fixture pin removed, fixture incomplete" => &["live drift lacks"],
        "positional insert in the product" | "positional insert, registered" => {
            &["positional INSERT"]
        }
        "select star in the product, registered" | "returning star in the product, registered" => {
            &["star or record expansion"]
        }
        "row-type function in an edge" => &["protected relation outside a named table context"],
        _ => &[],
    }
}
fn checked_path(root: &Path, name: &str) -> Result<PathBuf, String> {
    let path = Path::new(name);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("unsafe mutation path".into());
    }
    Ok(root.join(path))
}
fn copy_source(root: &Path, destination: &Path) -> Result<(), String> {
    // Preserve an owned index and immutable history for provenance tests. Shared
    // object storage avoids duplicating the repository; each source copy owns
    // its refs and metadata, so no mutation can modify the caller's checkout.
    let mut clone = CommandSpec::new("git");
    clone.args = vec![
        "clone".into(),
        "--shared".into(),
        "--no-checkout".into(),
        "--".into(),
        root.as_os_str().into(),
        destination.as_os_str().into(),
    ];
    clone.checked()?;
    let mut index = CommandSpec::new("git");
    index.cwd = Some(destination.into());
    index.args = ["reset", "--mixed", "HEAD"]
        .into_iter()
        .map(Into::into)
        .collect();
    index.checked()?;
    let mut cmd = CommandSpec::new("git");
    cmd.cwd = Some(root.into());
    cmd.args = [
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    let output = cmd.checked()?;
    for name in output.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let name = std::str::from_utf8(name).map_err(|e| e.to_string())?;
        let source = checked_path(root, name)?;
        if !source.exists() {
            continue;
        }
        if source
            .symlink_metadata()
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("source mutation refuses symlinks".into());
        }
        let target = checked_path(destination, name)?;
        fs::create_dir_all(target.parent().ok_or("parent missing")?).map_err(|e| e.to_string())?;
        fs::copy(source, target).map_err(|e| e.to_string())?;
    }
    Ok(())
}
fn document_edit(
    actual: &mut serde_json::Value,
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> Result<(), String> {
    if before == after {
        return Ok(());
    }
    if let (Some(actual), Some(before), Some(after)) = (
        actual.as_object_mut(),
        before.as_object(),
        after.as_object(),
    ) {
        for key in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
            match (before.get(key), after.get(key)) {
                (Some(old), Some(new)) if old != new => document_edit(
                    actual.get_mut(key).ok_or("mutation JSON key absent")?,
                    old,
                    new,
                )?,
                (Some(old), None) => {
                    if actual.get(key) != Some(old) {
                        return Err("mutation JSON removal anchor differs".into());
                    }
                    actual.remove(key);
                }
                (None, Some(new)) if actual.insert(key.clone(), new.clone()).is_some() => {
                    return Err("mutation JSON insertion key exists".into());
                }
                _ => (),
            }
        }
        return Ok(());
    }
    if actual != before {
        return Err("mutation JSON value anchor differs".into());
    }
    *actual = after.clone();
    Ok(())
}
fn replace_file(path: &Path, old: &str, new: &str) -> Result<(), String> {
    let text = read(path)?;
    if path
        .extension()
        .is_some_and(|extension| extension == "json")
        && let (Ok(before), Ok(after)) = (
            json::from_slice::<serde_json::Value>(old.as_bytes()),
            json::from_slice::<serde_json::Value>(new.as_bytes()),
        )
        && before.is_object()
        && after.is_object()
    {
        let mut actual = json::from_slice(text.as_bytes())?;
        document_edit(&mut actual, &before, &after)?;
        return fs::write(
            path,
            serde_json::to_vec_pretty(&actual).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string());
    }
    if old.is_empty() || text.matches(old).count() != 1 {
        return Err(format!("mutation anchor is not unique: {}", path.display()));
    }
    fs::write(path, text.replacen(old, new, 1)).map_err(|e| e.to_string())
}
fn implementation(name: &str, source: &Path) -> Result<Option<&'static str>, String> {
    let catalog = source.join("crates/pgokf-verifier/src/catalog.rs");
    let item = match name {
        "independent provider validation removed" => Some((
            &catalog,
            "crate::provider::validate(index, expected.provider, &expected.provider_context)?;",
            "let _ = (index, &expected.provider_context);",
            "catalog::tests::shared_provider_metadata_forgeries_fail",
        )),
        "tier database config ignored" => Some((
            &catalog,
            "            || !role.database_config.is_empty()",
            "            || (!role.database_config.is_empty() && false)",
            "catalog::tests::shared_database_config_fails",
        )),

        "declare any users order" => Some((
            &catalog,
            "order != fresh && !(allow_declared_order && order == declared)",
            "{ let _ = declared; order != fresh && !allow_declared_order }",
            "catalog::tests::third_order_rejected",
        )),
        "exception blind to comments" => Some((
            &catalog,
            "definition[0] = if allow_declared_order",
            "*definition.last_mut().ok_or(\"column metadata absent\")? = Value::Null;\n            definition[0] = if allow_declared_order",
            "catalog::tests::metadata_on_exception_stays_visible",
        )),
        "exception drops the declared tables" => Some((
            &catalog,
            "    let mut output = rows\n        .iter()",
            "    rows.retain(|r| r.0 != \"column\");\n    let mut output = rows\n        .iter()",
            "catalog::tests::metadata_on_exception_stays_visible",
        )),
        "runtime index kind unchecked" => Some((
            &catalog,
            "            || index.access_method != am",
            "            || (index.access_method != am && false)",
            "catalog::tests::shared_provider_metadata_forgeries_fail",
        )),
        "comparison blind to ACLs" => Some((
            &catalog,
            "list.sort_by_key(Value::to_string);",
            "list.clear();",
            "catalog::tests::metadata_on_exception_stays_visible",
        )),
        _ => None,
    };
    if let Some((path, old, new, test)) = item {
        // The ACL normalization occurs in both ordinary and default ACL paths.
        if name == "comparison blind to ACLs" {
            let text = read(path)?;
            if !text.contains(old) {
                return Err("ACL mutation anchor absent".into());
            }
            let ordinary = "if let Some(list) = value.as_array_mut() {\n                list.sort_by_key(Value::to_string);\n            } else if !value.is_null() {\n                return Err(\"ACL must be a list or null\".into());\n            }";
            let defaults = "if let Some(list) = row.2.as_array_mut() {\n                list.sort_by_key(Value::to_string);\n            } else if !row.2.is_null() {\n                return Err(\"default ACL must be a list or null\".into());\n            }";
            if text.matches(ordinary).count() != 1 || text.matches(defaults).count() != 1 {
                return Err("ACL semantic anchors missing".into());
            }
            fs::write(
                path,
                text.replace(ordinary, "*value = Value::Null;")
                    .replace(defaults, "row.2 = Value::Null;"),
            )
            .map_err(|e| e.to_string())?;
        } else {
            replace_file(path, old, new)?;
        }
        return Ok(Some(test));
    }
    if name == "argument continuity ignored" {
        replace_file(
            &source.join("crates/pgokf-verifier/src/upgrade.rs"),
            "if run.args.len() != arity",
            "if false && run.args.len() != arity",
        )?;
        return Ok(Some("upgrade::tests::argument_continuity_is_checked"));
    }
    if name == "row-type dependents unchecked" {
        let path = source.join("crates/pgokf-verifier/sql/catalog-snapshot.sql");
        replace_file(
            &path,
            " WHERE ((d.refclassid=",
            " WHERE false AND ((d.refclassid=",
        )?;
    }
    if name == "fixture pin removed, fixture incomplete" {
        replace_file(
            &source.join("crates/pgokf-verifier/src/reconciliation.rs"),
            "if keys != p.live_drift_manifest.iter().cloned().collect()",
            "if false && keys != p.live_drift_manifest.iter().cloned().collect()",
        )?;
    }
    Ok(None)
}
fn intended(m: &Mutation) -> String {
    let name = m.name.as_str();
    if name.contains("keep version") || name.contains("rewrite dev1") {
        return "exactly one current default_version".into();
    }
    if [
        "widen the exception to another table",
        "widen the runtime indexes",
        "allow-list expansion",
    ]
    .contains(&name)
    {
        return "catalog exceptions changed".into();
    }
    if name == "positional insert in the product" {
        return "positional".into();
    }
    if name.starts_with("omit drop ") {
        return "retired overload".into();
    }
    if name == "omit helper" {
        return "current helper".into();
    }
    if [
        "omit helper owner",
        "weaken search revoke",
        "weaken search grant",
        "weaken helper revoke",
        "skip search comments",
        "skip package comments",
        "skip initial privileges",
    ]
    .contains(&name)
    {
        return "reconciled function metadata drift".into();
    }
    if name.starts_with("fixture without") || name == "fixture of the rejected dev2 candidate" {
        return "live-drift fixture manifest differs".into();
    }
    if name.starts_with("skip web comment")
        || name == "reword web comment"
        || name == "third column-order drift"
    {
        return "catalog drift outside exact declared column-order exceptions".into();
    }
    if name == "undeclared users order" {
        return "declared physical order".into();
    }
    if name == "bypass guards" || name == "weaken guard ACL" {
        return "unsafe acceptance".into();
    }
    if name == "weaken guard dependents" {
        return "wrong refusal".into();
    }
    if name == "row-type dependents unchecked" {
        return "exception probe row-type".into();
    }
    if name.contains("registered") {
        return if name.contains("star") {
            "star or record expansion"
        } else {
            "positional"
        }
        .into();
    }
    m.legacy_diagnostic
        .clone()
        .unwrap_or_else(|| m.legacy_source_detector.clone().unwrap_or_default())
}
fn mutation_target(work: &Path, configured: Option<&std::ffi::OsStr>) -> Result<PathBuf, String> {
    let Some(configured) = configured else {
        return Ok(work.join("target"));
    };
    let path = PathBuf::from(configured);
    if !path.is_absolute() {
        return Err("CARGO_TARGET_DIR must be absolute for reconciliation mutations".into());
    }
    let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("CARGO_TARGET_DIR must be a pre-existing directory, not a symlink".into());
    }
    path.canonicalize().map_err(|error| error.to_string())
}

/// # Errors
/// Requires a passing baseline and successful compilation before any kill.
#[allow(
    clippy::too_many_lines,
    reason = "Campaign receipt ordering remains explicit"
)]
pub fn run(options: &Options, only: &BTreeSet<String>, results: &Path) -> Result<(), String> {
    let mutations: Vec<Mutation> =
        json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json"))?;
    if mutations.iter().any(|m| {
        m.unit_test.is_some()
            && m.failure_contains
                .as_deref()
                .is_none_or(|marker| !marker.starts_with("PGOKF_ASSERT:"))
    }) {
        return Err("intended semantic assertion marker required".into());
    }
    let selected_tests = mutations
        .iter()
        .filter(|m| only.is_empty() || only.contains(&m.name))
        .filter_map(|m| m.unit_test.as_deref())
        .collect::<BTreeSet<_>>();
    let names = mutations.iter().map(|m| &m.name).collect::<BTreeSet<_>>();
    if names.len() != mutations.len() {
        return Err("duplicate mutation identity".into());
    }
    for name in only {
        if !mutations.iter().any(|m| &m.name == name) {
            return Err(format!("unknown mutation: {name}"));
        }
    }
    if results.exists() {
        return Err("mutation results must be a new path".into());
    }
    let evidence = results.with_extension("evidence");
    let work = crate::scratch::directory("pgokf-mutations-")?;
    fs::create_dir(&evidence).map_err(|e| e.to_string())?;
    let campaign_id = uuid::Uuid::new_v4().to_string();
    let frozen = work.path().join("frozen");
    copy_source(&options.root, &frozen)?;
    crate::owned_docker::publish(
        &evidence.join("source-manifest.json"),
        &snapshot_manifest(&frozen, &frozen)?,
    )?;
    let source = work.path().join("source");
    copy_owned_tree(&frozen, &source)?;
    let mut identity = CommandSpec::new("git");
    identity.cwd = Some(options.root.clone());
    identity.args = ["rev-parse", "HEAD"].into_iter().map(Into::into).collect();
    fs::write(evidence.join("source-tip.txt"), identity.checked()?.stdout)
        .map_err(|e| e.to_string())?;

    let mut config = CommandSpec::new(&options.pg_config);
    config.args = vec!["--sharedir".into()];
    let sharedir = String::from_utf8(config.checked()?.stdout).map_err(|e| e.to_string())?;
    let input = options.extension_prefix.as_ref().map_or_else(
        || Path::new(sharedir.trim()).join("extension"),
        |prefix| prefix.join("extension"),
    );
    let installed = work.path().join("installed");
    fs::create_dir(&installed).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(&input).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_name().to_string_lossy().starts_with("pgokf") {
            if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
                return Err("extension input must be regular owned-copy files".into());
            }
            fs::copy(entry.path(), installed.join(entry.file_name())).map_err(|e| e.to_string())?;
        }
    }
    crate::owned_docker::publish(
        &evidence.join("installation-manifest.json"),
        &snapshot_manifest(&installed, &installed)?,
    )?;
    let prefix = work.path().join("prefix");
    fs::create_dir_all(prefix.join("extension")).map_err(|e| e.to_string())?;
    let target = mutation_target(work.path(), std::env::var_os("CARGO_TARGET_DIR").as_deref())?;
    let mut receipts = vec![];
    let cases = std::iter::once(None)
        .chain(
            mutations
                .iter()
                .filter(|m| only.is_empty() || only.contains(&m.name))
                .map(Some),
        )
        .chain(std::iter::once(None));
    for (number, case) in cases.enumerate() {
        // Reset only this campaign's owned scratch tree between semantic cases.
        if number > 0 {
            fs::remove_dir_all(&source).map_err(|e| e.to_string())?;
            copy_owned_tree(&frozen, &source)?;
            fs::remove_dir_all(prefix.join("extension")).map_err(|e| e.to_string())?;
            fs::create_dir(prefix.join("extension")).map_err(|e| e.to_string())?;
        }
        for entry in fs::read_dir(&installed).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if entry.file_name().to_string_lossy().starts_with("pgokf") {
                fs::copy(
                    entry.path(),
                    prefix.join("extension").join(entry.file_name()),
                )
                .map_err(|e| e.to_string())?;
            }
        }
        let is_baseline = case.is_none();
        let name = case.map_or(
            if number == 0 {
                "baseline"
            } else {
                "restored baseline"
            },
            |m| m.name.as_str(),
        );
        let case_id = format!("{number:02}:{name}");
        let service_receipts = evidence.join(format!("{number:02}-service-receipts"));
        fs::create_dir(&service_receipts).map_err(|error| error.to_string())?;
        let service_receipts = service_receipts
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let mut service_root_context = CommandSpec::new("/usr/bin/true");
        let service_binding =
            crate::upgrade::bind_service_receipts(&mut service_root_context, &service_receipts)?;
        crate::owned_docker::publish(
            &evidence.join(format!("{number:02}-identity.json")),
            &serde_json::json!({
                "campaign_id": campaign_id,
                "case_id": case_id,
                "service_receipts": service_binding,
            }),
        )?;
        let mut test = None;
        if let Some(m) = case {
            // Copies must precede removal of their original installer.
            for edit in m.edits.iter().filter(|e| e.copy_from.is_some()) {
                let from = checked_path(
                    &prefix.join("extension"),
                    edit.copy_from.as_deref().ok_or("copy source missing")?,
                )?;
                fs::copy(from, checked_path(&prefix.join("extension"), &edit.path)?)
                    .map_err(|e| e.to_string())?;
            }
            for edit in m.edits.iter().filter(|e| e.copy_from.is_none()) {
                let root = if edit.path.contains('/') {
                    source.clone()
                } else {
                    prefix.join("extension")
                };
                let path = checked_path(&root, &edit.path)?;
                if edit.delete {
                    fs::remove_file(&path).map_err(|e| e.to_string())?;
                    if !edit.path.contains('/') && edit.path.matches("--").count() == 2 {
                        let original = source.join("crates/extension/sql").join(&edit.path);
                        if original.exists() {
                            fs::remove_file(original).map_err(|e| e.to_string())?;
                        }
                    }
                } else {
                    replace_file(
                        &path,
                        edit.before.as_deref().ok_or("mutation before missing")?,
                        edit.after.as_deref().ok_or("mutation after missing")?,
                    )?;
                }
            }
            for entry in fs::read_dir(prefix.join("extension")).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let name = entry.file_name();
                let n = name.to_string_lossy();
                if n == "pgokf.control" {
                    fs::copy(entry.path(), source.join("crates/extension/pgokf.control"))
                        .map_err(|e| e.to_string())?;
                } else if n.matches("--").count() == 2 {
                    fs::copy(entry.path(), source.join("crates/extension/sql").join(name))
                        .map_err(|e| e.to_string())?;
                }
            }
            test = m.unit_test.as_deref().or(implementation(name, &source)?);
        }
        crate::owned_docker::publish(
            &evidence.join(format!("{number:02}-extension.json")),
            &snapshot_manifest(&prefix, &prefix)?,
        )?;
        let source_findings = source_policy(&source)?;
        let source_ok = case.map_or(source_findings.is_empty(), |m| {
            m.legacy_source_detector.is_none()
                || source_findings.iter().any(|finding| {
                    expected_source(m)
                        .iter()
                        .any(|expected| finding.contains(expected))
                })
        });
        // macOS copyfile may preserve an older mtime. Force the owned crate
        // source newer than every cached mutant, including the restored case.
        let library = source.join("crates/pgokf-verifier/src/lib.rs");
        fs::write(&library, fs::read(&library).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let mut build = CommandSpec::new("cargo");
        build.cwd = Some(source.clone());
        build.args = ["build", "--locked", "-p", "pgokf-verifier"]
            .into_iter()
            .map(Into::into)
            .collect();
        if test.is_some() {
            build.args = [
                "test",
                "--locked",
                "-p",
                "pgokf-verifier",
                "--lib",
                "--no-run",
            ]
            .into_iter()
            .map(Into::into)
            .collect();
        }
        build.args.push("--message-format=json".into());
        build
            .env
            .insert("CARGO_TARGET_DIR".into(), target.as_os_str().into());
        build.env.insert("CARGO_BUILD_JOBS".into(), "1".into());
        build.env.insert("CARGO_INCREMENTAL".into(), "0".into());
        build.timeout = Duration::from_mins(5);
        let compilation = build.run()?;
        fs::write(
            evidence.join(format!("{number:02}-build.log")),
            [compilation.stdout.clone(), compilation.stderr.clone()].concat(),
        )
        .map_err(|e| e.to_string())?;
        if !compilation.status.success() {
            receipts.push(Receipt {
                name: name.into(),
                compiled: false,
                baseline_passed: number > 0,
                killed: false,
                diagnostic: "compile failure is not a kill".into(),
                source_findings: source_findings.clone(),
                restored_green: false,
                command: command_record(&build),
                binding: None,
            });
            break;
        }
        let binding = bind_artifact(&compilation.stdout, &source, &target, test.is_some())?;
        crate::owned_docker::publish(
            &evidence.join(format!("{number:02}-binding.json")),
            &binding,
        )?;
        crate::owned_docker::publish(
            &evidence.join(format!("{number:02}-source.json")),
            &snapshot_manifest(&source, &source)?,
        )?;
        if is_baseline {
            let mut controls = CommandSpec::new(target.join("debug/pgokf-verifier"));
            controls.env.insert(
                "PGOKF_VERIFICATION_CAMPAIGN".into(),
                campaign_id.clone().into(),
            );
            controls
                .env
                .insert("PGOKF_VERIFICATION_CASE".into(), case_id.clone().into());
            let controls_service_receipts = service_receipts.join("controls");
            fs::create_dir(&controls_service_receipts).map_err(|error| error.to_string())?;
            let controls_service_receipts = controls_service_receipts
                .canonicalize()
                .map_err(|error| error.to_string())?;
            let controls_service_binding =
                crate::upgrade::bind_service_receipts(&mut controls, &controls_service_receipts)?;
            controls.args = vec![
                "parity".into(),
                "--source-root".into(),
                source.as_os_str().into(),
                "--pg-config".into(),
                options.pg_config.as_os_str().into(),
                "--controls-output".into(),
                evidence
                    .join(format!("{number:02}-controls.json"))
                    .into_os_string(),
            ];
            controls
                .args
                .extend(["--extension-prefix".into(), prefix.as_os_str().into()]);
            controls.timeout = Duration::from_mins(10);
            controls.durable_stdout =
                Some(evidence.join(format!("{number:02}-controls.stdout.partial")));
            controls.durable_stderr =
                Some(evidence.join(format!("{number:02}-controls.stderr.partial")));
            if let Some(provider) = &options.preload {
                controls.args.extend(["--preload".into(), provider.into()]);
            }
            crate::owned_docker::publish(
                &evidence.join(format!("{number:02}-controls-command.json")),
                &serde_json::json!({
                    "command": command_record(&controls),
                    "timeout_ms": controls.timeout.as_millis(),
                    "stdout": controls.durable_stdout.as_ref(),
                    "stderr": controls.durable_stderr.as_ref(),
                    "service_receipts": controls_service_binding,
                }),
            )?;
            let outcome = controls.run()?;
            fs::write(
                evidence.join(format!("{number:02}-controls.log")),
                [outcome.stdout, outcome.stderr].concat(),
            )
            .map_err(|e| e.to_string())?;
            if !outcome.status.success() {
                return Err("baseline catalog controls failed; no mutation credit".into());
            }
            let mut unit = build.clone();
            unit.args = [
                "test",
                "--locked",
                "-p",
                "pgokf-verifier",
                "--lib",
                "--no-run",
                "--message-format=json",
            ]
            .into_iter()
            .map(Into::into)
            .collect();
            let compiled_unit = unit.run()?;
            fs::write(
                evidence.join(format!("{number:02}-unit-build.log")),
                [compiled_unit.stdout.clone(), compiled_unit.stderr].concat(),
            )
            .map_err(|e| e.to_string())?;
            if !compiled_unit.status.success() {
                return Err("baseline unit compilation failed; no mutation credit".into());
            }
            let unit_binding = bind_artifact(&compiled_unit.stdout, &source, &target, true)?;
            crate::owned_docker::publish(
                &evidence.join(format!("{number:02}-unit-binding.json")),
                &unit_binding,
            )?;
            let mut run = CommandSpec::new(&unit_binding.executable);
            run.timeout = Duration::from_mins(5);
            let outcome = run.run()?;
            verify_binding(&unit_binding, &source)?;
            let output = format!(
                "{}\n{}",
                String::from_utf8_lossy(&outcome.stdout),
                String::from_utf8_lossy(&outcome.stderr)
            );
            fs::write(evidence.join(format!("{number:02}-unit.log")), &output)
                .map_err(|e| e.to_string())?;
            if !baseline_assertions(&selected_tests, outcome.status.success(), &output) {
                return Err(
                    "baseline/restored exact selected assertions failed; no mutation credit".into(),
                );
            }
        }
        if case.is_some_and(source_policy_only) {
            receipts.push(Receipt {
                name: name.into(),
                compiled: true,
                baseline_passed: true,
                killed: source_ok,
                diagnostic: format!(
                    "source policy rejection matching {:?}",
                    case.map(expected_source)
                ),
                source_findings,
                restored_green: false,
                command: vec![
                    "source_policy".into(),
                    source.to_string_lossy().into_owned(),
                ],
                binding: Some(binding.clone()),
            });

            continue;
        }
        let mut command = if let Some(test) = test {
            let mut cmd = CommandSpec::new(&binding.executable);
            cmd.args = [test, "--exact", "--nocapture"]
                .into_iter()
                .map(Into::into)
                .collect();
            cmd
        } else {
            let mut cmd = CommandSpec::new(&binding.executable);
            cmd.args = vec![
                "parity".into(),
                "--source-root".into(),
                source.as_os_str().into(),
                "--pg-config".into(),
                options.pg_config.as_os_str().into(),
                "--extension-prefix".into(),
                prefix.as_os_str().into(),
                "--reconciliation-only".into(),
            ];
            if let Some(case) = case {
                let mode = match case.name.as_str() {
                    "omit helper owner" => "other-superuser",
                    "bypass guards" => "refusal-symbol",
                    "weaken guard ACL" => "refusal-acl",
                    "weaken guard dependents" => "refusal-dependents",
                    "row-type dependents unchecked" => "exceptions",
                    name if name.starts_with("fixture ") => "manifest",
                    _ => "upgrade",
                };
                cmd.args[0] = "reconciliation-probe".into();
                cmd.args.insert(1, mode.into());
            }
            if case.and_then(|m| m.probe.as_deref()) == Some("catalog-controls") {
                cmd.args = vec![
                    "parity".into(),
                    "--source-root".into(),
                    source.as_os_str().into(),
                    "--pg-config".into(),
                    options.pg_config.as_os_str().into(),
                    "--controls-output".into(),
                    evidence
                        .join(format!("{number:02}-controls.json"))
                        .into_os_string(),
                ];
            }
            if case.and_then(|m| m.probe.as_deref()) == Some("catalog-controls") {
                cmd.args
                    .extend(["--extension-prefix".into(), prefix.as_os_str().into()]);
            }
            if let Some(provider) = &options.preload {
                cmd.args.extend(["--preload".into(), provider.into()]);
            }
            if let Some(operator) = case.and_then(|m| m.operator.as_ref()) {
                let path = work.path().join("operator.sql");
                fs::write(&path, operator).map_err(|e| e.to_string())?;
                cmd.args
                    .extend(["--operator-sql".into(), path.into_os_string()]);
            }
            cmd
        };
        command.env.insert(
            "PGOKF_VERIFICATION_CAMPAIGN".into(),
            campaign_id.clone().into(),
        );
        command
            .env
            .insert("PGOKF_VERIFICATION_CASE".into(), case_id.clone().into());
        command.timeout = Duration::from_mins(10);
        command.durable_stdout = Some(evidence.join(format!("{number:02}-run.stdout.partial")));
        command.durable_stderr = Some(evidence.join(format!("{number:02}-run.stderr.partial")));
        let run_service_binding = if test.is_none() {
            let run_service_receipts = service_receipts.join("run");
            fs::create_dir(&run_service_receipts).map_err(|error| error.to_string())?;
            let run_service_receipts = run_service_receipts
                .canonicalize()
                .map_err(|error| error.to_string())?;
            Some(crate::upgrade::bind_service_receipts(
                &mut command,
                &run_service_receipts,
            )?)
        } else {
            None
        };
        let infrastructure_receipt = evidence.join(format!("{number:02}-infrastructure.json"));
        let mut infrastructure = if let Some(mutation) = case.filter(|_| test.is_none()) {
            let nonce = uuid::Uuid::new_v4().to_string();
            let prepared = prepare_infrastructure_receipt(
                &infrastructure_receipt,
                &campaign_id,
                &case_id,
                &nonce,
            )?;
            command.env.insert(
                "PGOKF_VERIFICATION_CAMPAIGN".into(),
                campaign_id.clone().into(),
            );
            command
                .env
                .insert("PGOKF_VERIFICATION_CASE".into(), case_id.clone().into());
            command
                .env
                .insert("PGOKF_VERIFICATION_NONCE".into(), nonce.clone().into());
            #[cfg(unix)]
            {
                command.env.insert(
                    "PGOKF_VERIFICATION_DEVICE".into(),
                    prepared.identity.device_string().into(),
                );
                command.env.insert(
                    "PGOKF_VERIFICATION_INODE".into(),
                    prepared.identity.inode_string().into(),
                );
            }
            command.env.insert(
                "PGOKF_VERIFICATION_RECEIPT".into(),
                infrastructure_receipt.as_os_str().into(),
            );
            Some((
                prepared,
                nonce,
                intended_phase(mutation),
                intended_oracle(mutation)?,
            ))
        } else {
            None
        };
        crate::owned_docker::publish(
            &evidence.join(format!("{number:02}-run-command.json")),
            &serde_json::json!({
                "command": command_record(&command),
                "timeout_ms": command.timeout.as_millis(),
                "stdout": command.durable_stdout.as_ref(),
                "stderr": command.durable_stderr.as_ref(),
                "service_receipts": run_service_binding,
            }),
        )?;
        let outcome = command.run()?;
        verify_binding(&binding, &source)?;
        let output = format!(
            "{}\n{}",
            String::from_utf8_lossy(&outcome.stdout),
            String::from_utf8_lossy(&outcome.stderr)
        );
        fs::write(evidence.join(format!("{number:02}-run.log")), &output)
            .map_err(|e| e.to_string())?;
        let diagnostic = test.map_or_else(
            || case.map_or_else(String::new, intended),
            |_| {
                case.and_then(|m| m.failure_contains.clone())
                    .unwrap_or_default()
            },
        );
        let ok = if is_baseline {
            outcome.status.success()
        } else if test.is_some() {
            case.is_some_and(|m| intended_assertion(m, outcome.status.success(), &output))
        } else {
            let (prepared, nonce, expected_phase, expected_oracle) = infrastructure
                .as_mut()
                .ok_or("live command infrastructure receipt missing")?;
            live_command_killed(
                outcome.status.success(),
                &output,
                &diagnostic,
                prepared,
                &LiveCommandProof {
                    receipt: &infrastructure_receipt,
                    nonce: nonce.as_str(),
                    campaign_id: &campaign_id,
                    case_id: &case_id,
                    expected_phase,
                    expected_oracle,
                },
            )
        };
        let ok = ok && source_ok;
        receipts.push(Receipt {
            name: name.into(),
            compiled: true,
            baseline_passed: !is_baseline || ok,
            killed: !is_baseline && ok,
            diagnostic,
            source_findings,
            restored_green: false,
            command: command_record(&command),
            binding: Some(binding),
        });
        println!(
            "{name}: {}",
            if ok {
                "expected semantic outcome"
            } else {
                "FAILED"
            }
        );
        if !ok && is_baseline {
            break;
        }
    }
    let restored = receipts
        .last()
        .is_some_and(|r| r.name == "restored baseline" && r.baseline_passed);
    for receipt in &mut receipts {
        receipt.restored_green = restored;
    }
    crate::owned_docker::publish(results, &receipts)?;
    if receipts.first().is_none_or(|r| !r.baseline_passed)
        || receipts
            .iter()
            .skip(1)
            .any(|r| !r.killed && r.name != "restored baseline")
        || receipts
            .last()
            .is_none_or(|r| r.name != "restored baseline" || !r.baseline_passed)
        || receipts.len()
            != 2 + mutations
                .iter()
                .filter(|m| only.is_empty() || only.contains(&m.name))
                .count()
    {
        return Err("mutation campaign incomplete or survived; see receipts".into());
    }
    Ok(())
}
/// # Errors
/// Rejects missing arguments, unknown options, and any surviving semantic case.
pub fn cli(args: &[String]) -> Result<(), String> {
    let mut options = Options {
        root: std::env::current_dir().map_err(|e| e.to_string())?,
        pg_config: "pg_config".into(),
        preload: None,
        extension_prefix: None,
        operator_sql: None,
        reconciliation_only: true,
        docker_image: None,
        historical_install: None,
        provider_package: None,
        probe: None,
        embedding_policy: crate::catalog::EmbeddingPolicy::NotRequested,
        embedding_dimensions: None,
    };
    let mut only = BTreeSet::new();
    let mut results = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--pg-config" => options.pg_config = iter.next().ok_or("pg_config missing")?.into(),
            "--source-root" => options.root = iter.next().ok_or("root missing")?.into(),
            "--extension-prefix" => {
                options.extension_prefix =
                    Some(iter.next().ok_or("extension prefix missing")?.into());
            }
            "--preload" => options.preload = Some(iter.next().ok_or("provider missing")?.clone()),
            "--results" => results = Some(PathBuf::from(iter.next().ok_or("results missing")?)),
            "--only" => {
                only.insert(iter.next().ok_or("mutation name missing")?.clone());
            }
            _ => return Err(format!("unknown mutation option: {arg}")),
        }
    }
    run(
        &options,
        &only,
        &results.ok_or("--results NEW_PATH is required for durable mutation evidence")?,
    )
}

fn source_policy(root: &Path) -> Result<Vec<String>, String> {
    let mut findings = vec![];
    if let Err(error) =
        crate::catalog::check_exceptions(&read(root.join("tests/upgrade/catalog-exceptions.json"))?)
    {
        findings.push(error);
    }
    if let Err(error) = crate::reconciliation::check_fixture(root) {
        findings.push(error);
    }
    for name in [
        "pgokf--0.3.1--0.4.0-dev1.sql",
        "pgokf--0.4.0-dev1--0.4.0-dev2.sql",
    ] {
        if !root.join("crates/extension/sql").join(name).is_file() {
            findings.push(format!("reviewed edge removed: {name}"));
        }
    }
    if let Err(error) = crate::migration::check_repository(root, env!("CARGO_PKG_VERSION")) {
        findings.push(error);
    }
    if let Err(error) = crate::positional::scan(root) {
        findings.push(error);
    }
    if let Err(error) = crate::positional::product_statements(root) {
        findings.push(error);
    }
    Ok(findings)
}

fn copy_owned_tree(source: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target).map_err(|e| e.to_string())?;
    let mut entries = fs::read_dir(source)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_symlink() {
            return Err("owned snapshot contains a symlink".into());
        }
        let to = target.join(entry.file_name());
        if kind.is_dir() {
            copy_owned_tree(&entry.path(), &to)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), to).map_err(|e| e.to_string())?;
        } else {
            return Err("owned snapshot contains a special file".into());
        }
    }
    Ok(())
}

fn snapshot_manifest(root: &Path, directory: &Path) -> Result<BTreeSet<(String, String)>, String> {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut result = BTreeSet::new();
    for entry in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            result.extend(snapshot_manifest(root, &entry.path())?);
        } else {
            let hash = Sha256::digest(fs::read(entry.path()).map_err(|e| e.to_string())?)
                .iter()
                .fold(String::new(), |mut s, b| {
                    let _ = write!(s, "{b:02x}");
                    s
                });
            result.insert((
                entry
                    .path()
                    .strip_prefix(root)
                    .map_err(|e| e.to_string())?
                    .to_str()
                    .ok_or("non UTF8 snapshot")?
                    .into(),
                hash,
            ));
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_semantic_kills_require_authenticated_infrastructure() {
        let directory = tempfile::tempdir().unwrap();
        let receipt = directory.path().join("live-command.json");
        let campaign = "campaign-a";
        let case = "07:omit drop semantic";
        let nonce = "fresh-parent-nonce";
        let mut prepared = prepare_infrastructure_receipt(&receipt, campaign, case, nonce).unwrap();
        let output = "retired overload: expected semantic detection";
        let mut classify = |success, text: &str| {
            live_command_killed(
                success,
                text,
                "retired overload",
                &mut prepared,
                &LiveCommandProof {
                    receipt: &receipt,
                    nonce,
                    campaign_id: campaign,
                    case_id: case,
                    expected_phase: "reconciled",
                    expected_oracle: "reconciled.retired-semantic-absent",
                },
            )
        };
        let valid = serde_json::json!({
            "format_version": 3,
            "state": "complete",
            "nonce": nonce,
            "campaign_id": campaign,
            "case_id": case,
            "startup_succeeded": true,
            "semantic_phase": "reconciled",
            "semantic_oracle": "reconciled.retired-semantic-absent",
            "semantic_error": "retired overload: expected semantic detection",
            "service_running_before_cleanup": true,
            "cleanup_succeeded": true,
        });
        fs::write(&receipt, serde_json::to_vec(&valid).unwrap()).unwrap();
        assert!(classify(false, output));

        let mut dependent = valid.clone();
        dependent["semantic_oracle"] = serde_json::Value::Null;
        dependent["semantic_error"] =
            "psql: exit status: 72: retired overload: injected dependent failure".into();
        fs::write(&receipt, serde_json::to_vec(&dependent).unwrap()).unwrap();
        assert!(!classify(
            false,
            "psql: exit status: 72: retired overload: injected dependent failure"
        ));

        for (label, field, replacement) in [
            ("reviewer child death", "startup_succeeded", false.into()),
            ("intent never completed", "state", "intent".into()),
            ("stale parent", "nonce", "stale".into()),
            ("wrong phase", "semantic_phase", "manifest".into()),
            ("wrong oracle", "semantic_oracle", "reconciled.other".into()),
            ("wrong semantic error", "semantic_error", "other".into()),
            (
                "service child death",
                "service_running_before_cleanup",
                false.into(),
            ),
            ("cleanup failure", "cleanup_succeeded", false.into()),
            ("wrong campaign", "campaign_id", "campaign-b".into()),
            ("wrong case", "case_id", "08:other".into()),
        ] {
            let mut forged = valid.clone();
            forged[field] = replacement;
            fs::write(&receipt, serde_json::to_vec(&forged).unwrap()).unwrap();
            assert!(!classify(false, output), "{label}");
        }
        let mut unknown = valid.clone();
        unknown["forged"] = true.into();
        fs::write(&receipt, serde_json::to_vec(&unknown).unwrap()).unwrap();
        assert!(!classify(false, output));
        let mut old = valid.clone();
        old["format_version"] = 2.into();
        fs::write(&receipt, serde_json::to_vec(&old).unwrap()).unwrap();
        assert!(!classify(false, output));
        fs::write(&receipt, serde_json::to_vec(&valid).unwrap()).unwrap();
        assert!(!classify(true, output));
        assert!(!classify(false, "unrelated setup failure"));
    }
    #[test]
    fn infrastructure_receipt_intent_rejects_collisions_and_identity_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let receipt = directory.path().join("infrastructure.json");
        fs::write(&receipt, b"forged").unwrap();
        assert!(prepare_infrastructure_receipt(&receipt, "campaign", "case", "nonce").is_err());
        fs::remove_file(&receipt).unwrap();
        let prepared =
            prepare_infrastructure_receipt(&receipt, "campaign", "case", "nonce").unwrap();
        fs::remove_file(&receipt).unwrap();
        fs::write(&receipt, b"replacement").unwrap();
        assert!(!prepared.identity.matches(&receipt).unwrap());
    }
    #[test]
    fn receipt_path_replacement_after_descriptor_read_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let receipt = directory.path().join("infrastructure.json");
        let displaced = directory.path().join("displaced.json");
        let replacement = directory.path().join("replacement.json");
        let mut prepared =
            prepare_infrastructure_receipt(&receipt, "campaign", "case", "nonce").unwrap();
        fs::write(&replacement, b"replacement").unwrap();
        let result = prepared.authenticated_bytes_with_after_read(&receipt, || {
            fs::rename(&receipt, &displaced).unwrap();
            fs::rename(&replacement, &receipt).unwrap();
        });
        crate::semantic_assert!("receipt-post-read-path-identity", result.is_err());
        assert!(!prepared.identity.matches(&receipt).unwrap());
    }
    #[test]
    fn receipt_identity_and_bytes_are_bound_to_one_parent_descriptor() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let directory = tempfile::tempdir().unwrap();
        let receipt = directory.path().join("race.json");
        let campaign = "campaign-race";
        let case = "38:widen the runtime indexes";
        let nonce = "parent-nonce";
        let mut prepared = prepare_infrastructure_receipt(&receipt, campaign, case, nonce).unwrap();
        let original = directory.path().join("original.json");
        fs::hard_link(&receipt, &original).unwrap();
        let forged = directory.path().join("forged.json");
        fs::write(
            &forged,
            serde_json::to_vec(&serde_json::json!({
                "format_version": 3,
                "state": "complete",
                "nonce": nonce,
                "campaign_id": campaign,
                "case_id": case,
                "startup_succeeded": true,
                "semantic_phase": "reconciled",
                "semantic_oracle": "reconciled.catalog-equal-with-declared-order",
                "semantic_error": "catalog exceptions changed",
                "service_running_before_cleanup": true,
                "cleanup_succeeded": true,
            }))
            .unwrap(),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let racer_stop = Arc::clone(&stop);
        let racer_receipt = receipt.clone();
        let racer_original = original.clone();
        let racer_forged = forged.clone();
        let racer_directory = directory.path().to_path_buf();
        let racer = std::thread::spawn(move || {
            let mut sequence = 0_u64;
            while !racer_stop.load(Ordering::Relaxed) {
                for source in [&racer_original, &racer_forged] {
                    let temporary = racer_directory.join(format!("swap-{sequence}.json"));
                    sequence += 1;
                    fs::hard_link(source, &temporary).unwrap();
                    fs::rename(&temporary, &racer_receipt).unwrap();
                }
            }
        });
        let accepted = (0..100_000).any(|_| {
            live_command_killed(
                false,
                "catalog exceptions changed",
                "catalog exceptions changed",
                &mut prepared,
                &LiveCommandProof {
                    receipt: &receipt,
                    nonce,
                    campaign_id: campaign,
                    case_id: case,
                    expected_phase: "reconciled",
                    expected_oracle: "reconciled.catalog-equal-with-declared-order",
                },
            )
        });
        stop.store(true, Ordering::Relaxed);
        racer.join().unwrap();
        assert!(!accepted, "replacement bytes escaped the parent descriptor");
    }
    #[test]
    fn frozen_source_preserves_owned_git_history() {
        let work = tempfile::tempdir().unwrap();
        let root = work.path().join("repo");
        fs::create_dir(&root).unwrap();
        let git = |path: &Path, args: &[&str]| {
            let mut cmd = CommandSpec::new("git");
            cmd.cwd = Some(path.into());
            cmd.args = args.iter().map(Into::into).collect();
            cmd.checked().unwrap().stdout
        };
        git(&root, &["init", "--quiet"]);
        fs::write(root.join("input"), "committed").unwrap();
        git(&root, &["add", "input"]);
        git(
            &root,
            &[
                "-c",
                "user.name=Verifier",
                "-c",
                "user.email=verifier@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let head = git(&root, &["rev-parse", "HEAD"]);
        fs::write(root.join("input"), "working bytes").unwrap();
        let frozen = work.path().join("frozen");
        copy_source(&root, &frozen).unwrap();
        let copy = work.path().join("copy");
        copy_owned_tree(&frozen, &copy).unwrap();
        assert!(copy.join(".git").is_dir());
        assert_eq!(git(&copy, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&copy, &["show", "HEAD:input"]), b"committed");
        assert_eq!(fs::read(copy.join("input")).unwrap(), b"working bytes");
        fs::write(copy.join("input"), "mutated").unwrap();
        assert_eq!(fs::read(root.join("input")).unwrap(), b"working bytes");
        assert_eq!(git(&root, &["rev-parse", "HEAD"]), head);
    }
    #[test]
    fn semantic_order_mutant_compiles_with_warnings_denied() {
        let work = tempfile::tempdir().unwrap();
        let directory = work.path().join("crates/pgokf-verifier/src");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("catalog.rs");
        fs::write(&path, r#"
    fn rejects(order: &[&str], fresh: &[&str], declared: &[&str], allow_declared_order: bool) -> bool {
        order != fresh && !(allow_declared_order && order == declared)
    }
    fn main() {
        assert!(!rejects(&["third"], &["fresh"], &["declared"], true));
        assert!(rejects(&["third"], &["fresh"], &["declared"], false));
    }
    "#).unwrap();
        implementation("declare any users order", work.path()).unwrap();
        let executable = work.path().join("order-mutant");
        let mut build = CommandSpec::new("rustc");
        build.args = vec![
            "-Dwarnings".into(),
            path.into_os_string(),
            "-o".into(),
            executable.clone().into_os_string(),
        ];
        let output = build.run().unwrap();
        assert!(
            output.status.success(),
            "PGOKF_ASSERT: semantic mutant must compile with warnings denied: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        CommandSpec::new(executable).checked().unwrap();
    }

    #[test]
    fn named_static_assertions_are_not_short_circuited_by_source_policy() {
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        for name in [
            "keep version at dev1",
            "rewrite dev1 in place",
            "keep version at dev2",
            "rewrite dev2 in place",
            "widen the exception to another table",
            "widen the runtime indexes",
            "allow-list expansion",
            "positional insert in the product",
            "positional insert, registered",
            "select star in the product, registered",
            "returning star in the product, registered",
        ] {
            let case = cases.iter().find(|case| case.name == name).unwrap();
            crate::semantic_assert!(
                "static-named-assertion-routing",
                !source_policy_only(case),
                "{name}"
            );
        }
        for name in ["cascade", "fixture pin removed, fixture incomplete"] {
            let case = cases.iter().find(|case| case.name == name).unwrap();
            crate::semantic_assert!(
                "source-policy-only-routing",
                source_policy_only(case),
                "{name}"
            );
        }
    }

    #[test]
    fn every_live_case_requires_an_exact_semantic_oracle() {
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        let live = cases
            .iter()
            .filter(|case| case.unit_test.is_none() && !source_policy_only(case))
            .collect::<Vec<_>>();
        crate::semantic_assert!("live-oracle-count", live.len() == 33, "{}", live.len());
        for case in live {
            crate::semantic_assert!(
                "live-oracle-map",
                intended_oracle(case).is_ok(),
                "{}",
                case.name
            );
        }
    }

    #[test]
    fn caller_owned_absolute_target_is_reused_for_warm_campaign() {
        let work = tempfile::tempdir().unwrap();
        let external = work.path().join("external-target");
        fs::create_dir(&external).unwrap();
        assert_eq!(
            mutation_target(work.path(), Some(external.as_os_str())).unwrap(),
            external.canonicalize().unwrap()
        );
    }

    #[test]
    fn grant_mutations_match_catalog_control_emitter_contract() {
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        for (name, oracle, diagnostic) in [
            (
                "relation grant validation removed",
                "catalog-controls.shared-forgery-rejected",
                "shared catalog forgery survived: grant option",
            ),
            (
                "column grant validation removed",
                "catalog-controls.column-grant-forgery-rejected",
                "shared catalog forgery survived: column grant",
            ),
        ] {
            let case = cases.iter().find(|case| case.name == name).unwrap();
            crate::semantic_assert!(
                "catalog-grant-oracle-map",
                intended_oracle(case) == Ok(oracle),
                "{name}"
            );
            crate::semantic_assert!(
                "catalog-grant-diagnostic-map",
                case.legacy_diagnostic.as_deref() == Some(diagnostic),
                "{name}"
            );
        }
    }

    /// Applies one explicit `before`/`after` edit to the real catalog.rs with
    /// the campaign's single-match anchor rule.
    fn mutated_catalog_text(before: &str, after: &str) -> String {
        let text = include_str!("catalog.rs");
        assert_eq!(
            text.matches(before).count(),
            1,
            "mutation anchor is not unique in catalog.rs"
        );
        text.replacen(before, after, 1)
    }

    /// Applies a fixture case's catalog.rs edits with the campaign's
    /// single-match anchor rule.
    fn fixture_catalog(case: &Mutation) -> String {
        let mut text = include_str!("catalog.rs").to_owned();
        let mut applied = 0;
        for edit in &case.edits {
            if edit.copy_from.is_some() || edit.delete {
                continue;
            }
            assert_eq!(
                edit.path, "crates/pgokf-verifier/src/catalog.rs",
                "{}",
                case.name
            );
            let before = edit.before.as_deref().unwrap();
            let after = edit.after.as_deref().unwrap();
            assert_eq!(
                text.matches(before).count(),
                1,
                "fixture anchor is not unique: {}",
                case.name
            );
            text = text.replacen(before, after, 1);
            applied += 1;
        }
        assert!(
            applied > 0,
            "grant fixture case has no catalog edit: {}",
            case.name
        );
        text
    }

    /// Compiles the mutated `validate_relation_grants` and its (possibly
    /// disabled) call site from the mutated catalog.rs against a faithful
    /// inventory-row model, replays the committed catalog-controls ordering
    /// over the two hostile grant controls, and returns the self-forged
    /// acceptance verdicts plus the first tripped control.
    #[allow(
        clippy::too_many_lines,
        reason = "The grant-control model is one explicit harness template"
    )]
    fn grant_control_trip(mutated: &str) -> (String, String, String) {
        let call = mutated
            .lines()
            .filter(|line| line.contains("validate_relation_grants(&rows"))
            .collect::<Vec<_>>();
        assert_eq!(call.len(), 1, "mutated call site is not unique");
        let start = mutated
            .find("fn validate_relation_grants(")
            .expect("mutated catalog.rs lost validate_relation_grants");
        let mut depth = 0_usize;
        let mut opened = false;
        let mut end = mutated.len();
        for (offset, byte) in mutated[start..].bytes().enumerate() {
            match byte {
                b'{' => {
                    depth += 1;
                    opened = true;
                }
                b'}' => {
                    depth -= 1;
                    if opened && depth == 0 {
                        end = start + offset + 1;
                        break;
                    }
                }
                _ => (),
            }
        }
        assert!(opened && depth == 0, "function extraction is unbalanced");
        let function = &mutated[start..end];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("grant-model.rs");
        fs::write(
            &path,
            format!(
                r#"use std::collections::BTreeSet;

#[derive(Clone, PartialEq)]
pub struct InventoryRow(pub String, pub String, pub Value);

#[derive(Clone, PartialEq)]
pub enum Value {{
    Null,
    Bool(bool),
    Str(String),
    Array(Vec<Value>),
}}

impl Value {{
    fn is_null(&self) -> bool {{
        matches!(self, Value::Null)
    }}
    fn as_str(&self) -> Option<&str> {{
        match self {{
            Value::Str(text) => Some(text),
            _ => None,
        }}
    }}
    fn as_array(&self) -> Option<&[Value]> {{
        match self {{
            Value::Array(list) => Some(list),
            _ => None,
        }}
    }}
    fn get(&self, index: usize) -> Option<&Value> {{
        match self {{
            Value::Array(list) => list.get(index),
            _ => None,
        }}
    }}
}}

{function}

struct Catalog {{
    extension_owner: String,
}}

struct Expected {{
    catalog: Catalog,
}}

#[allow(unused_variables)]
fn model(rows: Vec<InventoryRow>, expected: &Expected) -> Result<(), String> {{
    {}
    Ok(())
}}

fn relation_grant(
    relation: &str,
    grantee: &str,
    privilege: &str,
    grantable: bool,
) -> InventoryRow {{
    InventoryRow(
        "relation grant".into(),
        relation.into(),
        Value::Array(vec![
            Value::Str(relation.into()),
            Value::Str("postgres".into()),
            Value::Str(grantee.into()),
            Value::Str(privilege.into()),
            Value::Bool(grantable),
        ]),
    )
}}

const READERS: [&str; 16] = [
    "pgokf.bundles",
    "pgokf.concepts",
    "pgokf.concept_metadata",
    "pgokf.bundle_log",
    "pgokf.concept_embedding",
    "pgokf.links",
    "pgokf.skills",
    "pgokf.scripts",
    "pgokf.reference_documents",
    "pgokf.concept_history",
    "pgokf.concept_provenance",
    "pgokf.concept_verification",
    "pgokf.concept_provenance_source",
    "pgokf.current_relationships",
    "pgokf.effective_freshness",
    "pgokf.concept_source",
];

const WRITERS: [&str; 4] = [
    "pgokf_web.users",
    "pgokf_web.sessions",
    "pgokf_web.identity_providers",
    "pgokf_web.mcp_tokens",
];

fn required_grants(concepts_grantable: bool) -> Vec<InventoryRow> {{
    // The complete undrifted grant inventory validate_relation_grants
    // requires: the owner self-grant (skipped by the owner branch), the
    // sixteen reader SELECT grants, and the writer grants
    // (pgokf_web.mcp_tokens carries no UPDATE).
    let mut rows = vec![relation_grant("pgokf.concepts", "postgres", "SELECT", true)];
    for relation in READERS {{
        rows.push(relation_grant(
            relation,
            "pgokf_reader",
            "SELECT",
            relation == "pgokf.concepts" && concepts_grantable,
        ));
    }}
    for relation in WRITERS {{
        for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE"] {{
            if relation == "pgokf_web.mcp_tokens" && privilege == "UPDATE" {{
                continue;
            }}
            rows.push(relation_grant(relation, "pgokf_writer", privilege, false));
        }}
    }}
    rows
}}

fn column_row(acl: Value) -> InventoryRow {{
    // The nine column fields of tests/upgrade/inventory.sql, with the ACL
    // at index 8; the leading field values are irrelevant to grant
    // validation and carry plausible encodings.
    InventoryRow(
        "column".into(),
        "pgokf.concepts.title".into(),
        Value::Array(vec![
            Value::Str("4".into()),
            Value::Str("text".into()),
            Value::Str("true".into()),
            Value::Str("".into()),
            Value::Str("".into()),
            Value::Str("main".into()),
            Value::Str("".into()),
            Value::Str("default".into()),
            acl,
        ]),
    )
}}

fn column_grant_control() -> Vec<InventoryRow> {{
    // GRANT UPDATE(title) ON pgokf.concepts TO pgokf_reader: the hostile
    // column-ACL drift of catalog-controls index 2 (a non-empty
    // aclexplode list on the column ACL) over the undrifted grants.
    let mut rows = required_grants(false);
    rows.push(column_row(Value::Array(vec![
        Value::Str("postgres=Uw*/postgres".into()),
        Value::Str("pgokf_reader=Uw/postgres".into()),
    ])));
    rows
}}

fn grant_option_control() -> Vec<InventoryRow> {{
    // GRANT SELECT ON pgokf.concepts TO pgokf_reader WITH GRANT OPTION:
    // the hostile relation-grant drift of catalog-controls index 4
    // (the concepts reader grant flips to grantable, column ACLs
    // untouched and null) over the otherwise undrifted grants.
    let mut rows = required_grants(true);
    rows.push(column_row(Value::Null));
    rows
}}

fn main() {{
    let expected = Expected {{
        catalog: Catalog {{
            extension_owner: "postgres".into(),
        }},
    }};
    // catalog-controls iterates hostile controls in a fixed order: the
    // "column grant" drift (index 2) is captured before the "grant option"
    // drift (index 4), and the tripwire fires at the FIRST control whose
    // self-forged acceptance the mutant validates.
    let controls = [
        ("column grant", column_grant_control()),
        ("grant option", grant_option_control()),
    ];
    let mut tripped = "none";
    for (label, rows) in &controls {{
        if model(rows.clone(), &expected).is_ok() {{
            tripped = label;
            break;
        }}
    }}
    println!(
        "column-grant-verdict={{:?}}",
        model(controls[0].1.clone(), &expected)
    );
    println!(
        "grant-option-verdict={{:?}}",
        model(controls[1].1.clone(), &expected)
    );
    println!("first-tripped={{tripped}}");
}}
"#,
                call[0].trim(),
            ),
        )
        .unwrap();
        let executable = directory.path().join("grant-model");
        let mut build = CommandSpec::new("rustc");
        build.args = vec![
            "-Dwarnings".into(),
            path.into_os_string(),
            "-o".into(),
            executable.clone().into_os_string(),
        ];
        let output = build.run().unwrap();
        assert!(
            output.status.success(),
            "PGOKF_ASSERT:grant-mutant-compiles:{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let run = CommandSpec::new(executable).checked().unwrap();
        let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
        let field = |prefix: &str| {
            stdout
                .lines()
                .find_map(|line| line.strip_prefix(prefix))
                .unwrap_or_else(|| panic!("grant model printed no {prefix}: {stdout}"))
                .to_owned()
        };
        (
            field("column-grant-verdict="),
            field("grant-option-verdict="),
            field("first-tripped="),
        )
    }

    #[test]
    fn grant_mutations_trip_their_mapped_control_first() {
        // Live stage 3 at 2842fe8d (receipt
        // pgokf-040-recon-live-2842fe8d-20261109, relation-campaign
        // 01-infrastructure.json) proved the relation-grant mutation must not
        // subsume the column-ACL check: disabling the whole call let the
        // earlier-ordered "column grant" hostile control (index 2) trip before
        // the mapped "grant option" control (index 4). This unit model replays
        // that mechanism deterministically: the fixture edit is applied to the
        // real catalog.rs, the mutated validate_relation_grants and its call
        // site are compiled against faithful inventory rows, and the committed
        // catalog-controls ordering decides which control trips first.
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        let case = |name: &str| {
            cases
                .iter()
                .find(|case| case.name == name)
                .unwrap_or_else(|| panic!("mutation missing: {name}"))
        };
        // The relation mutation removes only relation-grant validation: the
        // column-ACL check stays live, so the index-2 control stays rejected
        // and the mapped index-4 control trips first.
        let (column, grant_option, trip) =
            grant_control_trip(&fixture_catalog(case("relation grant validation removed")));
        crate::semantic_assert!(
            "grant-mutation-relation-oracle-reachable",
            trip == "grant option",
            "relation mutation tripped at {trip}"
        );
        crate::semantic_assert!(
            "grant-mutation-column-check-live",
            column.starts_with("Err(\"unsafe column grant: pgokf.concepts.title\")"),
            "column control verdict: {column}"
        );
        crate::semantic_assert!(
            "grant-mutation-forgery-accepted",
            grant_option == "Ok(())",
            "grant option control verdict: {grant_option}"
        );
        // The sibling column case keeps its own mapping: its committed edit
        // is already surgical on the column-ACL condition, so the index-2
        // drift validates and the relation check stays live.
        let (column, grant_option, trip) =
            grant_control_trip(&fixture_catalog(case("column grant validation removed")));
        crate::semantic_assert!(
            "grant-mutation-column-oracle-reachable",
            trip == "column grant" && column == "Ok(())",
            "column mutation tripped at {trip}: {column} / {grant_option}"
        );
        crate::semantic_assert!(
            "grant-mutation-relation-check-live",
            grant_option.starts_with("Err(\"relation grantor or grant option is unsafe\")"),
            "grant option control verdict: {grant_option}"
        );
        // A whole-function removal (the pre-fix fixture form, receipted live
        // at 2842fe8d as column-grant-forgery-rejected) stays DETECTED: it
        // fails closed through the earlier column-grant control, so the
        // real-world removal scenario keeps an oracle.
        let (_, _, trip) = grant_control_trip(&mutated_catalog_text(
            "        validate_relation_grants(&rows, &expected.catalog.extension_owner)?;",
            "        if false { validate_relation_grants(&rows, &expected.catalog.extension_owner)?; }",
        ));
        crate::semantic_assert!(
            "grant-mutation-removal-fail-closed",
            trip == "column grant",
            "whole-function removal tripped at {trip}"
        );
    }

    #[test]
    fn exact_named_assertion_accounting() {
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        let case = cases
            .iter()
            .find(|case| case.name == "tier database config ignored")
            .unwrap();
        let test = case.unit_test.as_deref().unwrap();
        let marker = case.failure_contains.as_deref().unwrap();
        let good = format!(
            "test {test} ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 124 filtered out; finished in 0.01s\nthread '{test}' panicked at catalog.rs:1:1:\n{marker}\n"
        );
        crate::semantic_assert!(
            "mutation-accounting",
            intended_assertion(case, false, &good)
        );
        for bad in [
            good.replace("0 passed", "10 passed"),
            good.replace("1 failed", "11 failed"),
            good.replace("0 ignored", "1 ignored"),
            good.replace(marker, "unrelated setup panic"),
            good.replace(&format!("test {test} ... FAILED"), "test wrong ... FAILED"),
            format!("{good}\ntest result: FAILED. 0 passed; 1 failed; 0 ignored;"),
            good.replace("panicked at", "error"),
        ] {
            crate::semantic_assert!(
                "mutation-accounting",
                !intended_assertion(case, false, &bad)
            );
        }
        crate::semantic_assert!(
            "mutation-accounting",
            !intended_assertion(case, true, &good)
        );
        let passed = format!(
            "test {test} ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 124 filtered out; finished in 0.01s"
        );
        let selected = BTreeSet::from([test]);
        crate::semantic_assert!(
            "mutation-accounting",
            !baseline_assertions(&selected, true, &passed.replace(test, "wrong_test")),
            "selected-name"
        );
        crate::semantic_assert!(
            "mutation-accounting",
            baseline_assertions(&selected, true, &passed)
        );
        crate::semantic_assert!(
            "mutation-accounting",
            !baseline_assertions(
                &selected,
                true,
                &passed.replace(&format!("test {test} ... ok"), "")
            )
        );
        crate::semantic_assert!(
            "mutation-accounting",
            !baseline_assertions(&selected, true, &passed.replace("1 passed", "0 passed"))
        );
    }
    #[test]
    fn executable_and_source_identity_are_bound() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("lib.rs"), "original").unwrap();
        let executable = target.join("test-bin");
        fs::write(&executable, "compiled").unwrap();
        let artifact=serde_json::to_vec(&serde_json::json!({"reason":"compiler-artifact","profile":{"test":true},"executable":executable})).unwrap();
        let binding = bind_artifact(&artifact, &source, &target, true).unwrap();
        verify_binding(&binding, &source).unwrap();
        fs::write(&executable, "replaced").unwrap();
        crate::semantic_assert!(
            "executable-binding",
            verify_binding(&binding, &source).is_err()
        );
        fs::write(&executable, "compiled").unwrap();
        fs::write(source.join("lib.rs"), "changed").unwrap();
        crate::semantic_assert!("source-binding", verify_binding(&binding, &source).is_err());
        let forged=serde_json::to_vec(&serde_json::json!({"reason":"compiler-artifact","profile":{"test":true},"executable":source.join("lib.rs")})).unwrap();
        assert!(bind_artifact(&forged, &source, &target, true).is_err());
    }
    #[test]
    fn every_legacy_source_detector_has_an_intended_reason() {
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        for case in cases
            .iter()
            .filter(|case| case.legacy_source_detector.is_some())
        {
            assert!(!expected_source(case).is_empty(), "{}", case.name);
            assert!(
                !expected_source(case)
                    .iter()
                    .any(|expected| "unrelated syntax failure".contains(expected))
            );
        }
    }
    #[test]
    fn structural_mutations_preserve_updated_prose() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("exceptions.json");
        fs::write(
            &path,
            r#"{"comment":"new documentation","rule":{"allowed":[1,2]}}"#,
        )
        .unwrap();
        replace_file(
            &path,
            r#"{"comment":"old documentation","rule":{"allowed":[1,2]}}"#,
            r#"{"comment":"old documentation","rule":{"allowed":[1,2,3]}}"#,
        )
        .unwrap();
        let actual: serde_json::Value = json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            actual,
            serde_json::json!({"comment":"new documentation","rule":{"allowed":[1,2,3]}})
        );
        assert!(
            replace_file(
                &path,
                r#"{"rule":{"allowed":[1,2]}}"#,
                r#"{"rule":{"allowed":[]}}"#
            )
            .is_err()
        );
    }

    /// Fixture edit-path spelling → location of the pristine bytes, relative
    /// to this crate's manifest directory. Generated extension artifacts
    /// (`pgokf--0.4.0-dev{1,2,3}.sql`) exist only inside campaign
    /// installations and stay unmapped.
    ///
    /// The bytes are READ AT RUNTIME, never `include_str!`-embedded. The
    /// live campaign proved why: `rewrite dev1 in place` (slot 17) and
    /// `rewrite dev2 in place` delete reviewed upgrade edges from the
    /// mutated tree before its test build, and an `include_str!` of a
    /// deleted file failed that build outright — `compile failure is not a
    /// kill`, the slot survived, and the campaign aborted before slot 18
    /// could run at all. Runtime reads are absence-tolerant at compile
    /// time by construction, while the guard below still fails closed
    /// when a TRACKED file is unreadable in the pristine tree it audits.
    const TRACKED_REPO_FILES: &[(&str, &str)] = &[
        ("crates/pgokf-verifier/src/catalog.rs", "src/catalog.rs"),
        ("crates/pgokf-verifier/src/upgrade.rs", "src/upgrade.rs"),
        ("crates/pgokf-verifier/src/provider.rs", "src/provider.rs"),
        (
            "crates/pgokf-verifier/src/historical.rs",
            "src/historical.rs",
        ),
        (
            "crates/pgokf-verifier/src/reconciliation.rs",
            "src/reconciliation.rs",
        ),
        (
            "crates/pgokf-verifier/src/reconciliation_mutations.rs",
            "src/reconciliation_mutations.rs",
        ),
        (
            "crates/pgokf-verifier/sql/catalog-snapshot.sql",
            "sql/catalog-snapshot.sql",
        ),
        (
            "tests/upgrade/live-drift.sql",
            "../../tests/upgrade/live-drift.sql",
        ),
        (
            "tests/upgrade/catalog-exceptions.json",
            "../../tests/upgrade/catalog-exceptions.json",
        ),
        (
            "tests/upgrade/web-identity-statements.json",
            "../../tests/upgrade/web-identity-statements.json",
        ),
        (
            "crates/pgokf-web/src/user_store.rs",
            "../pgokf-web/src/user_store.rs",
        ),
        (
            "crates/pgokf-web/src/session_store.rs",
            "../pgokf-web/src/session_store.rs",
        ),
        ("crates/pgokf-web/src/main.rs", "../pgokf-web/src/main.rs"),
        ("pgokf.control", "../extension/pgokf.control"),
        (
            "pgokf--0.3.1--0.4.0-dev1.sql",
            "../extension/sql/pgokf--0.3.1--0.4.0-dev1.sql",
        ),
        (
            "pgokf--0.4.0-dev1--0.4.0-dev2.sql",
            "../extension/sql/pgokf--0.4.0-dev1--0.4.0-dev2.sql",
        ),
        (
            "pgokf--0.4.0-dev2--0.4.0-dev3.sql",
            "../extension/sql/pgokf--0.4.0-dev2--0.4.0-dev3.sql",
        ),
        ("pgokf--0.5.0.sql", "../extension/sql/pgokf--0.5.0.sql"),
    ];

    /// The repo file a fixture edit path mutates, if it is a tracked
    /// repo-owned file rather than a generated installation artifact.
    fn tracked_repo_file(path: &str) -> Option<&'static str> {
        TRACKED_REPO_FILES
            .iter()
            .find_map(|&(key, relative)| (key == path).then_some(relative))
    }

    /// Pristine bytes of a tracked repo file, read at audit time. `None`
    /// only for paths outside the mapping; a tracked file that cannot be
    /// read is a broken tree the caller must fail on, never a generated
    /// artifact.
    fn pristine_bytes(path: &str) -> Option<String> {
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(tracked_repo_file(path)?))
            .ok()
    }

    /// Structural no-op guard for the live-found defect class (complete
    /// campaign `0adc7d1d-9f14-4b25-b56f-6e0f35f863f1`, case 41): every
    /// mutation case must land a real patch through one of the campaign's
    /// three channels — fixture edits that apply against the pristine repo
    /// bytes with the campaign's unique-anchor rule, a live SQL
    /// `operator`, or the `implementation()` fallback landing bytes. A
    /// case with no channel builds and runs the unmutated source, its
    /// mapped test passes and `killed:false` is deterministic, so the
    /// campaign can never close at zero failures.
    #[test]
    fn no_mutation_case_is_silently_patchless() {
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        for case in &cases {
            let work = tempfile::tempdir().unwrap();
            let mut landed = false;
            for edit in &case.edits {
                if let Some(source) = edit.copy_from.as_deref() {
                    // The campaign copies the source installer over the
                    // target inside the extension prefix: distinct names
                    // patch the prefix, and fs::copy fails closed when the
                    // source installer is absent.
                    assert_ne!(source, edit.path, "{}", case.name);
                    landed = true;
                    continue;
                }
                if edit.delete {
                    // A prefix deletion removes committed installer bytes
                    // (fs::remove_file fails closed when absent).
                    landed = true;
                    continue;
                }
                let before = edit.before.as_deref().unwrap();
                let after = edit.after.as_deref().unwrap();
                assert!(!before.is_empty(), "{}", case.name);
                assert_ne!(before, after, "{}", case.name);
                if tracked_repo_file(&edit.path).is_none() {
                    assert!(
                        !edit.path.contains('/'),
                        "untracked repo edit target: {}",
                        edit.path
                    );
                    // A generated extension artifact exists only inside
                    // campaign installations; a stale anchor fails the
                    // campaign closed through replace_file, so the edit
                    // cannot silently no-op.
                    landed = true;
                    continue;
                }
                // Absence tolerance is a COMPILE-time property (the mutated
                // tree builds without the file); at guard runtime a tracked
                // file that cannot be read is a broken audited tree, so
                // fail closed instead of downgrading the edit to the
                // generated-artifact channel above.
                let bytes = pristine_bytes(&edit.path)
                    .unwrap_or_else(|| panic!("tracked repo file is absent: {}", edit.path));
                let path = work.path().join(&edit.path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, &bytes).unwrap();
                replace_file(&path, before, after).unwrap();
                assert_ne!(
                    fs::read(&path).unwrap(),
                    bytes.as_bytes(),
                    "{}: edit applies but patches nothing",
                    case.name
                );
                landed = true;
            }
            if case.edits.is_empty() {
                if case.operator.is_some() {
                    // Live SQL mutation channel, forged in the database.
                    continue;
                }
                // run() evaluates the implementation() fallback eagerly for
                // every case, so empty fixture edits fall back to it; the
                // fallback must land bytes on the seeded pristine tree.
                let seeded = [
                    "crates/pgokf-verifier/src/catalog.rs",
                    "crates/pgokf-verifier/src/upgrade.rs",
                    "crates/pgokf-verifier/src/reconciliation.rs",
                    "crates/pgokf-verifier/sql/catalog-snapshot.sql",
                ];
                for name in seeded {
                    let path = work.path().join(name);
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, pristine_bytes(name).unwrap()).unwrap();
                }
                implementation(&case.name, work.path()).unwrap();
                landed = seeded.iter().any(|name| {
                    fs::read(work.path().join(name)).unwrap().as_slice()
                        != pristine_bytes(name).unwrap().as_bytes()
                });
            }
            crate::semantic_assert!("mutation-case-lands-a-patch", landed, "{}", case.name);
        }
    }

    /// Mapping audit: every edit target across all 126 fixture cases (134
    /// edit operations) must resolve through `TRACKED_REPO_FILES` or be one
    /// of the declared generated installation artifacts, and every mapping
    /// entry must be justified by the fixture corpus or the
    /// `implementation()` fallback channel. The symmetric difference — the
    /// mapping diff — must be exactly zero.
    #[test]
    fn mapping_diff_against_the_fixture_corpus_is_zero() {
        const GENERATED_INSTALLATION_ARTIFACTS: &[&str] = &[
            "pgokf--0.4.0-dev1.sql",
            "pgokf--0.4.0-dev2.sql",
            "pgokf--0.4.0-dev3.sql",
        ];
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        let mut universe = BTreeSet::from([
            "crates/pgokf-verifier/src/catalog.rs",
            "crates/pgokf-verifier/src/upgrade.rs",
            "crates/pgokf-verifier/src/reconciliation.rs",
            "crates/pgokf-verifier/sql/catalog-snapshot.sql",
        ]);
        for case in &cases {
            for edit in &case.edits {
                universe.insert(edit.path.as_str());
            }
        }
        let mut diff = vec![];
        for path in &universe {
            if tracked_repo_file(path).is_none() && !GENERATED_INSTALLATION_ARTIFACTS.contains(path)
            {
                diff.push(format!("edit target missing from the mapping: {path}"));
            }
        }
        for (path, _) in TRACKED_REPO_FILES {
            if !universe.contains(path) {
                diff.push(format!(
                    "mapping entry justified by no fixture edit: {path}"
                ));
            }
        }
        assert_eq!(
            TRACKED_REPO_FILES
                .iter()
                .map(|(path, _)| *path)
                .collect::<BTreeSet<_>>()
                .len(),
            TRACKED_REPO_FILES.len(),
            "duplicate mapping key"
        );
        assert!(
            diff.is_empty(),
            "mapping diff must be zero:\n{}",
            diff.join("\n")
        );
    }

    /// Absence-tolerance audit: scans every compile-time file reference
    /// (`include_str!`/`include_bytes!`) across the verifier's compiled lib
    /// surface and proves none of them names a file the campaign's delete
    /// channel can remove from the mutated tree. The corpus really
    /// exercises the channel — both reviewed dev-cycle upgrade edges are
    /// deleted by `rewrite dev1 in place` / `rewrite dev2 in place` — so a
    /// reintroduced compile-time dependency on a deletable file (the
    /// `include_str!` regression that aborted the live campaign at slot 17
    /// with `compile failure is not a kill`) fails here instead.
    #[test]
    fn compile_time_references_survive_fixture_deletions() {
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        // Repo-tree files the campaign's delete channel removes, mirroring
        // run()'s exact semantics: '/'-relative edits delete the source
        // file; extension-prefix deletions remove the repo original only
        // for upgrade-edge names (two `--` separators).
        let mut deleted = BTreeSet::new();
        for case in &cases {
            for edit in case.edits.iter().filter(|edit| edit.delete) {
                if edit.path.contains('/') {
                    deleted.insert(edit.path.clone());
                } else if edit.path.matches("--").count() == 2 {
                    deleted.insert(format!("crates/extension/sql/{}", edit.path));
                }
            }
        }
        assert_eq!(
            deleted,
            BTreeSet::from([
                "crates/extension/sql/pgokf--0.4.0-dev1--0.4.0-dev2.sql".to_owned(),
                "crates/extension/sql/pgokf--0.4.0-dev2--0.4.0-dev3.sql".to_owned(),
            ]),
            "the corpus's delete channel changed; re-audit the reference scan"
        );
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo = normalize_path(&manifest.join("../.."));
        let mut references = vec![];
        collect_compile_time_references(&manifest.join("src"), &mut references);
        assert!(!references.is_empty(), "the reference scan found nothing");
        let mut offenders = vec![];
        for (file, argument) in &references {
            let resolved = normalize_path(&file.with_file_name(argument));
            if let Ok(relative) = resolved.strip_prefix(&repo)
                && deleted.contains(&relative.to_string_lossy().into_owned())
            {
                offenders.push(format!(
                    "{} embeds fixture-deletable {}",
                    file.display(),
                    argument
                ));
            }
        }
        assert!(
            offenders.is_empty(),
            "compile-time reference to a fixture-deletable file:\n{}",
            offenders.join("\n")
        );
    }

    /// Collects every literal `include_str!("…")`/`include_bytes!("…")`
    /// argument in the `.rs` files under `directory`. Non-literal forms
    /// (`concat!`/`env!` builds) and string-embedded source fixtures are
    /// skipped by requiring a quote immediately after the macro paren.
    fn collect_compile_time_references(directory: &Path, out: &mut Vec<(PathBuf, String)>) {
        let mut entries = fs::read_dir(directory)
            .unwrap_or_else(|error| {
                panic!(
                    "reference scan cannot read {}: {error}",
                    directory.display()
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                collect_compile_time_references(&path, out);
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "rs") {
                continue;
            }
            let text = fs::read_to_string(&path).unwrap();
            for needle in ["include_str!(", "include_bytes!("] {
                let mut offset = 0;
                while let Some(found) = text[offset..].find(needle) {
                    if let Some(rest) = text[offset + found + needle.len()..]
                        .trim_start()
                        .strip_prefix('"')
                        && let Some(end) = rest.find('"')
                    {
                        out.push((path.clone(), rest[..end].to_owned()));
                    }
                    offset += found + needle.len();
                }
            }
        }
    }

    /// Lexically resolves `..` and `.` components without touching the
    /// filesystem, so scanner resolutions compare against repo-relative
    /// spellings.
    fn normalize_path(path: &Path) -> PathBuf {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    normalized.pop();
                }
                std::path::Component::CurDir => {}
                other => normalized.push(other),
            }
        }
        normalized
    }

    /// Extracts one `fn …` from catalog.rs text by brace matching (the
    /// grant-model extraction rule).
    fn extract_catalog_function(text: &str, signature: &str) -> String {
        let start = text
            .find(signature)
            .unwrap_or_else(|| panic!("mutated catalog.rs lost {signature}"));
        let mut depth = 0_usize;
        let mut opened = false;
        let mut end = text.len();
        for (offset, byte) in text[start..].bytes().enumerate() {
            match byte {
                b'{' => {
                    depth += 1;
                    opened = true;
                }
                b'}' => {
                    depth -= 1;
                    if opened && depth == 0 {
                        end = start + offset + 1;
                        break;
                    }
                }
                _ => (),
            }
        }
        assert!(opened && depth == 0, "function extraction is unbalanced");
        text[start..end].to_owned()
    }

    /// Unit-level control for the `comparison always equal` case (complete
    /// campaign `0adc7d1d-9f14-4b25-b56f-6e0f35f863f1`, receipted
    /// `killed:false`): reproduces the campaign's mutation of catalog.rs
    /// (fixture edits, then the `implementation()` fallback `run()` evaluates
    /// eagerly for every case) and proves the mutant flips the mapped
    /// assertion `compare_inventory(&fresh, &bad, true).is_err()`
    /// (catalog.rs:1250-1254) to false, so the unit test panics with its
    /// committed marker and the campaign credits the kill. The fallback
    /// shape inherited from 2842fe8d disabled the equality branch
    /// (`if false && …`), which made every comparison report *inequality*
    /// instead: the mapped `.is_err()` assertions stayed true, the unit
    /// test passed and the case could never be killed. This control fails
    /// on that shape and on any future no-op or wrong-direction edit.
    #[allow(
        clippy::too_many_lines,
        reason = "The comparison model is one explicit harness template"
    )]
    #[test]
    fn comparison_always_equal_flips_its_mapped_assertion() {
        let cases: Vec<Mutation> =
            json::from_slice(include_bytes!("../fixtures/reconciliation-mutations.json")).unwrap();
        let case = cases
            .iter()
            .find(|case| case.name == "comparison always equal")
            .unwrap();
        crate::semantic_assert!(
            "comparison-always-equal-mapping",
            case.unit_test.as_deref()
                == Some("catalog::tests::metadata_on_exception_stays_visible")
                && case.failure_contains.as_deref()
                    == Some(
                        "PGOKF_ASSERT:metadata_on_exception_stays_visible:compare_inventory(&fresh, &bad, true).is_err()"
                    ),
            "mapped unit test or marker changed"
        );
        // Campaign-faithful mutation of catalog.rs.
        let mutation_root = tempfile::tempdir().unwrap();
        let source_directory = mutation_root.path().join("crates/pgokf-verifier/src");
        fs::create_dir_all(&source_directory).unwrap();
        let path = source_directory.join("catalog.rs");
        fs::write(&path, include_str!("catalog.rs")).unwrap();
        for edit in &case.edits {
            assert_eq!(
                edit.path, "crates/pgokf-verifier/src/catalog.rs",
                "{}",
                case.name
            );
            replace_file(
                &path,
                edit.before.as_deref().unwrap(),
                edit.after.as_deref().unwrap(),
            )
            .unwrap();
        }
        implementation(&case.name, mutation_root.path()).unwrap();
        let mutated = fs::read_to_string(&path).unwrap();
        crate::semantic_assert!(
            "comparison-always-equal-lands-a-patch",
            mutated != include_str!("catalog.rs"),
            "case 41 applies no mutation — a silent no-op case"
        );
        let pristine = include_str!("catalog.rs");
        let pristine_functions = format!(
            "{}\n{}",
            extract_catalog_function(pristine, "fn inventory_difference("),
            extract_catalog_function(pristine, "pub fn compare_inventory(")
        );
        let mutated_functions = format!(
            "{}\n{}",
            extract_catalog_function(&mutated, "fn inventory_difference("),
            extract_catalog_function(&mutated, "pub fn compare_inventory(")
        );
        let harness = |functions: &str, checks: &str| {
            format!(
                r#"use std::collections::BTreeSet;

// Model stubs: the canonical form is per-line text, so equal inventories
// canonicalize equal and the mapped fresh/bad pair (one column type
// forged text -> varchar, catalog.rs:1248-1254) canonicalizes different —
// the reject/accept semantics the committed unit test proves.
fn canonical_inventory(text: &str, _allow_declared_order: bool) -> Result<Vec<String>, String> {{
    Ok(text.lines().map(str::to_string).collect())
}}
fn differing_keys(expected: &str, actual: &str) -> Result<BTreeSet<(String, String)>, String> {{
    Ok(expected
        .lines()
        .zip(actual.lines())
        .filter(|(left, right)| left != right)
        .map(|(left, right)| (left.to_string(), right.to_string()))
        .collect())
}}

{functions}

fn main() {{
    let fresh = "column pgokf.concepts.title text";
    let bad = fresh.replace("text", "varchar");
{checks}
}}
"#
            )
        };
        let pristine_checks = r"
    // Model faithfulness: the unmutated comparison rejects the drift and
    // accepts an identical pair, mirroring the committed baseline.
    assert!(compare_inventory(&fresh, &bad, true).is_err());
    assert!(compare_inventory(&fresh, &fresh, true).is_ok());
";
        let mutant_checks = r#"
    // The mapped assertion (catalog.rs:1250-1254) is
    // compare_inventory(&fresh, &bad, true).is_err(); the mutant must
    // report equality so that assertion flips false and the test is
    // killed.
    match compare_inventory(&fresh, &bad, true) {
        Ok(()) => (),
        Err(report) => panic!(
            "PGOKF_ASSERT:comparison-always-equal-mapped-assertion: mutated \
             comparison still reports {:?} for the drifting pair — \
             the mapped is_err() assertion stays true, the unit test passes \
             and the campaign credits killed:false (the receipted live defect)",
            report
        ),
    }
    assert!(compare_inventory(&fresh, &fresh, true).is_ok());
"#;
        let models = tempfile::tempdir().unwrap();
        let build = |source: &str, stem: &str| {
            let file = models.path().join(format!("{stem}.rs"));
            fs::write(&file, source).unwrap();
            let executable = models.path().join(format!("{stem}-model"));
            let mut build = CommandSpec::new("rustc");
            build.args = vec![
                "-Dwarnings".into(),
                file.into_os_string(),
                "-o".into(),
                executable.clone().into_os_string(),
            ];
            let output = build.run().unwrap();
            assert!(
                output.status.success(),
                "PGOKF_ASSERT:comparison-model-compiles-with-warnings-denied: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            executable
        };
        CommandSpec::new(build(
            &harness(&pristine_functions, pristine_checks),
            "comparison-pristine",
        ))
        .checked()
        .unwrap();
        CommandSpec::new(build(
            &harness(&mutated_functions, mutant_checks),
            "comparison-mutant",
        ))
        .checked()
        .unwrap();
    }
}
