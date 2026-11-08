//! Release artifact predicates and isolated installed-package smoke verification.
use crate::{owned_docker::publish, process::CommandSpec, service::Service};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

/// # Errors
/// Different bytes or unreadable files fail.
pub fn notices(expected: &Path, actual: &Path) -> Result<(), String> {
    if fs::read(expected).map_err(|e| e.to_string())?
        != fs::read(actual).map_err(|e| e.to_string())?
    {
        return Err("THIRD-PARTY-NOTICES.txt is stale; regenerate with cargo about".into());
    }
    Ok(())
}
/// # Errors
/// Missing active RPM sections fail.
pub fn rpm(source: &str) -> Result<(), String> {
    for required in ["Name:", "Version:", "%build", "%install", "%files"] {
        if !source
            .lines()
            .any(|line| line.split_whitespace().next() == Some(required))
        {
            return Err(format!("missing active RPM section: {required}"));
        }
    }
    Ok(())
}
/// Check required sections and retain the native macro parser when installed.
/// # Errors
/// Missing sections, unreadable files or a failed native parse fail the gate.
pub fn rpm_file(path: &Path) -> Result<(), String> {
    let parser = std::env::var_os("PATH").and_then(|p| {
        std::env::split_paths(&p)
            .map(|d| d.join("rpmspec"))
            .find(|p| p.is_file())
    });
    rpm_with_parser(path, parser.as_deref())
}
fn rpm_with_parser(path: &Path, parser: Option<&Path>) -> Result<(), String> {
    rpm(&fs::read_to_string(path).map_err(|e| e.to_string())?)?;
    if let Some(parser) = parser {
        let mut command = CommandSpec::new(parser);
        command.args = vec![
            "--parse".into(),
            "--define".into(),
            "pgmajorversion 16".into(),
            path.into(),
        ];
        command.checked()?;
    }
    Ok(())
}
/// # Errors
/// Missing required PGXN metadata fails.
pub fn pgxn(source: &[u8]) -> Result<(), String> {
    let value: serde_json::Value = crate::json::from_slice(source)?;
    let object = value.as_object().ok_or("PGXN metadata must be an object")?;
    for field in [
        "name",
        "version",
        "abstract",
        "maintainer",
        "license",
        "meta-spec",
    ] {
        if !object.contains_key(field) {
            return Err(format!("missing PGXN field: {field}"));
        }
    }
    // This single requirement covers both presence and object type.
    let providers = object
        .get("provides")
        .and_then(serde_json::Value::as_object)
        .ok_or("PGXN provides must be an object")?;
    if providers.is_empty() {
        return Err("PGXN provides is empty".into());
    }
    for provider in providers.values() {
        for field in ["file", "version"] {
            if provider.get(field).is_none() {
                return Err(format!("missing PGXN provider field: {field}"));
            }
        }
    }
    Ok(())
}
/// # Errors
/// Only literal SHA256 identities are accepted.
pub fn digest(value: &str) -> Result<(), String> {
    let hash = value
        .strip_prefix("sha256:")
        .ok_or("image digest requires sha256 prefix")?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("image digest requires 64 lowercase hexadecimal digits".into());
    }
    Ok(())
}
/// # Errors
/// Both architecture artifact identities must be present.
pub fn digests(directory: &Path) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    if entries.len() != 2 {
        return Err("exactly two architecture digest artifacts required".into());
    }
    for entry in entries {
        if !entry.file_type().map_err(|e| e.to_string())?.is_file() {
            return Err("digest artifact must be a regular file".into());
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "non-UTF8 digest artifact")?;
        digest(&format!("sha256:{name}"))?;
    }
    Ok(())
}
fn extension_version(actual: &str, expected: &str) -> Result<(), String> {
    if expected.is_empty() || actual.trim() != expected {
        return Err(format!(
            "installed pgokf version differs: expected {expected}, got {actual:?}"
        ));
    }
    Ok(())
}

fn pg_command(program: impl Into<std::path::PathBuf>) -> CommandSpec {
    let mut command = CommandSpec::new(program);
    for (name, _) in
        std::env::vars_os().filter(|(name, _)| name.to_string_lossy().starts_with("PG"))
    {
        command.env_remove.push(name);
    }
    command.env.insert("LC_ALL".into(), "C".into());
    command
}

/// Smoke an installed package using real SQL in a uniquely owned local cluster.
/// # Errors
/// Failed startup, SQL, version checks or durable cleanup fail; evidence must be new.
pub fn deb_smoke(bindir: &Path, expected: &str, evidence: &Path) -> Result<(), String> {
    // Keep the owned basename short enough for long task-local socket roots.
    let work = crate::scratch::directory("d")?;
    fs::create_dir(evidence).map_err(|e| e.to_string())?;
    let data = work.path().join("data");
    publish(
        &evidence.join("ownership.json"),
        &serde_json::json!({"directory":work.path(),"bindir":bindir,"expected_version":expected}),
    )?;
    let mut cleanup_safe = true;
    let result = (|| {
        let mut init = pg_command(bindir.join("initdb"));
        init.args = vec![
            "-D".into(),
            data.as_os_str().into(),
            "-U".into(),
            "postgres".into(),
            "--auth=trust".into(),
            "--no-locale".into(),
            "--encoding=UTF8".into(),
        ];
        init.checked()?;
        let mut server = pg_command(bindir.join("postgres"));
        server.args = vec![
            "-D".into(),
            data.as_os_str().into(),
            "-k".into(),
            work.path().as_os_str().into(),
            "-c".into(),
            "listen_addresses=".into(),
        ];
        server.timeout = Duration::from_mins(3);
        let mut service = Service::spawn(&server, &evidence.join("postgres.json"))?;
        let sql = |text: &str| -> Result<String, String> {
            let mut command = pg_command(bindir.join("psql"));
            command.args = vec![
                "-X".into(),
                "-qAt".into(),
                "-v".into(),
                "ON_ERROR_STOP=1".into(),
                "-h".into(),
                work.path().as_os_str().into(),
                "-U".into(),
                "postgres".into(),
                "-d".into(),
                "postgres".into(),
            ];
            command.timeout = Duration::from_secs(5);
            command.stdin = text.as_bytes().to_vec();
            String::from_utf8(command.checked()?.stdout).map_err(|e| e.to_string())
        };
        let check = (|| {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                service.check()?;
                if sql("SELECT 1;").is_ok() {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err("package PostgreSQL readiness timeout".into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            sql("CREATE EXTENSION pgokf;")?;
            let actual = sql("SELECT extversion FROM pg_extension WHERE extname='pgokf';")?;
            publish(
                &evidence.join("observation.json"),
                &serde_json::json!({"actual_version":actual,"expected_version":expected}),
            )?;
            extension_version(&actual, expected)
        })();
        if let Err(error) = service.stop() {
            cleanup_safe = false;
            return Err(error);
        }
        check
    })();
    let cleanup = if cleanup_safe {
        work.close().map_err(|e| e.to_string())
    } else {
        let retained = work.keep();
        Err(format!("stop unproven; retained {}", retained.display()))
    };
    publish(
        &evidence.join("cleanup.json"),
        &serde_json::json!({"directory_removed":cleanup.is_ok(),"verification_error":result.as_ref().err(),"cleanup_error":cleanup.as_ref().err()}),
    )?;
    cleanup?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notice_bytes_must_match() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::write(&a, b"notice\n").unwrap();
        std::fs::write(&b, b"notice\n").unwrap();
        notices(&a, &b).unwrap();
        std::fs::write(&b, b"notice changed\n").unwrap();
        assert!(notices(&a, &b).is_err(), "changed notices accepted");
        assert!(
            notices(&a, &tmp.path().join("missing")).is_err(),
            "missing notices accepted"
        );
        fs::write(&a, b"").unwrap();
        fs::write(&b, b"").unwrap();
        assert!(
            notices(&tmp.path().join("missing"), &b).is_err(),
            "missing expected notices treated as empty"
        );
        assert!(
            notices(&a, &tmp.path().join("missing")).is_err(),
            "missing actual notices treated as empty"
        );
    }
    #[test]
    fn rpm_requires_each_active_section() {
        let required = ["Name: pgokf", "Version: 1", "%build", "%install", "%files"];
        rpm(&required.join("\n")).unwrap();
        for missing in required {
            let bad = required
                .map(|s| {
                    if s == missing {
                        format!("# {s}")
                    } else {
                        s.into()
                    }
                })
                .join("\n");
            assert!(
                rpm(&bad).is_err(),
                "missing RPM section accepted: {missing}"
            );
        }
        assert!(
            rpm("Name: x\nVersion: 1\n%builder\n%installer\n%filesuffix").is_err(),
            "RPM prefix impostor accepted"
        );
    }
    #[test]
    fn pgxn_requires_metadata_and_each_provider() {
        let good = serde_json::json!({"name":"pgokf","version":"1","abstract":"a","maintainer":["m"],"license":"postgresql","provides":{"pgokf":{"file":"x.sql","version":"1"}},"meta-spec":{"version":"1.0.0"}});
        pgxn(&serde_json::to_vec(&good).unwrap()).unwrap();
        for key in [
            "name",
            "version",
            "abstract",
            "maintainer",
            "license",
            "provides",
            "meta-spec",
        ] {
            let mut bad = good.clone();
            bad.as_object_mut().unwrap().remove(key);
            assert!(
                pgxn(&serde_json::to_vec(&bad).unwrap()).is_err(),
                "missing PGXN field accepted: {key}"
            );
        }
        for key in ["file", "version"] {
            let mut bad = good.clone();
            bad["provides"]["pgokf"]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(
                pgxn(&serde_json::to_vec(&bad).unwrap()).is_err(),
                "missing provider field accepted: {key}"
            );
        }
        assert!(pgxn(b"not json").is_err(), "invalid PGXN JSON accepted");
        assert!(
            pgxn(&serde_json::to_vec(&serde_json::json!([good])).unwrap()).is_err(),
            "non-object PGXN metadata accepted"
        );
        for (label, providers) in [
            ("empty", serde_json::json!({})),
            ("array", serde_json::json!([good["provides"]])),
            ("scalar", serde_json::json!("invalid")),
            ("null", serde_json::Value::Null),
        ] {
            let mut bad = good.clone();
            bad["provides"] = providers;
            assert!(
                pgxn(&serde_json::to_vec(&bad).unwrap()).is_err(),
                "invalid PGXN provides accepted: {label}"
            );
        }
    }
    #[test]
    fn digest_requires_complete_literal_sha256() {
        digest(&format!("sha256:{}", "a".repeat(64))).unwrap();
        for bad in [
            "",
            "sha256:",
            "sha256:../escape",
            "sha512:abcd",
            " sha256:abcd",
        ] {
            assert!(digest(bad).is_err(), "invalid image digest accepted: {bad}");
        }
        assert!(
            digest(&format!("sha256:{}", "g".repeat(64))).is_err(),
            "nonhex digest accepted"
        );
        assert!(
            digest(&format!("sha512:{}", "a".repeat(64))).is_err(),
            "wrong digest prefix accepted"
        );
        assert!(
            digest(&format!("sha256:{}", "a".repeat(63))).is_err(),
            "wrong digest length accepted"
        );
    }
    #[test]
    fn architecture_artifacts_require_two_regular_digest_files() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            digests(tmp.path()).is_err(),
            "missing architecture digests accepted"
        );
        for c in ['a', 'b'] {
            std::fs::write(tmp.path().join(c.to_string().repeat(64)), b"").unwrap();
        }
        digests(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("c".repeat(64)), b"").unwrap();
        assert!(
            digests(tmp.path()).is_err(),
            "extra architecture digest accepted"
        );
        std::fs::remove_file(tmp.path().join("c".repeat(64))).unwrap();
        std::fs::remove_file(tmp.path().join("b".repeat(64))).unwrap();
        std::fs::create_dir(tmp.path().join("b".repeat(64))).unwrap();
        assert!(digests(tmp.path()).is_err(), "non-file digest accepted");
        fs::remove_dir(tmp.path().join("b".repeat(64))).unwrap();
        fs::write(tmp.path().join("invalid"), b"").unwrap();
        assert!(
            digests(tmp.path()).is_err(),
            "invalid artifact name accepted"
        );
    }
    #[test]
    fn installed_extension_version_is_exact() {
        extension_version("0.3.1\n", "0.3.1").unwrap();
        for bad in ["", "0.3.0", "prefix0.3.1", "0.3.1\n0.3.1"] {
            assert!(
                extension_version(bad, "0.3.1").is_err(),
                "wrong installed extension accepted: {bad}"
            );
        }
        assert!(
            extension_version("", "").is_err(),
            "empty expected version accepted"
        );
    }
    #[test]
    fn native_rpm_parser_failure_is_not_waived() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let spec = tmp.path().join("package.spec");
        fs::write(&spec, "Name: pgokf\nVersion: 1\n%build\n%install\n%files\n").unwrap();
        let parser = tmp.path().join("rpmspec");
        fs::write(&parser, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&parser, fs::Permissions::from_mode(0o700)).unwrap();
        rpm_with_parser(&spec, Some(&parser)).unwrap();
        fs::write(&parser, "#!/bin/sh\nexit 7\n").unwrap();
        assert!(
            rpm_with_parser(&spec, Some(&parser)).is_err(),
            "native RPM parser failure accepted"
        );
    }
}
