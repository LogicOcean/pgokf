// SPDX-License-Identifier: AGPL-3.0-only
//! Optional semantic (vector) and hybrid search over the catalog.
//!
//! This module adds three query surfaces plus the storage and index management
//! behind them, all built on `pgvector` - and, exactly like the `pg_search`
//! BM25 adapter in [`crate::catalog::search_backend`], **with no build-time or
//! install-time dependency on it**. `CREATE EXTENSION pgokf` succeeds on a
//! cluster where `pgvector` is absent; every `vector`-typed object is reached
//! only through runtime SQL, and the storage column is a plain `real[]` so the
//! `CREATE TABLE` inside `CREATE EXTENSION` never references a type that may not
//! exist.
//!
//! # Why `real[]` storage, not a `vector` column
//!
//! A `vector`-typed column would make `CREATE EXTENSION pgokf` fail outright on
//! a server without `pgvector` (the column type would be unresolvable during the
//! extension's own `CREATE TABLE`). Storing the embedding as the always-available
//! builtin `real[]` keeps the extension free of any static `pgvector` dependency;
//! `pgvector` registers a cast from `real[]` to `vector`, so the stored array is
//! losslessly cast to `vector(dim)` at query time (and in the HNSW index
//! expression) only when `pgvector` is actually present.
//!
//! # The three surfaces
//!
//! - [`set_concept_embedding`](pgokf::set_concept_embedding) - writer-tier
//!   ingest. A companion embedder (never this extension - it performs no model
//!   inference and no network I/O) streams caller-computed embeddings in as
//!   `real[]`; the row is validated (the concept must exist, the length must
//!   equal the durable `embedding_dim`) and upserted. This is the 0.2.0
//!   compatibility signature: it carries no provenance, so the row it writes
//!   is a legacy row that never ranks semantically.
//! - [`set_concept_embedding_cas`](pgokf::set_concept_embedding_cas) - the
//!   provenance-carrying writer-tier ingest: compare-and-set against the
//!   concept's current `file_hash` (a mismatch is a retryable `false`, never
//!   an error), recording the source file hash, the caller-computed input
//!   hash, the model, and the render-contract identity. Only rows written
//!   through it are eligible for semantic ranking.
//! - [`concept_search_semantic`](pgokf::concept_search_semantic) - reader-tier
//!   nearest-neighbor search by `pgvector` cosine distance (`<=>`) over
//!   **eligible** embeddings only: the row's `source_file_hash` must equal the
//!   concept's current `file_hash`, its model/dimension/contract must match
//!   the durable embedding policy, and the concept must be effectively fresh
//!   (bundle freshness `fresh`, no covering concept/path override). Stale or
//!   legacy rows never rank even while the HNSW index physically retains
//!   them. Semantic search has no lexical equivalent, so when `pgvector` is
//!   absent it raises a clear `22023` naming the missing dependency rather
//!   than silently returning nothing.
//! - [`concept_search_hybrid`](pgokf::concept_search_hybrid) - reader-tier
//!   Reciprocal Rank Fusion (RRF, k = 60) of the lexical result (through the
//!   configured `search_backend`) and the eligible-only semantic result, fused
//!   entirely in SQL. RRF needs no model, so when `pgvector` is absent this
//!   **sensibly** degrades to lexical-only with a `WARNING`.
//!
//! A sync that re-stages a concept deletes its embedding row in the same
//! transaction ([`invalidate_synced_concepts`]), so no old vector coexists
//! with new concept text past commit; the embedder's missing-row poll then
//! re-embeds it.
//!
//! Plus [`rebuild_embedding_index`](pgokf::rebuild_embedding_index) - admin-tier,
//! mirroring `rebuild_search_index`: it builds a `pgvector` HNSW (cosine) index
//! over the embeddings for the configured dimension, and is a logged no-op when
//! `pgvector` is absent.

use std::path::Path;

use pgrx::Spi;
use pgrx::spi::SpiTupleTable;

use crate::catalog::config;
use crate::catalog::search::{self, Filters};
use crate::catalog::spi_read::RowReader;
use crate::catalog::types::SearchHit;
use crate::errors::CatalogError;
use crate::security;

/// Standard Reciprocal Rank Fusion constant. `k = 60` is the value from the
/// original Cormack et al. RRF paper and the de-facto default across search
/// stacks; it damps the influence of any single list's exact ranks so a result
/// strong in both lists reliably outranks one strong in only one.
const RRF_K: f64 = 60.0;

/// pgvector's hard dimension ceiling for an HNSW index. Above it, embeddings are
/// still stored and searched exactly (sequential scan of the cosine distance);
/// only the index build is skipped.
const HNSW_MAX_DIM: i32 = 2000;

/// Fixed name of the HNSW index [`rebuild`] manages on `pgokf.concept_embedding`.
const HNSW_INDEX_NAME: &str = "concept_embedding_hnsw_idx";

pgrx::extension_sql!(
    r"
CREATE TABLE pgokf.concept_embedding (
    bundle_id  bigint      NOT NULL,
    concept_id text        NOT NULL,
    embedding  real[]      NOT NULL,
    dim        integer     NOT NULL,
    model      text,
    updated_at timestamptz NOT NULL DEFAULT now(),
    tenant_id  text        NOT NULL DEFAULT 'default',
    -- source_file_hash / input_hash / contract are appended last so a fresh
    -- install matches, column-for-column, an existing install upgraded via
    -- ADD COLUMN (see sql/pgokf--0.2.0--0.3.0-dev.sql). NULL on any of them
    -- marks a legacy (pre-0.3.0 or compatibility-setter) row, which is never
    -- eligible for semantic ranking.
    source_file_hash text,
    input_hash text,
    contract text,
    CONSTRAINT concept_embedding_pkey PRIMARY KEY (bundle_id, concept_id),
    CONSTRAINT concept_embedding_concept_fk
        FOREIGN KEY (bundle_id, concept_id)
        REFERENCES pgokf.concepts (bundle_id, id)
        ON DELETE CASCADE,
    CONSTRAINT concept_embedding_dim_chk CHECK (dim = cardinality(embedding))
);

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced, so the SECURITY DEFINER set_concept_embedding
-- path bypasses it to upsert a single-tenant bundle's vectors.
ALTER TABLE pgokf.concept_embedding ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_embedding_tenant_isolation ON pgokf.concept_embedding
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

COMMENT ON TABLE pgokf.concept_embedding IS
    'Opt-in per-concept embedding vectors, streamed in by a companion embedder via pgokf.set_concept_embedding / pgokf.set_concept_embedding_cas (the extension never computes embeddings or performs network I/O). The vector is stored as the builtin real[] - NOT a pgvector ''vector'' column - so CREATE EXTENSION pgokf succeeds without pgvector installed; it is cast to vector(dim) at query time and in the HNSW index only when pgvector is present. Rows cascade from pgokf.concepts, so removing a concept or unregistering a bundle drops its embedding automatically, and a sync that re-stages a concept deletes its row in the same transaction. Semantic ranking ranks only ELIGIBLE rows: source_file_hash equal to the concept''s current file_hash, model/dim/contract matching the current embedding policy, and the concept effectively fresh; a row with NULL provenance (legacy or written by the compatibility setter) never ranks.';
COMMENT ON COLUMN pgokf.concept_embedding.embedding IS
    'The caller-computed embedding as real[]. Its length must equal the durable embedding_dim configuration key at ingest time (enforced by pgokf.set_concept_embedding); dim records that length redundantly for a size-only read.';
COMMENT ON COLUMN pgokf.concept_embedding.dim IS
    'Length of embedding, constrained equal to cardinality(embedding); the effective dimension of the stored vector. Semantic eligibility additionally requires dim to equal the current embedding_dim policy.';
COMMENT ON COLUMN pgokf.concept_embedding.model IS
    'Identifier of the embedding model that computed the vector (pgokf.set_concept_embedding_cas requires it). NULL marks a legacy row - pre-0.3.0 or written through the compatibility setter pgokf.set_concept_embedding - which is never eligible for semantic ranking.';
COMMENT ON COLUMN pgokf.concept_embedding.updated_at IS
    'When this embedding row was last written by pgokf.set_concept_embedding / set_concept_embedding_cas; the embedded_at provenance of search result metadata.';
COMMENT ON COLUMN pgokf.concept_embedding.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.concept_embedding.source_file_hash IS
    'The concept''s file_hash at embed time (pgokf.set_concept_embedding_cas compare-and-sets against it). Semantic eligibility requires it to equal the concept''s current file_hash; NULL marks a legacy row that never ranks.';
COMMENT ON COLUMN pgokf.concept_embedding.input_hash IS
    'Hash of the exact bounded input text the embedder sent (title + description + body_text under the render contract), supplied by the embedder as provenance; the catalog stores it opaquely and never re-computes it. NULL marks a legacy row.';
COMMENT ON COLUMN pgokf.concept_embedding.contract IS
    'The embedder''s render-contract identity (input construction and truncation version), e.g. pgokf-embed/v1/max-chars:8000. When the embedding_contract policy key pins a value, semantic eligibility requires an exact match; NULL marks a legacy row that never ranks.';

GRANT SELECT ON pgokf.concept_embedding TO pgokf_reader;
",
    name = "embedding_table",
    requires = ["catalog_tables"]
);

fn spi_error(context: &'static str) -> impl Fn(pgrx::spi::Error) -> CatalogError {
    move |error| CatalogError::internal(format!("{context}: {error}"), Path::new(""))
}

/// Report whether the `pgvector` extension is installed in this database.
///
/// The probe is by `pg_extension` catalog membership - never by attempting to
/// use a `vector`-typed expression - so it is safe to call before any SQL that
/// would fail to parse when `pgvector` is absent.
fn pgvector_installed() -> Result<bool, CatalogError> {
    Spi::get_one::<bool>(
        "SELECT pg_catalog.count(*) > 0 FROM pg_catalog.pg_extension WHERE extname = 'vector'",
    )
    .map_err(spi_error("failed to check for the pgvector extension"))?
    .ok_or_else(|| CatalogError::internal("pgvector probe returned no row", Path::new("")))
}

/// The `pgvector` install schema, already `quote_ident`-escaped, for building
/// dynamic DDL that names the schema-scoped `vector` type and `vector_cosine_ops`
/// operator class. `None` when `pgvector` is not installed.
fn pgvector_schema() -> Result<Option<String>, CatalogError> {
    // `Spi::get_one` raises on an empty result rather than returning `None`, and
    // pgvector's absence is exactly the empty-result case here, so the read goes
    // through `is_empty` (the same pattern the bundle lookups use). `None` means
    // pgvector is not installed.
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT pg_catalog.quote_ident(n.nspname)
                 FROM pg_catalog.pg_extension e
                 JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
                 WHERE e.extname = 'vector'",
                Some(1),
                &[],
            )
            .map_err(spi_error("failed to resolve the pgvector schema"))?;
        if table.is_empty() {
            return Ok(None);
        }
        table
            .first()
            .get_one::<String>()
            .map_err(spi_error("failed to read the pgvector schema"))
    })
}

/// The current embedding contract policy a stored vector must match to be
/// eligible for semantic ranking.
///
/// `model`/`contract` are the durable `embedding_model` / `embedding_contract`
/// configuration keys; an empty value is the unpinned default, which matches
/// any non-NULL row value (a NULL row value is legacy and never matches).
/// `dim` is always pinned: a row stored under a different `embedding_dim` is
/// ineligible (and would fail the `vector(dim)` cast besides).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EmbeddingPolicy {
    pub dim: i32,
    pub model: String,
    pub contract: String,
}

/// Read the effective embedding policy through the reader-granted
/// `pgokf.get_config` projection, in one round trip. Used by the
/// invoker-rights search paths, which cannot read the admin-only config table
/// directly.
pub(crate) fn effective_embedding_policy() -> Result<EmbeddingPolicy, CatalogError> {
    Spi::connect(|client| {
        let table = client
            .select(
                "SELECT (cfg ->> 'embedding_dim')::pg_catalog.int4,
                        cfg ->> 'embedding_model',
                        cfg ->> 'embedding_contract'
                 FROM (SELECT pgokf.get_config() AS cfg) AS c",
                Some(1),
                &[],
            )
            .map_err(spi_error(
                "failed to read the embedding policy configuration",
            ))?;
        let Some(row) = table.into_iter().next() else {
            return Err(CatalogError::internal(
                "the embedding policy configuration is missing",
                Path::new(""),
            ));
        };
        let reader = RowReader::new(&row, "failed to read the embedding policy", "config");
        Ok(EmbeddingPolicy {
            dim: reader.required(1, "embedding_dim")?,
            model: reader.required(2, "embedding_model")?,
            contract: reader.required(3, "embedding_contract")?,
        })
    })
}

/// The physical-contract half of semantic eligibility, over the aliases `e`
/// (`pgokf.concept_embedding`) and `c` (`pgokf.concepts`): a non-legacy row
/// whose recorded source hash equals the concept's current `file_hash` and
/// whose model/dimension/contract match the current policy. An unpinned
/// (empty) model/contract policy matches any non-NULL row value; a NULL row
/// value is legacy and never matches. The caller binds the policy as the
/// numbered parameters `model_param`, `dim_param`, and `contract_param`.
///
/// The other half of eligibility - the concept's effective freshness and the
/// bundle being active - is expressed at each query site (the invoker-rights
/// ranking path reads the reader-granted `pgokf.effective_freshness`
/// projection because the raw freshness tables are granted to no API role).
/// Keep every site of this predicate in sync.
pub(crate) fn contract_match_sql(
    model_param: usize,
    dim_param: usize,
    contract_param: usize,
) -> String {
    format!(
        "e.source_file_hash IS NOT NULL AND e.source_file_hash = c.file_hash \
         AND e.model IS NOT NULL AND (${model_param} = '' OR e.model = ${model_param}) \
         AND e.dim = ${dim_param} \
         AND e.contract IS NOT NULL AND (${contract_param} = '' OR e.contract = ${contract_param})"
    )
}

/// Validate a caller-supplied embedding length against the expected dimension.
fn validate_embedding_length(len: usize, expected_dim: i32) -> Result<(), CatalogError> {
    let expected = usize::try_from(expected_dim).unwrap_or(0);
    if len == expected {
        Ok(())
    } else {
        Err(CatalogError::invalid_parameter(
            format!(
                "embedding has {len} dimensions but the configured embedding_dim is {expected_dim}; \
                 set embedding_dim to match your model or supply a {expected_dim}-dimensional vector"
            ),
            Path::new(""),
        ))
    }
}

/// Validate that every element of a caller-supplied embedding is finite.
///
/// Storage is `real[]`, so a `NaN`/`Infinity` element inserts without complaint,
/// but every query, index-build, and `rebuild_embedding_index` path casts the
/// stored array to `vector(dim)`, and `pgvector` rejects a non-finite component
/// at that cast - SQLSTATE `22023`. One poisoned write would therefore break
/// semantic/hybrid search and index rebuilds catalog-wide until the row is
/// found and fixed. Rejecting the non-finite element at write time (the same
/// `22023` class the length check raises), naming the offending index for the
/// caller, contains the fault to the single bad call.
fn validate_embedding_finite(embedding: &[f32]) -> Result<(), CatalogError> {
    if let Some(index) = embedding.iter().position(|value| !value.is_finite()) {
        return Err(CatalogError::invalid_parameter(
            format!(
                "embedding element at index {index} is not finite ({}); every element must be a \
                 finite real number - NaN and infinity are rejected because pgvector refuses them \
                 when the stored real[] is cast to vector(dim) at query and index time",
                embedding[index]
            ),
            Path::new(""),
        ));
    }
    Ok(())
}

/// The clear `22023` raised when a semantic query needs `pgvector` but it is
/// absent. Semantic search has no lexical equivalent, so this is an error rather
/// than a silent empty result.
fn missing_pgvector_error() -> CatalogError {
    CatalogError::invalid_parameter(
        "semantic search requires the pgvector extension, which is not installed; \
         run CREATE EXTENSION vector (or use pgokf.concept_search for lexical search)",
        Path::new(""),
    )
}

/// Read `pgokf.concept_search_result`-shaped rows (`bundle_id`, `concept_id`,
/// `path`, `title`, `type`, `rank`, `headline`) into [`SearchHit`]s, mirroring
/// the shared reader in [`crate::catalog::search_backend`] so semantic, hybrid,
/// and lexical results pack into the composite identically.
fn read_result_hits(table: SpiTupleTable) -> Result<Vec<SearchHit>, CatalogError> {
    let mut hits = Vec::with_capacity(table.len());
    for row in table {
        let reader = RowReader::new(&row, "failed to read embedding search row", "search result");
        hits.push(SearchHit {
            bundle_id: reader.required(1, "bundle_id")?,
            concept_id: reader.required(2, "concept_id")?,
            path: reader.required(3, "path")?,
            title: reader.optional(4)?,
            concept_type: reader.optional(5)?,
            rank: reader.required(6, "rank")?,
            headline: reader.optional(7)?,
        });
    }
    Ok(hits)
}

/// Run the nearest-neighbor query over **eligible** embedding rows only,
/// ordered by `pgvector` cosine distance, with a normalized cosine-similarity
/// score. Assumes `pgvector` is present and the query embedding length equals
/// the policy dimension (both checked by the callers).
///
/// Eligibility (the "stale vectors do not rank" invariant) requires, per row:
///
/// - the physical contract match ([`contract_match_sql`]): non-legacy
///   provenance whose `source_file_hash` equals the concept's current
///   `file_hash` and whose model/dimension/contract match `policy`;
/// - the concept's effective freshness is `fresh`: the bundle-scope
///   freshness row is `fresh` AND no covering concept/path scope override is
///   non-fresh. The invoker-rights query reads the reader-granted
///   `pgokf.effective_freshness` projection for this (the raw freshness
///   tables are granted to no API role); this is the deliberate
///   bundle-state-plus-override check rather than a per-candidate function
///   call;
/// - the bundle is active (enabled, not retired).
///
/// Ineligible rows may physically remain in the table (and in the HNSW index)
/// for audit/rollback, but they never rank: the HNSW index serves the
/// ordering only as a pre-filter, because the eligibility predicates are not
/// index expressions.
///
/// `policy.dim` is a trusted `integer` from validated configuration (never
/// caller input), so formatting it into the `vector(dim)` typmod is
/// injection-safe - an `i32` can only render as digits - and is required
/// because a typmod cannot be a bound parameter. The query embedding is bound
/// as `$1` (never interpolated). Both the stored column and the query vector
/// cast through the identical `embedding::vector(dim)` expression the HNSW
/// index is built on, so the index serves the ordering when present.
fn run_semantic_query(
    query_embedding: &[f32],
    bundle_id: Option<i64>,
    limit: i64,
    policy: &EmbeddingPolicy,
) -> Result<Vec<SearchHit>, CatalogError> {
    let dim = policy.dim;
    let eligibility = contract_match_sql(4, 5, 6);
    let query = format!(
        "SELECT c.bundle_id,
                c.id,
                c.path,
                c.title,
                c.type,
                (1.0 - (e.embedding::vector({dim}) <=> $1::vector({dim})))::pg_catalog.float4,
                NULL::pg_catalog.text
         FROM pgokf.concept_embedding e
         JOIN pgokf.concepts c ON c.bundle_id = e.bundle_id AND c.id = e.concept_id
         JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
         JOIN pgokf.effective_freshness ef
           ON ef.bundle_id = c.bundle_id AND ef.scope_kind = 'bundle' AND ef.state = 'fresh'
         WHERE {eligibility}
           AND NOT EXISTS (
               SELECT 1 FROM pgokf.effective_freshness eo
               WHERE eo.bundle_id = c.bundle_id
                 AND eo.state <> 'fresh'
                 AND ((eo.scope_kind = 'concept' AND eo.scope_key = c.id)
                      OR (eo.scope_kind = 'path' AND eo.scope_key = c.path)))
           AND ($2 IS NULL OR c.bundle_id = $2)
         ORDER BY e.embedding::vector({dim}) <=> $1::vector({dim}),
                  c.bundle_id, c.id
         LIMIT $3"
    );
    Spi::connect(|client| {
        let table = client
            .select(
                &query,
                None,
                &[
                    query_embedding.to_vec().into(),
                    bundle_id.into(),
                    limit.into(),
                    policy.model.clone().into(),
                    policy.dim.into(),
                    policy.contract.clone().into(),
                ],
            )
            .map_err(spi_error("semantic search query failed"))?;
        read_result_hits(table)
    })
}

/// Whether a concept exists in the catalog (the FK target for an embedding row).
fn concept_exists(bundle_id: i64, concept_id: &str) -> Result<bool, CatalogError> {
    Spi::get_one_with_args::<bool>(
        "SELECT EXISTS (SELECT 1 FROM pgokf.concepts WHERE bundle_id = $1 AND id = $2)",
        &[bundle_id.into(), concept_id.into()],
    )
    .map_err(spi_error("failed to look up concept for embedding"))?
    .ok_or_else(|| CatalogError::internal("concept existence probe returned no row", Path::new("")))
}

/// Authorize (writer), validate, and upsert one concept embedding.
///
/// The 0.2.0 compatibility setter: it carries no provenance arguments, so the
/// row it writes is a **legacy** row - `model`, `source_file_hash`,
/// `input_hash`, and `contract` are all NULL (an overwrite deliberately clears
/// any provenance a compare-and-set write had recorded, because this setter
/// cannot prove the vector it stores corresponds to the current concept). A
/// legacy row is stored but is never eligible for semantic ranking; the
/// embedding watcher re-embeds it through the compare-and-set setter.
fn set_concept_embedding_impl(
    bundle_id: i64,
    concept_id: &str,
    embedding: Vec<f32>,
) -> Result<(), CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    // Write-side tenant confinement: a scoped session may only embed into its own
    // tenant's bundle. Checked first (before the dimension/concept validation) so
    // a cross-tenant bundle_id is rejected as an unknown bundle without revealing
    // anything about that bundle's concepts.
    security::enforce_bundle_tenant(bundle_id)?;

    let dim = config::embedding_dim()?;
    validate_embedding_length(embedding.len(), dim)?;
    // Reject NaN/Infinity before the upsert: real[] would store them silently,
    // but pgvector rejects a non-finite component when the array is cast to
    // vector(dim) on every read/index path, so one bad write would otherwise
    // poison semantic/hybrid search and rebuild_embedding_index catalog-wide.
    validate_embedding_finite(&embedding)?;

    if !concept_exists(bundle_id, concept_id)? {
        return Err(CatalogError::invalid_parameter(
            format!("no such concept {concept_id} in bundle {bundle_id}"),
            Path::new(""),
        ));
    }

    // tenant_id is derived from the bundle (single-tenant) and left untouched on
    // conflict, so re-embedding a concept never rewrites its tenant.
    Spi::run_with_args(
        "INSERT INTO pgokf.concept_embedding
             (bundle_id, tenant_id, concept_id, embedding, dim, updated_at)
         VALUES ($1,
                 (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
                 $2, $3, $4, pg_catalog.now())
         ON CONFLICT (bundle_id, concept_id) DO UPDATE SET
             embedding = excluded.embedding,
             dim = excluded.dim,
             updated_at = pg_catalog.now(),
             model = NULL,
             source_file_hash = NULL,
             input_hash = NULL,
             contract = NULL",
        &[
            bundle_id.into(),
            concept_id.into(),
            embedding.into(),
            dim.into(),
        ],
    )
    .map_err(spi_error("failed to upsert concept embedding"))
}

/// Validate one non-empty provenance argument of the compare-and-set setter.
fn validate_provenance_arg(name: &str, value: &str) -> Result<(), CatalogError> {
    if value.trim().is_empty() {
        return Err(CatalogError::invalid_parameter(
            format!("{name} must not be empty"),
            Path::new(""),
        ));
    }
    Ok(())
}

/// Authorize (writer), validate, compare-and-set, and upsert one concept
/// embedding with full provenance.
///
/// The compare-and-set closes the inference race between a slow embedder and a
/// concurrent sync: the concept row is locked (`FOR UPDATE`) and its current
/// `file_hash` is read under that lock, so a sync's concept update either
/// commits first (and is observed here) or waits behind this write and then
/// deletes this row in its own transaction (see
/// [`invalidate_synced_concepts`]). Both orderings are safe; without the lock
/// an insert could land between a sync's delete and its commit and survive
/// with a stale hash - the ranking eligibility predicate still excludes such
/// a row, but the lock keeps the table itself clean.
///
/// Returns `Ok(false)` - a retryable rejection, not an error - when the
/// concept's current `file_hash` no longer equals `expected_file_hash`: the
/// concept changed since the caller read it, so the computed vector describes
/// an old input and must be recomputed (the caller re-polls).
fn set_concept_embedding_cas_impl(
    bundle_id: i64,
    concept_id: &str,
    embedding: Vec<f32>,
    expected_file_hash: &str,
    input_hash: &str,
    model: &str,
    contract: &str,
) -> Result<bool, CatalogError> {
    security::authorize_current_user(security::Operation::Ingest, Path::new(""))?;
    security::enforce_bundle_tenant(bundle_id)?;

    validate_provenance_arg("expected_file_hash", expected_file_hash)?;
    validate_provenance_arg("input_hash", input_hash)?;
    validate_provenance_arg("model", model)?;
    validate_provenance_arg("contract", contract)?;

    let dim = config::embedding_dim()?;
    validate_embedding_length(embedding.len(), dim)?;
    validate_embedding_finite(&embedding)?;

    // connect_mut + update: SPI read-only selects reject FOR UPDATE, and
    // Spi::get_one errors on an empty result instead of returning None.
    let current_hash = Spi::connect_mut(|client| {
        let mut table = client
            .update(
                "SELECT file_hash FROM pgokf.concepts
                 WHERE bundle_id = $1 AND id = $2
                 FOR UPDATE",
                Some(1),
                &[bundle_id.into(), concept_id.into()],
            )
            .map_err(spi_error(
                "failed to lock the concept for the embedding write",
            ))?;
        table
            .next()
            .map(|row| {
                RowReader::new(&row, "failed to read the concept file hash", "concept")
                    .required::<String>(1, "file_hash")
            })
            .transpose()
    })?;
    let Some(current_hash) = current_hash else {
        return Err(CatalogError::invalid_parameter(
            format!("no such concept {concept_id} in bundle {bundle_id}"),
            Path::new(""),
        ));
    };
    if current_hash != expected_file_hash {
        return Ok(false);
    }

    Spi::run_with_args(
        "INSERT INTO pgokf.concept_embedding
             (bundle_id, tenant_id, concept_id, embedding, dim, model, updated_at,
              source_file_hash, input_hash, contract)
         VALUES ($1,
                 (SELECT b.tenant_id FROM pgokf.bundles b WHERE b.id = $1),
                 $2, $3, $4, $5, pg_catalog.now(), $6, $7, $8)
         ON CONFLICT (bundle_id, concept_id) DO UPDATE SET
             embedding = excluded.embedding,
             dim = excluded.dim,
             model = excluded.model,
             updated_at = pg_catalog.now(),
             source_file_hash = excluded.source_file_hash,
             input_hash = excluded.input_hash,
             contract = excluded.contract",
        &[
            bundle_id.into(),
            concept_id.into(),
            embedding.into(),
            dim.into(),
            model.into(),
            expected_file_hash.into(),
            input_hash.into(),
            contract.into(),
        ],
    )
    .map_err(spi_error("failed to upsert concept embedding"))?;
    Ok(true)
}

/// Delete the embedding rows of the concepts a sync is re-staging, in bounded
/// batches, inside the sync's own transaction.
///
/// A re-staged concept is about to receive new text (and a new `file_hash`)
/// from the concept upsert; its old vector must not coexist with the new text
/// past commit, so the row is deleted here - immediately around the concept
/// DML. Removed and reclassified concepts need no explicit delete: their
/// embedding rows cascade from the concept delete through the foreign key.
/// After commit the embedder's missing-row poll naturally re-embeds the
/// concept.
///
/// # Errors
///
/// Returns a [`CatalogError`] on any SPI failure, aborting the sync.
pub(crate) fn invalidate_synced_concepts(
    bundle_id: i64,
    concept_ids: &[String],
) -> Result<(), CatalogError> {
    for chunk in concept_ids.chunks(500) {
        Spi::run_with_args(
            "DELETE FROM pgokf.concept_embedding
             WHERE bundle_id = $1 AND concept_id = ANY($2)",
            &[bundle_id.into(), chunk.to_vec().into()],
        )
        .map_err(spi_error(
            "failed to invalidate embeddings of synced concepts",
        ))?;
    }
    Ok(())
}

/// Authorize (reader), validate, require `pgvector`, and run the semantic query.
fn concept_search_semantic_impl(
    query_embedding: &[f32],
    bundle_id: Option<i64>,
    limit_count: i32,
) -> Result<Vec<SearchHit>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    let limit = search::validate_limit_count(limit_count)?;
    if !pgvector_installed()? {
        return Err(missing_pgvector_error());
    }
    let policy = effective_embedding_policy()?;
    validate_embedding_length(query_embedding.len(), policy.dim)?;
    run_semantic_query(query_embedding, bundle_id, limit, &policy)
}

/// The (`bundle_id`, `concept_id`) key of a ranked hit, in rank order - the RRF
/// fusion input.
struct RankKeys {
    bundle_ids: Vec<i64>,
    concept_ids: Vec<String>,
}

impl RankKeys {
    fn from_hits(hits: &[SearchHit]) -> Self {
        Self {
            bundle_ids: hits.iter().map(|hit| hit.bundle_id).collect(),
            concept_ids: hits.iter().map(|hit| hit.concept_id.clone()).collect(),
        }
    }
}

/// Fuse the lexical and semantic rank lists by Reciprocal Rank Fusion, entirely
/// in SQL, and project the top `limit` fused concepts as
/// `concept_search_result`-shaped rows.
///
/// Each list contributes `1 / (k + rank)` per concept (rank = 1-based position
/// via `WITH ORDINALITY`), summed across the `FULL OUTER JOIN` on concept
/// identity; a concept present in both lists therefore scores higher than one in
/// only one. Only the two key arrays cross the boundary - the fusion arithmetic
/// and final projection are SQL, needing no model. The final join re-filters to
/// enabled bundles as defense in depth (both input lists already did).
fn fuse_rrf(
    lexical: &RankKeys,
    semantic: &RankKeys,
    limit: i64,
) -> Result<Vec<SearchHit>, CatalogError> {
    // k is the fixed RRF constant, formatted as a literal (never data); the four
    // key arrays and the limit are bound parameters.
    let query = format!(
        "WITH lex AS (
             SELECT bundle_id, concept_id, ord::pg_catalog.float8 AS rank
             FROM unnest($1::pg_catalog.int8[], $2::pg_catalog.text[])
                  WITH ORDINALITY AS t(bundle_id, concept_id, ord)
         ),
         sem AS (
             SELECT bundle_id, concept_id, ord::pg_catalog.float8 AS rank
             FROM unnest($3::pg_catalog.int8[], $4::pg_catalog.text[])
                  WITH ORDINALITY AS t(bundle_id, concept_id, ord)
         ),
         fused AS (
             SELECT coalesce(l.bundle_id, s.bundle_id) AS bundle_id,
                    coalesce(l.concept_id, s.concept_id) AS concept_id,
                    coalesce(1.0 / ({RRF_K} + l.rank), 0.0)
                  + coalesce(1.0 / ({RRF_K} + s.rank), 0.0) AS score
             FROM lex l
             FULL OUTER JOIN sem s
               ON l.bundle_id = s.bundle_id AND l.concept_id = s.concept_id
         )
         SELECT c.bundle_id,
                c.id,
                c.path,
                c.title,
                c.type,
                f.score::pg_catalog.float4,
                NULL::pg_catalog.text
         FROM fused f
         JOIN pgokf.concepts c ON c.bundle_id = f.bundle_id AND c.id = f.concept_id
         JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
         ORDER BY f.score DESC, c.bundle_id, c.id
         LIMIT $5"
    );
    Spi::connect(|client| {
        let table = client
            .select(
                &query,
                None,
                &[
                    lexical.bundle_ids.clone().into(),
                    lexical.concept_ids.clone().into(),
                    semantic.bundle_ids.clone().into(),
                    semantic.concept_ids.clone().into(),
                    limit.into(),
                ],
            )
            .map_err(spi_error("hybrid fusion query failed"))?;
        read_result_hits(table)
    })
}

/// Authorize (reader), validate, run lexical + semantic, and RRF-fuse. Degrades
/// to lexical-only, with a `WARNING`, when `pgvector` is absent.
fn concept_search_hybrid_impl(
    query: &str,
    query_embedding: &[f32],
    bundle_id: Option<i64>,
    limit_count: i32,
) -> Result<Vec<SearchHit>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    search::validate_query(query)?;
    let limit = search::validate_limit_count(limit_count)?;

    // Lexical list through the configured search_backend (native or BM25).
    let lexical_hits =
        search::run_ranked_search(query, bundle_id, limit, Filters::default(), None)?;
    let lexical = RankKeys::from_hits(&lexical_hits);

    // Semantic list when pgvector is present; otherwise degrade to lexical-only.
    // The semantic side ranks eligible (current, fresh) vectors only, so an
    // ineligible embedding can never leak into the fused result through the
    // semantic component; the lexical side may still return the concept, with
    // the freshness label pgokf.concept_search_fresh annotates.
    let semantic = if pgvector_installed()? {
        let policy = effective_embedding_policy()?;
        validate_embedding_length(query_embedding.len(), policy.dim)?;
        let semantic_hits = run_semantic_query(query_embedding, bundle_id, limit, &policy)?;
        RankKeys::from_hits(&semantic_hits)
    } else {
        pgrx::warning!(
            "pgokf: pgvector is not installed; concept_search_hybrid is degrading to \
             lexical-only search. Run CREATE EXTENSION vector to enable semantic fusion."
        );
        RankKeys {
            bundle_ids: Vec::new(),
            concept_ids: Vec::new(),
        }
    };

    fuse_rrf(&lexical, &semantic, limit)
}

/// (Re)build the HNSW cosine index on `pgokf.concept_embedding`, or report the
/// no-op. Returns `true` when built, `false` for a logged no-op (no `pgvector`,
/// or a dimension above the HNSW limit).
fn rebuild() -> Result<bool, CatalogError> {
    security::authorize_current_user(security::Operation::Register, Path::new(""))?;

    let Some(schema) = pgvector_schema()? else {
        pgrx::notice!(
            "pgokf: pgvector is not installed; rebuild_embedding_index is a no-op. Run \
             CREATE EXTENSION vector to enable semantic and hybrid search."
        );
        return Ok(false);
    };

    let dim = config::embedding_dim()?;
    if dim > HNSW_MAX_DIM {
        pgrx::notice!(
            "pgokf: embedding_dim ({}) exceeds pgvector's HNSW limit ({}); skipping index build. \
             Semantic search still works via an exact scan.",
            dim,
            HNSW_MAX_DIM
        );
        return Ok(false);
    }

    // Fixed identifiers plus a quote_ident-escaped pgvector schema and a trusted
    // integer dim; no caller input reaches the DDL text. Drop-then-create keeps
    // the build idempotent across a dimension change. The vector type and the
    // vector_cosine_ops operator class are schema-scoped (this function pins its
    // search_path), so both are qualified with the resolved pgvector schema; the
    // hnsw access method is global and needs none. The index casts the stored
    // real[] to vector(dim) so semantic queries using the same cast expression
    // are served by it.
    Spi::run(&format!("DROP INDEX IF EXISTS pgokf.{HNSW_INDEX_NAME}")).map_err(|error| {
        CatalogError::internal(
            format!("failed to drop existing embedding index: {error}"),
            Path::new(""),
        )
    })?;
    Spi::run(&format!(
        "CREATE INDEX {HNSW_INDEX_NAME} ON pgokf.concept_embedding \
         USING hnsw ((embedding::{schema}.vector({dim})) {schema}.vector_cosine_ops)"
    ))
    .map_err(|error| {
        CatalogError::internal(
            format!("failed to create embedding HNSW index: {error}"),
            Path::new(""),
        )
    })?;
    Ok(true)
}

/// SQL-facing embedding and semantic/hybrid search surface, installed into the
/// `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{
        concept_search_hybrid_impl, concept_search_semantic_impl, rebuild,
        set_concept_embedding_cas_impl, set_concept_embedding_impl,
    };
    use crate::catalog::types;

    /// Store (or replace) a concept's embedding vector.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). `embedding` is a `real[]` whose length must equal the
    /// durable `embedding_dim` configuration key; the concept must already
    /// exist. This is how a companion embedder streams caller-computed vectors
    /// in - the extension never computes embeddings and performs no network I/O.
    /// Raises SQLSTATE `22023` on a wrong length or an unknown concept, and
    /// `42501` for a caller outside `pgokf_writer`.
    ///
    /// This is the 0.2.0 compatibility signature: it carries no provenance
    /// arguments, so the row it writes is a legacy row (`model`,
    /// `source_file_hash`, `input_hash`, and `contract` all NULL - an
    /// overwrite clears them) that is stored but never eligible for semantic
    /// ranking. New writers should use
    /// `pgokf.set_concept_embedding_cas`, which proves the vector matches the
    /// current concept.
    #[pg_extern(requires = ["embedding_table"])]
    fn set_concept_embedding(bundle_id: i64, concept_id: &str, embedding: Vec<f32>) {
        set_concept_embedding_impl(bundle_id, concept_id, embedding)
            .unwrap_or_else(|error| error.raise());
    }

    /// Store (or replace) a concept's embedding vector with full provenance,
    /// compare-and-set against the concept's current `file_hash`.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). Beyond the `set_concept_embedding` validation (the
    /// `embedding` length must equal the durable `embedding_dim` key and the
    /// concept must exist - SQLSTATE `22023` otherwise), every provenance
    /// argument must be non-empty: `expected_file_hash` is the concept's
    /// `file_hash` the caller read when it built the embedding input,
    /// `input_hash` is the caller-computed hash of the exact bounded input
    /// text it embedded, `model` identifies the embedding model, and
    /// `contract` is the caller's render-contract identity (input construction
    /// and truncation version).
    ///
    /// The write commits only when the concept's current `file_hash` still
    /// equals `expected_file_hash` under a row lock; on a mismatch the
    /// function returns `false` and writes nothing - a **retryable**
    /// rejection: the concept changed since the caller read it, so the caller
    /// should re-read and re-embed rather than error. Returns `true` when the
    /// vector was stored. Only rows written through this setter carry the
    /// provenance semantic ranking requires.
    // `embedding` is a `Vec<f32>` because that is the SQL `real[]` boundary
    // type; it is only borrowed into the impl, so pass-by-value is inherent to
    // the pgrx signature. Seven SQL arguments are the CAS contract itself.
    #[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
    #[pg_extern(requires = ["embedding_table"])]
    fn set_concept_embedding_cas(
        bundle_id: i64,
        concept_id: &str,
        embedding: Vec<f32>,
        expected_file_hash: &str,
        input_hash: &str,
        model: &str,
        contract: &str,
    ) -> bool {
        set_concept_embedding_cas_impl(
            bundle_id,
            concept_id,
            embedding,
            expected_file_hash,
            input_hash,
            model,
            contract,
        )
        .unwrap_or_else(|error| error.raise())
    }

    /// Rank concepts by semantic similarity to a query embedding.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Orders by
    /// `pgvector` cosine distance (`<=>`) over stored concept embeddings; the
    /// `rank` column is the normalized cosine similarity (`1 - distance`).
    /// `query_embedding` must have `embedding_dim` dimensions. Searches active
    /// bundles only; `limit_count` must lie in `1..=500`. **Requires pgvector**:
    /// raises SQLSTATE `22023` naming the missing dependency when the `pgvector`
    /// extension is not installed (semantic search has no lexical fallback - use
    /// `pgokf.concept_search` for that).
    ///
    /// Only **eligible** embeddings rank: the row's `source_file_hash` must
    /// equal the concept's current `file_hash`, its model/dimension/contract
    /// must match the durable embedding policy, and the concept must be
    /// effectively fresh. Stale or legacy (NULL-provenance) rows never rank,
    /// even while the HNSW index physically retains them.
    // `query_embedding` is a `Vec<f32>` because that is the SQL `real[]` boundary
    // type; it is only borrowed into the impl, so pass-by-value is inherent to the
    // pgrx signature.
    #[allow(clippy::needless_pass_by_value)]
    #[pg_extern(stable, parallel_safe, requires = ["embedding_table"])]
    fn concept_search_semantic(
        query_embedding: Vec<f32>,
        bundle_id: default!(Option<i64>, "NULL"),
        limit_count: default!(i32, 10),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.concept_search_result")> {
        let hits = concept_search_semantic_impl(&query_embedding, bundle_id, limit_count)
            .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = hits
            .into_iter()
            .map(|hit| types::concept_search_result(hit).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    /// Rank concepts by Reciprocal Rank Fusion of lexical and semantic search.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Fuses the
    /// lexical result of `query` (through the configured `search_backend`) with
    /// the semantic result of `query_embedding` using RRF (k = 60), entirely in
    /// SQL. The `rank` column is the fused RRF score. Searches enabled bundles
    /// only; `limit_count` must lie in `1..=500`. The semantic component ranks
    /// eligible embeddings only (the same predicate `concept_search_semantic`
    /// applies), so an ineligible vector never leaks into the fused result.
    /// When `pgvector` is not installed this **degrades to lexical-only** with
    /// a `WARNING` (RRF needs no model, so lexical-only is a sensible fallback).
    // `query_embedding` is a `Vec<f32>` because that is the SQL `real[]` boundary
    // type; it is only borrowed into the impl, so pass-by-value is inherent to the
    // pgrx signature.
    #[allow(clippy::needless_pass_by_value)]
    // PARALLEL RESTRICTED like `concept_search`: the lexical half runs the
    // configured backend, which may execute the bm25 provider's scoring.
    #[pg_extern(stable, parallel_restricted, requires = ["embedding_table"])]
    fn concept_search_hybrid(
        query: &str,
        query_embedding: Vec<f32>,
        bundle_id: default!(Option<i64>, "NULL"),
        limit_count: default!(i32, 10),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.concept_search_result")> {
        let hits = concept_search_hybrid_impl(query, &query_embedding, bundle_id, limit_count)
            .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = hits
            .into_iter()
            .map(|hit| types::concept_search_result(hit).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    /// (Re)build the `pgvector` HNSW cosine index on `pgokf.concept_embedding`.
    ///
    /// Requires membership in `pgokf_admin`. When `pgvector` is installed this
    /// drops and recreates the HNSW index used to accelerate
    /// `concept_search_semantic` / `concept_search_hybrid`, built with the
    /// `embedding_dim` typmod, returning `true`. It is a no-op returning `false`
    /// (with a `NOTICE`) when `pgvector` is absent, or when `embedding_dim`
    /// exceeds pgvector's 2000-dimension HNSW limit (semantic search then uses an
    /// exact scan). Run it after enabling pgvector, after bulk-loading
    /// embeddings, or after changing `embedding_dim`.
    #[pg_extern(requires = ["embedding_table"])]
    fn rebuild_embedding_index() -> bool {
        rebuild().unwrap_or_else(|error| error.raise())
    }

    extension_sql!(
        r"
ALTER FUNCTION pgokf.set_concept_embedding(bigint, text, real[])
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.set_concept_embedding_cas(bigint, text, real[], text, text, text, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION pgokf.rebuild_embedding_index()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.set_concept_embedding_cas(bigint, text, real[], text, text, text, text) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.rebuild_embedding_index() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.set_concept_embedding_cas(bigint, text, real[], text, text, text, text) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) TO pgokf_reader;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) TO pgokf_reader;
GRANT EXECUTE ON FUNCTION pgokf.rebuild_embedding_index() TO pgokf_admin;
COMMENT ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) IS
    'Store or replace one concept''s embedding (real[]) streamed in by a companion embedder; the extension never computes embeddings. Writer-tier (pgokf_writer; admin inherits it), SECURITY DEFINER. Validates the concept exists and len(embedding)=embedding_dim (else 22023) and upserts. The vector is stored as real[] so pgokf needs no static pgvector dependency. This 0.2.0 compatibility signature carries no provenance, so the row it writes is a legacy row (model/source_file_hash/input_hash/contract all NULL, cleared on overwrite) that never ranks semantically; use pgokf.set_concept_embedding_cas for an eligible, provenance-carrying write.';
COMMENT ON FUNCTION pgokf.set_concept_embedding_cas(bigint, text, real[], text, text, text, text) IS
    'Store or replace one concept''s embedding with full provenance, compare-and-set against the concept''s current file_hash: the write commits only when the concept''s file_hash still equals expected_file_hash under a row lock, returning true; a mismatch returns false having written nothing (retryable - re-read and re-embed, never an error-loop). input_hash is the caller-computed hash of the exact bounded input text, model the embedding model, contract the render-contract identity; all provenance arguments must be non-empty (22023 otherwise, as for a wrong dimension or an unknown concept; 42501 outside pgokf_writer). Only rows written through this setter carry the provenance semantic ranking requires.';
COMMENT ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) IS
    'Semantic nearest-neighbor search: rank concepts by pgvector cosine distance to query_embedding (rank = normalized cosine similarity). Reader-level, invoker rights; active bundles only. query_embedding must have embedding_dim dimensions; limit_count in 1..=500. Requires pgvector: raises 22023 naming the missing dependency when it is not installed (no lexical fallback). Only ELIGIBLE embeddings rank: source_file_hash equal to the concept''s current file_hash, model/dimension/contract matching the embedding_model/embedding_dim/embedding_contract policy, and the concept effectively fresh (bundle freshness fresh, no covering concept/path override); a stale or legacy (NULL-provenance) row never ranks even while the HNSW index physically retains it.';
COMMENT ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) IS
    'Hybrid search: Reciprocal Rank Fusion (RRF, k=60) of the lexical result of query (via the configured search_backend) and the semantic result of query_embedding, fused entirely in SQL (rank = fused RRF score). Reader-level, invoker rights; enabled bundles only; limit_count in 1..=500. The semantic component ranks eligible (current, fresh) embeddings only, so an ineligible vector never leaks into the fused result; the lexical component may still return a stale concept, labeled by pgokf.concept_search_fresh. Degrades to lexical-only with a WARNING when pgvector is not installed.';
COMMENT ON FUNCTION pgokf.rebuild_embedding_index() IS
    'Admin-only. (Re)build the pgvector HNSW (cosine) index on pgokf.concept_embedding for the configured embedding_dim; returns true when built, or false (with a NOTICE) when pgvector is absent or embedding_dim exceeds pgvector''s 2000-dimension HNSW limit. The index physically retains ineligible rows; the ranking predicates, not the index, enforce eligibility.';
",
        name = "embedding_function_hardening",
        requires = [
            set_concept_embedding,
            set_concept_embedding_cas,
            concept_search_semantic,
            concept_search_hybrid,
            rebuild_embedding_index
        ]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_embedding_length_accepts_a_matching_length() {
        // Arrange / Act / Assert
        assert!(validate_embedding_length(1536, 1536).is_ok());
    }

    #[test]
    fn validate_embedding_length_rejects_a_mismatch() {
        // Arrange / Act
        let error = validate_embedding_length(768, 1536)
            .expect_err("a length that differs from embedding_dim must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
        assert!(error.message().contains("768"));
        assert!(error.message().contains("1536"));
    }

    #[test]
    fn validate_embedding_finite_accepts_all_finite_elements() {
        // Arrange / Act / Assert: an ordinary vector passes.
        assert!(validate_embedding_finite(&[0.0, -1.5, 3.25, 1e30]).is_ok());
    }

    #[test]
    fn validate_embedding_finite_rejects_nan_and_names_the_index() {
        // Arrange: a vector with a NaN in a known position.
        let embedding = [0.1, 0.2, f32::NAN, 0.4];

        // Act
        let error = validate_embedding_finite(&embedding)
            .expect_err("a NaN element must be rejected before storage");

        // Assert: invalid-parameter (22023), naming the offending index.
        assert_eq!(error.sqlstate(), "22023");
        assert!(error.message().contains("index 2"));
    }

    #[test]
    fn validate_embedding_finite_rejects_infinity() {
        // Arrange / Act
        let error = validate_embedding_finite(&[f32::INFINITY])
            .expect_err("an infinite element must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
        assert!(error.message().contains("index 0"));
    }

    #[test]
    fn missing_pgvector_error_is_invalid_parameter_and_names_the_dependency() {
        // Arrange / Act
        let error = missing_pgvector_error();

        // Assert
        assert_eq!(error.sqlstate(), "22023");
        assert!(error.message().contains("pgvector"));
    }

    #[test]
    fn rank_keys_from_hits_preserves_order() {
        // Arrange: two hits in rank order.
        let hits = vec![
            SearchHit {
                bundle_id: 1,
                concept_id: "alpha".to_owned(),
                path: "alpha.md".to_owned(),
                title: None,
                concept_type: None,
                rank: 0.9,
                headline: None,
            },
            SearchHit {
                bundle_id: 1,
                concept_id: "beta".to_owned(),
                path: "beta.md".to_owned(),
                title: None,
                concept_type: None,
                rank: 0.5,
                headline: None,
            },
        ];

        // Act
        let keys = RankKeys::from_hits(&hits);

        // Assert
        assert_eq!(keys.bundle_ids, vec![1, 1]);
        assert_eq!(
            keys.concept_ids,
            vec!["alpha".to_owned(), "beta".to_owned()]
        );
    }

    #[test]
    fn contract_match_sql_binds_the_policy_parameters_and_requires_provenance() {
        // Arrange / Act
        let predicate = contract_match_sql(4, 5, 6);

        // Assert: the hash gate, the always-pinned dimension, and the
        // optionally pinned model/contract all reference the numbered policy
        // parameters; a NULL (legacy) provenance value never satisfies it.
        assert!(predicate.contains("e.source_file_hash IS NOT NULL"));
        assert!(predicate.contains("e.source_file_hash = c.file_hash"));
        assert!(predicate.contains("($4 = '' OR e.model = $4)"));
        assert!(predicate.contains("e.dim = $5"));
        assert!(predicate.contains("($6 = '' OR e.contract = $6)"));
        assert!(predicate.contains("e.model IS NOT NULL"));
        assert!(predicate.contains("e.contract IS NOT NULL"));
    }

    #[test]
    fn validate_provenance_arg_rejects_empty_and_blank_values() {
        // Arrange / Act / Assert: ordinary values pass; empty and
        // whitespace-only provenance is rejected with 22023.
        assert!(validate_provenance_arg("model", "text-embedding-3-small").is_ok());
        for invalid in ["", "   "] {
            let error = validate_provenance_arg("model", invalid)
                .expect_err("empty provenance must be rejected");
            assert_eq!(error.sqlstate(), "22023");
            assert!(error.message().contains("model"));
        }
    }
}
