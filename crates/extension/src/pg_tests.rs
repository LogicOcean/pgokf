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
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A monotonic-ish, process/clock-unique nonce for temp fixture names, so
    /// concurrently running test backends never collide on a path.
    fn unique_nonce() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_nanos()
    }

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
        /// Materialize the two-concept fixture bundle under the system temp dir.
        fn create() -> Self {
            Self::create_in(&std::env::temp_dir())
        }

        /// Materialize the two-concept fixture bundle in a subdirectory of
        /// `parent`, so a test can place a bundle inside (or outside) a
        /// configured `allowed_roots` boundary.
        fn create_in(parent: &Path) -> Self {
            let root = parent.join(format!(
                "pgokf-pg-test-{}-{}",
                std::process::id(),
                unique_nonce()
            ));
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

    /// A throwaway bundle seeded from the repo's `rich-metadata` fixture (the
    /// full OKF v0.2 shape) plus a bundle-root `index.md` declaring an
    /// `okf_version`, so the provenance re-model can be exercised end to end.
    struct RichFixture {
        root: PathBuf,
    }

    impl RichFixture {
        /// Copy every `.md` file from `tests/bundles/rich-metadata` into a fresh
        /// temp bundle and add a root `index.md` carrying `okf_version: 0.2`.
        fn create() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos();
            let root =
                std::env::temp_dir().join(format!("pgokf-rich-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&root).expect("rich fixture root is creatable");

            let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/bundles/rich-metadata")
                .canonicalize()
                .expect("rich-metadata fixture directory exists");
            for entry in fs::read_dir(&source).expect("rich-metadata dir is readable") {
                let path = entry.expect("dir entry is readable").path();
                if path.extension().is_some_and(|ext| ext == "md") {
                    let name = path.file_name().expect("fixture file has a name");
                    fs::copy(&path, root.join(name)).expect("fixture file copies");
                }
            }
            // The bundle-root index.md is reserved (never a concept); its only
            // recognized frontmatter is the OKF format version.
            fs::write(
                root.join("index.md"),
                "---\nokf_version: \"0.2\"\n---\n\n# Rich bundle\n",
            )
            .expect("index.md is writable");
            Self { root }
        }

        fn path(&self) -> String {
            self.root
                .to_str()
                .expect("rich fixture path is valid UTF-8")
                .to_owned()
        }
    }

    impl Drop for RichFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn register_rich(fixture: &RichFixture) -> i64 {
        Spi::get_one_with_args::<i64>(
            "SELECT bundle_id FROM pgokf.register_bundle($1) AS r",
            &[fixture.path().into()],
        )
        .expect("register_bundle executes")
        .expect("bundle_id is not NULL")
    }

    #[pg_test]
    fn rich_metadata_projects_okf_v0_2_provenance() {
        // Arrange: register the full OKF v0.2 fixture bundle.
        let fixture = RichFixture::create();
        let bundle_id = register_rich(&fixture);

        // Assert: the scalar provenance row maps every OKF v0.2 field.
        Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT generated_by,
                            generated_at IS NOT NULL AS has_generated_at,
                            status,
                            stale_after IS NOT NULL AS has_stale_after,
                            usage_window_from IS NOT NULL AS has_uw_from,
                            usage_window_to IS NOT NULL AS has_uw_to,
                            trust_tier
                     FROM pgokf.concept_provenance
                     WHERE bundle_id = $1 AND concept_id = 'rich-concept'",
                    Some(1),
                    &[bundle_id.into()],
                )
                .expect("concept_provenance query executes")
                .first();
            assert_eq!(
                row.get::<String>(1).expect("generated_by readable"),
                Some("catalog-agent/1.0".to_owned()),
                "generated_by maps OKF generated.by",
            );
            assert_eq!(
                row.get::<bool>(2).expect("has_generated_at readable"),
                Some(true),
                "generated_at is populated from OKF generated.at",
            );
            assert_eq!(
                row.get::<String>(3).expect("status readable"),
                Some("stable".to_owned()),
                "status maps OKF lifecycle status",
            );
            assert_eq!(
                row.get::<bool>(4).expect("has_stale_after readable"),
                Some(true),
                "stale_after is populated",
            );
            assert_eq!(
                row.get::<bool>(5).expect("has_uw_from readable"),
                Some(true),
                "usage_window_from is populated",
            );
            assert_eq!(
                row.get::<bool>(6).expect("has_uw_to readable"),
                Some(true),
                "usage_window_to is populated",
            );
            assert_eq!(
                row.get::<String>(7).expect("trust_tier readable"),
                Some("human-reviewed".to_owned()),
                "a human: verifier derives the human-reviewed tier",
            );
        });
    }

    #[pg_test]
    fn rich_metadata_projects_verification_events() {
        // Arrange
        let fixture = RichFixture::create();
        let bundle_id = register_rich(&fixture);

        // Act / Assert: the two verified[] events project in order with their
        // actors and timestamps.
        let event_count = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concept_verification
             WHERE bundle_id = $1 AND concept_id = 'rich-concept'",
            &[bundle_id.into()],
        )
        .expect("verification count executes")
        .expect("count is not NULL");
        assert_eq!(event_count, 2, "both verified[] events project a row");

        // The 0-ordinal event is the non-human process verifier; the 1-ordinal
        // event is the human reviewer, both with a parsed timestamp.
        let first_by = Spi::get_one_with_args::<String>(
            "SELECT verified_by FROM pgokf.concept_verification
             WHERE bundle_id = $1 AND concept_id = 'rich-concept' AND ordinal = 0",
            &[bundle_id.into()],
        )
        .expect("ordinal-0 query executes")
        .expect("ordinal 0 exists");
        assert_eq!(first_by, "process:metric-validation");

        let second_by = Spi::get_one_with_args::<String>(
            "SELECT verified_by FROM pgokf.concept_verification
             WHERE bundle_id = $1 AND concept_id = 'rich-concept' AND ordinal = 1",
            &[bundle_id.into()],
        )
        .expect("ordinal-1 query executes")
        .expect("ordinal 1 exists");
        assert_eq!(second_by, "human:fixture-reviewer");

        let both_have_at = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concept_verification
             WHERE bundle_id = $1 AND concept_id = 'rich-concept' AND verified_at IS NOT NULL",
            &[bundle_id.into()],
        )
        .expect("verified_at count executes")
        .expect("count is not NULL");
        assert_eq!(both_have_at, 2, "both events carry a parsed verified_at");
    }

    #[pg_test]
    fn rich_metadata_projects_provenance_sources() {
        // Arrange
        let fixture = RichFixture::create();
        let bundle_id = register_rich(&fixture);

        // Act / Assert: both sources[] materials project with resource, author,
        // and usage_count.
        let source_count = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concept_provenance_source
             WHERE bundle_id = $1 AND concept_id = 'rich-concept'",
            &[bundle_id.into()],
        )
        .expect("source count executes")
        .expect("count is not NULL");
        assert_eq!(source_count, 2, "both sources[] entries project a row");

        let policy_usage = Spi::get_one_with_args::<i64>(
            "SELECT usage_count FROM pgokf.concept_provenance_source
             WHERE bundle_id = $1 AND concept_id = 'rich-concept' AND source_id = 'account-policy'",
            &[bundle_id.into()],
        )
        .expect("policy usage query executes")
        .expect("usage_count is not NULL");
        assert_eq!(policy_usage, 4200, "the first source's usage_count maps");

        let events_author = Spi::get_one_with_args::<String>(
            "SELECT author FROM pgokf.concept_provenance_source
             WHERE bundle_id = $1 AND concept_id = 'rich-concept' AND source_id = 'events-table'",
            &[bundle_id.into()],
        )
        .expect("events author query executes")
        .expect("author is not NULL");
        assert_eq!(
            events_author, "process:warehouse-catalog",
            "the second source's author maps the actor",
        );

        let policy_resource = Spi::get_one_with_args::<String>(
            "SELECT resource FROM pgokf.concept_provenance_source
             WHERE bundle_id = $1 AND concept_id = 'rich-concept' AND source_id = 'account-policy'",
            &[bundle_id.into()],
        )
        .expect("policy resource query executes")
        .expect("resource is not NULL");
        assert_eq!(
            policy_resource, "https://docs.example.test/policies/active-account",
            "the first source's resource maps the URI",
        );
    }

    #[pg_test]
    fn rich_metadata_populates_bundle_okf_version() {
        // Arrange: the fixture's bundle-root index.md declares okf_version 0.2.
        let fixture = RichFixture::create();
        let bundle_id = register_rich(&fixture);

        // Act
        let okf_version = Spi::get_one_with_args::<String>(
            "SELECT okf_version FROM pgokf.bundles WHERE id = $1",
            &[bundle_id.into()],
        )
        .expect("okf_version query executes")
        .expect("okf_version is populated from the root index.md");

        // Assert
        assert_eq!(
            okf_version, "0.2",
            "the root index.md okf_version is stored"
        );
    }

    #[pg_test]
    fn bundle_without_index_leaves_okf_version_null() {
        // Arrange: the two-concept fixture has no bundle-root index.md.
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);

        // Act / Assert: an absent index.md leaves okf_version NULL, defensively.
        let okf_version = Spi::get_one_with_args::<String>(
            "SELECT okf_version FROM pgokf.bundles WHERE id = $1",
            &[bundle_id.into()],
        )
        .expect("okf_version query executes");
        assert_eq!(
            okf_version, None,
            "a bundle with no root index.md has a NULL okf_version",
        );
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
    fn export_parquet_writes_every_table_including_provenance_timestamps() {
        // Arrange: register the OKF v0.2 fixture. Its provenance carries
        // timestamptz columns (generated_at, stale_after, usage_window_*) that
        // export as epoch microseconds — a live path with no in-DB coverage
        // until now, where a missing epoch cast silently broke export_parquet.
        let fixture = RichFixture::create();
        let bundle_id = register_rich(&fixture);
        let dir = ExportDir::create();

        // Act: export the whole bundle to Parquet.
        let concepts_rows = Spi::get_one_with_args::<i64>(
            "SELECT concepts_rows FROM pgokf.export_parquet($1, $2)",
            &[bundle_id.into(), dir.path().into()],
        )
        .expect("export_parquet executes")
        .expect("concepts_rows is not NULL");

        // Assert: rows were written (the provenance read succeeded) and every
        // table's Parquet file exists on disk.
        assert!(concepts_rows > 0, "the fixture has concepts to export");
        let base = PathBuf::from(dir.path());
        for file in [
            "concepts.parquet",
            "concept_metadata.parquet",
            "links.parquet",
            "concept_provenance.parquet",
        ] {
            assert!(
                base.join(file).is_file(),
                "export_parquet must write {file}"
            );
        }
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

    // ---------------------------------------------------------------------
    // Incremental refresh: the diff + re-projection + guarded re-resolution.
    // ---------------------------------------------------------------------

    /// A third concept that is written once and never touched, so a refresh
    /// classifies it as `unchanged` (its content hash is stable). It links to
    /// `/gamma.md`, which does not exist at register time, so its edge starts
    /// unresolved; adding `gamma.md` on refresh must flip that edge to resolved
    /// *even though keep itself is unchanged* — the guarded re-resolution pass.
    const KEEP_CONCEPT: &str = "---\n\
type: Reference\n\
title: Keep Concept\n\
tags: [widgets]\n\
---\n\
\n\
# Keep\n\
\n\
The keep concept mentions the distinctive marmoset anchor term.\n\
It links to [the gamma concept](/gamma.md) that appears only on refresh.\n";

    /// The edited body for `alpha.md` on refresh: same frontmatter shape, a new
    /// distinctive body term (`quokka`) that did not exist at register time, and
    /// no link to the now-deleted `beta.md`.
    const ALPHA_EDITED: &str = "---\n\
type: Reference\n\
title: Alpha Widget Concept\n\
tags: [widgets, indexing]\n\
generated_by: pipeline/test\n\
status: stable\n\
---\n\
\n\
# Alpha\n\
\n\
The alpha concept now documents the quokka indexing strategy for widgets.\n";

    /// A brand-new concept added on refresh, classified as `added`.
    const GAMMA_CONCEPT: &str = "---\n\
type: Reference\n\
title: Gamma Concept\n\
tags: [widgets]\n\
---\n\
\n\
# Gamma\n\
\n\
The gamma concept is introduced during the refresh cycle.\n";

    #[pg_test]
    fn refresh_bundle_reflects_added_updated_and_removed_files() {
        // Arrange: register a three-file bundle (alpha, beta, keep). The default
        // FixtureBundle writes alpha+beta; add keep.md so at least one file
        // survives the refresh unchanged and exercises the `unchanged` bucket.
        let bundle = FixtureBundle::create();
        fs::write(bundle.root.join("keep.md"), KEEP_CONCEPT).expect("keep fixture is writable");
        let bundle_id = Spi::get_one_with_args::<i64>(
            "SELECT bundle_id FROM pgokf.register_bundle($1) AS r",
            &[bundle.path().into()],
        )
        .expect("register_bundle executes")
        .expect("bundle_id is not NULL");
        // Sanity: search finds the original alpha term but not the future one.
        let pre_alpha = Spi::get_one::<String>(
            "SELECT concept_id FROM pgokf.concept_search('peregrine') LIMIT 1",
        )
        .expect("pre-refresh search executes")
        .expect("the original alpha term matches before refresh");
        assert_eq!(pre_alpha, "alpha", "alpha's original body term is indexed");
        // Sanity: keep's edge to the not-yet-existing gamma starts unresolved.
        let pre_keep_edge = Spi::get_one_with_args::<bool>(
            "SELECT resolved FROM pgokf.links
             WHERE bundle_id = $1 AND source_id = 'keep' AND target_id = 'gamma'",
            &[bundle_id.into()],
        )
        .expect("pre-refresh keep-edge query executes")
        .expect("keep's edge to gamma exists");
        assert!(
            !pre_keep_edge,
            "keep's edge to the absent gamma is unresolved"
        );

        // Act: mutate the on-disk bundle — edit one file (alpha), add one file
        // (gamma), delete one file (beta) — then re-synchronize. keep.md is left
        // byte-identical so it must classify as unchanged.
        fs::write(bundle.root.join("alpha.md"), ALPHA_EDITED).expect("alpha edit is writable");
        fs::write(bundle.root.join("gamma.md"), GAMMA_CONCEPT).expect("gamma add is writable");
        fs::remove_file(bundle.root.join("beta.md")).expect("beta delete succeeds");

        let counts = Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT added, updated, removed, unchanged
                     FROM pgokf.refresh_bundle($1) AS r",
                    Some(1),
                    &[bundle_id.into()],
                )
                .expect("refresh_bundle executes")
                .first();
            (
                row.get::<i32>(1)
                    .expect("added readable")
                    .expect("added not NULL"),
                row.get::<i32>(2)
                    .expect("updated readable")
                    .expect("updated not NULL"),
                row.get::<i32>(3)
                    .expect("removed readable")
                    .expect("removed not NULL"),
                row.get::<i32>(4)
                    .expect("unchanged readable")
                    .expect("unchanged not NULL"),
            )
        });

        // Assert: the incremental diff counted each mutation exactly once.
        assert_eq!(counts.0, 1, "gamma is the single added file");
        assert_eq!(counts.1, 1, "alpha is the single updated file");
        assert_eq!(counts.2, 1, "beta is the single removed file");
        assert_eq!(counts.3, 1, "keep is the single unchanged file");

        // Assert: the re-projection re-indexed the edited body — the new term is
        // searchable and the stale term is gone.
        let post_alpha =
            Spi::get_one::<String>("SELECT concept_id FROM pgokf.concept_search('quokka') LIMIT 1")
                .expect("post-refresh search executes")
                .expect("the edited alpha term matches after refresh");
        assert_eq!(
            post_alpha, "alpha",
            "the refreshed alpha body is re-indexed"
        );
        let stale_hits =
            Spi::get_one::<i64>("SELECT count(*) FROM pgokf.concept_search('peregrine')")
                .expect("stale-term search executes")
                .expect("count is not NULL");
        assert_eq!(stale_hits, 0, "the removed alpha term no longer matches");

        // Assert: the added concept row exists and the removed one is gone.
        let gamma_rows = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concepts WHERE bundle_id = $1 AND id = 'gamma'",
            &[bundle_id.into()],
        )
        .expect("gamma row query executes")
        .expect("count is not NULL");
        assert_eq!(gamma_rows, 1, "the added gamma concept row is projected");
        let beta_rows = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concepts WHERE bundle_id = $1 AND id = 'beta'",
            &[bundle_id.into()],
        )
        .expect("beta row query executes")
        .expect("count is not NULL");
        assert_eq!(beta_rows, 0, "the removed beta concept row is deleted");

        // Assert: the guarded re-resolution ran — keep was classified unchanged
        // (so project() never re-touched its row), yet its edge to the
        // newly-added gamma flipped from unresolved to resolved by the bundle-
        // wide re-resolution pass over the finalized concept set.
        let post_keep_edge = Spi::get_one_with_args::<bool>(
            "SELECT resolved FROM pgokf.links
             WHERE bundle_id = $1 AND source_id = 'keep' AND target_id = 'gamma'",
            &[bundle_id.into()],
        )
        .expect("post-refresh keep-edge query executes")
        .expect("keep's edge to gamma still exists");
        assert!(
            post_keep_edge,
            "an unchanged concept's edge to a newly-added target must re-resolve",
        );
    }

    // ---------------------------------------------------------------------
    // allowed_roots sandbox boundary (end-to-end, not just unit-tested).
    // ---------------------------------------------------------------------

    #[pg_test]
    fn register_bundle_outside_allowed_roots_is_rejected_and_inside_succeeds() {
        // Arrange: an allowed-root directory, a bundle placed *inside* it, and a
        // second bundle placed *outside* it (a sibling under the temp dir).
        let allowed_root = ExportDir::create();
        let inside = FixtureBundle::create_in(&allowed_root.root);
        let outside = FixtureBundle::create();
        Spi::run_with_args(
            "SELECT pgokf.set_config('allowed_roots', jsonb_build_array($1))",
            &[allowed_root.path().into()],
        )
        .expect("allowed_roots is configurable");

        // A plpgsql probe registers a path and reports the denial SQLSTATE, so a
        // rejected register aborts only its own subtransaction.
        Spi::run(
            "CREATE FUNCTION pg_temp.register_sqlstate(p text) RETURNS text
             LANGUAGE plpgsql
             AS $probe$
             BEGIN
                 PERFORM pgokf.register_bundle(p);
                 RETURN 'ok';
             EXCEPTION WHEN OTHERS THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("register probe is creatable");

        // Act / Assert: a path outside allowed_roots is rejected with the
        // invalid-parameter class (22023) the sandbox raises.
        let outside_sqlstate = Spi::get_one_with_args::<String>(
            "SELECT pg_temp.register_sqlstate($1)",
            &[outside.path().into()],
        )
        .expect("outside probe executes")
        .expect("probe reports a SQLSTATE");
        assert_eq!(
            outside_sqlstate, "22023",
            "a bundle path outside allowed_roots must be rejected with 22023",
        );

        // Act / Assert: a path inside allowed_roots synchronizes normally.
        let inside_added = Spi::get_one_with_args::<i32>(
            "SELECT added FROM pgokf.register_bundle($1) AS r",
            &[inside.path().into()],
        )
        .expect("inside register executes")
        .expect("added is not NULL");
        assert_eq!(
            inside_added, 2,
            "a contained bundle registers its two concepts"
        );
    }

    // ---------------------------------------------------------------------
    // Resource ceilings: a real sync aborting on a configured limit.
    // ---------------------------------------------------------------------

    #[pg_test]
    fn register_bundle_exceeding_max_file_bytes_is_rejected() {
        // Arrange: the resource-ceiling GUCs (pgokf.max_file_bytes,
        // pgokf.max_bundle_files) are PGC_SIGHUP — they cannot be changed with
        // SET inside a session, so this exercises the *shipped default* ceiling
        // (4 MiB per file) by registering a bundle with one oversized file. A
        // bundle whose file exceeds the ceiling must abort discovery.
        let over_ceiling_bytes = (crate::guc::DEFAULT_MAX_FILE_BYTES as usize) + 1;
        let bundle = FixtureBundle::create();
        let mut oversized = String::with_capacity(over_ceiling_bytes + 64);
        oversized.push_str("---\ntype: Reference\ntitle: Oversized\n---\n\n");
        oversized.push_str(&"x".repeat(over_ceiling_bytes));
        fs::write(bundle.root.join("huge.md"), &oversized).expect("oversized fixture is writable");

        Spi::run(
            "CREATE FUNCTION pg_temp.register_ceiling_sqlstate(p text) RETURNS text
             LANGUAGE plpgsql
             AS $probe$
             BEGIN
                 PERFORM pgokf.register_bundle(p);
                 RETURN 'ok';
             EXCEPTION WHEN OTHERS THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("ceiling probe is creatable");

        // Act
        let sqlstate = Spi::get_one_with_args::<String>(
            "SELECT pg_temp.register_ceiling_sqlstate($1)",
            &[bundle.path().into()],
        )
        .expect("ceiling probe executes")
        .expect("probe reports a SQLSTATE");

        // Assert: the scan aborts with the invalid-parameter class (22023) the
        // sync engine maps a discovery failure to.
        assert_eq!(
            sqlstate, "22023",
            "a bundle file over max_file_bytes must abort the sync with 22023",
        );

        // Assert: the aborted register left no bundle row behind (the probe's
        // subtransaction rolled the speculative insert back).
        let leaked = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.bundles WHERE path LIKE $1",
            &[format!("%{}%", bundle.root.file_name().unwrap().to_str().unwrap()).into()],
        )
        .expect("leak query executes")
        .expect("count is not NULL");
        assert_eq!(leaked, 0, "an aborted register persists no bundle row");
    }

    // ---------------------------------------------------------------------
    // Upgrade smoke: default_version and the shipped upgrade script.
    // ---------------------------------------------------------------------

    #[pg_test]
    fn control_default_version_matches_crate_and_upgrade_script_is_present() {
        // A real ALTER EXTENSION UPDATE cannot be exercised in-process: the
        // pgrx test harness installs the latest schema directly rather than
        // stepping through prior versions. Instead assert the two release-hygiene
        // invariants that make an upgrade installable: the control file's
        // default_version equals the crate version, and the matching
        // prior->current upgrade script ships and is non-empty.
        let crate_version = env!("CARGO_PKG_VERSION");

        // Arrange: read the shipped control file next to the crate manifest.
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let control = fs::read_to_string(manifest_dir.join("pgokf.control"))
            .expect("control file is readable");

        // Assert: default_version tracks the crate version exactly.
        let declared = control
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "default_version")
                    .then(|| value.trim().trim_matches('\'').to_owned())
            })
            .expect("control file declares default_version");
        assert_eq!(
            declared, crate_version,
            "control default_version must match the crate version",
        );

        // Assert: the runtime version() function agrees with the control file,
        // tying the installed extension to the declared release version.
        let runtime_version = Spi::get_one::<String>("SELECT pgokf.version()")
            .expect("version query executes")
            .expect("version is not NULL");
        assert_eq!(
            runtime_version, declared,
            "the installed extension reports the declared default_version",
        );

        // Assert: the prior->current upgrade script exists and is non-empty, so
        // an existing install can be stepped up to this release.
        let upgrade_script = manifest_dir
            .join("sql")
            .join(format!("pgokf--0.1.4--{crate_version}.sql"));
        let metadata =
            fs::metadata(&upgrade_script).expect("the current upgrade script ships on disk");
        assert!(
            metadata.len() > 0,
            "the upgrade script {} must be non-empty",
            upgrade_script.display(),
        );
    }

    // ---- Mountless content ingestion (pgokf.register_bundle_content) --------

    /// `alpha` content concept: a distinctive search term (`quokka`),
    /// provenance frontmatter, and a resolved internal link to the nested
    /// `beta` concept.
    const CONTENT_ALPHA: &str = "---\n\
type: Reference\n\
title: Alpha Content Concept\n\
tags: [quokka, mountless]\n\
generated_by: pipeline/content\n\
status: stable\n\
---\n\
\n\
# Alpha\n\
\n\
The alpha content concept documents the quokka ingestion path.\n\
See [beta](/nested/beta.md) for the companion definition.\n";

    /// `nested/beta` content concept: the nested destination of alpha's link.
    const CONTENT_BETA: &str = "---\n\
type: Reference\n\
title: Beta Content Concept\n\
tags: [mountless]\n\
---\n\
\n\
# Beta\n\
\n\
The nested beta content concept, linked from alpha.\n";

    /// `gamma` content concept: added on the resync to exercise the added
    /// bucket.
    const CONTENT_GAMMA: &str = "---\n\
type: Reference\n\
title: Gamma Content Concept\n\
tags: [mountless]\n\
---\n\
\n\
# Gamma\n\
\n\
An added concept for the resync diff.\n";

    /// Register (or resync) a content bundle, returning
    /// `(bundle_id, added, updated, removed, unchanged)`.
    fn register_content(
        name: &str,
        paths: Vec<String>,
        contents: Vec<Vec<u8>>,
    ) -> (i64, i32, i32, i32, i32) {
        Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT bundle_id, added, updated, removed, unchanged
                     FROM pgokf.register_bundle_content($1, $2, $3) AS r",
                    Some(1),
                    &[name.into(), paths.into(), contents.into()],
                )
                .expect("register_bundle_content executes")
                .first();
            let read_i64 = |ord| {
                row.get::<i64>(ord)
                    .expect("bigint column is readable")
                    .expect("bigint column is not NULL")
            };
            let read_i32 = |ord| {
                row.get::<i32>(ord)
                    .expect("integer column is readable")
                    .expect("integer column is not NULL")
            };
            (
                read_i64(1),
                read_i32(2),
                read_i32(3),
                read_i32(4),
                read_i32(5),
            )
        })
    }

    #[pg_test]
    fn register_bundle_content_projects_concepts_links_and_provenance() {
        // Arrange: two in-memory concepts, one at a nested path, alpha carrying
        // provenance frontmatter and a link to the nested beta.
        let paths = vec!["alpha.md".to_owned(), "nested/beta.md".to_owned()];
        let contents = vec![
            CONTENT_ALPHA.as_bytes().to_vec(),
            CONTENT_BETA.as_bytes().to_vec(),
        ];

        // Act
        let (bundle_id, added, updated, removed, unchanged) =
            register_content("handbook", paths, contents);

        // Assert: both concepts registered as added.
        assert_eq!(
            (added, updated, removed, unchanged),
            (2, 0, 0, 0),
            "a first content sync classifies both concepts as added",
        );

        // Assert: the bundle is recorded as content-sourced under its
        // synthetic key.
        let source_type = Spi::get_one_with_args::<String>(
            "SELECT source_type FROM pgokf.bundles WHERE id = $1",
            &[bundle_id.into()],
        )
        .expect("source_type query executes")
        .expect("source_type is not NULL");
        assert_eq!(source_type, "content", "content bundles record source_type");
        let path = Spi::get_one_with_args::<String>(
            "SELECT path FROM pgokf.bundles WHERE id = $1",
            &[bundle_id.into()],
        )
        .expect("path query executes")
        .expect("path is not NULL");
        assert_eq!(path, "content:handbook", "the synthetic key is stored");

        // Assert: full-text search finds the distinctive term in alpha.
        let hit = Spi::get_one_with_args::<String>(
            "SELECT concept_id FROM pgokf.concept_search('quokka') LIMIT 1",
            &[],
        )
        .expect("concept_search executes")
        .expect("the search term matches the alpha concept");
        assert_eq!(hit, "alpha");

        // Assert: the nested concept projected at its nested path.
        let nested = Spi::get_one_with_args::<String>(
            "SELECT path FROM pgokf.concepts WHERE bundle_id = $1 AND id = 'nested/beta'",
            &[bundle_id.into()],
        )
        .expect("nested concept query executes")
        .expect("the nested concept exists");
        assert_eq!(nested, "nested/beta.md");

        // Assert: alpha's provenance frontmatter projected.
        let generated_by = Spi::get_one_with_args::<String>(
            "SELECT generated_by FROM pgokf.concept_provenance
             WHERE bundle_id = $1 AND concept_id = 'alpha'",
            &[bundle_id.into()],
        )
        .expect("concept_provenance query executes")
        .expect("alpha carries a provenance row");
        assert_eq!(generated_by, "pipeline/content");

        // Assert: alpha's internal link resolved to the nested beta.
        let neighbor = Spi::get_one_with_args::<String>(
            "SELECT neighbor_id FROM pgokf.concept_neighbors('alpha', 2, $1)
             WHERE neighbor_id = 'nested/beta'",
            &[bundle_id.into()],
        )
        .expect("concept_neighbors executes")
        .expect("alpha reaches the nested beta across a resolved link");
        assert_eq!(neighbor, "nested/beta");
    }

    #[pg_test]
    fn register_bundle_content_resync_diffs_changed_removed_and_added() {
        // Arrange: an initial two-concept content bundle.
        let (_bundle_id, added, ..) = register_content(
            "handbook",
            vec!["alpha.md".to_owned(), "nested/beta.md".to_owned()],
            vec![
                CONTENT_ALPHA.as_bytes().to_vec(),
                CONTENT_BETA.as_bytes().to_vec(),
            ],
        );
        assert_eq!(added, 2, "the first sync adds both concepts");

        // Act: resync with a changed alpha, the nested beta removed, and a new
        // gamma added.
        let changed_alpha = format!("{CONTENT_ALPHA}\nAn appended paragraph changes the hash.\n");
        let (_id2, added2, updated2, removed2, unchanged2) = register_content(
            "handbook",
            vec!["alpha.md".to_owned(), "gamma.md".to_owned()],
            vec![
                changed_alpha.into_bytes(),
                CONTENT_GAMMA.as_bytes().to_vec(),
            ],
        );

        // Assert: alpha updated, gamma added, nested beta removed, none
        // unchanged.
        assert_eq!(
            (added2, updated2, removed2, unchanged2),
            (1, 1, 1, 0),
            "the resync diffs added/updated/removed against the stored projection",
        );
    }

    #[pg_test]
    fn register_bundle_content_rejects_a_traversing_path() {
        // Arrange: a probe that reports the SQLSTATE of a traversing path.
        Spi::run(
            "CREATE FUNCTION pg_temp.content_path_sqlstate() RETURNS text
             LANGUAGE plpgsql
             AS $probe$
             BEGIN
                 PERFORM pgokf.register_bundle_content(
                     'evil',
                     ARRAY['../escape.md']::text[],
                     ARRAY['data'::bytea]::bytea[]);
                 RETURN 'no-error';
             EXCEPTION WHEN OTHERS THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("content path probe is creatable");

        // Act
        let sqlstate = Spi::get_one::<String>("SELECT pg_temp.content_path_sqlstate()")
            .expect("content path probe executes")
            .expect("the probe reports a SQLSTATE");

        // Assert: a path escaping the bundle is invalid_parameter (22023).
        assert_eq!(
            sqlstate, "22023",
            "a traversing content path must be rejected with 22023",
        );
    }

    #[pg_test]
    fn refresh_bundle_rejects_a_content_bundle() {
        // Arrange: a content bundle that has no filesystem root to refresh from.
        let (bundle_id, ..) = register_content(
            "handbook",
            vec!["alpha.md".to_owned()],
            vec![CONTENT_ALPHA.as_bytes().to_vec()],
        );
        Spi::run(
            "CREATE FUNCTION pg_temp.refresh_content_sqlstate(bid bigint) RETURNS text
             LANGUAGE plpgsql
             AS $probe$
             BEGIN
                 PERFORM pgokf.refresh_bundle(bid);
                 RETURN 'no-error';
             EXCEPTION WHEN OTHERS THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("refresh probe is creatable");

        // Act
        let sqlstate = Spi::get_one_with_args::<String>(
            "SELECT pg_temp.refresh_content_sqlstate($1)",
            &[bundle_id.into()],
        )
        .expect("refresh probe executes")
        .expect("the probe reports a SQLSTATE");

        // Assert: refreshing a content bundle from disk is a caller error
        // (22023).
        assert_eq!(
            sqlstate, "22023",
            "refresh_bundle must reject a content-sourced bundle with 22023",
        );
    }

    #[pg_test]
    fn register_bundle_content_denies_a_reader_role() {
        // Arrange: a role granted only pgokf_reader, and a probe that runs
        // register_bundle_content as that role.
        Spi::run("CREATE ROLE pgokf_content_reader").expect("reader role is creatable");
        Spi::run("GRANT pgokf_reader TO pgokf_content_reader").expect("reader role is grantable");
        Spi::run(
            "CREATE FUNCTION pg_temp.content_denied_sqlstate() RETURNS text
             LANGUAGE plpgsql
             SET role TO pgokf_content_reader
             AS $probe$
             BEGIN
                 PERFORM pgokf.register_bundle_content(
                     'denied',
                     ARRAY['a.md']::text[],
                     ARRAY['data'::bytea]::bytea[]);
                 RETURN 'not-denied';
             EXCEPTION WHEN insufficient_privilege THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("content authz probe is creatable");

        // Act
        let sqlstate = Spi::get_one::<String>("SELECT pg_temp.content_denied_sqlstate()")
            .expect("content authz probe executes")
            .expect("the probe reports a SQLSTATE");

        // Assert: a plain reader is denied ingestion with 42501.
        assert_eq!(
            sqlstate, "42501",
            "a reader role must be denied register_bundle_content with 42501",
        );
    }

    #[pg_test]
    fn register_bundle_content_with_store_source_round_trips_bytes() {
        // Arrange: enable the store_source tier, then ingest content.
        enable_store_source();
        let (bundle_id, ..) = register_content(
            "handbook",
            vec!["alpha.md".to_owned(), "nested/beta.md".to_owned()],
            vec![
                CONTENT_ALPHA.as_bytes().to_vec(),
                CONTENT_BETA.as_bytes().to_vec(),
            ],
        );

        // Act: retrieve the stored source bytes for alpha.
        let stored = Spi::get_one_with_args::<Vec<u8>>(
            "SELECT pgokf.get_concept_source($1, 'alpha')",
            &[bundle_id.into()],
        )
        .expect("get_concept_source executes")
        .expect("alpha carries stored source bytes");

        // Assert: the stored bytes equal the exact in-memory content that was
        // streamed in.
        assert_eq!(
            stored,
            CONTENT_ALPHA.as_bytes(),
            "store_source must persist the exact content bytes for a content bundle",
        );
    }

    // ---------------------------------------------------------------------
    // F1: sync/audit log (pgokf_private.sync_log + pgokf.list_sync_log).
    // ---------------------------------------------------------------------

    #[pg_test]
    fn sync_log_records_every_op_and_prunes_by_retention() {
        // Arrange: registering a filesystem bundle appends one 'register' row
        // whose total equals the two synced concepts.
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);
        let register_rows = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf_private.sync_log WHERE bundle_id = $1 AND op = 'register'",
            &[bundle_id.into()],
        )
        .expect("register audit query executes")
        .expect("count is not NULL");
        assert_eq!(register_rows, 1, "a register appends exactly one audit row");
        let register_total = Spi::get_one_with_args::<i32>(
            "SELECT total FROM pgokf_private.sync_log WHERE bundle_id = $1 AND op = 'register'",
            &[bundle_id.into()],
        )
        .expect("register total query executes")
        .expect("total is not NULL");
        assert_eq!(register_total, 2, "the register row records both concepts");

        // Act/Assert: a refresh appends a 'refresh' row.
        Spi::run_with_args("SELECT pgokf.refresh_bundle($1)", &[bundle_id.into()])
            .expect("refresh executes");
        let refresh_rows = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf_private.sync_log WHERE bundle_id = $1 AND op = 'refresh'",
            &[bundle_id.into()],
        )
        .expect("refresh audit query executes")
        .expect("count is not NULL");
        assert_eq!(refresh_rows, 1, "a refresh appends exactly one audit row");

        // A content ingest appends a 'content' row for its own bundle.
        let (content_id, ..) = register_content(
            "audited",
            vec!["alpha.md".to_owned()],
            vec![CONTENT_ALPHA.as_bytes().to_vec()],
        );
        let content_rows = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf_private.sync_log WHERE bundle_id = $1 AND op = 'content'",
            &[content_id.into()],
        )
        .expect("content audit query executes")
        .expect("count is not NULL");
        assert_eq!(content_rows, 1, "a content ingest appends one audit row");

        // Unregistering appends an 'unregister' row that survives the delete
        // (sync_log.bundle_id is intentionally FK-free).
        Spi::run_with_args("SELECT pgokf.unregister_bundle($1)", &[bundle_id.into()])
            .expect("unregister executes");
        let unregister_rows = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf_private.sync_log
             WHERE bundle_id = $1 AND op = 'unregister'",
            &[bundle_id.into()],
        )
        .expect("unregister audit query executes")
        .expect("count is not NULL");
        assert_eq!(
            unregister_rows, 1,
            "an unregister appends one audit row that outlives the bundle",
        );

        // Prune: an artificially old row, a 1-day retention, and a fresh sync
        // whose tail prune must delete history older than the window.
        Spi::run(
            "INSERT INTO pgokf_private.sync_log (bundle_id, bundle_path, op, synced_at)
             VALUES (NULL, '/legacy', 'register', now() - interval '10 days')",
        )
        .expect("an old audit row is insertable");
        Spi::run("SELECT pgokf.set_config('sync_log_retention_days', '1'::jsonb)")
            .expect("retention is configurable");
        let _ = register_content(
            "audited",
            vec!["alpha.md".to_owned()],
            vec![CONTENT_ALPHA.as_bytes().to_vec()],
        );
        let stale_rows = Spi::get_one::<i64>(
            "SELECT count(*) FROM pgokf_private.sync_log WHERE synced_at < now() - interval '1 day'",
        )
        .expect("stale audit query executes")
        .expect("count is not NULL");
        assert_eq!(
            stale_rows, 0,
            "the retention prune removed rows older than the window",
        );
    }

    #[pg_test]
    fn list_sync_log_returns_rows_to_a_reader() {
        // Arrange: a registered bundle (so an audit row exists) plus a role
        // granted only pgokf_reader.
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);
        Spi::run("CREATE ROLE pgokf_log_reader").expect("reader role is creatable");
        Spi::run("GRANT pgokf_reader TO pgokf_log_reader").expect("reader role is grantable");
        Spi::run(
            "CREATE FUNCTION pg_temp.reader_log_count(bid bigint) RETURNS bigint
             LANGUAGE plpgsql
             SET role TO pgokf_log_reader
             AS $probe$
             BEGIN
                 RETURN (SELECT count(*) FROM pgokf.list_sync_log(bid));
             END
             $probe$;",
        )
        .expect("reader log probe is creatable");

        // Act: a plain reader reads the log through the SECURITY DEFINER function.
        let count = Spi::get_one_with_args::<i64>(
            "SELECT pg_temp.reader_log_count($1)",
            &[bundle_id.into()],
        )
        .expect("reader log probe executes")
        .expect("count is not NULL");

        // Assert: the reader sees the bundle's audit row(s).
        assert!(
            count >= 1,
            "a reader can read the audit log via pgokf.list_sync_log",
        );
    }

    // ---------------------------------------------------------------------
    // F2: bundle enable/disable lifecycle (pgokf.set_bundle_enabled).
    // ---------------------------------------------------------------------

    #[pg_test]
    fn set_bundle_enabled_hides_from_search_and_traversal_and_is_reversible() {
        // Arrange: a registered bundle is enabled by default, so search and
        // traversal both surface it.
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);
        let pre_hits =
            Spi::get_one::<i64>("SELECT count(*) FROM pgokf.concept_search('peregrine')")
                .expect("pre-disable search executes")
                .expect("count is not NULL");
        assert_eq!(pre_hits, 1, "an enabled bundle is searchable");
        let pre_neighbors = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concept_neighbors('alpha', 2, $1)",
            &[bundle_id.into()],
        )
        .expect("pre-disable traversal executes")
        .expect("count is not NULL");
        assert_eq!(pre_neighbors, 1, "alpha reaches beta while enabled");

        // Act: disable the bundle.
        let disabled = Spi::get_one_with_args::<bool>(
            "SELECT enabled FROM pgokf.set_bundle_enabled($1, false)",
            &[bundle_id.into()],
        )
        .expect("set_bundle_enabled(false) executes")
        .expect("enabled is not NULL");
        assert!(
            !disabled,
            "set_bundle_enabled(false) reports the bundle disabled"
        );

        // Assert: disabled hides the bundle from BOTH search and traversal.
        let hidden_hits =
            Spi::get_one::<i64>("SELECT count(*) FROM pgokf.concept_search('peregrine')")
                .expect("post-disable search executes")
                .expect("count is not NULL");
        assert_eq!(hidden_hits, 0, "a disabled bundle is hidden from search");
        let hidden_neighbors = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concept_neighbors('alpha', 2, $1)",
            &[bundle_id.into()],
        )
        .expect("post-disable traversal executes")
        .expect("count is not NULL");
        assert_eq!(
            hidden_neighbors, 0,
            "a disabled bundle's concepts are not traversed",
        );

        // Act/Assert: re-enabling restores both surfaces (fully reversible).
        let enabled = Spi::get_one_with_args::<bool>(
            "SELECT enabled FROM pgokf.set_bundle_enabled($1, true)",
            &[bundle_id.into()],
        )
        .expect("set_bundle_enabled(true) executes")
        .expect("enabled is not NULL");
        assert!(
            enabled,
            "set_bundle_enabled(true) reports the bundle enabled"
        );
        let post_hits =
            Spi::get_one::<i64>("SELECT count(*) FROM pgokf.concept_search('peregrine')")
                .expect("post-enable search executes")
                .expect("count is not NULL");
        assert_eq!(post_hits, 1, "re-enabling restores search visibility");
        let post_neighbors = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pgokf.concept_neighbors('alpha', 2, $1)",
            &[bundle_id.into()],
        )
        .expect("post-enable traversal executes")
        .expect("count is not NULL");
        assert_eq!(post_neighbors, 1, "re-enabling restores traversal");
    }

    #[pg_test]
    fn set_bundle_enabled_denies_a_reader_role() {
        // Arrange: a registered bundle and a role granted only pgokf_reader.
        let bundle = FixtureBundle::create();
        let bundle_id = register_fixture(&bundle);
        Spi::run("CREATE ROLE pgokf_enable_reader").expect("reader role is creatable");
        Spi::run("GRANT pgokf_reader TO pgokf_enable_reader").expect("reader role is grantable");
        Spi::run(
            "CREATE FUNCTION pg_temp.enable_denied_sqlstate(bid bigint) RETURNS text
             LANGUAGE plpgsql
             SET role TO pgokf_enable_reader
             AS $probe$
             BEGIN
                 PERFORM pgokf.set_bundle_enabled(bid, false);
                 RETURN 'not-denied';
             EXCEPTION WHEN insufficient_privilege THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("enable authz probe is creatable");

        // Act
        let sqlstate = Spi::get_one_with_args::<String>(
            "SELECT pg_temp.enable_denied_sqlstate($1)",
            &[bundle_id.into()],
        )
        .expect("enable authz probe executes")
        .expect("the probe reports a SQLSTATE");

        // Assert: a plain reader is denied the writer-tier toggle with 42501.
        assert_eq!(
            sqlstate, "42501",
            "a reader role must be denied set_bundle_enabled with 42501",
        );
    }

    // ---------------------------------------------------------------------
    // F3: change notification (notify_channel).
    // ---------------------------------------------------------------------

    #[pg_test]
    fn notify_channel_is_gated_and_validated() {
        // Arrange: a valid channel enables notification. The test transaction is
        // rolled back, so a LISTEN could not observe the delivered message here;
        // instead assert the gated path runs cleanly (the sync fires pg_notify
        // without error) and that an unsafe channel name is rejected.
        Spi::run("SELECT pgokf.set_config('notify_channel', to_jsonb('pgokf_events'::text))")
            .expect("a valid notify channel is accepted");
        let bundle = FixtureBundle::create();

        // Act: a sync with the channel set completes and fires the notification.
        let added = Spi::get_one_with_args::<i32>(
            "SELECT added FROM pgokf.register_bundle($1) AS r",
            &[bundle.path().into()],
        )
        .expect("register with notify_channel set executes")
        .expect("added is not NULL");
        assert_eq!(
            added, 2,
            "a sync with notify_channel set completes normally"
        );

        // Assert: an unsafe channel name is rejected with 22023.
        Spi::run(
            "CREATE FUNCTION pg_temp.bad_channel_sqlstate() RETURNS text
             LANGUAGE plpgsql
             AS $probe$
             BEGIN
                 PERFORM pgokf.set_config('notify_channel', to_jsonb('1 drop'::text));
                 RETURN 'not-rejected';
             EXCEPTION WHEN OTHERS THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("bad channel probe is creatable");
        let bad = Spi::get_one::<String>("SELECT pg_temp.bad_channel_sqlstate()")
            .expect("bad channel probe executes")
            .expect("the probe reports a SQLSTATE");
        assert_eq!(bad, "22023", "an unsafe notify_channel name is rejected");
    }

    // ---------------------------------------------------------------------
    // F4/F5/F6: observability (catalog_stats / health / stale_concepts).
    // ---------------------------------------------------------------------

    #[pg_test]
    fn observability_functions_return_expected_shapes() {
        // Arrange: the rich OKF v0.2 fixture (five concepts, one carrying a
        // stale_after) registered into the catalog.
        let fixture = RichFixture::create();
        let bundle_id = register_rich(&fixture);

        // catalog_stats: the bundle row reports its indexed concepts, a fresh
        // (not stale) sync, and its enabled flag.
        Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT indexed_concepts, is_stale, enabled
                     FROM pgokf.catalog_stats() WHERE bundle_id = $1",
                    Some(1),
                    &[bundle_id.into()],
                )
                .expect("catalog_stats executes")
                .first();
            assert_eq!(
                row.get::<i64>(1).expect("indexed_concepts readable"),
                Some(5),
                "catalog_stats counts the five rich-metadata concepts",
            );
            assert_eq!(
                row.get::<bool>(2).expect("is_stale readable"),
                Some(false),
                "a just-synced bundle is not stale",
            );
            assert_eq!(
                row.get::<bool>(3).expect("enabled readable"),
                Some(true),
                "a fresh bundle is enabled",
            );
        });

        // health: ok with sane roles/config, reporting the native backend.
        let ok = Spi::get_one::<bool>("SELECT (pgokf.health() ->> 'ok')::boolean")
            .expect("health executes")
            .expect("ok is not NULL");
        assert!(ok, "health reports ok when roles and config are sane");
        let backend = Spi::get_one::<String>("SELECT pgokf.health() ->> 'search_backend'")
            .expect("health backend read executes")
            .expect("search_backend is present");
        assert_eq!(backend, "native", "health reports the default backend");

        // stale_concepts: with a far-future as_of, the one concept carrying a
        // stale_after surfaces with its path and type.
        Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT concept_id, path, concept_type
                     FROM pgokf.stale_concepts($1, '2999-01-01T00:00:00Z'::timestamptz)",
                    Some(1),
                    &[bundle_id.into()],
                )
                .expect("stale_concepts executes")
                .first();
            assert_eq!(
                row.get::<String>(1).expect("concept_id readable"),
                Some("rich-concept".to_owned()),
                "the concept carrying stale_after surfaces as stale",
            );
            assert!(
                row.get::<String>(2).expect("path readable").is_some(),
                "the stale concept carries its bundle-relative path",
            );
        });
    }

    // ---------------------------------------------------------------------
    // F7: OKF version-conformance policy (okf_version_policy).
    // ---------------------------------------------------------------------

    #[pg_test]
    fn okf_version_policy_rejects_only_under_reject() {
        // Arrange: an in-memory bundle whose root index.md declares an
        // unsupported okf_version.
        let bogus_index = "---\nokf_version: \"9.9\"\n---\n\n# Bundle\n";
        let paths = vec!["index.md".to_owned(), "alpha.md".to_owned()];
        let contents = vec![
            bogus_index.as_bytes().to_vec(),
            CONTENT_ALPHA.as_bytes().to_vec(),
        ];

        // Act/Assert: under the default 'warn' policy the bundle still indexes
        // (index.md is reserved, so exactly the one alpha concept is added).
        let (_id, added, ..) = register_content("warned", paths, contents);
        assert_eq!(
            added, 1,
            "the warn policy indexes despite an unsupported okf_version",
        );

        // Switch to 'reject' and a bogus okf_version aborts the sync with 22023.
        Spi::run("SELECT pgokf.set_config('okf_version_policy', to_jsonb('reject'::text))")
            .expect("okf_version_policy is configurable");
        let bundle = FixtureBundle::create();
        fs::write(bundle.root.join("index.md"), bogus_index).expect("bogus index.md is writable");
        Spi::run(
            "CREATE FUNCTION pg_temp.reject_register_sqlstate(p text) RETURNS text
             LANGUAGE plpgsql
             AS $probe$
             BEGIN
                 PERFORM pgokf.register_bundle(p);
                 RETURN 'not-rejected';
             EXCEPTION WHEN OTHERS THEN
                 RETURN SQLSTATE;
             END
             $probe$;",
        )
        .expect("reject register probe is creatable");
        let sqlstate = Spi::get_one_with_args::<String>(
            "SELECT pg_temp.reject_register_sqlstate($1)",
            &[bundle.path().into()],
        )
        .expect("reject register probe executes")
        .expect("the probe reports a SQLSTATE");
        assert_eq!(
            sqlstate, "22023",
            "the reject policy aborts a bundle with an unsupported okf_version",
        );
    }
}
