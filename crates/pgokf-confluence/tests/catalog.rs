// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in integration coverage for a disposable database with pgokf installed.
//! Never falls back to the application's database environment variables.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use pgokf_confluence::convert::convert_pages;
use pgokf_confluence::sink::{Catalog, PRODUCER, PendingOutcome};
use pgokf_confluence::snapshot::{self, Snapshot};
use pgokf_confluence::types::{Document, Page, SourceIdentity};
use tokio::task::JoinHandle;
use tokio_postgres::Client;

struct TestDatabase {
    url: String,
    native: Client,
    connection: JoinHandle<()>,
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        self.connection.abort();
    }
}

impl TestDatabase {
    async fn connect() -> Result<Self> {
        let url = std::env::var("PGOKF_CONFLUENCE_TEST_DATABASE_URL").context(
            "set PGOKF_CONFLUENCE_TEST_DATABASE_URL to a disposable database with pgokf installed",
        )?;
        let (native, connection) = pgokf_pgconn::connect(&url, false).await.map_err(|_| {
            anyhow::anyhow!("could not connect to the disposable Confluence test database")
        })?;
        native
            .batch_execute("SET statement_timeout = '10s'; SET lock_timeout = '1s'")
            .await?;
        pgokf_pgconn::set_tenant(&native, "default").await?;
        Ok(Self {
            url,
            native,
            connection,
        })
    }
}

fn source() -> SourceIdentity {
    SourceIdentity {
        site_url: "https://integration-test.atlassian.net".into(),
        space_id: "42".into(),
    }
}

fn bundle_name(label: &str) -> String {
    format!(
        "confluence_test_{}_{label}_{:016x}",
        std::process::id(),
        rand::random::<u64>()
    )
}

fn documents(version: u64) -> Result<Vec<Document>> {
    let pages = ["1", "2"].map(|id| Page {
        id: id.into(),
        space_id: "42".into(),
        title: format!("Integration page {id}"),
        parent_id: None,
        version,
        modified_at: Some("2026-11-06T10:00:00Z".into()),
        storage: format!("<p>Integration content {id}, revision {version}.</p>"),
        web_url: format!("https://integration-test.atlassian.net/wiki/spaces/AI/pages/{id}"),
    });
    convert_pages(&source(), &pages)
}

fn snapshot(catalog: &Catalog, name: &str, docs: &[Document]) -> Result<Snapshot> {
    let mut value = Snapshot::new(source(), name.into(), "default".into(), docs)?;
    value.database_fingerprint = Some(catalog.fingerprint().into());
    Ok(value)
}

async fn native_register(db: &Client, name: &str, docs: &[Document], options: &str) -> Result<i64> {
    let paths: Vec<_> = docs.iter().map(|doc| doc.path.clone()).collect();
    let contents: Vec<_> = docs.iter().map(|doc| doc.bytes.clone()).collect();
    let row = db
        .query_one(
            "SELECT bundle_id FROM pgokf.register_bundle_content($1, $2, $3, $4::text::jsonb)",
            &[&name, &paths, &contents, &options],
        )
        .await?;
    Ok(row.get(0))
}

async fn assert_projection(db: &Client, bundle_id: i64, expected: &Snapshot) -> Result<()> {
    let rows = db.query(
        "SELECT path, file_hash FROM pgokf.concepts WHERE bundle_id = $1 AND tenant_id = 'default' ORDER BY path",
        &[&bundle_id],
    ).await?;
    let actual: BTreeMap<String, String> = rows
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(
        actual,
        expected.hashes(),
        "catalog projection differs from snapshot"
    );
    Ok(())
}

/// The bundle-level freshness row a first import seeds from the sync context:
/// the producer label and the Confluence revisions it materialized.
async fn assert_import_provenance(db: &Client, bundle_id: i64, expected: &Snapshot) -> Result<()> {
    let row = db
        .query_one(
            "SELECT producer, observed_revision FROM pgokf.effective_freshness \
             WHERE bundle_id = $1 AND scope_kind = 'bundle'",
            &[&bundle_id],
        )
        .await?;
    let producer: Option<String> = row.get(0);
    let observed: Option<String> = row.get(1);
    assert_eq!(producer.as_deref(), Some(PRODUCER));
    assert_eq!(
        observed.as_deref(),
        Some(expected.source_generation().as_str())
    );
    Ok(())
}

async fn check_native_lock(db: &TestDatabase, name: &str, docs: &[Document]) -> Result<()> {
    let overlapping = Catalog::connect(&db.url, false, "default", name).await;
    assert!(
        overlapping.is_err(),
        "a second catalog must not acquire the same lock"
    );
    let error = native_register(&db.native, name, docs, "{}")
        .await
        .expect_err("native registration must honor the companion's held lock");
    let message = format!("{error:#}");
    assert!(
        message.contains("lock") || message.contains("timeout"),
        "native writer failed for an unexpected reason: {message}"
    );
    Ok(())
}

async fn check_parser_failure_rolls_back(
    db: &Client,
    catalog: &mut Catalog,
    name: &str,
    previous: &Snapshot,
    docs: &[Document],
) -> Result<()> {
    let mut invalid_docs = docs.to_vec();
    invalid_docs[0].bytes = b"---\nname: [unterminated YAML\n---\nInvalid document.\n".to_vec();
    let mut invalid = snapshot(catalog, name, &invalid_docs)?;
    assert!(
        catalog
            .apply(
                &mut invalid,
                Some(previous),
                &invalid_docs,
                false,
                record_nothing
            )
            .await
            .is_err()
    );
    assert_projection(db, previous.bundle_id.unwrap(), previous).await?;
    Ok(())
}

async fn check_lifecycle_and_recovery(db: &TestDatabase) -> Result<()> {
    let name = bundle_name("lifecycle");
    let mut catalog = Catalog::connect(&db.url, false, "default", &name).await?;
    let docs = documents(1)?;
    check_native_lock(db, &name, &docs).await?;
    let mut first = snapshot(&catalog, &name, &docs)?;
    let report = catalog
        .apply(&mut first, None, &docs, false, record_nothing)
        .await?;
    assert_eq!(
        (report.added, report.updated, report.removed, report.total),
        (2, 0, 0, 2)
    );
    let bundle_id = report.bundle_id;
    assert_projection(&db.native, bundle_id, &first).await?;
    assert_import_provenance(&db.native, bundle_id, &first).await?;

    let mut unchanged = snapshot(&catalog, &name, &docs)?;
    let report = catalog
        .apply(&mut unchanged, Some(&first), &docs, false, record_nothing)
        .await?;
    assert_eq!(
        (
            report.added,
            report.updated,
            report.removed,
            report.unchanged
        ),
        (0, 0, 0, 2)
    );

    let updated_docs = documents(2)?;
    let mut updated = snapshot(&catalog, &name, &updated_docs)?;
    let report = catalog
        .apply(
            &mut updated,
            Some(&unchanged),
            &updated_docs,
            false,
            record_nothing,
        )
        .await?;
    assert_eq!((report.updated, report.total), (2, 2));
    assert_projection(&db.native, bundle_id, &updated).await?;
    check_parser_failure_rolls_back(&db.native, &mut catalog, &name, &updated, &updated_docs)
        .await?;

    let reduced_docs = &updated_docs[..1];
    let mut reduced = snapshot(&catalog, &name, reduced_docs)?;
    let error = catalog
        .apply(
            &mut reduced,
            Some(&updated),
            reduced_docs,
            false,
            record_nothing,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("pages disappeared"));
    assert_projection(&db.native, bundle_id, &updated).await?;
    let report = catalog
        .apply(
            &mut reduced,
            Some(&updated),
            reduced_docs,
            true,
            record_nothing,
        )
        .await?;
    // Native total counts all plan buckets, including removed documents.
    assert_eq!((report.removed, report.unchanged, report.total), (1, 1, 2));
    assert_projection(&db.native, bundle_id, &reduced).await?;

    // Simulate a successful commit whose checkpoint was never persisted: only
    // the exact committed bytes and durable source binding may be recovered.
    let mut recovered = snapshot(&catalog, &name, reduced_docs)?;
    assert!(recovered.bundle_id.is_none());
    let report = catalog
        .apply(&mut recovered, None, reduced_docs, false, record_nothing)
        .await?;
    assert_eq!((report.bundle_id, report.unchanged), (bundle_id, 1));
    assert_eq!(recovered.bundle_id, Some(bundle_id));
    let changed_docs = documents(3)?;
    let mut different = snapshot(&catalog, &name, &changed_docs)?;
    let error = catalog
        .apply(&mut different, None, &changed_docs, false, record_nothing)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("no matching checkpoint"));
    assert_projection(&db.native, bundle_id, &recovered).await?;
    Ok(())
}

async fn check_foreign_bundle(db: &TestDatabase) -> Result<()> {
    let name = bundle_name("foreign");
    let docs = documents(1)?;
    let bundle_id = native_register(&db.native, &name, &docs, "{}").await?;
    let mut catalog = Catalog::connect(&db.url, false, "default", &name).await?;
    let mut next = snapshot(&catalog, &name, &docs)?;
    let error = catalog
        .apply(&mut next, None, &docs, false, record_nothing)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("not owned by this Confluence source")
    );
    assert_projection(&db.native, bundle_id, &next).await?;
    Ok(())
}

async fn check_out_of_band_changes(db: &TestDatabase) -> Result<()> {
    let name = bundle_name("external");
    let original = documents(1)?;
    let binding =
        serde_json::json!({"pgokf_confluence": {"schema_version": 1, "source": source()}})
            .to_string();
    let bundle_id = native_register(&db.native, &name, &original, &binding).await?;
    let external = documents(2)?;
    assert_eq!(
        native_register(&db.native, &name, &external, &binding).await?,
        bundle_id
    );
    let mut catalog = Catalog::connect(&db.url, false, "default", &name).await?;
    let mut previous = snapshot(&catalog, &name, &original)?;
    previous.bundle_id = Some(bundle_id);
    let proposed_docs = documents(3)?;
    let mut next = snapshot(&catalog, &name, &proposed_docs)?;
    let error = catalog
        .apply(
            &mut next,
            Some(&previous),
            &proposed_docs,
            false,
            record_nothing,
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("changed outside this checkpoint")
    );
    assert_projection(
        &db.native,
        bundle_id,
        &snapshot(&catalog, &name, &external)?,
    )
    .await?;
    Ok(())
}

/// A `before_commit` hook for imports that keep no state file.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the before_commit hook signature returns a Result"
)]
fn record_nothing(_: &Snapshot) -> Result<()> {
    Ok(())
}

/// The reviewed crash: S1 commits but its checkpoint is never promoted, a
/// page changes, and the next run must still import S2 - measuring removals
/// against S1, which the catalog holds, not the outdated S0 checkpoint.
async fn check_commit_without_promotion_recovers(db: &TestDatabase) -> Result<()> {
    let name = bundle_name("pending");
    let directory = tempfile::tempdir()?;
    let state = directory.path().join("state.json");
    let record = |committing: &Snapshot| snapshot::write_pending(&state, committing);
    let mut catalog = Catalog::connect(&db.url, false, "default", &name).await?;
    let s0_docs = documents(1)?;
    let mut s0 = snapshot(&catalog, &name, &s0_docs)?;
    catalog
        .apply(&mut s0, None, &s0_docs, false, record)
        .await?;
    snapshot::promote_pending(&state)?;
    let s1_docs = documents(2)?;
    let mut s1 = snapshot(&catalog, &name, &s1_docs)?;
    catalog
        .apply(&mut s1, Some(&s0), &s1_docs, false, record)
        .await?;
    drop(catalog); // the process dies before promoting S1

    let mut catalog = Catalog::connect(&db.url, false, "default", &name).await?;
    let checkpoint = snapshot::read_checkpoint(&state)?;
    assert_eq!(checkpoint.as_ref(), Some(&s0), "checkpoint still names S0");
    let s2_docs = documents(3)?;
    let mut outdated = snapshot(&catalog, &name, &s2_docs)?;
    let error = catalog
        .apply(&mut outdated, checkpoint.as_ref(), &s2_docs, false, record)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("changed outside this checkpoint")
    );

    let pending = snapshot::read_pending(&state)?.context("pending checkpoint recorded")?;
    assert_eq!(pending, s1);
    assert_eq!(
        catalog
            .resolve_pending(&pending, checkpoint.as_ref())
            .await?,
        PendingOutcome::Committed
    );
    snapshot::promote_pending(&state)?;
    let previous = snapshot::read_checkpoint(&state)?.context("S1 promoted")?;
    assert_eq!(previous, s1);
    // S2 edits page 1 and drops page 2: a removal measured against S1.
    let reduced = &s2_docs[..1];
    let mut s2 = snapshot(&catalog, &name, reduced)?;
    let error = catalog
        .apply(&mut s2, Some(&previous), reduced, false, record)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("pages disappeared"));
    catalog
        .apply(&mut s2, Some(&previous), reduced, true, record)
        .await?;
    snapshot::promote_pending(&state)?;
    assert_projection(&db.native, previous.bundle_id.unwrap(), &s2).await?;
    Ok(())
}

/// A pending checkpoint whose import never committed is discarded, and the
/// catalog keeps the last promoted snapshot.
async fn check_uncommitted_pending_is_discarded(db: &TestDatabase) -> Result<()> {
    let name = bundle_name("uncommitted");
    let catalog_docs = documents(1)?;
    let mut catalog = Catalog::connect(&db.url, false, "default", &name).await?;
    let mut s0 = snapshot(&catalog, &name, &catalog_docs)?;
    catalog
        .apply(&mut s0, None, &catalog_docs, false, record_nothing)
        .await?;
    let mut never_committed = snapshot(&catalog, &name, &documents(2)?)?;
    never_committed.bundle_id = s0.bundle_id;
    assert_eq!(
        catalog.resolve_pending(&never_committed, Some(&s0)).await?,
        PendingOutcome::NotCommitted
    );
    assert_projection(&db.native, s0.bundle_id.unwrap(), &s0).await?;
    Ok(())
}

/// Guards that refuse before any catalog write: a snapshot bound to another
/// database, documents that differ from the snapshot, and a disabled bundle.
async fn check_destination_guards(db: &TestDatabase) -> Result<()> {
    let name = bundle_name("guards");
    let docs = documents(1)?;
    let mut catalog = Catalog::connect(&db.url, false, "default", &name).await?;

    let mut foreign = snapshot(&catalog, &name, &docs)?;
    foreign.database_fingerprint = Some("another-endpoint".into());
    let error = catalog
        .apply(&mut foreign, None, &docs, false, record_nothing)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("database binding mismatch"));

    let mut mismatched = snapshot(&catalog, &name, &docs)?;
    let other_docs = documents(2)?;
    let error = catalog
        .apply(&mut mismatched, None, &other_docs, false, record_nothing)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("do not match complete snapshot"));

    let mut first = snapshot(&catalog, &name, &docs)?;
    catalog
        .apply(&mut first, None, &docs, false, record_nothing)
        .await?;
    // Release the importer's bundle lock so another writer can disable it.
    drop(catalog);
    db.native
        .execute(
            "SELECT pgokf.set_bundle_enabled($1, false)",
            &[&first.bundle_id.unwrap()],
        )
        .await?;
    let mut catalog = Catalog::connect(&db.url, false, "default", &name).await?;
    let mut again = snapshot(&catalog, &name, &docs)?;
    let error = catalog
        .apply(&mut again, Some(&first), &docs, false, record_nothing)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("disabled or retired"));
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit PGOKF_CONFLUENCE_TEST_DATABASE_URL for a disposable pgokf database"]
async fn catalog_import_guards_and_recovery() -> Result<()> {
    let database = TestDatabase::connect().await?;
    check_lifecycle_and_recovery(&database).await?;
    check_foreign_bundle(&database).await?;
    check_out_of_band_changes(&database).await?;
    check_commit_without_promotion_recovers(&database).await?;
    check_uncommitted_pending_is_discarded(&database).await?;
    check_destination_guards(&database).await?;
    Ok(())
}
