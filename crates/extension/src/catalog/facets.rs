//! Faceted result counts over the search set: `pgokf.search_facets`.
//!
//! A search UI often needs the *breakdown* of a result set, not the rows — "42
//! runbooks, 15 wikis", or the counts per bundle, status, trust tier, or tag —
//! so it can render filter chips before the user drills in. `search_facets`
//! answers exactly that: it counts the **same matching set** `pgokf.concept_search`
//! would (the native full-text match plus the identical structured filters),
//! grouped by one chosen facet.
//!
//! # The facet allow-list (never interpolated)
//!
//! The `facet` argument selects the grouping dimension from a fixed allow-list —
//! `type`, `bundle`, `status`, `trust_tier`, `tag` — validated against
//! [`Facet::parse`] (SQLSTATE `22023` otherwise). The facet is **dispatched on**,
//! never interpolated into SQL: each variant maps to one of a small set of
//! compile-time-constant queries whose grouping column is fixed in source, so no
//! caller string ever reaches the query text. Every value that *is* caller input
//! (the query, the bundle scope, and the four filters) binds as a parameter.
//!
//! # Security model
//!
//! Reader-tier and **invoker rights**, mirroring `concept_search`: it reads only
//! `pgokf.concepts` / `pgokf.bundles` / `pgokf.concept_provenance`, which
//! `pgokf_reader` already holds `SELECT` on, so row-level security filters the
//! scan to the session's tenant and escalating to the owner would grant nothing.

use std::path::Path;

use pgrx::heap_tuple::PgHeapTuple;
use pgrx::{AllocatedByRust, Spi};

use crate::catalog::search;
use crate::catalog::spi_read::RowReader;
use crate::errors::CatalogError;
use crate::security;

/// Qualified SQL name of the facet-count composite type.
const SEARCH_FACET_TYPE: &str = "pgokf.search_facet";

/// One facet bucket — a distinct facet value and how many matching concepts
/// carry it — prior to being packed into the `pgokf.search_facet` composite.
struct FacetCount {
    facet_value: String,
    count: i64,
}

/// The grouping dimension of a faceted count, drawn from a fixed allow-list.
///
/// Each variant owns the *constant* SQL fragments its query needs — the grouped
/// value expression and any extra `FROM` join — so the caller-supplied facet name
/// never becomes SQL text; it only selects which pre-written fragment runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Facet {
    /// Group by the OKF concept `type`.
    Type,
    /// Group by the owning `bundle_id` (rendered as text).
    Bundle,
    /// Group by the OKF lifecycle `status` from `concept_provenance`.
    Status,
    /// Group by the derived `trust_tier` from `concept_provenance`.
    TrustTier,
    /// Group by individual `tag` (a concept is counted once per tag it carries).
    Tag,
}

impl Facet {
    /// Parse a caller-supplied facet name, rejecting anything outside the
    /// allow-list with SQLSTATE `22023`.
    fn parse(facet: &str) -> Result<Self, CatalogError> {
        match facet {
            "type" => Ok(Self::Type),
            "bundle" => Ok(Self::Bundle),
            "status" => Ok(Self::Status),
            "trust_tier" => Ok(Self::TrustTier),
            "tag" => Ok(Self::Tag),
            other => Err(CatalogError::invalid_parameter(
                format!(
                    "facet must be one of 'type', 'bundle', 'status', 'trust_tier', 'tag', \
                     got {other}"
                ),
                Path::new(""),
            )),
        }
    }

    /// The fixed SQL expression this facet groups and counts by. A compile-time
    /// constant per variant — never caller input.
    const fn value_expr(self) -> &'static str {
        match self {
            Self::Type => "c.type",
            Self::Bundle => "(c.bundle_id)::pg_catalog.text",
            Self::Status => "cp.status",
            Self::TrustTier => "cp.trust_tier",
            Self::Tag => "tg.tag",
        }
    }

    /// The extra `FROM` fragment this facet needs. Only `tag` adds one — a
    /// `LATERAL unnest` that expands a concept's `tags` array so each tag is
    /// counted separately.
    const fn extra_from(self) -> &'static str {
        match self {
            Self::Tag => "CROSS JOIN LATERAL pg_catalog.unnest(c.tags) AS tg(tag)",
            _ => "",
        }
    }
}

/// Build the faceted-count query for `facet`.
///
/// The query mirrors the native `concept_search` matching set — the weighted
/// `body_tsv` FTS match plus the four structured filters — then groups by the
/// facet's fixed value expression. Only the two constant fragments
/// ([`Facet::value_expr`], [`Facet::extra_from`]) vary by facet, both chosen from
/// source, so no caller string is interpolated. The query text (`$1`), regconfig
/// (`$2`), bundle scope (`$3`), and the four filters (`$4`..`$7`) all bind as
/// parameters.
fn build_facets_query(facet: Facet) -> String {
    let value_expr = facet.value_expr();
    let extra_from = facet.extra_from();
    format!(
        "SELECT {value_expr} AS facet_value, pg_catalog.count(*) AS facet_count
         FROM pgokf.concepts c
         JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
         LEFT JOIN pgokf.concept_provenance cp
                ON cp.bundle_id = c.bundle_id AND cp.concept_id = c.id
         CROSS JOIN pg_catalog.websearch_to_tsquery($2::pg_catalog.regconfig, $1) AS q(query)
         {extra_from}
         WHERE c.body_tsv @@ q.query
           AND ($3 IS NULL OR c.bundle_id = $3)
           AND ($4 IS NULL OR c.type = $4)
           AND ($5 IS NULL OR c.tags @> $5)
           AND ($6 IS NULL OR cp.status = $6)
           AND ($7 IS NULL OR cp.trust_tier = $7)
           AND {value_expr} IS NOT NULL
         GROUP BY {value_expr}
         ORDER BY facet_count DESC, facet_value"
    )
}

/// Authorize (reader), validate, and run the faceted count.
fn search_facets_impl(
    query: &str,
    bundle_id: Option<i64>,
    facet: &str,
    concept_type: Option<&str>,
    tags: Option<&[String]>,
    status: Option<&str>,
    trust_tier: Option<&str>,
) -> Result<Vec<FacetCount>, CatalogError> {
    security::authorize_current_user(security::Operation::Search, Path::new(""))?;
    search::validate_query(query)?;
    let facet = Facet::parse(facet)?;
    // An empty tag slice is a true no-op (see Filters::new); normalize it so a
    // `Some([])` never binds `'{}'::text[]` and drops untagged concepts.
    let tags = tags.filter(|slice| !slice.is_empty());
    let text_search_config = search::effective_text_search_config()?;
    let statement = build_facets_query(facet);

    Spi::connect(|client| {
        let table = client
            .select(
                &statement,
                None,
                &[
                    query.into(),
                    text_search_config.as_str().into(),
                    bundle_id.into(),
                    concept_type.into(),
                    tags.map(<[String]>::to_vec).into(),
                    status.into(),
                    trust_tier.into(),
                ],
            )
            .map_err(|error| {
                CatalogError::internal(
                    format!("faceted search query failed: {error}"),
                    Path::new(""),
                )
            })?;
        let mut counts = Vec::with_capacity(table.len());
        for row in table {
            let reader = RowReader::new(&row, "failed to read search_facet column", "search_facet");
            counts.push(FacetCount {
                facet_value: reader.required(1, "facet_value")?,
                count: reader.required(2, "count")?,
            });
        }
        Ok(counts)
    })
}

fn composite_error(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::internal(
        format!("failed to build {SEARCH_FACET_TYPE} composite: {error}"),
        Path::new(""),
    )
}

/// Pack a [`FacetCount`] into a `pgokf.search_facet` heap tuple.
fn search_facet_tuple(
    count: FacetCount,
) -> Result<PgHeapTuple<'static, AllocatedByRust>, CatalogError> {
    let mut tuple = PgHeapTuple::new_composite_type(SEARCH_FACET_TYPE).map_err(composite_error)?;
    tuple
        .set_by_name("facet_value", count.facet_value)
        .map_err(composite_error)?;
    tuple
        .set_by_name("count", count.count)
        .map_err(composite_error)?;
    Ok(tuple)
}

/// SQL-facing faceted-count entry point, installed into the `pgokf` schema.
#[pgrx::pg_schema]
mod pgokf {
    use pgrx::iter::SetOfIterator;
    use pgrx::{default, extension_sql, pg_extern};

    use super::{search_facet_tuple, search_facets_impl};

    extension_sql!(
        r"
CREATE TYPE pgokf.search_facet AS (
    facet_value text,
    count       bigint
);

COMMENT ON TYPE pgokf.search_facet IS
    'One faceted-count bucket from pgokf.search_facets: a distinct facet value (a type, bundle id, status, trust tier, or tag) and how many matching concepts carry it.';
",
        name = "search_facet_type",
        requires = ["catalog_tables", "provenance_table"]
    );

    /// Count the `concept_search` matching set grouped by one facet.
    ///
    /// Requires membership in `pgokf_reader` (or `pgokf_admin`). Counts exactly
    /// the concepts `pgokf.concept_search` would match — the native full-text
    /// match of `query` plus the same optional structured filters
    /// (`concept_type`, `tags` ALL-of containment, `status`, `trust_tier`) — and
    /// groups them by `facet`, one of `type`, `bundle`, `status`, `trust_tier`,
    /// or `tag` (any other value raises SQLSTATE `22023`). The `tag` facet counts
    /// a concept once per tag it carries. Buckets return ordered by descending
    /// count then facet value; NULL facet values are omitted. Searches active
    /// bundles only (enabled AND not retired).
    // `tags` is a `Vec<String>` because that is the SQL `text[]` boundary type;
    // it is only borrowed (`as_deref`) into the impl, so pass-by-value is inherent
    // to the pgrx signature.
    #[allow(clippy::needless_pass_by_value)]
    #[pg_extern(stable, parallel_safe, requires = ["search_facet_type"])]
    fn search_facets(
        query: &str,
        bundle_id: default!(Option<i64>, "NULL"),
        facet: default!(&str, "'type'"),
        concept_type: default!(Option<&str>, "NULL"),
        tags: default!(Option<Vec<String>>, "NULL"),
        status: default!(Option<&str>, "NULL"),
        trust_tier: default!(Option<&str>, "NULL"),
    ) -> SetOfIterator<'static, pgrx::composite_type!('static, "pgokf.search_facet")> {
        let counts = search_facets_impl(
            query,
            bundle_id,
            facet,
            concept_type,
            tags.as_deref(),
            status,
            trust_tier,
        )
        .unwrap_or_else(|error| error.raise());
        let rows: Vec<_> = counts
            .into_iter()
            .map(|count| search_facet_tuple(count).unwrap_or_else(|error| error.raise()))
            .collect();
        SetOfIterator::new(rows)
    }

    extension_sql!(
        r"
REVOKE ALL ON FUNCTION pgokf.search_facets(text, bigint, text, text, text[], text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.search_facets(text, bigint, text, text, text[], text, text) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.search_facets(text, bigint, text, text, text[], text, text) IS
    'Count the concept_search matching set (native FTS match of query plus the same optional concept_type/tags/status/trust_tier filters) grouped by facet, as pgokf.search_facet. facet is one of type, bundle, status, trust_tier, tag (else 22023); the facet is dispatched on, never interpolated. Reader-level, STABLE, invoker rights (RLS-filtered to the tenant); active bundles only. The tag facet counts a concept once per tag; NULL facet values are omitted; ordered by count DESC then value.';
",
        name = "search_facets_hardening",
        requires = [search_facets]
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facet_parse_accepts_every_allowed_facet() {
        // Arrange
        let expected = [
            ("type", Facet::Type),
            ("bundle", Facet::Bundle),
            ("status", Facet::Status),
            ("trust_tier", Facet::TrustTier),
            ("tag", Facet::Tag),
        ];

        for (name, facet) in expected {
            // Act
            let parsed = Facet::parse(name).expect("allow-listed facet must parse");

            // Assert
            assert_eq!(parsed, facet);
        }
    }

    #[test]
    fn facet_parse_rejects_an_unknown_facet_as_invalid_parameter() {
        // Arrange / Act
        let error = Facet::parse("bogus").expect_err("an off-list facet must be rejected");

        // Assert
        assert_eq!(error.sqlstate(), "22023");
        assert!(error.message().contains("bogus"));
    }

    #[test]
    fn build_facets_query_uses_the_lateral_unnest_only_for_the_tag_facet() {
        // Arrange / Act / Assert: the tag facet needs the LATERAL unnest join;
        // the others must not add it.
        assert!(build_facets_query(Facet::Tag).contains("unnest"));
        assert!(!build_facets_query(Facet::Type).contains("unnest"));
    }

    #[test]
    fn build_facets_query_groups_by_the_facet_value_expression() {
        // Arrange / Act
        let query = build_facets_query(Facet::Bundle);

        // Assert: the bundle facet groups by the bundle_id-as-text expression.
        assert!(query.contains("(c.bundle_id)::pg_catalog.text"));
        assert!(query.contains("GROUP BY"));
    }
}
