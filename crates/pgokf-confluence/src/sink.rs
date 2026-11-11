// SPDX-License-Identifier: AGPL-3.0-only
//! Transactional catalog writes with ownership and exact-snapshot verification.
use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, GenericClient};

use crate::snapshot::Snapshot;
use crate::types::{Document, SourceIdentity};

/// The opaque producer label every import records in the catalog-change
/// outbox. Deliberately unversioned, so event consumers can route on it
/// across releases; the converter version is carried in each document.
pub const PRODUCER: &str = "pgokf-confluence";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Binding {
    schema_version: u32,
    source: SourceIdentity,
}

struct CatalogState {
    bundle_id: i64,
    binding: Option<Binding>,
    hashes: BTreeMap<String, String>,
}

/// Result reported only after transaction commit and full projection verification.
#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub bundle_id: i64,
    pub added: i32,
    pub updated: i32,
    pub removed: i32,
    pub unchanged: i32,
    pub total: i32,
}

/// A database connection holding pgokf's native per-bundle lock for the run.
/// Dropping it closes the session and releases the lock, including on failure.
pub struct Catalog {
    client: Client,
    connection: JoinHandle<()>,
    fingerprint: String,
    tenant: String,
    bundle_name: String,
}

impl Drop for Catalog {
    fn drop(&mut self) {
        self.connection.abort();
    }
}

impl Catalog {
    /// Connect and acquire the same advisory lock used by pgokf content ingestion.
    ///
    /// # Errors
    /// Rejects invalid credentials, overlapping imports, and unavailable databases.
    pub async fn connect(
        database_url: &str,
        tls: bool,
        tenant: &str,
        bundle_name: &str,
    ) -> Result<Self> {
        let fingerprint = database_fingerprint(database_url)?;
        // tokio-postgres errors name options, never their values, so the
        // cause (refused TLS, failed authentication, unreachable host) is kept.
        let (client, connection) = pgokf_pgconn::connect(database_url, tls)
            .await
            .context("connecting to the pgokf catalog")?;
        let catalog = Self {
            client,
            connection,
            fingerprint,
            tenant: tenant.into(),
            bundle_name: bundle_name.into(),
        };
        catalog
            .client
            .batch_execute("SET statement_timeout = '120s'; SET lock_timeout = '5s'")
            .await?;
        pgokf_pgconn::set_tenant(&catalog.client, tenant).await?;
        // Match catalog::sync::advisory_lock_key, including its cross-tenant key.
        let key = bundle_lock_key(bundle_name);
        let locked: bool = catalog
            .client
            .query_one("SELECT pg_catalog.pg_try_advisory_lock($1)", &[&key])
            .await?
            .get(0);
        ensure!(
            locked,
            "another writer holds this bundle's catalog lock; retry later"
        );
        Ok(catalog)
    }

    /// Endpoint identity excluding passwords so credential rotation is harmless.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Check ownership and atomically replace the complete bundle, then verify hashes.
    ///
    /// # Errors
    /// Refuses foreign or changed bundles, unapproved deletions, or partial projections.
    /// A failed transaction does not change the catalog.
    pub async fn apply(
        &mut self,
        next: &mut Snapshot,
        previous: Option<&Snapshot>,
        docs: &[Document],
        allow_deletions: bool,
    ) -> Result<ImportReport> {
        ensure!(
            next.tenant == self.tenant && next.bundle_name == self.bundle_name,
            "catalog destination does not match snapshot"
        );
        ensure!(
            next.database_fingerprint.as_deref() == Some(self.fingerprint()),
            "snapshot database binding mismatch"
        );
        if let Some(previous) = previous {
            next.check_previous(previous)?;
        }
        ensure!(
            allow_deletions || next.removed_ids(previous).is_empty(),
            "pages disappeared from the visible space; review dry-run removed_page_ids and use --allow-deletions only after review"
        );
        let expected = next.hashes();
        let submitted: BTreeMap<_, _> = docs
            .iter()
            .map(|doc| {
                (
                    doc.path.clone(),
                    blake3::hash(&doc.bytes).to_hex().to_string(),
                )
            })
            .collect();
        ensure!(
            !expected.is_empty() && docs.len() == expected.len() && submitted == expected,
            "submitted documents do not match complete snapshot"
        );
        let transaction = self
            .client
            .transaction()
            .await
            .context("beginning catalog transaction")?;
        let current = read_catalog(&transaction, &next.bundle_name, &next.tenant).await?;
        check_catalog(next, previous, current.as_ref())?;
        let paths: Vec<_> = docs.iter().map(|doc| doc.path.clone()).collect();
        let contents: Vec<_> = docs.iter().map(|doc| doc.bytes.clone()).collect();
        let binding = Binding {
            schema_version: 1,
            source: next.source.clone(),
        };
        let options = serde_json::json!({ "pgokf_confluence": binding }).to_string();
        let context = sync_context(next).to_string();
        let row = transaction
            .query_one(
                "SELECT bundle_id, added, updated, removed, unchanged, total \
                 FROM pgokf.register_bundle_content_with_context(\
                 $1, $2, $3, $4::text::jsonb, $5::text::jsonb) AS r",
                &[&next.bundle_name, &paths, &contents, &options, &context],
            )
            .await
            .context("registering complete Confluence bundle")?;
        let report = ImportReport {
            bundle_id: row.get(0),
            added: row.get(1),
            updated: row.get(2),
            removed: row.get(3),
            unchanged: row.get(4),
            total: row.get(5),
        };
        let after = read_catalog(&transaction, &next.bundle_name, &next.tenant)
            .await?
            .context("bundle missing after ingestion")?;
        ensure!(
            after.bundle_id == report.bundle_id && after.hashes == expected,
            "catalog did not index the complete snapshot; rolling back"
        );
        ensure!(
            after.binding == Some(binding),
            "catalog source binding changed; rolling back"
        );
        transaction
            .commit()
            .await
            .context("committing catalog import; rerun if the commit outcome is uncertain")?;
        next.bundle_id = Some(report.bundle_id);
        Ok(report)
    }
}

/// The provenance the catalog-change event of an import carries: who wrote
/// it, which space it mirrors, the exact document set, and the Confluence page
/// revisions it materializes - all opaque labels to the catalog.
fn sync_context(snapshot: &Snapshot) -> serde_json::Value {
    serde_json::json!({
        "producer": PRODUCER,
        "origin": snapshot.source.origin_label(),
        "manifest_hash": snapshot.manifest_hash(),
        "observed_source_generation": snapshot.source_generation(),
    })
}

async fn read_catalog(
    client: &impl GenericClient,
    name: &str,
    tenant: &str,
) -> Result<Option<CatalogState>> {
    let key = format!("content:{name}");
    let Some(row) = client.query_opt(
        "SELECT id, (options->'pgokf_confluence')::text, enabled, retired_at IS NOT NULL FROM pgokf.bundles WHERE path = $1 AND tenant_id = $2",
        &[&key, &tenant],
    ).await? else { return Ok(None); };
    let bundle_id: i64 = row.get(0);
    let raw_binding: Option<String> = row.get(1);
    let enabled: bool = row.get(2);
    let retired: bool = row.get(3);
    ensure!(
        enabled && !retired,
        "destination bundle is disabled or retired"
    );
    let binding = raw_binding
        .map(|value| serde_json::from_str(&value).context("invalid Confluence bundle binding"))
        .transpose()?;
    let rows = client
        .query(
            "SELECT path, file_hash FROM pgokf.concepts WHERE bundle_id = $1 AND tenant_id = $2",
            &[&bundle_id, &tenant],
        )
        .await?;
    let hashes = rows
        .into_iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    Ok(Some(CatalogState {
        bundle_id,
        binding,
        hashes,
    }))
}

fn check_catalog(
    next: &Snapshot,
    previous: Option<&Snapshot>,
    current: Option<&CatalogState>,
) -> Result<()> {
    let Some(current) = current else {
        ensure!(
            previous.is_none(),
            "checkpoint exists but catalog bundle is missing; refusing to recreate it automatically"
        );
        return Ok(());
    };
    let expected_binding = Binding {
        schema_version: 1,
        source: next.source.clone(),
    };
    ensure!(
        current.binding.as_ref() == Some(&expected_binding),
        "destination bundle is not owned by this Confluence source; choose a new bundle name"
    );
    if let Some(previous) = previous {
        ensure!(
            previous.bundle_id == Some(current.bundle_id),
            "catalog bundle ID differs from checkpoint"
        );
        ensure!(
            current.hashes == previous.hashes() || current.hashes == next.hashes(),
            "catalog contents changed outside this checkpoint; refusing to overwrite them"
        );
    } else {
        // Crash recovery for the first commit: adopt only an identical manifest with
        // our durable source binding, never an arbitrary pre-existing content bundle.
        ensure!(
            current.hashes == next.hashes(),
            "existing bundle has no matching checkpoint; restore the checkpoint or choose a new bundle name"
        );
    }
    Ok(())
}

fn bundle_lock_key(name: &str) -> i64 {
    let digest = blake3::hash(format!("pgokf.bundle:content:{name}").as_bytes());
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest.as_bytes()[..8]);
    i64::from_be_bytes(first)
}

fn database_fingerprint(url: &str) -> Result<String> {
    let config: tokio_postgres::Config = url
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL connection configuration"))?;
    let identity = format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}",
        config.get_hosts(),
        config.get_hostaddrs(),
        config.get_ports(),
        config.get_dbname(),
        config.get_user()
    );
    Ok(blake3::hash(identity.as_bytes()).to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Entry;

    fn snapshot() -> Snapshot {
        Snapshot {
            schema_version: 1,
            source: SourceIdentity {
                site_url: "https://test.atlassian.net".into(),
                cloud_id: None,
                space_id: "1".into(),
            },
            bundle_name: "wiki".into(),
            tenant: "default".into(),
            database_fingerprint: None,
            bundle_id: Some(7),
            documents: BTreeMap::from([(
                "pages/1.md".into(),
                Entry {
                    page_id: "1".into(),
                    version: 1,
                    hash: "a".repeat(64),
                },
            )]),
        }
    }

    fn catalog(state: &Snapshot) -> CatalogState {
        CatalogState {
            bundle_id: 7,
            binding: Some(Binding {
                schema_version: 1,
                source: state.source.clone(),
            }),
            hashes: state.hashes(),
        }
    }

    #[test]
    fn rejects_foreign_missing_and_out_of_band_bundles() {
        let state = snapshot();
        let mut actual = catalog(&state);
        assert!(check_catalog(&state, Some(&state), None).is_err());
        actual.binding = None;
        assert!(check_catalog(&state, None, Some(&actual)).is_err());
        actual = catalog(&state);
        actual.hashes.insert("foreign.md".into(), "0".repeat(64));
        assert!(check_catalog(&state, Some(&state), Some(&actual)).is_err());
    }

    #[test]
    fn commit_checkpoint_crash_recovers_only_an_identical_projection() {
        let state = snapshot();
        let actual = catalog(&state);
        assert!(check_catalog(&state, None, Some(&actual)).is_ok());
        let mut old = state.clone();
        old.documents.get_mut("pages/1.md").unwrap().hash = "b".repeat(64);
        assert!(check_catalog(&state, Some(&old), Some(&actual)).is_ok());
        assert!(check_catalog(&old, None, Some(&actual)).is_err());
    }

    #[test]
    fn sync_context_labels_the_producer_source_and_revisions() {
        // Arrange
        let state = snapshot();

        // Act
        let context = sync_context(&state);

        // Assert: exactly the documented provenance fields, no operation
        // override (a complete resync is a plain refresh_bundle event).
        assert_eq!(context["producer"], PRODUCER);
        assert_eq!(
            context["origin"],
            "confluence:https://test.atlassian.net:space:1"
        );
        assert_eq!(context["manifest_hash"], state.manifest_hash());
        assert_eq!(
            context["observed_source_generation"],
            state.source_generation()
        );
        assert_eq!(context.as_object().map(serde_json::Map::len), Some(4));
    }

    #[test]
    fn database_password_rotation_preserves_identity() {
        let one = database_fingerprint("postgresql://writer:first@localhost/wiki").unwrap();
        let two = database_fingerprint("postgresql://writer:second@localhost/wiki").unwrap();
        assert_eq!(one, two);
        assert_ne!(
            one,
            database_fingerprint("postgresql://writer:first@localhost/another").unwrap()
        );
    }

    #[test]
    fn database_hostaddr_is_part_of_endpoint_identity() {
        let one =
            database_fingerprint("host=shared-name hostaddr=10.0.0.1 dbname=wiki user=writer")
                .unwrap();
        let two =
            database_fingerprint("host=shared-name hostaddr=10.0.0.2 dbname=wiki user=writer")
                .unwrap();
        assert_ne!(one, two);
    }

    #[test]
    fn native_lock_key_matches_pgokf_hex_algorithm() {
        let hash = blake3::hash(b"pgokf.bundle:content:wiki")
            .to_hex()
            .to_string();
        let leading = u64::from_str_radix(&hash[..16], 16).unwrap();
        assert_eq!(
            bundle_lock_key("wiki"),
            i64::from_le_bytes(leading.to_le_bytes())
        );
    }
}
