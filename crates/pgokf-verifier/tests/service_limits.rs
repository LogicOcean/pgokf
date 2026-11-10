#![cfg(unix)]
use pgokf_verifier::{process::CommandSpec, service::Service};
use std::{
    fs,
    process::{Command, Stdio},
    time::Duration,
};

const DETACHED_ROLE: &str = "PGOKF_DETACHED_DESCENDANT_ROLE";
const DETACHED_MARKER: &str = "PGOKF_DETACHED_DESCENDANT_MARKER";
const DETACHED_LEADER_SLEEP_MS: &str = "PGOKF_DETACHED_LEADER_SLEEP_MS";
const DETACHED_LEADER_OUTPUT_BYTES: &str = "PGOKF_DETACHED_LEADER_OUTPUT_BYTES";
const DETACHED_LEADER_OUTPUT_MARKER: &str = "PGOKF_DETACHED_LEADER_OUTPUT_MARKER";

#[test]
fn detached_descendant_helper() {
    match std::env::var(DETACHED_ROLE).as_deref() {
        Ok("leader") => {
            let executable = std::env::current_exe().unwrap();
            let marker = std::env::var_os(DETACHED_MARKER).unwrap();
            let mut child = Command::new(executable)
                .args(["--exact", "detached_descendant_helper", "--nocapture"])
                .env(DETACHED_ROLE, "child")
                .env(DETACHED_MARKER, marker)
                .env_remove("PGOKF_PROCESS_OWNER")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            for _ in 0..500 {
                if std::env::var_os(DETACHED_MARKER)
                    .is_some_and(|path| std::path::Path::new(&path).exists())
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(child.try_wait().unwrap().is_none());
            if let Ok(bytes) = std::env::var(DETACHED_LEADER_OUTPUT_BYTES) {
                println!("{}", "x".repeat(bytes.parse().unwrap()));
                fs::write(
                    std::env::var_os(DETACHED_LEADER_OUTPUT_MARKER).unwrap(),
                    b"ready",
                )
                .unwrap();
            }
            if let Ok(milliseconds) = std::env::var(DETACHED_LEADER_SLEEP_MS) {
                std::thread::sleep(Duration::from_millis(milliseconds.parse().unwrap()));
            }
        }
        Ok("child") => {
            nix::unistd::setsid().unwrap();
            let marker = std::env::var_os(DETACHED_MARKER).unwrap();
            fs::write(marker, std::process::id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
        _ => {}
    }
}

fn detached_spec(marker: &std::path::Path) -> CommandSpec {
    let mut spec = CommandSpec::new(std::env::current_exe().unwrap());
    spec.args = ["--exact", "detached_descendant_helper", "--nocapture"]
        .map(Into::into)
        .into();
    spec.env.insert(DETACHED_ROLE.into(), "leader".into());
    spec.env
        .insert(DETACHED_MARKER.into(), marker.as_os_str().into());
    spec.env
        .insert("PGOKF_PROCESS_OWNER".into(), "caller-forgery".into());
    spec.timeout = Duration::from_secs(5);
    spec
}

fn cleanup_detached(marker: &std::path::Path) -> bool {
    use nix::{sys::signal::kill, unistd::Pid};
    let pid = Pid::from_raw(fs::read_to_string(marker).unwrap().parse().unwrap());
    let alive = kill(pid, None).is_ok();
    #[cfg(not(target_os = "macos"))]
    let _ = kill(pid, nix::sys::signal::Signal::SIGKILL);
    alive
}

#[test]
fn successful_command_terminates_a_detached_closed_pipe_descendant() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("command-detached.pid");
    let result = detached_spec(&marker).run();
    assert!(marker.exists(), "detached descendant marker missing");
    let alive = cleanup_detached(&marker);
    #[cfg(target_os = "macos")]
    {
        assert!(!alive, "detached descendant survived successful command");
        assert!(result.unwrap().status.success());
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert!(
            alive,
            "probe prerequisite failed: descendant already exited"
        );
        let error = result.unwrap_err();
        assert!(error.contains("process containment"), "{error}");
    }
}

#[test]
fn timed_out_command_terminates_detached_descendant() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("command-timeout-detached.pid");
    let mut spec = detached_spec(&marker);
    spec.env
        .insert(DETACHED_LEADER_SLEEP_MS.into(), "30000".into());
    spec.timeout = Duration::from_secs(1);
    let error = spec.run().unwrap_err();
    assert!(marker.exists(), "detached descendant marker missing");
    let alive = cleanup_detached(&marker);
    assert!(error.contains("timeout exceeded"), "{error}");
    #[cfg(target_os = "macos")]
    assert!(!alive, "detached descendant survived command timeout");
    #[cfg(not(target_os = "macos"))]
    {
        assert!(
            alive,
            "probe prerequisite failed: descendant already exited"
        );
        assert!(error.contains("process containment"), "{error}");
    }
}

#[test]
fn service_stop_terminates_a_detached_closed_pipe_descendant() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("service-detached.pid");
    let receipt = directory.path().join("service.json");
    let mut service = Service::spawn(&detached_spec(&marker), &receipt).unwrap();
    for _ in 0..500 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(marker.exists(), "detached descendant marker missing");
    let result = service.stop();
    let alive = cleanup_detached(&marker);
    let stopped: serde_json::Value =
        serde_json::from_slice(&fs::read(receipt.with_extension("stopped.json")).unwrap()).unwrap();
    #[cfg(target_os = "macos")]
    {
        assert!(!alive, "detached descendant survived service stop");
        result.unwrap();
        assert_eq!(stopped["state"], "stopped");
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert!(
            alive,
            "probe prerequisite failed: descendant already exited"
        );
        let error = result.unwrap_err();
        assert!(error.contains("process containment"), "{error}");
        assert_eq!(stopped["state"], "cleanup-failed");
    }
}

#[test]
fn repeated_service_stop_preserves_detached_descendant_result() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("service-repeat-detached.pid");
    let receipt = directory.path().join("service-repeat.json");
    let mut service = Service::spawn(&detached_spec(&marker), &receipt).unwrap();
    for _ in 0..500 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(marker.exists(), "detached descendant marker missing");
    let first = service.stop();
    let second = service.stop();
    let alive = cleanup_detached(&marker);
    #[cfg(target_os = "macos")]
    {
        assert!(!alive, "detached descendant survived repeated stop");
        first.unwrap();
        second.unwrap();
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert!(
            alive,
            "probe prerequisite failed: descendant already exited"
        );
        let first = first.unwrap_err();
        let second = second.unwrap_err();
        assert!(first.contains("process containment"), "{first}");
        assert_eq!(second, first);
    }
}

#[test]
fn service_stop_preserves_output_failure_while_containing_descendant() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("service-combined-detached.pid");
    let output_marker = directory.path().join("service-combined-output.ready");
    let receipt = directory.path().join("service-combined.json");
    let mut spec = detached_spec(&marker);
    spec.env
        .insert(DETACHED_LEADER_OUTPUT_BYTES.into(), "4096".into());
    spec.env.insert(
        DETACHED_LEADER_OUTPUT_MARKER.into(),
        output_marker.as_os_str().into(),
    );
    spec.env
        .insert(DETACHED_LEADER_SLEEP_MS.into(), "30000".into());
    spec.output_cap = 64;
    let mut service = Service::spawn(&spec, &receipt).unwrap();
    for _ in 0..500 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(marker.exists(), "detached descendant marker missing");
    for _ in 0..500 {
        if output_marker.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(output_marker.exists(), "leader output marker missing");
    let first = service.stop().unwrap_err();
    let second = service.stop().unwrap_err();
    let alive = cleanup_detached(&marker);
    let stopped: serde_json::Value =
        serde_json::from_slice(&fs::read(receipt.with_extension("stopped.json")).unwrap()).unwrap();
    let receipt_error = stopped["error"].as_str().unwrap();
    #[cfg(target_os = "macos")]
    {
        assert!(!alive, "detached descendant survived failed service stop");
        assert!(first.contains("output cap"), "{first}");
        assert!(!first.contains("process containment"), "{first}");
        assert_eq!(second, first);
        assert!(receipt_error.contains("output cap"), "{stopped}");
        assert!(!receipt_error.contains("process containment"), "{stopped}");
    }
    #[cfg(not(target_os = "macos"))]
    {
        assert!(
            alive,
            "probe prerequisite failed: descendant already exited"
        );
        assert!(first.contains("output cap"), "{first}");
        assert!(first.contains("process containment"), "{first}");
        assert_eq!(second, first);
        assert!(receipt_error.contains("output cap"), "{stopped}");
        assert!(receipt_error.contains("process containment"), "{stopped}");
    }
}

fn finite() -> CommandSpec {
    let mut spec = CommandSpec::new("/bin/sh");
    spec.args = ["-c", "sleep 1"].map(Into::into).into();
    spec.timeout = Duration::from_secs(1);
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
    assert!(service.stop().unwrap_err().contains("output cap"));
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

#[test]
fn stop_drains_diagnostics_after_lifetime_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("term-delivered");
    let receipt = dir.path().join("service.json");
    let mut spec = CommandSpec::new("/bin/sh");
    spec.args = [
        "-c".into(),
        format!(
            "trap 'printf delivered > {}; printf FINAL_AFTER_LIFETIME >&2; exit 0' TERM; printf 'ready\\n'; while :; do sleep 0.01; done",
            marker.display()
        )
        .into(),
    ]
    .into();
    spec.timeout = Duration::from_secs(1);
    let mut service = Service::spawn(&spec, &receipt).unwrap();
    assert_eq!(
        service.line(Duration::from_millis(100)).unwrap(),
        b"ready\n"
    );
    std::thread::sleep(Duration::from_millis(1050));
    assert!(
        service
            .line(Duration::from_millis(20))
            .unwrap_err()
            .contains("lifetime deadline")
    );
    service.stop().unwrap();
    assert_eq!(fs::read(marker).unwrap(), b"delivered");
    let stopped: serde_json::Value =
        serde_json::from_slice(&fs::read(receipt.with_extension("stopped.json")).unwrap()).unwrap();
    assert!(
        stopped["observation"]["stderr_tail"]
            .as_str()
            .unwrap()
            .contains("FINAL_AFTER_LIFETIME"),
        "{stopped}"
    );
}
