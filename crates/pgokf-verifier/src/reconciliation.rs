// SPDX-License-Identifier: AGPL-3.0-only
//! Live-shaped reconciliation, atomic refusals and semantic catalog controls.
use crate::{
    catalog, json,
    upgrade::{self, Cluster, Options, Probes, Registry, Report, equal, literal, read},
};

use sha2::{Digest, Sha256};
use std::fmt::Write as _;

fn rowtypes(c: &Cluster, db: &str) -> Result<String, String> {
    let snapshot = c.sql(db, include_str!("../sql/catalog-snapshot.sql"))?;
    let parsed: catalog::CatalogSnapshot = json::from_slice(snapshot.as_bytes())?;
    if !parsed.rowtype_dependents.is_empty() {
        return Err(format!(
            "objects depend on the web identity row types: {:?}",
            parsed.rowtype_dependents
        ));
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
    rowtypes(c, "exception_probe")?;
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
            return Err(format!("exception probe {label}: UNSAFE ACCEPTANCE"));
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
        rowtypes(self.c, db)?;
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
            equal(order,&self.c.sql(db,&format!("SELECT string_agg(attname, ',' ORDER BY attnum) FROM pg_attribute WHERE attrelid='{table}'::regclass AND attnum>0 AND NOT attisdropped;"))?,"declared physical order")?;
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
            equal(
                "f",
                &self.c.sql(
                    db,
                    &format!(
                        "SELECT to_regprocedure({}) IS NOT NULL;",
                        literal(signature)
                    ),
                )?,
                "retired overload",
            )?;
        }
        equal(
            "f",
            &self.c.sql(
                db,
                &format!(
                    "SELECT to_regprocedure({}) IS NULL;",
                    literal(&self.p.helper)
                ),
            )?,
            "current helper",
        )?;
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
            return Err("reconciled function metadata drift".into());
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
) -> Result<(), String> {
    if o.probe.is_none() || o.probe == Some(upgrade::Probe::Exceptions) {
        exceptions(c, p, expected, inventory, r)?;
    }
    if o.probe == Some(upgrade::Probe::Exceptions) {
        return Ok(());
    }
    let registry: Registry = json::from_slice(
        &std::fs::read(o.root.join("tests/upgrade/web-identity-statements.json"))
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
    gate.drifted("factory", "0.3.0-dev2", "0.3.0-dev2", false)?;
    gate.drifted("probe", "0.3.0-dev2", "0.3.0-dev2", true)?;
    let keys = catalog::differing_keys(&c.sql("factory", inventory)?, &c.sql("probe", inventory)?)?;
    if keys != p.live_drift_manifest.iter().cloned().collect() {
        return Err(format!("live-drift fixture manifest differs: {keys:?}"));
    }
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
    c.drop_catalog("probe")?;
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
        let (_, setup, diagnostic, _) = p
            .refusals
            .iter()
            .find(|case| case.0 == label)
            .ok_or("refusal probe absent")?;
        gate.drifted("refusal", "0.3.0-dev2", "0.3.0-dev2", true)?;
        c.sql("refusal", setup)?;
        upgrade::refusal(c, "refusal", inventory, diagnostic, "0.3.0-dev2")?;
        c.drop_catalog("refusal")?;
        r.passed(format!("targeted refusal {label}"));
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
        let drift_at = if at == "0.4.0-dev2" { "0.3.0-dev2" } else { at };
        let (fp, before) = gate.drifted(route, at, drift_at, true)?;
        if user != "postgres" {
            c.sql("postgres", &format!("CREATE ROLE {user} LOGIN SUPERUSER;"))?;
        }
        gate.update(route, user)?;
        gate.reconciled(route, &before, &fp)?;
        if user != "postgres" {
            c.sql("postgres", &format!("DROP ROLE {user};"))?;
        }
        r.passed(format!("reconciliation {route}"));
    }
    if o.probe.is_some() {
        return Ok(());
    }
    let (fp, before) = gate.drifted("interrupted", "0.3.0-dev2", "0.3.0-dev2", true)?;
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
    for (label, setup, diagnostic, repair) in &p.refusals {
        let (fp, before) = gate.drifted("refusal", "0.3.0-dev2", "0.3.0-dev2", true)?;
        c.sql("refusal", setup)?;
        upgrade::refusal(c, "refusal", inventory, diagnostic, "0.3.0-dev2")?;
        if let Some(repair) = repair {
            c.sql("refusal", repair)?;
            gate.update("refusal", "postgres")?;
            gate.reconciled("refusal", &before, &fp)?;
        } else {
            c.drop_catalog("refusal")?;
        }
        r.passed(format!("refusal {label}"));
    }
    Ok(())
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
