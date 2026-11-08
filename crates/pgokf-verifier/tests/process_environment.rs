#![cfg(unix)]
use pgokf_verifier::{process::CommandSpec, service::Service};
use std::time::Duration;

fn spec() -> CommandSpec {
    assert!(
        std::env::var_os("PATH").is_some(),
        "test requires inherited PATH"
    );
    let mut spec = CommandSpec::new("/usr/bin/env");
    spec.env_remove = vec!["PATH".into(), "PGOKF_EXPLICIT_ENV".into()];
    spec.env
        .insert("PGOKF_EXPLICIT_ENV".into(), "restored".into());
    spec
}

fn verify(bytes: &[u8]) {
    let output = std::str::from_utf8(bytes).unwrap();
    assert!(
        !output.lines().any(|line| line.starts_with("PATH=")),
        "inherited environment was not removed"
    );
    assert!(
        output
            .lines()
            .any(|line| line == "PGOKF_EXPLICIT_ENV=restored"),
        "explicit override was not restored"
    );
}

#[test]
fn commands_remove_inherited_environment_before_overrides() {
    verify(&spec().checked().unwrap().stdout);
}

#[test]
fn services_remove_inherited_environment_before_overrides() {
    let directory = tempfile::tempdir().unwrap();
    let mut service = Service::spawn(&spec(), &directory.path().join("service.json")).unwrap();
    let mut output = Vec::new();
    while let Ok(line) = service.line(Duration::from_secs(2)) {
        output.extend(line);
    }
    service.stop().unwrap();
    verify(&output);
}
