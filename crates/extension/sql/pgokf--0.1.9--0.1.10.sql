-- pgokf extension upgrade: 0.1.9 -> 0.1.10
--
-- This upgrade carries the COMPLETE delta from a fresh 0.1.9 install up to a
-- fresh 0.1.10 install, so that `ALTER EXTENSION pgokf UPDATE TO '0.1.10'` yields
-- a catalog functionally identical to `CREATE EXTENSION pgokf` at 0.1.10. The
-- whole file runs in a single transaction.
--
-- 0.1.10 is one additive OKF-conformance feature batch:
--   F1 Attested Computation type-specific fields as graph edges — the
--      computation / executor / attester references of an Attested Computation
--      concept are resolved into pgokf.links as typed, traversable internal
--      edges. A new additive pgokf.links.link_relation column carries the
--      relation ('reference' for every ordinary link; 'attestation:*' for the
--      new edges).
--   F2 reserved log.md projection — the per-directory OKF log.md activity logs,
--      previously dropped, are now projected into a new pgokf.bundle_log table
--      and read through pgokf.list_bundle_log.
--
-- Behavior that lives entirely in the 0.1.10 shared library needs no SQL here:
-- the attestation-edge resolution is compiled into the links projection, and the
-- log.md read/parse/project into the sync engine. Loading the new module on
-- update activates both. This script only creates the SQL objects those code
-- paths read and write. Every statement is additive and non-destructive: no row
-- is truncated or deleted, and no data-bearing object is dropped. The one column
-- added to pgokf.links is backfilled by its DEFAULT, so existing links keep the
-- 'reference' relation a fresh install gives them; the attestation edges of an
-- already-registered Attested Computation concept appear on its next refresh,
-- exactly as a fresh install only projects them at registration time.

-- ===========================================================================
-- F1: the pgokf.links.link_relation column.
--
-- Appended (ALTER TABLE ... ADD COLUMN) after tenant_id, matching the column
-- layout of the fresh 0.1.10 pgokf.links table (whose CREATE TABLE likewise
-- places link_relation last). The NOT NULL DEFAULT 'reference' backfills every
-- existing link row with the same relation a fresh install assigns an ordinary
-- Markdown link, so upgraded == fresh. IF NOT EXISTS keeps the step idempotent.
-- ===========================================================================
ALTER TABLE pgokf.links
    ADD COLUMN IF NOT EXISTS link_relation text NOT NULL DEFAULT 'reference';

COMMENT ON COLUMN pgokf.links.link_relation IS
    'Semantic relation the edge represents, distinct from the Markdown construct in link_kind. ''reference'' (the default) for every ordinary Markdown link; for an Attested Computation concept''s type-specific reference fields, ''attestation:computation'', ''attestation:executor'', or ''attestation:attester'', so a reader can SELECT the typed edges while concept_neighbors traverses them like any resolved internal edge.';

-- The ordinal comment is refreshed to document that frontmatter-derived
-- attestation edges are numbered after a source's body links.
COMMENT ON COLUMN pgokf.links.ordinal IS
    'Zero-based position of the link within its source document, in document order. Frontmatter-derived attestation edges are numbered after the body links of the same source (from the body link count upward), so they never collide on the (bundle_id, source_id, ordinal) key.';

-- ===========================================================================
-- F2, Step 1: the pgokf.bundle_log projection table.
--
-- One row per parsed entry of a reserved per-directory log.md, keyed by the
-- containing directory and a zero-based in-file ordinal. Cascades from
-- pgokf.bundles. Opt-in-by-usage multi-tenant row-level security on the
-- denormalized tenant_id, matching pgokf.links. Reproduces, statement for
-- statement, the fresh 0.1.10 bundle_log_table block.
-- ===========================================================================
CREATE TABLE pgokf.bundle_log (
    bundle_id bigint      NOT NULL,
    tenant_id text        NOT NULL DEFAULT 'default',
    directory text        NOT NULL,
    ordinal   integer     NOT NULL,
    logged_at timestamptz,
    entry     text        NOT NULL,
    CONSTRAINT bundle_log_pkey PRIMARY KEY (bundle_id, directory, ordinal),
    CONSTRAINT bundle_log_bundle_fk
        FOREIGN KEY (bundle_id)
        REFERENCES pgokf.bundles (id)
        ON DELETE CASCADE
);

ALTER TABLE pgokf.bundle_log ENABLE ROW LEVEL SECURITY;
CREATE POLICY bundle_log_tenant_isolation ON pgokf.bundle_log
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

COMMENT ON TABLE pgokf.bundle_log IS
    'Projection of the reserved OKF per-directory log.md activity logs of a bundle: one row per parsed log entry, keyed by the containing directory and a zero-based ordinal. Reserved log.md files are never concepts; this table is the only place they are projected. Replaced wholesale on every sync so it tracks the files, and cascades from pgokf.bundles.';
COMMENT ON COLUMN pgokf.bundle_log.bundle_id IS
    'Bundle the log entry belongs to (references pgokf.bundles.id; ON DELETE CASCADE).';
COMMENT ON COLUMN pgokf.bundle_log.tenant_id IS
    'Multi-tenant owner, denormalized from the entry''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.bundle_log.directory IS
    'Bundle-relative directory that contained the log.md this entry came from; the empty string for a root-level log.md. Part of the primary key.';
COMMENT ON COLUMN pgokf.bundle_log.ordinal IS
    'Zero-based position of the entry within its directory''s log.md, in file order; part of the primary key.';
COMMENT ON COLUMN pgokf.bundle_log.logged_at IS
    'The entry''s leading ISO 8601 timestamp (after any Markdown bullet/heading marker), parsed to timestamptz; NULL when the entry carries no parseable leading timestamp.';
COMMENT ON COLUMN pgokf.bundle_log.entry IS
    'The log entry text, stored losslessly as the trimmed source line (including any leading timestamp).';

GRANT SELECT ON pgokf.bundle_log TO pgokf_reader;

-- ===========================================================================
-- F2, Step 2: the pgokf.bundle_log_entry composite and the reader-level
-- pgokf.list_bundle_log projection.
--
-- INVOKER rights (no SECURITY DEFINER) over the public projection table, so the
-- caller's own opt-in tenant row-level security applies — matching
-- concept_neighbors. Reproduces the fresh 0.1.10 bundle_log_entry_type and
-- bundle_log_function_hardening blocks and the pgrx-generated CREATE FUNCTION.
-- ===========================================================================
CREATE TYPE pgokf.bundle_log_entry AS (
    bundle_id bigint,
    directory text,
    ordinal   integer,
    logged_at timestamptz,
    entry     text
);

COMMENT ON TYPE pgokf.bundle_log_entry IS
    'One reserved-log.md activity-log entry from pgokf.list_bundle_log: the bundle, the containing directory (empty string at the root), the zero-based in-file ordinal, the parsed leading timestamp (NULL when absent), and the lossless entry text.';

CREATE FUNCTION pgokf."list_bundle_log"(
    "bundle_id" bigint,
    "directory" text DEFAULT NULL,
    "max_rows" integer DEFAULT 500
) RETURNS SETOF pgokf.bundle_log_entry
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'list_bundle_log_wrapper';

REVOKE ALL ON FUNCTION pgokf.list_bundle_log(bigint, text, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_bundle_log(bigint, text, integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.list_bundle_log(bigint, text, integer) IS
    'List a bundle''s reserved-log.md activity-log entries as pgokf.bundle_log_entry, ordered by directory then ordinal and bounded by max_rows. Reader-level, STABLE, invoker rights (the caller''s tenant row-level security applies); optionally scoped to one directory. Raises 22023 when max_rows < 0.';
