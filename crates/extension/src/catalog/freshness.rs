// SPDX-License-Identifier: AGPL-3.0-only
//! Freshness state, dependency registration, and publication fences
//! (capability A plus the cross-cutting generation/fence capability).
//!
//! # Producer identity
//!
//! Every `producer` argument and column in this module is a **caller-supplied
//! opaque label, not authorization**. Authorization is always `session_user`
//! membership in the writer/admin tiers ([`crate::security`]); the label only
//! travels with the row as provenance. The extension point for a future
//! producer-principal registry is the `created_by`/`producer` column pair: a
//! later release can bind labels to authenticated principals without changing
//! any wire shape.
//!
//! # Freshness state
//!
//! `pgokf.bundle_freshness` holds one row per bundle (created at registration
//! as `fresh`, backfilled `stale` for pre-0.3.0 bundles by the upgrade
//! script): state (`fresh|stale|reconciling|blocked|retired`), machine-readable
//! reason codes, the producer's opaque observed/materialized source revisions,
//! the catalog generation the materialization covers, the reconciliation
//! timestamps, the dependency invalidation epoch pair (see below), and the
//! relationship-coverage evidence column. `pgokf.concept_freshness` holds
//! sparse per-scope overrides keyed `(bundle_id, scope_kind, scope_key)` with
//! generic scope kinds (`concept|path|group`); the reader surface
//! ([`pgokf.effective_freshness`](pgokf)) combines bundle state with the
//! overrides so consumers never read the raw tables.
//!
//! State transitions are conservative: only registration and the
//! compare-and-set [`mark_fresh`](pgokf::mark_fresh) establish `fresh`;
//! retirement maps to `retired`; unretire and disable map to `stale` (currency
//! must be re-established by the producer - no false clear).
//!
//! # Dependency evaluation
//!
//! `pgokf.freshness_dependency` maps a source selector (exact bundle, exact
//! concept id, exact path, or path prefix - **case-sensitive, no glob/regex in
//! v1**) onto a target bundle/scope. For every catalog write, the same
//! transaction that commits the change and its outbox event
//! ([`crate::catalog::change_event`]) evaluates the enabled dependencies whose
//! source bundle changed ([`evaluate_dependencies`]):
//!
//! - a matching selector marks the target bundle (or target scope) `stale`
//!   with reason `dependency_source_changed`, idempotently by generation
//!   (each dependency tracks the source catalog generation it has evaluated
//!   up to; a newly registered dependency starts from the source bundle's
//!   current generation as its baseline, read under the source bundle's
//!   advisory lock so registration serializes against an in-flight source
//!   mutation);
//! - invalidation is **transitive**: a bundle marked `stale` at bundle scope
//!   by dependency evaluation becomes a walk root, and the enabled
//!   bundle-scope dependencies sourced at it mark their own targets in the
//!   same transaction (A -> B -> C: changing A stales B and C). The walk
//!   carries a visited set, so cycles (A -> B -> A) and self-loops terminate,
//!   and it is bounded by [`TRANSITIVE_INVALIDATION_ROW_CAP`];
//! - every bundle-level dependency invalidation bumps the target row's
//!   `dependency_invalidation_epoch`. The compare-and-set
//!   [`mark_fresh`](pgokf::mark_fresh) refuses while the epoch exceeds
//!   `claimed_invalidation_epoch`; [`mark_reconciling`](pgokf::mark_reconciling)
//!   claims the newest epoch, so a completion prepared before an invalidation
//!   landed can never erase it - the producer must re-claim (observing the
//!   invalidation) before completing;
//! - a content change whose change summary was truncated (see
//!   [`crate::catalog::change_event::CHANGE_RECORD_CAP`]) cannot be proved
//!   against a narrowed selector, and a lifecycle event carries no concept
//!   detail at all - in both cases the changed **source** bundle itself is
//!   marked `stale` with reason `change_scope_unknown` ("unknown scope is
//!   stale scope") and the dependency's **registered** dependent is
//!   invalidated directly at its registered target scope (the registration
//!   proves the dependency even when the changed scope cannot); no
//!   unregistered target is ever guessed;
//! - causation suppression: an event whose `causation_key` equals the
//!   dependency's own `causation_key` does not trigger it, so a
//!   producer-driven refresh cannot recursively retrigger the reconciliation
//!   that caused it (cycles mark stale idempotently by generation; no
//!   recursive synthetic events). The same suppression applies edge-by-edge
//!   during the transitive walk;
//! - source removal: `unregister_bundle`/`purge_retired` invalidate the
//!   removed source's enabled registered dependents (reason
//!   `dependency_source_changed`, epoch-bumped and walked transitively) in
//!   the same transaction, before the dependency rows cascade away with the
//!   source - a dependent never stays falsely fresh after its source leaves.
//!
//! The `relationship_coverage_missing` evidence is independent of the mutable
//! state reasons: `mark_relationship_coverage_missing` records it on the
//! dedicated `relationship_coverage_missing_since` column, which
//! [`mark_reconciling`](pgokf::mark_reconciling) (or any other state
//! transition) cannot erase, and the compare-and-set refuses while it stands.
//! Only re-established coverage ([`clear_relationship_coverage_missing`]) or
//! an explicit admin repair clears it.
//!
//! # Publication fences
//!
//! `pgokf.publication_fence` holds one fence slot per
//! `(tenant_id, producer, bundle_id)` with a monotonic `target_generation` and
//! catalog-assigned `fencing_token`. Issuance and release are compare-and-set
//! under the bundle advisory lock: issuance refuses a stale
//! `expected_catalog_generation` or a non-advancing `target_generation`, and
//! release refuses a token that is not the live one, so a superseded or
//! expired producer attempt can never complete a publication.
//!
//! # Security/grants
//!
//! Raw state/dependency/fence tables are granted to **no** API role; all
//! mutation flows through `SECURITY DEFINER` functions (`PUBLIC` revoked;
//! writer tier for registration and `mark_*`, admin tier for repair and
//! list-all), each confining itself to the session's tenant. Readers receive
//! `SELECT` on the `pgokf.effective_freshness` projection only, which applies
//! the standard opt-in tenant predicate inline. Every table additionally
//! carries the standard non-forced tenant row-level-security policy as
//! defense in depth.

use std::collections::HashSet;
use std::path::Path;

use pgrx::datum::TimestampWithTimeZone;
use pgrx::heap_tuple::PgHeapTuple;
use pgrx::spi::SpiHeapTupleData;
use pgrx::{AllocatedByRust, Spi, extension_sql};

use crate::catalog::change_event::ConceptChange;
use crate::catalog::spi_read::RowReader;
use crate::catalog::sync::advisory_lock_key;
use crate::errors::CatalogError;
use crate::security;

/// The freshness states a bundle or scope override may carry.
const STATES: [&str; 5] = ["fresh", "stale", "reconciling", "blocked", "retired"];

/// The scope kinds a `pgokf.concept_freshness` override may address.
const SCOPE_KINDS: [&str; 3] = ["concept", "path", "group"];

/// The selector kinds a `pgokf.freshness_dependency` source may use.
const SELECTOR_KINDS: [&str; 4] = ["bundle", "concept", "path", "path_prefix"];

/// The scope kinds a dependency target may address (`bundle` plus the
/// `pgokf.concept_freshness` scope kinds).
const TARGET_SCOPE_KINDS: [&str; 4] = ["bundle", "concept", "path", "group"];

/// Reason code recorded when a dependency's source changed underneath its
/// target.
const REASON_SOURCE_CHANGED: &str = "dependency_source_changed";
/// Reason code recorded when a change's scope cannot be proved against a
/// narrowed selector ("unknown scope is stale scope").
const REASON_SCOPE_UNKNOWN: &str = "change_scope_unknown";

/// Reason code recorded when a refresh superseded a bundle's relationship
/// coverage (its active [`crate::catalog::relationships`] publications) and no
/// staged publication matched the accepted generation. While it stands, the
/// compare-and-set [`mark_fresh`](pgokf::mark_fresh) refuses: the producer
/// must publish a matching replacement (which clears the reason) before its
/// reconciliation can complete.
pub(crate) const REASON_RELATIONSHIP_COVERAGE_MISSING: &str = "relationship_coverage_missing";

/// Bound on the rows one transitive invalidation walk may visit. The
/// dependency registry is explicitly registered and small; the cap only
/// bounds a pathological registration set (the walk's visited set already
/// makes cycles and self-loops terminate).
const TRANSITIVE_INVALIDATION_ROW_CAP: usize = 1024;

fn spi_error(context: &str, error: &pgrx::spi::Error) -> CatalogError {
    CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// The shared `22023` shape for an unregistered (or cross-tenant) bundle id,
/// identical to what every other bundle-addressed entry point raises.
fn unknown_bundle_error(bundle_id: i64) -> CatalogError {
    CatalogError::invalid_parameter(
        format!("bundle {bundle_id} is not registered"),
        Path::new(""),
    )
}

/// The stored canonical path of a registered bundle (owner-rights read: the
/// callers are `SECURITY DEFINER` bodies already tenant-confined by
/// [`security::enforce_bundle_tenant`]).
pub(crate) fn bundle_path(bundle_id: i64) -> Result<String, CatalogError> {
    Spi::get_one_with_args::<String>(
        "SELECT path FROM pgokf.bundles WHERE id = $1",
        &[bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to look up bundle path", &error))?
    .ok_or_else(|| unknown_bundle_error(bundle_id))
}

/// Initialize the bundle's freshness row at registration.
///
/// Inserted `fresh` with the registration's catalog generation and sync hash
/// as the initial materialization evidence (a fresh install's bundles are
/// born current; pre-0.3.0 bundles are backfilled `stale` by the upgrade
/// script instead). `ON CONFLICT DO NOTHING` makes the call idempotent for
/// refresh/content resyncs, which never establish freshness on their own.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure.
pub(crate) fn initialize_bundle(
    bundle_id: i64,
    catalog_generation: i64,
    sync_hash: &str,
    context: &crate::catalog::change_event::ChangeContext,
) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "INSERT INTO pgokf.bundle_freshness
             (bundle_id, tenant_id, state, reason_codes,
              observed_source_generation, materialized_source_generation,
              materialized_catalog_generation, last_reconciled_at,
              producer, manifest_hash)
         SELECT $1, b.tenant_id, 'fresh', '{}'::text[], $5, $5, $2,
                pg_catalog.now(), $3, $4
         FROM pgokf.bundles b
         WHERE b.id = $1
         ON CONFLICT (bundle_id) DO NOTHING",
        &[
            bundle_id.into(),
            catalog_generation.into(),
            context.producer.as_deref().into(),
            Some(sync_hash).into(),
            context.observed_source_generation.as_deref().into(),
        ],
    )
    .map_err(|error| spi_error("failed to initialize bundle freshness", &error))
}

/// Upsert a bundle's freshness state, merging reason codes.
///
/// `stale_since` is set on the transition into a non-fresh state and preserved
/// while the bundle remains stale; `last_reconciled_at` is only ever advanced
/// by the compare-and-set completion. A `retired` row is never moved back by
/// this path (only [`unretire`] and repair do so deliberately).
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure.
fn set_bundle_state(
    bundle_id: i64,
    state: &str,
    reason_codes: &[&str],
    producer: Option<&str>,
    observed_source_generation: Option<&str>,
) -> Result<(), CatalogError> {
    debug_assert!(STATES.contains(&state));
    debug_assert!(state != "fresh", "freshness is established only by CAS");
    let reasons: Vec<&str> = reason_codes.to_vec();
    Spi::run_with_args(
        "INSERT INTO pgokf.bundle_freshness
             (bundle_id, tenant_id, state, reason_codes, producer,
              observed_source_generation, stale_since)
         SELECT $1, b.tenant_id, $2, $3::text[], $4, $5,
                CASE WHEN $2 = 'fresh' THEN NULL ELSE pg_catalog.now() END
         FROM pgokf.bundles b
         WHERE b.id = $1
         ON CONFLICT (bundle_id) DO UPDATE SET
             state = $2,
             -- Reason codes describe the CURRENT state: replace them on a
             -- state change, merge (deduplicated) while the state persists.
             -- The merge COALESCEs: array_agg over an empty union (a repeated
             -- reason-less transition such as a second mark_reconciling) is
             -- NULL, and reason_codes is NOT NULL.
             reason_codes = CASE
                 WHEN pgokf.bundle_freshness.state = $2
                     THEN COALESCE(
                         (SELECT pg_catalog.array_agg(DISTINCT r)
                          FROM pg_catalog.unnest(
                              pgokf.bundle_freshness.reason_codes || $3::text[]) AS r),
                         '{}'::text[])
                 ELSE $3::text[] END,
             stale_since = CASE
                 WHEN $2 = 'stale' AND pgokf.bundle_freshness.state = 'stale'
                     THEN pgokf.bundle_freshness.stale_since
                 WHEN $2 = 'stale' THEN pg_catalog.now()
                 ELSE pgokf.bundle_freshness.stale_since END,
             producer = COALESCE($4, pgokf.bundle_freshness.producer),
             observed_source_generation =
                 COALESCE($5, pgokf.bundle_freshness.observed_source_generation),
             updated_at = pg_catalog.now()
         WHERE pgokf.bundle_freshness.state <> 'retired'",
        &[
            bundle_id.into(),
            state.into(),
            reasons.into(),
            producer.into(),
            observed_source_generation.into(),
        ],
    )
    .map_err(|error| spi_error("failed to update bundle freshness", &error))
}

/// The bundle-state transitions the admin mutation sites drive.
///
/// Retirement maps to `retired`; unretire and disable map to `stale` with the
/// given reason (currency evidence must be re-established by a producer
/// compare-and-set - the catalog never clears staleness on its own). Enabling
/// a bundle is not a content-currency transition and leaves the state as is.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure.
pub(crate) fn transition_bundle(
    bundle_id: i64,
    transition: BundleTransition,
) -> Result<(), CatalogError> {
    match transition {
        BundleTransition::Retire => {
            set_bundle_state(bundle_id, "retired", &["bundle_retired"], None, None)
        }
        BundleTransition::Unretire => {
            // The retired guard in set_bundle_state would skip this row, so the
            // restore updates it directly.
            Spi::run_with_args(
                "UPDATE pgokf.bundle_freshness
                 SET state = 'stale',
                     reason_codes = ARRAY['bundle_restored']::text[],
                     stale_since = pg_catalog.now(),
                     updated_at = pg_catalog.now()
                 WHERE bundle_id = $1",
                &[bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to restore bundle freshness", &error))
        }
        BundleTransition::Disable => {
            set_bundle_state(bundle_id, "stale", &["bundle_disabled"], None, None)
        }
    }
}

/// The admin-mutation transitions that move a bundle's freshness state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BundleTransition {
    /// `retire_bundle`: the bundle leaves default discovery; state `retired`.
    Retire,
    /// `unretire_bundle`: the bundle returns; currency must be re-established,
    /// so state `stale` with reason `bundle_restored`.
    Unretire,
    /// `set_bundle_enabled(false)`: conservative `stale` with reason
    /// `bundle_disabled`; enabling does not clear it.
    Disable,
}

/// Mark a bundle stale because a refresh superseded its relationship coverage
/// without a matching replacement (the `relationship_coverage_missing`
/// reason). Called by the sync tail when
/// [`crate::catalog::relationships::activate_staged`] reports the loss.
///
/// The evidence is also recorded on the dedicated
/// `relationship_coverage_missing_since` column, which no state transition
/// (including `mark_reconciling`) erases: the compare-and-set
/// [`mark_fresh`](pgokf::mark_fresh) refuses until coverage is genuinely
/// re-established or an admin repair clears it.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure.
pub(crate) fn mark_relationship_coverage_missing(bundle_id: i64) -> Result<(), CatalogError> {
    set_bundle_state(
        bundle_id,
        "stale",
        &[REASON_RELATIONSHIP_COVERAGE_MISSING],
        None,
        None,
    )?;
    Spi::run_with_args(
        "UPDATE pgokf.bundle_freshness
         SET relationship_coverage_missing_since =
                 COALESCE(relationship_coverage_missing_since, pg_catalog.now()),
             updated_at = pg_catalog.now()
         WHERE bundle_id = $1 AND state <> 'retired'",
        &[bundle_id.into()],
    )
    .map_err(|error| {
        spi_error(
            "failed to record the relationship coverage evidence",
            &error,
        )
    })
}

/// Clear a standing `relationship_coverage_missing` reason and its dedicated
/// evidence column, leaving the state and every other reason untouched.
/// Called when relationship coverage is re-established (a publication
/// activates, immediately or at sync time) so the compare-and-set
/// [`mark_fresh`](pgokf::mark_fresh) can complete again.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure.
pub(crate) fn clear_relationship_coverage_missing(bundle_id: i64) -> Result<(), CatalogError> {
    Spi::run_with_args(
        "UPDATE pgokf.bundle_freshness
         SET reason_codes = pg_catalog.array_remove(reason_codes, $2),
             relationship_coverage_missing_since = NULL,
             updated_at = pg_catalog.now()
         WHERE bundle_id = $1
           AND (reason_codes @> ARRAY[$2]::text[]
                OR relationship_coverage_missing_since IS NOT NULL)",
        &[
            bundle_id.into(),
            REASON_RELATIONSHIP_COVERAGE_MISSING.into(),
        ],
    )
    .map_err(|error| spi_error("failed to clear the relationship coverage reason", &error))
}

/// Mark a bundle stale because one of its registered dependency sources was
/// invalidated - a direct selector match, an unprovable-scope lifecycle or
/// truncated change, a transitive walk hop, or the source leaving the
/// catalog. Records `dependency_source_changed` and bumps the row's
/// dependency invalidation epoch, so a compare-and-set completion prepared
/// from pre-invalidation evidence is refused until the producer claims the
/// newest epoch with [`mark_reconciling`](pgokf::mark_reconciling).
fn invalidate_dependent_bundle(bundle_id: i64) -> Result<(), CatalogError> {
    set_bundle_state(bundle_id, "stale", &[REASON_SOURCE_CHANGED], None, None)?;
    Spi::run_with_args(
        "UPDATE pgokf.bundle_freshness
         SET dependency_invalidation_epoch = dependency_invalidation_epoch + 1,
             updated_at = pg_catalog.now()
         WHERE bundle_id = $1 AND state <> 'retired'",
        &[bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to bump the dependency invalidation epoch", &error))
}

/// Walk the registered dependency graph transitively from newly invalidated
/// bundles, marking every reachable bundle-level dependent stale (epoch
/// bumped per hop).
///
/// Cycle-safe: `visited` (seeded with the roots, which are already stale)
/// makes cycles and self-loops terminate, and the walk is bounded by
/// [`TRANSITIVE_INVALIDATION_ROW_CAP`]. Only bundle-scope edges propagate -
/// a scope override does not imply the whole bundle, so it never feeds the
/// walk. The originating event's causation key suppresses matching edges
/// exactly as in direct evaluation, so a producer-driven reconciliation loop
/// cannot retrigger itself through a cycle.
fn propagate_transitive(roots: &[i64], causation_key: Option<&str>) -> Result<(), CatalogError> {
    let mut visited: HashSet<i64> = roots.iter().copied().collect();
    let mut frontier: Vec<i64> = roots.to_vec();
    while let Some(bundle_id) = frontier.pop() {
        if visited.len() >= TRANSITIVE_INVALIDATION_ROW_CAP {
            break;
        }
        let edges: Vec<(i64, Option<String>)> = Spi::connect(|client| {
            let table = client
                .select(
                    "SELECT target_bundle_id, causation_key
                     FROM pgokf.freshness_dependency
                     WHERE source_bundle_id = $1
                       AND enabled
                       AND target_scope_kind = 'bundle'
                     ORDER BY dependency_id",
                    None,
                    &[bundle_id.into()],
                )
                .map_err(|error| {
                    spi_error("failed to load transitive freshness dependencies", &error)
                })?;
            let mut edges = Vec::with_capacity(table.len());
            for row in table {
                let reader =
                    RowReader::new(&row, "failed to read transitive dependency", "dependency");
                edges.push((reader.required(1, "target_bundle_id")?, reader.optional(2)?));
            }
            Ok(edges)
        })?;
        for (target_bundle_id, edge_causation_key) in edges {
            // Causation suppression, edge by edge, with the originating key.
            if causation_key.is_some() && edge_causation_key.as_deref() == causation_key {
                continue;
            }
            // Already invalidated (a root, a cycle back-edge, or a self-loop).
            if !visited.insert(target_bundle_id) {
                continue;
            }
            invalidate_dependent_bundle(target_bundle_id)?;
            if visited.len() >= TRANSITIVE_INVALIDATION_ROW_CAP {
                break;
            }
            frontier.push(target_bundle_id);
        }
    }
    Ok(())
}

/// One enabled registered dependent of a source bundle that is leaving the
/// catalog (unregister/purge), captured before the dependency rows cascade
/// away with the source.
pub(crate) struct DepartingDependent {
    bundle_id: i64,
    scope_kind: String,
    scope_key: Option<String>,
}

/// Load the enabled registered dependents of a departing source bundle.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure.
pub(crate) fn departing_dependents(
    source_bundle_id: i64,
) -> Result<Vec<DepartingDependent>, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT target_bundle_id, target_scope_kind, target_scope_key
                 FROM pgokf.freshness_dependency
                 WHERE source_bundle_id = $1 AND enabled
                 ORDER BY dependency_id",
                None,
                &[source_bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to load departing dependents", &error))?;
        let mut dependents = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(&row, "failed to read departing dependent", "dependency");
            dependents.push(DepartingDependent {
                bundle_id: reader.required(1, "target_bundle_id")?,
                scope_kind: reader.required(2, "target_scope_kind")?,
                scope_key: reader.optional(3)?,
            });
        }
        Ok(dependents)
    })
}

/// Invalidate the dependents of a removed source bundle (reason
/// `dependency_source_changed`; bundle targets are epoch-bumped and feed the
/// transitive walk), in the same transaction as the removal. Called by
/// `unregister_bundle`/`purge_retired` after the source row (and with it the
/// dependency registrations) has been deleted, so a dependent never stays
/// falsely fresh after its source leaves the catalog.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure.
pub(crate) fn invalidate_departing_dependents(
    dependents: &[DepartingDependent],
) -> Result<(), CatalogError> {
    let mut roots = Vec::new();
    for dependent in dependents {
        match dependent.scope_kind.as_str() {
            "bundle" => {
                invalidate_dependent_bundle(dependent.bundle_id)?;
                roots.push(dependent.bundle_id);
            }
            scope_kind => set_scope_state(
                dependent.bundle_id,
                scope_kind,
                dependent
                    .scope_key
                    .as_deref()
                    .expect("a scoped target carries a scope key"),
                "stale",
                &[REASON_SOURCE_CHANGED],
                None,
            )?,
        }
    }
    propagate_transitive(&roots, None)
}

/// Upsert a scope override's freshness state (used by dependency evaluation
/// and `mark_scope_stale`).
fn set_scope_state(
    bundle_id: i64,
    scope_kind: &str,
    scope_key: &str,
    state: &str,
    reason_codes: &[&str],
    producer: Option<&str>,
) -> Result<(), CatalogError> {
    debug_assert!(SCOPE_KINDS.contains(&scope_kind));
    let reasons: Vec<&str> = reason_codes.to_vec();
    Spi::run_with_args(
        "INSERT INTO pgokf.concept_freshness
             (bundle_id, scope_kind, scope_key, tenant_id, state, reason_codes,
              producer, stale_since)
         SELECT $1, $2, $3, b.tenant_id, $4, $5::text[], $6,
                CASE WHEN $4 = 'fresh' THEN NULL ELSE pg_catalog.now() END
         FROM pgokf.bundles b
         WHERE b.id = $1
         ON CONFLICT (bundle_id, scope_kind, scope_key) DO UPDATE SET
             state = $4,
             reason_codes = CASE
                 WHEN pgokf.concept_freshness.state = $4
                     THEN (SELECT pg_catalog.array_agg(DISTINCT r)
                           FROM pg_catalog.unnest(
                               pgokf.concept_freshness.reason_codes || $5::text[]) AS r)
                 ELSE $5::text[] END,
             stale_since = CASE
                 WHEN $4 = 'stale' AND pgokf.concept_freshness.state = 'stale'
                     THEN pgokf.concept_freshness.stale_since
                 WHEN $4 = 'stale' THEN pg_catalog.now()
                 ELSE pgokf.concept_freshness.stale_since END,
             producer = COALESCE($6, pgokf.concept_freshness.producer),
             updated_at = pg_catalog.now()
         WHERE pgokf.concept_freshness.state <> 'retired'",
        &[
            bundle_id.into(),
            scope_kind.into(),
            scope_key.into(),
            state.into(),
            reasons.into(),
            producer.into(),
        ],
    )
    .map_err(|error| spi_error("failed to update scope freshness", &error))
}

/// One enabled dependency row, as the evaluation loop reads it.
struct Dependency {
    id: i64,
    selector_kind: String,
    selector_value: String,
    target_bundle_id: i64,
    target_scope_kind: String,
    target_scope_key: Option<String>,
    causation_key: Option<String>,
}

/// Whether a narrowed (`concept`/`path`/`path_prefix`) selector matches the change
/// set. Exact, case-sensitive matches only - no glob/regex in v1.
fn selector_matches(selector_kind: &str, selector_value: &str, changes: &[ConceptChange]) -> bool {
    match selector_kind {
        "concept" => changes.iter().any(|change| change.id == selector_value),
        "path" => changes.iter().any(|change| change.path == selector_value),
        "path_prefix" => changes
            .iter()
            .any(|change| change.path.starts_with(selector_value)),
        _ => false,
    }
}

/// Evaluate the enabled dependencies whose source is the changed bundle.
///
/// Called inside the mutation's transaction, after the outbox event is
/// committed ([`crate::catalog::change_event::record`]) and while the bundle
/// advisory lock is held. `is_state_change` distinguishes a lifecycle event
/// (no concept detail) from a content change; `truncated` marks a change set
/// whose per-bucket cap was hit (see
/// [`crate::catalog::change_event::CHANGE_RECORD_CAP`]). Both make a narrowed
/// selector unprovable, triggering the unknown-scope rule: the *source* bundle
/// is marked stale and the dependency's *registered* dependent is invalidated
/// directly at its registered target scope - the registration proves the
/// dependency even when the changed scope cannot, so no unregistered target
/// is ever guessed.
///
/// Every evaluated dependency's watermark (`last_source_catalog_generation`,
/// `last_event_id`) advances to this event, which is what makes repeated
/// marking idempotent by generation. An event whose causation key equals the
/// dependency's own is recorded but does not mark anything.
///
/// Bundle-level invalidations are epoch-bumped ([`invalidate_dependent_bundle`])
/// and feed the transitive walk ([`propagate_transitive`]): every registered
/// dependent reachable through bundle-scope edges is marked stale in this
/// same transaction, cycle-safely.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the surrounding
/// transaction so evaluation commits atomically with the change.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_dependencies(
    source_bundle_id: i64,
    event_id: i64,
    catalog_generation: i64,
    is_state_change: bool,
    changes: &[ConceptChange],
    truncated: bool,
    causation_key: Option<&str>,
) -> Result<(), CatalogError> {
    let dependencies: Vec<Dependency> = Spi::connect(|client| {
        let table = client
            .select(
                "SELECT dependency_id, selector_kind, selector_value,
                        target_bundle_id, target_scope_kind, target_scope_key, causation_key
                 FROM pgokf.freshness_dependency
                 WHERE source_bundle_id = $1
                   AND enabled
                   AND last_source_catalog_generation < $2
                 ORDER BY dependency_id",
                None,
                &[source_bundle_id.into(), catalog_generation.into()],
            )
            .map_err(|error| spi_error("failed to load freshness dependencies", &error))?;
        let mut dependencies = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(&row, "failed to read freshness dependency", "dependency");
            dependencies.push(Dependency {
                id: reader.required(1, "dependency_id")?,
                selector_kind: reader.required(2, "selector_kind")?,
                selector_value: reader.required(3, "selector_value")?,
                target_bundle_id: reader.required(4, "target_bundle_id")?,
                target_scope_kind: reader.required(5, "target_scope_kind")?,
                target_scope_key: reader.optional(6)?,
                causation_key: reader.optional(7)?,
            });
        }
        Ok(dependencies)
    })?;

    // Bundles marked stale at bundle scope by this evaluation; each becomes a
    // root of the transitive walk below.
    let mut invalidated: Vec<i64> = Vec::new();
    for dependency in dependencies {
        // Record the evaluation watermark first: each enabled dependency
        // advances monotonically with the source bundle's generation, so a
        // retried evaluation of the same event re-marks nothing.
        Spi::run_with_args(
            "UPDATE pgokf.freshness_dependency
             SET last_source_catalog_generation = $2,
                 last_event_id = $3,
                 updated_at = pg_catalog.now()
             WHERE dependency_id = $1",
            &[
                dependency.id.into(),
                catalog_generation.into(),
                event_id.into(),
            ],
        )
        .map_err(|error| spi_error("failed to advance dependency watermark", &error))?;

        // Causation suppression: a change caused by this dependency's own
        // reconciliation loop never re-triggers it (no synthetic recursion).
        if causation_key.is_some() && dependency.causation_key.as_deref() == causation_key {
            continue;
        }

        if dependency.selector_kind != "bundle" && (is_state_change || truncated) {
            // Unknown/unmapped scope: mark the changed source bundle itself
            // stale, and invalidate the registered dependent directly at its
            // registered target scope; never guess an unregistered target.
            set_bundle_state(
                source_bundle_id,
                "stale",
                &[REASON_SCOPE_UNKNOWN],
                None,
                None,
            )?;
            if invalidate_registered_target(&dependency)? {
                invalidated.push(dependency.target_bundle_id);
            }
            continue;
        }

        let matched = if dependency.selector_kind == "bundle" {
            true
        } else {
            selector_matches(
                &dependency.selector_kind,
                &dependency.selector_value,
                changes,
            )
        };
        if !matched {
            continue;
        }

        if invalidate_registered_target(&dependency)? {
            invalidated.push(dependency.target_bundle_id);
        }
    }
    propagate_transitive(&invalidated, causation_key)
}

/// Invalidate one dependency's registered dependent at its registered target
/// scope (reason `dependency_source_changed`; a bundle target is
/// epoch-bumped and feeds the transitive walk). Returns whether the
/// invalidation landed at bundle scope.
fn invalidate_registered_target(dependency: &Dependency) -> Result<bool, CatalogError> {
    match dependency.target_scope_kind.as_str() {
        "bundle" => {
            invalidate_dependent_bundle(dependency.target_bundle_id)?;
            Ok(true)
        }
        scope_kind => set_scope_state(
            dependency.target_bundle_id,
            scope_kind,
            dependency
                .target_scope_key
                .as_deref()
                .expect("a scoped target carries a scope key"),
            "stale",
            &[REASON_SOURCE_CHANGED],
            None,
        )
        .map(|()| false),
    }
}

// ---------------------------------------------------------------------------
// DDL: tables, indexes, RLS, comments, grants (fresh-install block).
// ---------------------------------------------------------------------------

extension_sql!(
    r"
CREATE TABLE pgokf.bundle_freshness (
    bundle_id         bigint PRIMARY KEY,
    tenant_id         text NOT NULL DEFAULT 'default',
    state             text NOT NULL DEFAULT 'fresh',
    reason_codes      text[] NOT NULL DEFAULT '{}',
    observed_source_generation    text,
    materialized_source_generation text,
    materialized_catalog_generation bigint,
    stale_since       timestamptz,
    last_reconciled_at timestamptz,
    producer          text,
    manifest_hash     text,
    embedding_contract jsonb,
    dependency_invalidation_epoch bigint NOT NULL DEFAULT 0,
    claimed_invalidation_epoch bigint NOT NULL DEFAULT 0,
    relationship_coverage_missing_since timestamptz,
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT bundle_freshness_bundle_fk
        FOREIGN KEY (bundle_id) REFERENCES pgokf.bundles (id) ON DELETE CASCADE,
    CONSTRAINT bundle_freshness_state_chk
        CHECK (state IN ('fresh', 'stale', 'reconciling', 'blocked', 'retired'))
);

CREATE TABLE pgokf.concept_freshness (
    bundle_id         bigint NOT NULL,
    scope_kind        text NOT NULL,
    scope_key         text NOT NULL,
    tenant_id         text NOT NULL DEFAULT 'default',
    state             text NOT NULL DEFAULT 'stale',
    reason_codes      text[] NOT NULL DEFAULT '{}',
    observed_source_generation    text,
    materialized_source_generation text,
    materialized_catalog_generation bigint,
    stale_since       timestamptz,
    last_reconciled_at timestamptz,
    producer          text,
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT concept_freshness_pkey PRIMARY KEY (bundle_id, scope_kind, scope_key),
    CONSTRAINT concept_freshness_bundle_fk
        FOREIGN KEY (bundle_id) REFERENCES pgokf.bundles (id) ON DELETE CASCADE,
    CONSTRAINT concept_freshness_scope_kind_chk
        CHECK (scope_kind IN ('concept', 'path', 'group')),
    CONSTRAINT concept_freshness_state_chk
        CHECK (state IN ('fresh', 'stale', 'reconciling', 'blocked', 'retired'))
);

CREATE INDEX concept_freshness_tenant_idx ON pgokf.concept_freshness (tenant_id);

CREATE TABLE pgokf.freshness_dependency (
    dependency_id     bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id         text NOT NULL DEFAULT 'default',
    producer          text NOT NULL,
    enabled           boolean NOT NULL DEFAULT true,
    created_by        text NOT NULL DEFAULT session_user,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    origin            text,
    causation_key     text,
    source_bundle_id  bigint NOT NULL,
    selector_kind     text NOT NULL,
    selector_value    text NOT NULL DEFAULT '',
    last_source_catalog_generation bigint NOT NULL DEFAULT 0,
    last_event_id     bigint,
    target_bundle_id  bigint NOT NULL,
    target_scope_kind text NOT NULL DEFAULT 'bundle',
    target_scope_key  text,
    reconciliation_watermark text,
    CONSTRAINT freshness_dependency_source_fk
        FOREIGN KEY (source_bundle_id) REFERENCES pgokf.bundles (id) ON DELETE CASCADE,
    CONSTRAINT freshness_dependency_target_fk
        FOREIGN KEY (target_bundle_id) REFERENCES pgokf.bundles (id) ON DELETE CASCADE,
    CONSTRAINT freshness_dependency_selector_kind_chk
        CHECK (selector_kind IN ('bundle', 'concept', 'path', 'path_prefix')),
    CONSTRAINT freshness_dependency_selector_value_chk
        CHECK ((selector_kind = 'bundle') = (selector_value = '')),
    CONSTRAINT freshness_dependency_target_scope_chk
        CHECK (target_scope_kind IN ('bundle', 'concept', 'path', 'group')
               AND (target_scope_kind = 'bundle') = (target_scope_key IS NULL)),
    CONSTRAINT freshness_dependency_uq UNIQUE NULLS NOT DISTINCT
        (tenant_id, producer, source_bundle_id, selector_kind, selector_value,
         target_bundle_id, target_scope_kind, target_scope_key)
);

-- The evaluation loop loads the enabled dependencies of one changed source
-- bundle, oldest first.
CREATE INDEX freshness_dependency_source_idx
    ON pgokf.freshness_dependency (source_bundle_id) WHERE enabled;
CREATE INDEX freshness_dependency_target_idx
    ON pgokf.freshness_dependency (target_bundle_id);

CREATE TABLE pgokf.publication_fence (
    tenant_id         text NOT NULL DEFAULT 'default',
    producer          text NOT NULL,
    bundle_id         bigint NOT NULL,
    target_generation bigint NOT NULL,
    fencing_token     bigint NOT NULL,
    expected_catalog_generation bigint NOT NULL,
    manifest_hash     text,
    state             text NOT NULL DEFAULT 'issued',
    issued_at         timestamptz NOT NULL DEFAULT now(),
    expires_at        timestamptz NOT NULL,
    created_by        text NOT NULL DEFAULT session_user,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT publication_fence_pkey PRIMARY KEY (tenant_id, producer, bundle_id),
    CONSTRAINT publication_fence_bundle_fk
        FOREIGN KEY (bundle_id) REFERENCES pgokf.bundles (id) ON DELETE CASCADE,
    CONSTRAINT publication_fence_state_chk
        CHECK (state IN ('issued', 'released', 'superseded', 'expired')),
    CONSTRAINT publication_fence_monotonic_chk
        CHECK (target_generation > 0 AND fencing_token > 0)
);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id of each table. Not forced, so the SECURITY DEFINER
-- write/evaluation paths bypass it; no API role holds direct DML or SELECT on
-- the raw tables (readers get the pgokf.effective_freshness projection only),
-- so the policies are defense in depth.
ALTER TABLE pgokf.bundle_freshness ENABLE ROW LEVEL SECURITY;
CREATE POLICY bundle_freshness_tenant_isolation ON pgokf.bundle_freshness
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_freshness ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_freshness_tenant_isolation ON pgokf.concept_freshness
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.freshness_dependency ENABLE ROW LEVEL SECURITY;
CREATE POLICY freshness_dependency_tenant_isolation ON pgokf.freshness_dependency
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.publication_fence ENABLE ROW LEVEL SECURITY;
CREATE POLICY publication_fence_tenant_isolation ON pgokf.publication_fence
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

REVOKE ALL ON pgokf.bundle_freshness FROM PUBLIC;
REVOKE ALL ON pgokf.concept_freshness FROM PUBLIC;
REVOKE ALL ON pgokf.freshness_dependency FROM PUBLIC;
REVOKE ALL ON pgokf.publication_fence FROM PUBLIC;

COMMENT ON TABLE pgokf.bundle_freshness IS
    'One freshness row per bundle: generic state (fresh/stale/reconciling/blocked/retired) with machine-readable reason codes, the producer''s opaque observed/materialized source revisions, the catalog generation the materialization covers, reconciliation timestamps, the dependency invalidation epoch pair guarding the compare-and-set, and the relationship-coverage evidence column. Created fresh at bundle registration; pre-0.3.0 bundles were backfilled stale (reason legacy_pre_0.3.0) and stay stale until a producer compare-and-set (pgokf.mark_fresh) re-establishes currency. Mutated only through the SECURITY DEFINER mark_* functions and dependency evaluation; granted to no API role.';
COMMENT ON COLUMN pgokf.bundle_freshness.bundle_id IS
    'The bundle this state belongs to (ON DELETE CASCADE: the row leaves with the bundle).';
COMMENT ON COLUMN pgokf.bundle_freshness.tenant_id IS
    'Multi-tenant owner, denormalized from the bundle for a local row-level-security predicate; always equals pgokf.bundles.tenant_id.';
COMMENT ON COLUMN pgokf.bundle_freshness.state IS
    'Effective bundle freshness: fresh, stale, reconciling (a reconciliation attempt owns the newest target; still effectively stale), blocked (a nonretryable failure; prior data stays labeled), or retired (the bundle is retired). Only registration and the compare-and-set pgokf.mark_fresh establish fresh.';
COMMENT ON COLUMN pgokf.bundle_freshness.reason_codes IS
    'Machine-readable, producer-supplied reason codes explaining the current non-fresh state (merged, deduplicated). Catalog-defined codes: legacy_pre_0.3.0, dependency_source_changed, change_scope_unknown, bundle_retired, bundle_restored, bundle_disabled, relationship_coverage_missing; producers may add their own opaque codes.';
COMMENT ON COLUMN pgokf.bundle_freshness.observed_source_generation IS
    'The newest source revision the producer has observed for this bundle, as opaque producer-supplied text; the catalog never interprets it. pgokf.mark_fresh compares against it (compare-and-set).';
COMMENT ON COLUMN pgokf.bundle_freshness.materialized_source_generation IS
    'The source revision the current catalog materialization corresponds to (producer-reported, opaque), set by a successful pgokf.mark_fresh.';
COMMENT ON COLUMN pgokf.bundle_freshness.materialized_catalog_generation IS
    'The pgokf.bundles.catalog_generation value the current materialization covers; pgokf.mark_fresh refuses an older or mismatched expected generation.';
COMMENT ON COLUMN pgokf.bundle_freshness.stale_since IS
    'When the bundle first entered its current stale period (preserved while it remains stale; cleared by pgokf.mark_fresh).';
COMMENT ON COLUMN pgokf.bundle_freshness.last_reconciled_at IS
    'When a producer reconciliation last completed (a successful pgokf.mark_fresh).';
COMMENT ON COLUMN pgokf.bundle_freshness.producer IS
    'Opaque caller-supplied producer label of the last state transition. NOT authorization: authorization is session_user membership in the writer/admin tiers.';
COMMENT ON COLUMN pgokf.bundle_freshness.manifest_hash IS
    'Hash of the publication manifest the current materialization was produced from (producer-supplied evidence; initialized to the registration sync hash).';
COMMENT ON COLUMN pgokf.bundle_freshness.embedding_contract IS
    'The embedding contract (model/dimension/render version) the producer reconciled against, as opaque jsonb evidence recorded by pgokf.mark_fresh. Semantic ranking does not read this evidence: it enforces the live embedding_model / embedding_dim / embedding_contract policy against each embedding row''s own provenance.';
COMMENT ON COLUMN pgokf.bundle_freshness.dependency_invalidation_epoch IS
    'Monotonic counter bumped every time dependency evaluation (direct, unprovable-scope, transitive, or source-removal) marks this row non-fresh. The compare-and-set pgokf.mark_fresh refuses while it exceeds claimed_invalidation_epoch, so a completion prepared before the newest dependency invalidation can never erase it.';
COMMENT ON COLUMN pgokf.bundle_freshness.claimed_invalidation_epoch IS
    'The dependency_invalidation_epoch the producer''s current reconciliation attempt has claimed via pgokf.mark_reconciling (an admin repair settles it to the standing epoch). pgokf.mark_fresh completes only when the two epochs match.';
COMMENT ON COLUMN pgokf.bundle_freshness.relationship_coverage_missing_since IS
    'When the relationship_coverage_missing evidence was recorded (a refresh superseded the bundle''s relationship coverage without a matching replacement). Independent of the mutable state reasons: no state transition (including pgokf.mark_reconciling) erases it; only re-established coverage (pgokf.replace_relationships activation) or an admin repair clears it, and pgokf.mark_fresh refuses while it stands.';
COMMENT ON COLUMN pgokf.bundle_freshness.updated_at IS
    'When this row last changed.';

COMMENT ON TABLE pgokf.concept_freshness IS
    'Sparse per-scope freshness overrides within a bundle, keyed (bundle_id, scope_kind, scope_key) with generic scope kinds concept (an exact concept id), path (an exact bundle-relative path), or group (a producer-defined group label). Only overrides are stored: a scope with no row inherits its bundle''s state through pgokf.effective_freshness. Written by dependency evaluation and pgokf.mark_scope_stale; granted to no API role.';
COMMENT ON COLUMN pgokf.concept_freshness.scope_kind IS
    'What scope_key names: concept (an OKF concept id), path (a bundle-relative path), or group (a producer-defined group label the catalog never interprets). Generic values only; no producer vocabulary.';
COMMENT ON COLUMN pgokf.concept_freshness.scope_key IS
    'The scope identifier within scope_kind, matched exactly and case-sensitively.';
COMMENT ON COLUMN pgokf.concept_freshness.tenant_id IS
    'Multi-tenant owner, denormalized from the bundle for a local row-level-security predicate; always equals pgokf.bundles.tenant_id.';
COMMENT ON COLUMN pgokf.concept_freshness.state IS
    'This scope''s freshness override: fresh, stale, reconciling, blocked, or retired.';
COMMENT ON COLUMN pgokf.concept_freshness.reason_codes IS
    'Machine-readable reason codes for the override (merged, deduplicated); dependency evaluation records dependency_source_changed.';
COMMENT ON COLUMN pgokf.concept_freshness.observed_source_generation IS
    'The newest source revision observed for this scope (producer-supplied, opaque).';
COMMENT ON COLUMN pgokf.concept_freshness.materialized_source_generation IS
    'The source revision this scope''s current materialization corresponds to (producer-reported, opaque).';
COMMENT ON COLUMN pgokf.concept_freshness.materialized_catalog_generation IS
    'The catalog generation this scope''s current materialization covers.';
COMMENT ON COLUMN pgokf.concept_freshness.stale_since IS
    'When this scope first entered its current stale period.';
COMMENT ON COLUMN pgokf.concept_freshness.last_reconciled_at IS
    'When this scope last reconciled.';
COMMENT ON COLUMN pgokf.concept_freshness.producer IS
    'Opaque caller-supplied producer label of the last transition; not authorization.';
COMMENT ON COLUMN pgokf.concept_freshness.updated_at IS
    'When this override last changed.';

COMMENT ON TABLE pgokf.freshness_dependency IS
    'Registered freshness dependencies: a source selector (bundle / exact concept id / exact path / path prefix, matched case-sensitively - no glob or regex in v1) on a source bundle maps to a target bundle and target scope. Evaluated in the same transaction as every catalog change to the source bundle: a match marks the target stale (idempotent by generation via last_source_catalog_generation); an unprovable scope marks the source bundle itself stale and invalidates the registered dependent at its registered scope instead of guessing; bundle-level invalidation propagates transitively through bundle-scope registrations (cycle-safe, bounded) and bumps the target row''s dependency_invalidation_epoch; removing the source invalidates its registered dependents before their rows cascade away. Registered and removed through pgokf.register_freshness_dependency / remove_freshness_dependency (writer-tier, audited); granted to no API role.';
COMMENT ON COLUMN pgokf.freshness_dependency.dependency_id IS
    'Identity of the registration (GENERATED ALWAYS AS IDENTITY), returned by pgokf.register_freshness_dependency.';
COMMENT ON COLUMN pgokf.freshness_dependency.tenant_id IS
    'Multi-tenant owner, stamped from the source bundle''s tenant at registration.';
COMMENT ON COLUMN pgokf.freshness_dependency.producer IS
    'Opaque caller-supplied producer label. NOT authorization: registration requires session_user membership in pgokf_writer (admin inherits), and both endpoints are tenant-confined.';
COMMENT ON COLUMN pgokf.freshness_dependency.enabled IS
    'Whether the dependency is evaluated on catalog changes (pgokf.disable_freshness_dependency flips it off without losing the registration).';
COMMENT ON COLUMN pgokf.freshness_dependency.created_by IS
    'The session_user that registered the dependency, captured by column default.';
COMMENT ON COLUMN pgokf.freshness_dependency.origin IS
    'Opaque origin metadata supplied at registration, when any.';
COMMENT ON COLUMN pgokf.freshness_dependency.causation_key IS
    'Opaque causation key of the reconciliation loop this dependency belongs to: a catalog-change event carrying the same causation key is recorded but does NOT trigger this dependency, suppressing recursive self-triggering.';
COMMENT ON COLUMN pgokf.freshness_dependency.source_bundle_id IS
    'The bundle whose catalog changes are evaluated against the selector (ON DELETE CASCADE: the registration leaves with the bundle).';
COMMENT ON COLUMN pgokf.freshness_dependency.selector_kind IS
    'The source selector grammar: bundle (any change to the source bundle), concept (an exact concept id), path (an exact bundle-relative path), or path_prefix (a leading path prefix). Matching is exact and case-sensitive; there is no glob or regex in v1.';
COMMENT ON COLUMN pgokf.freshness_dependency.selector_value IS
    'The selector operand: empty for bundle, otherwise the exact concept id, exact path, or path prefix to match (case-sensitive).';
COMMENT ON COLUMN pgokf.freshness_dependency.last_source_catalog_generation IS
    'Watermark: the newest source-bundle catalog generation this dependency has evaluated. Set at registration to the source bundle''s then-current generation (a new dependency starts from the latest source generation) and advanced per evaluated event, making marking idempotent by generation.';
COMMENT ON COLUMN pgokf.freshness_dependency.last_event_id IS
    'The newest pgokf.catalog_change_event.event_id evaluated for this dependency. Not a foreign key: events age out under the change_event_retention_days policy while the watermark must survive.';
COMMENT ON COLUMN pgokf.freshness_dependency.target_bundle_id IS
    'The bundle marked stale when the selector matches (ON DELETE CASCADE).';
COMMENT ON COLUMN pgokf.freshness_dependency.target_scope_kind IS
    'What is marked stale on a match: bundle (the whole target bundle), or a pgokf.concept_freshness scope - concept, path, or group.';
COMMENT ON COLUMN pgokf.freshness_dependency.target_scope_key IS
    'The target scope identifier for non-bundle target_scope_kind; NULL for a bundle target.';
COMMENT ON COLUMN pgokf.freshness_dependency.reconciliation_watermark IS
    'Producer-maintained reconciliation watermark (opaque): where the producer''s catch-up for this dependency stands.';

COMMENT ON TABLE pgokf.publication_fence IS
    'Publication fencing slots, one per (tenant_id, producer, bundle_id): a monotonic target_generation and a catalog-assigned fencing_token bind a producer''s publication attempt to the expected prior catalog generation and manifest hash. Issuance (pgokf.issue_publication_fence) and release (pgokf.release_publication_fence) compare-and-set under the bundle advisory lock; a stale expected generation, a non-advancing target, or a wrong token is rejected, so a superseded or expired attempt can never complete a publication. Granted to no API role.';
COMMENT ON COLUMN pgokf.publication_fence.tenant_id IS
    'Multi-tenant owner, stamped from the bundle''s tenant at issuance; part of the fence slot key.';
COMMENT ON COLUMN pgokf.publication_fence.producer IS
    'Opaque caller-supplied producer label; part of the fence slot key. NOT authorization: issuance/release require session_user membership in pgokf_writer (admin inherits).';
COMMENT ON COLUMN pgokf.publication_fence.bundle_id IS
    'The bundle being published to (ON DELETE CASCADE); part of the fence slot key.';
COMMENT ON COLUMN pgokf.publication_fence.target_generation IS
    'The producer-side monotonic generation this attempt publishes; issuance rejects a value that does not advance past the slot''s current target.';
COMMENT ON COLUMN pgokf.publication_fence.fencing_token IS
    'Catalog-assigned token, incremented on every issuance for the slot; pgokf.release_publication_fence (and later generation-bound write APIs) accept only the live token.';
COMMENT ON COLUMN pgokf.publication_fence.expected_catalog_generation IS
    'The pgokf.bundles.catalog_generation the issuer observed; issuance rejects anything but the current value, so publication always builds on the newest catalog state.';
COMMENT ON COLUMN pgokf.publication_fence.manifest_hash IS
    'Hash of the publication manifest this attempt carries (producer-supplied).';
COMMENT ON COLUMN pgokf.publication_fence.state IS
    'issued (live until expires_at), released (completed normally), superseded (replaced by a newer issuance), or expired.';
COMMENT ON COLUMN pgokf.publication_fence.issued_at IS
    'When the current fence was issued.';
COMMENT ON COLUMN pgokf.publication_fence.expires_at IS
    'When the current fence lease expires; an expired fence no longer authorizes completion.';
COMMENT ON COLUMN pgokf.publication_fence.created_by IS
    'The session_user that first created this slot, captured by column default.';
COMMENT ON COLUMN pgokf.publication_fence.created_at IS
    'When this slot was first created.';
COMMENT ON COLUMN pgokf.publication_fence.updated_at IS
    'When this slot last changed.';
",
    name = "freshness_tables",
    requires = ["catalog_tables"]
);

// The reader surface: one row per recorded scope (the bundle row plus every
// override), with the live catalog generation joined in. The view runs with
// the extension owner's rights over the raw tables (readers hold no grant on
// them) and applies the standard opt-in tenant predicate inline, so a
// `pgokf_reader` sees exactly its tenant's rows.
extension_sql!(
    r"
CREATE VIEW pgokf.effective_freshness AS
SELECT bf.bundle_id,
       'bundle'::text AS scope_kind,
       NULL::text AS scope_key,
       bf.state,
       bf.reason_codes AS reasons,
       bf.stale_since,
       bf.observed_source_generation AS observed_revision,
       bf.materialized_source_generation AS indexed_revision,
       bf.materialized_catalog_generation::text AS published_revision,
       b.catalog_generation,
       bf.last_reconciled_at,
       bf.producer,
       bf.manifest_hash,
       bf.embedding_contract,
       bf.tenant_id
FROM pgokf.bundle_freshness bf
JOIN pgokf.bundles b ON b.id = bf.bundle_id
WHERE (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
         OR pg_catalog.current_setting('pgokf.tenant', true) = '')
        AND NOT (SELECT pgokf.tenant_required()))
       OR bf.tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
UNION ALL
SELECT cf.bundle_id,
       cf.scope_kind,
       cf.scope_key,
       cf.state,
       cf.reason_codes,
       cf.stale_since,
       cf.observed_source_generation,
       cf.materialized_source_generation,
       cf.materialized_catalog_generation::text,
       b.catalog_generation,
       cf.last_reconciled_at,
       cf.producer,
       NULL,
       NULL,
       cf.tenant_id
FROM pgokf.concept_freshness cf
JOIN pgokf.bundles b ON b.id = cf.bundle_id
WHERE (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
         OR pg_catalog.current_setting('pgokf.tenant', true) = '')
        AND NOT (SELECT pgokf.tenant_required()))
       OR cf.tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

COMMENT ON VIEW pgokf.effective_freshness IS
    'Reader surface for catalog freshness: one row per recorded scope - the bundle-scope row (scope_kind ''bundle'', scope_key NULL) plus every concept/path/group override - combining state, reason codes, stale_since, the producer''s opaque revisions (observed_revision, indexed_revision) with the catalog generation the materialization covers (published_revision) and the bundle''s live catalog_generation, last_reconciled_at, and the embedding contract evidence. A scope with no override inherits its bundle''s row. Tenant-scoped like the projection tables; SELECT is granted to pgokf_reader while the raw tables stay writer-only.';
GRANT SELECT ON pgokf.effective_freshness TO pgokf_reader;

-- Capability/version declaration (Producer Contract): reader-safe, immutable,
-- a constant object so later phases add entries additively.
CREATE FUNCTION pgokf.capabilities() RETURNS jsonb
    LANGUAGE sql
    IMMUTABLE
    PARALLEL SAFE
    SET search_path = pg_catalog, pg_temp
    AS $fn$
        SELECT pg_catalog.jsonb_build_object(
            'catalog_generation', 1,
            'publication_fence', 1,
            'freshness_dependency', 1,
            'effective_freshness', 1,
            'catalog_change_event', 1,
            'search_freshness', 1,
            'embedding_freshness', 1,
            'typed_relationships', 1)
    $fn$;
REVOKE ALL ON FUNCTION pgokf.capabilities() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.capabilities() TO pgokf_reader;
COMMENT ON FUNCTION pgokf.capabilities() IS
    'The catalog capabilities this pgokf release implements, as a jsonb object of capability name to interface version: catalog_generation, publication_fence, freshness_dependency, effective_freshness, catalog_change_event, search_freshness, embedding_freshness, and typed_relationships (all version 1). Immutable; a producer declares the capabilities it requires and checks them here. Later releases only add entries or raise versions.';
",
    name = "effective_freshness_view",
    requires = ["freshness_tables"]
);

// ---------------------------------------------------------------------------
// Controlled writer/admin APIs.
// ---------------------------------------------------------------------------

/// Validate the opaque producer label every write API takes.
fn validate_producer(producer: &str) -> Result<(), CatalogError> {
    if producer.trim().is_empty() {
        return Err(CatalogError::invalid_parameter(
            "producer must not be empty",
            Path::new(""),
        ));
    }
    Ok(())
}

/// Validate a dependency source selector against the v1 grammar: an exact
/// bundle (empty value), an exact concept id, an exact path, or a path
/// prefix - all matched case-sensitively, with no glob/regex support.
fn validate_selector(selector_kind: &str, selector_value: &str) -> Result<(), CatalogError> {
    if !SELECTOR_KINDS.contains(&selector_kind) {
        return Err(CatalogError::invalid_parameter(
            format!(
                "selector_kind must be one of {}, got {selector_kind}",
                SELECTOR_KINDS.map(|kind| format!("'{kind}'")).join(", ")
            ),
            Path::new(""),
        ));
    }
    if selector_kind == "bundle" && !selector_value.is_empty() {
        return Err(CatalogError::invalid_parameter(
            "selector_value must be empty for a bundle selector",
            Path::new(""),
        ));
    }
    if selector_kind != "bundle" && selector_value.is_empty() {
        return Err(CatalogError::invalid_parameter(
            format!("selector_value must not be empty for a {selector_kind} selector"),
            Path::new(""),
        ));
    }
    Ok(())
}

/// Validate a dependency target scope: `bundle` (NULL key) or one of the
/// `pgokf.concept_freshness` scope kinds with a non-empty key.
fn validate_target_scope(scope_kind: &str, scope_key: Option<&str>) -> Result<(), CatalogError> {
    if !TARGET_SCOPE_KINDS.contains(&scope_kind) {
        return Err(CatalogError::invalid_parameter(
            format!(
                "target_scope_kind must be one of {}, got {scope_kind}",
                TARGET_SCOPE_KINDS
                    .map(|kind| format!("'{kind}'"))
                    .join(", ")
            ),
            Path::new(""),
        ));
    }
    if scope_kind == "bundle" && scope_key.is_some() {
        return Err(CatalogError::invalid_parameter(
            "target_scope_key must be NULL for a bundle target",
            Path::new(""),
        ));
    }
    if scope_kind != "bundle" && scope_key.is_none_or(str::is_empty) {
        return Err(CatalogError::invalid_parameter(
            format!("target_scope_key must not be empty for a {scope_kind} target"),
            Path::new(""),
        ));
    }
    Ok(())
}

/// Validate a caller-supplied reason-code set, defaulting an empty one.
fn validated_reasons(reason_codes: Vec<String>) -> Result<Vec<String>, CatalogError> {
    for reason in &reason_codes {
        if reason.trim().is_empty() {
            return Err(CatalogError::invalid_parameter(
                "reason_codes must not contain empty entries",
                Path::new(""),
            ));
        }
    }
    Ok(if reason_codes.is_empty() {
        vec!["producer_reported".to_owned()]
    } else {
        reason_codes
    })
}

/// Record a dependency registration/removal in the audit trail.
///
/// Reuses the `pgokf_private.sync_log` machinery (op `dependency_register` /
/// `dependency_remove`, counts/hash NULL) so the audit row carries the same
/// actor/timestamp/retention semantics as every other catalog-mutating
/// operation.
fn audit_dependency(
    target_bundle_id: i64,
    op: &str,
    dependency_id: i64,
) -> Result<(), CatalogError> {
    let retention_days = crate::catalog::config::sync_log_retention_days()?;
    let path = bundle_path(target_bundle_id)?;
    let _ = crate::catalog::audit::record(
        target_bundle_id,
        &format!("{path}#dependency:{dependency_id}"),
        op,
        None,
        None,
        retention_days,
        None,
    )?;
    Ok(())
}

/// Register a freshness dependency, returning its identity.
///
/// Authorization: writer tier. Both endpoints are tenant-confined
/// ([`security::enforce_bundle_tenant`]): an unknown or cross-tenant bundle on
/// either side raises the shared `22023` unknown-bundle error, so registration
/// can neither see nor write across tenants. The new dependency's watermark
/// starts at the source bundle's **current** catalog generation - registration
/// reconciles from the latest source generation, never from historical events.
///
/// The baseline is read under the source bundle's advisory lock (the same
/// lock every register/refresh/lifecycle mutation holds for its whole
/// transaction), so registration serializes against an in-flight source
/// change: the change either commits before the lock is granted (and the
/// baseline reads its generation) or evaluates after this registration
/// commits (and sees the new dependency). A registration can never slip a
/// stale watermark in between a committed change and its evaluation.
#[allow(clippy::too_many_arguments)]
fn register_dependency_impl(
    producer: &str,
    source_bundle_id: i64,
    selector_kind: &str,
    selector_value: &str,
    target_bundle_id: i64,
    target_scope_kind: &str,
    target_scope_key: Option<&str>,
    causation_key: Option<&str>,
) -> Result<i64, CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    validate_producer(producer)?;
    validate_selector(selector_kind, selector_value)?;
    validate_target_scope(target_scope_kind, target_scope_key)?;
    // Source visibility and target write authority: a foreign or absent id on
    // either side is indistinguishable from an unregistered bundle.
    security::enforce_bundle_tenant(source_bundle_id)?;
    security::enforce_bundle_tenant(target_bundle_id)?;

    // Serialize against an in-flight mutation of the source bundle before
    // reading its generation (see the fn-level note).
    let source_path = bundle_path(source_bundle_id)?;
    let key = advisory_lock_key(&source_path);
    Spi::run_with_args("SELECT pg_catalog.pg_advisory_xact_lock($1)", &[key.into()])
        .map_err(|error| spi_error("failed to acquire bundle advisory lock", &error))?;

    let baseline = Spi::get_one_with_args::<i64>(
        "SELECT catalog_generation FROM pgokf.bundles WHERE id = $1",
        &[source_bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to read source catalog generation", &error))?
    .ok_or_else(|| unknown_bundle_error(source_bundle_id))?;

    // connect_mut + next(): an ON CONFLICT DO NOTHING conflict returns zero
    // rows, and Spi::get_one errors on an empty result instead of returning
    // None, so the conflict (23505) is detected by position.
    let dependency_id = Spi::connect_mut(|client| {
        let mut table = client
            .update(
                "INSERT INTO pgokf.freshness_dependency
                     (tenant_id, producer, causation_key,
                      source_bundle_id, selector_kind, selector_value,
                      last_source_catalog_generation,
                      target_bundle_id, target_scope_kind, target_scope_key)
                 SELECT b.tenant_id, $1, $2, $3, $4, $5, $6, $7, $8, $9
                 FROM pgokf.bundles b
                 WHERE b.id = $3
                 ON CONFLICT ON CONSTRAINT freshness_dependency_uq DO NOTHING
                 RETURNING dependency_id",
                None,
                &[
                    producer.into(),
                    causation_key.into(),
                    source_bundle_id.into(),
                    selector_kind.into(),
                    selector_value.into(),
                    baseline.into(),
                    target_bundle_id.into(),
                    target_scope_kind.into(),
                    target_scope_key.into(),
                ],
            )
            .map_err(|error| spi_error("failed to register freshness dependency", &error))?;
        table
            .next()
            .map(|row| {
                RowReader::new(&row, "failed to read dependency id", "freshness dependency")
                    .required::<i64>(1, "dependency_id")
            })
            .transpose()
    })?
    .ok_or_else(|| {
        CatalogError::duplicate_path(
            "an identical freshness dependency is already registered \
             (same producer, source selector, and target)",
            Path::new(""),
        )
    })?;
    audit_dependency(target_bundle_id, "dependency_register", dependency_id)?;
    Ok(dependency_id)
}

/// Load one dependency's tenant identity, confined to the session's tenant.
///
/// Returns `None` when the dependency does not exist or belongs to another
/// tenant, so the two are indistinguishable (the same rule
/// [`security::enforce_bundle_tenant`] applies to bundles).
fn select_dependency_in_tenant(dependency_id: i64) -> Result<Option<i64>, CatalogError> {
    Spi::get_one_with_args::<i64>(
        "SELECT target_bundle_id
         FROM pgokf.freshness_dependency
         WHERE dependency_id = $1
           AND (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                 OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                AND NOT (SELECT pgokf.tenant_required()))
             OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))",
        &[dependency_id.into()],
    )
    .map_err(|error| spi_error("failed to look up freshness dependency", &error))
}

/// The shared `22023` for an unknown (or cross-tenant) dependency id.
fn unknown_dependency_error(dependency_id: i64) -> CatalogError {
    CatalogError::invalid_parameter(
        format!("freshness dependency {dependency_id} is not registered"),
        Path::new(""),
    )
}

/// Disable a dependency (kept, but no longer evaluated).
fn disable_dependency_impl(dependency_id: i64) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    select_dependency_in_tenant(dependency_id)?
        .ok_or_else(|| unknown_dependency_error(dependency_id))?;
    Spi::run_with_args(
        "UPDATE pgokf.freshness_dependency
         SET enabled = false, updated_at = pg_catalog.now()
         WHERE dependency_id = $1",
        &[dependency_id.into()],
    )
    .map_err(|error| spi_error("failed to disable freshness dependency", &error))
}

/// Remove a dependency entirely (audited).
fn remove_dependency_impl(dependency_id: i64) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    let target_bundle_id = select_dependency_in_tenant(dependency_id)?
        .ok_or_else(|| unknown_dependency_error(dependency_id))?;
    Spi::run_with_args(
        "DELETE FROM pgokf.freshness_dependency WHERE dependency_id = $1",
        &[dependency_id.into()],
    )
    .map_err(|error| spi_error("failed to remove freshness dependency", &error))?;
    audit_dependency(target_bundle_id, "dependency_remove", dependency_id)?;
    Ok(())
}

/// `mark_stale`: producer-reported staleness of a whole bundle.
fn mark_stale_impl(
    bundle_id: i64,
    reason_codes: Vec<String>,
    producer: Option<&str>,
    observed_source_generation: Option<&str>,
) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;
    let _ = bundle_path(bundle_id)?;
    let reasons = validated_reasons(reason_codes)?;
    let reason_refs: Vec<&str> = reasons.iter().map(String::as_str).collect();
    set_bundle_state(
        bundle_id,
        "stale",
        &reason_refs,
        producer,
        observed_source_generation,
    )
}

/// `mark_reconciling`: a reconciliation attempt owns the newest target; the
/// bundle remains effectively stale (`stale_since` is preserved).
///
/// The attempt also claims the row's standing dependency invalidation epoch:
/// the compare-and-set [`mark_fresh`](pgokf::mark_fresh) completes only for an
/// attempt whose claim covers the newest epoch, so a dependency invalidation
/// that lands after the claim refuses the completion until the producer
/// re-claims (observing the invalidation). The claim does not clear the
/// `relationship_coverage_missing` evidence column.
fn mark_reconciling_impl(bundle_id: i64, producer: Option<&str>) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;
    let _ = bundle_path(bundle_id)?;
    set_bundle_state(bundle_id, "reconciling", &[], producer, None)?;
    Spi::run_with_args(
        "UPDATE pgokf.bundle_freshness
         SET claimed_invalidation_epoch = dependency_invalidation_epoch
         WHERE bundle_id = $1",
        &[bundle_id.into()],
    )
    .map_err(|error| spi_error("failed to claim the dependency invalidation epoch", &error))
}

/// `mark_blocked`: a nonretryable failure; prior data stays labeled.
fn mark_blocked_impl(
    bundle_id: i64,
    reason_codes: Vec<String>,
    producer: Option<&str>,
) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;
    let _ = bundle_path(bundle_id)?;
    let reasons = validated_reasons(reason_codes)?;
    let reason_refs: Vec<&str> = reasons.iter().map(String::as_str).collect();
    set_bundle_state(bundle_id, "blocked", &reason_refs, producer, None)
}

/// The compare-and-set completion statement of [`mark_fresh_impl`]: refuses
/// (updates zero rows) unless the observed source revision, the live catalog
/// generation, and the materialized generation all still match the producer's
/// evidence, the bundle is not retired, the newest dependency invalidation
/// epoch has been claimed by a `mark_reconciling` after it landed (a
/// completion prepared before the invalidation can never erase it), and no
/// `relationship_coverage_missing` evidence stands - the dedicated column,
/// which no state transition erases, is authoritative; the reason-code check
/// is kept as defense in depth (a refresh that superseded the bundle's
/// relationship coverage without a matching replacement must be answered with
/// a new publication first - see [`crate::catalog::relationships`]).
const MARK_FRESH_CAS: &str = "UPDATE pgokf.bundle_freshness f
         SET state = 'fresh',
             reason_codes = '{}'::text[],
             stale_since = NULL,
             last_reconciled_at = pg_catalog.now(),
             materialized_source_generation = $2,
             materialized_catalog_generation = $3,
             manifest_hash = COALESCE($4, f.manifest_hash),
             embedding_contract = COALESCE($5, f.embedding_contract),
             producer = COALESCE($6, f.producer),
             updated_at = pg_catalog.now()
         FROM pgokf.bundles b
         WHERE f.bundle_id = $1
           AND b.id = f.bundle_id
           AND f.state <> 'retired'
           AND f.observed_source_generation IS NOT DISTINCT FROM $2
           AND b.catalog_generation = $3
           AND (f.materialized_catalog_generation IS NULL
                OR f.materialized_catalog_generation <= $3)
           AND f.dependency_invalidation_epoch <= f.claimed_invalidation_epoch
           AND f.relationship_coverage_missing_since IS NULL
           AND NOT (f.reason_codes @> ARRAY['relationship_coverage_missing']::text[])
         RETURNING f.bundle_id";

/// Compare-and-set `mark_fresh`: completes a reconciliation only when the
/// producer's evidence still matches the newest catalog state.
///
/// All of the following must hold, atomically, or the call refuses (returns
/// `false`) and changes nothing:
///
/// - the bundle's observed source revision still equals
///   `expected_observed_source_generation` (a newer observed input wins);
/// - the bundle's live `catalog_generation` equals
///   `expected_catalog_generation` (the reconciliation covers exactly the
///   current catalog state);
/// - the recorded materialized generation does not exceed the expected one
///   (an older attempt never clears a newer one);
/// - the newest dependency invalidation epoch was claimed by a
///   `mark_reconciling` after it landed (a completion based on evidence older
///   than the latest dependency invalidation is refused);
/// - no `relationship_coverage_missing` evidence stands (the producer must
///   re-establish relationship coverage first);
/// - the bundle is not retired.
///
/// The check runs under the bundle advisory lock - the same lock every
/// register/refresh/lifecycle mutation holds for its whole transaction - so
/// the completion never certifies a generation or epoch older than a
/// committed mutation it would otherwise have waited behind on the row lock.
///
/// On success the bundle becomes `fresh`: reasons cleared, `stale_since`
/// cleared, `last_reconciled_at` set, and the manifest/embedding-contract
/// evidence recorded.
fn mark_fresh_impl(
    bundle_id: i64,
    expected_observed_source_generation: Option<&str>,
    expected_catalog_generation: i64,
    manifest_hash: Option<&str>,
    embedding_contract: Option<pgrx::JsonB>,
    producer: Option<&str>,
) -> Result<bool, CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;
    let stored_path = bundle_path(bundle_id)?;
    // Serialize with a concurrent mutation of this bundle: the completion
    // reads the post-mutation committed state rather than a pre-wait
    // snapshot's row predicate.
    let key = advisory_lock_key(&stored_path);
    Spi::run_with_args("SELECT pg_catalog.pg_advisory_xact_lock($1)", &[key.into()])
        .map_err(|error| spi_error("failed to acquire bundle advisory lock", &error))?;
    Spi::connect_mut(|client| {
        let mut table = client
            .update(
                MARK_FRESH_CAS,
                None,
                &[
                    bundle_id.into(),
                    expected_observed_source_generation.into(),
                    expected_catalog_generation.into(),
                    manifest_hash.into(),
                    embedding_contract.into(),
                    producer.into(),
                ],
            )
            .map_err(|error| spi_error("failed to compare-and-set bundle freshness", &error))?;
        Ok(table.next().is_some())
    })
}

/// `mark_scope_stale`: producer-reported staleness of one scope within a
/// bundle (`concept`, `path`, or `group` override).
fn mark_scope_stale_impl(
    bundle_id: i64,
    scope_kind: &str,
    scope_key: &str,
    reason_codes: Vec<String>,
    producer: Option<&str>,
) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;
    let _ = bundle_path(bundle_id)?;
    if !SCOPE_KINDS.contains(&scope_kind) {
        return Err(CatalogError::invalid_parameter(
            format!(
                "scope_kind must be one of {}, got {scope_kind}",
                SCOPE_KINDS.map(|kind| format!("'{kind}'")).join(", ")
            ),
            Path::new(""),
        ));
    }
    if scope_key.is_empty() {
        return Err(CatalogError::invalid_parameter(
            "scope_key must not be empty",
            Path::new(""),
        ));
    }
    let reasons = validated_reasons(reason_codes)?;
    let reason_refs: Vec<&str> = reasons.iter().map(String::as_str).collect();
    set_scope_state(
        bundle_id,
        scope_kind,
        scope_key,
        "stale",
        &reason_refs,
        producer,
    )
}

/// Remove a scope override, returning the scope to the bundle's state.
fn clear_freshness_scope_impl(
    bundle_id: i64,
    scope_kind: &str,
    scope_key: &str,
) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;
    let _ = bundle_path(bundle_id)?;
    // connect_mut + next(): an absent override deletes zero rows, and
    // Spi::get_one errors on an empty result instead of None.
    let removed = Spi::connect_mut(|client| {
        let mut table = client
            .update(
                "DELETE FROM pgokf.concept_freshness
                 WHERE bundle_id = $1 AND scope_kind = $2 AND scope_key = $3
                 RETURNING bundle_id",
                None,
                &[bundle_id.into(), scope_kind.into(), scope_key.into()],
            )
            .map_err(|error| spi_error("failed to clear freshness scope", &error))?;
        Ok(table.next().is_some())
    })?;
    if !removed {
        return Err(CatalogError::invalid_parameter(
            format!("bundle {bundle_id} has no {scope_kind} freshness override for {scope_key}"),
            Path::new(""),
        ));
    }
    Ok(())
}

/// Admin repair: set a bundle's freshness state directly.
///
/// An operator escape hatch (the "repair" operation): it replaces the state
/// and reason codes exactly as given and records the transition. It cannot
/// establish *producer* currency evidence - the generation/revision columns
/// are left untouched - so a repaired `fresh` row still carries whatever
/// materialization evidence it had. The repair also settles the dependency
/// invalidation epoch pair (the standing epoch becomes claimed, so a
/// pre-repair invalidation does not block later completions) and resets the
/// `relationship_coverage_missing` evidence column consistently with the
/// given reason set (kept - with its original instant - when the reason is
/// repaired in, cleared otherwise).
fn repair_bundle_freshness_impl(
    bundle_id: i64,
    state: &str,
    reason_codes: Vec<String>,
) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;
    let _ = bundle_path(bundle_id)?;
    if !STATES.contains(&state) {
        return Err(CatalogError::invalid_parameter(
            format!(
                "state must be one of {}, got {state}",
                STATES.map(|s| format!("'{s}'")).join(", ")
            ),
            Path::new(""),
        ));
    }
    let reasons = validated_reasons(reason_codes)?;
    Spi::run_with_args(
        "INSERT INTO pgokf.bundle_freshness
             (bundle_id, tenant_id, state, reason_codes, stale_since, last_reconciled_at,
              relationship_coverage_missing_since)
         SELECT $1, b.tenant_id, $2, $3::text[],
                CASE WHEN $2 = 'fresh' THEN NULL ELSE pg_catalog.now() END,
                CASE WHEN $2 = 'fresh' THEN pg_catalog.now() ELSE NULL END,
                CASE WHEN $3::text[] @> ARRAY['relationship_coverage_missing']::text[]
                     THEN pg_catalog.now() ELSE NULL END
         FROM pgokf.bundles b
         WHERE b.id = $1
         ON CONFLICT (bundle_id) DO UPDATE SET
             state = $2,
             reason_codes = $3::text[],
             stale_since = CASE WHEN $2 = 'fresh' THEN NULL ELSE pg_catalog.now() END,
             last_reconciled_at = CASE WHEN $2 = 'fresh'
                 THEN pg_catalog.now() ELSE pgokf.bundle_freshness.last_reconciled_at END,
             claimed_invalidation_epoch = pgokf.bundle_freshness.dependency_invalidation_epoch,
             relationship_coverage_missing_since = CASE
                 WHEN $3::text[] @> ARRAY['relationship_coverage_missing']::text[]
                     THEN COALESCE(pgokf.bundle_freshness.relationship_coverage_missing_since,
                                   pg_catalog.now())
                 ELSE NULL END,
             updated_at = pg_catalog.now()",
        &[bundle_id.into(), state.into(), reasons.into()],
    )
    .map_err(|error| spi_error("failed to repair bundle freshness", &error))
}

/// One dependency row projected onto the `pgokf.freshness_dependency_info`
/// shape.
struct DependencyInfo {
    dependency_id: i64,
    tenant_id: String,
    producer: String,
    enabled: bool,
    created_by: String,
    created_at: TimestampWithTimeZone,
    updated_at: TimestampWithTimeZone,
    origin: Option<String>,
    causation_key: Option<String>,
    source_bundle_id: i64,
    selector_kind: String,
    selector_value: String,
    last_source_catalog_generation: i64,
    last_event_id: Option<i64>,
    target_bundle_id: i64,
    target_scope_kind: String,
    target_scope_key: Option<String>,
    reconciliation_watermark: Option<String>,
}

/// Column projection shared by the dependency list read, in the attribute
/// order of [`read_dependency_info`].
const DEPENDENCY_INFO_COLUMNS: &str = "dependency_id, tenant_id, producer, enabled, created_by,
    created_at, updated_at, origin, causation_key, source_bundle_id, selector_kind,
    selector_value, last_source_catalog_generation, last_event_id, target_bundle_id,
    target_scope_kind, target_scope_key, reconciliation_watermark";

fn read_dependency_info(row: &SpiHeapTupleData<'_>) -> Result<DependencyInfo, CatalogError> {
    let reader = RowReader::new(
        row,
        "failed to read freshness_dependency_info column",
        "freshness dependency",
    );
    Ok(DependencyInfo {
        dependency_id: reader.required(1, "dependency_id")?,
        tenant_id: reader.required(2, "tenant_id")?,
        producer: reader.required(3, "producer")?,
        enabled: reader.required(4, "enabled")?,
        created_by: reader.required(5, "created_by")?,
        created_at: reader.required(6, "created_at")?,
        updated_at: reader.required(7, "updated_at")?,
        origin: reader.optional(8)?,
        causation_key: reader.optional(9)?,
        source_bundle_id: reader.required(10, "source_bundle_id")?,
        selector_kind: reader.required(11, "selector_kind")?,
        selector_value: reader.required(12, "selector_value")?,
        last_source_catalog_generation: reader.required(13, "last_source_catalog_generation")?,
        last_event_id: reader.optional(14)?,
        target_bundle_id: reader.required(15, "target_bundle_id")?,
        target_scope_kind: reader.required(16, "target_scope_kind")?,
        target_scope_key: reader.optional(17)?,
        reconciliation_watermark: reader.optional(18)?,
    })
}

fn dependency_info_tuple(
    info: &DependencyInfo,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type("pgokf.freshness_dependency_info")
        .map_err(composite_error)?;
    tuple
        .set_by_name("dependency_id", info.dependency_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("tenant_id", info.tenant_id.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("producer", info.producer.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("enabled", info.enabled)
        .map_err(composite_error)?;
    tuple
        .set_by_name("created_by", info.created_by.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("created_at", info.created_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("updated_at", info.updated_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("origin", info.origin.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("causation_key", info.causation_key.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("source_bundle_id", info.source_bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("selector_kind", info.selector_kind.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("selector_value", info.selector_value.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name(
            "last_source_catalog_generation",
            info.last_source_catalog_generation,
        )
        .map_err(composite_error)?;
    tuple
        .set_by_name("last_event_id", info.last_event_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("target_bundle_id", info.target_bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("target_scope_kind", info.target_scope_kind.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("target_scope_key", info.target_scope_key.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name(
            "reconciliation_watermark",
            info.reconciliation_watermark.clone(),
        )
        .map_err(composite_error)?;
    Ok(tuple)
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build freshness composite: {error}"),
        Path::new(""),
    )
}

/// Admin list-all over the dependency registry, tenant-scoped.
fn list_dependencies_impl(max_rows: i32) -> Result<Vec<DependencyInfo>, CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;
    if !(0..=10_000).contains(&max_rows) {
        return Err(CatalogError::invalid_parameter(
            format!("max_rows must be between 0 and 10000, got {max_rows}"),
            Path::new(""),
        ));
    }
    let query = format!(
        "SELECT {DEPENDENCY_INFO_COLUMNS}
         FROM pgokf.freshness_dependency
         WHERE (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                 OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                AND NOT (SELECT pgokf.tenant_required()))
             OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
         ORDER BY dependency_id
         LIMIT $1"
    );
    Spi::connect(|client| {
        let table = client
            .select(query.as_str(), None, &[i64::from(max_rows).into()])
            .map_err(|error| spi_error("failed to list freshness dependencies", &error))?;
        let mut infos = Vec::with_capacity(table.len());
        for row in table {
            infos.push(read_dependency_info(&row)?);
        }
        Ok(infos)
    })
}

/// One `pgokf.publication_fence` slot projected onto the
/// `pgokf.publication_fence_info` shape.
struct PublicationFence {
    tenant_id: String,
    producer: String,
    bundle_id: i64,
    target_generation: i64,
    fencing_token: i64,
    expected_catalog_generation: i64,
    manifest_hash: Option<String>,
    state: String,
    issued_at: TimestampWithTimeZone,
    expires_at: TimestampWithTimeZone,
}

fn fence_tuple(
    fence: &PublicationFence,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple =
        PgHeapTuple::new_composite_type("pgokf.publication_fence_info").map_err(composite_error)?;
    tuple
        .set_by_name("tenant_id", fence.tenant_id.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("producer", fence.producer.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("bundle_id", fence.bundle_id)
        .map_err(composite_error)?;
    tuple
        .set_by_name("target_generation", fence.target_generation)
        .map_err(composite_error)?;
    tuple
        .set_by_name("fencing_token", fence.fencing_token)
        .map_err(composite_error)?;
    tuple
        .set_by_name(
            "expected_catalog_generation",
            fence.expected_catalog_generation,
        )
        .map_err(composite_error)?;
    tuple
        .set_by_name("manifest_hash", fence.manifest_hash.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("state", fence.state.clone())
        .map_err(composite_error)?;
    tuple
        .set_by_name("issued_at", fence.issued_at)
        .map_err(composite_error)?;
    tuple
        .set_by_name("expires_at", fence.expires_at)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// Read one `publication_fence` row in the column order of the issuance
/// `RETURNING` list.
fn read_fence(row: &SpiHeapTupleData<'_>) -> Result<PublicationFence, CatalogError> {
    let reader = RowReader::new(row, "failed to read issued fence", "publication fence");
    Ok(PublicationFence {
        tenant_id: reader.required(1, "tenant_id")?,
        producer: reader.required(2, "producer")?,
        bundle_id: reader.required(3, "bundle_id")?,
        target_generation: reader.required(4, "target_generation")?,
        fencing_token: reader.required(5, "fencing_token")?,
        expected_catalog_generation: reader.required(6, "expected_catalog_generation")?,
        manifest_hash: reader.optional(7)?,
        state: reader.required(8, "state")?,
        issued_at: reader.required(9, "issued_at")?,
        expires_at: reader.required(10, "expires_at")?,
    })
}

/// Insert or supersede the fence slot, handing out the next `fencing_token`,
/// and return the issued row. The monotonic-target check runs before this in
/// [`issue_fence_impl`]; the upsert runs under the bundle advisory lock.
#[allow(clippy::too_many_arguments)]
fn upsert_fence(
    tenant_id: &str,
    producer: &str,
    bundle_id: i64,
    target_generation: i64,
    expected_catalog_generation: i64,
    manifest_hash: Option<&str>,
    lease_seconds: i32,
) -> Result<PublicationFence, CatalogError> {
    Spi::connect_mut(|client| {
        let mut table = client
            .update(
                "INSERT INTO pgokf.publication_fence
                     (tenant_id, producer, bundle_id, target_generation, fencing_token,
                      expected_catalog_generation, manifest_hash, state, issued_at, expires_at)
                 VALUES ($1, $2, $3, $4, 1, $5, $6, 'issued', pg_catalog.now(),
                         pg_catalog.now() + pg_catalog.make_interval(secs => $7))
                 ON CONFLICT (tenant_id, producer, bundle_id) DO UPDATE SET
                     target_generation = $4,
                     fencing_token = pgokf.publication_fence.fencing_token + 1,
                     expected_catalog_generation = $5,
                     manifest_hash = $6,
                     state = 'issued',
                     issued_at = pg_catalog.now(),
                     expires_at = pg_catalog.now() + pg_catalog.make_interval(secs => $7),
                     updated_at = pg_catalog.now()
                 RETURNING tenant_id, producer, bundle_id, target_generation, fencing_token,
                           expected_catalog_generation, manifest_hash, state, issued_at,
                           expires_at",
                None,
                &[
                    tenant_id.into(),
                    producer.into(),
                    bundle_id.into(),
                    target_generation.into(),
                    expected_catalog_generation.into(),
                    manifest_hash.into(),
                    lease_seconds.into(),
                ],
            )
            .map_err(|error| spi_error("failed to issue publication fence", &error))?;
        let Some(row) = table.next() else {
            return Err(CatalogError::internal(
                "publication fence issuance returned no row",
                Path::new(""),
            ));
        };
        read_fence(&row)
    })
}

/// Issue (or supersede) the publication fence of `(tenant, producer, bundle)`.
///
/// Compare-and-set under the bundle advisory lock: the producer's
/// `expected_catalog_generation` must equal the bundle's current catalog
/// generation, and `target_generation` must advance past the slot's current
/// target, or issuance is refused with SQLSTATE `22023`. Each issuance hands
/// out the next `fencing_token` for the slot, invalidating every earlier one.
#[allow(clippy::too_many_arguments)]
fn issue_fence_impl(
    bundle_id: i64,
    producer: &str,
    target_generation: i64,
    expected_catalog_generation: i64,
    manifest_hash: Option<&str>,
    lease_seconds: i32,
) -> Result<PublicationFence, CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    validate_producer(producer)?;
    if target_generation <= 0 {
        return Err(CatalogError::invalid_parameter(
            format!("target_generation must be greater than 0, got {target_generation}"),
            Path::new(""),
        ));
    }
    if !(1..=86_400).contains(&lease_seconds) {
        return Err(CatalogError::invalid_parameter(
            format!("lease_seconds must be between 1 and 86400, got {lease_seconds}"),
            Path::new(""),
        ));
    }
    security::enforce_bundle_tenant(bundle_id)?;
    let stored_path = bundle_path(bundle_id)?;
    let key = advisory_lock_key(&stored_path);
    Spi::run_with_args("SELECT pg_catalog.pg_advisory_xact_lock($1)", &[key.into()])
        .map_err(|error| spi_error("failed to acquire bundle advisory lock", &error))?;

    let (tenant_id, catalog_generation) = Spi::connect(|client| {
        let table = client
            .select(
                "SELECT tenant_id, catalog_generation FROM pgokf.bundles WHERE id = $1",
                Some(1),
                &[bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to read bundle generation", &error))?;
        let Some(row) = table.into_iter().next() else {
            return Err(unknown_bundle_error(bundle_id));
        };
        let reader = RowReader::new(&row, "failed to read bundle row", "bundle");
        Ok((
            reader.required::<String>(1, "tenant_id")?,
            reader.required::<i64>(2, "catalog_generation")?,
        ))
    })?;
    if catalog_generation != expected_catalog_generation {
        return Err(CatalogError::invalid_parameter(
            format!(
                "expected_catalog_generation {expected_catalog_generation} does not match the \
                 bundle's current catalog generation {catalog_generation}; re-read the catalog \
                 and re-issue"
            ),
            Path::new(""),
        ));
    }

    let current_target = Spi::connect(|client| {
        // connect + next(): a first issuance finds zero rows, and
        // Spi::get_one errors on an empty result instead of returning None.
        let mut table = client
            .select(
                "SELECT target_generation FROM pgokf.publication_fence
                 WHERE tenant_id = $1 AND producer = $2 AND bundle_id = $3",
                Some(1),
                &[tenant_id.clone().into(), producer.into(), bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to read publication fence", &error))?;
        table
            .next()
            .map(|row| {
                RowReader::new(
                    &row,
                    "failed to read publication fence",
                    "publication fence",
                )
                .required::<i64>(1, "target_generation")
            })
            .transpose()
    })?;
    if let Some(current) = current_target
        && target_generation <= current
    {
        return Err(CatalogError::invalid_parameter(
            format!(
                "target_generation {target_generation} does not advance past the fence's current \
                 target {current}; only the newest producer attempt may hold the fence"
            ),
            Path::new(""),
        ));
    }

    upsert_fence(
        &tenant_id,
        producer,
        bundle_id,
        target_generation,
        expected_catalog_generation,
        manifest_hash,
        lease_seconds,
    )
}

/// Release a live fence: only the live `fencing_token` releases the slot, so a
/// superseded or expired attempt fails loudly (SQLSTATE `22023`) instead of
/// silently completing.
fn release_fence_impl(
    bundle_id: i64,
    producer: &str,
    fencing_token: i64,
) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    validate_producer(producer)?;
    security::enforce_bundle_tenant(bundle_id)?;
    let stored_path = bundle_path(bundle_id)?;
    let key = advisory_lock_key(&stored_path);
    Spi::run_with_args("SELECT pg_catalog.pg_advisory_xact_lock($1)", &[key.into()])
        .map_err(|error| spi_error("failed to acquire bundle advisory lock", &error))?;

    // connect_mut + next(): a wrong/expired/superseded token updates zero
    // rows, and Spi::get_one errors on an empty result instead of None.
    let released = Spi::connect_mut(|client| {
        let mut table = client
            .update(
                "UPDATE pgokf.publication_fence f
                 SET state = 'released', updated_at = pg_catalog.now()
                 WHERE f.bundle_id = $3
                   AND f.producer = $2
                   AND f.tenant_id = (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $3)
                   AND f.fencing_token = $1
                   AND f.state = 'issued'
                   AND f.expires_at > pg_catalog.now()
                 RETURNING f.fencing_token",
                None,
                &[fencing_token.into(), producer.into(), bundle_id.into()],
            )
            .map_err(|error| spi_error("failed to release publication fence", &error))?;
        Ok(table.next().is_some())
    })?;
    if !released {
        return Err(CatalogError::invalid_parameter(
            format!(
                "no live publication fence for producer {producer} on bundle {bundle_id} with \
                 fencing_token {fencing_token} (the token is superseded, expired, or never issued)"
            ),
            Path::new(""),
        ));
    }
    Ok(())
}

/// SQL-facing freshness/fence API, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{
        clear_freshness_scope_impl, dependency_info_tuple, disable_dependency_impl, fence_tuple,
        issue_fence_impl, list_dependencies_impl, mark_blocked_impl, mark_fresh_impl,
        mark_reconciling_impl, mark_scope_stale_impl, mark_stale_impl, register_dependency_impl,
        release_fence_impl, remove_dependency_impl, repair_bundle_freshness_impl,
    };

    extension_sql!(
        r"
CREATE TYPE pgokf.freshness_dependency_info AS (
    dependency_id     bigint,
    tenant_id         text,
    producer          text,
    enabled           boolean,
    created_by        text,
    created_at        timestamptz,
    updated_at        timestamptz,
    origin            text,
    causation_key     text,
    source_bundle_id  bigint,
    selector_kind     text,
    selector_value    text,
    last_source_catalog_generation bigint,
    last_event_id     bigint,
    target_bundle_id  bigint,
    target_scope_kind text,
    target_scope_key  text,
    reconciliation_watermark text
);

COMMENT ON TYPE pgokf.freshness_dependency_info IS
    'One registered freshness dependency as pgokf.list_freshness_dependencies reports it: producer label, source selector, target scope, enabled flag, evaluation watermark, and audit provenance.';

CREATE TYPE pgokf.publication_fence_info AS (
    tenant_id         text,
    producer          text,
    bundle_id         bigint,
    target_generation bigint,
    fencing_token     bigint,
    expected_catalog_generation bigint,
    manifest_hash     text,
    state             text,
    issued_at         timestamptz,
    expires_at        timestamptz
);

COMMENT ON TYPE pgokf.publication_fence_info IS
    'One publication fence slot as pgokf.issue_publication_fence returns it: the slot key, the monotonic target and catalog-assigned fencing token, the expected catalog generation it was CAS-issued against, and its lease expiry.';
",
        name = "freshness_types",
        requires = ["freshness_tables"]
    );

    /// Register a freshness dependency, returning its identity.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). The selector grammar is exact and case-sensitive:
    /// `selector_kind` is `bundle` (empty `selector_value`), `concept` (exact
    /// concept id), `path` (exact bundle-relative path), or `path_prefix`;
    /// `target_scope_kind` is `bundle` (NULL `target_scope_key`), `concept`,
    /// `path`, or `group` (the defaulted arguments follow the required ones).
    /// Both bundles must belong to the session's tenant
    /// (an unknown or cross-tenant id raises SQLSTATE `22023`). The new
    /// dependency starts from the source bundle's current catalog generation.
    /// `producer` is an opaque label, not authorization.
    #[pg_extern(requires = ["freshness_tables"])]
    #[allow(clippy::too_many_arguments)]
    fn register_freshness_dependency(
        producer: &str,
        source_bundle_id: i64,
        selector_kind: &str,
        target_bundle_id: i64,
        selector_value: default!(&str, "''"),
        target_scope_kind: default!(&str, "'bundle'"),
        target_scope_key: default!(Option<&str>, "NULL"),
        causation_key: default!(Option<&str>, "NULL"),
    ) -> i64 {
        register_dependency_impl(
            producer,
            source_bundle_id,
            selector_kind,
            selector_value,
            target_bundle_id,
            target_scope_kind,
            target_scope_key,
            causation_key,
        )
        .unwrap_or_else(|error| error.raise())
    }

    /// Disable a freshness dependency (it stays registered but is no longer
    /// evaluated). Requires `pgokf_writer`.
    #[pg_extern(requires = ["freshness_tables"])]
    fn disable_freshness_dependency(dependency_id: i64) {
        disable_dependency_impl(dependency_id).unwrap_or_else(|error| error.raise());
    }

    /// Remove a freshness dependency entirely (audited). Requires
    /// `pgokf_writer`.
    #[pg_extern(requires = ["freshness_tables"])]
    fn remove_freshness_dependency(dependency_id: i64) {
        remove_dependency_impl(dependency_id).unwrap_or_else(|error| error.raise());
    }

    /// Mark a bundle stale (producer-reported). Requires `pgokf_writer`.
    #[pg_extern(requires = ["freshness_tables"])]
    fn mark_stale(
        bundle_id: i64,
        reason_codes: default!(Vec<String>, "'{}'"),
        producer: default!(Option<&str>, "NULL"),
        observed_source_generation: default!(Option<&str>, "NULL"),
    ) {
        mark_stale_impl(
            bundle_id,
            reason_codes,
            producer,
            observed_source_generation,
        )
        .unwrap_or_else(|error| error.raise());
    }

    /// Mark a bundle reconciling (a reconciliation attempt owns the newest
    /// target and claims the standing dependency invalidation epoch; the
    /// bundle remains effectively stale). Requires `pgokf_writer`.
    #[pg_extern(requires = ["freshness_tables"])]
    fn mark_reconciling(bundle_id: i64, producer: default!(Option<&str>, "NULL")) {
        mark_reconciling_impl(bundle_id, producer).unwrap_or_else(|error| error.raise());
    }

    /// Mark a bundle blocked (a nonretryable failure; prior data stays
    /// labeled). Requires `pgokf_writer`.
    #[pg_extern(requires = ["freshness_tables"])]
    fn mark_blocked(
        bundle_id: i64,
        reason_codes: default!(Vec<String>, "'{}'"),
        producer: default!(Option<&str>, "NULL"),
    ) {
        mark_blocked_impl(bundle_id, reason_codes, producer).unwrap_or_else(|error| error.raise());
    }

    /// Compare-and-set completion: mark a bundle fresh only if the producer's
    /// evidence still matches the newest catalog state.
    ///
    /// Requires `pgokf_writer`. Returns `false` (changing nothing) when the
    /// bundle's observed source revision has advanced past
    /// `expected_observed_source_generation`, when its live catalog generation
    /// differs from `expected_catalog_generation`, when a newer materialized
    /// generation already exists, when the newest dependency invalidation
    /// epoch was not claimed by a `mark_reconciling` after it landed, when
    /// `relationship_coverage_missing` evidence stands, or when the bundle is
    /// retired. The check runs under the bundle advisory lock.
    #[pg_extern(requires = ["freshness_tables"])]
    fn mark_fresh(
        bundle_id: i64,
        expected_catalog_generation: i64,
        expected_observed_source_generation: default!(Option<&str>, "NULL"),
        manifest_hash: default!(Option<&str>, "NULL"),
        embedding_contract: default!(Option<pgrx::JsonB>, "NULL"),
        producer: default!(Option<&str>, "NULL"),
    ) -> bool {
        mark_fresh_impl(
            bundle_id,
            expected_observed_source_generation,
            expected_catalog_generation,
            manifest_hash,
            embedding_contract,
            producer,
        )
        .unwrap_or_else(|error| error.raise())
    }

    /// Mark one scope within a bundle stale (`concept`, `path`, or `group`
    /// override). Requires `pgokf_writer`.
    #[pg_extern(requires = ["freshness_tables"])]
    fn mark_scope_stale(
        bundle_id: i64,
        scope_kind: &str,
        scope_key: &str,
        reason_codes: default!(Vec<String>, "'{}'"),
        producer: default!(Option<&str>, "NULL"),
    ) {
        mark_scope_stale_impl(bundle_id, scope_kind, scope_key, reason_codes, producer)
            .unwrap_or_else(|error| error.raise());
    }

    /// Remove a scope override, returning the scope to the bundle's state.
    /// Requires `pgokf_writer`.
    #[pg_extern(requires = ["freshness_tables"])]
    fn clear_freshness_scope(bundle_id: i64, scope_kind: &str, scope_key: &str) {
        clear_freshness_scope_impl(bundle_id, scope_kind, scope_key)
            .unwrap_or_else(|error| error.raise());
    }

    /// List every registered freshness dependency (admin repair/list-all
    /// surface). Requires `pgokf_admin`; tenant-scoped.
    #[pg_extern(requires = ["freshness_types"])]
    fn list_freshness_dependencies(
        max_rows: default!(i32, 100),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.freshness_dependency_info")>
    {
        let infos = list_dependencies_impl(max_rows).unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = infos
            .iter()
            .map(|info| dependency_info_tuple(info).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    /// Admin repair: set a bundle's freshness state and reason codes directly.
    /// Requires `pgokf_admin`.
    #[pg_extern(requires = ["freshness_tables"])]
    fn repair_bundle_freshness(
        bundle_id: i64,
        state: &str,
        reason_codes: default!(Vec<String>, "'{}'"),
    ) {
        repair_bundle_freshness_impl(bundle_id, state, reason_codes)
            .unwrap_or_else(|error| error.raise());
    }

    /// Issue the publication fence for `(tenant, producer, bundle)`.
    ///
    /// Requires `pgokf_writer`. Compare-and-set under the bundle advisory
    /// lock: `expected_catalog_generation` must equal the bundle's current
    /// catalog generation and `target_generation` must advance past the slot's
    /// current target (SQLSTATE `22023` otherwise). Each issuance hands out
    /// the next `fencing_token`.
    #[pg_extern(requires = ["freshness_types"])]
    fn issue_publication_fence(
        bundle_id: i64,
        producer: &str,
        target_generation: i64,
        expected_catalog_generation: i64,
        manifest_hash: default!(Option<&str>, "NULL"),
        lease_seconds: default!(i32, 300),
    ) -> pgrx::composite_type!('static, "pgokf.publication_fence_info") {
        let fence = issue_fence_impl(
            bundle_id,
            producer,
            target_generation,
            expected_catalog_generation,
            manifest_hash,
            lease_seconds,
        )
        .unwrap_or_else(|error| error.raise());
        fence_tuple(&fence).unwrap_or_else(|error| error.raise())
    }

    /// Release a live publication fence; only the live `fencing_token`
    /// releases it (SQLSTATE `22023` otherwise). Requires `pgokf_writer`.
    #[pg_extern(requires = ["freshness_tables"])]
    fn release_publication_fence(bundle_id: i64, producer: &str, fencing_token: i64) {
        release_fence_impl(bundle_id, producer, fencing_token)
            .unwrap_or_else(|error| error.raise());
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.register_freshness_dependency(text, bigint, text, bigint, text, text, text, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.disable_freshness_dependency(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.remove_freshness_dependency(bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.mark_stale(bigint, text[], text, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.mark_reconciling(bigint, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.mark_blocked(bigint, text[], text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.mark_fresh(bigint, bigint, text, text, jsonb, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.mark_scope_stale(bigint, text, text, text[], text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.clear_freshness_scope(bigint, text, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.list_freshness_dependencies(integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.repair_bundle_freshness(bigint, text, text[])
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.issue_publication_fence(bigint, text, bigint, bigint, text, integer)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.release_publication_fence(bigint, text, bigint)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;

REVOKE ALL ON FUNCTION pgokf.register_freshness_dependency(text, bigint, text, bigint, text, text, text, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.disable_freshness_dependency(bigint) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.remove_freshness_dependency(bigint) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.mark_stale(bigint, text[], text, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.mark_reconciling(bigint, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.mark_blocked(bigint, text[], text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.mark_fresh(bigint, bigint, text, text, jsonb, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.mark_scope_stale(bigint, text, text, text[], text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.clear_freshness_scope(bigint, text, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.list_freshness_dependencies(integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.repair_bundle_freshness(bigint, text, text[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.issue_publication_fence(bigint, text, bigint, bigint, text, integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.release_publication_fence(bigint, text, bigint) FROM PUBLIC;

GRANT EXECUTE ON FUNCTION pgokf.register_freshness_dependency(text, bigint, text, bigint, text, text, text, text) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.disable_freshness_dependency(bigint) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.remove_freshness_dependency(bigint) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.mark_stale(bigint, text[], text, text) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.mark_reconciling(bigint, text) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.mark_blocked(bigint, text[], text) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.mark_fresh(bigint, bigint, text, text, jsonb, text) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.mark_scope_stale(bigint, text, text, text[], text) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.clear_freshness_scope(bigint, text, text) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.list_freshness_dependencies(integer) TO pgokf_admin;
GRANT EXECUTE ON FUNCTION pgokf.repair_bundle_freshness(bigint, text, text[]) TO pgokf_admin;
GRANT EXECUTE ON FUNCTION pgokf.issue_publication_fence(bigint, text, bigint, bigint, text, integer) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.release_publication_fence(bigint, text, bigint) TO pgokf_writer;

COMMENT ON FUNCTION pgokf.register_freshness_dependency(text, bigint, text, bigint, text, text, text, text) IS
    'Register a freshness dependency (source selector -> target bundle/scope) and return its identity. Writer-tier (pgokf_writer; admin inherits). Selector grammar, exact and case-sensitive (no glob/regex in v1): selector_kind bundle (empty selector_value), concept (exact concept id), path (exact bundle-relative path), or path_prefix; target_scope_kind bundle (NULL key), concept, path, or group. Both bundles must belong to the session''s tenant (22023 otherwise); producer is an opaque label, not authorization. The dependency starts from the source bundle''s current catalog generation (read under the source bundle''s advisory lock, so registration serializes against an in-flight source change) and is evaluated in the same transaction as every later catalog change to the source; registration is audited. Raises 23505 for an identical existing registration.';
COMMENT ON FUNCTION pgokf.disable_freshness_dependency(bigint) IS
    'Disable a registered freshness dependency (kept but no longer evaluated). Writer-tier; raises 22023 for an unknown or cross-tenant dependency id.';
COMMENT ON FUNCTION pgokf.remove_freshness_dependency(bigint) IS
    'Remove a freshness dependency entirely (audited). Writer-tier; raises 22023 for an unknown or cross-tenant dependency id.';
COMMENT ON FUNCTION pgokf.mark_stale(bigint, text[], text, text) IS
    'Mark a bundle stale with machine-readable reason codes (default ''{producer_reported}''), optionally advancing the observed source revision (opaque text). Writer-tier; tenant-confined (22023 for an unknown or cross-tenant bundle). External-source observations must call this before the producer acknowledges the observation or queues work.';
COMMENT ON FUNCTION pgokf.mark_reconciling(bigint, text) IS
    'Mark a bundle reconciling: a reconciliation attempt owns the newest target and claims the standing dependency invalidation epoch (pgokf.mark_fresh completes only for an attempt whose claim covers the newest epoch, so a dependency invalidation landing after the claim refuses the completion until the producer re-claims). The bundle remains effectively stale (stale_since is preserved); only the compare-and-set pgokf.mark_fresh clears it. The claim never clears the relationship_coverage_missing evidence. Writer-tier; tenant-confined.';
COMMENT ON FUNCTION pgokf.mark_blocked(bigint, text[], text) IS
    'Mark a bundle blocked (a nonretryable failure) with reason codes; the prior data stays available, labeled stale/blocked. Writer-tier; tenant-confined.';
COMMENT ON FUNCTION pgokf.mark_fresh(bigint, bigint, text, text, jsonb, text) IS
    'Compare-and-set reconciliation completion: mark the bundle fresh only if its observed source revision still equals expected_observed_source_generation AND its live catalog generation equals expected_catalog_generation AND no newer materialized generation exists AND the newest dependency invalidation epoch was claimed by pgokf.mark_reconciling after it landed (a completion based on evidence older than the latest dependency invalidation is refused) AND no relationship_coverage_missing evidence stands (a refresh that superseded the bundle''s relationship coverage must be answered with a matching pgokf.replace_relationships publication first) AND it is not retired; returns false (changing nothing) otherwise, so a superseded attempt can never clear staleness. The check runs under the bundle advisory lock, so it never certifies a generation or epoch older than a committed mutation it waited behind. On success records the manifest hash and embedding contract evidence and sets last_reconciled_at. Writer-tier; tenant-confined.';
COMMENT ON FUNCTION pgokf.mark_scope_stale(bigint, text, text, text[], text) IS
    'Mark one scope within a bundle (scope_kind concept/path/group with an exact, case-sensitive scope_key) stale with reason codes. Writer-tier; tenant-confined. The override shadows the bundle state for that scope in pgokf.effective_freshness until cleared.';
COMMENT ON FUNCTION pgokf.clear_freshness_scope(bigint, text, text) IS
    'Remove a concept/path/group freshness override, returning the scope to the bundle''s state. Writer-tier; tenant-confined; raises 22023 when no such override exists.';
COMMENT ON FUNCTION pgokf.list_freshness_dependencies(integer) IS
    'List every registered freshness dependency as pgokf.freshness_dependency_info, ordered by identity, bounded by max_rows (default 100). Admin-only (pgokf_admin); tenant-scoped; the raw table is granted to no role.';
COMMENT ON FUNCTION pgokf.repair_bundle_freshness(bigint, text, text[]) IS
    'Admin repair: set a bundle''s freshness state (fresh/stale/reconciling/blocked/retired) and reason codes directly, replacing both. Admin-only (pgokf_admin); tenant-confined. Does not establish producer currency evidence (generation/revision columns are untouched), so a repaired fresh row carries only the evidence it already had. Settles the dependency invalidation epoch claim and resets the relationship_coverage_missing evidence consistently with the given reason set.';
COMMENT ON FUNCTION pgokf.issue_publication_fence(bigint, text, bigint, bigint, text, integer) IS
    'Issue (or supersede) the publication fence for (tenant, producer, bundle), returning pgokf.publication_fence_info with the catalog-assigned fencing_token. Writer-tier; compare-and-set under the bundle advisory lock: raises 22023 unless expected_catalog_generation equals the bundle''s current catalog generation and target_generation advances past the slot''s current target. Lease defaults to 300 seconds.';
COMMENT ON FUNCTION pgokf.release_publication_fence(bigint, text, bigint) IS
    'Release a live publication fence; only the live fencing_token releases the slot, so a superseded or expired attempt fails with 22023 instead of silently completing. Writer-tier; tenant-confined.';
",
        name = "freshness_function_hardening",
        requires = [
            register_freshness_dependency,
            disable_freshness_dependency,
            remove_freshness_dependency,
            mark_stale,
            mark_reconciling,
            mark_blocked,
            mark_fresh,
            mark_scope_stale,
            clear_freshness_scope,
            list_freshness_dependencies,
            repair_bundle_freshness,
            issue_publication_fence,
            release_publication_fence
        ]
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ErrorKind;

    #[test]
    fn validate_selector_enforces_the_v1_grammar() {
        // Arrange / Act / Assert: exact kinds pass with their required values.
        assert!(validate_selector("bundle", "").is_ok());
        assert!(validate_selector("concept", "alpha").is_ok());
        assert!(validate_selector("path", "docs/a.md").is_ok());
        assert!(validate_selector("path_prefix", "docs/").is_ok());
        // Glob/regex spellings are plain operands, not syntax - they are
        // accepted as literal values and matched exactly.
        assert!(validate_selector("path_prefix", "docs/*").is_ok());
        // Unknown kinds, a valued bundle selector, and empty narrowed values
        // are rejected with 22023.
        for (kind, value) in [
            ("glob", "docs/*"),
            ("bundle", "anything"),
            ("concept", ""),
            ("path", ""),
            ("path_prefix", ""),
        ] {
            let error = validate_selector(kind, value).expect_err("must be rejected");
            assert_eq!(error.kind(), ErrorKind::InvalidParameter, "{kind}/{value}");
        }
    }

    #[test]
    fn validate_target_scope_pairs_kind_and_key() {
        // Arrange / Act / Assert
        assert!(validate_target_scope("bundle", None).is_ok());
        assert!(validate_target_scope("concept", Some("alpha")).is_ok());
        assert!(validate_target_scope("bundle", Some("alpha")).is_err());
        assert!(validate_target_scope("concept", None).is_err());
        assert!(validate_target_scope("concept", Some("")).is_err());
        assert!(validate_target_scope("row", None).is_err());
    }

    #[test]
    fn selector_matches_is_exact_and_case_sensitive() {
        // Arrange
        let changes = vec![
            crate::catalog::change_event::ConceptChange {
                id: "alpha".to_owned(),
                path: "docs/alpha.md".to_owned(),
                kind: "updated",
                before_hash: None,
                after_hash: None,
            },
            crate::catalog::change_event::ConceptChange {
                id: "beta".to_owned(),
                path: "docs/nested/beta.md".to_owned(),
                kind: "added",
                before_hash: None,
                after_hash: None,
            },
        ];

        // Act / Assert
        assert!(selector_matches("concept", "alpha", &changes));
        assert!(!selector_matches("concept", "Alpha", &changes));
        assert!(selector_matches("path", "docs/alpha.md", &changes));
        assert!(!selector_matches("path", "docs/alpha", &changes));
        assert!(selector_matches("path_prefix", "docs/", &changes));
        assert!(selector_matches("path_prefix", "docs/nested", &changes));
        assert!(!selector_matches("path_prefix", "other/", &changes));
        assert!(!selector_matches("bundle", "", &changes));
    }

    #[test]
    fn validated_reasons_defaults_an_empty_set_and_rejects_blank_entries() {
        // Arrange / Act / Assert
        assert_eq!(
            validated_reasons(Vec::new()).expect("empty defaults"),
            vec!["producer_reported".to_owned()]
        );
        assert!(validated_reasons(vec![" source_changed ".to_owned()]).is_ok());
        assert!(validated_reasons(vec!["  ".to_owned()]).is_err());
    }
}
