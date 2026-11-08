#![cfg(unix)]
use pgokf_verifier::{process::CommandSpec, service::Service};
use std::{fs, time::Duration};
fn finite() -> CommandSpec {
    let mut spec = CommandSpec::new("/bin/sh");
    spec.args = ["-c", "sleep 1"].map(Into::into).into();
    spec.timeout = Duration::from_millis(200);
    spec
}
#[test]
fn lifetime_deadline_has_its_own_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = finite();
    spec.timeout = Duration::from_millis(20);
    let mut service = Service::spawn(&spec, &dir.path().join("service.json")).unwrap();
    let error = service.line(Duration::from_secs(2)).unwrap_err();
    service.stop().unwrap();
    assert!(error.contains("lifetime deadline"), "{error}");
}
#[test]
fn response_deadline_and_nonblocking_pipes_are_independent_of_lifetime() {
    let dir = tempfile::tempdir().unwrap();
    let mut service = Service::spawn(&finite(), &dir.path().join("service.json")).unwrap();
    let error = service.line(Duration::from_millis(20)).unwrap_err();
    service.stop().unwrap();
    assert!(error.contains("response deadline"), "{error}");
}
#[test]
fn output_cap_has_its_own_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = finite();
    spec.args = ["-c", "while :; do printf x; done"].map(Into::into).into();
    spec.output_cap = 64;
    let mut service = Service::spawn(&spec, &dir.path().join("service.json")).unwrap();
    let error = service.line(Duration::from_secs(2)).unwrap_err();
    service.stop().unwrap();
    assert!(error.contains("output cap"), "{error}");
}
#[test]
fn stop_kills_term_resistant_descendants() {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    let dir = tempfile::tempdir().unwrap();
    let mut spec = finite();
    spec.timeout = Duration::from_secs(5);
    spec.args = [
        "-c",
        "(trap '' TERM; echo ready; exec sleep 3) & echo $!; wait",
    ]
    .map(Into::into)
    .into();
    let mut service = Service::spawn(&spec, &dir.path().join("service.json")).unwrap();
    let first = service.line(Duration::from_secs(1)).unwrap();
    let second = service.line(Duration::from_secs(1)).unwrap();
    let lines = [first, second];
    assert!(lines.iter().any(|line| line == b"ready\n"));
    let pid = Pid::from_raw(
        lines
            .iter()
            .find_map(|line| std::str::from_utf8(line).unwrap().trim().parse().ok())
            .unwrap(),
    );

    service.stop().unwrap();
    for _ in 0..50 {
        if kill(pid, None) == Err(nix::errno::Errno::ESRCH) {
            return;
        }
        // A container's PID 1 may not reap orphaned children. A zombie has
        // terminated and cannot execute, even though kill(pid, 0) still works.
        #[cfg(target_os = "linux")]
        if fs::read_to_string(format!("/proc/{}/stat", pid.as_raw()))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, fields)| fields.trim().starts_with("Z "))
            })
            == Some(true)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // Always terminate our exact spawned descendant before reporting a mutant.
    let _ = kill(pid, Signal::SIGKILL);
    std::thread::sleep(Duration::from_millis(50));
    panic!("owned descendant survived service stop");
}
#[test]
fn failed_started_receipt_still_publishes_stopped_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("service.json");
    fs::write(receipt.with_extension("started.json"), "occupied").unwrap();
    let result = Service::spawn(&finite(), &receipt);
    // A drop-removal mutant must not leave an unbounded fixture process.
    if !receipt.with_extension("stopped.json").exists() {
        std::thread::sleep(Duration::from_millis(1200));
    }
    assert!(result.is_err());
    assert!(
        receipt.with_extension("stopped.json").exists(),
        "missing stopped receipt after publication failure"
    );
    assert_eq!(
        fs::read_to_string(receipt.with_extension("started.json")).unwrap(),
        "occupied"
    );
}
