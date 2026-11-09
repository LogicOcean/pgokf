use pgokf_verifier::source;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
fn check(root: &Path) -> Result<(), String> {
    let bytes = fs::read(root.join("source-review.json")).map_err(|e| e.to_string())?;
    let trusted = pgokf_verifier::json::from_slice(&bytes)?;
    source::check_reviewed(root, &trusted)
}
fn git(root: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap()
            .success(),
        "git {args:?}"
    );
}
fn git_output(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    git(
        root.path(),
        &[
            "-c",
            "user.name=Source Guard Test",
            "-c",
            "user.email=source-guard@example.invalid",
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "fixture root",
        ],
    );
    root
}
struct CaseSensitiveFixture {
    _owner: tempfile::TempDir,
    repo: PathBuf,
    #[cfg(target_os = "macos")]
    mount: Option<PathBuf>,
}
impl CaseSensitiveFixture {
    fn path(&self) -> &Path {
        &self.repo
    }
}
#[cfg(target_os = "macos")]
impl Drop for CaseSensitiveFixture {
    fn drop(&mut self) {
        if let Some(mount) = &self.mount {
            let _ = Command::new("hdiutil")
                .args(["detach", "-quiet"])
                .arg(mount)
                .status();
        }
    }
}
fn filesystem_is_case_sensitive(root: &Path) -> bool {
    let lower = root.join("case-probe");
    let upper = root.join("CASE-PROBE");
    fs::write(&lower, "lower").unwrap();
    fs::write(&upper, "upper").unwrap();
    let sensitive = fs::read_to_string(&lower).unwrap() == "lower"
        && fs::read_to_string(&upper).unwrap() == "upper";
    fs::remove_file(lower).unwrap();
    if sensitive {
        fs::remove_file(upper).unwrap();
    }
    sensitive
}
fn case_sensitive_fixture() -> CaseSensitiveFixture {
    let owner = tempfile::tempdir().unwrap();
    if filesystem_is_case_sensitive(owner.path()) {
        let repo = owner.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        return CaseSensitiveFixture {
            _owner: owner,
            repo,
            #[cfg(target_os = "macos")]
            mount: None,
        };
    }

    #[cfg(target_os = "macos")]
    {
        let image = owner.path().join("case-sensitive.dmg");
        let mount = owner.path().join("mnt");
        fs::create_dir(&mount).unwrap();
        let created = Command::new("hdiutil")
            .args([
                "create",
                "-quiet",
                "-size",
                "32m",
                "-fs",
                "Case-sensitive APFS",
                "-volname",
                "PGOKFSourceGuard",
            ])
            .arg(&image)
            .status()
            .unwrap();
        assert!(created.success(), "create case-sensitive disk image");
        let attached = Command::new("hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&image)
            .status()
            .unwrap();
        assert!(attached.success(), "attach case-sensitive disk image");
        assert!(filesystem_is_case_sensitive(&mount));
        let repo = mount.join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet"]);
        CaseSensitiveFixture {
            _owner: owner,
            repo,
            mount: Some(mount),
        }
    }

    #[cfg(not(target_os = "macos"))]
    panic!("source guard case-collision regression requires a case-sensitive filesystem");
}
fn commit_all(root: &Path) {
    git(root, &["add", "--all"]);
    commit_index(root);
}
fn commit_index(root: &Path) {
    git(
        root,
        &[
            "-c",
            "user.name=Source Guard Test",
            "-c",
            "user.email=source-guard@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
}

#[test]
fn untracked_gitignore_cannot_hide_itself_or_sibling() {
    let root = fixture();
    fs::write(root.path().join("payload.data"), "reviewed").unwrap();
    review(root.path(), &["payload.data"]);
    commit_all(root.path());
    assert!(check(root.path()).is_ok());

    fs::create_dir(root.path().join("hidden")).unwrap();
    fs::write(root.path().join("hidden/.gitignore"), "*\n").unwrap();
    fs::write(root.path().join("hidden/evil.sh"), "python3 -c pass\n").unwrap();
    let error = check(root.path()).unwrap_err();
    assert!(
        error.contains("hidden/.gitignore: unreviewed source execution input"),
        "{error}"
    );
    assert!(
        error.contains("hidden/evil.sh: unreviewed source execution input"),
        "{error}"
    );

    fs::remove_file(root.path().join("hidden/.gitignore")).unwrap();
    let visible = check(root.path()).unwrap_err();
    assert!(visible.contains("hidden/evil.sh: unreviewed source execution input"));
    fs::remove_dir_all(root.path().join("hidden")).unwrap();
    assert!(check(root.path()).is_ok());
}

#[test]
fn repository_ignore_case_cannot_hide_case_colliding_source() {
    let root = case_sensitive_fixture();
    fs::write(root.path().join("README.md"), "reviewed").unwrap();
    review(root.path(), &["README.md"]);
    commit_all(root.path());
    assert!(check(root.path()).is_ok());

    fs::write(root.path().join("README.MD"), "unreviewed").unwrap();
    git(root.path(), &["config", "core.ignoreCase", "true"]);
    let error = check(root.path()).unwrap_err();
    assert!(
        error.contains("README.MD: unreviewed source execution input"),
        "{error}"
    );

    git(root.path(), &["config", "core.ignoreCase", "false"]);
    let visible = check(root.path()).unwrap_err();
    assert!(visible.contains("README.MD: unreviewed source execution input"));
    fs::remove_file(root.path().join("README.MD")).unwrap();
    git(root.path(), &["config", "core.ignoreCase", "true"]);
    assert!(check(root.path()).is_ok());
}

#[test]
fn committed_head_is_required_for_source_provenance() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "--quiet"]);
    fs::write(root.path().join("payload.data"), "reviewed").unwrap();
    review(root.path(), &["payload.data"]);
    git(root.path(), &["add", "--all"]);

    let error = check(root.path()).unwrap_err();
    assert!(
        error.contains("source guard requires a committed HEAD"),
        "{error}"
    );

    commit_index(root.path());
    assert!(check(root.path()).is_ok());
}

#[test]
fn committed_ignore_rules_exclude_only_authenticated_generated_inputs() {
    let root = fixture();
    fs::write(root.path().join(".gitignore"), "generated/\n").unwrap();
    fs::write(root.path().join("payload.data"), "reviewed").unwrap();
    review(root.path(), &[".gitignore", "payload.data"]);
    commit_all(root.path());
    fs::create_dir(root.path().join("generated")).unwrap();
    fs::write(root.path().join("generated/artifact.data"), "generated").unwrap();

    assert!(check(root.path()).is_ok());
}

#[test]
fn modified_tracked_gitignore_cannot_authorize_unreviewed_source() {
    let root = fixture();
    fs::write(root.path().join(".gitignore"), "generated/\n").unwrap();
    fs::write(root.path().join("payload.data"), "reviewed").unwrap();
    review(root.path(), &[".gitignore", "payload.data"]);
    commit_all(root.path());
    assert!(check(root.path()).is_ok());

    fs::write(root.path().join(".gitignore"), "*\n").unwrap();
    fs::write(root.path().join("evil.sh"), "python3 -c pass\n").unwrap();
    let error = check(root.path()).unwrap_err();
    assert!(
        error.contains(".gitignore: unreviewed source execution input"),
        "{error}"
    );
    assert!(
        error.contains("evil.sh: unreviewed source execution input"),
        "{error}"
    );
}
#[allow(clippy::format_collect)]
fn review(root: &std::path::Path, files: &[&str]) {
    let files: BTreeMap<_, _> = files
        .iter()
        .map(|name| {
            (
                *name,
                Sha256::digest(fs::read(root.join(name)).unwrap())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>(),
            )
        })
        .collect();
    fs::write(
        root.join("source-review.json"),
        serde_json::to_vec(&serde_json::json!({"version":1,"files":files.into_iter().map(|(name,sha256)| (name,serde_json::json!({"sha256":sha256,"executable":false}))).collect::<BTreeMap<_,_>>()})).unwrap(),
    )
    .unwrap();
}
#[test]
fn computed_and_transitive_execution_requires_review() {
    let root = fixture();
    fs::write(
        root.path().join("gate.sh"),
        "#!/bin/sh\ncargo test --locked\n",
    )
    .unwrap();
    review(root.path(), &["gate.sh"]);
    assert!(check(root.path()).is_ok());
    for command in [
        "p=py; q=thon; $p$q -c pass",
        "sh payload.data",
        "eval $(cat payload.data)",
        "$(printf '\\160\\171\\164\\150\\157\\156') -c pass",
    ] {
        fs::write(root.path().join("gate.sh"), command).unwrap();
        assert!(check(root.path()).is_err(), "{command}");
    }
}
#[test]
fn new_data_wrapper_and_dependency_changes_fail_closed() {
    let root = fixture();
    fs::write(root.path().join("gate.sh"), "cargo test").unwrap();
    review(root.path(), &["gate.sh"]);
    fs::write(root.path().join("payload.data"), "p=py; q=thon; $p$q").unwrap();
    assert!(check(root.path()).is_err());
}
#[test]
fn docs_alias_cannot_launder_exemption() {
    let root = fixture();
    fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
    let name = ".github/workflows/docs.yml";
    fs::write(root.path().join(name), "jobs:\n  docs:\n    steps:\n    - name: Install MkDocs Material\n      run: pip install mkdocs-material\n      env: &injected {TOOL: python3}\n    - name: Install MkDocs Material\n      run: pip install mkdocs-material\n      <<: {env: *injected}\n").unwrap();
    review(root.path(), &[name]);
    assert!(check(root.path()).is_err());
}
#[test]
fn exact_docs_steps_are_allowed_and_changed_steps_refused() {
    let root = fixture();
    fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
    let name = ".github/workflows/docs.yml";
    for (extra, ok) in [("", true), (" pytest", false)] {
        fs::write(root.path().join(name), format!("jobs:\n  docs:\n    steps:\n    - name: Install MkDocs Material\n      run: pip install mkdocs-material{extra}\n")).unwrap();
        review(root.path(), &[name]);
        assert_eq!(check(root.path()).is_ok(), ok);
    }
}
#[test]
fn duplicate_review_keys_and_stale_entries_are_refused() {
    let root = fixture();
    fs::write(
        root.path().join("source-review.json"),
        "{\"version\":1,\"version\":1,\"files\":{}}",
    )
    .unwrap();
    assert!(check(root.path()).is_err());
    fs::write(
        root.path().join("source-review.json"),
        "{\"version\":1,\"files\":{\"missing\":\"digest\"}}",
    )
    .unwrap();
    assert!(check(root.path()).is_err());
}
#[test]
fn rehashing_hostile_commands_does_not_change_trusted_review() {
    let root = fixture();
    fs::write(root.path().join("gate.sh"), "cargo test --locked").unwrap();
    review(root.path(), &["gate.sh"]);
    let trusted = pgokf_verifier::json::from_slice(
        &fs::read(root.path().join("source-review.json")).unwrap(),
    )
    .unwrap();
    for command in [
        "p'y'thon3 -c pass",
        "p=py; q=thon; $p$q -c pass",
        "eval $(base64 -d payload.data)",
        "sh extensionless",
    ] {
        fs::write(root.path().join("gate.sh"), command).unwrap();
        review(root.path(), &["gate.sh"]);
        assert!(
            source::check_reviewed(root.path(), &trusted)
                .unwrap_err()
                .contains("differs from trusted verifier inventory")
        );
    }
}
#[test]
fn reviewed_python_files_shebangs_dependencies_and_local_actions_still_fail() {
    for (name, text) in [
        ("hidden.PY", "pass"),
        ("hidden.pyw", "pass"),
        ("hidden.pyc", "bytes"),
        ("hidden.rs", "#!/usr/bin/env python3\npass"),
        ("requirements.txt", "PyYAML==6.0.3"),
        ("tools/requirements-dev.txt", "PyYAML==6.0.3"),
        ("Cargo.toml", "[dependencies]\npytest = '1'"),
        ("Dockerfile.test", "RUN python3 -m unittest"),
        (
            "action.yml",
            "runs:\n  using: composite\n  steps:\n  - run: python3 check\n",
        ),
        ("Makefile", "test:\n\tpytest"),
    ] {
        let root = fixture();
        fs::create_dir_all(root.path().join(name).parent().unwrap()).unwrap();
        fs::write(root.path().join(name), text).unwrap();
        review(root.path(), &[name]);
        assert!(check(root.path()).is_err(), "{name}");
    }
}
#[cfg(unix)]
#[test]
fn execution_permission_changes_require_review() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture();
    let path = root.path().join("payload.data");
    fs::write(&path, "exit 0").unwrap();
    review(root.path(), &["payload.data"]);
    assert!(check(root.path()).is_ok());
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(check(root.path()).is_err());
}
#[cfg(unix)]
#[test]
fn staged_index_execution_mode_mutation_is_rejected() {
    let root = fixture();
    fs::write(root.path().join("payload.data"), "exit 0").unwrap();
    review(root.path(), &["payload.data"]);
    commit_all(root.path());
    assert!(check(root.path()).is_ok());

    git(root.path(), &["update-index", "--chmod=+x", "payload.data"]);
    assert!(
        check(root.path())
            .unwrap_err()
            .contains("Git index differs from committed HEAD")
    );
}
#[cfg(unix)]
#[test]
fn committed_tree_execution_mode_mutation_is_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    let path = root.path().join("payload.data");
    fs::write(&path, "exit 0").unwrap();
    review(root.path(), &["payload.data"]);
    commit_all(root.path());

    git(root.path(), &["update-index", "--chmod=+x", "payload.data"]);
    commit_index(root.path());
    assert_eq!(
        fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o111,
        0
    );
    assert!(
        check(root.path())
            .unwrap_err()
            .contains("payload.data: checkout mode/type differs from committed HEAD")
    );
}
#[test]
fn staged_content_cannot_replace_committed_head_behind_reviewed_checkout() {
    let root = fixture();
    let path = root.path().join("payload.data");
    fs::write(&path, "reviewed").unwrap();
    review(root.path(), &["payload.data"]);
    commit_all(root.path());

    fs::write(&path, "staged replacement").unwrap();
    git(root.path(), &["add", "payload.data"]);
    fs::write(&path, "reviewed").unwrap();
    assert!(
        check(root.path())
            .unwrap_err()
            .contains("Git index differs from committed HEAD")
    );
}
#[cfg(unix)]
#[test]
fn committed_tree_symlink_type_mutation_is_rejected() {
    let root = fixture();
    let path = root.path().join("payload.data");
    fs::write(&path, "reviewed").unwrap();
    review(root.path(), &["payload.data"]);
    commit_all(root.path());

    let object = git_output(root.path(), &["rev-parse", "HEAD:payload.data"]);
    git(
        root.path(),
        &[
            "update-index",
            "--cacheinfo",
            "120000",
            &object,
            "payload.data",
        ],
    );
    commit_index(root.path());
    assert!(fs::symlink_metadata(&path).unwrap().is_file());
    assert!(
        check(root.path())
            .unwrap_err()
            .contains("payload.data: checkout mode/type differs from committed HEAD")
    );
}
#[test]
fn hostile_git_environment_cannot_redirect_index_provenance() {
    let root = fixture();
    fs::write(root.path().join("payload.data"), "reviewed").unwrap();
    review(root.path(), &["payload.data"]);
    commit_all(root.path());

    let clean_index = tempfile::NamedTempFile::new().unwrap();
    fs::copy(root.path().join(".git/index"), clean_index.path()).unwrap();
    git(root.path(), &["update-index", "--chmod=+x", "payload.data"]);

    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("hostile_git_environment_worker")
        .arg("--nocapture")
        .env("PGOKF_SOURCE_GUARD_WORKER_ROOT", root.path())
        .env(
            "PGOKF_SOURCE_GUARD_EXPECTED_ERROR",
            "Git index differs from committed HEAD",
        )
        .env("GIT_INDEX_FILE", clean_index.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[cfg(unix)]
#[test]
fn inherited_git_config_cannot_hide_sources_or_execute_helpers() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    fs::write(root.path().join("payload.data"), "reviewed").unwrap();
    review(root.path(), &["payload.data"]);
    commit_all(root.path());
    fs::write(root.path().join("evil.data"), "unreviewed").unwrap();

    let hostile = tempfile::tempdir().unwrap();
    let excludes = hostile.path().join("excludes");
    let hook = hostile.path().join("fsmonitor");
    let marker = hostile.path().join("executed");
    fs::write(&excludes, "evil.data\n").unwrap();
    fs::write(&hook, format!("#!/bin/sh\n: > '{}'\n", marker.display())).unwrap();
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        hostile.path().join(".gitconfig"),
        format!(
            "[core]\n\texcludesFile = {}\n\tfsmonitor = {}\n",
            excludes.display(),
            hook.display()
        ),
    )
    .unwrap();

    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("hostile_git_environment_worker")
        .arg("--nocapture")
        .env("PGOKF_SOURCE_GUARD_WORKER_ROOT", root.path())
        .env(
            "PGOKF_SOURCE_GUARD_EXPECTED_ERROR",
            "evil.data: unreviewed source execution input",
        )
        .env("PGOKF_SOURCE_GUARD_FORBIDDEN_MARKER", &marker)
        .env("HOME", hostile.path())
        .env("XDG_CONFIG_HOME", hostile.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[cfg(unix)]
#[test]
fn inherited_path_cannot_replace_git_provenance() {
    use std::os::unix::fs::PermissionsExt;

    let root = fixture();
    fs::write(root.path().join("payload.data"), "reviewed").unwrap();
    review(root.path(), &["payload.data"]);
    commit_all(root.path());
    fs::write(root.path().join("evil.data"), "unreviewed").unwrap();

    let hostile = tempfile::tempdir().unwrap();
    let wrapper = hostile.path().join("git");
    let marker = hostile.path().join("executed");
    fs::write(
        &wrapper,
        format!("#!/bin/sh\n: > '{}'\nexit 99\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();

    let output = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("hostile_git_environment_worker")
        .arg("--nocapture")
        .env("PGOKF_SOURCE_GUARD_WORKER_ROOT", root.path())
        .env(
            "PGOKF_SOURCE_GUARD_EXPECTED_ERROR",
            "evil.data: unreviewed source execution input",
        )
        .env("PGOKF_SOURCE_GUARD_FORBIDDEN_MARKER", &marker)
        .env("PATH", hostile.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn hostile_git_environment_worker() {
    let Some(root) = std::env::var_os("PGOKF_SOURCE_GUARD_WORKER_ROOT") else {
        return;
    };
    let expected = std::env::var("PGOKF_SOURCE_GUARD_EXPECTED_ERROR").unwrap();
    assert!(check(Path::new(&root)).unwrap_err().contains(&expected));
    if let Some(marker) = std::env::var_os("PGOKF_SOURCE_GUARD_FORBIDDEN_MARKER") {
        assert!(
            !Path::new(&marker).exists(),
            "Git executed an untrusted helper"
        );
    }
}
#[test]
fn replacement_refs_cannot_substitute_committed_head() {
    let root = fixture();
    let path = root.path().join("payload.data");
    fs::write(&path, "committed").unwrap();
    review(root.path(), &["payload.data"]);
    commit_all(root.path());

    fs::write(&path, "replacement").unwrap();
    review(root.path(), &["payload.data"]);
    git(root.path(), &["add", "--all"]);
    let tree = git_output(root.path(), &["write-tree"]);
    let replacement = git_output(
        root.path(),
        &[
            "-c",
            "user.name=Source Guard Test",
            "-c",
            "user.email=source-guard@example.invalid",
            "commit-tree",
            &tree,
            "-p",
            "HEAD",
            "-m",
            "replacement",
        ],
    );
    git(root.path(), &["replace", "HEAD", &replacement]);
    assert!(
        check(root.path())
            .unwrap_err()
            .contains("Git index differs from committed HEAD")
    );
}
#[test]
fn review_version_and_unknown_fields_are_closed() {
    let root = fixture();
    for manifest in [
        r#"{"version":2,"files":{}}"#,
        r#"{"version":1,"files":{},"unexpected":true}"#,
    ] {
        fs::write(root.path().join("source-review.json"), manifest).unwrap();
        assert!(check(root.path()).is_err(), "{manifest}");
    }
    fs::write(root.path().join("gate.sh"), "cargo test").unwrap();
    review(root.path(), &["gate.sh"]);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("source-review.json")).unwrap()).unwrap();
    manifest["files"]["gate.sh"]["unexpected"] = serde_json::json!(true);
    fs::write(
        root.path().join("source-review.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    assert!(check(root.path()).is_err());
}
#[test]
fn stale_review_entry_cannot_authorize_an_absent_input() {
    let root = fixture();
    fs::write(root.path().join("gate.sh"), "cargo test").unwrap();
    review(root.path(), &["gate.sh"]);
    fs::remove_file(root.path().join("gate.sh")).unwrap();
    assert!(
        check(root.path())
            .unwrap_err()
            .contains("stale source review entry")
    );
}
#[test]
fn workflow_errors_are_deterministically_sorted() {
    let root = fixture();
    let name = ".github/workflows/ci.yml";
    fs::create_dir_all(root.path().join(".github/workflows")).unwrap();
    fs::write(
        root.path().join(name),
        "steps:\n  - run: python3 z\n  - run: python3 a\n",
    )
    .unwrap();
    review(root.path(), &[name]);
    let error = check(root.path()).unwrap_err();
    let lines: Vec<_> = error.lines().collect();
    let mut sorted = lines.clone();
    sorted.sort_unstable();
    assert_eq!(lines, sorted);
}
