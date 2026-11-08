//! Public image behavior gate with receipt-bound resources and backup/restore.
use crate::{
    owned_docker::{self, DockerApi, DurableStore},
    ownership::CreationReceipt,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmokeReport {
    pub image: String,
    pub version: String,
    pub optional: bool,
    pub assertions: Vec<String>,
    pub containers: Vec<String>,
}
fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}
fn call(docker: &mut impl DockerApi, parts: &[&str]) -> Result<String, String> {
    docker.call(&args(parts)).map(|s| s.trim().to_owned())
}
fn sql(docker: &mut impl DockerApi, id: &str, user: &str, query: &str) -> Result<String, String> {
    call(
        docker,
        &[
            "exec",
            id,
            "psql",
            "-h",
            "127.0.0.1",
            "-U",
            user,
            "-d",
            "postgres",
            "-v",
            "ON_ERROR_STOP=1",
            "-tA",
            "-c",
            query,
        ],
    )
}
fn equal(
    report: &mut SmokeReport,
    label: &str,
    actual: &str,
    expected: &str,
) -> Result<(), String> {
    if actual != expected {
        return Err(format!("{label}: expected {expected:?}, got {actual:?}"));
    }
    report.assertions.push(label.into());
    Ok(())
}
#[derive(Debug, PartialEq, Eq)]
enum WaitOutcome {
    Ready,
    Retry,
    Expired,
}
fn wait_outcome(actual: Option<&str>, expected: &str, elapsed: Duration) -> WaitOutcome {
    if actual == Some(expected) {
        WaitOutcome::Ready
    } else if elapsed >= Duration::from_mins(2) {
        WaitOutcome::Expired
    } else {
        WaitOutcome::Retry
    }
}
fn wait_sql(
    docker: &mut impl DockerApi,
    id: &str,
    query: &str,
    expected: &str,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        let actual = sql(docker, id, "postgres", query).ok();
        match wait_outcome(actual.as_deref(), expected, start.elapsed()) {
            WaitOutcome::Ready => return Ok(()),
            WaitOutcome::Expired => return Err(format!("initialization deadline: {query}")),
            WaitOutcome::Retry => std::thread::sleep(Duration::from_secs(2)),
        }
    }
}
fn create(
    docker: &mut impl DockerApi,
    evidence: &Path,
    run: &str,
    label: &str,
    image: &str,
    options: &[String],
    owned: &mut Vec<(String, CreationReceipt)>,
) -> Result<String, String> {
    let proof = owned_docker::create(
        docker,
        &mut DurableStore,
        "pgokf.smoke-run",
        run,
        &evidence.join(format!("{label}.creation.json")),
        image,
        options,
    )?;
    let id = proof.container.id.clone();
    owned.push((label.into(), proof));
    Ok(id)
}
/// Execute the shipped server image through initialization, roles, policy,
/// provider reader search, cron, materialization, backup and fresh restore.
/// # Errors
/// Any product assertion, deadline, receipt or owned cleanup failure fails.
pub fn smoke(
    docker: &mut impl DockerApi,
    root: &Path,
    evidence: &Path,
    image: &str,
    version: &str,
    optional: bool,
) -> Result<SmokeReport, String> {
    std::fs::create_dir(evidence).map_err(|e| e.to_string())?;
    let evidence = evidence.canonicalize().map_err(|e| e.to_string())?;
    let run = uuid::Uuid::new_v4().simple().to_string();
    let mut owned = Vec::new();
    let result = probe(
        docker, root, &evidence, &run, image, version, optional, &mut owned,
    );
    let mut cleanup_errors = Vec::new();
    for (label, proof) in &owned {
        if let Err(error) = owned_docker::cleanup(
            docker,
            &mut DurableStore,
            &proof.container.id,
            "pgokf.smoke-run",
            &run,
            &evidence.join(format!("{label}.cleanup.json")),
            Some(proof),
        ) {
            cleanup_errors.push(error);
        }
    }
    if !cleanup_errors.is_empty() {
        return Err(format!(
            "probe: {result:?}; cleanup: {}",
            cleanup_errors.join("; ")
        ));
    }
    let report = result?;
    owned_docker::publish(&evidence.join("smoke.json"), &report)?;
    Ok(report)
}
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn probe(
    docker: &mut impl DockerApi,
    root: &Path,
    evidence: &Path,
    run: &str,
    image: &str,
    version: &str,
    optional: bool,
    owned: &mut Vec<(String, CreationReceipt)>,
) -> Result<SmokeReport, String> {
    let mut report = SmokeReport {
        image: image.into(),
        version: version.into(),
        optional,
        assertions: Vec::new(),
        containers: Vec::new(),
    };
    let probe = create(
        docker,
        evidence,
        run,
        "probe",
        image,
        &args(&[
            "--entrypoint",
            "find",
            "--",
            "/usr/share/postgresql",
            "-name",
            "*.control",
        ]),
        owned,
    )?;
    let carried = call(docker, &["start", "-a", &probe])?;
    let mut preload = vec!["pgokf"];
    for name in ["pg_cron", "pg_textsearch", "pg_search"] {
        if carried
            .lines()
            .any(|s| s.ends_with(&format!("/{name}.control")))
        {
            preload.push(name);
        }
    }
    let mut options = args(&[
        "-e",
        "POSTGRES_PASSWORD=smoke",
        "-e",
        "POSTGRES_HOST_AUTH_METHOD=trust",
        "-e",
        "PGOKF_ADMIN_PASSWORD=admin-pw",
        "-e",
        "PGOKF_WRITER_PASSWORD=writer-pw",
        "-e",
        "PGOKF_READER_PASSWORD=reader-pw",
        "-e",
        "PGOKF_POLICY={\"embedding_dim\":1024,\"allowed_roots\":[\"/bundles\"],\"store_source\":true}",
        "--",
        "postgres",
        "-c",
    ]);
    options.push(format!("shared_preload_libraries={}", preload.join(",")));
    options.extend(args(&["-c", "cron.database_name=postgres"]));
    let main = create(docker, evidence, run, "main", image, &options, owned)?;
    call(docker, &["start", &main])?;
    wait_sql(
        docker,
        &main,
        "SELECT extversion FROM pg_extension WHERE extname = 'pgokf'",
        version,
    )?;
    report.assertions.push("extension version".into());
    wait_sql(
        docker,
        &main,
        "SELECT pgokf.get_config() ->> 'embedding_dim'",
        "1024",
    )?;
    report.assertions.push("policy initialization".into());
    equal(
        &mut report,
        "allowed_roots policy",
        &sql(
            docker,
            &main,
            "postgres",
            "SELECT pgokf.get_config() -> 'allowed_roots'",
        )?,
        "[\"/bundles\"]",
    )?;
    let available = sql(
        docker,
        &main,
        "postgres",
        "SELECT string_agg(name, ',' ORDER BY name) FROM pg_available_extensions WHERE name IN ('pg_cron','pg_search','pg_textsearch','vector')",
    )?;
    equal(
        &mut report,
        "optional extensions initialized",
        &sql(
            docker,
            &main,
            "postgres",
            "SELECT string_agg(extname, ',' ORDER BY extname) FROM pg_extension WHERE extname IN ('pg_cron','pg_search','pg_textsearch','vector')",
        )?,
        &available,
    )?;
    let extensions: Vec<_> = available.split(',').collect();
    let major = sql(
        docker,
        &main,
        "postgres",
        "SELECT pg_catalog.current_setting('server_version_num')::int / 10000",
    )?;
    validate_optional(&extensions, &major, optional)?;
    report.assertions.push("optional provider coverage".into());
    equal(
        &mut report,
        "env-driven login roles",
        &sql(
            docker,
            &main,
            "postgres",
            "SELECT string_agg(r.rolname || ':' || t.rolname, ',' ORDER BY r.rolname) FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.member JOIN pg_roles t ON t.oid = m.roleid WHERE r.rolname IN ('okf_admin','okf_writer','okf_reader') AND r.rolcanlogin",
        )?,
        "okf_admin:pgokf_admin,okf_reader:pgokf_reader,okf_writer:pgokf_writer",
    )?;
    call(docker, &["exec", &main, "mkdir", "-p", "/bundles"])?;
    let sample = root
        .join("examples/sample-bundle")
        .canonicalize()
        .map_err(|e| e.to_string())?;
    call(
        docker,
        &[
            "cp",
            &sample.to_string_lossy(),
            &format!("{main}:/bundles/sample"),
        ],
    )?;
    sql(
        docker,
        &main,
        "postgres",
        "SELECT * FROM pgokf.register_bundle('/bundles/sample', 'sample')",
    )?;
    let concepts = sql(
        docker,
        &main,
        "postgres",
        "SELECT count(*) FROM pgokf.concepts",
    )?;
    if concepts.parse::<u64>().map_err(|e| e.to_string())? == 0 {
        return Err("sample materialized zero concepts".into());
    }
    report.assertions.push("sample materialization".into());
    if extensions.contains(&"pg_cron") {
        let id = sql(
            docker,
            &main,
            "postgres",
            "SELECT min(id) FROM pgokf.bundles",
        )?
        .parse::<u64>()
        .map_err(|e| e.to_string())?;
        sql(
            docker,
            &main,
            "postgres",
            &format!("SELECT pgokf.schedule_refresh({id}, '0 3 * * *')"),
        )?;
        equal(
            &mut report,
            "cron tenant binding",
            &sql(
                docker,
                &main,
                "postgres",
                &format!("SELECT command FROM cron.job WHERE jobname = 'pgokf_refresh_{id}'"),
            )?,
            &format!(
                "SELECT pg_catalog.set_config('pgokf.tenant', 'default', false); SELECT pgokf.refresh_bundle({id})"
            ),
        )?;
        equal(
            &mut report,
            "cron removed",
            &sql(
                docker,
                &main,
                "postgres",
                &format!("SELECT pgokf.unschedule_refresh({id})"),
            )?,
            "t",
        )?;
    }
    equal(
        &mut report,
        "catalog health",
        &sql(docker, &main, "postgres", "SELECT pgokf.health() ->> 'ok'")?,
        "true",
    )?;
    if extensions.contains(&"pg_search") || extensions.contains(&"pg_textsearch") {
        sql(
            docker,
            &main,
            "postgres",
            "SELECT pgokf.set_config('search_backend', '\"bm25\"'::jsonb)",
        )?;
        equal(
            &mut report,
            "BM25 built",
            &sql(
                docker,
                &main,
                "postgres",
                "SELECT pgokf.rebuild_search_index()",
            )?,
            "t",
        )?;
        let hit = sql(
            docker,
            &main,
            "okf_reader",
            "SELECT concept_id FROM pgokf.concept_search('failover', NULL, 1)",
        )?;
        if hit.is_empty() {
            return Err("BM25 reader search returned no hit".into());
        }
        report.assertions.push("BM25 reader search".into());
        sql(
            docker,
            &main,
            "postgres",
            "SELECT pgokf.set_config('search_backend', '\"native\"'::jsonb)",
        )?;
    }
    equal(
        &mut report,
        "pgvector visibility",
        &sql(
            docker,
            &main,
            "postgres",
            "SELECT pgokf.search_index_status() -> 'embedding' ->> 'pgvector_available'",
        )?,
        if optional { "true" } else { "false" },
    )?;
    sql(
        docker,
        &main,
        "postgres",
        "SELECT pgokf.set_config('embedding_dim', '768'::jsonb)",
    )?;
    call(
        docker,
        &[
            "exec",
            "-e",
            "PGUSER=postgres",
            "-e",
            "PGDATABASE=postgres",
            "-e",
            "PGOKF_BACKUP_DIR=/tmp/smoke-backups",
            &main,
            "pgokf-backup",
        ],
    )?;
    let dumps = call(
        docker,
        &[
            "exec",
            &main,
            "find",
            "/tmp/smoke-backups",
            "-maxdepth",
            "1",
            "-type",
            "f",
            "-name",
            "postgres-*.dump",
        ],
    )?;
    let dumps: Vec<_> = dumps.lines().collect();
    if dumps.len() != 1 {
        return Err("backup must contain exactly one archive".into());
    }
    let contents = call(docker, &["exec", &main, "pg_restore", "--list", dumps[0]])?;
    if !contents.contains("TABLE DATA pgokf concepts ") {
        return Err("backup omits concept table data".into());
    }
    report
        .assertions
        .push("backup contains catalog data".into());
    let archive: PathBuf = evidence.join("restore.dump");
    call(
        docker,
        &[
            "cp",
            &format!("{main}:{}", dumps[0]),
            &archive.to_string_lossy(),
        ],
    )?;
    let restore = create(docker, evidence, run, "restore", image, &options, owned)?;
    call(docker, &["start", &restore])?;
    wait_sql(
        docker,
        &restore,
        "SELECT pgokf.get_config() ->> 'embedding_dim'",
        "1024",
    )?;
    call(
        docker,
        &[
            "cp",
            &archive.to_string_lossy(),
            &format!("{restore}:/tmp/restore.dump"),
        ],
    )?;
    call(
        docker,
        &[
            "exec",
            "-e",
            "PGUSER=postgres",
            "-e",
            "PGDATABASE=postgres",
            &restore,
            "pgokf-restore",
            "/tmp/restore.dump",
        ],
    )?;
    equal(
        &mut report,
        "restored concept count",
        &sql(
            docker,
            &restore,
            "postgres",
            "SELECT count(*) FROM pgokf.concepts",
        )?,
        &concepts,
    )?;
    equal(
        &mut report,
        "restored dumped policy",
        &sql(
            docker,
            &restore,
            "postgres",
            "SELECT pgokf.get_config() ->> 'embedding_dim'",
        )?,
        "768",
    )?;
    equal(
        &mut report,
        "restored health",
        &sql(
            docker,
            &restore,
            "postgres",
            "SELECT pgokf.health() ->> 'ok'",
        )?,
        "true",
    )?;
    report.containers = owned.iter().map(|(_, p)| p.container.id.clone()).collect();
    Ok(report)
}
fn validate_optional(extensions: &[&str], major: &str, optional: bool) -> Result<(), String> {
    if optional && (!extensions.contains(&"vector") || !extensions.contains(&"pg_cron")) {
        return Err("image lacks required optional extensions".into());
    }
    if optional
        && matches!(major, "17" | "18")
        && !extensions.contains(&"pg_textsearch")
        && !extensions.contains(&"pg_search")
    {
        return Err("image lacks required BM25 provider".into());
    }
    Ok(())
}
/// Exercise every companion executable, early invalid-interval refusal and the
/// image's unprivileged UID. All containers have durable ownership receipts.
/// # Errors
/// Setup errors cannot substitute for the intended interval refusal.
pub fn companions(docker: &mut impl DockerApi, evidence: &Path, image: &str) -> Result<(), String> {
    std::fs::create_dir(evidence).map_err(|e| e.to_string())?;
    let evidence = evidence.canonicalize().map_err(|e| e.to_string())?;
    let run = uuid::Uuid::new_v4().simple().to_string();
    let mut owned = Vec::new();
    let result = (|| {
        for binary in ["pgokf-ingest", "pgokf-embed", "pgokf-mcp", "pgokf-web"] {
            let id = create(
                docker,
                &evidence,
                &run,
                binary,
                image,
                &args(&["--", binary, "--help"]),
                &mut owned,
            )?;
            let help = call(docker, &["start", "-a", &id])?;
            if !help.to_ascii_lowercase().contains("usage") {
                return Err(format!("{binary}: help output missing"));
            }
        }
        let id = create(
            docker,
            &evidence,
            &run,
            "interval",
            image,
            &args(&[
                "-e",
                "OKF_PG_URL=postgresql://x@localhost/x",
                "-e",
                "OKF_EMBED_ENDPOINT=http://localhost",
                "-e",
                "OKF_EMBED_MODEL=m",
                "--",
                "pgokf-embed",
                "--watch",
                "--interval",
                "0",
            ]),
            &mut owned,
        )?;
        match call(docker, &["start", "-a", &id]) {
            Err(error) if error.contains("--interval must be at least 1 second") => {}
            other => return Err(format!("expected semantic interval refusal, got {other:?}")),
        }
        let id = create(
            docker,
            &evidence,
            &run,
            "uid",
            image,
            &args(&["--entrypoint", "id", "--", "-u"]),
            &mut owned,
        )?;
        let uid = call(docker, &["start", "-a", &id])?;
        if uid != "10001" {
            return Err(format!("unprivileged UID mismatch: {uid}"));
        }
        Ok(())
    })();
    let mut failures = Vec::new();
    for (label, proof) in owned {
        if let Err(error) = owned_docker::cleanup(
            docker,
            &mut DurableStore,
            &proof.container.id,
            "pgokf.smoke-run",
            &run,
            &evidence.join(format!("{label}.cleanup.json")),
            Some(&proof),
        ) {
            failures.push(error);
        }
    }
    if !failures.is_empty() {
        return Err(format!("companions {result:?}; cleanup {failures:?}"));
    }
    result?;
    owned_docker::publish(
        &evidence.join("companions.json"),
        &serde_json::json!({"image":image,"help_commands":4,"interval_refusal":true,"uid":10001}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initialization_requires_exact_observation_and_expires() {
        assert_eq!(
            wait_outcome(Some("ready"), "ready", Duration::ZERO),
            WaitOutcome::Ready
        );
        for actual in [None, Some("wrong")] {
            assert_eq!(
                wait_outcome(actual, "ready", Duration::ZERO),
                WaitOutcome::Retry
            );
            assert_eq!(
                wait_outcome(actual, "ready", Duration::from_mins(2)),
                WaitOutcome::Expired
            );
        }
    }
    #[test]
    fn optional_coverage_is_provider_and_major_aware() {
        for provider in ["pg_search", "pg_textsearch"] {
            validate_optional(&["vector", "pg_cron", provider], "18", true).unwrap();
        }
        validate_optional(&["vector", "pg_cron"], "16", true).unwrap();
        validate_optional(&[], "19", false).unwrap();
        for e in [
            vec![],
            vec!["vector"],
            vec!["pg_cron"],
            vec!["vector", "pg_cron"],
            vec!["pg_textsearch"],
            vec!["pg_search", "vector"],
            vec!["pg_textsearch", "pg_cron"],
        ] {
            assert!(validate_optional(&e, "18", true).is_err());
        }
    }
    #[test]
    fn semantic_assertions_do_not_accept_changed_values() {
        let mut r = SmokeReport {
            image: String::new(),
            version: String::new(),
            optional: false,
            assertions: vec![],
            containers: vec![],
        };
        assert!(equal(&mut r, "policy", "1024", "768").is_err());
        assert!(r.assertions.is_empty());
        equal(&mut r, "policy", "768", "768").unwrap();
        assert_eq!(r.assertions, ["policy"]);
    }
}
