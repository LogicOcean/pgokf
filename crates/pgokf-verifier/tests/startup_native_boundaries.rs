//! Actual `PostgreSQL` deadline and stale socket controls in isolated processes.
#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn executable(root: &Path, name: &str, body: &str) {
    let path = root.join(name);
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn native(mode: &str) -> (serde_json::Value, tempfile::TempDir) {
    let bindir = std::env::var_os("PGOKF_NATIVE_BINDIR").expect("native PostgreSQL bin required");
    let task = tempfile::tempdir().unwrap();
    let bin = task.path().join("bin");
    let scratch = task.path().join("scratch");
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&scratch).unwrap();
    executable(&bin, "pg_config", "cd -- \"$(dirname -- \"$0\")\" && pwd");
    for name in ["initdb", "pg_ctl"] {
        std::os::unix::fs::symlink(Path::new(&bindir).join(name), bin.join(name)).unwrap();
    }
    let postgres = match mode {
        "near" => "sleep 58; exec \"$STARTUP_NATIVE_BINDIR/postgres\" \"$@\"",
        "stale" => {
            r#"PGOKF_STALE_SOCKET="$(dirname -- "$2")/.s.PGSQL.5432" "$STARTUP_TEST_EXE" --exact stale_socket_worker
: > "$(dirname -- "$0")/stale-created"
sleep 0.5
exec "$STARTUP_NATIVE_BINDIR/postgres" "$@""#
        }
        "deadline" => "exec \"$STARTUP_NATIVE_BINDIR/postgres\" \"$@\"",
        _ => panic!("unknown native mode"),
    };
    executable(&bin, "postgres", postgres);
    executable(
        &bin,
        "psql",
        if mode == "deadline" {
            "sleep 90; exec \"$STARTUP_NATIVE_BINDIR/psql\" \"$@\""
        } else {
            "exec \"$STARTUP_NATIVE_BINDIR/psql\" \"$@\""
        },
    );
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "native_startup_worker", "--nocapture"])
        .env("PGOKF_NATIVE_FIXTURE", task.path())
        .env("STARTUP_NATIVE_BINDIR", bindir)
        .env("STARTUP_TEST_EXE", std::env::current_exe().unwrap())
        .env("PGOKF_VERIFICATION_CAMPAIGN", "native-boundaries")
        .env("PGOKF_VERIFICATION_CASE", mode)
        .env("TMPDIR", &scratch)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let result =
        serde_json::from_slice(&fs::read(task.path().join("result.json")).unwrap()).unwrap();
    (result, task)
}

fn receipt(task: &Path) -> serde_json::Value {
    let directories = fs::read_dir(task.join("scratch"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(directories.len(), 1, "PGOKF_ASSERT: native data cleanup");
    let read = |name| {
        serde_json::from_slice::<serde_json::Value>(&fs::read(directories[0].join(name)).unwrap())
            .unwrap()
    };
    let receipt = read("startup.json");
    let stopped = read("service.stopped.json");
    println!(
        "PGOKF_NATIVE_RECEIPTS: {}",
        serde_json::json!({"startup": receipt, "stopped": stopped, "intent": read("service.json"), "started": read("service.started.json")})
    );
    for name in [
        "service.json",
        "service.started.json",
        "service.stopped.json",
    ] {
        assert_eq!(receipt["identity"], read(name)["identity"]);
    }
    let pid = i32::try_from(stopped["pid"].as_u64().unwrap()).unwrap();
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err());
    assert!(!Path::new(receipt["data"].as_str().unwrap()).exists());
    assert!(!Path::new(receipt["socket"].as_str().unwrap()).exists());
    assert_eq!(receipt["deadline_ms"], 60_000);
    receipt
}

#[test]
#[ignore = "requires PGOKF_NATIVE_BINDIR and an isolated native resource window"]
fn native_startup_near_deadline_succeeds() {
    let (result, task) = native("near");
    assert_eq!(
        result["error"],
        serde_json::Value::Null,
        "PGOKF_ASSERT: native near deadline: {result}"
    );
    let receipt = receipt(task.path());
    assert!((58_000..60_000).contains(&receipt["elapsed_ms"].as_u64().unwrap()));
    assert_eq!(
        receipt["attempts"].as_array().unwrap().last().unwrap()["success"],
        true
    );
}

#[test]
#[ignore = "requires PGOKF_NATIVE_BINDIR and an isolated native resource window"]
fn native_readiness_cannot_extend_deadline() {
    let (result, task) = native("deadline");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|s| s.contains("timed out")),
        "PGOKF_ASSERT: native deadline refusal: {result}"
    );
    let receipt = receipt(task.path());
    assert!(
        (60_000..62_000).contains(&receipt["elapsed_ms"].as_u64().unwrap()),
        "PGOKF_ASSERT: native deadline budget"
    );
    assert_eq!(receipt["cleanup"]["directory_removed"], true);
}

#[test]
#[ignore = "requires PGOKF_NATIVE_BINDIR and an isolated native resource window"]
fn native_stale_socket_is_replaced_and_cleaned() {
    let (result, task) = native("stale");
    assert!(task.path().join("bin/stale-created").exists());
    assert_eq!(
        result["error"],
        serde_json::Value::Null,
        "PGOKF_ASSERT: native stale socket: {result}"
    );
    let receipt = receipt(task.path());
    assert_eq!(
        receipt["attempts"].as_array().unwrap().last().unwrap()["success"],
        true
    );
}

#[test]
fn stale_socket_worker() {
    let Some(path) = std::env::var_os("PGOKF_STALE_SOCKET") else {
        return;
    };
    let socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(socket);
    assert!(Path::new(&path).exists());
    assert!(std::os::unix::net::UnixStream::connect(&path).is_err());
}

#[test]
fn native_startup_worker() {
    let Some(task) = std::env::var_os("PGOKF_NATIVE_FIXTURE") else {
        return;
    };
    let task = Path::new(&task);
    let options = pgokf_verifier::upgrade::Options {
        root: task.into(),
        pg_config: task.join("bin/pg_config"),
        preload: None,
        extension_prefix: None,
        operator_sql: None,
        reconciliation_only: false,
        docker_image: None,
        historical_install: None,
        provider_package: None,
        probe: None,
        embedding_policy: pgokf_verifier::catalog::EmbeddingPolicy::NotRequested,
        embedding_dimensions: None,
    };
    let result = pgokf_verifier::upgrade::Cluster::start(&options);
    let error = result.as_ref().err().cloned();
    drop(result);
    fs::write(
        task.join("result.json"),
        serde_json::to_vec(&serde_json::json!({"error":error})).unwrap(),
    )
    .unwrap();
}
