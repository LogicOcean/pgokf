// SPDX-License-Identifier: AGPL-3.0-only
//! Durable catalog-change outbox: `pgokf.catalog_change_event` (capability B).
//!
//! # What this records
//!
//! Every catalog mutation - a register/refresh/content sync
//! ([`crate::catalog::sync::run_bundle_sync`]) and every bundle state mutation
//! (enable/disable/retire/unretire/unregister/purge in
//! [`crate::catalog::admin`]) - appends exactly one row to
//! `pgokf.catalog_change_event` **inside the mutation's own transaction**, after
//! the catalog write and before dependency evaluation
//! ([`crate::catalog::freshness`]). A committed catalog change therefore always
//! has its event, and a rolled-back change has none: the outbox is atomic with
//! the catalog by construction. The pre-existing `LISTEN`/`NOTIFY`
//! announcement remains as a non-durable wake-up hint only; its payload points
//! at the event id.
//!
//! # Delivery model
//!
//! Delivery is at-least-once: a dispatcher claims pending (or expired-claim)
//! events with [`claim_catalog_change_events`](pgokf::claim_catalog_change_events)
//! (`FOR UPDATE SKIP LOCKED`, bounded lease, attempt counter) and acknowledges
//! with [`ack_catalog_change_event`](pgokf::ack_catalog_change_event) only after
//! the subscribed producer durably accepted the event. Acknowledgment is
//! idempotent and producer-bound: the same producer label that claimed must
//! ack, and a repeated ack with the same `acceptance_key` is a no-op. An
//! unacknowledged event stays retryable and is **never** pruned; acknowledged
//! events are pruned after the `change_event_retention_days` policy (default
//! 30 days) by the prune hooked into the sync tail, alongside the existing
//! sync-log/history prunes.
//!
//! # Bounded payloads
//!
//! `changes` is a bounded `jsonb` summary - at most [`CHANGE_RECORD_CAP`]
//! records per bucket (`added`/`updated`/`removed`), each carrying the concept
//! id, path, and before/after `file_hash`, plus exact per-bucket `counts`, a
//! `truncated` flag, and the sync's aggregate hash. A refresh of a very large
//! bundle therefore never produces an unbounded payload; when the flag is set,
//! scope-narrowed dependency selectors cannot be proved against the event and
//! the catalog falls back to the unknown-scope rule (see
//! [`crate::catalog::freshness`]).
//!
//! # Identity snapshots and deletion
//!
//! The row snapshots the bundle's identity (`bundle_path`, `bundle_name`,
//! `tenant_id`) at write time; `bundle_id` is a *live* reference that is set
//! to `NULL` when the bundle is unregistered or purged (`ON DELETE SET NULL`),
//! so a hard deletion can never cascade away unacknowledged or audit-relevant
//! events.
//!
//! # Producer identity
//!
//! The `producer` columns and arguments are **caller-supplied opaque labels,
//! not authorization**. Authorization is `session_user` membership in the
//! writer/admin tiers (see [`crate::security`]) or, for claim/ack, the
//! dedicated `pgokf_dispatcher` role. The extension point for a future
//! producer-principal registry is the `created_by`/`producer` column pair: a
//! later release can bind labels to principals without changing the wire
//! shape.
//!
//! # Security/grants
//!
//! The raw table is producer-delivery data: `SELECT` is granted to **no** API
//! role (not even `pgokf_reader`). Claim/ack run `SECURITY DEFINER` granted to
//! `pgokf_dispatcher` only (writers do **not** get delivery rights); admin
//! inspection goes through `pgokf.list_catalog_change_events`. The standard
//! non-forced tenant row-level-security policy is still enabled on the table
//! as defense in depth, and the `SECURITY DEFINER` functions apply the same
//! opt-in tenant filter explicitly.

use std::collections::BTreeMap;
use std::path::Path;

use pgrx::datum::TimestampWithTimeZone;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::spi::SpiHeapTupleData;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::catalog::spi_read::RowReader;
use crate::catalog::types::{StagedConcept, count_to_i32};
use crate::errors::CatalogError;
use crate::security;

/// The operations a `pgokf.catalog_change_event.operation` may carry. Sync
/// operations (`register`/`refresh`/`content`) all record `refresh_bundle`
/// unless the producer's sync context names a more precise content operation
/// (`put_document` / `delete_document` / `concept_change`); the state
/// operations are recorded by the admin mutation sites.
const OPERATIONS: [&str; 10] = [
    "refresh_bundle",
    "put_document",
    "delete_document",
    "enable_bundle",
    "disable_bundle",
    "retire_bundle",
    "unretire_bundle",
    "unregister_bundle",
    "purge_bundle",
    "concept_change",
];

/// The operations a producer sync context may override a sync's operation to:
/// content-addressed writes only (state operations are never caller-settable,
/// they are recorded by the admin mutation sites themselves).
const CONTEXT_OPERATIONS: [&str; 4] = [
    "refresh_bundle",
    "put_document",
    "delete_document",
    "concept_change",
];

/// Maximum number of change records stored per bucket (`added`/`updated`/
/// `removed`) in one event's `changes` payload. Exact per-bucket counts are
/// always stored alongside, and `truncated` marks a capped bucket, so the
/// payload stays bounded for a refresh of any size.
pub(crate) const CHANGE_RECORD_CAP: usize = 256;

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// One concept-level change carried by a catalog-change event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConceptChange {
    /// The concept's path-derived OKF id.
    pub id: String,
    /// Normalized bundle-relative path.
    pub path: String,
    /// `added`, `updated`, or `removed` (the `sync_log_change` vocabulary).
    pub kind: &'static str,
    /// The stored `file_hash` before the change (`updated`/`removed`).
    pub before_hash: Option<String>,
    /// The `file_hash` after the change (`added`/`updated`).
    pub after_hash: Option<String>,
}

/// Build the complete (unbounded) change list of one sync from the data the
/// sync engine already computed.
///
/// `stored_hashes`/`stored_ids` are the *pre-sync* projection (`path ->
/// file_hash` / `path -> concept id`): a staged concept at a previously stored
/// path under the same id is `updated`, at a previously unseen path `added`,
/// and at a stored path under a *different* id (a reclassification) both an
/// `added` under the new id and a `removed` under the old one - mirroring the
/// change-manifest classification in [`crate::catalog::sync`].
pub(crate) fn build_changes(
    stored_hashes: &BTreeMap<String, String>,
    stored_ids: &BTreeMap<String, String>,
    staged: &[StagedConcept],
    removed_paths: &[String],
) -> Vec<ConceptChange> {
    let mut changes = Vec::with_capacity(staged.len() + removed_paths.len());
    for entry in staged {
        let path = &entry.concept.path;
        let before = stored_hashes.get(path);
        let old_id = stored_ids.get(path);
        let reclassified = old_id.is_some_and(|old| *old != entry.concept.id);
        if reclassified {
            changes.push(ConceptChange {
                id: old_id.expect("reclassified implies a stored id").clone(),
                path: path.clone(),
                kind: "removed",
                before_hash: before.cloned(),
                after_hash: None,
            });
        }
        changes.push(ConceptChange {
            id: entry.concept.id.clone(),
            path: path.clone(),
            kind: if before.is_some() && !reclassified {
                "updated"
            } else {
                "added"
            },
            before_hash: if reclassified { None } else { before.cloned() },
            after_hash: Some(entry.file_hash.clone()),
        });
    }
    for path in removed_paths {
        changes.push(ConceptChange {
            id: stored_ids
                .get(path)
                .cloned()
                .unwrap_or_else(|| path.clone()),
            path: path.clone(),
            kind: "removed",
            before_hash: stored_hashes.get(path).cloned(),
            after_hash: None,
        });
    }
    changes
}

/// Whether the change list exceeds the per-bucket payload cap
/// ([`CHANGE_RECORD_CAP`]), in which case a narrowed dependency selector
/// cannot be proved against the event (the unknown-scope rule applies).
pub(crate) fn is_truncated(changes: &[ConceptChange]) -> bool {
    let mut counts = (0_usize, 0_usize, 0_usize);
    for change in changes {
        match change.kind {
            "added" => counts.0 += 1,
            "updated" => counts.1 += 1,
            _ => counts.2 += 1,
        }
    }
    counts.0 > CHANGE_RECORD_CAP || counts.1 > CHANGE_RECORD_CAP || counts.2 > CHANGE_RECORD_CAP
}

/// The producer-supplied provenance context attached to a catalog change.
///
/// Populated from the `pgokf.sync_context` GUC (a JSON object a producer sets
/// before invoking a sync) or from the explicit `context` argument of
/// `pgokf.register_bundle_content_with_context`. Every field is an opaque
/// label: the catalog stores and replays it verbatim and never interprets it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ChangeContext {
    /// What originated the change (an opaque system/pipeline label).
    pub origin: Option<String>,
    /// Causation key: a refresh caused by reconciliation `K` must not dispatch
    /// new source events for dependencies registered with the same key.
    pub causation_key: Option<String>,
    /// The producer's reconciliation identity for the attempt, when any.
    pub reconciliation_key: Option<String>,
    /// The producer's opaque label (not authorization - see module docs).
    pub producer: Option<String>,
    /// Hash of the publication manifest the change was produced from, when any.
    pub manifest_hash: Option<String>,
    /// The producer's opaque source revision the change materializes, when any.
    pub observed_source_generation: Option<String>,
    /// Operation override for the content path (`put_document` etc.).
    pub operation: Option<String>,
}

impl ChangeContext {
    /// Parse the raw JSON text of a sync context.
    ///
    /// Extraction runs in SQL (one bound `$1::jsonb` read), so no JSON parser
    /// dependency is needed and an invalid or non-object value is rejected
    /// with SQLSTATE `22023` rather than a raw cast error.
    ///
    /// # Errors
    ///
    /// Returns an [`crate::errors::ErrorKind::InvalidParameter`] error when the
    /// text is not a JSON object or names an unsupported `operation` override.
    pub(crate) fn parse(raw: &str) -> Result<Self, CatalogError> {
        if raw.trim().is_empty() {
            return Ok(Self::default());
        }
        let parsed = Spi::connect(|client| {
            let row = client
                .select(
                    "SELECT j ->> 'origin', j ->> 'causation_key', j ->> 'reconciliation_key',
                            j ->> 'producer', j ->> 'manifest_hash',
                            j ->> 'observed_source_generation', j ->> 'operation'
                     FROM (SELECT $1::pg_catalog.jsonb AS j) AS s
                     WHERE pg_catalog.jsonb_typeof(s.j) = 'object'",
                    Some(1),
                    &[raw.into()],
                )
                .map_err(|error| {
                    CatalogError::invalid_parameter(
                        format!("sync context must be a JSON object: {error}"),
                        Path::new(""),
                    )
                })?;
            let Some(row) = row.into_iter().next() else {
                return Err(CatalogError::invalid_parameter(
                    "sync context must be a JSON object",
                    Path::new(""),
                ));
            };
            let reader = RowReader::new(&row, "failed to read sync context", "sync context");
            Ok(Self {
                origin: reader.optional(1)?,
                causation_key: reader.optional(2)?,
                reconciliation_key: reader.optional(3)?,
                producer: reader.optional(4)?,
                manifest_hash: reader.optional(5)?,
                observed_source_generation: reader.optional(6)?,
                operation: reader.optional(7)?,
            })
        })?;
        if let Some(operation) = &parsed.operation
            && !CONTEXT_OPERATIONS.contains(&operation.as_str())
        {
            return Err(CatalogError::invalid_parameter(
                format!(
                    "sync context operation must be one of {}, got {operation}",
                    CONTEXT_OPERATIONS.map(|op| format!("'{op}'")).join(", ")
                ),
                Path::new(""),
            ));
        }
        Ok(parsed)
    }

    /// The sync context of the current session (the `pgokf.sync_context` GUC).
    ///
    /// # Errors
    ///
    /// See [`Self::parse`].
    pub(crate) fn from_session() -> Result<Self, CatalogError> {
        Self::parse(&crate::guc::sync_context())
    }

    /// The operation the event records for a sync: the validated context
    /// override when present, else `refresh_bundle` for every sync operation.
    pub(crate) fn operation_for_sync(&self) -> &'static str {
        // The override was validated against CONTEXT_OPERATIONS at parse time;
        // map it back to the &'static str constant.
        match self.operation.as_deref() {
            Some("put_document") => "put_document",
            Some("delete_document") => "delete_document",
            Some("concept_change") => "concept_change",
            _ => "refresh_bundle",
        }
    }
}

/// One bounded change bucket (`added`/`updated`/`removed`) of an event
/// payload: the capped record columns plus the exact total count.
#[derive(Default)]
struct ChangeBucket {
    ids: Vec<String>,
    paths: Vec<String>,
    before_hashes: Vec<Option<String>>,
    after_hashes: Vec<Option<String>>,
    total: usize,
}

impl ChangeBucket {
    /// Record one change, storing its columns only while the bucket is under
    /// the [`CHANGE_RECORD_CAP`] (the exact `total` always advances).
    fn push(&mut self, change: &ConceptChange) {
        self.total += 1;
        if self.ids.len() < CHANGE_RECORD_CAP {
            self.ids.push(change.id.clone());
            self.paths.push(change.path.clone());
            self.before_hashes.push(change.before_hash.clone());
            self.after_hashes.push(change.after_hash.clone());
        }
    }

    /// Whether the cap cut records off this bucket.
    fn is_truncated(&self) -> bool {
        self.total > self.ids.len()
    }
}

/// Append one outbox event for a committed catalog change, returning its id.
///
/// `changes` is the complete change list; the stored payload is bounded to
/// [`CHANGE_RECORD_CAP`] records per bucket (with exact counts and a
/// `truncated` flag), so a very large refresh never builds an unbounded row.
/// The bundle's identity (path, name, tenant) is snapshotted from its live
/// `pgokf.bundles` row, which must still exist - callers insert the event
/// *before* deleting the bundle (unregister/purge), and the row's
/// `ON DELETE SET NULL` reference then detaches without cascading.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding
/// transaction so the event commits atomically with the catalog change.
pub(crate) fn record(
    bundle_id: i64,
    operation: &str,
    catalog_generation: i64,
    changes: &[ConceptChange],
    context: &ChangeContext,
    sync_hash: Option<&str>,
) -> Result<i64, CatalogError> {
    const INSERT: &str = "
        INSERT INTO pgokf.catalog_change_event
            (tenant_id, bundle_id, bundle_path, bundle_name, catalog_generation,
             operation, changes, origin, causation_key, reconciliation_key, producer)
        SELECT b.tenant_id, b.id, b.path, b.name, $2, $3,
            pg_catalog.jsonb_build_object(
                'added', COALESCE((
                    SELECT pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(
                        'id', a.id, 'path', a.path, 'after_hash', a.after_hash))
                    FROM unnest($4::text[], $5::text[], $6::text[])
                         AS a(id, path, after_hash)), '[]'::pg_catalog.jsonb),
                'updated', COALESCE((
                    SELECT pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(
                        'id', u.id, 'path', u.path,
                        'before_hash', u.before_hash, 'after_hash', u.after_hash))
                    FROM unnest($7::text[], $8::text[], $9::text[], $10::text[])
                         AS u(id, path, before_hash, after_hash)), '[]'::pg_catalog.jsonb),
                'removed', COALESCE((
                    SELECT pg_catalog.jsonb_agg(pg_catalog.jsonb_build_object(
                        'id', r.id, 'path', r.path, 'before_hash', r.before_hash))
                    FROM unnest($11::text[], $12::text[], $13::text[])
                         AS r(id, path, before_hash)), '[]'::pg_catalog.jsonb),
                'counts', pg_catalog.jsonb_build_object(
                    'added', $14::integer, 'updated', $15::integer, 'removed', $16::integer),
                'truncated', $17::boolean,
                'sync_hash', $18::text),
            $19, $20, $21, $22
        FROM pgokf.bundles b
        WHERE b.id = $1
        RETURNING event_id";

    debug_assert!(OPERATIONS.contains(&operation));
    let mut added = ChangeBucket::default();
    let mut updated = ChangeBucket::default();
    let mut removed = ChangeBucket::default();
    for change in changes {
        match change.kind {
            "added" => added.push(change),
            "updated" => updated.push(change),
            _ => removed.push(change),
        }
    }
    let truncated = added.is_truncated() || updated.is_truncated() || removed.is_truncated();

    Spi::get_one_with_args::<i64>(
        INSERT,
        &[
            bundle_id.into(),
            catalog_generation.into(),
            operation.into(),
            added.ids.into(),
            added.paths.into(),
            added.after_hashes.into(),
            updated.ids.into(),
            updated.paths.into(),
            updated.before_hashes.into(),
            updated.after_hashes.into(),
            removed.ids.into(),
            removed.paths.into(),
            removed.before_hashes.into(),
            count_to_i32(added.total).into(),
            count_to_i32(updated.total).into(),
            count_to_i32(removed.total).into(),
            truncated.into(),
            sync_hash.into(),
            context.origin.as_deref().into(),
            context.causation_key.as_deref().into(),
            context.reconciliation_key.as_deref().into(),
            context.producer.as_deref().into(),
        ],
    )
    .map_err(|error| spi_error("failed to append catalog change event", &error))?
    .ok_or_else(|| CatalogError::internal("change-event insert returned no id", Path::new("")))
}

/// Prune acknowledged events older than the retention window.
///
/// Only `acknowledged` rows whose `acknowledged_at` predates
/// `now() - retention_days` are removed; pending and claimed-but-
/// unacknowledged events are never pruned (delivery is at-least-once). A
/// retention of `0` keeps acknowledged events indefinitely. Runs inside the
/// sync transaction at its tail, alongside the existing sync-log/history
/// prunes.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure.
pub(crate) fn prune(retention_days: i32) -> Result<(), CatalogError> {
    if retention_days > 0 {
        Spi::run_with_args(
            "DELETE FROM pgokf.catalog_change_event
             WHERE status = 'acknowledged'
               AND acknowledged_at < pg_catalog.now() - pg_catalog.make_interval(days => $1)",
            &[retention_days.into()],
        )
        .map_err(|error| spi_error("failed to prune acknowledged change events", &error))?;
    }
    Ok(())
}

/// The opt-in tenant predicate the `SECURITY DEFINER` claim/ack/inspection
/// functions apply explicitly (they run as the table owner and so bypass the
/// row-level-security policy, which is still enabled as defense in depth).
/// Identical, term for term, to the table's policy expression.
const TENANT_PREDICATE: &str = "(((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
         OR pg_catalog.current_setting('pgokf.tenant', true) = '')
        AND NOT (SELECT pgokf.tenant_required()))
       OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))";

/// Column projection shared by the claim and inspection reads, in the
/// attribute order of [`read_event`].
const EVENT_COLUMNS: &str = "event_id, tenant_id, bundle_id, bundle_path, bundle_name,
    catalog_generation, operation, changes, origin, causation_key, reconciliation_key,
    created_at, status, claimed_by, claim_expires_at, attempts, last_error, acknowledged_at";

/// The same projection qualified with the `e` table alias, for the claim
/// statement's `UPDATE ... FROM picked ... RETURNING` (where a bare
/// `event_id` would be ambiguous against the CTE).
const EVENT_COLUMNS_QUALIFIED: &str = "e.event_id, e.tenant_id, e.bundle_id, e.bundle_path,
    e.bundle_name, e.catalog_generation, e.operation, e.changes, e.origin,
    e.causation_key, e.reconciliation_key, e.created_at, e.status, e.claimed_by,
    e.claim_expires_at, e.attempts, e.last_error, e.acknowledged_at";

/// One `pgokf.catalog_change_event` row.
struct ChangeEvent {
    event_id: i64,
    tenant_id: String,
    bundle_id: Option<i64>,
    bundle_path: String,
    bundle_name: Option<String>,
    catalog_generation: i64,
    operation: String,
    changes: pgrx::JsonB,
    origin: Option<String>,
    causation_key: Option<String>,
    reconciliation_key: Option<String>,
    created_at: TimestampWithTimeZone,
    status: String,
    claimed_by: Option<String>,
    claim_expires_at: Option<TimestampWithTimeZone>,
    attempts: i32,
    last_error: Option<String>,
    acknowledged_at: Option<TimestampWithTimeZone>,
}

fn read_event(row: &SpiHeapTupleData<'_>) -> Result<ChangeEvent, CatalogError> {
    let reader = RowReader::new(
        row,
        "failed to read catalog_change_event column",
        "change event",
    );
    Ok(ChangeEvent {
        event_id: reader.required(1, "event_id")?,
        tenant_id: reader.required(2, "tenant_id")?,
        bundle_id: reader.optional(3)?,
        bundle_path: reader.required(4, "bundle_path")?,
        bundle_name: reader.optional(5)?,
        catalog_generation: reader.required(6, "catalog_generation")?,
        operation: reader.required(7, "operation")?,
        changes: reader.required(8, "changes")?,
        origin: reader.optional(9)?,
        causation_key: reader.optional(10)?,
        reconciliation_key: reader.optional(11)?,
        created_at: reader.required(12, "created_at")?,
        status: reader.required(13, "status")?,
        claimed_by: reader.optional(14)?,
        claim_expires_at: reader.optional(15)?,
        attempts: reader.required(16, "attempts")?,
        last_error: reader.optional(17)?,
        acknowledged_at: reader.optional(18)?,
    })
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build change-event composite: {error}"),
        Path::new(""),
    )
}

/// Pack a claimed event into a `pgokf.claimed_change_event` heap tuple.
fn claimed_tuple(
    event: &ChangeEvent,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type("pgokf.claimed_change_event").map_err(composite_error)?;
    tuple
        .set_by_name("event_id", event.event_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("tenant_id", event.tenant_id.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_id", event.bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_path", event.bundle_path.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_name", event.bundle_name.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("catalog_generation", event.catalog_generation)
        .map_err(composite_error)?;
    tuple
        .set_by_name("operation", event.operation.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("changes", pgrx::JsonB(event.changes.0.clone()))
        .map_err(composite_error)?;
    tuple
        .set_by_name("origin", event.origin.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("causation_key", event.causation_key.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("reconciliation_key", event.reconciliation_key.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("created_at", event.created_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("attempts", event.attempts)
        .map_err(composite_error)?;
    tuple
        .set_by_name("claim_expires_at", event.claim_expires_at)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// Pack an event into a `pgokf.catalog_change_event_info` heap tuple.
fn info_tuple(event: &ChangeEvent) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type("pgokf.catalog_change_event_info")
        .map_err(composite_error)?;
    tuple
        .set_by_name("event_id", event.event_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("tenant_id", event.tenant_id.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_id", event.bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_path", event.bundle_path.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_name", event.bundle_name.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("catalog_generation", event.catalog_generation)
        .map_err(composite_error)?;
    tuple
        .set_by_name("operation", event.operation.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("status", event.status.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("claimed_by", event.claimed_by.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("claim_expires_at", event.claim_expires_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("attempts", event.attempts)
        .map_err(composite_error)?;
    tuple
        .set_by_name("last_error", event.last_error.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("created_at", event.created_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("acknowledged_at", event.acknowledged_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("origin", event.origin.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("causation_key", event.causation_key.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("reconciliation_key", event.reconciliation_key.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("changes", pgrx::JsonB(event.changes.0.clone()))
        .map_err(composite_error)?;
    Ok(tuple)
}

/// Validate a claim/inspection bound: `1..=1000`, SQLSTATE `22023` otherwise.
fn validate_limit(limit: i32) -> Result<i64, CatalogError> {
    if (1..=1000).contains(&limit) {
        Ok(i64::from(limit))
    } else {
        Err(CatalogError::invalid_parameter(
            format!("limit must be between 1 and 1000, got {limit}"),
            Path::new(""),
        ))
    }
}

/// Validate the producer label and claim lease.
fn validate_claim_args(producer: &str, lease_seconds: i32) -> Result<(), CatalogError> {
    if producer.trim().is_empty() {
        return Err(CatalogError::invalid_parameter(
            "producer must not be empty",
            Path::new(""),
        ));
    }
    if !(1..=86_400).contains(&lease_seconds) {
        return Err(CatalogError::invalid_parameter(
            format!("lease_seconds must be between 1 and 86400, got {lease_seconds}"),
            Path::new(""),
        ));
    }
    Ok(())
}

/// Claim up to `limit` pending (or expired-claim) events for `producer`.
///
/// Rows are picked oldest-first with `FOR UPDATE SKIP LOCKED` (so concurrent
/// dispatchers never claim the same event), stamped with the claim owner, the
/// lease expiry, and an incremented attempt counter, and returned with their
/// full payload. Scoped to the session's tenant by the same opt-in predicate
/// the table policy uses.
fn claim_impl(
    producer: &str,
    limit: i32,
    lease_seconds: i32,
) -> Result<Vec<ChangeEvent>, CatalogError> {
    security::authorize_dispatcher_current_user()?;
    let limit = validate_limit(limit)?;
    validate_claim_args(producer, lease_seconds)?;
    let statement = format!(
        "WITH picked AS (
             SELECT event_id
             FROM pgokf.catalog_change_event
             WHERE (status = 'pending'
                    OR (status = 'claimed' AND claim_expires_at < pg_catalog.now()))
               AND {TENANT_PREDICATE}
             ORDER BY event_id
             LIMIT $2
             FOR UPDATE SKIP LOCKED
         )
         UPDATE pgokf.catalog_change_event e
         SET status = 'claimed',
             claimed_by = $1,
             claim_expires_at = pg_catalog.now() + pg_catalog.make_interval(secs => $3),
             attempts = e.attempts + 1
         FROM picked
         WHERE e.event_id = picked.event_id
         RETURNING {EVENT_COLUMNS_QUALIFIED}"
    );
    Spi::connect_mut(|client| {
        let table = client
            .update(
                statement.as_str(),
                None,
                &[producer.into(), limit.into(), lease_seconds.into()],
            )
            .map_err(|error| spi_error("failed to claim catalog change events", &error))?;
        let mut events = Vec::with_capacity(table.len());
        for row in table {
            events.push(read_event(&row)?);
        }
        Ok(events)
    })
}

/// Acknowledge a claimed event: idempotent and producer-bound.
///
/// Returns `true` when the event is acknowledged after the call. A first ack
/// requires the event to be currently claimed by the same `producer` label;
/// a repeated ack with the same `acceptance_key` is a no-op returning `true`,
/// while a repeated ack with a *different* key is a conflict (SQLSTATE
/// `22023`). Acking an event claimed by another producer is denied (SQLSTATE
/// `42501`); acking an unknown or unclaimed event is a caller error
/// (`22023`). The tenant filter applies exactly as in claiming.
fn ack_impl(event_id: i64, producer: &str, acceptance_key: &str) -> Result<bool, CatalogError> {
    security::authorize_dispatcher_current_user()?;
    if producer.trim().is_empty() || acceptance_key.trim().is_empty() {
        return Err(CatalogError::invalid_parameter(
            "producer and acceptance_key must not be empty",
            Path::new(""),
        ));
    }
    // connect_mut + next(): a non-matching row updates zero rows, and
    // Spi::get_one errors on an empty result instead of returning None.
    let statement = format!(
        "UPDATE pgokf.catalog_change_event
         SET status = 'acknowledged',
             acceptance_key = $3,
             acknowledged_at = pg_catalog.now()
         WHERE event_id = $1
           AND status = 'claimed'
           AND claimed_by = $2
           AND {TENANT_PREDICATE}
         RETURNING event_id"
    );
    let acknowledged = Spi::connect_mut(|client| {
        let mut table = client
            .update(
                statement.as_str(),
                None,
                &[event_id.into(), producer.into(), acceptance_key.into()],
            )
            .map_err(|error| spi_error("failed to acknowledge catalog change event", &error))?;
        Ok(table.next().is_some())
    })?;
    if acknowledged {
        return Ok(true);
    }

    // The fast path did not match: inspect the row to distinguish an idempotent
    // retry from a conflict, a foreign claim, and an unknown/unclaimed event.
    let current = Spi::connect(|client| {
        let table = client
            .select(
                format!(
                    "SELECT status, claimed_by, acceptance_key
                     FROM pgokf.catalog_change_event
                     WHERE event_id = $1 AND {TENANT_PREDICATE}"
                )
                .as_str(),
                Some(1),
                &[event_id.into()],
            )
            .map_err(|error| spi_error("failed to read catalog change event", &error))?;
        let Some(row) = table.into_iter().next() else {
            return Ok(None);
        };
        let reader = RowReader::new(&row, "failed to read change event state", "change event");
        Ok(Some((
            reader.required::<String>(1, "status")?,
            reader.optional::<String>(2)?,
            reader.optional::<String>(3)?,
        )))
    })?;
    match current {
        None => Err(CatalogError::invalid_parameter(
            format!("catalog change event {event_id} does not exist"),
            Path::new(""),
        )),
        Some((status, claimed_by, stored_key)) if status == "acknowledged" => {
            if claimed_by.as_deref() == Some(producer)
                && stored_key.as_deref() == Some(acceptance_key)
            {
                Ok(true)
            } else {
                Err(CatalogError::invalid_parameter(
                    format!(
                        "catalog change event {event_id} is already acknowledged with a different \
                         producer or acceptance_key"
                    ),
                    Path::new(""),
                ))
            }
        }
        Some((_, claimed_by, _)) if claimed_by.as_deref() != Some(producer) => {
            Err(CatalogError::insufficient_privilege(
                format!("catalog change event {event_id} is not claimed by producer {producer}"),
                Path::new(""),
            ))
        }
        Some(_) => Err(CatalogError::invalid_parameter(
            format!("catalog change event {event_id} is not currently claimed"),
            Path::new(""),
        )),
    }
}

/// Admin inspection: recent events, newest first, optionally scoped to one
/// bundle, with the same opt-in tenant filter the claim path applies.
fn list_impl(bundle_id: Option<i64>, max_rows: i32) -> Result<Vec<ChangeEvent>, CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    let limit = validate_limit(max_rows)?;
    let query = format!(
        "SELECT {EVENT_COLUMNS}
         FROM pgokf.catalog_change_event
         WHERE ($1::bigint IS NULL OR bundle_id = $1)
           AND {TENANT_PREDICATE}
         ORDER BY event_id DESC
         LIMIT $2"
    );
    Spi::connect(|client| {
        let table = client
            .select(query.as_str(), None, &[bundle_id.into(), limit.into()])
            .map_err(|error| spi_error("failed to list catalog change events", &error))?;
        let mut events = Vec::with_capacity(table.len());
        for row in table {
            events.push(read_event(&row)?);
        }
        Ok(events)
    })
}

extension_sql!(
    r"
CREATE TABLE pgokf.catalog_change_event (
    event_id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id        text NOT NULL DEFAULT 'default',
    bundle_id        bigint REFERENCES pgokf.bundles (id) ON DELETE SET NULL,
    bundle_path      text NOT NULL,
    bundle_name      text,
    catalog_generation bigint NOT NULL,
    operation        text NOT NULL,
    changes          jsonb NOT NULL DEFAULT '{}'::jsonb,
    origin           text,
    causation_key    text,
    reconciliation_key text,
    producer         text,
    created_at       timestamptz NOT NULL DEFAULT now(),
    status           text NOT NULL DEFAULT 'pending',
    claimed_by       text,
    claim_expires_at timestamptz,
    attempts         integer NOT NULL DEFAULT 0,
    last_error       text,
    acceptance_key   text,
    acknowledged_at  timestamptz,
    CONSTRAINT catalog_change_event_operation_chk CHECK (operation IN (
        'refresh_bundle', 'put_document', 'delete_document',
        'enable_bundle', 'disable_bundle', 'retire_bundle', 'unretire_bundle',
        'unregister_bundle', 'purge_bundle', 'concept_change')),
    CONSTRAINT catalog_change_event_status_chk
        CHECK (status IN ('pending', 'claimed', 'acknowledged'))
);

-- Claim order is event_id order; the partial index serves the claimable scan
-- (pending or claimed-and-expired) without indexing the acknowledged history.
CREATE INDEX catalog_change_event_claimable_idx
    ON pgokf.catalog_change_event (event_id) WHERE status <> 'acknowledged';
CREATE INDEX catalog_change_event_tenant_idx ON pgokf.catalog_change_event (tenant_id);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced, so the SECURITY DEFINER sync path bypasses
-- it; the claim/ack/inspection functions apply the identical predicate
-- explicitly. No API role holds SELECT on the raw table (it is producer-
-- delivery data); the policy is defense in depth.
ALTER TABLE pgokf.catalog_change_event ENABLE ROW LEVEL SECURITY;
CREATE POLICY catalog_change_event_tenant_isolation ON pgokf.catalog_change_event
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

REVOKE ALL ON pgokf.catalog_change_event FROM PUBLIC;

COMMENT ON TABLE pgokf.catalog_change_event IS
    'Durable transactional outbox of catalog changes: exactly one row per committed register/refresh/content sync and per bundle state mutation (enable/disable/retire/unretire/unregister/purge), written inside the mutation''s own transaction before dependency evaluation. Delivery is at-least-once via pgokf.claim_catalog_change_events / pgokf.ack_catalog_change_event (granted to pgokf_dispatcher only); unacknowledged events stay retryable and are never pruned, acknowledged ones age out under the change_event_retention_days policy (default 30 days). No API role may SELECT the raw table; admins inspect it through pgokf.list_catalog_change_events.';
COMMENT ON COLUMN pgokf.catalog_change_event.event_id IS
    'Monotonic event identity (GENERATED ALWAYS AS IDENTITY): the delivery and claim order.';
COMMENT ON COLUMN pgokf.catalog_change_event.tenant_id IS
    'Multi-tenant owner, snapshotted from the bundle at event time; the row-level-security policy and the SECURITY DEFINER delivery functions filter on it.';
COMMENT ON COLUMN pgokf.catalog_change_event.bundle_id IS
    'Live reference to the affected bundle, or NULL after the bundle was unregistered/purged (ON DELETE SET NULL): a hard deletion never cascades away unacknowledged or audit-relevant events. Use the snapshotted bundle_path/bundle_name for the durable identity.';
COMMENT ON COLUMN pgokf.catalog_change_event.bundle_path IS
    'Immutable snapshot of the bundle''s canonical path (or content:<name> key) at event time; survives bundle deletion.';
COMMENT ON COLUMN pgokf.catalog_change_event.bundle_name IS
    'Immutable snapshot of the bundle''s display name at event time.';
COMMENT ON COLUMN pgokf.catalog_change_event.catalog_generation IS
    'The bundle''s catalog generation after this change (pgokf.bundles.catalog_generation, incremented once per accepted sync and per state mutation under the bundle advisory lock).';
COMMENT ON COLUMN pgokf.catalog_change_event.operation IS
    'What changed: refresh_bundle (any register/refresh/content resync), put_document / delete_document / concept_change (producer-declared content operations via the sync context), or a bundle state mutation enable_bundle / disable_bundle / retire_bundle / unretire_bundle / unregister_bundle / purge_bundle.';
COMMENT ON COLUMN pgokf.catalog_change_event.changes IS
    'Bounded jsonb change summary: per-bucket arrays added/updated/removed (at most 256 records each) of {id, path, before_hash, after_hash} as applicable, exact per-bucket counts, a truncated flag (true when a bucket was capped - scope selectors then cannot be proved against this event), and the sync''s aggregate sync_hash. Empty for pure state mutations.';
COMMENT ON COLUMN pgokf.catalog_change_event.origin IS
    'Opaque caller-supplied label of what originated the change (from the sync context); never interpreted by the catalog.';
COMMENT ON COLUMN pgokf.catalog_change_event.causation_key IS
    'Opaque caller-supplied causation key: a refresh caused by reconciliation K carries K here, and a freshness dependency registered with the same causation key is NOT triggered by the event - the loop-suppression rule that keeps a producer-generated refresh from recursively triggering itself.';
COMMENT ON COLUMN pgokf.catalog_change_event.reconciliation_key IS
    'Opaque caller-supplied identity of the reconciliation attempt that produced the change, when any.';
COMMENT ON COLUMN pgokf.catalog_change_event.producer IS
    'Opaque caller-supplied producer label. NOT authorization: authorization is session_user membership in the writer/admin tiers; this label only binds claim/ack ownership.';
COMMENT ON COLUMN pgokf.catalog_change_event.created_at IS
    'When the change committed (transaction now()).';
COMMENT ON COLUMN pgokf.catalog_change_event.status IS
    'Delivery state: pending (never claimed or lease expired), claimed (owned by claimed_by until claim_expires_at), or acknowledged (durably accepted by the producer). Unacknowledged events remain retryable and are never pruned.';
COMMENT ON COLUMN pgokf.catalog_change_event.claimed_by IS
    'The producer label holding the current claim; only that label may acknowledge the event.';
COMMENT ON COLUMN pgokf.catalog_change_event.claim_expires_at IS
    'When the current claim lease expires; the event becomes claimable again afterward.';
COMMENT ON COLUMN pgokf.catalog_change_event.attempts IS
    'How many times the event has been claimed (incremented per claim).';
COMMENT ON COLUMN pgokf.catalog_change_event.last_error IS
    'Free-text note of the last failed delivery attempt, when a dispatcher recorded one; informational only.';
COMMENT ON COLUMN pgokf.catalog_change_event.acceptance_key IS
    'The acknowledgment key stored by the first successful ack; a repeated ack must present the same key (idempotent) or it is rejected as a conflict.';
COMMENT ON COLUMN pgokf.catalog_change_event.acknowledged_at IS
    'When the producer durably accepted the event; the retention prune compares against this instant.';
",
    name = "catalog_change_event_table",
    requires = ["catalog_tables"]
);

/// SQL-facing outbox delivery/inspection surface, installed into the `pgokf`
/// schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{ack_impl, claim_impl, claimed_tuple, info_tuple, list_impl};

    extension_sql!(
        r"
CREATE TYPE pgokf.claimed_change_event AS (
    event_id           bigint,
    tenant_id          text,
    bundle_id          bigint,
    bundle_path        text,
    bundle_name        text,
    catalog_generation bigint,
    operation          text,
    changes            jsonb,
    origin             text,
    causation_key      text,
    reconciliation_key text,
    created_at         timestamptz,
    attempts           integer,
    claim_expires_at   timestamptz
);

COMMENT ON TYPE pgokf.claimed_change_event IS
    'One catalog-change event claimed through pgokf.claim_catalog_change_events: full delivery payload plus the attempt counter and this claim''s lease expiry.';

CREATE TYPE pgokf.catalog_change_event_info AS (
    event_id           bigint,
    tenant_id          text,
    bundle_id          bigint,
    bundle_path        text,
    bundle_name        text,
    catalog_generation bigint,
    operation          text,
    status             text,
    claimed_by         text,
    claim_expires_at   timestamptz,
    attempts           integer,
    last_error         text,
    created_at         timestamptz,
    acknowledged_at    timestamptz,
    origin             text,
    causation_key      text,
    reconciliation_key text,
    changes            jsonb
);

COMMENT ON TYPE pgokf.catalog_change_event_info IS
    'One catalog-change outbox row as pgokf.list_catalog_change_events reports it (admin inspection): delivery state, claim ownership, attempts, and the full change payload.';
",
        name = "change_event_types",
        requires = ["catalog_change_event_table"]
    );

    /// Claim pending catalog-change events for a producer.
    ///
    /// Requires membership in `pgokf_dispatcher` (or `pgokf_admin`). Claims up
    /// to `limit` claimable events (pending, or claimed with an expired lease)
    /// oldest-first with `FOR UPDATE SKIP LOCKED`, sets the claim owner and
    /// lease expiry, and increments each event's attempt counter. Scoped to
    /// the session's tenant.
    #[pg_extern(requires = ["change_event_types"])]
    fn claim_catalog_change_events(
        producer: &str,
        limit: default!(i32, 100),
        lease_seconds: default!(i32, 300),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.claimed_change_event")> {
        let events =
            claim_impl(producer, limit, lease_seconds).unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = events
            .iter()
            .map(|event| claimed_tuple(event).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    /// Acknowledge a claimed catalog-change event (idempotent, producer-bound).
    ///
    /// Requires membership in `pgokf_dispatcher` (or `pgokf_admin`). Only the
    /// producer label holding the claim may ack; a repeated ack with the same
    /// `acceptance_key` is a no-op. Returns whether the event is acknowledged.
    #[pg_extern(requires = ["change_event_types"])]
    fn ack_catalog_change_event(event_id: i64, producer: &str, acceptance_key: &str) -> bool {
        ack_impl(event_id, producer, acceptance_key).unwrap_or_else(|error| error.raise())
    }

    /// List recent catalog-change events, newest first (admin inspection).
    ///
    /// Requires membership in `pgokf_admin`. Pass `bundle_id` to scope the
    /// listing to one bundle; `max_rows` bounds the rows returned.
    #[pg_extern(requires = ["change_event_types"])]
    fn list_catalog_change_events(
        bundle_id: default!(Option<i64>, "NULL"),
        max_rows: default!(i32, 100),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.catalog_change_event_info")>
    {
        let events = list_impl(bundle_id, max_rows).unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = events
            .iter()
            .map(|event| info_tuple(event).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.claim_catalog_change_events(text, integer, integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.ack_catalog_change_event(bigint, text, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.list_catalog_change_events(bigint, integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.claim_catalog_change_events(text, integer, integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.ack_catalog_change_event(bigint, text, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.list_catalog_change_events(bigint, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.claim_catalog_change_events(text, integer, integer) TO pgokf_dispatcher;
GRANT EXECUTE ON FUNCTION pgokf.ack_catalog_change_event(bigint, text, text) TO pgokf_dispatcher;
GRANT EXECUTE ON FUNCTION pgokf.list_catalog_change_events(bigint, integer) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.claim_catalog_change_events(text, integer, integer) IS
    'Claim up to limit (default 100) pending or expired-claim catalog-change events for producer, oldest-first, with FOR UPDATE SKIP LOCKED; sets claim owner and a lease of lease_seconds (default 300) and increments each event''s attempts. Dispatcher-tier (pgokf_dispatcher, or pgokf_admin); tenant-scoped. Delivery is at-least-once: acknowledge with pgokf.ack_catalog_change_event after the producer durably accepts the event.';
COMMENT ON FUNCTION pgokf.ack_catalog_change_event(bigint, text, text) IS
    'Acknowledge a claimed catalog-change event after durable producer acceptance. Dispatcher-tier (pgokf_dispatcher, or pgokf_admin); only the producer label holding the claim may ack (42501 otherwise), a retry with the same acceptance_key is an idempotent no-op returning true, and a conflicting key or an unknown/unclaimed event raises 22023. Unacknowledged events stay retryable and are never pruned.';
COMMENT ON FUNCTION pgokf.list_catalog_change_events(bigint, integer) IS
    'Admin inspection of the catalog-change outbox: recent events as pgokf.catalog_change_event_info, newest first, optionally scoped to one bundle and bounded by max_rows. Admin-only (pgokf_admin); tenant-scoped; the raw table itself is granted to no role.';
",
        name = "change_event_function_hardening",
        requires = [
            claim_catalog_change_events,
            ack_catalog_change_event,
            list_catalog_change_events
        ]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;

    fn staged_concept(id: &str, path: &str, hash: &str) -> StagedConcept {
        let markdown = format!("---\ntype: Reference\ntitle: {id}\n---\n\nbody\n");
        let concept = okf_parser::parse_concept(
            markdown.as_bytes(),
            path,
            okf_parser::ParserLimits::default(),
        )
        .expect("fixture parses");
        StagedConcept {
            concept,
            file_hash: hash.to_owned(),
            modified_at_epoch: None,
            raw_content: None,
            typed: None,
        }
    }

    #[test]
    fn build_changes_classifies_added_updated_removed_and_reclassified() {
        // Arrange: a pre-sync projection with one unchanged-looking path whose
        // hash changed (update), one path that keeps its bytes but changes id
        // (reclassification), and one path that disappeared (removal); plus one
        // brand-new staged concept.
        let stored_hashes = BTreeMap::from([
            ("edit.md".to_owned(), "h1".to_owned()),
            ("recls.md".to_owned(), "h2".to_owned()),
            ("gone.md".to_owned(), "h3".to_owned()),
        ]);
        let stored_ids = BTreeMap::from([
            ("edit.md".to_owned(), "edit".to_owned()),
            ("recls.md".to_owned(), "recls-old".to_owned()),
            ("gone.md".to_owned(), "gone".to_owned()),
        ]);
        let staged = vec![
            staged_concept("edit", "edit.md", "h1b"),
            staged_concept("recls", "recls.md", "h2"),
            staged_concept("new", "new.md", "h4"),
        ];

        // Act
        let changes = build_changes(
            &stored_hashes,
            &stored_ids,
            &staged,
            &["gone.md".to_owned()],
        );

        // Assert
        let mut kinds: Vec<(&str, &str)> =
            changes.iter().map(|c| (c.id.as_str(), c.kind)).collect();
        kinds.sort_unstable();
        assert_eq!(
            kinds,
            vec![
                ("edit", "updated"),
                ("gone", "removed"),
                ("new", "added"),
                ("recls", "added"),
                ("recls-old", "removed"),
            ],
            "every staged/removed concept is classified exactly once"
        );
        let updated = changes
            .iter()
            .find(|c| c.id == "edit")
            .expect("updated record");
        assert_eq!(updated.before_hash.as_deref(), Some("h1"));
        assert_eq!(updated.after_hash.as_deref(), Some("h1b"));
    }

    #[test]
    fn validate_limit_bounds_claim_and_list_sizes() {
        // Arrange / Act / Assert
        assert_eq!(validate_limit(1).expect("lower bound"), 1);
        assert_eq!(validate_limit(1000).expect("upper bound"), 1000);
        for invalid in [0, -1, 1001] {
            assert_eq!(
                validate_limit(invalid).expect_err("out of range").kind(),
                ErrorKind::InvalidParameter
            );
        }
    }

    #[test]
    fn validate_claim_args_rejects_empty_producer_and_bad_lease() {
        // Arrange / Act / Assert
        assert!(validate_claim_args("dispatcher", 300).is_ok());
        assert!(validate_claim_args("", 300).is_err());
        assert!(validate_claim_args("dispatcher", 0).is_err());
        assert!(validate_claim_args("dispatcher", 86_401).is_err());
    }
}
