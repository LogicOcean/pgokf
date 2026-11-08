#![cfg(unix)]
use pgokf_verifier::process::CommandSpec;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

#[test]
fn failed_benchmark_stop_preserves_owned_scratch() {
    let source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/bench/run_bench.sh"),
    )
    .unwrap();
    let body = source.split_once("cleanup() {").unwrap().1;
    let body = body.split_once("\n}\ntrap cleanup EXIT").unwrap().0;
    let temporary = tempfile::tempdir().unwrap();
    let work = temporary.path().join("work");
    let socket = temporary.path().join("socket");
    let data = work.join("data");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir(&socket).unwrap();
    fs::write(data.join("postmaster.pid"), "owned fixture\n").unwrap();
    let controller = temporary.path().join("pg_ctl");
    fs::write(
        &controller,
        "#!/bin/sh\nfor arg do if [ \"$arg\" = status ]; then exit 0; fi; done\nexit 1\n",
    )
    .unwrap();
    fs::set_permissions(&controller, fs::Permissions::from_mode(0o700)).unwrap();
    let mut command = CommandSpec::new("bash");
    command.args = vec![
        "-c".into(),
        format!("cleanup() {{{body}\n}}\ncleanup").into(),
    ];
    for (name, value) in [
        ("PGDATA", &data),
        ("WORK", &work),
        ("SOCK", &socket),
        ("PG_CTL", &controller),
    ] {
        command.env.insert(name.into(), value.as_os_str().into());
    }
    let result = command.run().unwrap();
    assert!(!result.status.success(), "failed stop was accepted");
    assert!(
        data.join("postmaster.pid").exists(),
        "live scratch was deleted"
    );
    assert!(socket.is_dir(), "live socket directory was deleted");
}
