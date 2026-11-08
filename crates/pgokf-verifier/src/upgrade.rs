// SPDX-License-Identifier: AGPL-3.0-only
//! Isolated native `PostgreSQL` upgrade and reconciliation verification.
use crate::{catalog, json, process::CommandSpec};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Write as _;
use std::io::Write as _;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Probes {
    pub adopt: Vec<String>,
    pub retired: Vec<String>,
    pub helper: String,
    pub semantic: String,
    pub hybrid: String,
    pub reconciled: Vec<String>,
    pub defaulted_calls: Vec<String>,
    pub web_fingerprint: String,
    pub web_snapshot: String,
    pub attempt: String,
    pub constraint_probes: Vec<(String, String)>,
    pub reader_probes: Vec<String>,
    pub live_drift_manifest: Vec<(String, String)>,
    pub legacy_order: String,
    pub appended: Vec<(String, String)>,
    pub exception_probes: BTreeMap<String, String>,
    pub skill_hex: String,
    pub refusals: Vec<(String, String, String, Option<String>)>,
    pub function_metadata: String,
    pub probes: BTreeMap<String, String>,
    pub definition_probes: BTreeMap<String, (String, String)>,
}
/// # Errors
/// Rejects invalid embedded probe metadata.
pub fn probes() -> Result<Probes, String> {
    json::from_slice(include_bytes!("../fixtures/upgrade-probes.json"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub comment: String,
    pub statements: Vec<Statement>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Statement {
    pub sql: String,
    pub runs: Vec<Run>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub args: Vec<Value>,
    #[serde(default = "success_state")]
    pub sqlstate: String,
}
fn success_state() -> String {
    "00000".into()
}
#[must_use]
pub fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn arguments(args: &[Value]) -> Result<String, String> {
    if args.is_empty() {
        return Ok(String::new());
    }
    let args = args
        .iter()
        .map(|a| match a {
            Value::Null => Ok("NULL".into()),
            Value::Bool(b) => Ok(b.to_string()),
            Value::Number(n) => Ok(n.to_string()),
            Value::String(s) => Ok(literal(s)),
            _ => Err("SQL arguments must be scalar".to_string()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!("({})", args.join(", ")))
}
/// # Errors
/// Rejects empty runs or unsupported parameter values.
pub fn behaviour_script(registry: &Registry, p: &Probes) -> Result<String, String> {
    validate_registry(registry)?;
    let mut lines = vec![
        "\\set ON_ERROR_STOP 0".into(),
        "\\set ON_ERROR_ROLLBACK on".into(),
        "BEGIN;".into(),
        p.attempt.clone(),
        "ANALYZE pgokf_web.users;".into(),
        "ANALYZE pgokf_web.sessions;".into(),
        "SET LOCAL ROLE pgokf_writer;".into(),
        p.web_snapshot.clone(),
    ];
    for (i, s) in registry.statements.iter().enumerate() {
        if s.runs.is_empty() {
            return Err("statement has no argument runs".into());
        }
        lines.push(format!("PREPARE product_{i} AS {};", s.sql));
        lines.push(format!("\\echo prepare {i} :SQLSTATE"));
        for (n, run) in s.runs.iter().enumerate() {
            lines.push(format!("EXECUTE product_{i}{};", arguments(&run.args)?));
            lines.push(format!("\\echo statement {i} run {n} :SQLSTATE"));
            if !s.sql.starts_with("SELECT") {
                lines.push(p.web_snapshot.clone());
            }
        }
    }
    lines.push("SET LOCAL enable_seqscan = off;".into());
    for (i, s) in registry
        .statements
        .iter()
        .enumerate()
        .filter(|(_, s)| s.sql.starts_with("SELECT"))
    {
        lines.push(format!(
            "EXPLAIN (COSTS OFF) EXECUTE product_{i}{};",
            arguments(&s.runs[0].args)?
        ));
    }
    lines.push("RESET enable_seqscan;".into());
    for (label, s) in &p.constraint_probes {
        lines.push(format!(
            "SELECT {}, pg_temp.attempt({});",
            literal(&format!("constraint {label}")),
            literal(s)
        ));
    }
    lines.push(p.web_snapshot.clone());
    lines.push("SET LOCAL ROLE pgokf_reader;".into());
    for s in &p.reader_probes {
        lines.push(format!("SELECT 'reader', pg_temp.attempt({});", literal(s)));
    }
    lines.push("ROLLBACK;".into());
    Ok(lines.join("\n") + "\n")
}
/// # Errors
/// Every preparation and argument run must appear once in exact registry order.
pub fn check_behaviour(transcript: &str, registry: &Registry) -> Result<(), String> {
    let observed: Vec<_> = transcript
        .lines()
        .filter(|l| l.starts_with("prepare ") || l.starts_with("statement "))
        .map(str::to_owned)
        .collect();
    let mut expected = vec![];
    for (i, s) in registry.statements.iter().enumerate() {
        expected.push(format!("prepare {i} 00000"));
        for (n, run) in s.runs.iter().enumerate() {
            expected.push(format!("statement {i} run {n} {}", run.sqlstate));
        }
    }
    if observed != expected {
        return Err("product statements did not run as declared".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Exceptions,
    Manifest,
    Upgrade,
    OtherSuperuser,
    RefusalSymbol,
    RefusalAcl,
    RefusalDependents,
}
#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub pg_config: PathBuf,
    pub preload: Option<String>,
    pub extension_prefix: Option<PathBuf>,
    pub operator_sql: Option<PathBuf>,
    pub reconciliation_only: bool,
    pub docker_image: Option<String>,
    pub historical_install: Option<PathBuf>,
    pub provider_package: Option<PathBuf>,
    pub probe: Option<Probe>,
    pub embedding_policy: catalog::EmbeddingPolicy,
    pub embedding_dimensions: Option<u32>,
}
#[derive(Debug, Serialize, Default)]
pub struct Report {
    pub checks: Vec<String>,
}
impl Report {
    pub fn passed(&mut self, label: impl Into<String>) {
        let label = label.into();
        println!("{label}: passed");
        self.checks.push(label);
    }
}

pub struct Cluster {
    pub bindir: PathBuf,
    service: Option<std::cell::RefCell<crate::service::Service>>,
    work: tempfile::TempDir,
    started: bool,
    docker: Option<crate::ownership::CreationReceipt>,
    receipts: PathBuf,
}
impl Cluster {
    /// # Errors
    /// Requires installed `PostgreSQL` binaries and a private local socket.
    pub fn start(options: &Options) -> Result<Self, String> {
        if options.docker_image.is_some() {
            return Self::docker_start(options);
        }
        let mut config = CommandSpec::new(&options.pg_config);
        config.args = vec!["--bindir".into()];
        let bindir = PathBuf::from(text(config.checked()?.stdout)?);
        // Reserve the Unix socket pathname budget for the caller's root.
        let work = crate::scratch::directory("p")?;
        let mut cluster = Self {
            bindir,
            service: None,
            work,
            started: false,
            docker: None,
            // Receipts must survive the cluster's temporary data directory.
            receipts: crate::scratch::directory("pgokf-parity-receipts-")?.keep(),
        };
        cluster
            .command(
                "initdb",
                &[
                    "-D".into(),
                    cluster.data().into_os_string(),
                    "-U".into(),
                    "postgres".into(),
                    "--auth=trust".into(),
                    "--no-locale".into(),
                ],
            )?
            .checked()?;
        let mut conf = format!(
            "\nlisten_addresses = ''\nunix_socket_directories = {}\n",
            literal(cluster.work.path().to_str().ok_or("non-UTF8 socket path")?)
        );
        if let Some(provider) = &options.preload {
            if !["pg_search", "pg_textsearch"].contains(&provider.as_str()) {
                return Err("unsupported provider".into());
            }
            writeln!(conf, "shared_preload_libraries = {}", literal(provider))
                .map_err(|e| e.to_string())?;
        }
        if let Some(prefix) = &options.extension_prefix {
            writeln!(
                conf,
                "extension_control_path = {}",
                literal(&format!("{}:$system", prefix.display()))
            )
            .map_err(|e| e.to_string())?;
        }
        let mut configuration = fs::OpenOptions::new()
            .append(true)
            .open(cluster.data().join("postgresql.conf"))
            .map_err(|e| e.to_string())?;
        configuration
            .write_all(conf.as_bytes())
            .map_err(|e| e.to_string())?;
        configuration.sync_all().map_err(|e| e.to_string())?;
        fs::write(cluster.work.path().join("ownership.json"),serde_json::to_vec(&serde_json::json!({"data":cluster.data(),"socket":cluster.work.path(),"purpose":"upgrade-verification"})).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?;
        cluster.started = true;
        let mut command =
            cluster.command("postgres", &["-D".into(), cluster.data().into_os_string()])?;
        command.timeout = Duration::from_hours(1);
        cluster.service = Some(std::cell::RefCell::new(crate::service::Service::spawn(
            &command,
            &cluster.receipts.join("service.json"),
        )?));
        let deadline = std::time::Instant::now() + Duration::from_mins(1);
        loop {
            if cluster.sql("postgres", "SELECT 1;").is_ok() {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err("owned PostgreSQL startup timed out".into());
            }
            cluster
                .service
                .as_mut()
                .ok_or("service missing")?
                .borrow_mut()
                .check()?;
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(cluster)
    }
    fn data(&self) -> PathBuf {
        self.work.path().join("data")
    }
    #[allow(
        clippy::unnecessary_wraps,
        reason = "Shared command construction remains fallible at the transport boundary"
    )]
    fn command(&self, name: &str, args: &[std::ffi::OsString]) -> Result<CommandSpec, String> {
        let mut command = CommandSpec::new(self.bindir.join(name));
        command.args = args.to_vec();
        command.timeout = Duration::from_mins(2);
        for (name, _) in std::env::vars_os().filter(|(n, _)| n.to_string_lossy().starts_with("PG"))
        {
            command.env_remove.push(name);
        }
        command.env.insert("LC_ALL".into(), "C".into());
        Ok(command)
    }
    pub(crate) fn sql_command(
        &self,
        db: &str,
        statement: &str,
        user: &str,
    ) -> Result<CommandSpec, String> {
        if let Some(service) = &self.service {
            service.borrow_mut().check()?;
        }
        let mut command = if let Some(proof) = &self.docker {
            let mut command = CommandSpec::new("docker");
            command.args = [
                "exec",
                "-i",
                &proof.container.id,
                "psql",
                "-X",
                "-qAt",
                "-v",
                "ON_ERROR_STOP=1",
                "-U",
                user,
                "-d",
                db,
            ]
            .into_iter()
            .map(Into::into)
            .collect();
            command
        } else {
            self.command(
                "psql",
                &[
                    "-X".into(),
                    "-qAt".into(),
                    "-v".into(),
                    "ON_ERROR_STOP=1".into(),
                    "-h".into(),
                    self.work.path().as_os_str().into(),
                    "-U".into(),
                    user.into(),
                    "-d".into(),
                    db.into(),
                ],
            )?
        };
        command.stdin = statement.as_bytes().to_vec();
        Ok(command)
    }
    pub(crate) fn sql_as(&self, db: &str, statement: &str, user: &str) -> Result<String, String> {
        text(self.sql_command(db, statement, user)?.checked()?.stdout)
    }
    pub(crate) fn sql(&self, db: &str, statement: &str) -> Result<String, String> {
        self.sql_as(db, statement, "postgres")
    }
    pub(crate) fn drop_catalog(&self, db: &str) -> Result<(), String> {
        if !db
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err("invalid database identifier".into());
        }
        self.sql("postgres", &format!("DROP DATABASE IF EXISTS {db};"))?;
        self.sql(
            "postgres",
            "DROP ROLE IF EXISTS pgokf_admin, pgokf_writer, pgokf_reader, pgokf_dispatcher;",
        )?;
        Ok(())
    }
}
impl Cluster {
    fn close(&mut self) -> Result<(), String> {
        if let Some(proof) = &self.docker {
            use crate::owned_docker::{Docker, DurableStore, cleanup};
            let key = "pgokf.parity-run";
            cleanup(
                &mut Docker::default(),
                &mut DurableStore,
                &proof.container.id,
                key,
                &proof.ownership_label[key],
                &self.receipts.join("delete.json"),
                Some(proof),
            )?;
            self.docker = None;
            return Ok(());
        }
        if self.started {
            let stopped = self
                .command(
                    "pg_ctl",
                    &[
                        "-D".into(),
                        self.data().into_os_string(),
                        "-m".into(),
                        "immediate".into(),
                        "-w".into(),
                        "stop".into(),
                    ],
                )
                .and_then(|c| c.checked().map(|_| ()));
            let reaped = self
                .service
                .as_ref()
                .map_or(Ok(()), |service| service.borrow_mut().stop());
            stopped.and(reaped)?;
            self.started = false;
        }
        Ok(())
    }
}
impl Drop for Cluster {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            eprintln!(
                "owned cluster cleanup failed: {error}; retained at {}",
                self.work.path().display()
            );
            self.work.disable_cleanup(true);
        }
    }
}

fn text(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes)
        .map(|s| s.trim().into())
        .map_err(|e| e.to_string())
}
pub(crate) fn equal(expected: &str, actual: &str, label: &str) -> Result<(), String> {
    if expected == actual {
        Ok(())
    } else {
        Err(format!("{label}: values differ"))
    }
}
pub(crate) fn read(path: impl AsRef<Path>) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| e.to_string())
}
/// # Errors
/// Inventories and all historical values must be present and byte-identical.
pub fn fingerprint(cluster: &Cluster, db: &str) -> Result<String, String> {
    let columns:BTreeMap<String,String>=json::from_slice(cluster.sql(db,"SELECT json_object_agg(table_name, cols ORDER BY table_name) FROM (SELECT c.table_name, string_agg(quote_ident(c.column_name), ',' ORDER BY c.ordinal_position) cols FROM information_schema.columns c WHERE c.table_schema='pgokf' AND c.table_name IN ('bundles','concepts','concept_source','concept_metadata') GROUP BY c.table_name) s;")?.as_bytes())?;
    if columns.len() != 4 {
        return Err("seed fingerprint requires all four historical tables".into());
    }
    Ok(format!(
        "SELECT * FROM ({}) rows ORDER BY 1;",
        columns
            .iter()
            .map(|(table, cols)| format!(
                "SELECT '{table}:' || row_to_json(r)::text FROM (SELECT {cols} FROM pgokf.{table}) r"
            ))
            .collect::<Vec<_>>()
            .join(" UNION ALL ")
    ))
}

#[derive(Debug, PartialEq)]
struct SeedSnapshot(BTreeMap<String, Vec<Value>>);
impl SeedSnapshot {
    fn parse(text: &str) -> Result<Self, String> {
        let mut tables: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        let mut rows = std::collections::BTreeSet::new();
        for line in text.lines() {
            let (table, row) = line
                .split_once(':')
                .ok_or("seed row lacks table identity")?;
            let row: Value = json::from_slice(row.as_bytes())?;
            if !row.is_object() {
                return Err("seed row must be an object".into());
            }
            if !rows.insert((table.to_owned(), row.to_string())) {
                return Err("duplicate seed row".into());
            }
            tables.entry(table.into()).or_default().push(row);
        }
        if tables.keys().map(String::as_str).collect::<Vec<_>>()
            != ["bundles", "concept_metadata", "concept_source", "concepts"]
        {
            return Err("seed table coverage differs".into());
        }
        for rows in tables.values_mut() {
            rows.sort_by_key(Value::to_string);
        }
        Ok(Self(tables))
    }
}
fn verify_seed(before: &str, after: &str) -> Result<(), String> {
    if SeedSnapshot::parse(before)? != SeedSnapshot::parse(after)? {
        return Err("seed preservation: values differ".into());
    }
    Ok(())
}

/// Run the committed installer with restoration on every returned error.
/// # Errors
/// Propagates database, verification, cleanup and restoration failures.
pub fn check_committed_install(
    mut sql: impl FnMut(&str, &str) -> Result<String, String>,
    installed: &Path,
    committed: &Path,
    expected: &str,
    inventory: &str,
    version: &str,
) -> Result<(), String> {
    let backup = fs::read(installed).map_err(|e| e.to_string())?;
    let replacement = fs::read(committed).map_err(|e| e.to_string())?;
    let work = (|| {
        fs::write(installed, replacement).map_err(|e| e.to_string())?;
        sql("postgres", "CREATE DATABASE fresh_committed;")?;
        sql("fresh_committed", "CREATE EXTENSION pgokf;")?;
        equal(
            expected,
            &sql("fresh_committed", inventory)?,
            "committed install catalog",
        )?;
        equal(
            version,
            &sql("fresh_committed", "SELECT pgokf.version();")?,
            "committed install version",
        )
    })();
    let cleanup = (|| {
        sql("postgres", "DROP DATABASE IF EXISTS fresh_committed;")?;
        sql(
            "postgres",
            "DROP ROLE IF EXISTS pgokf_admin, pgokf_writer, pgokf_reader, pgokf_dispatcher;",
        )?;
        Ok(())
    })();
    let restore = fs::write(installed, backup).map_err(|e| e.to_string());
    work.and(cleanup).and(restore)
}

/// # Errors
/// Runs the full requested gate; a missing prerequisite is a failure.
#[allow(
    clippy::too_many_lines,
    reason = "Sequential preflight and parity evidence ordering"
)]
pub fn run(options: &Options) -> Result<Report, String> {
    if let Some(path) = &options.historical_install {
        crate::historical::validate(&fs::read(path).map_err(|e| e.to_string())?)?;
    }
    let p = probes()?;
    catalog::check_exceptions(&read(
        options.root.join("tests/upgrade/catalog-exceptions.json"),
    )?)?;
    let version = crate::release::compatibility_source(&options.root)?;
    let mut cluster = Cluster::start(options)?;
    let sharedir = cluster.external("pg_config", &["--sharedir"])?;
    let extension = options
        .extension_prefix
        .as_ref()
        .map_or(PathBuf::from(sharedir.trim()).join("extension"), |p| {
            p.join("extension")
        });
    equal(
        &read(options.root.join("crates/extension/pgokf.control"))?,
        &cluster.file(&extension.join("pgokf.control"))?,
        "installed control",
    )?;
    for entry in
        fs::read_dir(options.root.join("crates/extension/sql")).map_err(|e| e.to_string())?
    {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("pgokf--") && name.matches("--").count() == 2 && name.ends_with(".sql")
        {
            equal(
                &read(entry.path())?,
                &cluster.file(&extension.join(name.as_ref()))?,
                "installed migration",
            )?;
        }
    }
    if let Some(historical) = &options.historical_install {
        let proof = cluster
            .docker
            .as_ref()
            .ok_or("historical install override requires isolated Docker")?;
        let mut command = CommandSpec::new("docker");
        command.args = vec![
            "exec".into(),
            "-i".into(),
            proof.container.id.clone().into(),
            "tee".into(),
            extension.join("pgokf--0.2.0.sql").into_os_string(),
        ];
        command.stdin = fs::read(historical).map_err(|e| e.to_string())?;
        command.checked()?;
    }
    let historical = cluster.file(&extension.join("pgokf--0.2.0.sql"))?;
    crate::historical::validate(historical.as_bytes())?;
    let mut report = Report::default();
    if let Some(provider) = &options.preload {
        equal(
            "t",
            &cluster.sql(
                "postgres",
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM pg_available_extensions WHERE name={});",
                    literal(provider)
                ),
            )?,
            "provider availability",
        )?;
    }
    let inventory = read(options.root.join("tests/upgrade/inventory.sql"))?;
    let seed = read(options.root.join("tests/upgrade/seed.sql"))?;
    cluster.sql("postgres", "CREATE DATABASE fresh;")?;
    cluster.sql("fresh", "CREATE EXTENSION pgokf;")?;
    validate_stage(
        &cluster,
        "fresh",
        &inventory,
        catalog::EmbeddingPolicy::NotRequested,
    )?;
    let expected = cluster.sql("fresh", &inventory)?;
    let metadata = cluster.sql("fresh", &p.function_metadata)?;
    equal(
        &version,
        &cluster.sql("fresh", "SELECT pgokf.version();")?,
        "library version",
    )?;
    cluster.drop_catalog("fresh")?;
    report.passed(format!("fresh {version}"));
    let committed = options
        .root
        .join(format!("crates/extension/sql/pgokf--{version}.sql"));
    if committed.is_file() && options.docker_image.is_none() {
        check_committed_install(
            |d, s| cluster.sql(d, s),
            &extension.join(committed.file_name().ok_or("install filename")?),
            &committed,
            &expected,
            &inventory,
            &version,
        )?;
        report.passed("committed installer");
    }
    crate::reconciliation::run(
        &cluster,
        options,
        &p,
        &expected,
        &metadata,
        &inventory,
        &seed,
        &version,
        &mut report,
    )?;
    if !options.reconciliation_only && options.probe.is_none() {
        routes(
            &cluster,
            &p,
            &expected,
            &inventory,
            &seed,
            &version,
            &mut report,
        )?;
    }
    cluster.close()?;
    Ok(report)
}
#[allow(
    clippy::too_many_arguments,
    reason = "Explicit historical and fresh evidence inputs"
)]
fn invariants(
    c: &Cluster,
    db: &str,
    before: &str,
    fp: &str,
    expected: &str,
    inventory: &str,
    version: &str,
    declared: bool,
) -> Result<(), String> {
    equal(
        version,
        &c.sql(
            db,
            "SELECT extversion FROM pg_extension WHERE extname='pgokf';",
        )?,
        "upgrade target",
    )?;
    verify_seed(before, &c.sql(db, fp)?)?;
    validate_stage(c, db, inventory, catalog::EmbeddingPolicy::NotRequested)?;
    equal("stale:legacy_pre_0.3.0",&c.sql(db,"SELECT state || ':' || array_to_string(reason_codes, ',') FROM pgokf.bundle_freshness;")?,"legacy freshness")?;
    catalog::compare_inventory(expected, &c.sql(db, inventory)?, declared)?;
    equal(
        "t",
        &c.sql(
            db,
            "SELECT last_reconciled_at IS NULL FROM pgokf.bundle_freshness;",
        )?,
        "reconciliation evidence",
    )?;
    c.sql(db,"SELECT pgokf.register_bundle_content('upgrade-fixture', ARRAY['entry.md'], ARRAY[convert_to('---\ntype: Concept\ntitle: Refreshed fixture\n---\nBody', 'UTF8')]);")?;
    equal(
        "stale",
        &c.sql(db, "SELECT state FROM pgokf.bundle_freshness;")?,
        "refresh freshness",
    )?;
    equal(
        "t",
        &c.sql(
            db,
            "SELECT last_reconciled_at IS NULL FROM pgokf.bundle_freshness;",
        )?,
        "refresh evidence",
    )
}
pub(crate) fn check_invariants(
    c: &Cluster,
    db: &str,
    before: &str,
    fp: &str,
    expected: &str,
    inventory: &str,
    version: &str,
) -> Result<(), String> {
    invariants(c, db, before, fp, expected, inventory, version, true)
}
#[allow(
    clippy::too_many_lines,
    reason = "Sequential legacy route registry kept together for coverage review"
)]
fn routes(
    c: &Cluster,
    p: &Probes,
    expected: &str,
    inventory: &str,
    seed: &str,
    version: &str,
    report: &mut Report,
) -> Result<(), String> {
    for point in [
        "0.3.0-dev",
        "0.3.0-dev1",
        "0.3.0-dev2",
        "0.3.0-dev3",
        "0.3.0",
        "0.3.1",
        "0.4.0-dev1",
        "0.4.0-dev2",
        "0.4.0-dev3",
    ] {
        c.sql("postgres", "CREATE DATABASE point;")?;
        c.sql(
            "point",
            &format!("CREATE EXTENSION pgokf VERSION '0.2.0';{seed}"),
        )?;
        let fp = fingerprint(c, "point")?;
        let before = c.sql("point", &fp)?;
        c.sql(
            "point",
            &format!("ALTER EXTENSION pgokf UPDATE TO {};", literal(point)),
        )?;
        equal(
            point,
            &c.sql(
                "point",
                "SELECT extversion FROM pg_extension WHERE extname='pgokf';",
            )?,
            "point reached",
        )?;
        c.sql("point", "ALTER EXTENSION pgokf UPDATE;")?;
        invariants(
            c, "point", &before, &fp, expected, inventory, version, false,
        )?;
        c.drop_catalog("point")?;
        report.passed(format!("point {point}"));
    }
    for route in [
        "normal",
        "missing",
        "unowned",
        "body",
        "member_body",
        "acl_order",
    ] {
        c.sql("postgres", "CREATE DATABASE adoption;")?;
        c.sql(
            "adoption",
            &format!("CREATE EXTENSION pgokf VERSION '0.2.0';{seed}"),
        )?;
        let fp = fingerprint(c, "adoption")?;
        let before = c.sql("adoption", &fp)?;
        c.sql("adoption", "ALTER EXTENSION pgokf UPDATE TO '0.3.0-dev';")?;
        for signature in &p.adopt {
            if ["missing", "unowned", "body"].contains(&route) {
                c.sql(
                    "adoption",
                    &format!("ALTER EXTENSION pgokf DROP FUNCTION {signature};"),
                )?;
            }
            if route == "acl_order" {
                c.sql("adoption",&format!("REVOKE EXECUTE ON FUNCTION {signature} FROM postgres; GRANT EXECUTE ON FUNCTION {signature} TO postgres;"))?;
            }
            if ["body", "member_body"].contains(&route) {
                let definition = c.sql(
                    "adoption",
                    &format!(
                        "SELECT pg_get_functiondef({}::regprocedure);",
                        literal(signature)
                    ),
                )?;
                let (prefix, _) = definition
                    .split_once("AS $function$")
                    .ok_or("function body delimiter absent")?;
                c.sql("adoption",&format!("{prefix} AS $function$ BEGIN RAISE EXCEPTION 'hostile body'; END $function$;"))?;
            }
            if route == "missing" {
                c.sql("adoption", &format!("DROP FUNCTION {signature};"))?;
            }
        }
        c.sql("adoption", "ALTER EXTENSION pgokf UPDATE;")?;
        invariants(
            c, "adoption", &before, &fp, expected, inventory, version, false,
        )?;
        c.drop_catalog("adoption")?;
        report.passed(format!("adoption {route}"));
    }
    for (i, signature) in p.adopt.iter().enumerate() {
        let role = if i == 0 {
            "pgokf_reader"
        } else {
            "pgokf_admin"
        };
        let mut cases = vec![
            (
                "owner",
                format!("ALTER FUNCTION {signature} OWNER TO pgokf_reader;"),
            ),
            (
                "writer",
                format!("GRANT EXECUTE ON FUNCTION {signature} TO pgokf_writer;"),
            ),
            (
                "other",
                format!("GRANT EXECUTE ON FUNCTION {signature} TO hostile_other;"),
            ),
            (
                "missing",
                format!("REVOKE EXECUTE ON FUNCTION {signature} FROM {role};"),
            ),
            (
                "grant_option",
                format!("GRANT EXECUTE ON FUNCTION {signature} TO {role} WITH GRANT OPTION;"),
            ),
            (
                "public",
                format!("GRANT EXECUTE ON FUNCTION {signature} TO PUBLIC;"),
            ),
        ];
        if i > 0 {
            cases.push((
                "reader",
                format!("GRANT EXECUTE ON FUNCTION {signature} TO pgokf_reader;"),
            ));
        }
        for detached in [false, true] {
            for (name, mutation) in &cases {
                c.sql(
                    "postgres",
                    "CREATE DATABASE hostile; CREATE ROLE hostile_other;",
                )?;
                c.sql("hostile","CREATE EXTENSION pgokf VERSION '0.2.0'; ALTER EXTENSION pgokf UPDATE TO '0.3.0-dev';")?;
                if detached {
                    c.sql(
                        "hostile",
                        &format!("ALTER EXTENSION pgokf DROP FUNCTION {signature};"),
                    )?;
                }
                c.sql("hostile", mutation)?;
                refusal(c, "hostile", inventory, "security metadata", "0.3.0-dev")?;
                c.drop_catalog("hostile")?;
                c.sql("postgres", "DROP ROLE hostile_other;")?;
                report.passed(format!("adoption refusal {i} {detached} {name}"));
            }
        }
        for conflict in ["return", "arguments", "other_extension"] {
            c.sql("postgres", "CREATE DATABASE incompatible;")?;
            c.sql("incompatible",&format!("CREATE EXTENSION pgokf VERSION '0.2.0'; ALTER EXTENSION pgokf UPDATE TO '0.3.0-dev'; ALTER EXTENSION pgokf DROP FUNCTION {signature};"))?;
            let diagnostic = if conflict == "other_extension" {
                c.sql(
                    "incompatible",
                    &format!("ALTER EXTENSION plpgsql ADD FUNCTION {signature};"),
                )?;
                "already a member of extension"
            } else {
                let declaration = if conflict == "return" {
                    format!("{signature} RETURNS integer LANGUAGE sql AS 'SELECT 1'")
                } else if i == 0 {
                    format!(
                        "{signature} RETURNS TABLE (hostile_id bigint, schedule text) LANGUAGE plpgsql AS 'BEGIN RETURN; END'"
                    )
                } else {
                    format!(
                        "{} RETURNS void LANGUAGE plpgsql AS 'BEGIN RETURN; END'",
                        signature.replace("(uuid,", "(hostile_id uuid,")
                    )
                };
                c.sql("incompatible",&format!("DROP FUNCTION {signature}; CREATE FUNCTION {declaration}; REVOKE ALL ON FUNCTION {signature} FROM PUBLIC; GRANT EXECUTE ON FUNCTION {signature} TO {role};"))?;
                if conflict == "arguments" && i > 0 {
                    "cannot change name of input parameter"
                } else {
                    "cannot change return type"
                }
            };
            refusal(c, "incompatible", inventory, diagnostic, "0.3.0-dev")?;
            c.drop_catalog("incompatible")?;
            report.passed(format!("signature refusal {i} {conflict}"));
        }
    }
    c.sql("postgres", "CREATE DATABASE fresh;")?;
    c.sql("fresh", "CREATE EXTENSION pgokf;")?;
    equal(expected, &c.sql("fresh", inventory)?, "repeat fresh")?;
    let orphan = "INSERT INTO pgokf.concepts (bundle_id,id,path,title,file_hash,body_text) VALUES (987654321,'orphan','orphan.md','orphan','hash','body');";
    let error = c
        .sql("fresh", &format!("BEGIN;{orphan}ROLLBACK;"))
        .err()
        .ok_or("canonical catalog permits an orphan")?;
    if !error.contains("foreign key constraint") {
        return Err(error);
    }
    equal("1",&c.sql("fresh",&format!("BEGIN; ALTER TABLE pgokf.concepts DISABLE TRIGGER ALL; {orphan} SELECT count(*) FROM pgokf.concepts WHERE bundle_id=987654321; ROLLBACK;"))?,"FK control")?;
    for (name, mutation) in &p.probes {
        let actual = c.sql(
            "fresh",
            &format!("BEGIN;{mutation}\n{inventory}\nROLLBACK;"),
        )?;
        if actual == expected {
            return Err(format!("catalog mutation survived: {name}"));
        }
        report.passed(format!("catalog mutation {name}"));
    }
    for (name, (setup, mutation)) in &p.definition_probes {
        let baseline = c.sql("fresh", &format!("BEGIN;{setup}\n{inventory}\nROLLBACK;"))?;
        let actual = c.sql(
            "fresh",
            &format!("BEGIN;{setup}\n{mutation}\n{inventory}\nROLLBACK;"),
        )?;
        if actual == baseline {
            return Err(format!("definition mutation survived: {name}"));
        }
        report.passed(format!("definition mutation {name}"));
    }
    equal(expected, &c.sql("fresh", inventory)?, "mutations rollback")?;
    c.drop_catalog("fresh")?;
    Ok(())
}
pub(crate) fn refusal(
    c: &Cluster,
    db: &str,
    inventory: &str,
    diagnostic: &str,
    version: &str,
) -> Result<(), String> {
    let before = c.sql(db, inventory)?;
    match c.sql(db, "ALTER EXTENSION pgokf UPDATE;") {
        Ok(_) => return Err(format!("unsafe acceptance: {diagnostic}")),
        Err(e) if e.contains(diagnostic) => (),
        Err(e) => return Err(format!("wrong refusal: {e}")),
    }
    equal(&before, &c.sql(db, inventory)?, "refusal atomicity")?;
    equal(
        version,
        &c.sql(
            db,
            "SELECT extversion FROM pg_extension WHERE extname='pgokf';",
        )?,
        "refusal version",
    )
}
/// # Errors
/// Rejects unknown options and failed verification.
pub fn cli(args: &[String]) -> Result<(), String> {
    cli_mode(args, None)
}
fn cli_mode(args: &[String], probe: Option<Probe>) -> Result<(), String> {
    let mut o = Options {
        root: std::env::current_dir().map_err(|e| e.to_string())?,
        pg_config: "pg_config".into(),
        preload: None,
        extension_prefix: None,
        operator_sql: None,
        reconciliation_only: false,
        docker_image: None,
        historical_install: None,
        provider_package: None,
        probe,
        embedding_policy: catalog::EmbeddingPolicy::NotRequested,
        embedding_dimensions: None,
    };
    let mut contract_output = None;
    let mut controls_output = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--controls-output" => {
                controls_output =
                    Some(PathBuf::from(iter.next().ok_or("controls output missing")?));
            }
            "--contract-output" => {
                contract_output =
                    Some(PathBuf::from(iter.next().ok_or("contract output missing")?));
            }
            "--reconciliation-only" => o.reconciliation_only = true,
            "--source-root" => o.root = iter.next().ok_or("source root missing")?.into(),
            "--pg-config" => o.pg_config = iter.next().ok_or("pg_config missing")?.into(),
            "--embedding-policy" => {
                o.embedding_policy = match iter.next().map(String::as_str) {
                    Some("not_requested") => catalog::EmbeddingPolicy::NotRequested,
                    Some("rebuilt") => catalog::EmbeddingPolicy::Rebuilt,
                    _ => return Err("invalid embedding policy".into()),
                }
            }
            "--embedding-dim" => {
                o.embedding_dimensions = Some(
                    iter.next()
                        .ok_or("dimension missing")?
                        .parse()
                        .map_err(|_| "invalid dimension")?,
                );
            }
            "--provider-package" => {
                o.provider_package = Some(iter.next().ok_or("package missing")?.into());
            }
            "--historical-install" => {
                o.historical_install = Some(iter.next().ok_or("historical SQL missing")?.into());
            }
            "--docker-image" => {
                o.docker_image = Some(iter.next().ok_or("docker image missing")?.clone());
            }
            "--preload" => o.preload = Some(iter.next().ok_or("provider missing")?.clone()),
            "--extension-prefix" => {
                o.extension_prefix = Some(iter.next().ok_or("prefix missing")?.into());
            }
            "--operator-sql" => {
                o.operator_sql = Some(iter.next().ok_or("operator SQL missing")?.into());
            }
            _ => return Err(format!("unknown parity option: {arg}")),
        }
    }
    if let Some(output) = controls_output {
        return catalog_controls(&o, &output);
    }
    if let Some(output) = contract_output {
        return reference(&o, &output);
    }
    let report = run(&o)?;
    println!(
        "{}",
        serde_json::to_string(&report).map_err(|e| e.to_string())?
    );
    Ok(())
}

impl Cluster {
    fn external(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let mut command = if let Some(proof) = &self.docker {
            let mut c = CommandSpec::new("docker");
            c.args = vec![
                "exec".into(),
                proof.container.id.clone().into(),
                program.into(),
            ];
            c
        } else {
            CommandSpec::new(self.bindir.join(program))
        };
        command.args.extend(args.iter().map(Into::into));
        text(command.checked()?.stdout)
    }
    fn file(&self, path: &Path) -> Result<String, String> {
        if let Some(proof) = &self.docker {
            let mut command = CommandSpec::new("docker");
            command.args = vec![
                "exec".into(),
                proof.container.id.clone().into(),
                "cat".into(),
                path.as_os_str().into(),
            ];
            String::from_utf8(command.checked()?.stdout).map_err(|e| e.to_string())
        } else {
            read(path)
        }
    }
    #[allow(
        clippy::too_many_lines,
        reason = "Owned creation and readiness are one cleanup boundary"
    )]
    fn docker_start(options: &Options) -> Result<Self, String> {
        use crate::owned_docker::{Docker, DockerApi, DurableStore, create};
        // Validate the root before even persistent receipt creation.
        let receipts = crate::scratch::directory("pgokf-parity-receipts-")?.keep();
        let work = crate::scratch::directory("p")?;
        let mut docker = Docker::default();
        let value = uuid::Uuid::new_v4().to_string();
        let args = vec![
            "--network".into(),
            if options.provider_package.is_some() {
                "bridge".into()
            } else {
                "none".into()
            },
            "--entrypoint".into(),
            "sleep".into(),
            "--".into(),
            "infinity".into(),
        ];
        let proof = create(
            &mut docker,
            &mut DurableStore,
            "pgokf.parity-run",
            &value,
            &receipts.join("create.json"),
            options.docker_image.as_deref().ok_or("image missing")?,
            &args,
        )?;
        let cluster = Self {
            bindir: PathBuf::new(),
            service: None,
            work,
            started: false,
            docker: Some(proof),
            receipts,
        };
        docker.call(&[
            "start".into(),
            cluster
                .docker
                .as_ref()
                .ok_or("creation missing")?
                .container
                .id
                .clone(),
        ])?;
        let id = &cluster
            .docker
            .as_ref()
            .ok_or("creation missing")?
            .container
            .id;
        if let Some(package) = &options.provider_package {
            use sha2::{Digest, Sha256};
            let filename = package
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("provider filename missing")?;
            let manifest = read(options.root.join("packaging/docker/pg_search.sha256"))?;
            let expected = manifest
                .lines()
                .filter_map(|line| line.split_once("  "))
                .find(|(_, name)| *name == filename)
                .map(|(digest, _)| digest)
                .ok_or("provider package is not pinned")?;
            let bytes = fs::read(package).map_err(|e| e.to_string())?;
            let digest = Sha256::digest(bytes)
                .iter()
                .fold(String::new(), |mut text, byte| {
                    let _ = write!(text, "{byte:02x}");
                    text
                });
            if digest != expected {
                return Err("provider package checksum differs".into());
            }
            docker.call(&[
                "cp".into(),
                package.to_string_lossy().into_owned(),
                format!("{id}:/tmp/provider.deb"),
            ])?;
            docker.command.timeout = Duration::from_mins(5);
            docker.call(
                &["exec", id, "apt-get", "update", "-qq"]
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<_>>(),
            )?;
            docker.call(
                &["exec", id, "apt-get", "install", "-y", "/tmp/provider.deb"]
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<_>>(),
            )?;
            docker.call(
                &["network", "disconnect", "bridge", id]
                    .into_iter()
                    .map(Into::into)
                    .collect::<Vec<_>>(),
            )?;
        }
        docker.call(
            &[
                "exec",
                "-u",
                "postgres",
                id,
                "initdb",
                "-D",
                "/tmp/parity-data",
                "-U",
                "postgres",
                "--auth=trust",
                "--no-locale",
            ]
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>(),
        )?;
        let mut start: Vec<String> = [
            "exec",
            "-d",
            "-u",
            "postgres",
            id,
            "postgres",
            "-D",
            "/tmp/parity-data",
            "-c",
            "listen_addresses=",
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        if let Some(provider) = &options.preload {
            if !["pg_textsearch", "pg_search"].contains(&provider.as_str()) {
                return Err("unsupported provider".into());
            }
            start.extend(["-c".into(), format!("shared_preload_libraries={provider}")]);
        }
        docker.call(&start)?;
        let deadline = std::time::Instant::now() + Duration::from_mins(2);
        loop {
            if cluster.sql("postgres", "SELECT 1;").is_ok() {
                break;
            }
            if std::time::Instant::now() > deadline {
                return Err("owned Docker PostgreSQL readiness deadline".into());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Ok(cluster)
    }
}

/// Create a reference only by bootstrapping an independent owned cluster.
/// # Errors
/// Installed identity, provider eligibility, and any unsafe captured state refuse.
pub fn reference(options: &Options, output: &Path) -> Result<(), String> {
    let mut cluster = Cluster::start(options)?;
    cluster.sql("postgres", "CREATE DATABASE reference;")?;
    cluster.sql("reference", "CREATE EXTENSION pgokf;")?;
    let version = crate::release::compatibility_source(&options.root)?;
    equal(
        &version,
        &cluster.sql("reference", "SELECT pgokf.version();")?,
        "reference library version",
    )?;
    if let Some(provider) = &options.preload {
        cluster.sql("reference",&format!("CREATE EXTENSION {provider} CASCADE; SELECT pgokf.set_config('search_backend','\"bm25\"'::jsonb); SELECT pgokf.rebuild_search_index();"))?;
    }
    prepare_embedding(&cluster, "reference", options)?;
    let snapshot: catalog::CatalogSnapshot = json::from_slice(
        cluster
            .sql("reference", include_str!("../sql/catalog-snapshot.sql"))?
            .as_bytes(),
    )?;
    let contract = catalog::CatalogContract {
        version: snapshot.version,
        provider: snapshot.provider,
        provider_context: snapshot.provider_context.clone(),
        extension_owner: snapshot.extension_owner.clone(),
        embedding_rows_present: false,
        embedding_policy: options.embedding_policy,
        indexes: snapshot.indexes.clone(),
        relation_indexes: snapshot.relation_indexes.clone(),
        tiers: snapshot
            .roles
            .iter()
            .filter(|role| catalog::TIERS.contains(&role.name.as_str()))
            .cloned()
            .collect(),
        tier_memberships: snapshot
            .memberships
            .iter()
            .filter(|edge| catalog::TIERS.contains(&edge.member.as_str()))
            .cloned()
            .collect(),
    };
    let inventory = cluster.sql(
        "reference",
        &read(options.root.join("tests/upgrade/inventory.sql"))?,
    )?;
    let contract = catalog::AcceptanceContract {
        format_version: 1,
        catalog: contract,
        member_inventory: inventory.clone(),
    };
    catalog::validate_acceptance(
        &contract,
        &catalog::AcceptanceCapture {
            catalog: snapshot,
            member_inventory: inventory,
        },
        false,
    )?;
    crate::owned_docker::publish(output, &contract)?;
    cluster.drop_catalog("reference")?;
    cluster.close()
}

/// Validate independent runtime, privilege and tenant policy before comparing
/// the complete member inventory against the separate fresh installation.
pub(crate) fn validate_stage(
    cluster: &Cluster,
    db: &str,
    inventory: &str,
    policy: catalog::EmbeddingPolicy,
) -> Result<(), String> {
    let observed = capture(cluster, db, inventory, "")?;
    catalog::validate_acceptance(&acceptance(&observed, policy), &observed, true)
}

fn acceptance(
    capture: &catalog::AcceptanceCapture,
    policy: catalog::EmbeddingPolicy,
) -> catalog::AcceptanceContract {
    let s = &capture.catalog;
    catalog::AcceptanceContract {
        format_version: 1,
        catalog: catalog::CatalogContract {
            version: s.version,
            provider: s.provider,
            provider_context: s.provider_context.clone(),
            extension_owner: s.extension_owner.clone(),
            embedding_rows_present: s.embedding_rows_present,
            embedding_policy: policy,
            indexes: s.indexes.clone(),
            relation_indexes: s.relation_indexes.clone(),
            tiers: s
                .roles
                .iter()
                .filter(|r| catalog::TIERS.contains(&r.name.as_str()))
                .cloned()
                .collect(),
            tier_memberships: s
                .memberships
                .iter()
                .filter(|r| catalog::TIERS.contains(&r.member.as_str()))
                .cloned()
                .collect(),
        },
        member_inventory: capture.member_inventory.clone(),
    }
}
fn capture(
    cluster: &Cluster,
    db: &str,
    inventory: &str,
    mutation: &str,
) -> Result<catalog::AcceptanceCapture, String> {
    let output = cluster.sql(
        db,
        &format!(
            "BEGIN;{mutation}\n{inventory}\n\\echo TYPED-SNAPSHOT\n{}\nROLLBACK;",
            include_str!("../sql/catalog-snapshot.sql")
        ),
    )?;
    let (inventory, snapshot) = output
        .split_once("TYPED-SNAPSHOT\n")
        .ok_or("snapshot marker absent")?;
    Ok(catalog::AcceptanceCapture {
        catalog: json::from_slice(snapshot.as_bytes())?,
        member_inventory: inventory.into(),
    })
}
fn catalog_control_cases(
    options: &Options,
    contract: &catalog::AcceptanceContract,
) -> Vec<(&'static str, &'static str)> {
    let mut mutations = vec![
        (
            "tier database config",
            "ALTER ROLE pgokf_reader IN DATABASE candidate SET row_security=off;",
        ),
        ("tier bypassrls", "ALTER ROLE pgokf_reader BYPASSRLS;"),
        (
            "column grant",
            "GRANT UPDATE(title) ON pgokf.concepts TO pgokf_reader;",
        ),
        ("tier comment", "COMMENT ON ROLE pgokf_reader IS 'forged';"),
        (
            "grant option",
            "GRANT SELECT ON pgokf.concepts TO pgokf_reader WITH GRANT OPTION;",
        ),
        (
            "required grant",
            "REVOKE SELECT ON pgokf.concepts FROM pgokf_reader;",
        ),
        (
            "tenant policy expression",
            "ALTER POLICY concepts_tenant_isolation ON pgokf.concepts USING (true);",
        ),
        (
            "bundles RLS",
            "ALTER TABLE pgokf.bundles DISABLE ROW LEVEL SECURITY;",
        ),
        (
            "broad membership",
            "GRANT pg_read_all_data TO pgokf_reader;",
        ),
        (
            "unsafe owner",
            "ALTER TABLE pgokf.concepts OWNER TO pgokf_reader;",
        ),
        (
            "unsafe grant",
            "GRANT UPDATE ON pgokf.concepts TO pgokf_reader;",
        ),
        (
            "rls disabled",
            "ALTER TABLE pgokf.concepts DISABLE ROW LEVEL SECURITY;",
        ),
        (
            "outside rowtype default",
            "CREATE TABLE public.hostile_default(x text DEFAULT (NULL::pgokf_web.users).name);",
        ),
        (
            "third index",
            "CREATE INDEX concepts_hostile_idx ON pgokf.concepts(title);",
        ),
    ];
    if options.preload.is_some() {
        mutations.extend([("missing runtime index","DROP INDEX pgokf.concepts_bm25_idx;"),("runtime comment","COMMENT ON INDEX pgokf.concepts_bm25_idx IS 'forged';"),("runtime wrong access method","DROP INDEX pgokf.concepts_bm25_idx; CREATE INDEX concepts_bm25_idx ON pgokf.concepts(title);")]);
    }
    if options.embedding_policy == catalog::EmbeddingPolicy::Rebuilt
        && contract.catalog.provider_context.vector_schema.is_some()
        && (1..=2000).contains(&contract.catalog.provider_context.embedding_dimensions)
    {
        mutations.push((
            "missing required HNSW",
            "DROP INDEX pgokf.concept_embedding_hnsw_idx;",
        ));
    }
    mutations
}

/// Real `PostgreSQL` controls for runtime-index and tier-role trust boundaries.
/// # Errors
/// Baseline refusal, setup errors, or a surviving mutation fail the campaign.
pub fn catalog_controls(options: &Options, output: &Path) -> Result<(), String> {
    let mut cluster = Cluster::start(options)?;
    let inventory = read(options.root.join("tests/upgrade/inventory.sql"))?;
    for db in ["reference", "candidate"] {
        cluster.sql("postgres", &format!("CREATE DATABASE {db};"))?;
        cluster.sql(db, "CREATE EXTENSION pgokf;")?;
        if let Some(provider) = &options.preload {
            cluster.sql(db,&format!("CREATE EXTENSION {provider} CASCADE; SELECT pgokf.set_config('search_backend','\"bm25\"'::jsonb); SELECT pgokf.rebuild_search_index();"))?;
        }
    }
    for db in ["reference", "candidate"] {
        prepare_embedding(&cluster, db, options)?;
    }
    let reference = capture(&cluster, "reference", &inventory, "")?;
    let contract = acceptance(&reference, options.embedding_policy);
    let baseline = capture(&cluster, "candidate", &inventory, "")?;
    catalog::validate_acceptance(&contract, &baseline, false)?;
    crate::owned_docker::publish(&output.with_extension("reference.json"), &contract)?;
    let mutations = catalog_control_cases(options, &contract);
    let mut results = vec![];
    for (i, (label, sql)) in mutations.iter().enumerate() {
        let candidate = capture(&cluster, "candidate", &inventory, sql)?;
        let result = catalog::validate_acceptance(&contract, &candidate, false);
        crate::owned_docker::publish(&output.with_extension(format!("{i}.json")), &candidate)?;
        if result.is_ok() {
            return Err(format!("catalog control survived: {label}"));
        }
        if *label != "third index" {
            let forged = acceptance(&candidate, options.embedding_policy);
            if catalog::validate_acceptance(&forged, &candidate, false).is_ok() {
                return Err(format!("shared catalog forgery survived: {label}"));
            }
        }
        results.push(serde_json::json!({"control":label,"rejected":true,"reason":result.err()}));
    }
    let after = capture(&cluster, "candidate", &inventory, "")?;
    catalog::validate_acceptance(&contract, &after, false)?;
    crate::owned_docker::publish(output, &results)?;
    cluster.sql("postgres", "DROP DATABASE reference;")?;
    cluster.drop_catalog("candidate")?;
    cluster.close()
}

/// Extract the historical SQL through owned Docker resources and run parity.
/// # Errors
/// Identity, extraction, restoration, cleanup or any parity failure refuses.
pub fn compatibility(root: &Path, pg_config: &Path) -> Result<(), String> {
    use crate::owned_docker::{Docker, DockerApi, DurableStore, cleanup, create};
    crate::release::compatibility_source(root)?;
    let evidence =
        std::env::temp_dir().join(format!("pgokf-compatibility-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&evidence).map_err(|e| e.to_string())?;
    let mut docker = Docker::default();
    let key = "pgokf.parity-run";
    let value = uuid::Uuid::new_v4().to_string();
    let (image, platform) = crate::historical::helper_image()?;
    docker.call(&[
        "pull".into(),
        "--platform".into(),
        platform.into(),
        image.into(),
    ])?;
    let proof = create(
        &mut docker,
        &mut DurableStore,
        key,
        &value,
        &evidence.join("create.json"),
        image,
        &[
            "--platform".into(),
            platform.into(),
            "--network".into(),
            "none".into(),
            "--entrypoint".into(),
            "cat".into(),
            "--".into(),
            "/usr/share/postgresql/18/extension/pgokf--0.2.0.sql".into(),
        ],
    )?;
    let outcome = (|| {
        let sql = docker.call(&["start".into(), "-a".into(), proof.container.id.clone()])?;
        crate::historical::validate(sql.as_bytes())?;
        let mut command = CommandSpec::new(pg_config);
        command.args = vec!["--sharedir".into()];
        let path =
            PathBuf::from(text(command.checked()?.stdout)?).join("extension/pgokf--0.2.0.sql");
        let before = match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        let result = (|| {
            fs::write(&path, sql).map_err(|e| e.to_string())?;
            run(&Options {
                root: root.into(),
                pg_config: pg_config.into(),
                preload: None,
                extension_prefix: None,
                operator_sql: None,
                reconciliation_only: false,
                docker_image: None,
                historical_install: None,
                provider_package: None,
                probe: None,
                embedding_policy: catalog::EmbeddingPolicy::NotRequested,
                embedding_dimensions: None,
            })
        })();
        let restored = if let Some(bytes) = before {
            fs::write(&path, bytes)
        } else {
            fs::remove_file(&path)
        }
        .map_err(|e| e.to_string());
        result.map(|_| ()).and(restored)
    })();
    let removed = cleanup(
        &mut docker,
        &mut DurableStore,
        &proof.container.id,
        key,
        &value,
        &evidence.join("delete.json"),
        Some(&proof),
    );
    outcome.and(removed)
}

/// # Errors
/// Rejects missing/extra/unused parameters and duplicate statements or SQLSTATEs
/// that cannot be compared to `PostgreSQL`'s five-byte state codes.
pub fn validate_registry(registry: &Registry) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for statement in &registry.statements {
        if !seen.insert(&statement.sql) {
            return Err("duplicate product statement".into());
        }
        let tokens = crate::sql::lex(&statement.sql)?;
        let mut parameters = std::collections::BTreeSet::new();
        for pair in tokens.windows(2) {
            if pair[0].text == "$" && pair[0].offset + 1 == pair[1].offset {
                let parameter: usize = pair[1].text.parse().map_err(|_| "invalid SQL parameter")?;
                if parameter == 0 {
                    return Err("zero SQL parameter".into());
                }
                parameters.insert(parameter);
            }
        }
        let arity = parameters.iter().next_back().copied().unwrap_or(0);
        if parameters != (1..=arity).collect() {
            return Err("SQL parameter ordinals are not contiguous".into());
        }
        if statement.runs.is_empty() {
            return Err("product statement has no run".into());
        }
        for run in &statement.runs {
            if run.args.len() != arity {
                return Err("product argument continuity differs from SQL parameters".into());
            }
            if run.sqlstate.len() != 5
                || !run
                    .sqlstate
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
            {
                return Err("invalid expected SQLSTATE".into());
            }
            arguments(&run.args)?;
        }
    }
    if seen.is_empty() {
        return Err("empty product statement registry".into());
    }
    Ok(())
}

/// # Errors
/// Runs only a declared semantic mutation probe; this is not an acceptance gate.
pub fn probe_cli(args: &[String]) -> Result<(), String> {
    let (mode, args) = args.split_first().ok_or("mutation probe mode required")?;
    let probe = match mode.as_str() {
        "exceptions" => Probe::Exceptions,
        "manifest" => Probe::Manifest,
        "upgrade" => Probe::Upgrade,
        "other-superuser" => Probe::OtherSuperuser,
        "refusal-symbol" => Probe::RefusalSymbol,
        "refusal-acl" => Probe::RefusalAcl,
        "refusal-dependents" => Probe::RefusalDependents,
        _ => return Err("unknown mutation probe mode".into()),
    };
    println!("Running a targeted mutation probe; not full parity acceptance");
    cli_mode(args, Some(probe))
}

fn prepare_embedding(cluster: &Cluster, db: &str, options: &Options) -> Result<(), String> {
    if options.embedding_policy == catalog::EmbeddingPolicy::Rebuilt {
        if cluster.sql(
            db,
            "SELECT EXISTS(SELECT 1 FROM pg_available_extensions WHERE name='vector');",
        )? == "t"
        {
            cluster.sql(db, "CREATE EXTENSION IF NOT EXISTS vector;")?;
        }
        if let Some(dim) = options.embedding_dimensions {
            cluster.sql(
                db,
                &format!("SELECT pgokf.set_config('embedding_dim','{dim}'::jsonb);"),
            )?;
        }
        cluster.sql(db, "SELECT pgokf.rebuild_embedding_index();")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seed_rows_and_all_fields_are_preserved() {
        let good = "bundles:{\"id\":1,\"name\":\"fixture\"}\nconcept_metadata:{\"key\":\"fixture\",\"value\":{\"preserved\":true}}\nconcept_source:{\"raw_content\":\"\\\\x000aff\",\"byte_size\":3}\nconcepts:{\"id\":\"entry\",\"body\":\"preserved\"}";
        verify_seed(good, good).unwrap();
        for bad in [
            good.replace("preserved", "changed"),
            good.replace(",\"byte_size\":3", ""),
            good.lines().take(3).collect::<Vec<_>>().join("\n"),
            format!("{good}\nbundles:{{\"id\":1,\"name\":\"fixture\"}}"),
            good.replace("\"id\":1", "\"id\":1,\"id\":2"),
        ] {
            crate::semantic_assert!(
                "seed_rows_and_all_fields_are_preserved",
                verify_seed(good, &bad).is_err()
            );
        }
    }
    #[test]
    fn seed_shapes_fail_closed() {
        for bad in [
            "",
            "bundles:{}",
            "bundles:{}\nbundles:{}\nconcept_metadata:{}\nconcept_source:{}\nconcepts:{}",
            "bundles:[]\nconcept_metadata:{}\nconcept_source:{}\nconcepts:{}",
        ] {
            crate::semantic_assert!("seed_shapes_fail_closed", verify_seed(bad, bad).is_err());
        }
    }
    #[test]
    fn registry_shapes_fail_closed() {
        let good: Registry = json::from_slice(
            br#"{"comment":"test","statements":[{"sql":"SELECT $1","runs":[{"args":["x"]}]}]}"#,
        )
        .unwrap();
        validate_registry(&good).unwrap();
        let value = serde_json::json!({"comment":"test","statements":[]});
        crate::semantic_assert!(
            "registry_shapes_fail_closed",
            validate_registry(
                &json::from_slice::<Registry>(&serde_json::to_vec(&value).unwrap()).unwrap()
            )
            .is_err()
        );
        for mode in ["duplicate", "zero", "no runs", "sqlstate", "argument"] {
            let mut registry: Registry = json::from_slice(
                br#"{"comment":"test","statements":[{"sql":"SELECT $1","runs":[{"args":["x"]}]}]}"#,
            )
            .unwrap();
            match mode {
                "duplicate" => {
                    registry.statements.push(
                        json::from_slice(br#"{"sql":"SELECT $1","runs":[{"args":["x"]}]}"#)
                            .unwrap(),
                    );
                }
                "zero" => registry.statements[0].sql = "SELECT $0".into(),
                "no runs" => registry.statements[0].runs.clear(),
                "sqlstate" => registry.statements[0].runs[0].sqlstate = "bad".into(),
                _ => registry.statements[0].runs[0].args = vec![serde_json::json!({"x":1})],
            }
            crate::semantic_assert!(
                "registry_shapes_fail_closed",
                validate_registry(&registry).is_err(),
                "{mode}"
            );
        }
    }
    #[test]
    fn legacy_probe_registry_complete() {
        let p = probes().unwrap();
        assert_eq!(p.refusals.len(), 41);
        assert_eq!(p.constraint_probes.len(), 17);
        assert_eq!(p.probes.len(), 13);
        assert_eq!(p.definition_probes.len(), 15);
        assert_eq!(p.exception_probes.len(), 60);
    }
    #[test]
    fn argument_continuity_is_checked() {
        let mut registry:Registry=json::from_slice(br#"{"comment":"x","statements":[{"sql":"SELECT $1, '$2'","runs":[{"args":["value"]}]}]}"#).unwrap();
        validate_registry(&registry).unwrap();
        registry.statements[0].runs[0].args.clear();
        crate::semantic_assert!(
            "argument_continuity_is_checked",
            validate_registry(&registry).is_err()
        );
        registry.statements[0].sql = "SELECT $2".into();
        registry.statements[0].runs[0].args = vec![Value::Null, Value::Null];
        crate::semantic_assert!(
            "argument_continuity_is_checked",
            validate_registry(&registry).is_err()
        );
    }
    #[test]
    fn parameter_runs_and_preparation_are_fail_closed() {
        let r:Registry=json::from_slice(br#"{"comment":"test","statements":[{"sql":"SELECT $1","runs":[{"args":["x"]},{"args":[null],"sqlstate":"23502"}]}]}"#).unwrap();
        let good = "prepare 0 00000\nstatement 0 run 0 00000\nstatement 0 run 1 23502";
        check_behaviour(good, &r).unwrap();
        for bad in [
            good.replace("prepare 0 00000", "prepare 0 42703"),
            good.replace("run 1 23502", "run 1 00000"),
            good.replace("statement 0 run 0 00000\n", ""),
            format!("{good}\nstatement 0 run 1 23502"),
        ] {
            crate::semantic_assert!(
                "parameter_runs_and_preparation_are_fail_closed",
                check_behaviour(&bad, &r).is_err()
            );
        }
        crate::semantic_assert!(
            "parameter_runs_and_preparation_are_fail_closed",
            behaviour_script(&r, &probes().unwrap())
                .unwrap()
                .contains("EXECUTE product_0('x');")
        );
    }
    #[test]
    fn standalone_always_cleans_and_restores() {
        for failure in ["none", "create", "inventory", "version", "cleanup"] {
            let dir = tempfile::tempdir().unwrap();
            let installed = dir.path().join("installed.sql");
            let committed = dir.path().join("committed.sql");
            fs::write(&installed, "generated").unwrap();
            fs::write(&committed, "committed").unwrap();
            let mut calls = vec![];
            let result = check_committed_install(
                |_, s| {
                    calls.push(s.to_owned());
                    if (s == "CREATE EXTENSION pgokf;" && failure == "create")
                        || (s.starts_with("DROP DATABASE") && failure == "cleanup")
                    {
                        return Err(failure.into());
                    }
                    Ok(match s {
                        "inventory" => {
                            if failure == "inventory" {
                                "bad"
                            } else {
                                "expected"
                            }
                        }
                        "SELECT pgokf.version();" => {
                            if failure == "version" {
                                "bad"
                            } else {
                                "0.3.0"
                            }
                        }
                        _ => "",
                    }
                    .into())
                },
                &installed,
                &committed,
                "expected",
                "inventory",
                "0.3.0",
            );
            assert_eq!(result.is_ok(), failure == "none");
            assert_eq!(read(&installed).unwrap(), "generated");
            assert!(calls.iter().any(|s| s.starts_with("DROP DATABASE")));
            if failure != "cleanup" {
                assert!(calls.iter().any(|s| s.starts_with("DROP ROLE")));
            }
        }
    }
}
