// SPDX-License-Identifier: AGPL-3.0-only
//! Live-shaped reconciliation, atomic refusals and semantic catalog controls.
use crate::{
    catalog, json,
    process::CommandSpec,
    upgrade::{self, Cluster, Options, Probes, Registry, Report, equal, literal, read},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Output,
    thread::JoinHandle,
    time::Duration,
};

fn rowtypes(c: &Cluster, db: &str, oracle: Option<&str>) -> Result<String, String> {
    let snapshot = c.sql(db, include_str!("../sql/catalog-snapshot.sql"))?;
    let parsed: catalog::CatalogSnapshot = json::from_slice(snapshot.as_bytes())?;
    if !parsed.rowtype_dependents.is_empty() {
        let error = format!(
            "objects depend on the web identity row types: {:?}",
            parsed.rowtype_dependents
        );
        return if let Some(oracle) = oracle {
            c.semantic_failure(oracle, error)
        } else {
            Err(error)
        };
    }
    Ok(snapshot)
}
fn legacy(c: &Cluster, db: &str, p: &Probes) -> Result<String, String> {
    let mut sql = p.legacy_order.clone();
    for (table, column) in &p.appended {
        let comment=c.sql(db,&format!("SELECT col_description('{table}'::regclass,(SELECT attnum FROM pg_attribute WHERE attrelid='{table}'::regclass AND attname='{column}'));"))?;
        write!(
            sql,
            "COMMENT ON COLUMN {table}.{column} IS {};",
            literal(&comment)
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(sql)
}

fn retired_oracle(signature: &str) -> Result<&'static str, String> {
    if signature.contains("concept_search_semantic") {
        Ok("reconciled.retired-semantic-absent")
    } else if signature.contains("concept_search_hybrid") {
        Ok("reconciled.retired-hybrid-absent")
    } else if signature.contains("bm25_hits") {
        Ok("reconciled.retired-bm25-absent")
    } else {
        Err(format!("unknown retired overload oracle: {signature}"))
    }
}
fn exceptions(
    c: &Cluster,
    p: &Probes,
    expected: &str,
    inventory: &str,
    r: &mut Report,
) -> Result<(), String> {
    c.sql("postgres", "CREATE DATABASE exception_probe;")?;
    c.sql("exception_probe", "CREATE EXTENSION pgokf;")?;
    let baseline = c.sql("exception_probe", inventory)?;
    catalog::compare_inventory(expected, &baseline, true)?;
    rowtypes(c, "exception_probe", None)?;
    let legacy = legacy(c, "exception_probe", p)?;
    let actual = c.sql(
        "exception_probe",
        &format!("BEGIN;{legacy}\n{inventory}\nROLLBACK;"),
    )?;
    catalog::compare_inventory(expected, &actual, true)?;
    if catalog::compare_inventory(expected, &actual, false).is_ok() {
        return Err("strict comparison accepts legacy order".into());
    }
    r.passed("declared-order positive and strict negative controls");
    for (label, sql) in &p.exception_probes {
        let sql = sql.replace("{LEGACY}", &legacy);
        let output = c.sql(
            "exception_probe",
            &format!(
                "BEGIN;{sql}\n{inventory}\n\\echo SNAPSHOT\n{}\nROLLBACK;",
                include_str!("../sql/catalog-snapshot.sql")
            ),
        )?;
        let (actual, snapshot) = output
            .split_once("SNAPSHOT\n")
            .ok_or("snapshot marker missing")?;
        let snapshot: catalog::CatalogSnapshot = json::from_slice(snapshot.as_bytes())?;
        if catalog::compare_inventory(expected, actual, true).is_ok()
            && snapshot.rowtype_dependents.is_empty()
        {
            return c.semantic_failure(
                "exceptions.rowtype-dependents-rejected",
                format!("exception probe {label}: UNSAFE ACCEPTANCE"),
            );
        }
        r.passed(format!("exception probe {label}"));
    }
    equal(
        &baseline,
        &c.sql("exception_probe", inventory)?,
        "exception probe rollback",
    )?;
    c.drop_catalog("exception_probe")
}
struct Gate<'a> {
    c: &'a Cluster,
    o: &'a Options,
    p: &'a Probes,
    expected: &'a str,
    metadata: &'a str,
    inventory: &'a str,
    seed: &'a str,
    version: &'a str,
    script: String,
    registry: Registry,
    web: String,
    behaviour: String,
    positional: String,
}
impl Gate<'_> {
    fn drifted(
        &self,
        db: &str,
        at: &str,
        drift_at: &str,
        web: bool,
    ) -> Result<(String, String), String> {
        self.c.sql("postgres", &format!("CREATE DATABASE {db};"))?;
        self.c.sql(
            db,
            &format!("CREATE EXTENSION pgokf VERSION '0.2.0';{}", self.seed),
        )?;
        let fp = upgrade::fingerprint(self.c, db)?;
        let before = self.c.sql(db, &fp)?;
        self.c.sql(db,&format!("ALTER EXTENSION pgokf UPDATE TO '0.3.0-dev2'; INSERT INTO pgokf.skills (bundle_id, concept_id, visibility, agent_skill, skill_md, package_root, package_hash, source_file_hash) SELECT id, 'entry', 'public', '{{\"name\":\"upgrade-skill\"}}', '\\x{}', 'skills/upgrade-skill', repeat('b',64),repeat('a',64) FROM pgokf.bundles;{}",self.p.skill_hex,read(self.o.root.join("tests/upgrade/web-seed.sql"))?))?;
        if drift_at != "0.3.0-dev2" {
            self.c.sql(
                db,
                &format!("ALTER EXTENSION pgokf UPDATE TO {};", literal(drift_at)),
            )?;
        }
        if web {
            equal(
                &self.web,
                &self.c.sql(db, &self.p.web_fingerprint)?,
                "drifted web seed",
            )?;
            self.c
                .sql(db, &read(self.o.root.join("tests/upgrade/live-drift.sql"))?)?;
        }
        if at != drift_at {
            self.c.sql(
                db,
                &format!("ALTER EXTENSION pgokf UPDATE TO {};", literal(at)),
            )?;
        }
        Ok((fp, before))
    }
    fn update(&self, db: &str, user: &str) -> Result<(), String> {
        self.c.sql_as(db, "ALTER EXTENSION pgokf UPDATE;", user)?;
        if let Some(path) = &self.o.operator_sql {
            self.c.sql(db, &read(path)?)?;
        }
        Ok(())
    }
    fn web(&self, db: &str) -> Result<(), String> {
        equal(
            &self.web,
            &self.c.sql(db, &self.p.web_fingerprint)?,
            "web identity preservation",
        )?;
        let behaviour = self.c.sql(db, &self.script)?;
        upgrade::check_behaviour(&behaviour, &self.registry)?;
        equal(&self.behaviour, &behaviour, "product statements")?;
        rowtypes(self.c, db, Some("reconciled.rowtype-dependents-empty"))?;
        for (table, order) in [
            (
                "pgokf_web.users",
                "name,role,password_hash,created_at,updated_at,display_name,provider",
            ),
            (
                "pgokf_web.sessions",
                "nonce,subject,mode,expires_at,created_at,provider",
            ),
        ] {
            let actual = self.c.sql(db,&format!("SELECT string_agg(attname, ',' ORDER BY attnum) FROM pg_attribute WHERE attrelid='{table}'::regclass AND attnum>0 AND NOT attisdropped;"))?;
            if let Err(error) = equal(order, &actual, "declared physical order") {
                return if table == "pgokf_web.users" {
                    self.c
                        .semantic_failure("reconciled.users-declared-order", error)
                } else {
                    Err(error)
                };
            }
        }
        if self.positional
            == self
                .c
                .sql(db, "SELECT * FROM pgokf_web.users ORDER BY name;")?
        {
            return Err("positional control does not observe declared order".into());
        }
        Ok(())
    }
    fn reconciled(&self, db: &str, before: &str, fp: &str) -> Result<(), String> {
        equal(
            self.version,
            &self.c.sql(
                db,
                "SELECT extversion FROM pg_extension WHERE extname='pgokf';",
            )?,
            "reconciled version",
        )?;
        for signature in &self.p.retired {
            let actual = self.c.sql(
                db,
                &format!(
                    "SELECT to_regprocedure({}) IS NOT NULL;",
                    literal(signature)
                ),
            )?;
            if let Err(error) = equal("f", &actual, "retired overload") {
                return self.c.semantic_failure(retired_oracle(signature)?, error);
            }
        }
        let helper = self.c.sql(
            db,
            &format!(
                "SELECT to_regprocedure({}) IS NULL;",
                literal(&self.p.helper)
            ),
        )?;
        if let Err(error) = equal("f", &helper, "current helper") {
            return self
                .c
                .semantic_failure("reconciled.current-helper-present", error);
        }
        for call in &self.p.defaulted_calls {
            let plan = self.c.sql(
                db,
                &format!("EXPLAIN (VERBOSE,COSTS OFF) SELECT * FROM {call};"),
            )?;
            if !plan.contains("NULL::text[])") {
                return Err(format!(
                    "defaulted call does not resolve current signature: {call}"
                ));
            }
        }
        let expected: serde_json::Value = json::from_slice(self.metadata.as_bytes())?;
        let actual: serde_json::Value =
            json::from_slice(self.c.sql(db, &self.p.function_metadata)?.as_bytes())?;
        if expected != actual {
            return self.c.semantic_failure(
                "reconciled.function-metadata-equal",
                "reconciled function metadata drift",
            );
        }
        equal("0",&self.c.sql(db,"SELECT count(*) FROM pg_init_privs WHERE classoid='pg_proc'::regclass AND objoid NOT IN (SELECT oid FROM pg_proc);")?,"orphan initial privileges")?;
        let bytes = (0..self.p.skill_hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&self.p.skill_hex[i..i + 2], 16).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        equal(&format!("{}:{}:{}",self.p.skill_hex,Sha256::digest(&bytes).iter().fold(String::new(),|mut s,b|{write!(s,"{b:02x}").expect("string write");s}),bytes.len()),&self.c.sql(db,"SELECT encode(skill_md,'hex') || ':' || skill_md_sha256 || ':' || skill_md_byte_size FROM pgokf.skills;")?,"skill bytes and backfill")?;
        self.web(db)?;
        upgrade::check_invariants(
            self.c,
            db,
            before,
            fp,
            self.expected,
            self.inventory,
            self.version,
        )?;
        self.search(db)
    }
    fn search(&self, db: &str) -> Result<(), String> {
        self.c.sql(
            "postgres",
            "CREATE ROLE reconcile_reader LOGIN; GRANT pgokf_reader TO reconcile_reader;",
        )?;
        equal("entry",&self.c.sql(db,"SET ROLE reconcile_reader;SELECT string_agg(concept_id, ',' ORDER BY concept_id) FROM pgokf.concept_search_hybrid('refreshed',ARRAY[0.1,0.2]::real[],NULL,10,ARRAY['Concept']);")?,"typed hybrid search")?;
        if self.c.sql(
            db,
            "SELECT count(*) FROM pg_available_extensions WHERE name='vector';",
        )? == "1"
        {
            self.c.sql(
                db,
                "CREATE EXTENSION vector; SELECT pgokf.set_config('embedding_dim','2'::jsonb);",
            )?;
            self.c.sql(db,"SET ROLE reconcile_reader; SELECT count(*) FROM pgokf.concept_search_semantic(ARRAY[0.1,0.2]::real[]);")?;
            equal("entry",&self.c.sql(db,"SET ROLE reconcile_reader; SELECT string_agg(concept_id, ',' ORDER BY concept_id) FROM pgokf.concept_search_hybrid('refreshed',ARRAY[0.1,0.2]::real[]);")?,"vector hybrid search")?;
        }
        if let Some(provider) = &self.o.preload {
            self.c.sql(db,&format!("CREATE EXTENSION IF NOT EXISTS {provider} CASCADE; SELECT pgokf.set_config('search_backend','\"bm25\"'::jsonb); SELECT pgokf.rebuild_search_index();"))?;
        }
        equal("entry",&self.c.sql(db,"SET ROLE reconcile_reader; SELECT string_agg(concept_id, ',' ORDER BY concept_id) FROM pgokf.concept_search('refreshed',NULL,5);")?,"lexical search")?;
        if self.o.preload.as_deref() == Some("pg_search") {
            equal("entry",&self.c.sql(db,"SET ROLE reconcile_reader;SELECT string_agg(concept_id, ',' ORDER BY concept_id) FROM pgokf.bm25_hits('refreshed',NULL,5,'pg_catalog.english',NULL,NULL,NULL,NULL,NULL,NULL,NULL,ARRAY['Concept']);")?,"twelve argument helper")?;
        }
        if self.o.preload.is_some() {
            let capture: catalog::CatalogSnapshot = json::from_slice(
                self.c
                    .sql(db, include_str!("../sql/catalog-snapshot.sql"))?
                    .as_bytes(),
            )?;
            if capture.indexes.is_empty() {
                return Err("required provider index absent".into());
            }
            for index in &capture.indexes {
                crate::provider::validate(index, capture.provider, &capture.provider_context)?;
            }
        }
        upgrade::validate_stage(
            self.c,
            db,
            self.inventory,
            catalog::EmbeddingPolicy::NotRequested,
        )?;
        self.c.drop_catalog(db)?;
        self.c.sql("postgres", "DROP ROLE reconcile_reader;")?;
        Ok(())
    }
}
/// Build the fresh web behaviour baseline the refusal oracle compares against.
/// # Errors
/// Registry identity, fresh install or behaviour transcript failures refuse.
fn web_baseline(
    c: &Cluster,
    o: &Options,
    p: &Probes,
) -> Result<(Registry, String, String, String, String), String> {
    let registry: Registry = json::from_slice(
        &fs::read(o.root.join("tests/upgrade/web-identity-statements.json"))
            .map_err(|e| e.to_string())?,
    )?;
    let product = crate::positional::product_statements(&o.root)?;
    let mut registered = registry
        .statements
        .iter()
        .map(|s| s.sql.clone())
        .collect::<Vec<_>>();
    registered.sort();
    if product != registered {
        return Err("statement registry differs from the product statements".into());
    }
    let script = upgrade::behaviour_script(&registry, p)?;
    c.sql("postgres", "CREATE DATABASE fresh_web;")?;
    c.sql(
        "fresh_web",
        &format!(
            "CREATE EXTENSION pgokf;{}",
            read(o.root.join("tests/upgrade/web-seed.sql"))?
        ),
    )?;
    let behaviour = c.sql("fresh_web", &script)?;
    upgrade::check_behaviour(&behaviour, &registry)?;
    let web = c.sql("fresh_web", &p.web_fingerprint)?;
    let positional = c.sql("fresh_web", "SELECT * FROM pgokf_web.users ORDER BY name;")?;
    c.drop_catalog("fresh_web")?;
    Ok((registry, script, web, behaviour, positional))
}

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    reason = "Explicit evidence inputs and sequential gate phases"
)]
pub(crate) fn run(
    c: &Cluster,
    o: &Options,
    p: &Probes,
    expected: &str,
    metadata: &str,
    inventory: &str,
    seed: &str,
    version: &str,
    r: &mut Report,
    refusal_workers: usize,
) -> Result<(), String> {
    let mut shards = ShardGuard(None);
    if o.probe.is_none() {
        if refusal_workers > 1 && refusal_workers != REFUSAL_SHARDS {
            return Err("refusal shard count must be exactly three".into());
        }
        if refusal_workers > 1 {
            let launches = c.observed("reconciliation.refusal-shards-launch", || {
                launch_refusal_shards(c, o, p)
            })?;
            shards.0 = Some(launches);
        }
    }
    if o.probe.is_none() || o.probe == Some(upgrade::Probe::Exceptions) {
        c.observed("reconciliation.exceptions", || {
            c.semantic("exceptions", || exceptions(c, p, expected, inventory, r))
        })?;
    }
    if o.probe == Some(upgrade::Probe::Exceptions) {
        return Ok(());
    }
    let (registry, script, web, behaviour, positional) =
        c.observed("reconciliation.web-baseline", || web_baseline(c, o, p))?;
    let gate = Gate {
        c,
        o,
        p,
        expected,
        metadata,
        inventory,
        seed,
        version,
        script,
        registry,
        web,
        behaviour,
        positional,
    };
    r.passed("fresh web registry");
    c.observed("reconciliation.live-drift", || {
        gate.drifted("factory", "0.3.0-dev2", "0.3.0-dev2", false)?;
        gate.drifted("probe", "0.3.0-dev2", "0.3.0-dev2", true)?;
        let keys =
            catalog::differing_keys(&c.sql("factory", inventory)?, &c.sql("probe", inventory)?)?;
        c.semantic("manifest", || {
            if keys != p.live_drift_manifest.iter().cloned().collect() {
                return c.semantic_failure(
                    "manifest.live-drift-exact",
                    format!("live-drift fixture manifest differs: {keys:?}"),
                );
            }
            Ok(())
        })?;
        c.sql("postgres", "DROP DATABASE factory;")?;
        for call in &p.defaulted_calls {
            match c.sql("probe", &format!("EXPLAIN SELECT * FROM {call};")) {
                Err(e) if e.contains("is not unique") => (),
                other => return Err(format!("ambiguity control failed: {other:?}")),
            }
        }
        equal(
            "t",
            &c.sql(
                "probe",
                &format!("SELECT to_regprocedure({}) IS NULL;", literal(&p.helper)),
            )?,
            "missing helper control",
        )?;
        gate.web("probe")?;
        c.drop_catalog("probe")
    })?;
    r.passed("complete live-drift manifest and ambiguous calls");
    if o.probe == Some(upgrade::Probe::Manifest) {
        return Ok(());
    }
    let refusal_label = match o.probe {
        Some(upgrade::Probe::RefusalSymbol) => Some("concept_search_semantic symbol"),
        Some(upgrade::Probe::RefusalAcl) => Some("concept_search_semantic acl writer"),
        Some(upgrade::Probe::RefusalDependents) => Some("concept_search_semantic dependent"),
        _ => None,
    };
    if let Some(label) = refusal_label {
        let phase = format!("reconciliation.targeted-refusal-{label}");
        c.observed(&phase, || {
            let (_, setup, diagnostic, _) = p
                .refusals
                .iter()
                .find(|case| case.0 == label)
                .ok_or("refusal probe absent")?;
            gate.drifted("refusal", "0.3.0-dev2", "0.3.0-dev2", true)?;
            c.sql("refusal", setup)?;
            c.semantic("refusal", || {
                upgrade::refusal(c, "refusal", inventory, diagnostic, "0.3.0-dev2")
            })?;
            c.drop_catalog("refusal")?;
            r.passed(format!("targeted refusal {label}"));
            Ok(())
        })?;
        return Ok(());
    }
    for (route, at, user) in [
        ("live_dev2", "0.3.0-dev2", "postgres"),
        ("live_dev1", "0.4.0-dev1", "postgres"),
        ("live_dev4_2", "0.4.0-dev2", "postgres"),
        ("other_superuser", "0.3.0-dev2", "reconcile_superuser"),
    ] {
        if o.probe.is_some()
            && !((o.probe == Some(upgrade::Probe::OtherSuperuser) && route == "other_superuser")
                || (o.probe != Some(upgrade::Probe::OtherSuperuser) && route == "live_dev2"))
        {
            continue;
        }
        let phase = format!("reconciliation.route-{route}");
        c.observed(&phase, || {
            let drift_at = if at == "0.4.0-dev2" { "0.3.0-dev2" } else { at };
            let (fp, before) = gate.drifted(route, at, drift_at, true)?;
            if user != "postgres" {
                c.sql("postgres", &format!("CREATE ROLE {user} LOGIN SUPERUSER;"))?;
            }
            gate.update(route, user)?;
            c.semantic("reconciled", || gate.reconciled(route, &before, &fp))?;
            if user != "postgres" {
                c.sql("postgres", &format!("DROP ROLE {user};"))?;
            }
            r.passed(format!("reconciliation {route}"));
            Ok(())
        })?;
    }
    if o.probe.is_some() {
        return Ok(());
    }
    c.observed("reconciliation.interrupted", || {
        let (fp, before) =
            gate.drifted("interrupted", "0.3.0-dev2", "0.3.0-dev2", true)?;
        let state = c.sql("interrupted", inventory)?;
        let mut command = c.sql_command(
            "interrupted",
            "BEGIN; ALTER EXTENSION pgokf UPDATE; SELECT pg_sleep(600); COMMIT;",
            "postgres",
        )?;
        command.timeout = std::time::Duration::from_secs(90);
        std::thread::scope(|scope| -> Result<(), String> {
            let session = scope.spawn(|| command.run());
            let mut reached = false;
            for _ in 0..600 {
                let pid=c.sql("postgres","SELECT pid FROM pg_stat_activity WHERE datname='interrupted' AND wait_event='PgSleep' AND state='active';")?;
                if !pid.is_empty() {
                    equal(
                        "t",
                        &c.sql("postgres", &format!("SELECT pg_terminate_backend({pid});"))?,
                        "interrupt termination",
                    )?;
                    reached = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            let result = session.join().map_err(|_| "interrupt thread panicked")??;
            if !reached || result.status.success() {
                return Err("interruption did not reach and abort updated transaction".into());
            }
            Ok(())
        })?;
        equal(
            &state,
            &c.sql("interrupted", inventory)?,
            "interrupted rollback",
        )?;
        gate.update("interrupted", "postgres")?;
        gate.reconciled("interrupted", &before, &fp)?;
        r.passed("interrupted atomic rollback and retry");
        Ok(())
    })?;
    match shards.0.take() {
        None => {
            let indices = (0..p.refusals.len()).collect::<Vec<_>>();
            let mut ops = GateRefusals { gate: &gate };
            run_refusal_cases(p, &indices, &mut ops, &mut |record| {
                r.passed(format!("refusal {}", record.label));
                Ok(())
            })?;
        }
        Some(launches) => {
            let labels = c.observed("reconciliation.refusal-shards", || {
                join_refusal_shards(o, p, launches)
            })?;
            for label in labels {
                r.passed(format!("refusal {label}"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Exact-oracle refusal scheduling. The per-case sequence (drift, hostile
// setup, refused update, repair, committed update, complete reconciliation)
// is unchanged; only the schedule that executes independent cases changes.

/// The prescribed shard count; the deadline analysis validates only this.
pub(crate) const REFUSAL_SHARDS: usize = 3;
const SHARD_FORMAT_VERSION: u32 = 1;
/// Declared scheduling weights: repairable cases repeat the complete
/// successful reconciliation proof, terminal cases stop at the refusal.
const REPAIRABLE_REFUSAL_WEIGHT: u64 = 4;
const TERMINAL_REFUSAL_WEIGHT: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShardAssignment {
    pub id: usize,
    pub indices: Vec<usize>,
}

/// Deterministic load-balanced partition of the fixture-derived refusal cases.
/// # Errors
/// Any shard count without at least one case per shard refuses.
pub(crate) fn partition_refusals(
    p: &Probes,
    shards: usize,
) -> Result<Vec<ShardAssignment>, String> {
    if shards == 0 {
        return Err("refusal shard count must be positive".into());
    }
    if p.refusals.len() < shards {
        return Err("refusal fixture has fewer cases than shards".into());
    }
    let mut loads = vec![0u64; shards];
    let mut indices: Vec<Vec<usize>> = vec![Vec::new(); shards];
    let mut next = 0;
    for (index, case) in p.refusals.iter().enumerate() {
        if case.3.is_some() {
            loads[next] += REPAIRABLE_REFUSAL_WEIGHT;
            indices[next].push(index);
            next = (next + 1) % shards;
        }
    }
    for (index, case) in p.refusals.iter().enumerate() {
        if case.3.is_none() {
            let mut target = 0;
            for candidate in 1..shards {
                if loads[candidate] < loads[target] {
                    target = candidate;
                }
            }
            loads[target] += TERMINAL_REFUSAL_WEIGHT;
            indices[target].push(index);
        }
    }
    for shard in &mut indices {
        shard.sort_unstable();
    }
    let assignments = indices
        .into_iter()
        .enumerate()
        .map(|(id, indices)| ShardAssignment { id, indices })
        .collect::<Vec<_>>();
    validate_partitions(shards, p.refusals.len(), &assignments)?;
    Ok(assignments)
}

/// Shards must cover every fixture index exactly once, in dense id order,
/// each ascending within the shard.
/// # Errors
/// Overlap, gaps, reordering, relabeled ids or empty shards refuse.
pub(crate) fn validate_partitions(
    shards: usize,
    count: usize,
    assignments: &[ShardAssignment],
) -> Result<(), String> {
    if assignments.len() != shards {
        return Err("refusal shard assignment count differs".into());
    }
    let mut covered = Vec::with_capacity(count);
    for (position, assignment) in assignments.iter().enumerate() {
        if assignment.id != position {
            return Err("refusal shard identities are not dense".into());
        }
        if assignment.indices.is_empty() {
            return Err(format!("refusal shard {} is empty", assignment.id));
        }
        let mut previous = None;
        for &index in &assignment.indices {
            if index >= count {
                return Err("refusal shard index is outside the fixture".into());
            }
            if previous.is_some_and(|prev| index <= prev) {
                return Err("refusal shard indices are not strictly ascending".into());
            }
            previous = Some(index);
            covered.push(index);
        }
    }
    covered.sort_unstable();
    if covered != (0..count).collect::<Vec<_>>() {
        return Err("refusal shard partitions must cover every fixture index exactly once".into());
    }
    Ok(())
}

pub(crate) struct RefusalCasePlan<'a> {
    pub index: usize,
    pub label: &'a str,
    pub phase: String,
    pub setup: &'a str,
    pub diagnostic: &'a str,
    pub repair: Option<&'a str>,
}

/// Bind each requested fixture index to its exact label, phase, setup,
/// diagnostic and repair. Indices must be an explicit ascending set.
/// # Errors
/// Missing, unordered, duplicated or out-of-range indices refuse.
pub(crate) fn refusal_plans<'a>(
    p: &'a Probes,
    indices: &[usize],
) -> Result<Vec<RefusalCasePlan<'a>>, String> {
    if indices.is_empty() {
        return Err("refusal case indices are missing".into());
    }
    let mut plans = Vec::with_capacity(indices.len());
    let mut previous: Option<usize> = None;
    for &index in indices {
        let count = p.refusals.len();
        if index >= count {
            return Err(format!("refusal case index {index} is outside the fixture"));
        }
        if previous.is_some_and(|prev| index <= prev) {
            return Err("refusal case indices must be strictly ascending".into());
        }
        previous = Some(index);
        let (label, setup, diagnostic, repair) = &p.refusals[index];
        plans.push(RefusalCasePlan {
            index,
            label,
            phase: format!("reconciliation.refusal-{index:02}-{label}"),
            setup,
            diagnostic,
            repair: repair.as_deref(),
        });
    }
    Ok(plans)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefusalCaseRecord {
    pub index: usize,
    pub label: String,
    pub phase: String,
    pub repair: bool,
}

/// Executed case records must bind the exact executed indices, fixture
/// labels, phase names and repair presence.
/// # Errors
/// Omitted, duplicated, relabeled, rephased or repair-mismatched records refuse.
pub(crate) fn validate_refusal_records(
    p: &Probes,
    expected: &[usize],
    records: &[RefusalCaseRecord],
) -> Result<(), String> {
    if records.len() != expected.len() {
        return Err(format!(
            "refusal case records cover {} indices but {} were executed",
            records.len(),
            expected.len()
        ));
    }
    for (record, &index) in records.iter().zip(expected) {
        let Some(case) = p.refusals.get(index) else {
            return Err("refusal case record index is outside the fixture".into());
        };
        if record.index != index {
            return Err(format!(
                "refusal case record is misbound to executed index {index}"
            ));
        }
        if record.label != case.0 {
            return Err("refusal case record was relabeled".into());
        }
        if record.phase != format!("reconciliation.refusal-{:02}-{}", record.index, case.0) {
            return Err("refusal case record phase was relabeled".into());
        }
        if record.repair != case.3.is_some() {
            return Err("refusal case record repair binding differs".into());
        }
    }
    Ok(())
}

/// The unchanged per-case refusal sequence, driven over an explicit ordered
/// set of fixture indices.
pub(crate) trait RefusalCaseOps {
    fn drifted(&mut self) -> Result<(String, String), String>;
    fn setup(&mut self, sql: &str) -> Result<(), String>;
    fn refuse(&mut self, diagnostic: &str) -> Result<(), String>;
    fn repair(&mut self, sql: &str) -> Result<(), String>;
    fn update(&mut self) -> Result<(), String>;
    fn reconciled(&mut self, before: &str, fingerprint: &str) -> Result<(), String>;
    fn drop_case(&mut self) -> Result<(), String>;
    fn observed_case(
        &mut self,
        phase: &str,
        case: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String>;
}

/// # Errors
/// Propagates the first failing case; every executed case must complete the
/// exact oracle sequence before its label is credited.
pub(crate) fn run_refusal_cases<O: RefusalCaseOps>(
    p: &Probes,
    indices: &[usize],
    ops: &mut O,
    credit: &mut dyn FnMut(&RefusalCaseRecord) -> Result<(), String>,
) -> Result<(), String> {
    for plan in refusal_plans(p, indices)? {
        let record = RefusalCaseRecord {
            index: plan.index,
            label: plan.label.to_owned(),
            phase: plan.phase.clone(),
            repair: plan.repair.is_some(),
        };
        ops.observed_case(&plan.phase, |ops| {
            let (fingerprint, before) = ops.drifted()?;
            ops.setup(plan.setup)?;
            ops.refuse(plan.diagnostic)?;
            if let Some(repair) = plan.repair {
                ops.repair(repair)?;
                ops.update()?;
                ops.reconciled(&before, &fingerprint)?;
            } else {
                ops.drop_case()?;
            }
            credit(&record)?;
            Ok(())
        })?;
    }
    Ok(())
}

/// The live per-case operations over the shared drifted `refusal` database.
struct GateRefusals<'a> {
    gate: &'a Gate<'a>,
}
impl RefusalCaseOps for GateRefusals<'_> {
    fn drifted(&mut self) -> Result<(String, String), String> {
        self.gate
            .drifted("refusal", "0.3.0-dev2", "0.3.0-dev2", true)
    }
    fn setup(&mut self, sql: &str) -> Result<(), String> {
        self.gate.c.sql("refusal", sql).map(|_| ())
    }
    fn refuse(&mut self, diagnostic: &str) -> Result<(), String> {
        upgrade::refusal(
            self.gate.c,
            "refusal",
            self.gate.inventory,
            diagnostic,
            "0.3.0-dev2",
        )
    }
    fn repair(&mut self, sql: &str) -> Result<(), String> {
        self.gate.c.sql("refusal", sql).map(|_| ())
    }
    fn update(&mut self) -> Result<(), String> {
        self.gate.update("refusal", "postgres")
    }
    fn reconciled(&mut self, before: &str, fingerprint: &str) -> Result<(), String> {
        self.gate.reconciled("refusal", before, fingerprint)
    }
    fn drop_case(&mut self) -> Result<(), String> {
        self.gate.c.drop_catalog("refusal")
    }
    fn observed_case(
        &mut self,
        phase: &str,
        case: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let cluster = self.gate.c;
        cluster.observed(phase, || case(self))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ShardIdentity {
    pub source_commit: String,
    pub source_tree: String,
    pub executable_sha256: String,
    pub extension_control_sha256: String,
    pub install_sql_sha256: String,
    pub fixture_sha256: String,
}

fn sha256_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn sha256_file(path: &Path) -> Result<String, String> {
    Ok(sha256_bytes(
        &fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?,
    ))
}

/// Bind the exact source, executable and fixture identity a shard verifies.
/// # Errors
/// Missing git identity, source files or verifier executable refuse.
pub(crate) fn shard_identity(root: &Path) -> Result<ShardIdentity, String> {
    // Contained commands do not inherit this process's working directory,
    // so the git identity must resolve an absolute repository path.
    let repository = root.canonicalize().map_err(|error| {
        format!(
            "{}: source root canonicalization failed: {error}",
            root.display()
        )
    })?;
    let mut command = CommandSpec::new("git");
    command.cwd = Some(repository);
    command.args = ["rev-parse".into(), "HEAD".into(), "HEAD^{tree}".into()].to_vec();
    let output = command.checked()?;
    let identity = String::from_utf8(output.stdout).map_err(|e| e.to_string())?;
    let mut fields = identity.split_whitespace();
    let source_commit = fields
        .next()
        .ok_or("source commit identity missing")?
        .to_owned();
    let source_tree = fields
        .next()
        .ok_or("source tree identity missing")?
        .to_owned();
    if fields.next().is_some() {
        return Err("unexpected source identity fields".into());
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut migrations = Vec::new();
    for entry in
        fs::read_dir(root.join("crates/extension/sql")).map_err(|error| error.to_string())?
    {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("pgokf--")
            && std::path::Path::new(&name)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("sql"))
        {
            migrations.push(name);
        }
    }
    migrations.sort();
    let mut manifest = String::new();
    for name in &migrations {
        let digest = sha256_file(&root.join("crates/extension/sql").join(name))?;
        let _ = writeln!(manifest, "{name} {digest}");
    }
    Ok(ShardIdentity {
        source_commit,
        source_tree,
        executable_sha256: sha256_file(&executable)?,
        extension_control_sha256: sha256_file(&root.join("crates/extension/pgokf.control"))?,
        install_sql_sha256: sha256_bytes(manifest.as_bytes()),
        fixture_sha256: sha256_file(
            &root.join("crates/pgokf-verifier/fixtures/upgrade-probes.json"),
        )?,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ShardResult {
    pub format_version: u32,
    pub shard_id: usize,
    pub indices: Vec<usize>,
    pub cases: Vec<RefusalCaseRecord>,
    pub start_phase: String,
    pub end_phase: String,
    pub source_commit: String,
    pub source_tree: String,
    pub executable_sha256: String,
    pub extension_control_sha256: String,
    pub install_sql_sha256: String,
    pub fixture_sha256: String,
    pub campaign_id: Option<String>,
    pub case_id: Option<String>,
    pub cleanup_succeeded: bool,
}

/// A published shard receipt must match the assigned partition, campaign,
/// source identity, executed cases and cleanup before any credit.
/// # Errors
/// Any relabeled, stale, incomplete or misbound shard evidence refuses.
pub(crate) fn validate_shard_result(
    p: &Probes,
    assignment: &ShardAssignment,
    result: &ShardResult,
    identity: &ShardIdentity,
    campaign: Option<&str>,
    case: Option<&str>,
) -> Result<(), String> {
    let id = assignment.id;
    if result.format_version != SHARD_FORMAT_VERSION {
        return Err("refusal shard result format version differs".into());
    }
    if result.shard_id != id {
        return Err(format!(
            "refusal shard identity differs: {}",
            result.shard_id
        ));
    }
    if result.indices != assignment.indices {
        return Err(format!(
            "refusal shard {id} indices differ from the assigned partition"
        ));
    }
    if result.campaign_id.as_deref() != campaign || result.case_id.as_deref() != case {
        return Err(format!(
            "refusal shard {id} campaign or case identity differs"
        ));
    }
    if !result.cleanup_succeeded {
        return Err(format!("refusal shard {id} cleanup is incomplete"));
    }
    for (name, actual, expected) in [
        (
            "source commit",
            &result.source_commit,
            &identity.source_commit,
        ),
        ("source tree", &result.source_tree, &identity.source_tree),
        (
            "verifier executable",
            &result.executable_sha256,
            &identity.executable_sha256,
        ),
        (
            "extension control",
            &result.extension_control_sha256,
            &identity.extension_control_sha256,
        ),
        (
            "install SQL",
            &result.install_sql_sha256,
            &identity.install_sql_sha256,
        ),
        (
            "refusal fixture",
            &result.fixture_sha256,
            &identity.fixture_sha256,
        ),
    ] {
        if actual != expected {
            return Err(format!("refusal shard {id} {name} identity differs"));
        }
    }
    validate_refusal_records(p, &assignment.indices, &result.cases)?;
    let Some(first) = result.cases.first() else {
        return Err(format!("refusal shard {id} executed no cases"));
    };
    let last = &result.cases[result.cases.len() - 1];
    if result.start_phase != first.phase || result.end_phase != last.phase {
        return Err(format!(
            "refusal shard {id} phase span differs from its executed cases"
        ));
    }
    Ok(())
}

fn read_shard_result(path: &Path) -> Result<ShardResult, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("refusal shard receipt missing: {}: {error}", path.display()))?;
    json::from_slice(&bytes).map_err(|error| format!("refusal shard receipt is not valid: {error}"))
}

/// The shard's durable phase journal must contain every executed case phase
/// with a successful end, no unfinished phase, and a successful cleanup.
/// # Errors
/// Missing, incomplete, failed or misbound journal phases refuse.
pub(crate) fn authenticate_shard_journal(
    path: &Path,
    cases: &[RefusalCaseRecord],
    campaign: Option<&str>,
    case: Option<&str>,
) -> Result<(), String> {
    let bytes =
        fs::read(path).map_err(|error| format!("refusal shard journal missing: {error}"))?;
    let mut starts: BTreeMap<String, u32> = BTreeMap::new();
    let mut ends: BTreeMap<String, (u32, bool)> = BTreeMap::new();
    for (sequence, line) in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let event: serde_json::Value = json::from_slice(line)
            .map_err(|error| format!("journal entry is not valid: {error}"))?;
        if event["format_version"] != 1 {
            return Err("journal entry format version differs".into());
        }
        if event["sequence"].as_u64()
            != Some(u64::try_from(sequence).map_err(|_| "journal entry sequence is not dense")?)
        {
            return Err("journal entry sequence is not dense".into());
        }
        let bound_campaign = event["campaign_id"].as_str();
        let bound_case = event["case_id"].as_str();
        if bound_campaign != campaign || bound_case != case {
            return Err("journal campaign or case identity differs".into());
        }
        let Some(phase) = event["phase"].as_str().map(str::to_owned) else {
            return Err("journal entry phase is missing".into());
        };
        match event["event"].as_str() {
            Some("start") => *starts.entry(phase).or_default() += 1,
            Some("end") => {
                let success = event["output"]["status"] == "success";
                let entry = ends.entry(phase).or_default();
                entry.0 += 1;
                entry.1 |= success;
            }
            _ => return Err("journal entry event kind is unknown".into()),
        }
    }
    for (phase, count) in &starts {
        if ends.get(phase).is_none_or(|entry| entry.0 != *count) {
            return Err(format!("journal phase {phase} lacks its end"));
        }
    }
    for record in cases {
        if starts.get(&record.phase).is_none_or(|count| *count == 0)
            || ends.get(&record.phase).is_none_or(|entry| !entry.1)
        {
            return Err(format!(
                "journal lacks a successful end for {}",
                record.phase
            ));
        }
    }
    if !ends
        .get("cleanup")
        .is_some_and(|entry| entry.1 && starts.get("cleanup").is_some_and(|count| *count > 0))
    {
        return Err("journal lacks a successful cleanup".into());
    }
    Ok(())
}

struct ShardLaunch {
    assignment: ShardAssignment,
    directory: PathBuf,
    binding: serde_json::Value,
    campaign: Option<String>,
    case: Option<String>,
    handle: JoinHandle<Result<Output, String>>,
}

/// The joined remains of a shard command, with its owned handle consumed.
struct ShardOutcome {
    assignment: ShardAssignment,
    directory: PathBuf,
    binding: serde_json::Value,
    campaign: Option<String>,
    case: Option<String>,
}

/// Joins still-running shard commands on every early exit so no owned
/// descendant outlives this command.
struct ShardGuard(Option<Vec<ShardLaunch>>);
impl Drop for ShardGuard {
    fn drop(&mut self) {
        if let Some(launches) = self.0.take() {
            for launch in launches {
                let _ = launch.handle.join();
            }
        }
    }
}

/// Launch exactly three private verifier shard commands, each with its own
/// bound receipt directory under the parent evidence root.
/// # Errors
/// Partition, identity, receipt-binding or spawn failures refuse.
fn launch_refusal_shards(c: &Cluster, o: &Options, p: &Probes) -> Result<Vec<ShardLaunch>, String> {
    let assignments = partition_refusals(p, REFUSAL_SHARDS)?;
    let executable = std::env::current_exe()
        .map_err(|error| format!("verifier executable identity failed: {error}"))?;
    let campaign = std::env::var("PGOKF_VERIFICATION_CAMPAIGN").ok();
    let case = std::env::var("PGOKF_VERIFICATION_CASE").ok();
    let mut launches = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let directory = c
            .service_receipts()
            .join(format!("shard-{}", assignment.id));
        fs::create_dir(&directory).map_err(|error| error.to_string())?;
        let directory = directory
            .canonicalize()
            .map_err(|error| error.to_string())?;
        let indices = assignment
            .indices
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut command = CommandSpec::new(&executable);
        // Contained commands start without this process's working directory;
        // give the shard the parent's so relative source roots stay identical.
        command.cwd = Some(
            std::env::current_dir()
                .map_err(|error| format!("parent working directory failed: {error}"))?,
        );
        command.args = vec![
            "reconciliation-shard".into(),
            "--shard-id".into(),
            assignment.id.to_string().into(),
            "--shard-indices".into(),
            indices.into(),
            "--source-root".into(),
            o.root.as_os_str().into(),
            "--pg-config".into(),
            o.pg_config.as_os_str().into(),
        ];
        if let Some(prefix) = &o.extension_prefix {
            command
                .args
                .extend(["--extension-prefix".into(), prefix.as_os_str().into()]);
        }
        if let Some(provider) = &o.preload {
            command.args.extend(["--preload".into(), provider.into()]);
        }
        if let Some(operator) = &o.operator_sql {
            command
                .args
                .extend(["--operator-sql".into(), operator.as_os_str().into()]);
        }
        if let Some(image) = &o.docker_image {
            command.args.extend(["--docker-image".into(), image.into()]);
        }
        if let Some(package) = &o.provider_package {
            command
                .args
                .extend(["--provider-package".into(), package.as_os_str().into()]);
        }
        if let Some(historical) = &o.historical_install {
            command
                .args
                .extend(["--historical-install".into(), historical.as_os_str().into()]);
        }
        if let Some(campaign) = &campaign {
            command
                .env
                .insert("PGOKF_VERIFICATION_CAMPAIGN".into(), campaign.into());
        }
        if let Some(case) = &case {
            command
                .env
                .insert("PGOKF_VERIFICATION_CASE".into(), case.clone().into());
        }
        // Bound inside the outer 600 000 ms campaign deadline so a hung
        // shard fails the parent before the outer kill.
        command.timeout = Duration::from_mins(9);
        command.durable_stdout = Some(directory.join("child.stdout.partial"));
        command.durable_stderr = Some(directory.join("child.stderr.partial"));
        let binding = upgrade::bind_service_receipts(&mut command, &directory)?;
        crate::owned_docker::publish(
            &directory.join("launch.json"),
            &serde_json::json!({
                "program": executable.to_string_lossy(),
                "shard_id": assignment.id,
                "indices": assignment.indices,
                "timeout_ms": command.timeout.as_secs() * 1000,
                "stdout": command.durable_stdout.as_ref().map(|path| path.to_string_lossy()),
                "stderr": command.durable_stderr.as_ref().map(|path| path.to_string_lossy()),
                "service_receipts": binding,
            }),
        )?;
        launches.push(ShardLaunch {
            assignment,
            directory,
            binding,
            campaign: campaign.clone(),
            case: case.clone(),
            handle: std::thread::spawn(move || command.run()),
        });
    }
    Ok(launches)
}

/// Reject a shard command that timed out, failed or exited nonzero.
/// # Errors
/// Command transport failures and nonzero exits refuse.
fn require_shard_exit(shard: usize, outcome: Result<Output, String>) -> Result<(), String> {
    let output =
        outcome.map_err(|error| format!("refusal shard {shard} command failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "refusal shard {shard} exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr[output.stderr.len().saturating_sub(4096)..])
        ));
    }
    Ok(())
}

/// Wait for every shard, then bind each receipt to the assigned partition,
/// the unchanged source identity, the authenticated journal and the cleanup
/// before any label is credited.
/// # Errors
/// Missing receipts, stale identity, nonzero exits, incomplete cleanup,
/// overlapping or gapped indices all refuse before report credit.
fn join_refusal_shards(
    o: &Options,
    p: &Probes,
    launches: Vec<ShardLaunch>,
) -> Result<Vec<String>, String> {
    let identity = shard_identity(&o.root)?;
    let mut joined = Vec::with_capacity(launches.len());
    for launch in launches {
        let outcome = launch
            .handle
            .join()
            .map_err(|_| "refusal shard thread panicked".to_string())?;
        joined.push((
            ShardOutcome {
                assignment: launch.assignment,
                directory: launch.directory,
                binding: launch.binding,
                campaign: launch.campaign,
                case: launch.case,
            },
            outcome,
        ));
    }
    for (launch, outcome) in joined {
        require_shard_exit(launch.assignment.id, outcome)?;
        let receipt = launch.directory.join("shard-result.json");
        let result = read_shard_result(&receipt)?;
        let binding = upgrade::validate_service_receipts(
            &launch.directory,
            launch.binding["device"].as_str(),
            launch.binding["inode"].as_str(),
        )?;
        if binding != launch.binding {
            return Err(format!(
                "refusal shard {} receipt directory identity differs",
                launch.assignment.id
            ));
        }
        validate_shard_result(
            p,
            &launch.assignment,
            &result,
            &identity,
            launch.campaign.as_deref(),
            launch.case.as_deref(),
        )?;
        authenticate_shard_journal(
            &launch.directory.join("phase-journal.jsonl"),
            &result.cases,
            launch.campaign.as_deref(),
            launch.case.as_deref(),
        )?;
    }
    Ok(p.refusals.iter().map(|case| case.0.clone()).collect())
}

/// Parse a shard's explicit ordered fixture-index set.
/// # Errors
/// Empty fields or non-numeric indices refuse.
fn parse_shard_indices(value: &str) -> Result<Vec<usize>, String> {
    let mut indices = Vec::new();
    for field in value.split(',') {
        indices.push(
            field
                .parse()
                .map_err(|_| format!("invalid refusal index: {field}"))?,
        );
    }
    Ok(indices)
}

/// Execute one refusal shard in a private cluster and publish its bound
/// receipt. Every case keeps the unchanged drift, refusal, repair, committed
/// update and complete reconciliation sequence in a freshly created database.
/// # Errors
/// Cluster startup, installed identity, baseline, case or cleanup failures refuse.
fn run_shard(o: &Options, shard_id: usize, indices: &[usize]) -> Result<(), String> {
    let p = upgrade::probes()?;
    refusal_plans(&p, indices)?;
    catalog::check_exceptions(&read(o.root.join("tests/upgrade/catalog-exceptions.json"))?)?;
    let version = crate::release::compatibility_source(&o.root)?;
    let identity = shard_identity(&o.root)?;
    let mut cluster = Cluster::start(o)?;
    upgrade::verify_installed(&cluster, o)?;
    let inventory = read(o.root.join("tests/upgrade/inventory.sql"))?;
    let seed = read(o.root.join("tests/upgrade/seed.sql"))?;
    let (expected, metadata) = cluster.observed("shard.fresh-baseline", || {
        upgrade::fresh_baseline(&cluster, &p, &inventory, &version)
    })?;
    let (registry, script, web, behaviour, positional) =
        cluster.observed("shard.web-baseline", || web_baseline(&cluster, o, &p))?;
    let mut cases = Vec::new();
    {
        let gate = Gate {
            c: &cluster,
            o,
            p: &p,
            expected: &expected,
            metadata: &metadata,
            inventory: &inventory,
            seed: &seed,
            version: &version,
            script,
            registry,
            web,
            behaviour,
            positional,
        };
        let mut ops = GateRefusals { gate: &gate };
        run_refusal_cases(&p, indices, &mut ops, &mut |record| {
            println!("refusal {}: passed", record.label);
            cases.push(record.clone());
            Ok(())
        })?;
    }
    cluster.close()?;
    let Some(first) = cases.first() else {
        return Err("refusal shard executed no cases".into());
    };
    let result = ShardResult {
        format_version: SHARD_FORMAT_VERSION,
        shard_id,
        indices: indices.to_vec(),
        start_phase: first.phase.clone(),
        end_phase: cases[cases.len() - 1].phase.clone(),
        source_commit: identity.source_commit,
        source_tree: identity.source_tree,
        executable_sha256: identity.executable_sha256,
        extension_control_sha256: identity.extension_control_sha256,
        install_sql_sha256: identity.install_sql_sha256,
        fixture_sha256: identity.fixture_sha256,
        campaign_id: std::env::var("PGOKF_VERIFICATION_CAMPAIGN").ok(),
        case_id: std::env::var("PGOKF_VERIFICATION_CASE").ok(),
        cleanup_succeeded: true,
        cases,
    };
    crate::owned_docker::publish(
        &cluster.service_receipts().join("shard-result.json"),
        &result,
    )?;
    Ok(())
}

/// Private verifier entry for one refusal shard; not a public command.
/// # Errors
/// Unknown or incomplete shard options and any shard failure refuse.
pub fn shard_cli(args: &[String]) -> Result<(), String> {
    let mut o = Options {
        root: std::env::current_dir().map_err(|e| e.to_string())?,
        pg_config: "pg_config".into(),
        preload: None,
        extension_prefix: None,
        operator_sql: None,
        reconciliation_only: true,
        docker_image: None,
        historical_install: None,
        provider_package: None,
        probe: None,
        embedding_policy: catalog::EmbeddingPolicy::NotRequested,
        embedding_dimensions: None,
    };
    let mut shard_id = None;
    let mut indices = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--shard-id" => {
                shard_id = Some(
                    iter.next()
                        .ok_or("shard id missing")?
                        .parse()
                        .map_err(|_| "invalid shard id")?,
                );
            }
            "--shard-indices" => {
                indices = Some(parse_shard_indices(
                    iter.next().ok_or("shard indices missing")?,
                )?);
            }
            "--source-root" => o.root = iter.next().ok_or("source root missing")?.into(),
            "--pg-config" => o.pg_config = iter.next().ok_or("pg_config missing")?.into(),
            "--extension-prefix" => {
                o.extension_prefix = Some(iter.next().ok_or("prefix missing")?.into());
            }
            "--preload" => o.preload = Some(iter.next().ok_or("provider missing")?.clone()),
            "--operator-sql" => {
                o.operator_sql = Some(iter.next().ok_or("operator SQL missing")?.into());
            }
            "--docker-image" => {
                o.docker_image = Some(iter.next().ok_or("docker image missing")?.clone());
            }
            "--provider-package" => {
                o.provider_package = Some(iter.next().ok_or("package missing")?.into());
            }
            "--historical-install" => {
                o.historical_install = Some(iter.next().ok_or("historical SQL missing")?.into());
            }
            _ => return Err(format!("unknown shard option: {arg}")),
        }
    }
    let shard_id = shard_id.ok_or("shard id is required")?;
    let indices = indices.ok_or("shard indices are required")?;
    run_shard(&o, shard_id, &indices)
}

/// # Errors
/// Fixture updates require an explicit reviewed pin, rather than dropping probes.
pub fn check_fixture(root: &std::path::Path) -> Result<(), String> {
    let path = root.join("crates/extension/src/catalog");
    let sources =
        crate::positional::rust_strings(&read(path.join("web_identity.rs"))?, &path, root)?;
    let fresh = sources
        .iter()
        .filter(|sql| sql.contains("CREATE TABLE pgokf_web.users"))
        .collect::<Vec<_>>();
    if fresh.len() != 1 {
        return Err("fresh web catalog source identity is ambiguous".into());
    }
    crate::positional::check_live_drift_fixture(
        &read(root.join("tests/upgrade/live-drift.sql"))?,
        fresh[0],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulated hostile database for the refusal oracle: knows the true
    /// diagnostics and which cases require a repair before the committed
    /// update can succeed.
    struct RefusalSimulator {
        repairable: Vec<bool>,
        diagnostics: Vec<String>,
        current: usize,
        repaired: bool,
        noop_repair: bool,
        calls: Vec<&'static str>,
    }
    impl RefusalSimulator {
        fn new(p: &Probes) -> Self {
            Self {
                repairable: p.refusals.iter().map(|case| case.3.is_some()).collect(),
                diagnostics: p.refusals.iter().map(|case| case.2.clone()).collect(),
                current: 0,
                repaired: false,
                noop_repair: false,
                calls: Vec::new(),
            }
        }
    }
    impl RefusalCaseOps for RefusalSimulator {
        fn drifted(&mut self) -> Result<(String, String), String> {
            self.current += 1;
            self.repaired = false;
            self.calls.push("drifted");
            Ok(("fingerprint".into(), "before".into()))
        }
        fn setup(&mut self, _: &str) -> Result<(), String> {
            self.calls.push("setup");
            Ok(())
        }
        fn refuse(&mut self, diagnostic: &str) -> Result<(), String> {
            self.calls.push("refuse");
            let expected = &self.diagnostics[self.current - 1];
            if diagnostic == expected {
                Ok(())
            } else {
                Err(format!("wrong refusal: expected {expected}"))
            }
        }
        fn repair(&mut self, _: &str) -> Result<(), String> {
            self.calls.push("repair");
            if !self.noop_repair {
                self.repaired = true;
            }
            Ok(())
        }
        fn update(&mut self) -> Result<(), String> {
            self.calls.push("update");
            if self.repairable[self.current - 1] && !self.repaired {
                return Err("unsafe acceptance: update succeeded without repair".into());
            }
            Ok(())
        }
        fn reconciled(&mut self, _: &str, _: &str) -> Result<(), String> {
            self.calls.push("reconciled");
            Ok(())
        }
        fn drop_case(&mut self) -> Result<(), String> {
            self.calls.push("drop");
            Ok(())
        }
        fn observed_case(
            &mut self,
            _: &str,
            case: impl FnOnce(&mut Self) -> Result<(), String>,
        ) -> Result<(), String> {
            case(self)
        }
    }

    fn all_indices(p: &Probes) -> Vec<usize> {
        (0..p.refusals.len()).collect()
    }

    fn case_records(p: &Probes, indices: &[usize]) -> Vec<RefusalCaseRecord> {
        indices
            .iter()
            .map(|&index| RefusalCaseRecord {
                index,
                label: p.refusals[index].0.clone(),
                phase: format!("reconciliation.refusal-{index:02}-{}", p.refusals[index].0),
                repair: p.refusals[index].3.is_some(),
            })
            .collect()
    }

    fn mutated_probes(index: usize, diagnostic: &str) -> Probes {
        let mut value: serde_json::Value =
            json::from_slice(include_bytes!("../fixtures/upgrade-probes.json")).unwrap();
        value["refusals"][index][2] = serde_json::json!(diagnostic);
        json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap()
    }

    fn synthetic_identity() -> ShardIdentity {
        ShardIdentity {
            source_commit: "a".repeat(40),
            source_tree: "b".repeat(40),
            executable_sha256: "c".repeat(64),
            extension_control_sha256: "d".repeat(64),
            install_sql_sha256: "e".repeat(64),
            fixture_sha256: "f".repeat(64),
        }
    }

    fn valid_result(
        p: &Probes,
        assignment: &ShardAssignment,
        identity: &ShardIdentity,
    ) -> ShardResult {
        let cases = case_records(p, &assignment.indices);
        ShardResult {
            format_version: SHARD_FORMAT_VERSION,
            shard_id: assignment.id,
            indices: assignment.indices.clone(),
            start_phase: cases[0].phase.clone(),
            end_phase: cases[cases.len() - 1].phase.clone(),
            source_commit: identity.source_commit.clone(),
            source_tree: identity.source_tree.clone(),
            executable_sha256: identity.executable_sha256.clone(),
            extension_control_sha256: identity.extension_control_sha256.clone(),
            install_sql_sha256: identity.install_sql_sha256.clone(),
            fixture_sha256: identity.fixture_sha256.clone(),
            campaign_id: None,
            case_id: None,
            cleanup_succeeded: true,
            cases,
        }
    }

    #[test]
    fn three_shard_partition_is_exact_ordered_balanced_and_deterministic() {
        let p = upgrade::probes().unwrap();
        assert_eq!(p.refusals.len(), 41);
        let shards = partition_refusals(&p, REFUSAL_SHARDS).unwrap();
        validate_partitions(REFUSAL_SHARDS, p.refusals.len(), &shards).unwrap();
        assert_eq!(shards, partition_refusals(&p, REFUSAL_SHARDS).unwrap());
        let weight = |case: &(String, String, String, Option<String>)| {
            if case.3.is_some() {
                REPAIRABLE_REFUSAL_WEIGHT
            } else {
                TERMINAL_REFUSAL_WEIGHT
            }
        };
        let total: u64 = p.refusals.iter().map(weight).sum();
        for shard in &shards {
            assert!(!shard.indices.is_empty());
        }
        let loads: Vec<u64> = shards
            .iter()
            .map(|shard| {
                shard
                    .indices
                    .iter()
                    .map(|&index| weight(&p.refusals[index]))
                    .sum()
            })
            .collect();
        assert_eq!(loads.iter().sum::<u64>(), total);
        let spread = loads.iter().max().unwrap() - loads.iter().min().unwrap();
        assert!(
            spread <= REPAIRABLE_REFUSAL_WEIGHT,
            "declared weights are unbalanced: {loads:?}"
        );
    }

    #[test]
    fn partition_validation_rejects_overlap_gaps_and_reordering() {
        let p = upgrade::probes().unwrap();
        let count = p.refusals.len();
        let live = partition_refusals(&p, REFUSAL_SHARDS).unwrap();
        validate_partitions(REFUSAL_SHARDS, count, &live).unwrap();
        let assignment = |id: usize, indices: &[usize]| ShardAssignment {
            id,
            indices: indices.to_vec(),
        };
        // Synthetic exact cover over a five-case fixture shape.
        let exact = vec![
            assignment(0, &[0, 1]),
            assignment(1, &[2, 3]),
            assignment(2, &[4]),
        ];
        validate_partitions(3, 5, &exact).unwrap();
        let gap = vec![
            assignment(0, &[0, 1]),
            assignment(1, &[3]),
            assignment(2, &[4]),
        ];
        assert!(validate_partitions(3, 5, &gap).is_err());
        let overlap = vec![
            assignment(0, &[0, 1]),
            assignment(1, &[1, 2]),
            assignment(2, &[3, 4]),
        ];
        assert!(validate_partitions(3, 5, &overlap).is_err());
        let reordered = vec![
            ShardAssignment {
                id: 0,
                indices: vec![1, 0],
            },
            assignment(1, &[2, 3]),
            assignment(2, &[4]),
        ];
        assert!(validate_partitions(3, 5, &reordered).is_err());
        let missing = vec![assignment(0, &[0, 1]), assignment(1, &[2, 3])];
        assert!(validate_partitions(3, 5, &missing).is_err());
        let relabeled = vec![
            assignment(7, &[0, 1]),
            assignment(1, &[2, 3]),
            assignment(2, &[4]),
        ];
        assert!(validate_partitions(3, 5, &relabeled).is_err());
        let empty = vec![
            assignment(0, &[]),
            assignment(1, &[2, 3]),
            assignment(2, &[4]),
        ];
        assert!(validate_partitions(3, 5, &empty).is_err());
        let outside = vec![
            assignment(0, &[0, 1]),
            assignment(1, &[2, 3]),
            assignment(2, &[4, 5]),
        ];
        assert!(validate_partitions(3, 5, &outside).is_err());
        // Live partitions gapped or overlapped by hand also refuse.
        let mut gapped = live.clone();
        if let Some(first) = gapped[0].indices.first().copied() {
            gapped[0].indices.retain(|&index| index != first);
            assert!(validate_partitions(REFUSAL_SHARDS, count, &gapped).is_err());
        }
        let mut overlapped = live.clone();
        let stolen = overlapped[2].indices.pop().unwrap();
        overlapped[1].indices.insert(0, stolen);
        assert!(validate_partitions(REFUSAL_SHARDS, count, &overlapped).is_err());
    }

    #[test]
    fn one_worker_mode_reproduces_the_serial_refusal_labels() {
        let p = upgrade::probes().unwrap();
        let indices = all_indices(&p);
        let mut simulator = RefusalSimulator::new(&p);
        let mut labels: Vec<String> = Vec::new();
        run_refusal_cases(&p, &indices, &mut simulator, &mut |record| {
            labels.push(record.label.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            labels,
            p.refusals
                .iter()
                .map(|case| case.0.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(simulator.current, p.refusals.len());
        let mut expected_calls: Vec<&str> = Vec::new();
        for case in &p.refusals {
            expected_calls.extend(["drifted", "setup", "refuse"]);
            if case.3.is_some() {
                expected_calls.extend(["repair", "update", "reconciled"]);
            } else {
                expected_calls.push("drop");
            }
        }
        assert_eq!(simulator.calls, expected_calls);
    }

    #[test]
    fn planted_wrong_diagnostic_and_skipped_repair_fail() {
        let p = upgrade::probes().unwrap();
        // Wrong diagnostic: the runner is driven by a stale fixture while the
        // hostile state still produces the original refusal text.
        let mutated = mutated_probes(3, "hostile wrong diagnostic");
        let mut wrong = RefusalSimulator::new(&p);
        assert!(
            run_refusal_cases(&mutated, &all_indices(&p), &mut wrong, &mut |_| Ok(()))
                .unwrap_err()
                .contains("wrong refusal")
        );
        // Skipped repair: a no-op repair leaves the hostile state in place,
        // so the committed update must refuse.
        let mut skipped = RefusalSimulator::new(&p);
        skipped.noop_repair = true;
        assert!(
            run_refusal_cases(&p, &all_indices(&p), &mut skipped, &mut |_| Ok(()))
                .unwrap_err()
                .contains("without repair")
        );
    }

    #[test]
    fn refusal_plans_and_records_reject_omitted_duplicated_and_relabeled_cases() {
        let p = upgrade::probes().unwrap();
        let indices = all_indices(&p);
        let records = case_records(&p, &indices);
        validate_refusal_records(&p, &indices, &records).unwrap();
        let mut omitted = records.clone();
        omitted.pop();
        assert!(validate_refusal_records(&p, &indices, &omitted).is_err());
        let mut duplicated = records.clone();
        duplicated.push(records[records.len() - 1].clone());
        assert!(validate_refusal_records(&p, &indices, &duplicated).is_err());
        let mut relabeled = records.clone();
        relabeled[2].label = "relabeled".into();
        assert!(validate_refusal_records(&p, &indices, &relabeled).is_err());
        let mut rephased = records.clone();
        rephased[2].phase = "reconciliation.refusal-99-relabeled".into();
        assert!(validate_refusal_records(&p, &indices, &rephased).is_err());
        let mut unbound = records.clone();
        unbound[0].repair = !unbound[0].repair;
        assert!(validate_refusal_records(&p, &indices, &unbound).is_err());
        let mut misbound = records.clone();
        misbound[1].index = 5;
        assert!(validate_refusal_records(&p, &indices, &misbound).is_err());
        assert!(refusal_plans(&p, &[1, 1]).is_err());
        assert!(refusal_plans(&p, &[4, 2]).is_err());
        assert!(refusal_plans(&p, &[]).is_err());
        assert!(refusal_plans(&p, &[p.refusals.len()]).is_err());
        let plans = refusal_plans(&p, &[0, 1, 2]).unwrap();
        assert_eq!(plans[1].label, p.refusals[1].0);
        assert_eq!(
            plans[1].phase,
            format!("reconciliation.refusal-01-{}", p.refusals[1].0)
        );
    }

    #[test]
    fn shard_results_reject_stale_identity_incomplete_cleanup_and_misbinding() {
        let p = upgrade::probes().unwrap();
        let identity = synthetic_identity();
        let assignments = partition_refusals(&p, REFUSAL_SHARDS).unwrap();
        for assignment in &assignments {
            let result = valid_result(&p, assignment, &identity);
            validate_shard_result(&p, assignment, &result, &identity, None, None).unwrap();
            let mut stale = result.clone();
            stale.fixture_sha256 = "stale".into();
            assert!(validate_shard_result(&p, assignment, &stale, &identity, None, None).is_err());
            let mut dirty = result.clone();
            dirty.cleanup_succeeded = false;
            assert!(validate_shard_result(&p, assignment, &dirty, &identity, None, None).is_err());
            let mut gapped = result.clone();
            gapped.indices.pop();
            assert!(validate_shard_result(&p, assignment, &gapped, &identity, None, None).is_err());
            let mut reordered = result.clone();
            reordered.indices.reverse();
            assert!(
                validate_shard_result(&p, assignment, &reordered, &identity, None, None).is_err()
            );
            let mut campaigned = result.clone();
            campaigned.campaign_id = Some("other-campaign".into());
            assert!(
                validate_shard_result(&p, assignment, &campaigned, &identity, None, None).is_err()
            );
            let mut mislabeled = result.clone();
            mislabeled.shard_id += 1;
            assert!(
                validate_shard_result(&p, assignment, &mislabeled, &identity, None, None).is_err()
            );
            let mut span = result.clone();
            span.start_phase = "reconciliation.refusal-99-x".into();
            assert!(validate_shard_result(&p, assignment, &span, &identity, None, None).is_err());
            let mut omitted = result.clone();
            omitted.cases.pop();
            assert!(
                validate_shard_result(&p, assignment, &omitted, &identity, None, None).is_err()
            );
        }
    }

    #[test]
    fn shard_receipts_and_exits_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("absent.json");
        assert!(read_shard_result(&missing).is_err());
        let p = upgrade::probes().unwrap();
        let identity = synthetic_identity();
        let assignment = partition_refusals(&p, REFUSAL_SHARDS).unwrap().remove(0);
        let result = valid_result(&p, &assignment, &identity);
        let receipt = directory.path().join("shard-result.json");
        let text = serde_json::to_string(&result).unwrap();
        for binding in [
            "source_commit",
            "source_tree",
            "executable_sha256",
            "extension_control_sha256",
            "install_sql_sha256",
            "fixture_sha256",
            "campaign_id",
            "case_id",
            "shard_id",
            "indices",
            "start_phase",
            "end_phase",
            "cleanup_succeeded",
            "cases",
        ] {
            assert!(text.contains(binding), "child evidence lacks {binding}");
        }
        fs::write(&receipt, &text).unwrap();
        assert_eq!(read_shard_result(&receipt).unwrap(), result);
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("cleanup_succeeded")
            .unwrap();
        fs::write(&receipt, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(read_shard_result(&receipt).is_err());
        assert!(require_shard_exit(0, Err("timeout exceeded".into())).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            let failed = Output {
                status: std::process::ExitStatus::from_raw(256),
                stdout: Vec::new(),
                stderr: b"refusal boom".to_vec(),
            };
            let error = require_shard_exit(1, Ok(failed)).unwrap_err();
            assert!(error.contains("refusal boom"));
            let succeeded = Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
            };
            require_shard_exit(2, Ok(succeeded)).unwrap();
        }
    }

    #[test]
    fn shard_journal_authentication_requires_complete_success_phases() {
        let directory = tempfile::tempdir().unwrap();
        let p = upgrade::probes().unwrap();
        let indices: Vec<usize> = (0..2).collect();
        let records = case_records(&p, &indices);
        let entry = |sequence: u64, phase: &str, event: &str, status: Option<&str>| {
            let mut value = serde_json::json!({
                "format_version": 1,
                "sequence": sequence,
                "phase": phase,
                "event": event,
                "campaign_id": serde_json::Value::Null,
                "case_id": serde_json::Value::Null,
                "unix_ms": 0,
                "campaign_elapsed_ms": 0,
            });
            if let Some(status) = status {
                value["output"] = serde_json::json!({"status": status});
                value["start_sequence"] = serde_json::json!(sequence - 1);
                value["elapsed_ms"] = serde_json::json!(0);
            }
            value
        };
        let write = |name: &str, events: &[serde_json::Value]| {
            let path = directory.path().join(name);
            let mut text = String::new();
            for event in events {
                text.push_str(&serde_json::to_string(event).unwrap());
                text.push('\n');
            }
            fs::write(&path, text).unwrap();
            path
        };
        let mut complete: Vec<serde_json::Value> = Vec::new();
        let mut sequence = 0u64;
        for record in &records {
            complete.push(entry(sequence, &record.phase, "start", None));
            sequence += 1;
            complete.push(entry(sequence, &record.phase, "end", Some("success")));
            sequence += 1;
        }
        complete.push(entry(sequence, "cleanup", "start", None));
        complete.push(entry(sequence + 1, "cleanup", "end", Some("success")));
        let path = write("complete.jsonl", &complete);
        authenticate_shard_journal(&path, &records, None, None).unwrap();

        let mut failed_case = complete.clone();
        failed_case[3] = entry(3, &records[1].phase, "end", Some("error"));
        let path = write("failed-case.jsonl", &failed_case);
        assert!(authenticate_shard_journal(&path, &records, None, None).is_err());

        let mut unfinished = complete.clone();
        unfinished.remove(3);
        let path = write("unfinished.jsonl", &unfinished);
        assert!(authenticate_shard_journal(&path, &records, None, None).is_err());

        let mut dirty_cleanup = complete.clone();
        let last = dirty_cleanup.len() - 1;
        dirty_cleanup[last] = entry(last as u64, "cleanup", "end", Some("error"));
        let path = write("dirty-cleanup.jsonl", &dirty_cleanup);
        assert!(authenticate_shard_journal(&path, &records, None, None).is_err());

        let mut no_cleanup = complete.clone();
        no_cleanup.pop();
        no_cleanup.pop();
        let path = write("no-cleanup.jsonl", &no_cleanup);
        assert!(authenticate_shard_journal(&path, &records, None, None).is_err());

        let mut campaign_bound = complete.clone();
        campaign_bound[2]["campaign_id"] = serde_json::json!("other-campaign");
        let path = write("campaign.jsonl", &campaign_bound);
        assert!(authenticate_shard_journal(&path, &records, None, None).is_err());

        let missing = directory.path().join("absent.jsonl");
        assert!(authenticate_shard_journal(&missing, &records, None, None).is_err());
    }

    #[test]
    fn shard_index_parsing_is_strict() {
        assert_eq!(parse_shard_indices("0,1,2").unwrap(), vec![0, 1, 2]);
        for rejected in ["", "1,x", " 1", "1,,2", "-1"] {
            crate::semantic_assert!(
                "shard_index_parsing_is_strict",
                parse_shard_indices(rejected).is_err(),
                "{rejected}"
            );
        }
    }

    #[test]
    fn shard_identity_binds_the_exact_repository_state() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let identity = shard_identity(&root).unwrap();
        assert_eq!(identity.source_commit.len(), 40);
        assert_eq!(identity.source_tree.len(), 40);
        assert_eq!(identity.executable_sha256.len(), 64);
        assert_eq!(identity.extension_control_sha256.len(), 64);
        assert_eq!(identity.install_sql_sha256.len(), 64);
        assert_eq!(identity.fixture_sha256.len(), 64);
        assert_eq!(identity, shard_identity(&root).unwrap());
    }
}
