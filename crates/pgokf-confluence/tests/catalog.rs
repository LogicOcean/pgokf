// SPDX-License-Identifier: AGPL-3.0-only
//! Opt-in integration coverage for a disposable database with pgokf installed.
//! Never falls back to the application's database environment variables.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use pgokf_confluence::convert::convert_pages;
use pgokf_confluence::sink::{Catalog, PRODUCER};
use pgokf_confluence::snapshot::Snapshot;
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
        cloud_id: None,
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
            .apply(&mut invalid, Some(previous), &invalid_docs, false)
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
    let report = catalog.apply(&mut first, None, &docs, false).await?;
    assert_eq!(
        (report.added, report.updated, report.removed, report.total),
        (2, 0, 0, 2)
    );
    let bundle_id = report.bundle_id;
    assert_projection(&db.native, bundle_id, &first).await?;
    assert_import_provenance(&db.native, bundle_id, &first).await?;

    let mut unchanged = snapshot(&catalog, &name, &docs)?;
    let report = catalog
        .apply(&mut unchanged, Some(&first), &docs, false)
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
        .apply(&mut updated, Some(&unchanged), &updated_docs, false)
        .await?;
    assert_eq!((report.updated, report.total), (2, 2));
    assert_projection(&db.native, bundle_id, &updated).await?;
    check_parser_failure_rolls_back(&db.native, &mut catalog, &name, &updated, &updated_docs)
        .await?;

    let reduced_docs = &updated_docs[..1];
    let mut reduced = snapshot(&catalog, &name, reduced_docs)?;
    let error = catalog
        .apply(&mut reduced, Some(&updated), reduced_docs, false)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("pages disappeared"));
    assert_projection(&db.native, bundle_id, &updated).await?;
    let report = catalog
        .apply(&mut reduced, Some(&updated), reduced_docs, true)
        .await?;
    // Native total counts all plan buckets, including removed documents.
    assert_eq!((report.removed, report.unchanged, report.total), (1, 1, 2));
    assert_projection(&db.native, bundle_id, &reduced).await?;

    // Simulate a successful commit whose checkpoint was never persisted: only
    // the exact committed bytes and durable source binding may be recovered.
    let mut recovered = snapshot(&catalog, &name, reduced_docs)?;
    assert!(recovered.bundle_id.is_none());
    let report = catalog
        .apply(&mut recovered, None, reduced_docs, false)
        .await?;
    assert_eq!((report.bundle_id, report.unchanged), (bundle_id, 1));
    assert_eq!(recovered.bundle_id, Some(bundle_id));
    let changed_docs = documents(3)?;
    let mut different = snapshot(&catalog, &name, &changed_docs)?;
    let error = catalog
        .apply(&mut different, None, &changed_docs, false)
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
        .apply(&mut next, None, &docs, false)
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
        .apply(&mut next, Some(&previous), &proposed_docs, false)
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

#[tokio::test]
#[ignore = "requires explicit PGOKF_CONFLUENCE_TEST_DATABASE_URL for a disposable pgokf database"]
async fn catalog_import_guards_and_recovery() -> Result<()> {
    let database = TestDatabase::connect().await?;
    check_lifecycle_and_recovery(&database).await?;
    check_foreign_bundle(&database).await?;
    check_out_of_band_changes(&database).await?;
    Ok(())
}
