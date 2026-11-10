//! Process-isolated fault controls and opt-in native `PostgreSQL` regressions.
#![cfg(unix)]
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command, time::Instant};

fn executable(root: &Path, name: &str, body: &str) {
    let path = root.join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn fault(mode: &str) -> (serde_json::Value, tempfile::TempDir) {
    let task = tempfile::tempdir().unwrap();
    let bin = task.path().join("bin");
    let scratch = task.path().join("scratch");
    fs::create_dir(&bin).unwrap();
    fs::create_dir(&scratch).unwrap();
    executable(&bin, "pg_config", "cd -- \"$(dirname -- \"$0\")\" && pwd");
    executable(
        &bin,
        "initdb",
        "mkdir -p \"$2\"; : > \"$2/postgresql.conf\"",
    );
    executable(&bin, "pg_ctl", "exit 0");
    let (postgres, psql) = match mode {
        "exit" => (
            "sleep 0.2; printf 'FAULT_CHILD_EXIT_42\\n' >&2; exit 42",
            "printf 'FAULT_PROBE_WAITING\\n' >&2; sleep 3; touch \"$(dirname -- \"$0\")/probe-finished\"; exit 2",
        ),
        "exit-cleanup" => (
            "sleep 0.2; printf 'FAULT_CHILD_EXIT_42\n' >&2; exit 42",
            "sleep 3; exit 2",
        ),
        "native-pressure" => (
            "exec \"$STARTUP_NATIVE_BINDIR/postgres\" \"$@\"",
            r#"exec "$STARTUP_NATIVE_BINDIR/psql" "$@" <<'SQL'
DO $$ BEGIN FOR i IN 1..2048 LOOP RAISE LOG '%', repeat('pressure', 128); END LOOP; END $$;
SELECT 1;
SQL"#,
        ),
        "native-exit" => (
            "exec \"$STARTUP_NATIVE_BINDIR/postgres\" \"$@\" -c shared_preload_libraries=pgokf_missing_startup_library",
            "sleep 3; touch \"$(dirname -- \"$0\")/probe-finished\"; exit 2",
        ),
        "pressure" => (
            "while [ ! -e \"$2/probe\" ]; do sleep 0.01; done; head -c 1048576 /dev/zero >&2; touch \"$2/ready\"; exec sleep 90",
            "touch \"$6/data/probe\"; n=0; while [ ! -e \"$6/data/ready\" ]; do sleep 0.01; n=$((n+1)); [ \"$n\" -lt 200 ] || { touch \"$(dirname -- \"$0\")/probe-finished\"; exit 2; }; done; printf '1\\n'",
        ),
        "stop-pressure" => (
            "trap 'head -c 1048576 /dev/zero >&2; printf SHUTDOWN_DRAINED >&2; exit 0' TERM; while :; do sleep 0.1; done",
            "printf '1\n'",
        ),
        "blocked-probe" => ("exec sleep 120", "sleep 90; exit 2"),
        "refusal" => (
            "exec sleep 90",
            "printf 'FAULT_CONNECTION_REFUSED\\n' >&2; exit 2",
        ),
        "near-bound" => (
            "while [ ! -e \"$2/probe\" ]; do sleep 0.01; done; sleep 45; touch \"$2/ready\"; exec sleep 90",
            "touch \"$6/data/probe\"; [ -e \"$6/data/ready\" ] || { printf 'FAULT_STARTING\\n' >&2; exit 2; }; printf '1\\n'",
        ),
        _ => panic!("unknown fault"),
    };
    if mode.starts_with("native-") {
        let bindir = std::env::var_os("PGOKF_NATIVE_BINDIR")
            .expect("PGOKF_NATIVE_BINDIR must name an installed PostgreSQL bin directory");
        for name in ["initdb", "pg_ctl"] {
            fs::remove_file(bin.join(name)).unwrap();
            std::os::unix::fs::symlink(Path::new(&bindir).join(name), bin.join(name)).unwrap();
        }
    }
    if mode == "exit-cleanup" {
        executable(
            &bin,
            "pg_ctl",
            "printf 'server is not running\n' >&2; exit 3",
        );
    }
    executable(&bin, "postgres", postgres);
    executable(&bin, "psql", psql);
    let result = run_worker(task.path(), mode);
    (result, task)
}

fn run_worker(task: &Path, mode: &str) -> serde_json::Value {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "startup_worker", "--nocapture"])
        .env("PGOKF_STARTUP_FIXTURE", task)
        .env("PGOKF_VERIFICATION_CAMPAIGN", "startup-regression")
        .env("PGOKF_VERIFICATION_CASE", mode)
        .env(
            "STARTUP_NATIVE_BINDIR",
            std::env::var_os("PGOKF_NATIVE_BINDIR").unwrap_or_default(),
        )
        .env("TMPDIR", task.join("scratch"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&fs::read(task.join("result.json")).unwrap()).unwrap()
}

fn startup_receipt(task: &Path) -> serde_json::Value {
    let receipts: Vec<_> = fs::read_dir(task.join("scratch"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(receipts.len(), 1, "PGOKF_ASSERT: owned data cleanup");
    let receipt: serde_json::Value = serde_json::from_slice(
        &fs::read(receipts[0].join("startup.json")).expect("PGOKF_ASSERT: startup receipt"),
    )
    .unwrap();
    let stopped: serde_json::Value =
        serde_json::from_slice(&fs::read(receipts[0].join("service.stopped.json")).unwrap())
            .unwrap();
    let intent: serde_json::Value =
        serde_json::from_slice(&fs::read(receipts[0].join("service.json")).unwrap()).unwrap();
    let started: serde_json::Value =
        serde_json::from_slice(&fs::read(receipts[0].join("service.started.json")).unwrap())
            .unwrap();
    assert_eq!(
        intent["identity"]["campaign_id"], "startup-regression",
        "PGOKF_ASSERT: campaign identity"
    );
    assert!(intent["identity"]["case_id"].as_str().is_some());
    assert_eq!(intent["identity"], started["identity"]);
    assert_eq!(intent["identity"], receipt["identity"]);
    assert_eq!(
        intent["identity"], stopped["identity"],
        "PGOKF_ASSERT: stopped identity"
    );
    println!(
        "PGOKF_STARTUP_RECEIPTS: {}",
        serde_json::json!({"startup": receipt, "intent": intent, "started": started, "stopped": stopped})
    );
    assert_eq!(receipt["deadline_ms"], 60_000);
    let pid = i32::try_from(stopped["pid"].as_u64().unwrap()).unwrap();
    assert!(nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err());
    assert!(!Path::new(receipt["data"].as_str().unwrap()).exists());
    receipt
}

#[test]
fn child_exit_interrupts_blocked_readiness() {
    let (result, task) = fault("exit");
    assert!(
        !task.path().join("bin/probe-finished").exists(),
        "PGOKF_ASSERT: dead service must interrupt probe: {result}"
    );
    let receipt = startup_receipt(task.path());
    assert!(receipt["error"].as_str().unwrap().contains("42"));
    assert!(
        receipt["service"]["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("FAULT_CHILD_EXIT_42"),
        "PGOKF_ASSERT: service exit diagnostics"
    );
    assert_eq!(receipt["cleanup"]["directory_removed"], true);
}

#[test]
fn exited_owned_service_proves_cleanup_without_pg_ctl() {
    let (_, task) = fault("exit-cleanup");
    let receipt = startup_receipt(task.path());
    assert_eq!(receipt["cleanup"]["error"], serde_json::Value::Null);
    assert_eq!(receipt["cleanup"]["directory_removed"], true);
}

#[test]
#[ignore = "requires PGOKF_NATIVE_BINDIR pointing to installed PostgreSQL binaries"]
fn native_postgres_log_pressure() {
    let (result, task) = fault("native-pressure");
    assert_eq!(result["error"], serde_json::Value::Null, "{result}");
    let receipt = startup_receipt(task.path());
    assert!(receipt["service"]["output_bytes"].as_u64().unwrap() >= 1_048_576);
    assert_eq!(
        receipt["attempts"].as_array().unwrap().last().unwrap()["success"],
        true
    );
}

#[test]
#[ignore = "requires PGOKF_NATIVE_BINDIR pointing to installed PostgreSQL binaries"]
fn native_postgres_startup_exit() {
    let (result, task) = fault("native-exit");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("pgokf_missing_startup_library")
    );
    assert!(!task.path().join("bin/probe-finished").exists());
    let receipt = startup_receipt(task.path());
    assert_eq!(receipt["cleanup"]["error"], serde_json::Value::Null);
    assert_eq!(receipt["cleanup"]["directory_removed"], true);
}

#[test]
fn dependent_probe_does_not_block_service_output() {
    let (result, task) = fault("pressure");
    assert_eq!(
        result["error"],
        serde_json::Value::Null,
        "PGOKF_ASSERT: drained service must become ready"
    );
    assert!(
        !task.path().join("bin/probe-finished").exists(),
        "PGOKF_ASSERT: service output must drain during probe: {result}"
    );
    let receipt = startup_receipt(task.path());
    assert_eq!(receipt["attempts"].as_array().unwrap().len(), 1);
    assert!(receipt["service"]["output_bytes"].as_u64().unwrap() >= 1_048_576);
}

#[test]
fn persistent_refusal_retains_attempts_and_cleanup() {
    let (result, task) = fault("refusal");
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("FAULT_CONNECTION_REFUSED"),
        "PGOKF_ASSERT: last SQL error: {result}"
    );
    let receipt = startup_receipt(task.path());
    assert!(receipt["attempts"].as_array().unwrap().len() > 1);
    assert_eq!(receipt["service"]["state"], "running");
    assert_eq!(receipt["cleanup"]["directory_removed"], true);
    assert!(receipt["elapsed_ms"].as_u64().unwrap() < 62_000);
}

#[test]
fn delayed_live_startup_within_original_budget_succeeds() {
    let (result, task) = fault("near-bound");
    assert_eq!(
        result["error"],
        serde_json::Value::Null,
        "PGOKF_ASSERT: startup within original budget"
    );
    let receipt = startup_receipt(task.path());
    assert!(receipt["elapsed_ms"].as_u64().unwrap() >= 45_000);
    assert!(receipt["elapsed_ms"].as_u64().unwrap() < 60_000);
    assert!(
        receipt["attempts"].as_array().unwrap().last().unwrap()["success"]
            .as_bool()
            .unwrap()
    );
}

#[test]
fn startup_worker() {
    let Some(task) = std::env::var_os("PGOKF_STARTUP_FIXTURE") else {
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
    let start = Instant::now();
    let result = pgokf_verifier::upgrade::Cluster::start(&options);
    let elapsed_ms = start.elapsed().as_millis();
    let error = result.as_ref().err().cloned();
    drop(result);
    fs::write(
        task.join("result.json"),
        serde_json::to_vec(&serde_json::json!({"elapsed_ms":elapsed_ms,"error":error})).unwrap(),
    )
    .unwrap();
}

#[test]
fn shutdown_drains_final_service_diagnostics() {
    let (_, task) = fault("stop-pressure");
    startup_receipt(task.path());
    let directory = fs::read_dir(task.path().join("scratch"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let stopped: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.join("service.stopped.json")).unwrap()).unwrap();
    let tail = stopped["observation"]["stderr_tail"].as_str().unwrap();
    assert!(
        tail.contains("SHUTDOWN_DRAINED"),
        "PGOKF_ASSERT: shutdown drains final diagnostics"
    );
    assert!(
        tail.len() <= 4096,
        "PGOKF_ASSERT: bounded shutdown diagnostics"
    );
}

#[test]
fn blocked_probe_cannot_extend_original_budget() {
    let (result, task) = fault("blocked-probe");
    assert!(
        result["error"]
            .as_str()
            .is_some_and(|s| s.contains("timed out")),
        "PGOKF_ASSERT: original deadline refusal"
    );
    let receipt = startup_receipt(task.path());
    assert!(
        (60_000..63_000).contains(&receipt["elapsed_ms"].as_u64().unwrap()),
        "PGOKF_ASSERT: original probe budget"
    );
}
