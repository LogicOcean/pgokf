//! Optional semantic (vector) and hybrid search over the catalog.
//!
//! This module adds three query surfaces plus the storage and index management
//! behind them, all built on `pgvector` — and, exactly like the `pg_search`
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
//! - [`set_concept_embedding`](pgokf::set_concept_embedding) — writer-tier
//!   ingest. A companion embedder (never this extension — it performs no model
//!   inference and no network I/O) streams caller-computed embeddings in as
//!   `real[]`; the row is validated (the concept must exist, the length must
//!   equal the durable `embedding_dim`) and upserted.
//! - [`concept_search_semantic`](pgokf::concept_search_semantic) — reader-tier
//!   nearest-neighbor search by `pgvector` cosine distance (`<=>`). Semantic
//!   search has no lexical equivalent, so when `pgvector` is absent it raises a
//!   clear `22023` naming the missing dependency rather than silently returning
//!   nothing.
//! - [`concept_search_hybrid`](pgokf::concept_search_hybrid) — reader-tier
//!   Reciprocal Rank Fusion (RRF, k = 60) of the lexical result (through the
//!   configured `search_backend`) and the semantic result, fused entirely in
//!   SQL. RRF needs no model, so when `pgvector` is absent this **sensibly**
//!   degrades to lexical-only with a `WARNING`.
//!
//! Plus [`rebuild_embedding_index`](pgokf::rebuild_embedding_index) — admin-tier,
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
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

COMMENT ON TABLE pgokf.concept_embedding IS
    'Opt-in per-concept embedding vectors, streamed in by a companion embedder via pgokf.set_concept_embedding (the extension never computes embeddings or performs network I/O). The vector is stored as the builtin real[] — NOT a pgvector ''vector'' column — so CREATE EXTENSION pgokf succeeds without pgvector installed; it is cast to vector(dim) at query time and in the HNSW index only when pgvector is present. Rows cascade from pgokf.concepts, so removing a concept or unregistering a bundle drops its embedding automatically.';
COMMENT ON COLUMN pgokf.concept_embedding.embedding IS
    'The caller-computed embedding as real[]. Its length must equal the durable embedding_dim configuration key at ingest time (enforced by pgokf.set_concept_embedding); dim records that length redundantly for a size-only read.';
COMMENT ON COLUMN pgokf.concept_embedding.dim IS
    'Length of embedding, constrained equal to cardinality(embedding); the effective dimension of the stored vector.';
COMMENT ON COLUMN pgokf.concept_embedding.model IS
    'Optional identifier of the embedding model/producer that computed the vector, for provenance; NULL when not supplied.';
COMMENT ON COLUMN pgokf.concept_embedding.updated_at IS
    'When this embedding row was last written by pgokf.set_concept_embedding.';
COMMENT ON COLUMN pgokf.concept_embedding.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';

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
/// The probe is by `pg_extension` catalog membership — never by attempting to
/// use a `vector`-typed expression — so it is safe to call before any SQL that
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

/// The effective `embedding_dim` for an invoker-rights reader path, read through
/// the reader-granted `SECURITY DEFINER` `pgokf.get_config` projection (the same
/// indirection `concept_search` uses for `search_backend`, because these search
/// functions cannot read the admin-only config table directly).
fn effective_embedding_dim() -> Result<i32, CatalogError> {
    Spi::get_one::<i32>("SELECT (pgokf.get_config() ->> 'embedding_dim')::pg_catalog.int4")
        .map_err(spi_error("failed to read embedding_dim configuration"))?
        .ok_or_else(|| {
            CatalogError::internal("embedding_dim is missing from configuration", Path::new(""))
        })
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

/// Run the nearest-neighbor query, ordered by `pgvector` cosine distance, with a
/// normalized cosine-similarity score. Assumes `pgvector` is present and the
/// query embedding length equals `dim` (both checked by the callers).
///
/// `dim` is a trusted `integer` from validated configuration (never caller
/// input), so formatting it into the `vector(dim)` typmod is injection-safe — an
/// `i32` can only render as digits — and is required because a typmod cannot be
/// a bound parameter. The query embedding is bound as `$1` (never interpolated).
/// Both the stored column and the query vector cast through the identical
/// `embedding::vector(dim)` expression the HNSW index is built on, so the index
/// serves the ordering when present.
fn run_semantic_query(
    query_embedding: &[f32],
    bundle_id: Option<i64>,
    limit: i64,
    dim: i32,
) -> Result<Vec<SearchHit>, CatalogError> {
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
         WHERE ($2 IS NULL OR c.bundle_id = $2)
         ORDER BY e.embedding::vector({dim}) <=> $1::vector({dim})
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
             updated_at = pg_catalog.now()",
        &[
            bundle_id.into(),
            concept_id.into(),
            embedding.into(),
            dim.into(),
        ],
    )
    .map_err(spi_error("failed to upsert concept embedding"))
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
    let dim = effective_embedding_dim()?;
    validate_embedding_length(query_embedding.len(), dim)?;
    run_semantic_query(query_embedding, bundle_id, limit, dim)
}

/// The (`bundle_id`, `concept_id`) key of a ranked hit, in rank order — the RRF
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
/// only one. Only the two key arrays cross the boundary — the fusion arithmetic
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
         ORDER BY f.score DESC, c.id
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
    let semantic = if pgvector_installed()? {
        let dim = effective_embedding_dim()?;
        validate_embedding_length(query_embedding.len(), dim)?;
        let semantic_hits = run_semantic_query(query_embedding, bundle_id, limit, dim)?;
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
        set_concept_embedding_impl,
    };
    use crate::catalog::types;

    /// Store (or replace) a concept's embedding vector.
    ///
    /// Requires membership in `pgokf_writer` (an admin qualifies by
    /// inheritance). `embedding` is a `real[]` whose length must equal the
    /// durable `embedding_dim` configuration key; the concept must already
    /// exist. This is how a companion embedder streams caller-computed vectors
    /// in — the extension never computes embeddings and performs no network I/O.
    /// Raises SQLSTATE `22023` on a wrong length or an unknown concept, and
    /// `42501` for a caller outside `pgokf_writer`.
    #[pg_extern(requires = ["embedding_table"])]
    fn set_concept_embedding(bundle_id: i64, concept_id: &str, embedding: Vec<f32>) {
        set_concept_embedding_impl(bundle_id, concept_id, embedding)
            .unwrap_or_else(|error| error.raise());
    }

    /// Rank concepts by semantic similarity to a query embedding.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Orders by
    /// `pgvector` cosine distance (`<=>`) over stored concept embeddings; the
    /// `rank` column is the normalized cosine similarity (`1 - distance`).
    /// `query_embedding` must have `embedding_dim` dimensions. Searches enabled
    /// bundles only; `limit_count` must lie in `1..=500`. **Requires pgvector**:
    /// raises SQLSTATE `22023` naming the missing dependency when the `pgvector`
    /// extension is not installed (semantic search has no lexical fallback — use
    /// `pgokf.concept_search` for that).
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
    /// only; `limit_count` must lie in `1..=500`. When `pgvector` is not
    /// installed this **degrades to lexical-only** with a `WARNING` (RRF needs
    /// no model, so lexical-only is a sensible fallback).
    // `query_embedding` is a `Vec<f32>` because that is the SQL `real[]` boundary
    // type; it is only borrowed into the impl, so pass-by-value is inherent to the
    // pgrx signature.
    #[allow(clippy::needless_pass_by_value)]
    #[pg_extern(stable, parallel_safe, requires = ["embedding_table"])]
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
ALTER FUNCTION pgokf.rebuild_embedding_index()
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.rebuild_embedding_index() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) TO pgokf_writer;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) TO pgokf_reader;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) TO pgokf_reader;
GRANT EXECUTE ON FUNCTION pgokf.rebuild_embedding_index() TO pgokf_admin;
COMMENT ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) IS
    'Store or replace one concept''s embedding (real[]) streamed in by a companion embedder; the extension never computes embeddings. Writer-tier (pgokf_writer; admin inherits it), SECURITY DEFINER. Validates the concept exists and len(embedding)=embedding_dim (else 22023) and upserts. The vector is stored as real[] so pgokf needs no static pgvector dependency.';
COMMENT ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) IS
    'Semantic nearest-neighbor search: rank concepts by pgvector cosine distance to query_embedding (rank = normalized cosine similarity). Reader-level, invoker rights; enabled bundles only. query_embedding must have embedding_dim dimensions; limit_count in 1..=500. Requires pgvector: raises 22023 naming the missing dependency when it is not installed (no lexical fallback).';
COMMENT ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) IS
    'Hybrid search: Reciprocal Rank Fusion (RRF, k=60) of the lexical result of query (via the configured search_backend) and the semantic result of query_embedding, fused entirely in SQL (rank = fused RRF score). Reader-level, invoker rights; enabled bundles only; limit_count in 1..=500. Degrades to lexical-only with a WARNING when pgvector is not installed.';
COMMENT ON FUNCTION pgokf.rebuild_embedding_index() IS
    'Admin-only. (Re)build the pgvector HNSW (cosine) index on pgokf.concept_embedding for the configured embedding_dim; returns true when built, or false (with a NOTICE) when pgvector is absent or embedding_dim exceeds pgvector''s 2000-dimension HNSW limit.';
",
        name = "embedding_function_hardening",
        requires = [
            set_concept_embedding,
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
}
