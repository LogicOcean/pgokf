//! Live fault injection at the Docker observation boundary. These tests require
//! explicitly supplied local images and retain receipts in the evidence root.
use pgokf_verifier::owned_docker::{Docker, DockerApi};
use std::path::Path;
struct Fault {
    docker: Docker,
    needle: String,
    replacement: String,
    occurrence: usize,
    seen: usize,
    main_id: Option<String>,
    restored: bool,
}
impl DockerApi for Fault {
    fn call(&mut self, args: &[String]) -> Result<String, String> {
        if args.iter().any(|s| s == "pgokf-restore") {
            self.restored = true;
        }
        let result = self.docker.call(args);
        if args.first().is_some_and(|s| s == "start") && args.len() == 2 && self.main_id.is_none() {
            self.main_id = Some(args[1].clone());
        }
        let correct_container =
            if self.needle.contains("pgokf.health()") || self.replacement == "$WAIT" {
                self.main_id
                    .as_ref()
                    .is_some_and(|id| (args.get(1) == Some(id)) == (self.occurrence == 1))
            } else {
                true
            };
        if correct_container
            && !(self.replacement == "$WAIT" && self.restored)
            && args.iter().any(|s| s.contains(&self.needle))
        {
            self.seen += 1;
            if self.needle.contains("pgokf.health()")
                || self.replacement == "$WAIT"
                || self.occurrence == usize::MAX
                || self.seen == self.occurrence
                || (self.occurrence == 0 && result.as_ref().is_ok_and(|s| s.trim() == "768"))
            {
                if self.replacement == "$DUPLICATE" {
                    let original = result?;
                    return Ok(format!("{}\n{}", original.trim(), original.trim()));
                }
                return Ok(self.replacement.clone());
            }
        }
        result
    }
}
#[allow(clippy::too_many_lines)] // Keep the closed observation fault table together.
fn server_fault(fault: &str) -> (&'static str, &'static str, usize, &'static str) {
    match fault {
        "version" => (
            "SELECT extversion FROM pg_extension",
            "wrong",
            usize::MAX,
            "initialization deadline",
        ),
        "policy_init" => (
            "SELECT pgokf.get_config() ->> 'embedding_dim'",
            "$WAIT",
            1,
            "initialization deadline",
        ),
        "restore_init" => (
            "SELECT pgokf.get_config() ->> 'embedding_dim'",
            "$WAIT",
            2,
            "initialization deadline",
        ),
        "roots" => (
            "SELECT pgokf.get_config() -> 'allowed_roots'",
            "[]",
            1,
            "allowed_roots policy",
        ),
        "extensions" => (
            "SELECT string_agg(extname",
            "",
            1,
            "optional extensions initialized",
        ),
        "roles" => (
            "SELECT string_agg(r.rolname",
            "",
            1,
            "env-driven login roles",
        ),
        "materialization" => (
            "SELECT count(*) FROM pgokf.concepts",
            "0",
            usize::MAX,
            "sample materialized zero concepts",
        ),
        "cron_binding" => (
            "SELECT command FROM cron.job",
            "SELECT 1",
            1,
            "cron tenant binding",
        ),
        "cron_removed" => ("SELECT pgokf.unschedule_refresh", "f", 1, "cron removed"),
        "health" => (
            "SELECT pgokf.health() ->> 'ok'",
            "false",
            1,
            "catalog health",
        ),
        "bm25_build" => ("SELECT pgokf.rebuild_search_index()", "f", 1, "BM25 built"),
        "bm25_reader" => (
            "SELECT concept_id FROM pgokf.concept_search",
            "",
            1,
            "BM25 reader search returned no hit",
        ),
        "vector" => (
            "SELECT pgokf.search_index_status()",
            "false",
            1,
            "pgvector visibility",
        ),
        "backup_count" => ("postgres-*.dump", "$DUPLICATE", 1, "exactly one archive"),
        "backup_data" => ("--list", "", 1, "backup omits concept table data"),
        "restore_count" => (
            "SELECT count(*) FROM pgokf.concepts",
            "999999",
            2,
            "restored concept count",
        ),
        "restore_policy" => (
            "SELECT pgokf.get_config() ->> 'embedding_dim'",
            "1024",
            0,
            "restored dumped policy",
        ),
        "restore_health" => (
            "SELECT pgokf.health() ->> 'ok'",
            "false",
            2,
            "restored health",
        ),
        _ => panic!("unknown fault"),
    }
}

#[test]
#[ignore = "requires explicit local optional server image and new evidence directory"]
fn server_observation_fault_is_refused() {
    let fault = std::env::var("PGOKF_SMOKE_FAULT").expect("fault name required");
    let (needle, replacement, occurrence, diagnostic) = server_fault(&fault);
    let mut docker = Fault {
        docker: Docker::default(),
        needle: needle.into(),
        replacement: replacement.into(),
        occurrence,
        seen: 0,
        main_id: None,
        restored: false,
    };
    let image = std::env::var("PGOKF_SMOKE_IMAGE").expect("image required");
    let evidence = std::env::var("PGOKF_SMOKE_EVIDENCE").expect("new evidence path required");
    let result = pgokf_verifier::image_smoke::smoke(
        &mut docker,
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."),
        Path::new(&evidence),
        &image,
        env!("CARGO_PKG_VERSION"),
        true,
    );
    let error = result.unwrap_err();
    assert!(error.contains(diagnostic), "{fault}: {error}");
}
// Creation carries the entrypoint, while start carries only the resulting ID;
// record that ID and alter its actual attached output.
struct CompanionFault {
    docker: Docker,
    needle: String,
    replacement: String,
    id: Option<String>,
}
impl DockerApi for CompanionFault {
    fn call(&mut self, args: &[String]) -> Result<String, String> {
        let result = self.docker.call(args);
        if args.first().is_some_and(|s| s == "create")
            && args.iter().any(|s| s == &self.needle)
            && (!self.needle.starts_with("pgokf-") || args.iter().any(|s| s == "--help"))
        {
            self.id = result.as_ref().ok().map(|s| s.trim().to_owned());
        }
        if args.first().is_some_and(|s| s == "start")
            && self.id.as_ref().is_some_and(|id| args.contains(id))
        {
            return Ok(self.replacement.clone());
        }
        result
    }
}

#[test]
#[ignore = "requires explicit local companions image and new evidence directory"]
fn companion_observation_fault_is_refused() {
    let fault = std::env::var("PGOKF_SMOKE_FAULT").expect("fault name required");
    let (needle, replacement, diagnostic) = match fault.as_str() {
        "embed" => ("pgokf-embed", "invalid", "help"),
        "ingest" => ("pgokf-ingest", "invalid", "help"),
        "web" => ("pgokf-web", "invalid", "help"),
        "mcp" => ("pgokf-mcp", "invalid", "help"),
        "interval" => ("--interval", "", "interval"),
        "uid" => ("-u", "0", "UID mismatch"),
        _ => panic!("unknown fault"),
    };
    let mut docker = CompanionFault {
        docker: Docker::default(),
        needle: needle.into(),
        replacement: replacement.into(),
        id: None,
    };
    let image = std::env::var("PGOKF_SMOKE_IMAGE").expect("image required");
    let evidence = std::env::var("PGOKF_SMOKE_EVIDENCE").expect("new evidence path required");
    let result = pgokf_verifier::image_smoke::companions(&mut docker, Path::new(&evidence), &image);
    let error = result.unwrap_err();
    assert!(error.contains(diagnostic), "{fault}: {error}");
}
