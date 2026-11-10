#![cfg(target_os = "macos")]

use pgokf_verifier::process::CommandSpec;
use std::{
    os::unix::process::ExitStatusExt,
    time::{Duration, Instant},
};

#[test]
fn large_stdin_to_nonreader_respects_timeout() {
    let mut spec = CommandSpec::new("/bin/sleep");
    spec.args = vec!["30".into()];
    spec.stdin = vec![b'x'; 1_048_576];
    spec.timeout = Duration::from_millis(300);
    let started = Instant::now();
    let error = spec.run().unwrap_err();
    assert!(error.contains("timeout exceeded"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn signal_termination_preserves_raw_exit_status() {
    let mut spec = CommandSpec::new("/bin/sh");
    spec.args = ["-c", "kill -TERM $$"].map(Into::into).into();
    spec.timeout = Duration::from_secs(5);
    let output = spec.run().unwrap();
    assert_eq!(output.status.signal(), Some(nix::libc::SIGTERM));
}
