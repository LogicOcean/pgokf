// SPDX-License-Identifier: AGPL-3.0-only
//! Semantic substitution campaign over isolated source and extension prefixes.
use crate::{
    json,
    process::CommandSpec,
    upgrade::{Options, read},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
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
            "order != fresh && !allow_declared_order",
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
        "comparison always equal" => Some((
            &catalog,
            "    if canonical_inventory(expected, allow_declared_order)?",
            "    if false && canonical_inventory(expected, allow_declared_order)?",
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
    let target = work.path().join("target");
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
            if let Some(provider) = &options.preload {
                controls.args.extend(["--preload".into(), provider.into()]);
            }
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
        if case.is_some_and(|m| m.legacy_diagnostic.is_none() && m.legacy_source_detector.is_some())
        {
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
        command.timeout = Duration::from_mins(10);
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
            !outcome.status.success() && output.contains(&diagnostic)
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
}
