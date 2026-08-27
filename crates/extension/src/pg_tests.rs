//! In-database (`#[pg_test]`) integration tests for the `pgokf` SQL surface.
//!
//! Unlike the crate's unit tests (which exercise pure Rust logic without a
//! backend) and `tests/api_stability.rs` (which locks the public surface at the
//! *source* level), these tests run inside a real `PostgreSQL` instance that
//! `cargo pgrx test` starts and into which it installs the freshly built
//! extension. They register a fixture bundle written to disk at test time and
//! assert the end-to-end SQL behavior: search, listing, provenance, graph
//! traversal, authorization, and — the runtime counterpart of the source-level
//! guardrails in `tests/api_stability.rs` — that every catalog object in the
//! live database carries a `COMMENT`/`obj_description`. Without this suite
//! `cargo pgrx test` would install nothing and run zero SQL, so a broken
//! generated SQL block or a reordered projection would still pass CI green.
//!
//! Every `#[pg_test]` runs in its own transaction that the harness rolls back,
//! so the fixtures each test builds — bundles, roles, temp functions — never
//! leak into another test. Assertions deliberately target stable, happy-path
//! behavior and never the edge cases other work streams are actively changing.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// `alpha` concept: carries a distinctive search term, provenance
    /// frontmatter (so it projects a `concept_provenance` row), and a resolved
    /// internal link to `beta` (so it projects a graph edge).
    const ALPHA_CONCEPT: &str = "---\n\
type: Reference\n\
title: Alpha Widget Concept\n\
tags: [widgets, indexing]\n\
generated_by: pipeline/test\n\
status: stable\n\
---\n\
\n\
# Alpha\n\
\n\
The alpha concept documents the peregrine indexing strategy for widgets.\n\
See [the beta concept](/beta.md) for the companion definition.\n";

    /// `beta` concept: the resolved destination of alpha's internal link.
    const BETA_CONCEPT: &str = "---\n\
type: Reference\n\
title: Beta Widget Concept\n\
tags: [widgets]\n\
---\n\
\n\
# Beta\n\
\n\
The beta concept is the companion definition referenced by alpha.\n";

    /// A throwaway on-disk OKF bundle written at test time and removed on drop.
    ///
    /// The bundle root lives under the system temp directory with a
    /// process/clock-unique name so concurrently running test backends never
    /// collide, mirroring the fixture pattern the crate's unit tests use.
    struct FixtureBundle {
        root: PathBuf,
    }

    impl FixtureBundle {
        /// Materialize the two-concept fixture bundle on disk.
        fn create() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("pgokf-pg-test-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&root).expect("fixture bundle root is creatable");
            fs::write(root.join("alpha.md"), ALPHA_CONCEPT).expect("alpha fixture is writable");
            fs::write(root.join("beta.md"), BETA_CONCEPT).expect("beta fixture is writable");
            Self { root }
        }

        /// The bundle root as a UTF-8 path string for `register_bundle`.
        fn path(&self) -> String {
            self.root
                .to_str()
                .expect("fixture bundle path is valid UTF-8")
                .to_owned()
        }
    }

    impl Drop for FixtureBundle {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Register the fixture bundle and return its assigned identity.
    ///
    /// Asserts the sync classified both concept files as added, confirming the
    /// register path parsed and staged the fixture end to end.
    fn register_fixture(bundle: &FixtureBundle) -> i64 {
        Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT bundle_id, added FROM pgokf.register_bundle($1) AS r",
                    Some(1),
                    &[bundle.path().into()],
                )
                .expect("register_bundle executes")
                .first();
            let bundle_id = row
                .get::<i64>(1)
                .expect("bundle_id column is readable")
                .expect("bundle_id is not NULL");
            let added = row
                .get::<i32>(2)
                .expect("added column is readable")
                .expect("added is not NULL");
            assert_eq!(added, 2, "the fixture registers exactly two concepts");
            bundle_id
        })
    }

    #[pg_test]
    fn version_returns_the_crate_package_version() {
        // Arrange / Act: a bare install must answer the version function.
        let version = Spi::get_one::<String>("SELECT pgokf.version()")
            .expect("version query executes")
            .expect("version is not NULL");

        // Assert: it reports the compiled crate version.
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }

    #[pg_test]
    fn register_then_search_list_and_provenance_round_trip() {
        // Arrange: a fresh two-concept bundle registered into the catalog.
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);

        // Act / Assert: full-text search finds the distinctive term in alpha.
        let hit = Spi::get_one_with_args::<String>(
            "SELECT concept_id FROM pgokf.concept_search('peregrine') LIMIT 1",
            &[],
        )
        .expect("concept_search executes")
        .expect("the search term matches the alpha concept");
        assert_eq!(hit, "alpha");

        // Assert: list_bundles surfaces the freshly registered bundle.
        let listed = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.list_bundles() WHERE id = $1",
            &[bundle_id.into()],
        )
        .expect("list_bundles executes")
        .expect("count is not NULL");
        assert_eq!(listed, 1, "the registered bundle appears exactly once");

        // Assert: bundle_info reports both concept files for the bundle.
        let file_count = Spi::get_one_with_args::<i32>(
            "SELECT file_count FROM pgokf.bundle_info($1)",
            &[bundle_id.into()],
        )
        .expect("bundle_info executes")
        .expect("file_count is not NULL");
        assert_eq!(file_count, 2, "bundle_info counts both concepts");

        // Assert: provenance frontmatter projected a row for the alpha concept.
        let generated_by = Spi::get_one_with_args::<String>(
            "SELECT generated_by FROM pgokf.concept_provenance
             WHERE bundle_id = $1 AND concept_id = 'alpha'",
            &[bundle_id.into()],
        )
        .expect("concept_provenance query executes")
        .expect("alpha carries a provenance row");
        assert_eq!(generated_by, "pipeline/test");
    }

    #[pg_test]
    fn concept_neighbors_walks_a_resolved_internal_edge() {
        // Arrange: register the bundle whose alpha links to beta.
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);

        // Act: traverse outward from alpha within the bundle.
        let neighbor = Spi::get_one_with_args::<String>(
            "SELECT neighbor_id FROM pgokf.concept_neighbors('alpha', 2, $1)
             ORDER BY hops, neighbor_id
             LIMIT 1",
            &[bundle_id.into()],
        )
        .expect("concept_neighbors executes")
        .expect("alpha reaches at least one neighbor");

        // Assert: beta is reachable across the resolved internal link.
        assert_eq!(neighbor, "beta");

        let hops = Spi::get_one_with_args::<i32>(
            "SELECT hops FROM pgokf.concept_neighbors('alpha', 2, $1)
             WHERE neighbor_id = 'beta'",
            &[bundle_id.into()],
        )
        .expect("concept_neighbors hop query executes")
        .expect("beta has a hop count");
        assert_eq!(hops, 1, "beta is one resolved edge away from alpha");
    }

    #[pg_test]
    fn set_config_denies_a_non_member_role_with_insufficient_privilege() {
        // Arrange: a role that belongs to neither pgokf_reader nor pgokf_admin,
        // plus a probe that runs set_config as that role and reports the
        // SQLSTATE of the denial. A function-local `SET role` clause scopes the
        // switch to the probe body, so the surrounding session stays superuser.
        Spi::run("CREATE ROLE pgokf_test_outsider").expect("outsider role is creatable");
        Spi::run(
            "CREATE FUNCTION pg_temp.pgokf_denied_sqlstate() RETURNS text
             LANGUAGE plpgsql
             SET role TO pgokf_test_outsider
             AS $probe$
             BEGIN
                 PERFORM pgokf.set_config('default_strict', 'false'::jsonb);
                 RETURN 'not-denied';
             EXCEPTION WHEN insufficient_privilege THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("authz probe function is creatable");

        // Act: invoke set_config as the non-member role.
        let sqlstate = Spi::get_one::<String>("SELECT pg_temp.pgokf_denied_sqlstate()")
            .expect("authz probe executes")
            .expect("the probe reports a SQLSTATE");

        // Assert: the non-member is denied with insufficient_privilege (42501).
        assert_eq!(
            sqlstate, "42501",
            "a non-member role must be denied set_config with SQLSTATE 42501",
        );
    }

    /// A throwaway, writable server-side directory for reconstruction tests,
    /// created with a process/clock-unique name and removed on drop.
    struct ExportDir {
        root: PathBuf,
    }

    impl ExportDir {
        fn create() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos();
            let root = std::env::temp_dir()
                .join(format!("pgokf-src-export-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&root).expect("export dir is creatable");
            Self { root }
        }

        fn path(&self) -> String {
            self.root
                .to_str()
                .expect("export dir path is valid UTF-8")
                .to_owned()
        }
    }

    impl Drop for ExportDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// Enable the `store_source` tier for the current (rolled-back) test
    /// transaction, so a subsequent register persists verbatim source bytes.
    fn enable_store_source() {
        Spi::run("SELECT pgokf.set_config('store_source', 'true'::jsonb)")
            .expect("store_source can be enabled");
    }

    #[pg_test]
    fn store_source_on_retrieves_and_reconstructs_exact_bytes() {
        // Arrange: enable the store_source tier, then register a fixture whose
        // exact on-disk bytes are known (the ALPHA_CONCEPT / BETA_CONCEPT
        // constants).
        enable_store_source();
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);

        // Act / Assert: get_concept_source returns the alpha file byte-for-byte.
        let stored = Spi::get_one_with_args::<Vec<u8>>(
            "SELECT pgokf.get_concept_source($1, 'alpha')",
            &[bundle_id.into()],
        )
        .expect("get_concept_source executes")
        .expect("alpha carries stored source bytes");
        assert_eq!(
            stored,
            ALPHA_CONCEPT.as_bytes(),
            "stored source must equal the original alpha file bytes",
        );

        // Act: reconstruct the whole bundle on disk into a fresh directory.
        let dest = ExportDir::create();
        let files_written = Spi::get_one_with_args::<i64>(
            "SELECT concepts_rows FROM pgokf.export_sources($1, $2)",
            &[bundle_id.into(), dest.path().into()],
        )
        .expect("export_sources executes")
        .expect("concepts_rows is not NULL");

        // Assert: both concept files were reconstructed, byte-for-byte, and
        // their BLAKE3 digests equal the originals' digests.
        assert_eq!(files_written, 2, "both stored sources are reconstructed");
        let alpha_bytes = fs::read(bundle.root.join("alpha.md")).expect("original alpha readable");
        let rebuilt_alpha =
            fs::read(dest.root.join("alpha.md")).expect("reconstructed alpha readable");
        assert_eq!(
            rebuilt_alpha, alpha_bytes,
            "reconstructed alpha must be byte-for-byte identical",
        );
        assert_eq!(
            okf_sync::hash_bytes(&rebuilt_alpha),
            okf_sync::hash_bytes(&alpha_bytes),
            "reconstructed alpha must hash identically to the original",
        );
        let rebuilt_beta =
            fs::read(dest.root.join("beta.md")).expect("reconstructed beta readable");
        assert_eq!(
            rebuilt_beta,
            BETA_CONCEPT.as_bytes(),
            "reconstructed beta must be byte-for-byte identical",
        );
    }

    #[pg_test]
    fn store_source_off_stores_no_source_rows() {
        // Arrange: the default policy (store_source disabled) — register without
        // enabling it.
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);

        // Act: count the stored sources for the bundle.
        let rows = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concept_source WHERE bundle_id = $1",
            &[bundle_id.into()],
        )
        .expect("concept_source count executes")
        .expect("count is not NULL");

        // Assert: default behavior is unchanged — no source bytes are stored.
        assert_eq!(rows, 0, "the default policy stores no concept_source rows");
    }

    #[pg_test]
    fn reader_can_retrieve_source_but_is_denied_export() {
        // Arrange: a role granted pgokf_reader, and a store_source-backed
        // bundle it can read.
        enable_store_source();
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);
        Spi::run("CREATE ROLE pgokf_src_reader").expect("reader role is creatable");
        Spi::run("GRANT pgokf_reader TO pgokf_src_reader").expect("reader role is grantable");

        // A function-local `SET role` scopes each probe to the reader identity
        // while the surrounding session stays privileged.
        Spi::run(
            "CREATE FUNCTION pg_temp.reader_get_source(bid bigint) RETURNS text
             LANGUAGE plpgsql
             SET role TO pgokf_src_reader
             AS $probe$
             BEGIN
                 PERFORM pgokf.get_concept_source(bid, 'alpha');
                 RETURN 'ok';
             EXCEPTION WHEN insufficient_privilege THEN
                 RETURN 'denied';
             END
             $probe$;",
        )
        .expect("reader get probe is creatable");
        Spi::run(
            "CREATE FUNCTION pg_temp.reader_export_sources(bid bigint, dst text) RETURNS text
             LANGUAGE plpgsql
             SET role TO pgokf_src_reader
             AS $probe$
             BEGIN
                 PERFORM pgokf.export_sources(bid, dst);
                 RETURN 'not-denied';
             EXCEPTION WHEN insufficient_privilege THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("reader export probe is creatable");

        // Act: the reader retrieves a concept source, then attempts an export.
        let get_outcome = Spi::get_one_with_args::<String>(
            "SELECT pg_temp.reader_get_source($1)",
            &[bundle_id.into()],
        )
        .expect("reader get probe executes")
        .expect("probe reports an outcome");
        let dest = ExportDir::create();
        let export_sqlstate = Spi::get_one_with_args::<String>(
            "SELECT pg_temp.reader_export_sources($1, $2)",
            &[bundle_id.into(), dest.path().into()],
        )
        .expect("reader export probe executes")
        .expect("probe reports a SQLSTATE");

        // Assert: retrieval is a reader-level disclosure (allowed), while
        // reconstruction is admin-only (denied with 42501).
        assert_eq!(get_outcome, "ok", "a reader may retrieve a concept source");
        assert_eq!(
            export_sqlstate, "42501",
            "a reader must be denied export_sources with SQLSTATE 42501",
        );
    }

    #[pg_test]
    fn unregister_bundle_cascades_concept_source_to_zero_rows() {
        // Arrange: a store_source-backed bundle with stored source rows.
        enable_store_source();
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);
        let before = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concept_source WHERE bundle_id = $1",
            &[bundle_id.into()],
        )
        .expect("pre-count executes")
        .expect("count is not NULL");
        assert_eq!(before, 2, "the store_source bundle has two stored sources");

        // Act: unregister the bundle.
        Spi::run_with_args("SELECT pgokf.unregister_bundle($1)", &[bundle_id.into()])
            .expect("unregister_bundle executes");

        // Assert: the foreign key cascade removed every concept_source row.
        let after = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concept_source WHERE bundle_id = $1",
            &[bundle_id.into()],
        )
        .expect("post-count executes")
        .expect("count is not NULL");
        assert_eq!(after, 0, "unregister cascades concept_source to zero rows");
    }

    #[pg_test]
    fn get_concept_source_raises_22023_for_absent_source_and_concept() {
        // Arrange: the default policy (store_source disabled), so the concepts
        // exist but no source was stored.
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);
        Spi::run(
            "CREATE FUNCTION pg_temp.get_source_sqlstate(bid bigint, cid text) RETURNS text
             LANGUAGE plpgsql
             AS $probe$
             BEGIN
                 PERFORM pgokf.get_concept_source(bid, cid);
                 RETURN 'no-error';
             EXCEPTION WHEN OTHERS THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("get_concept_source probe is creatable");

        // Act: a concept that exists but has no stored source, and a concept
        // that does not exist at all.
        let absent_source = Spi::get_one_with_args::<String>(
            "SELECT pg_temp.get_source_sqlstate($1, 'alpha')",
            &[bundle_id.into()],
        )
        .expect("absent-source probe executes")
        .expect("probe reports a SQLSTATE");
        let absent_concept = Spi::get_one_with_args::<String>(
            "SELECT pg_temp.get_source_sqlstate($1, 'ghost')",
            &[bundle_id.into()],
        )
        .expect("absent-concept probe executes")
        .expect("probe reports a SQLSTATE");

        // Assert: both are invalid-parameter (22023), distinguishing "no source
        // stored" from "no such concept" only in the message, never the class.
        assert_eq!(
            absent_source, "22023",
            "a stored-source miss must raise invalid_parameter (22023)",
        );
        assert_eq!(
            absent_concept, "22023",
            "an unknown concept must raise invalid_parameter (22023)",
        );
    }

    #[pg_test]
    fn every_catalog_object_carries_a_comment() {
        // This is the runtime, database-truth counterpart of the source-level
        // COMMENT guardrails in tests/api_stability.rs: it reads obj_description
        // straight from the installed catalog, so it is blind to source drift
        // and catches any pgokf.* function, standalone composite type, or table
        // that ships without documentation.

        // Assert: every pgokf.* / pgokf_private.* function is documented.
        let undocumented_functions = Spi::get_one::<String>(
            "SELECT string_agg(n.nspname || '.' || p.proname, ', ' ORDER BY p.proname)
             FROM pg_catalog.pg_proc p
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname IN ('pgokf', 'pgokf_private')
               AND pg_catalog.obj_description(p.oid, 'pg_proc') IS NULL",
        )
        .expect("function coverage query executes");
        assert_eq!(
            undocumented_functions, None,
            "every pgokf function must carry a COMMENT; undocumented: {undocumented_functions:?}",
        );

        // Assert: every standalone composite type is documented (table rowtypes
        // are excluded by relkind = 'c' and checked as tables below).
        let undocumented_types = Spi::get_one::<String>(
            "SELECT string_agg(n.nspname || '.' || t.typname, ', ' ORDER BY t.typname)
             FROM pg_catalog.pg_type t
             JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
             JOIN pg_catalog.pg_class c ON c.oid = t.typrelid
             WHERE n.nspname IN ('pgokf', 'pgokf_private')
               AND c.relkind = 'c'
               AND pg_catalog.obj_description(t.oid, 'pg_type') IS NULL",
        )
        .expect("type coverage query executes");
        assert_eq!(
            undocumented_types, None,
            "every pgokf composite type must carry a COMMENT; undocumented: {undocumented_types:?}",
        );

        // Assert: every catalog table is documented.
        let undocumented_tables = Spi::get_one::<String>(
            "SELECT string_agg(n.nspname || '.' || c.relname, ', ' ORDER BY c.relname)
             FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname IN ('pgokf', 'pgokf_private')
               AND c.relkind = 'r'
               AND pg_catalog.obj_description(c.oid, 'pg_class') IS NULL",
        )
        .expect("table coverage query executes");
        assert_eq!(
            undocumented_tables, None,
            "every pgokf table must carry a COMMENT; undocumented: {undocumented_tables:?}",
        );
    }
}
