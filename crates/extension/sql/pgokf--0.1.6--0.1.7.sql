-- pgokf extension upgrade: 0.1.6 -> 0.1.7
--
-- This upgrade carries the COMPLETE delta from a fresh 0.1.6 install up to a
-- fresh 0.1.7 install, so that `ALTER EXTENSION pgokf UPDATE TO '0.1.7'` yields a
-- catalog functionally identical to `CREATE EXTENSION pgokf` at 0.1.7. The whole
-- file runs in a single transaction.
--
-- 0.1.7 adds OPT-IN multi-tenant isolation with strict backward compatibility.
-- The behavior is: a denormalized `tenant_id` on every projection table, a
-- per-session `pgokf.tenant` GUC (registered by the 0.1.7 shared library at
-- load), and a row-level-security policy on each table whose predicate is a
-- no-op for any session that has NOT set `pgokf.tenant` - so an upgraded install,
-- and every existing session, sees ALL rows and behaves EXACTLY as under 0.1.6.
-- A session that DOES set `pgokf.tenant` sees only that tenant's rows.
--
-- All of it is additive and non-destructive with the single, carefully-justified
-- exception in Step 2 (the bundles UNIQUE key swap; see there): every existing
-- row backfills to the tenant 'default', nothing is truncated, and no row is
-- deleted. Behavior that lives entirely in the shared library - stamping writes
-- from `effective_tenant()`, tenant-scoping the SECURITY DEFINER readers
-- (`list_sync_log`, `health`), and registering the `pgokf.tenant` GUC - needs no
-- SQL here; this script only creates the SQL objects those code paths read and
-- write (the `tenant_id` columns, the `effective_tenant()` helper, the policies).

-- ===========================================================================
-- Step 1: the denormalized tenant_id column on every projection table.
--
-- Added last with its NOT NULL default 'default', so every existing row
-- backfills non-destructively to the 'default' tenant and a fresh 0.1.7 install
-- (which appends the same column last) is column-for-column identical. IF NOT
-- EXISTS keeps re-running idempotent.
-- ===========================================================================
ALTER TABLE pgokf.bundles
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
ALTER TABLE pgokf.concepts
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
ALTER TABLE pgokf.concept_metadata
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
ALTER TABLE pgokf.links
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
ALTER TABLE pgokf.concept_provenance
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
ALTER TABLE pgokf.concept_verification
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
ALTER TABLE pgokf.concept_provenance_source
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
ALTER TABLE pgokf.concept_source
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
ALTER TABLE pgokf.concept_embedding
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';
ALTER TABLE pgokf_private.sync_log
    ADD COLUMN IF NOT EXISTS tenant_id text NOT NULL DEFAULT 'default';

COMMENT ON COLUMN pgokf.bundles.tenant_id IS
    'Multi-tenant owner of this bundle, stamped at registration from pgokf.tenant (effective_tenant(); ''default'' for a session that set no tenant). A bundle is single-tenant and its tenant never changes on refresh/unregister/enable; combined with path it forms the per-tenant registration key UNIQUE (tenant_id, path), so two tenants may register the same filesystem or content:<name> path. The row-level-security policy shows it only to a matching or unset pgokf.tenant.';
COMMENT ON COLUMN pgokf.concepts.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle so the row-level-security predicate is local and index-friendly; always equals pgokf.bundles.tenant_id for the concept''s bundle.';
COMMENT ON COLUMN pgokf.concept_metadata.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.links.tenant_id IS
    'Multi-tenant owner, denormalized from the edge''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.concept_provenance.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.concept_verification.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.concept_provenance_source.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.concept_source.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf.concept_embedding.tenant_id IS
    'Multi-tenant owner, denormalized from the concept''s bundle for a local row-level-security predicate; always equals the bundle''s tenant_id.';
COMMENT ON COLUMN pgokf_private.sync_log.tenant_id IS
    'Multi-tenant owner of the operation, stamped from pgokf.tenant (effective_tenant(); ''default'' when unset). The table stays administrator-only (no row-level security); the reader-facing pgokf.list_sync_log applies the same opt-in tenant filter so a tenant session sees only its own audit rows.';

-- Index the RLS discriminator on concepts (the highest-cardinality projection
-- table), matching the fresh 0.1.7 schema. On pgokf.bundles the new
-- UNIQUE (tenant_id, path) index (Step 2) already leads with tenant_id.
CREATE INDEX IF NOT EXISTS concepts_tenant_id_idx ON pgokf.concepts (tenant_id);

-- ===========================================================================
-- Step 2: the per-tenant bundle registration key.
--
-- 0.1.6 keyed bundles on UNIQUE (path); 0.1.7 keys them on UNIQUE (tenant_id,
-- path) so two tenants can register the same filesystem or content:<name> path.
-- The old single-column key must be REPLACED, not merely supplemented: leaving it
-- would forbid a second tenant from registering a path the first already holds,
-- defeating the feature. This is the one deliberate exception to the file's
-- otherwise purely-additive rule, and it is NOT data-destructive - a UNIQUE
-- CONSTRAINT is metadata, not data; no row is touched; and the replacement
-- UNIQUE (tenant_id, path) is a STRICT SUPERSET, satisfied by every existing row
-- (all backfilled to tenant_id = 'default', where (tenant_id, path) is unique
-- exactly because (path) was). Both statements are guarded so re-running is
-- idempotent. The result is identical to the fresh 0.1.7 constraint set.
ALTER TABLE pgokf.bundles DROP CONSTRAINT IF EXISTS bundles_path_key;

DO $pgokf_bundles_tenant_path_key$
BEGIN
    ALTER TABLE pgokf.bundles
        ADD CONSTRAINT bundles_tenant_path_key UNIQUE (tenant_id, path);
EXCEPTION WHEN duplicate_table OR duplicate_object THEN
    NULL;
END
$pgokf_bundles_tenant_path_key$;

-- ===========================================================================
-- Step 3: the multi-tenant write helper (pgokf_private.effective_tenant()).
--
-- Resolves the tenant a write is stamped with from the per-session pgokf.tenant
-- GUC (unset/empty -> 'default'). Lives in the administrator-only pgokf_private
-- schema so it never widens the public pgokf API surface; called only from the
-- SECURITY DEFINER write functions. CREATE OR REPLACE keeps re-running idempotent.
-- ===========================================================================
CREATE OR REPLACE FUNCTION pgokf_private.effective_tenant() RETURNS text
    LANGUAGE sql
    STABLE
    AS $$
        SELECT coalesce(
            nullif(pg_catalog.current_setting('pgokf.tenant', true), ''),
            'default')
    $$;

REVOKE ALL ON FUNCTION pgokf_private.effective_tenant() FROM PUBLIC;

COMMENT ON FUNCTION pgokf_private.effective_tenant() IS
    'Resolve the tenant that a catalog write is stamped with from the per-session pgokf.tenant GUC: an unset or empty value yields the literal ''default'' (matching the tenant_id column default and the pre-multi-tenancy behavior), any other value is the active tenant. Internal helper for the SECURITY DEFINER write paths; the row-level-security policies inline current_setting instead so readers need no access to pgokf_private.';

-- ===========================================================================
-- Step 4: enable row-level security and install the opt-in isolation policy on
-- every projection table.
--
-- The predicate is the backward-compatible core: a session that has NOT set
-- pgokf.tenant (current_setting returns NULL or '') matches every row (unchanged
-- behavior for every existing install and session), and a session that HAS set it
-- matches only its tenant's rows. RLS is enabled but NOT forced, so the SECURITY
-- DEFINER write/admin functions (running as the table owner) bypass it and may
-- stamp and read across tenants - correct because each operates strictly within
-- one single-tenant bundle. The matching WITH CHECK constrains any future
-- invoker-side write to the active tenant. sync_log is deliberately excluded: it
-- stays administrator-only with no RLS, and pgokf.list_sync_log applies the same
-- filter explicitly. These statements are textually identical to the fresh 0.1.7
-- schema, and the whole update is one transaction (a failure rolls the batch back
-- cleanly), so no per-statement guard is needed.
ALTER TABLE pgokf.bundles ENABLE ROW LEVEL SECURITY;
CREATE POLICY bundles_tenant_isolation ON pgokf.bundles
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concepts ENABLE ROW LEVEL SECURITY;
CREATE POLICY concepts_tenant_isolation ON pgokf.concepts
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_metadata ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_metadata_tenant_isolation ON pgokf.concept_metadata
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.links ENABLE ROW LEVEL SECURITY;
CREATE POLICY links_tenant_isolation ON pgokf.links
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_provenance ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_provenance_tenant_isolation ON pgokf.concept_provenance
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_verification ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_verification_tenant_isolation ON pgokf.concept_verification
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_provenance_source ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_provenance_source_tenant_isolation ON pgokf.concept_provenance_source
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_source ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_source_tenant_isolation ON pgokf.concept_source
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.concept_embedding ENABLE ROW LEVEL SECURITY;
CREATE POLICY concept_embedding_tenant_isolation ON pgokf.concept_embedding
    USING (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (pg_catalog.current_setting('pgokf.tenant', true) IS NULL
        OR pg_catalog.current_setting('pgokf.tenant', true) = ''
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));
