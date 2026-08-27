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
