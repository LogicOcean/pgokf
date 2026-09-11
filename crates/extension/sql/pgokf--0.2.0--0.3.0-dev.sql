-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.2.0 -> 0.3.0-dev
--
-- 0.3.0-dev adds the generic producer-facing capabilities:
--
--   * catalog generations (pgokf.bundles.catalog_generation, monotonic, bumped
--     once per accepted sync and per state mutation under the bundle advisory
--     lock) and publication fences (pgokf.publication_fence);
--   * freshness state and dependency evaluation (pgokf.bundle_freshness,
--     pgokf.concept_freshness, pgokf.freshness_dependency, the reader surface
--     pgokf.effective_freshness, the mark_* writer APIs, and the
--     pgokf.capabilities() declaration);
--   * the durable catalog-change outbox (pgokf.catalog_change_event) with
--     dispatcher claim/ack (the new pgokf_dispatcher role), admin inspection,
--     and acknowledged-only retention pruning
--     (change_event_retention_days, default 30);
--   * the additive freshness-aware search variant
--     pgokf.concept_search_fresh.
--
-- Every statement is additive: no row is dropped, truncated, deleted, or
-- rewritten. The one DROP is of the sync_log op CHECK constraint, immediately
-- re-created with two new operation names (a constraint carries no data - the
-- 0.2.0 script set the precedent). Existing bundles are backfilled into
-- pgokf.bundle_freshness as STALE (reason legacy_pre_0.3.0): freshness is
-- never claimed without reconciliation evidence, so an upgraded bundle stays
-- stale until a producer compare-and-set (pgokf.mark_fresh) clears it. A
-- catalog upgraded with this script is identical to a fresh 0.3.0-dev
-- install.
--
-- Sections in this file are bannered; each is the verbatim counterpart of the
-- named SQL block in src/catalog/*.rs (same statements, same order), plus the
-- upgrade-only pieces (ALTER TABLEs, the role, the backfill). Later phases
-- insert their sections BEFORE the final register_dump_relations() call.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

-- ===========================================================================
-- 1. The pgokf_dispatcher role (upgrade-only: roles are cluster-wide shared
--    objects and cannot be extension members, so - exactly like the bootstrap
--    on a fresh install - the idempotent block creates the role only when it
--    does not exist, and the grants/comment follow). The dispatcher is
--    deliberately OUTSIDE the reader < writer < admin ladder.
DO $pgokf$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'pgokf_dispatcher') THEN
        CREATE ROLE pgokf_dispatcher NOLOGIN;
    END IF;
END
$pgokf$;

GRANT USAGE ON SCHEMA pgokf TO pgokf_dispatcher;

COMMENT ON ROLE pgokf_dispatcher IS
    'pgokf outbox delivery role: may claim and acknowledge durable catalog-change events (pgokf.claim_catalog_change_events / pgokf.ack_catalog_change_event) and nothing else. Deliberately outside the reader < writer < admin ladder: it inherits nothing and no tier inherits it, so an event consumer holds no search or ingestion rights; pgokf_admin inspects the outbox through pgokf.list_catalog_change_events. Intended account for an automated event dispatcher.';

-- ===========================================================================
-- 2. Catalog generation on pgokf.bundles (upgrade-only ALTER; the fresh
--    install declares the column in the same position - last - in the
--    catalog_tables block of src/catalog/schema.rs).
ALTER TABLE pgokf.bundles ADD COLUMN catalog_generation bigint NOT NULL DEFAULT 0;
COMMENT ON COLUMN pgokf.bundles.catalog_generation IS
    'Monotonic catalog generation of this bundle: 0 before the first successful sync, then incremented exactly once per accepted register/refresh/content resync (inside the same transaction as the concept writes) and once per state mutation (enable/disable/retire/unretire), always under the bundle advisory lock. Durable change events (pgokf.catalog_change_event) and freshness evidence (pgokf.bundle_freshness) are generation-bound to it. Producer-side source revisions are opaque text and never stored here.';

-- ===========================================================================
-- 3. The change_event_retention_days policy key (upgrade-only ALTER; the
--    fresh install declares the column in the same position - last - in the
--    config_table block of src/catalog/config.rs).
ALTER TABLE pgokf_private.config ADD COLUMN change_event_retention_days integer NOT NULL DEFAULT 30;
ALTER TABLE pgokf_private.config ADD CONSTRAINT config_change_event_retention_nonneg_chk
    CHECK (change_event_retention_days >= 0);
COMMENT ON COLUMN pgokf_private.config.change_event_retention_days IS
    'Retention window in days for ACKNOWLEDGED pgokf.catalog_change_event rows: an acknowledged event whose acknowledged_at predates now() - this many days is pruned in the same transaction after a successful sync appends new events. Pending or claimed-but-unacknowledged events are NEVER pruned (delivery is at-least-once; an unacknowledged event stays retryable). 0 keeps acknowledged events indefinitely; must be >= 0. Default 30.';

-- ===========================================================================
-- 4. The audit trail links to the outbox (upgrade-only ALTER; the fresh
--    install declares the column in the same position - last - in the
--    sync_log_table block of src/catalog/audit.rs), and the op CHECK accepts
--    the two freshness-dependency audit operations (widened by drop/re-add,
--    which carries no data).
ALTER TABLE pgokf_private.sync_log ADD COLUMN change_event_id bigint;
COMMENT ON COLUMN pgokf_private.sync_log.change_event_id IS
    'The durable outbox event (pgokf.catalog_change_event.event_id) this operation committed, linking the audit row to the delivery/claim record; NULL for rows written before 0.3.0. The audit row and the event commit in the same transaction, so the link is always exact.';
ALTER TABLE pgokf_private.sync_log DROP CONSTRAINT IF EXISTS sync_log_op_chk;
ALTER TABLE pgokf_private.sync_log ADD CONSTRAINT sync_log_op_chk
    CHECK (op IN ('register', 'refresh', 'content', 'unregister',
                  'dependency_register', 'dependency_remove'));
COMMENT ON COLUMN pgokf_private.sync_log.op IS
    'The operation: register / refresh / content (register_bundle_content) / unregister; dependency_register / dependency_remove for freshness-dependency registration and removal (counts and hash NULL, bundle_path carrying the target bundle path plus the dependency id).';

-- ===========================================================================
-- 5. The freshness/fence tables (the freshness_tables block of
--    src/catalog/freshness.rs, verbatim).
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
    'One freshness row per bundle: generic state (fresh/stale/reconciling/blocked/retired) with machine-readable reason codes, the producer''s opaque observed/materialized source revisions, the catalog generation the materialization covers, and reconciliation timestamps. Created fresh at bundle registration; pre-0.3.0 bundles were backfilled stale (reason legacy_pre_0.3.0) and stay stale until a producer compare-and-set (pgokf.mark_fresh) re-establishes currency. Mutated only through the SECURITY DEFINER mark_* functions and dependency evaluation; granted to no API role.';
COMMENT ON COLUMN pgokf.bundle_freshness.bundle_id IS
    'The bundle this state belongs to (ON DELETE CASCADE: the row leaves with the bundle).';
COMMENT ON COLUMN pgokf.bundle_freshness.tenant_id IS
    'Multi-tenant owner, denormalized from the bundle for a local row-level-security predicate; always equals pgokf.bundles.tenant_id.';
COMMENT ON COLUMN pgokf.bundle_freshness.state IS
    'Effective bundle freshness: fresh, stale, reconciling (a reconciliation attempt owns the newest target; still effectively stale), blocked (a nonretryable failure; prior data stays labeled), or retired (the bundle is retired). Only registration and the compare-and-set pgokf.mark_fresh establish fresh.';
COMMENT ON COLUMN pgokf.bundle_freshness.reason_codes IS
    'Machine-readable, producer-supplied reason codes explaining the current non-fresh state (merged, deduplicated). Catalog-defined codes: legacy_pre_0.3.0, dependency_source_changed, change_scope_unknown, bundle_retired, bundle_restored, bundle_disabled; producers may add their own opaque codes.';
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
    'The embedding contract (model/dimension/render version) the producer reconciled against, as opaque jsonb evidence; semantic gating on it arrives with the embedding freshness capability.';
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
    'Registered freshness dependencies: a source selector (bundle / exact concept id / exact path / path prefix, matched case-sensitively - no glob or regex in v1) on a source bundle maps to a target bundle and target scope. Evaluated in the same transaction as every catalog change to the source bundle: a match marks the target stale (idempotent by generation via last_source_catalog_generation); an unprovable scope marks the source bundle itself stale instead of guessing. Registered and removed through pgokf.register_freshness_dependency / remove_freshness_dependency (writer-tier, audited); granted to no API role.';
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

-- ===========================================================================
-- 6. Legacy backfill (upgrade-only): every pre-0.3.0 bundle starts STALE -
--    freshness is never claimed without reconciliation evidence - until a
--    producer compare-and-set clears it.
INSERT INTO pgokf.bundle_freshness
    (bundle_id, tenant_id, state, reason_codes, materialized_catalog_generation)
SELECT b.id, b.tenant_id, 'stale', '{legacy_pre_0.3.0}'::text[], b.catalog_generation
FROM pgokf.bundles b
ON CONFLICT (bundle_id) DO NOTHING;

-- ===========================================================================
-- 7. The reader projection and the capability declaration (the
--    effective_freshness_view block of src/catalog/freshness.rs, verbatim).
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
            'search_freshness', 1)
    $fn$;
REVOKE ALL ON FUNCTION pgokf.capabilities() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.capabilities() TO pgokf_reader;
COMMENT ON FUNCTION pgokf.capabilities() IS
    'The catalog capabilities this pgokf release implements, as a jsonb object of capability name to interface version: catalog_generation, publication_fence, freshness_dependency, effective_freshness, catalog_change_event, and search_freshness (all version 1). Immutable; a producer declares the capabilities it requires and checks them here. Later releases only add entries or raise versions.';

-- ===========================================================================
-- 8. The durable outbox (the catalog_change_event_table block of
--    src/catalog/change_event.rs, verbatim).
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

CREATE INDEX catalog_change_event_claimable_idx
    ON pgokf.catalog_change_event (event_id) WHERE status <> 'acknowledged';
CREATE INDEX catalog_change_event_tenant_idx ON pgokf.catalog_change_event (tenant_id);

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

-- ===========================================================================
-- 9. The composite result types (the freshness_types block of
--    src/catalog/freshness.rs, the change_event_types block of
--    src/catalog/change_event.rs, and the search_fresh_type block of
--    src/catalog/search.rs, verbatim).
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

CREATE TYPE pgokf.concept_search_fresh_result AS (
    bundle_id          bigint,
    concept_id         text,
    path               text,
    title              text,
    type               text,
    rank               real,
    headline           text,
    freshness_state    text,
    freshness_reasons  text[],
    freshness_scope    text,
    stale_since        timestamptz,
    observed_revision  text,
    indexed_revision   text,
    published_revision text,
    catalog_generation bigint,
    last_reconciled_at timestamptz
);

COMMENT ON TYPE pgokf.concept_search_fresh_result IS
    'One ranked hit from pgokf.concept_search_fresh: the concept_search_result columns plus the concept''s effective freshness annotation - state, reason codes, the scope the state was recorded at, stale_since, the producer''s opaque observed/indexed revisions, the catalog generation the materialization covers (published_revision), the bundle''s live catalog_generation, and last_reconciled_at. Embedding provenance columns arrive with the embedding freshness capability.';

-- ===========================================================================
-- 10. The SQL-callable functions, declared exactly as the 0.3.0-dev install
--     script declares them (C-language wrappers exported by the 0.3.0-dev
--     shared library), then hardened as the freshness_function_hardening,
--     change_event_function_hardening, search_fresh_function_hardening, and
--     content_function_hardening blocks harden them.
CREATE FUNCTION pgokf."register_freshness_dependency"(
    "producer" TEXT,
    "source_bundle_id" bigint,
    "selector_kind" TEXT,
    "target_bundle_id" bigint,
    "selector_value" TEXT DEFAULT '',
    "target_scope_kind" TEXT DEFAULT 'bundle',
    "target_scope_key" TEXT DEFAULT NULL,
    "causation_key" TEXT DEFAULT NULL
) RETURNS bigint
LANGUAGE c
AS 'MODULE_PATHNAME', 'register_freshness_dependency_wrapper';

CREATE FUNCTION pgokf."disable_freshness_dependency"(
    "dependency_id" bigint
) RETURNS void
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'disable_freshness_dependency_wrapper';

CREATE FUNCTION pgokf."remove_freshness_dependency"(
    "dependency_id" bigint
) RETURNS void
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'remove_freshness_dependency_wrapper';

CREATE FUNCTION pgokf."mark_stale"(
    "bundle_id" bigint,
    "reason_codes" TEXT[] DEFAULT '{}',
    "producer" TEXT DEFAULT NULL,
    "observed_source_generation" TEXT DEFAULT NULL
) RETURNS void
LANGUAGE c
AS 'MODULE_PATHNAME', 'mark_stale_wrapper';

CREATE FUNCTION pgokf."mark_reconciling"(
    "bundle_id" bigint,
    "producer" TEXT DEFAULT NULL
) RETURNS void
LANGUAGE c
AS 'MODULE_PATHNAME', 'mark_reconciling_wrapper';

CREATE FUNCTION pgokf."mark_blocked"(
    "bundle_id" bigint,
    "reason_codes" TEXT[] DEFAULT '{}',
    "producer" TEXT DEFAULT NULL
) RETURNS void
LANGUAGE c
AS 'MODULE_PATHNAME', 'mark_blocked_wrapper';

CREATE FUNCTION pgokf."mark_fresh"(
    "bundle_id" bigint,
    "expected_catalog_generation" bigint,
    "expected_observed_source_generation" TEXT DEFAULT NULL,
    "manifest_hash" TEXT DEFAULT NULL,
    "embedding_contract" jsonb DEFAULT NULL,
    "producer" TEXT DEFAULT NULL
) RETURNS bool
LANGUAGE c
AS 'MODULE_PATHNAME', 'mark_fresh_wrapper';

CREATE FUNCTION pgokf."mark_scope_stale"(
    "bundle_id" bigint,
    "scope_kind" TEXT,
    "scope_key" TEXT,
    "reason_codes" TEXT[] DEFAULT '{}',
    "producer" TEXT DEFAULT NULL
) RETURNS void
LANGUAGE c
AS 'MODULE_PATHNAME', 'mark_scope_stale_wrapper';

CREATE FUNCTION pgokf."clear_freshness_scope"(
    "bundle_id" bigint,
    "scope_kind" TEXT,
    "scope_key" TEXT
) RETURNS void
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'clear_freshness_scope_wrapper';

CREATE FUNCTION pgokf."list_freshness_dependencies"(
    "max_rows" INT DEFAULT 100
) RETURNS SETOF pgokf.freshness_dependency_info
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'list_freshness_dependencies_wrapper';

CREATE FUNCTION pgokf."repair_bundle_freshness"(
    "bundle_id" bigint,
    "state" TEXT,
    "reason_codes" TEXT[] DEFAULT '{}'
) RETURNS void
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'repair_bundle_freshness_wrapper';

CREATE FUNCTION pgokf."issue_publication_fence"(
    "bundle_id" bigint,
    "producer" TEXT,
    "target_generation" bigint,
    "expected_catalog_generation" bigint,
    "manifest_hash" TEXT DEFAULT NULL,
    "lease_seconds" INT DEFAULT 300
) RETURNS pgokf.publication_fence_info
LANGUAGE c
AS 'MODULE_PATHNAME', 'issue_publication_fence_wrapper';

CREATE FUNCTION pgokf."release_publication_fence"(
    "bundle_id" bigint,
    "producer" TEXT,
    "fencing_token" bigint
) RETURNS void
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'release_publication_fence_wrapper';

CREATE FUNCTION pgokf."claim_catalog_change_events"(
    "producer" TEXT,
    "limit" INT DEFAULT 100,
    "lease_seconds" INT DEFAULT 300
) RETURNS SETOF pgokf.claimed_change_event
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'claim_catalog_change_events_wrapper';

CREATE FUNCTION pgokf."ack_catalog_change_event"(
    "event_id" bigint,
    "producer" TEXT,
    "acceptance_key" TEXT
) RETURNS bool
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'ack_catalog_change_event_wrapper';

CREATE FUNCTION pgokf."list_catalog_change_events"(
    "bundle_id" bigint DEFAULT NULL,
    "max_rows" INT DEFAULT 100
) RETURNS SETOF pgokf.catalog_change_event_info
LANGUAGE c
AS 'MODULE_PATHNAME', 'list_catalog_change_events_wrapper';

CREATE FUNCTION pgokf."concept_search_fresh"(
    "query" TEXT,
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" INT DEFAULT 20,
    "freshness" TEXT DEFAULT 'any',
    "concept_type" TEXT DEFAULT NULL,
    "tags" TEXT[] DEFAULT NULL,
    "status" TEXT DEFAULT NULL,
    "trust_tier" TEXT DEFAULT NULL,
    "after_cursor" jsonb DEFAULT NULL
) RETURNS SETOF pgokf.concept_search_fresh_result
STABLE PARALLEL RESTRICTED
LANGUAGE c
AS 'MODULE_PATHNAME', 'concept_search_fresh_wrapper';

CREATE FUNCTION pgokf."register_bundle_content_with_context"(
    "name" TEXT,
    "paths" TEXT[],
    "contents" bytea[],
    "options" jsonb DEFAULT '{}',
    "context" jsonb DEFAULT NULL
) RETURNS pgokf.bundle_sync_result
LANGUAGE c
AS 'MODULE_PATHNAME', 'register_bundle_content_with_context_wrapper';

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
    'Register a freshness dependency (source selector -> target bundle/scope) and return its identity. Writer-tier (pgokf_writer; admin inherits). Selector grammar, exact and case-sensitive (no glob/regex in v1): selector_kind bundle (empty selector_value), concept (exact concept id), path (exact bundle-relative path), or path_prefix; target_scope_kind bundle (NULL key), concept, path, or group. Both bundles must belong to the session''s tenant (22023 otherwise); producer is an opaque label, not authorization. The dependency starts from the source bundle''s current catalog generation and is evaluated in the same transaction as every later catalog change to the source; registration is audited. Raises 23505 for an identical existing registration.';
COMMENT ON FUNCTION pgokf.disable_freshness_dependency(bigint) IS
    'Disable a registered freshness dependency (kept but no longer evaluated). Writer-tier; raises 22023 for an unknown or cross-tenant dependency id.';
COMMENT ON FUNCTION pgokf.remove_freshness_dependency(bigint) IS
    'Remove a freshness dependency entirely (audited). Writer-tier; raises 22023 for an unknown or cross-tenant dependency id.';
COMMENT ON FUNCTION pgokf.mark_stale(bigint, text[], text, text) IS
    'Mark a bundle stale with machine-readable reason codes (default ''{producer_reported}''), optionally advancing the observed source revision (opaque text). Writer-tier; tenant-confined (22023 for an unknown or cross-tenant bundle). External-source observations must call this before the producer acknowledges the observation or queues work.';
COMMENT ON FUNCTION pgokf.mark_reconciling(bigint, text) IS
    'Mark a bundle reconciling: a reconciliation attempt owns the newest target. The bundle remains effectively stale (stale_since is preserved); only the compare-and-set pgokf.mark_fresh clears it. Writer-tier; tenant-confined.';
COMMENT ON FUNCTION pgokf.mark_blocked(bigint, text[], text) IS
    'Mark a bundle blocked (a nonretryable failure) with reason codes; the prior data stays available, labeled stale/blocked. Writer-tier; tenant-confined.';
COMMENT ON FUNCTION pgokf.mark_fresh(bigint, bigint, text, text, jsonb, text) IS
    'Compare-and-set reconciliation completion: mark the bundle fresh only if its observed source revision still equals expected_observed_source_generation AND its live catalog generation equals expected_catalog_generation AND no newer materialized generation exists AND it is not retired; returns false (changing nothing) otherwise, so a superseded attempt can never clear staleness. On success records the manifest hash and embedding contract evidence and sets last_reconciled_at. Writer-tier; tenant-confined.';
COMMENT ON FUNCTION pgokf.mark_scope_stale(bigint, text, text, text[], text) IS
    'Mark one scope within a bundle (scope_kind concept/path/group with an exact, case-sensitive scope_key) stale with reason codes. Writer-tier; tenant-confined. The override shadows the bundle state for that scope in pgokf.effective_freshness until cleared.';
COMMENT ON FUNCTION pgokf.clear_freshness_scope(bigint, text, text) IS
    'Remove a concept/path/group freshness override, returning the scope to the bundle''s state. Writer-tier; tenant-confined; raises 22023 when no such override exists.';
COMMENT ON FUNCTION pgokf.list_freshness_dependencies(integer) IS
    'List every registered freshness dependency as pgokf.freshness_dependency_info, ordered by identity, bounded by max_rows (default 100). Admin-only (pgokf_admin); tenant-scoped; the raw table is granted to no role.';
COMMENT ON FUNCTION pgokf.repair_bundle_freshness(bigint, text, text[]) IS
    'Admin repair: set a bundle''s freshness state (fresh/stale/reconciling/blocked/retired) and reason codes directly, replacing both. Admin-only (pgokf_admin); tenant-confined. Does not establish producer currency evidence (generation/revision columns are untouched), so a repaired fresh row carries only the evidence it already had.';
COMMENT ON FUNCTION pgokf.issue_publication_fence(bigint, text, bigint, bigint, text, integer) IS
    'Issue (or supersede) the publication fence for (tenant, producer, bundle), returning pgokf.publication_fence_info with the catalog-assigned fencing_token. Writer-tier; compare-and-set under the bundle advisory lock: raises 22023 unless expected_catalog_generation equals the bundle''s current catalog generation and target_generation advances past the slot''s current target. Lease defaults to 300 seconds.';
COMMENT ON FUNCTION pgokf.release_publication_fence(bigint, text, bigint) IS
    'Release a live publication fence; only the live fencing_token releases the slot, so a superseded or expired attempt fails with 22023 instead of silently completing. Writer-tier; tenant-confined.';

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

REVOKE ALL ON FUNCTION pgokf.concept_search_fresh(text, bigint, integer, text, text, text[], text, text, jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_fresh(text, bigint, integer, text, text, text[], text, text, jsonb) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search_fresh(text, bigint, integer, text, text, text[], text, text, jsonb) IS
    'Rank catalog concepts with effective freshness: the concept_search contract plus a freshness filter (any - the default - fresh, or stale; 22023 otherwise) and a per-hit freshness annotation (state, reasons, scope, stale_since, opaque observed/indexed revisions, published_revision, catalog_generation, last_reconciled_at) with concept > path > bundle override precedence; a concept with no recorded row is fresh. Lexical results may include stale concepts, always labeled; the filter applies before pagination. Reader-level and tenant-scoped like concept_search. This variant always ranks with the native FTS pipeline; composition with the optional BM25 backend is deferred. Semantic/embedding freshness gating is a later capability and is not applied here.';

ALTER FUNCTION pgokf.register_bundle_content_with_context(text, text[], bytea[], jsonb, jsonb)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.register_bundle_content_with_context(text, text[], bytea[], jsonb, jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.register_bundle_content_with_context(text, text[], bytea[], jsonb, jsonb) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.register_bundle_content_with_context(text, text[], bytea[], jsonb, jsonb) IS
    'register_bundle_content with an explicit change-provenance context (jsonb: origin, causation_key, reconciliation_key, producer, manifest_hash, observed_source_generation, and operation - refresh_bundle/put_document/delete_document/concept_change) recorded on the durable catalog-change event, so a companion''s one-document put/delete keeps its precise operation and origin; context NULL (the default) applies the session GUC instead. Writer-tier (pgokf_writer; admin inherits it). Without this function, the session GUC pgokf.sync_context supplies the same context to register_bundle, refresh_bundle, and register_bundle_content.';

-- Last, so the new relations are registered for pg_dump (the rule for every
-- upgrade script since 0.1.14). Later phases insert their sections BEFORE
-- this line.
SELECT pgokf_private.register_dump_relations();
