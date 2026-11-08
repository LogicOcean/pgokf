#![cfg(unix)]
use pgokf_verifier::artifact_checks::deb_smoke;
use std::{fs, os::unix::fs::PermissionsExt};
#[test]
fn package_smoke_owns_cleanup_after_sql_failure_and_success() {
    for fail in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for (name, body) in [
            ("initdb", "#!/bin/sh\nexit 0\n".to_string()),
            ("postgres", "#!/bin/sh\nexec sleep 600\n".into()),
            (
                "psql",
                format!(
                    "#!/bin/sh\ninput=$(cat)\ncase \"$input\" in\n  'SELECT 1;') echo 1 ;;\n  'CREATE EXTENSION pgokf;') {} ;;\n  *) echo 0.3.1 ;;\nesac\n",
                    if fail {
                        "echo intended-sql-failure >&2; exit 1"
                    } else {
                        ":"
                    }
                ),
            ),
        ] {
            let file = bin.join(name);
            fs::write(&file, body).unwrap();
            fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let evidence = tmp.path().join("evidence");
        let result = deb_smoke(&bin, "0.3.1", &evidence);
        if fail {
            assert!(result.unwrap_err().contains("intended-sql-failure"));
        } else {
            result.unwrap();
        }
        let started: serde_json::Value =
            serde_json::from_slice(&fs::read(evidence.join("postgres.started.json")).unwrap())
                .unwrap();
        let stopped: serde_json::Value =
            serde_json::from_slice(&fs::read(evidence.join("postgres.stopped.json")).unwrap())
                .unwrap();
        assert_eq!(started["pid"], stopped["pid"], "wrong process stopped");
        let pid =
            nix::unistd::Pid::from_raw(i32::try_from(started["pid"].as_i64().unwrap()).unwrap());
        assert!(
            nix::sys::signal::kill(pid, None).is_err(),
            "package PostgreSQL survived cleanup"
        );
        let receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(evidence.join("cleanup.json")).unwrap()).unwrap();
        assert_eq!(
            receipt["directory_removed"], true,
            "scratch cleanup not proven"
        );
        let ownership: serde_json::Value =
            serde_json::from_slice(&fs::read(evidence.join("ownership.json")).unwrap()).unwrap();
        assert!(!std::path::Path::new(ownership["directory"].as_str().unwrap()).exists());
    }
}

#[test]
fn package_smoke_preserves_scratch_when_stop_receipt_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let evidence = tmp.path().join("evidence");
    for (name, body) in [
        ("initdb", "#!/bin/sh\nexit 0\n".to_string()),
        ("postgres", "#!/bin/sh\nexec sleep 600\n".into()),
        (
            "psql",
            format!(
                "#!/bin/sh\ncat >/dev/null\ntouch '{}'\necho 0.3.1\n",
                evidence.join("postgres.stopped.json").display()
            ),
        ),
    ] {
        let file = bin.join(name);
        fs::write(&file, body).unwrap();
        fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert!(
        deb_smoke(&bin, "0.3.1", &evidence).is_err(),
        "failed stop receipt accepted"
    );
    let started: serde_json::Value =
        serde_json::from_slice(&fs::read(evidence.join("postgres.started.json")).unwrap()).unwrap();
    let pid = nix::unistd::Pid::from_raw(i32::try_from(started["pid"].as_i64().unwrap()).unwrap());
    assert!(nix::sys::signal::kill(pid, None).is_err());
    let ownership: serde_json::Value =
        serde_json::from_slice(&fs::read(evidence.join("ownership.json")).unwrap()).unwrap();
    let work = std::path::Path::new(ownership["directory"].as_str().unwrap());
    assert!(work.is_dir(), "unproven stop deleted scratch");
    let cleanup: serde_json::Value =
        serde_json::from_slice(&fs::read(evidence.join("cleanup.json")).unwrap()).unwrap();
    assert_eq!(cleanup["directory_removed"], false);
    fs::remove_dir_all(work).unwrap();
}
