//! Exercise typed `PostgreSQL` pins and real provisioning argv with command fixtures.
use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

struct Command(pgokf_verifier::process::CommandSpec);
impl Command {
    fn new(program: &str) -> Self {
        let mut spec = pgokf_verifier::process::CommandSpec::new("/usr/bin/env");
        spec.args = ["-u", "BASH_ENV", "-u", "ENV", program]
            .into_iter()
            .map(Into::into)
            .collect();
        Self(spec)
    }
    fn args(&mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> &mut Self {
        self.0
            .args
            .extend(args.into_iter().map(|s| s.as_ref().to_os_string()));
        self
    }
    fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.0.args.push(arg.as_ref().to_os_string());
        self
    }
    fn env(&mut self, key: &str, value: impl AsRef<OsStr>) -> &mut Self {
        self.0.env.insert(key.into(), value.as_ref().to_os_string());
        self
    }
    fn output(&self) -> Result<std::process::Output, String> {
        self.0.run()
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn installed_beta_package_must_match_exact_release() {
    for (version, expected) in [
        ("19~beta3-1.pgdg13+1", true),
        ("19~beta2-1.pgdg13+1", false),
        ("19~rc1-1", false),
        ("19.0-1", false),
        ("", false),
    ] {
        assert_eq!(
            pgokf_verifier::postgres_pin::package(19, "19beta3", version).is_ok(),
            expected,
            "{version}"
        );
    }
}

#[test]
fn beta_image_must_have_exact_immutable_digest() {
    for (digest, expected) in [
        (pgokf_verifier::postgres_pin::BETA_DIGEST.to_owned(), true),
        (format!("sha256:{}", "a".repeat(64)), false),
        (String::new(), false),
    ] {
        assert_eq!(
            pgokf_verifier::postgres_pin::digest(&digest).is_ok(),
            expected,
            "{digest}"
        );
    }
}

#[test]
fn beta_provisioning_refuses_missing_drifted_and_failed_packages() {
    let original = std::fs::read_to_string(root().join("packaging/install-postgres.sh")).unwrap();
    for (scenario, candidate, expected) in [
        ("ok", "19~beta3-1.pgdg24.04+1", true),
        ("missing", "(none)", false),
        ("drift", "19~beta4-1", false),
        ("ga", "19.0-1", false),
        ("install-failure", "19~beta3-1", false),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let target = |name| directory.path().join(name).to_str().unwrap().to_owned();
        std::fs::write(target("os-release"), "VERSION_CODENAME=noble\n").unwrap();
        let body = original
            .replace("/etc/os-release", &target("os-release"))
            .replace("/etc/apt/sources.list.d/pgdg.list", &target("pgdg.list"))
            .replace("/usr/share/postgresql-common/pgdg", &target("keys"));
        std::fs::write(target("install.sh"), body).unwrap();
        std::fs::write(
            target("dpkg-query"),
            "#!/bin/sh\nprintf '%s' \"$CANDIDATE\"\n",
        )
        .unwrap();
        std::fs::set_permissions(target("dpkg-query"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::copy(
            root().join("packaging/verify-postgres.sh"),
            target("verify-postgres.sh"),
        )
        .unwrap();
        let result = Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                r#"
            curl() { :; }; gpg() { cat >/dev/null; }
            apt-cache() { echo "  Candidate: $CANDIDATE"; }
            dpkg-query() { echo "$CANDIDATE"; }
            apt-get() { [[ "$1" != install || "$SCENARIO" != install-failure ]]; }
            export -f curl gpg apt-cache dpkg-query apt-get
            bash "$1" 19
        "#,
                "_",
            ])
            .arg(target("install.sh"))
            .env("PGOKF_VERIFIER", env!("CARGO_BIN_EXE_pgokf-verifier"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    directory.path().display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("SCENARIO", scenario)
            .env("CANDIDATE", candidate)
            .output()
            .unwrap();
        assert_eq!(
            result.status.success(),
            expected,
            "{scenario}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            std::fs::read_to_string(target("pgdg.list"))
                .unwrap()
                .contains("noble-pgdg main 19")
        );
    }
}

#[test]
fn missing_beta_image_fails_actual_workflow_step() {
    let source = std::fs::read_to_string(root().join(".github/workflows/packages.yml")).unwrap();
    let workflow = pgokf_verifier::workflow::parse(&source).unwrap();
    let step = workflow["jobs"]["docker"]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "base")
        .unwrap();
    let script = step["run"]
        .as_str()
        .unwrap()
        .replace("${{ matrix.pg }}", "19")
        .replace("${{ matrix.experimental }}", "true");
    let work = tempfile::tempdir().unwrap();
    let mut spec = pgokf_verifier::process::CommandSpec::new("/bin/bash");
    spec.args = [
        "--noprofile",
        "--norc",
        "-e",
        "-c",
        &format!("docker() {{ return 1; }}; {script}"),
    ]
    .into_iter()
    .map(Into::into)
    .collect();
    spec.env.insert(
        "GITHUB_OUTPUT".into(),
        work.path().join("out").into_os_string(),
    );
    assert!(!spec.run().unwrap().status.success());
}
#[test]
fn stable_deb_refuses_beta_before_building() {
    let mut spec = pgokf_verifier::process::CommandSpec::new("/bin/bash");
    spec.args = vec![
        root().join("packaging/deb/build-deb.sh").into_os_string(),
        "19".into(),
    ];
    let result = spec.run().unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("unsupported PG_MAJOR"));
}

#[test]
fn installed_driver_checks_each_package_and_command_status() {
    let work = tempfile::tempdir().unwrap();
    let executable = work.path().join("dpkg-query");
    std::fs::write(
        &executable,
        r#"#!/bin/sh
case "$3" in
    postgresql-19) printf '%s' "$SERVER_VERSION" ;;
    postgresql-server-dev-19) printf '%s' "$HEADER_VERSION" ;;
    *) exit 3 ;;
esac
[ "$3" != "$FAIL_PACKAGE" ]
"#,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (server, headers, failed, expected) in [
        ("19~beta3-1", "19~beta3-1", "", true),
        ("19~beta4-1", "19~beta3-1", "", false),
        ("19~beta3-1", "19~beta4-1", "", false),
        ("19~beta3-1", "19~beta3-1", "postgresql-19", false),
        (
            "19~beta3-1",
            "19~beta3-1",
            "postgresql-server-dev-19",
            false,
        ),
    ] {
        let mut command =
            pgokf_verifier::process::CommandSpec::new(env!("CARGO_BIN_EXE_pgokf-verifier"));
        command.args = ["postgres-check", "19", "19beta3"]
            .into_iter()
            .map(Into::into)
            .collect();
        command.env.insert(
            "PATH".into(),
            format!(
                "{}:{}",
                work.path().display(),
                std::env::var("PATH").unwrap()
            )
            .into(),
        );
        command.env.insert("SERVER_VERSION".into(), server.into());
        command.env.insert("HEADER_VERSION".into(), headers.into());
        command.env.insert("FAIL_PACKAGE".into(), failed.into());
        let result = command.run().unwrap();
        assert_eq!(
            result.status.success(),
            expected,
            "server={server} headers={headers} failed={failed}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
