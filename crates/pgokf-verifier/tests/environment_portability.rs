//! Process-isolated controls: never mutate the test runner's environment.
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn child(scratch: &Path, evidence: &Path, mode: &str) -> std::process::Output {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "environment_worker", "--nocapture"])
        .env("TMPDIR", scratch)
        .env("PGOKF_ENV_CONTROL", mode)
        .env("PGOKF_ENV_EVIDENCE", evidence);
    if mode == "docker-refusal" {
        // Refusal controls must never reach a container engine, even on regression.
        command.env("PATH", "");
    }
    command.output().unwrap()
}

#[test]
fn caller_scratch_and_failure_cleanup() {
    let task = tempfile::tempdir().unwrap();
    let scratch = task.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    fs::write(task.path().join("sentinel"), b"untouched").unwrap();
    let evidence = task.path().join("evidence");
    let result = child(&scratch, &evidence, "package");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stdout)
    );
    let ownership: serde_json::Value =
        serde_json::from_slice(&fs::read(evidence.join("ownership.json")).unwrap()).unwrap();
    let directory = Path::new(ownership["directory"].as_str().unwrap());
    assert_eq!(
        directory.parent().unwrap(),
        scratch.canonicalize().unwrap(),
        "PGOKF_ASSERT: caller scratch ignored"
    );
    assert!(
        directory.file_name().unwrap().as_encoded_bytes().len() <= 7,
        "PGOKF_ASSERT: native socket basename budget"
    );
    assert!(!directory.exists(), "PGOKF_ASSERT: scratch cleanup omitted");

    let cleanup: serde_json::Value =
        serde_json::from_slice(&fs::read(evidence.join("cleanup.json")).unwrap()).unwrap();
    assert_eq!(cleanup["directory_removed"], true);
    assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
    assert_eq!(
        fs::read(task.path().join("sentinel")).unwrap(),
        b"untouched"
    );
}

#[cfg(unix)]
#[test]
fn redirected_scratch_is_refused() {
    let task = tempfile::tempdir().unwrap();
    let outside = task.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel"), b"untouched").unwrap();
    let scratch = task.path().join("redirect");
    std::os::unix::fs::symlink(&outside, &scratch).unwrap();
    for (index, redirected) in [
        scratch.clone(),
        scratch.join("."),
        PathBuf::from(format!("{}/", scratch.display())),
        outside.join("../outside"),
        PathBuf::from("relative-scratch"),
        outside.join("missing"),
    ]
    .iter()
    .enumerate()
    {
        let evidence = task.path().join(format!("evidence-{index}"));
        let result = child(redirected, &evidence, "package");
        assert!(result.status.success());
        assert!(
            !evidence.join("ownership.json").exists(),
            "PGOKF_ASSERT: redirected scratch accepted: {}",
            redirected.display()
        );
        let result = child(redirected, &evidence, "docker-refusal");
        assert!(result.status.success());
        assert_eq!(
            fs::read_dir(&outside).unwrap().count(),
            1,
            "PGOKF_ASSERT: receipt escaped refused scratch"
        );
    }
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"untouched");
}

#[test]
fn environment_worker() {
    let Ok(mode) = std::env::var("PGOKF_ENV_CONTROL") else {
        return;
    };
    let evidence = PathBuf::from(std::env::var_os("PGOKF_ENV_EVIDENCE").unwrap());
    if mode == "docker-refusal" {
        let options = pgokf_verifier::upgrade::Options {
            root: root(),
            pg_config: "unavailable".into(),
            preload: None,
            extension_prefix: None,
            operator_sql: None,
            reconciliation_only: false,
            docker_image: Some("unavailable".into()),
            historical_install: None,
            provider_package: None,
            probe: None,
            embedding_policy: pgokf_verifier::catalog::EmbeddingPolicy::NotRequested,
            embedding_dimensions: None,
        };
        assert!(pgokf_verifier::upgrade::Cluster::start(&options).is_err());
    } else if mode == "package" {
        let error = pgokf_verifier::artifact_checks::deb_smoke(
            &evidence.join("missing-binaries"),
            "invalid",
            &evidence,
        )
        .unwrap_err();
        println!("expected failure: {error}");
    } else {
        let source = root().join("scripts/test-homebrew-environment.rb");
        let script = fs::read_to_string(source).unwrap();
        let isolated = evidence.join("source");
        fs::create_dir_all(isolated.join("scripts")).unwrap();
        // Record the real Formulary result before executing all original cases.
        let observation = serde_json::to_string(&evidence.join("loaded-path")).unwrap();
        let script = script.replace("formula = Formulary.factory(ARGV.fetch(0))", &format!("formula = Formulary.factory(ARGV.fetch(0))\nFile.write({observation}, formula.path.realpath.to_s)"));
        fs::write(
            isolated.join("scripts/test-homebrew-environment.rb"),
            script,
        )
        .unwrap();
        let result = pgokf_verifier::release_runtime::homebrew_policy(
            &isolated,
            &root().join("packaging/homebrew/pgokf.rb"),
        );
        println!("{result:?}");
        assert!(
            result.is_ok(),
            "PGOKF_ASSERT: isolated real Formula API failed"
        );
    }
}

#[cfg(unix)]
fn inventory(path: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, entries: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            let bytes = if kind.is_symlink() {
                fs::read_link(&path)
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
            } else if kind.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            entries.insert(path.strip_prefix(root).unwrap().to_path_buf(), bytes);
            if kind.is_dir() {
                visit(root, &path, entries);
            }
        }
    }
    let mut entries = std::collections::BTreeMap::new();
    visit(path, path, &mut entries);
    entries
}

#[cfg(target_os = "macos")]
#[test]
fn real_homebrew_uses_owned_formula_without_live_tap_writes() {
    let task = tempfile::tempdir().unwrap();
    let scratch = task.path().join("scratch");
    fs::create_dir(&scratch).unwrap();
    let repo = Command::new("brew").arg("--repository").output().unwrap();
    assert!(repo.status.success());
    let taps = Path::new(String::from_utf8(repo.stdout).unwrap().trim()).join("Library/Taps");
    let before = inventory(&taps);
    // A kernel-enforced write denial catches transient entries too, including
    // entries removed before the final byte/name inventory.
    let profile = format!(
        "(version 1)(allow default)(deny file-write* (subpath {}))",
        serde_json::to_string(taps.to_str().unwrap()).unwrap()
    );
    let output = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "environment_worker", "--nocapture"])
        .env("TMPDIR", &scratch)
        .env("PGOKF_ENV_CONTROL", "homebrew")
        .env("PGOKF_ENV_EVIDENCE", task.path())
        .output()
        .unwrap();
    assert_eq!(
        inventory(&taps),
        before,
        "PGOKF_ASSERT: live tap inventory changed"
    );
    assert!(
        output.status.success(),
        "PGOKF_ASSERT: live tap write or API failure\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    println!("{}", String::from_utf8_lossy(&output.stdout));
    println!(
        "live tap inventory: {} entries, byte/name equality verified",
        before.len()
    );
    let loaded = PathBuf::from(
        fs::read_to_string(task.path().join("loaded-path"))
            .expect("PGOKF_ASSERT: real Formula API bypassed"),
    );
    assert!(
        loaded.starts_with(scratch.canonicalize().unwrap()),
        "PGOKF_ASSERT: formula outside owned scratch"
    );
    assert!(!loaded.exists(), "PGOKF_ASSERT: formula cleanup omitted");
    assert_eq!(fs::read_dir(scratch).unwrap().count(), 0);
}

#[test]
fn verifier_has_no_literal_host_temporary_root() {
    struct Roots(Vec<String>);
    impl<'ast> syn::visit::Visit<'ast> for Roots {
        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            if call.method == "tempdir_in"
                && let Some(syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(root),
                    ..
                })) = call.args.first()
            {
                self.0.push(root.value());
            }
            syn::visit::visit_expr_method_call(self, call);
        }
    }
    fn scan(path: &Path) {
        use syn::visit::Visit as _;
        for entry in fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                scan(&path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let mut roots = Roots(Vec::new());
                roots.visit_file(&syn::parse_file(&fs::read_to_string(&path).unwrap()).unwrap());
                assert!(
                    roots.0.is_empty(),
                    "PGOKF_ASSERT: literal host scratch root: {} {:?}",
                    path.display(),
                    roots.0
                );
            }
        }
    }
    scan(&root().join("crates/pgokf-verifier/src"));
}

#[cfg(unix)]
fn output_requires_valid_scratch(package: bool) {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let task = tempfile::tempdir().unwrap();
    let outside = task.path().join("outside");
    let binaries = task.path().join("bin");
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&binaries).unwrap();
    fs::write(outside.join("sentinel"), b"outside\0\xff").unwrap();
    let alias = task.path().join("redirect");
    symlink(&outside, &alias).unwrap();
    let reached = task.path().join("executable-reached");
    for name in [
        "initdb",
        "postgres",
        "pg_ctl",
        "psql",
        "pg_config",
        "docker",
    ] {
        let path = binaries.join(name);
        fs::write(
            &path,
            b"#!/bin/sh\nprintf reached >> \"$REFUSAL_EXECUTED\"\nexit 97\n",
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    for (scratch, reason) in [
        (
            alias.clone(),
            "temporary root must be a directory, not a symlink",
        ),
        (
            alias.join("."),
            "temporary root must be a directory, not a symlink",
        ),
        (task.path().join("missing"), "No such file or directory"),
    ] {
        // Exercise both output through the rejected alias and an independent
        // caller path. Existing evidence must survive even an invalid root.
        for parent in [&alias, task.path()] {
            for existing in [false, true] {
                let output = parent.join("result.json");
                let evidence = if package {
                    output.clone()
                } else {
                    output.with_extension("evidence")
                };
                if existing {
                    fs::create_dir(&evidence).unwrap();
                    fs::create_dir(evidence.join("nested")).unwrap();
                    fs::write(evidence.join("nested/receipt"), b"caller\0\xff").unwrap();
                }
                let before = inventory(task.path());
                let mut command = Command::new(env!("CARGO_BIN_EXE_pgokf-verifier"));
                if package {
                    command
                        .arg("deb-smoke")
                        .arg(&binaries)
                        .arg("invalid")
                        .arg(&output);
                } else {
                    command
                        .args(["reconciliation-mutations", "--results"])
                        .arg(&output)
                        .arg("--pg-config")
                        .arg(binaries.join("pg_config"));
                }
                let result = command
                    .env("TMPDIR", &scratch)
                    .env("PATH", &binaries)
                    .env("REFUSAL_EXECUTED", &reached)
                    .output()
                    .unwrap();
                let diagnostic = String::from_utf8_lossy(&result.stderr);
                assert!(
                    !result.status.success() && diagnostic.contains(reason),
                    "PGOKF_ASSERT: intended scratch refusal: {diagnostic}"
                );
                assert!(
                    !reached.exists(),
                    "PGOKF_ASSERT: PG/Docker reached before validation"
                );
                assert_eq!(
                    inventory(task.path()),
                    before,
                    "PGOKF_ASSERT: no output before scratch validation"
                );
                if existing {
                    fs::remove_dir_all(&evidence).unwrap();
                } else {
                    assert!(
                        !output.exists() && !evidence.exists(),
                        "PGOKF_ASSERT: no output before scratch validation"
                    );
                }
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn package_output_requires_valid_scratch() {
    output_requires_valid_scratch(true);
}

#[cfg(unix)]
#[test]
fn reconciliation_output_requires_valid_scratch() {
    output_requires_valid_scratch(false);
}
