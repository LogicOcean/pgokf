//! Reproducible implementation substitution campaigns in an owned isolated clone.
//! Compilation failures, missing assertions and timeouts never count as kills.
use crate::process::CommandSpec;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    path::{Component, Path},
    time::Duration,
};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Campaign {
    pub mutations: Vec<Mutation>,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub name: String,
    pub path: String,
    pub before: String,
    pub after: String,
    /// Exact libtest name; a zero-test execution is never accepted.
    pub test: String,
    pub target: Target,
    pub failure_contains: String,
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Target {
    Lib,
    Integration(String),
}
#[derive(Debug, Serialize)]
pub struct ResultRow {
    pub name: String,
    pub outcome: String,
}
fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| e.to_string())?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())?;
    fs::File::open(path.parent().ok_or("no evidence parent")?)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}
fn valid_relative(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}
fn contains_test(tokens: proc_macro2::TokenStream) -> bool {
    tokens.into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(name) => name == "test",
        proc_macro2::TokenTree::Group(group) => contains_test(group.stream()),
        _ => false,
    })
}

/// Track actual enclosing items, including nested modules and associated items.
/// Conditional test attributes are conservatively protected even under negation.
/// Opaque item macros are protected without claiming to expand them; ordinary
/// expression macros remain part of their surrounding production implementation.
fn overlaps_test(source: &str, changed: std::ops::Range<usize>) -> Result<bool, String> {
    use syn::{spanned::Spanned, visit::Visit};
    struct Tests {
        stack: Vec<std::ops::Range<usize>>,
        protected: Vec<std::ops::Range<usize>>,
    }
    impl<'ast> Visit<'ast> for Tests {
        fn visit_item(&mut self, item: &'ast syn::Item) {
            self.stack.push(item.span().byte_range());
            if matches!(item, syn::Item::Macro(_)) {
                self.protected.push(item.span().byte_range());
            }
            syn::visit::visit_item(self, item);
            self.stack.pop();
        }
        fn visit_impl_item(&mut self, item: &'ast syn::ImplItem) {
            self.stack.push(item.span().byte_range());
            if matches!(item, syn::ImplItem::Macro(_)) {
                self.protected.push(item.span().byte_range());
            }
            syn::visit::visit_impl_item(self, item);
            self.stack.pop();
        }
        fn visit_trait_item(&mut self, item: &'ast syn::TraitItem) {
            self.stack.push(item.span().byte_range());
            if matches!(item, syn::TraitItem::Macro(_)) {
                self.protected.push(item.span().byte_range());
            }
            syn::visit::visit_trait_item(self, item);
            self.stack.pop();
        }
        fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
            let test = attribute
                .path()
                .segments
                .last()
                .is_some_and(|s| s.ident == "test")
                || matches!(&attribute.meta, syn::Meta::List(list)
                    if (list.path.is_ident("cfg") || list.path.is_ident("cfg_attr"))
                        && contains_test(list.tokens.clone()));
            if test {
                self.protected.push(self.stack.last().unwrap().clone());
            }
        }
    }
    let file = syn::parse_file(source).map_err(|e| format!("invalid mutation source: {e}"))?;
    let mut visitor = Tests {
        stack: std::iter::once(0..source.len()).collect(),
        protected: Vec::new(),
    };
    visitor.visit_file(&file);
    Ok(visitor
        .protected
        .iter()
        .any(|range| changed.start < range.end && range.start < changed.end))
}
fn cargo(root: &Path, mutation: &Mutation, compile: bool) -> CommandSpec {
    let mut command = CommandSpec::new("cargo");
    command.cwd = Some(root.into());
    command.timeout = Duration::from_mins(5);
    command.env.insert(
        "CARGO_BUILD_JOBS".into(),
        if std::env::var("CARGO_BUILD_JOBS").as_deref() == Ok("1") {
            "1".into()
        } else {
            "2".into()
        },
    );
    command.env.insert("CARGO_INCREMENTAL".into(), "0".into());
    command.args = ["test", "--locked", "-p", "pgokf-verifier"]
        .map(Into::into)
        .into();
    match &mutation.target {
        Target::Lib => command.args.push("--lib".into()),
        Target::Integration(name) => command.args.extend(["--test".into(), name.into()]),
    }
    if compile {
        command.args.push("--no-run".into());
    } else {
        command.args.extend([
            mutation.test.clone().into(),
            "--".into(),
            "--exact".into(),
            "--nocapture".into(),
        ]);
    }
    command
}
fn execute(
    command: &CommandSpec,
    evidence: &Path,
    execute_tool: &mut impl FnMut(&CommandSpec) -> Result<std::process::Output, String>,
) -> Result<(bool, String), String> {
    let result = execute_tool(command);
    let (ok, text) = match result {
        Ok(output) => (
            output.status.success(),
            format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        ),
        Err(error) => {
            write(evidence, error.as_bytes())?;
            return Err(error);
        }
    };
    write(evidence, text.as_bytes())?;
    Ok((ok, text))
}
fn require_build(ok: bool, reason: &str) -> Result<(), String> {
    if !ok {
        return Err(reason.into());
    }
    Ok(())
}
fn require_pass(ok: bool, output: &str) -> Result<(), String> {
    if !ok
        || !output.lines().any(|line| {
            line == "1 passed; 0 failed" || line.starts_with("test result: ok. 1 passed; 0 failed;")
        })
    {
        return Err("baseline-test-failed-or-missing".into());
    }
    Ok(())
}
fn require_kill(m: &Mutation, ok: bool, output: &str) -> Result<(), String> {
    if ok {
        return Err("survived".into());
    }
    if !output.lines().any(|line| {
        line == "0 passed; 1 failed" || line.starts_with("test result: FAILED. 0 passed; 1 failed;")
    }) || !output.contains(&format!("test {} ... FAILED", m.test))
        || !output.contains("panicked at")
        || !output.contains(&m.failure_contains)
    {
        return Err("failed-without-intended-assertion".into());
    }
    Ok(())
}

/// Run exact Rust tests before, during and after each single implementation edit.
/// Evidence must be a new external directory. Source and tests are cloned from HEAD.
/// # Errors
/// Any survivor, setup/compile failure, missing test or cleanup failure fails the campaign.
#[allow(clippy::too_many_lines)]
pub fn run(root: &Path, campaign: &Campaign, evidence: &Path) -> Result<(), String> {
    run_with(root, campaign, evidence, &mut CommandSpec::run)
}
#[allow(clippy::too_many_lines)]
fn run_with(
    root: &Path,
    campaign: &Campaign,
    evidence: &Path,
    execute_tool: &mut impl FnMut(&CommandSpec) -> Result<std::process::Output, String>,
) -> Result<(), String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let parent = evidence
        .parent()
        .ok_or("evidence parent required")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if parent.starts_with(&root) {
        return Err("mutation evidence must be outside source".into());
    }
    if campaign.mutations.is_empty() {
        return Err("empty mutation campaign".into());
    }
    let mut names = BTreeSet::new();
    for m in &campaign.mutations {
        if !valid_relative(&m.path)
            || !m.path.starts_with("crates/pgokf-verifier/src/")
            || Path::new(&m.path).extension().is_none_or(|ext| ext != "rs")
            || m.before.is_empty()
            || m.before == m.after
            || m.test.is_empty()
            || m.failure_contains.is_empty()
            || !m
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
            || m.name.is_empty()
            || !names.insert(&m.name)
        {
            return Err(format!("invalid or duplicate mutation: {}", m.name));
        }
    }
    fs::create_dir(evidence).map_err(|e| e.to_string())?;
    let temporary = tempfile::Builder::new()
        .prefix("pgokf-mutation-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let source = temporary.path().join("source");
    write(
        &evidence.join("ownership.json"),
        &serde_json::to_vec(&serde_json::json!({"owned_directory":temporary.path(),"source":root}))
            .map_err(|e| e.to_string())?,
    )?;
    let campaign_result = (|| -> Result<Vec<ResultRow>, String> {
        let mut clone = CommandSpec::new("git");
        clone.args = vec![
            "clone".into(),
            "--quiet".into(),
            "--shared".into(),
            "--".into(),
            root.into_os_string(),
            source.clone().into_os_string(),
        ];
        clone.checked()?;
        let mut identity = CommandSpec::new("git");
        identity.cwd = Some(source.clone());
        identity.args = ["rev-parse", "HEAD"].map(Into::into).into();
        write(
            &evidence.join("source-head.txt"),
            &identity.checked()?.stdout,
        )?;
        write(
            &evidence.join("campaign.json"),
            &serde_json::to_vec_pretty(campaign).map_err(|e| e.to_string())?,
        )?;

        let mut results = Vec::new();
        for m in &campaign.mutations {
            let path = source.join(&m.path);
            let canonical = path.canonicalize().map_err(|e| e.to_string())?;
            if !canonical.starts_with(source.canonicalize().map_err(|e| e.to_string())?)
                || !path
                    .symlink_metadata()
                    .map_err(|e| e.to_string())?
                    .file_type()
                    .is_file()
            {
                return Err("mutation source must be an owned regular file".into());
            }
            let original = fs::read_to_string(&path).map_err(|e| e.to_string())?;

            let verdict = (|| {
                if original.matches(&m.before).count() != 1 {
                    return Err("substitution must match exactly once".into());
                }
                let start = original.find(&m.before).ok_or("missing mutation anchor")?;
                if overlaps_test(&original, start..start + m.before.len())? {
                    return Err("cannot mutate test implementation".into());
                }
                let (ok, _) = execute(
                    &cargo(&source, m, true),
                    &evidence.join(format!("{}-baseline-build.log", m.name)),
                    execute_tool,
                )?;
                require_build(ok, "baseline-build-failed")?;
                let (ok, output) = execute(
                    &cargo(&source, m, false),
                    &evidence.join(format!("{}-baseline.log", m.name)),
                    execute_tool,
                )?;
                require_pass(ok, &output)?;
                fs::write(&path, original.replacen(&m.before, &m.after, 1))
                    .map_err(|e| e.to_string())?;
                let (ok, _) = execute(
                    &cargo(&source, m, true),
                    &evidence.join(format!("{}-build.log", m.name)),
                    execute_tool,
                )?;
                require_build(ok, "compile-failed-not-a-kill")?;
                let (ok, output) = execute(
                    &cargo(&source, m, false),
                    &evidence.join(format!("{}-test.log", m.name)),
                    execute_tool,
                )?;
                require_kill(m, ok, &output)?;
                Ok(())
            })();
            fs::write(&path, &original).map_err(|e| e.to_string())?;
            let restore = execute(
                &cargo(&source, m, false),
                &evidence.join(format!("{}-restored.log", m.name)),
                execute_tool,
            );
            let outcome = match (verdict, restore) {
                (Ok(()), Ok((ok, output))) if require_pass(ok, &output).is_ok() => "killed".into(),
                (Err(error), _) => error,
                _ => "restored-baseline-failed".into(),
            };
            results.push(ResultRow {
                name: m.name.clone(),
                outcome,
            });
        }
        write(
            &evidence.join("results.json"),
            &serde_json::to_vec_pretty(&results).map_err(|e| e.to_string())?,
        )?;
        Ok(results)
    })();
    let cleanup = temporary.close().map_err(|e| e.to_string());
    write(
        &evidence.join("cleanup.json"),
        &serde_json::to_vec(&serde_json::json!({
            "owned_directory_removed":cleanup.is_ok(),
            "campaign_error":campaign_result.as_ref().err(),
            "cleanup_error":cleanup.as_ref().err(),
        }))
        .map_err(|e| e.to_string())?,
    )?;
    cleanup?;
    let results = campaign_result?;

    if results.iter().all(|r| r.outcome == "killed") {
        Ok(())
    } else {
        Err("mutation campaign has non-kills; see results.json".into())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opaque_items_and_qualified_tests_are_protected() {
        for source in [
            "#[tokio::test] async fn fixture() { let marker = 1; }",
            "macro_rules! tests { () => { #[test] fn fixture() { let marker = 1; } }; }",
            "generate_tests! { marker }",
            "impl Thing { generate_tests! { marker } }",
            "trait Thing { generate_tests! { marker } }",
        ] {
            let start = source.find("marker").unwrap();
            assert!(overlaps_test(source, start..start + 6).unwrap(), "{source}");
        }
        let source =
            "fn production() { let marker = matches!(1, 1); let text = format!(\"{}\", marker); }";
        let start = source.find("marker").unwrap();
        assert!(!overlaps_test(source, start..start + 6).unwrap());
    }
    #[test]
    fn production_after_tests_is_mutable_but_test_bodies_are_not() {
        let source = "#[cfg(test)] mod tests { fn fixture() { let x = 1; } }\nfn production() { let y = 2; }\n";
        let production = source.find("let y").unwrap();
        assert!(!overlaps_test(source, production..production + 5).unwrap());
        let fixture = source.find("let x").unwrap();
        assert!(overlaps_test(source, fixture..fixture + 5).unwrap());
        assert!(overlaps_test(source, fixture..production + 5).unwrap());
    }
    #[test]
    fn test_boundary_ignores_attribute_names_in_implementation_strings() {
        let implementation = "fn implementation() { let markers = [\"#[cfg(test)]\"]; }\n";
        assert!(!overlaps_test(implementation, 0..implementation.len()).unwrap());
        for attribute in [
            "#[cfg(test)]",
            "#[cfg(all(unix, test))]",
            "#[cfg_attr(unix, test)]",
            "#[test]",
        ] {
            let source = format!(
                "{implementation} mod nested {{ {attribute} fn fixture() {{}} fn live() {{}} }}"
            );
            let fixture = source.find("fn fixture").unwrap();
            assert!(overlaps_test(&source, fixture..fixture + 2).unwrap());
            assert!(!overlaps_test(&source, 0..implementation.len()).unwrap());
            let live = source.find("fn live").unwrap();
            assert!(!overlaps_test(&source, live..live + 2).unwrap());
        }
        let associated = "impl Thing { #[cfg(test)] fn fixture() {} fn live() {} }";
        let fixture = associated.find("fn fixture").unwrap();
        assert!(overlaps_test(associated, fixture..fixture + 2).unwrap());
        let live = associated.find("fn live").unwrap();
        assert!(!overlaps_test(associated, live..live + 2).unwrap());
        let associated = "trait Thing { #[cfg(test)] fn fixture(); fn live(); }";
        let fixture = associated.find("fn fixture").unwrap();
        assert!(overlaps_test(associated, fixture..fixture + 2).unwrap());
        let live = associated.find("fn live").unwrap();
        assert!(!overlaps_test(associated, live..live + 2).unwrap());
        assert!(overlaps_test("#![cfg(test)] fn fixture() {}", 20..25).unwrap());
        assert!(overlaps_test("invalid Rust", 0..2).is_err());
    }
    fn case() -> Mutation {
        Mutation {
            name: "probe".into(),
            path: "crates/pgokf-verifier/src/source.rs".into(),
            before: "before".into(),
            after: "after".into(),
            test: "selected".into(),
            target: Target::Lib,
            failure_contains: "specific invariant".into(),
        }
    }
    #[test]
    fn compilation_failure_is_not_semantic_credit() {
        assert!(require_build(true, "compile").is_ok());
        assert!(require_build(false, "compile").is_err());
    }
    #[test]
    fn baseline_and_restoration_require_one_successful_test() {
        assert!(require_pass(true, "1 passed; 0 failed").is_ok());
        for (status, text) in [
            (false, "1 passed; 0 failed"),
            (true, "0 passed; 0 failed"),
            (true, "2 passed; 0 failed"),
            (true, "11 passed; 0 failed"),
            (true, "test result: ok. 11 passed; 0 failed; 0 ignored;"),
            (true, "1 passed; 1 failed"),
        ] {
            assert!(
                require_pass(status, text).is_err(),
                "status={status}: {text}"
            );
        }
    }
    #[test]
    fn semantic_kill_requires_every_independent_evidence_field() {
        let text =
            "test selected ... FAILED\n0 passed; 1 failed\npanicked at source: specific invariant";
        let case = case();
        assert!(require_kill(&case, false, text).is_ok());
        assert!(require_kill(&case, true, text).is_err());
        for (before, after) in [
            ("0 passed; 1 failed", "0 passed; 0 failed"),
            ("0 passed; 1 failed", "10 passed; 1 failed"),
            ("selected", "other"),
            ("panicked at", "process exited"),
            ("specific invariant", "unrelated panic"),
        ] {
            assert!(
                require_kill(&case, false, &text.replace(before, after)).is_err(),
                "missing {before}"
            );
        }
    }
    #[cfg(unix)]
    #[test]
    fn campaign_orders_compilation_and_assertions_and_rechecks_restoration() {
        use std::os::unix::process::ExitStatusExt;
        for fail_restore in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let reports = tempfile::tempdir().unwrap();
            let file = root.path().join("crates/pgokf-verifier/src/source.rs");
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(&file, "fn before() {}").unwrap();
            let git = |args: &[&str]| {
                let mut cmd = CommandSpec::new("git");
                cmd.cwd = Some(root.path().into());
                cmd.args = args.iter().map(Into::into).collect();
                cmd.checked().unwrap();
            };
            git(&["init", "--quiet"]);
            git(&["add", "."]);
            git(&[
                "-c",
                "user.name=Verifier",
                "-c",
                "user.email=verifier@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ]);
            let mut stage = 0;
            let outcome = run_with(
                root.path(),
                &Campaign {
                    mutations: vec![case()],
                },
                &reports.path().join("case"),
                &mut |command| {
                    let index = stage;
                    stage += 1;
                    assert_eq!(
                        command.args.iter().any(|arg| arg == "--no-run"),
                        matches!(index, 0 | 2),
                        "compile/test stage changed"
                    );
                    let source = command
                        .cwd
                        .as_ref()
                        .unwrap()
                        .join("crates/pgokf-verifier/src/source.rs");
                    assert_eq!(
                        fs::read_to_string(source).unwrap(),
                        if matches!(index, 2 | 3) {
                            "fn after() {}"
                        } else {
                            "fn before() {}"
                        },
                        "source restoration/order changed"
                    );
                    let failed = index == 3 || (index == 4 && fail_restore);
                    let text = if failed {
                        "test selected ... FAILED\n0 passed; 1 failed\npanicked at source: specific invariant"
                    } else {
                        "test selected ... ok\n1 passed; 0 failed"
                    };
                    Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(if failed { 256 } else { 0 }),
                        stdout: text.as_bytes().to_vec(),
                        stderr: Vec::new(),
                    })
                },
            );
            assert_eq!(
                stage, 5,
                "missing baseline, compilation, assertion or restored execution"
            );
            assert_eq!(
                outcome.is_ok(),
                !fail_restore,
                "failed restoration must block credit"
            );
            assert_eq!(fs::read_to_string(file).unwrap(), "fn before() {}");
        }
    }
}
