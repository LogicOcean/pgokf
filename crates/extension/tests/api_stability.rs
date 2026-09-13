// SPDX-License-Identifier: AGPL-3.0-only
//! Stable-API guardrail tests for the `pgokf` extension.
//!
//! These tests lock the public SQL surface and its documentation coverage at
//! the source level, so a regression is caught by an ordinary
//! `cargo test -p pgokf` run without a live `PostgreSQL` backend. They are the
//! compile-time twin of the runtime `obj_description` coverage query in
//! `docs/release-checklist.md`, which the release process runs against an
//! installed extension:
//!
//! ```sql
//! SELECT n.nspname, p.proname
//! FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
//! WHERE n.nspname = 'pgokf' AND obj_description(p.oid, 'pg_proc') IS NULL;
//! ```
//!
//! The extension SQL is generated from `#[pg_extern]` items and
//! `extension_sql!` / `extension_sql_file!` blocks in `src/catalog/*.rs` and
//! `sql/bootstrap.sql`; every `COMMENT ON` statement for a public object lives
//! in one of those blocks. These tests read that source directly, so the
//! contract encoded below IS the stable public surface: adding a public object
//! without both listing it here and giving it a `COMMENT ON` fails the build.
//!
//! # Relationship to the in-database coverage test
//!
//! Because these checks are raw source-substring matches, they are blind to
//! signature drift and to the real installed catalog: a `COMMENT ON` whose
//! argument list no longer matches the generated function, or an object that
//! only exists once pgrx assembles the SQL, would slip past them. The
//! *authoritative* comment-coverage check therefore now also runs in-database,
//! as the `every_catalog_object_carries_a_comment` `#[pg_test]` in
//! `src/pg_tests.rs`: it queries `obj_description` for every `pgokf.*` /
//! `pgokf_private.*` function, standalone composite type, and table in the live
//! database and fails on any gap. These build-time contract tests remain the
//! fast, backend-free first line of defense; the in-DB test verifies coverage
//! against database truth.

use std::fs;
use std::path::{Path, PathBuf};

/// The 69 stable public functions, as `(name, argument-type list)`. The pair
/// renders to the exact `COMMENT ON FUNCTION pgokf.<name>(<args>)` prefix that
/// the hardening blocks emit.
const PUBLIC_FUNCTIONS: &[(&str, &str)] = &[
    ("register_bundle", "text, text, jsonb"),
    ("register_bundle_content", "text, text[], bytea[], jsonb"),
    (
        "register_bundle_content_with_context",
        "text, text[], bytea[], jsonb, jsonb",
    ),
    ("refresh_bundle", "bigint"),
    ("unregister_bundle", "bigint"),
    ("set_bundle_enabled", "bigint, boolean"),
    ("retire_bundle", "bigint"),
    ("unretire_bundle", "bigint"),
    ("purge_retired", "interval"),
    ("list_bundles", ""),
    ("bundle_info", "bigint"),
    ("duplicate_concepts", "bigint, integer"),
    (
        "concept_search",
        "text, bigint, integer, text, text[], text, text, jsonb",
    ),
    (
        "concept_search_fresh",
        "text, bigint, integer, text, text, text[], text, text, jsonb",
    ),
    (
        "search_facets",
        "text, bigint, text, text, text[], text, text",
    ),
    ("search_index_status", ""),
    ("schedule_refresh", "bigint, text"),
    ("unschedule_refresh", "bigint"),
    ("list_scheduled_refreshes", ""),
    ("find_similar", "text, bigint, integer"),
    ("concept_search_semantic", "real[], bigint, integer, text[]"),
    (
        "concept_search_hybrid",
        "text, real[], bigint, integer, text[]",
    ),
    ("set_concept_embedding", "bigint, text, real[]"),
    (
        "set_concept_embedding_cas",
        "bigint, text, real[], text, text, text, text",
    ),
    ("rebuild_embedding_index", ""),
    ("concept_neighbors", "text, integer, bigint"),
    ("concept_history", "bigint, text, integer"),
    ("concept_as_of", "bigint, text, timestamptz"),
    ("set_config", "text, jsonb"),
    ("reset_config", "text"),
    ("get_config", ""),
    ("list_sync_log", "bigint, integer"),
    ("list_sync_changes", "bigint, integer"),
    ("list_access_log", "bigint, integer"),
    ("list_bundle_log", "bigint, text, integer"),
    ("catalog_stats", ""),
    ("health", ""),
    ("stale_concepts", "bigint, timestamptz"),
    ("export_parquet", "bigint, text"),
    ("get_concept_source", "bigint, text"),
    ("export_sources", "bigint, text"),
    ("get_skill", "bigint, text"),
    ("get_script", "bigint, text"),
    ("get_reference", "bigint, text, boolean"),
    ("rebuild_search_index", ""),
    ("version", ""),
    ("tenant_required", ""),
    ("mcp_token_bearer", "text"),
    ("capabilities", ""),
    (
        "register_freshness_dependency",
        "text, bigint, text, bigint, text, text, text, text",
    ),
    ("disable_freshness_dependency", "bigint"),
    ("remove_freshness_dependency", "bigint"),
    ("mark_stale", "bigint, text[], text, text"),
    ("mark_reconciling", "bigint, text"),
    ("mark_blocked", "bigint, text[], text"),
    (
        "mark_fresh",
        "bigint, bigint, bigint, text, text, jsonb, text",
    ),
    ("mark_scope_stale", "bigint, text, text, text[], text"),
    ("clear_freshness_scope", "bigint, text, text"),
    ("list_freshness_dependencies", "integer"),
    ("repair_bundle_freshness", "bigint, text, text[]"),
    (
        "issue_publication_fence",
        "bigint, text, bigint, bigint, text, integer",
    ),
    ("release_publication_fence", "bigint, text, bigint"),
    ("claim_catalog_change_events", "text, integer, integer"),
    ("ack_catalog_change_event", "bigint, text, text"),
    ("list_catalog_change_events", "bigint, integer"),
    (
        "replace_relationships",
        "text, bigint, bigint, bigint, bigint, jsonb",
    ),
    (
        "concept_relationship_neighbors",
        "bigint, text, integer, text, text[], integer",
    ),
    ("registry_set_status", "uuid, text"),
    ("registry_set_poll_interval", "uuid, integer"),
];

/// The 24 stable public composite types.
const PUBLIC_TYPES: &[&str] = &[
    "bundle_sync_result",
    "concept_search_result",
    "concept_neighbor",
    "bundle_info",
    "export_result",
    "sync_log_entry",
    "catalog_stat",
    "stale_concept",
    "sync_change",
    "access_log_entry",
    "duplicate_group",
    "search_facet",
    "bundle_log_entry",
    "concept_version",
    "skill_result",
    "script_result",
    "reference_result",
    "freshness_dependency_info",
    "publication_fence_info",
    "claimed_change_event",
    "catalog_change_event_info",
    "concept_search_fresh_result",
    "relationship_publication_info",
    "relationship_neighbor",
];

/// The 29 catalog tables, as fully-qualified `schema.table` identifiers.
/// Nineteen are public (`pgokf`); the singleton policy row and the three
/// admin-only history/audit logs live in the `pgokf_private` schema; the
/// web UI's people, sessions, MCP tokens, and identity-provider settings
/// live in `pgokf_web` (writer-only). All are documented all the same.
const CATALOG_TABLES: &[&str] = &[
    "pgokf.bundles",
    "pgokf.concepts",
    "pgokf.concept_metadata",
    "pgokf.links",
    "pgokf.concept_provenance",
    "pgokf.concept_verification",
    "pgokf.concept_provenance_source",
    "pgokf.concept_source",
    "pgokf.concept_embedding",
    "pgokf.bundle_log",
    "pgokf.concept_history",
    "pgokf.skills",
    "pgokf.scripts",
    "pgokf.reference_documents",
    "pgokf.bundle_freshness",
    "pgokf.concept_freshness",
    "pgokf.freshness_dependency",
    "pgokf.publication_fence",
    "pgokf.catalog_change_event",
    "pgokf.relationship_publication",
    "pgokf.relationship",
    "pgokf_private.config",
    "pgokf_private.sync_log",
    "pgokf_private.sync_log_change",
    "pgokf_private.access_log",
    "pgokf_web.users",
    "pgokf_web.sessions",
    "pgokf_web.mcp_tokens",
    "pgokf_web.identity_providers",
];

/// The four public API roles created by `sql/bootstrap.sql`
/// (`pgokf_reader` < `pgokf_writer` < `pgokf_admin`, plus the out-of-ladder
/// `pgokf_dispatcher` outbox delivery role).
const API_ROLES: &[&str] = &[
    "pgokf_reader",
    "pgokf_writer",
    "pgokf_admin",
    "pgokf_dispatcher",
];

/// The number of `#[pg_extern]` functions defined under `src/catalog/`. Seven
/// public functions are not `#[pg_extern]`s there: `pgokf.version()` is
/// declared in `src/lib.rs`, `pgokf.tenant_required()` is plain SQL in
/// `sql/bootstrap.sql` (every row-level-security policy references it, so it
/// must exist before any table), `pgokf.mcp_token_bearer(text)` is plain
/// SQL beside the table it reads (`src/catalog/web_identity.rs`),
/// `pgokf.capabilities()` is plain SQL in the `effective_freshness_view` block
/// of `src/catalog/freshness.rs` (an immutable constant, so no Rust wrapper is
/// needed), `pgokf.list_scheduled_refreshes()` is plain SQL in the
/// `scheduled_refreshes_reader` block of `src/catalog/schedule.rs` (a
/// SECURITY DEFINER read over the runtime-only `cron.job`, so no Rust
/// wrapper is needed either), and `pgokf.registry_set_status(uuid, text)` /
/// `pgokf.registry_set_poll_interval(uuid, integer)` are plain SQL in the
/// `registry_surface` block of `src/catalog/registry.rs` (SECURITY DEFINER
/// writes over the runtime-only `ast_graph.repository_registry`, so no Rust
/// wrapper is needed for them either), so the catalog count is seven less
/// than [`PUBLIC_FUNCTIONS`].
const CATALOG_PG_EXTERN_COUNT: usize = PUBLIC_FUNCTIONS.len() - 7;

/// SQL keywords that must never appear in an executable upgrade statement,
/// because they would break the no-data-loss guarantee.
fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Read every `.rs` file under `src/catalog/` plus `sql/bootstrap.sql` and
/// concatenate them. Every `COMMENT ON` for a public object is emitted from
/// one of these sources, so the join is the searchable SQL surface.
fn sql_surface() -> String {
    let mut surface = String::new();

    let catalog = crate_dir().join("src").join("catalog");
    let mut rs_files: Vec<PathBuf> = fs::read_dir(&catalog)
        .expect("src/catalog must be readable")
        .map(|entry| entry.expect("directory entry must be readable").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    rs_files.sort();

    for path in rs_files {
        surface.push_str(&read_to_string(&path));
        surface.push('\n');
    }

    surface.push_str(&read_to_string(
        &crate_dir().join("sql").join("bootstrap.sql"),
    ));
    surface
}

fn read_to_string(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

#[test]
fn every_public_function_carries_a_comment() {
    // Arrange
    let surface = sql_surface();

    // Act / Assert
    for (name, args) in PUBLIC_FUNCTIONS {
        let needle = format!("COMMENT ON FUNCTION pgokf.{name}({args})");
        assert!(
            surface.contains(&needle),
            "public function pgokf.{name}({args}) is missing a COMMENT ON FUNCTION statement; \
             expected to find `{needle}` in the catalog SQL source",
        );
    }
}

#[test]
fn every_public_type_carries_a_comment() {
    // Arrange
    let surface = sql_surface();

    // Act / Assert
    for name in PUBLIC_TYPES {
        let needle = format!("COMMENT ON TYPE pgokf.{name}");
        assert!(
            surface.contains(&needle),
            "public type pgokf.{name} is missing a COMMENT ON TYPE statement",
        );
    }
}

#[test]
fn every_catalog_table_carries_a_comment() {
    // Arrange
    let surface = sql_surface();

    // Act / Assert
    for table in CATALOG_TABLES {
        let needle = format!("COMMENT ON TABLE {table}");
        assert!(
            surface.contains(&needle),
            "catalog table {table} is missing a COMMENT ON TABLE statement",
        );
    }
}

#[test]
fn all_api_roles_carry_a_comment() {
    // Arrange
    let surface = sql_surface();

    // Act / Assert
    for role in API_ROLES {
        let needle = format!("COMMENT ON ROLE {role}");
        assert!(
            surface.contains(&needle),
            "API role {role} is missing a COMMENT ON ROLE statement in bootstrap.sql",
        );
    }
}

#[test]
fn public_function_surface_count_is_locked() {
    // Arrange
    let catalog = crate_dir().join("src").join("catalog");
    let mut count = 0usize;
    for entry in fs::read_dir(&catalog).expect("src/catalog must be readable") {
        let path = entry.expect("directory entry must be readable").path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            count += read_to_string(&path).matches("#[pg_extern").count();
        }
    }

    // Act / Assert
    assert_eq!(
        count, CATALOG_PG_EXTERN_COUNT,
        "the number of #[pg_extern] functions under src/catalog changed \
         (found {count}, expected {CATALOG_PG_EXTERN_COUNT}); the public API surface is stable - \
         update PUBLIC_FUNCTIONS and add a COMMENT ON FUNCTION before changing it",
    );
}

#[test]
fn every_upgrade_script_is_forward_compatible() {
    // Arrange: every shipped upgrade script, not just the first. An
    // upgrade runs against a live catalog, so none of them may lose data.
    let sql = crate_dir().join("sql");
    let mut scripts: Vec<PathBuf> = std::fs::read_dir(&sql)
        .expect("sql directory is readable")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.starts_with("pgokf--") && name.contains("--"))
        })
        .filter(|path| {
            // An upgrade script names two versions; the base install script
            // names one.
            path.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.matches("--").count() == 2)
        })
        .collect();
    scripts.sort();
    assert!(
        scripts.len() >= 2,
        "expected several upgrade scripts, found {}",
        scripts.len()
    );

    // Act / Assert
    for script in &scripts {
        let name = script.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if RESHAPED_DELIBERATELY.contains(&name) {
            continue;
        }
        // Strip line comments so prose that mentions destructive keywords
        // (the "never DROP/TRUNCATE/DELETE" guidance, say) is not mistaken
        // for an executable statement.
        let executable: String = read_to_string(script)
            .lines()
            .map(|line| line.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n")
            .to_uppercase();
        for statement in executable.split(';') {
            let statement = statement.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(
                !destroys_data(&statement),
                "upgrade script {name} has a destructive statement; upgrades must lose no \
                 data:\n  {statement}"
            );
        }
    }
}

/// The one shipped script that drops columns, and did so on purpose: 0.1.3
/// re-modelled `concept_provenance` from the boolean `verified` shape to
/// the event shape the catalog has now. It is history and cannot change;
/// every script since is held to the rule.
const RESHAPED_DELIBERATELY: &[&str] = &["pgokf--0.1.2--0.1.3.sql"];

/// Whether one normalized, upper-cased statement would lose data.
///
/// `ON DELETE CASCADE` is a foreign-key clause, and dropping a `CHECK`
/// constraint to re-add a wider one keeps every row - so the rule is about
/// what is dropped, not about the word.
fn destroys_data(statement: &str) -> bool {
    statement.contains("TRUNCATE ")
        || statement.starts_with("DELETE FROM")
        || statement.contains("DROP TABLE")
        || statement.contains("DROP COLUMN")
        || statement.contains("DROP SCHEMA")
        || statement.contains("DROP DATABASE")
}
