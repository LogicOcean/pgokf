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
--     pgokf.concept_search_fresh;
--   * embedding freshness (section 11): provenance columns on
--     pgokf.concept_embedding, the compare-and-set setter
--     pgokf.set_concept_embedding_cas, the embedding_model/embedding_contract
--     policy keys, eligibility gating of semantic/hybrid ranking, and the
--     embedding provenance annotation on concept_search_fresh results.
--   * generation-bound typed relationships (section 12):
--     pgokf.relationship_publication / pgokf.relationship, the
--     pgokf.current_relationships reader projection, the compare-and-set
--     pgokf.replace_relationships writer API, the
--     pgokf.concept_relationship_neighbors typed traversal, and the
--     relationship_coverage_missing participation in pgokf.mark_fresh.
--   * the scheduled-refresh read surface pgokf.list_scheduled_refreshes
--     (section 14): the tenant-confined, SECURITY DEFINER listing of the
--     pg_cron jobs pgokf.schedule_refresh registers under the extension
--     owner's identity, invisible to an ordinary login reading cron.job
--     directly.
--   * the external repository-registry surface (section 15): the guarded
--     column-level SELECT grant that lets pgokf_reader list the producer
--     service's ast_graph.repository_registry where that schema shares the
--     database, and the admin-tier SECURITY DEFINER writers
--     pgokf.registry_set_status / pgokf.registry_set_poll_interval (pause /
--     resume and the poll interval), both runtime-only couplings that raise
--     a curated 22023 where the producer schema is absent.
--
-- Every statement is additive in the sense that matters: no row is dropped,
-- truncated, deleted, or rewritten. The DROPs are of objects that carry no
-- data: the sync_log op CHECK constraint, immediately re-created with two new
-- operation names (a constraint carries no data - the 0.2.0 script set the
-- precedent), and - in section 13 - three FUNCTIONS whose signatures gain one
-- optional trailing argument with a default (concept_search_semantic,
-- concept_search_hybrid, and the internal bm25_hits helper). A function
-- identity is its argument list, so the superseded overloads must be dropped
-- before the widened ones are created (the concept_search after_cursor
-- replacement of 0.1.8 -> 0.1.9 set the precedent); each is re-created in the
-- same transaction as a STRICT SUPERSET that resolves every historical call
-- through the new argument's default. Existing bundles are backfilled into
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
    'Monotonic counter bumped every time dependency evaluation (direct, unprovable-scope, transitive, or source-removal) marks this row non-fresh. The compare-and-set pgokf.mark_fresh refuses while it exceeds the claim token the completing attempt presents (returned by its own pgokf.mark_reconciling), so a completion prepared before the newest dependency invalidation can never erase it.';
COMMENT ON COLUMN pgokf.bundle_freshness.claimed_invalidation_epoch IS
    'The dependency_invalidation_epoch the producer''s latest reconciliation attempt has claimed via pgokf.mark_reconciling (an admin repair settles it to the standing epoch). pgokf.mark_fresh completes only when the attempt presents a claim token covering both this standing claim and the newest dependency_invalidation_epoch, so one attempt''s claim can never validate another attempt''s completion.';
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
    'Registered freshness dependencies: a source selector (bundle / exact concept id / exact path / path prefix, matched case-sensitively - no glob or regex in v1) on a source bundle maps to a target bundle and target scope. Evaluated in the same transaction as every catalog change to the source bundle: a match marks the target stale (idempotent by generation via last_source_catalog_generation); an unprovable scope marks the source bundle itself stale and invalidates the registered dependent at its registered scope instead of guessing; bundle-level invalidation propagates transitively through bundle-scope registrations (cycle-safe, capped - past the cap a conservative blanket invalidation marks every bundle with an enabled bundle-scope edge stale, so no dependent stays falsely fresh) and bumps the target row''s dependency_invalidation_epoch; removing the source invalidates its registered dependents before their rows cascade away. Registered and removed through pgokf.register_freshness_dependency / remove_freshness_dependency (writer-tier, audited); granted to no API role.';
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
) RETURNS bigint
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
    "claimed_invalidation_epoch" bigint,
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
ALTER FUNCTION pgokf.mark_fresh(bigint, bigint, bigint, text, text, jsonb, text)
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
REVOKE ALL ON FUNCTION pgokf.mark_fresh(bigint, bigint, bigint, text, text, jsonb, text) FROM PUBLIC;
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
GRANT EXECUTE ON FUNCTION pgokf.mark_fresh(bigint, bigint, bigint, text, text, jsonb, text) TO pgokf_writer;
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
    'Mark a bundle reconciling: a reconciliation attempt owns the newest target and claims the standing dependency invalidation epoch, returned to the caller as the attempt''s claim token (pgokf.mark_fresh completes only for an attempt presenting a token that covers the newest epoch and the standing claim, so a dependency invalidation landing after the claim refuses the completion until the producer re-claims, and a newer attempt''s claim on the shared row can never validate an older attempt''s evidence). The bundle remains effectively stale (stale_since is preserved); only the compare-and-set pgokf.mark_fresh clears it. The claim never clears the relationship_coverage_missing evidence. Writer-tier; tenant-confined.';
COMMENT ON FUNCTION pgokf.mark_blocked(bigint, text[], text) IS
    'Mark a bundle blocked (a nonretryable failure) with reason codes; the prior data stays available, labeled stale/blocked. Writer-tier; tenant-confined.';
COMMENT ON FUNCTION pgokf.mark_fresh(bigint, bigint, bigint, text, text, jsonb, text) IS
    'Compare-and-set reconciliation completion: mark the bundle fresh only if its observed source revision still equals expected_observed_source_generation AND its live catalog generation equals expected_catalog_generation AND no newer materialized generation exists AND claimed_invalidation_epoch - the claim token this attempt''s own pgokf.mark_reconciling returned - covers the newest dependency invalidation epoch and the standing claim (a completion based on evidence older than the latest dependency invalidation is refused, and a newer attempt''s claim can never validate an older attempt''s token) AND no relationship_coverage_missing evidence stands (a refresh that superseded the bundle''s relationship coverage must be answered with a matching pgokf.replace_relationships publication first) AND it is not retired; returns false (changing nothing) otherwise, so a superseded attempt can never clear staleness. The check runs under the bundle advisory lock, so it never certifies a generation or epoch older than a committed mutation it waited behind. On success records the manifest hash and embedding contract evidence and sets last_reconciled_at. Writer-tier; tenant-confined.';
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
    'Claim up to limit (default 100) pending or expired-claim catalog-change events for producer, oldest-first, with FOR UPDATE SKIP LOCKED; sets claim owner and a lease of lease_seconds (default 300) and increments each event''s attempts. Dispatcher-tier (pgokf_dispatcher only, the sole claim/acknowledge role; admins inspect the outbox through pgokf.list_catalog_change_events); tenant-scoped. Delivery is at-least-once: acknowledge with pgokf.ack_catalog_change_event after the producer durably accepts the event.';
COMMENT ON FUNCTION pgokf.ack_catalog_change_event(bigint, text, text) IS
    'Acknowledge a claimed catalog-change event after durable producer acceptance. Dispatcher-tier (pgokf_dispatcher only, the sole claim/acknowledge role; admins inspect the outbox through pgokf.list_catalog_change_events); only the producer label holding the claim may ack (42501 otherwise), a retry with the same acceptance_key is an idempotent no-op returning true, and a conflicting key or an unknown/unclaimed event raises 22023. Unacknowledged events stay retryable and are never pruned.';
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

-- ===========================================================================
-- 11. Embedding freshness (the stale-embedding fix).
--
--     11a. The provenance columns on pgokf.concept_embedding (upgrade-only
--          ALTERs; the fresh install declares the columns in the same
--          position - last - in the embedding_table block of
--          src/catalog/embedding.rs). All three are nullable: an existing
--          (pre-0.3.0) row carries no source/input hash evidence and is
--          therefore LEGACY - never eligible for semantic ranking - and is
--          re-embedded by the watcher's missing-or-stale poll. Hashes are not
--          backfilled: they cannot be proved for rows written before this
--          capability existed.
ALTER TABLE pgokf.concept_embedding ADD COLUMN source_file_hash text;
ALTER TABLE pgokf.concept_embedding ADD COLUMN input_hash text;
ALTER TABLE pgokf.concept_embedding ADD COLUMN contract text;

-- The refreshed table/column comments (COMMENT ON replaces; the texts are
-- verbatim those of the fresh install's embedding_table block).
COMMENT ON TABLE pgokf.concept_embedding IS
    'Opt-in per-concept embedding vectors, streamed in by a companion embedder via pgokf.set_concept_embedding / pgokf.set_concept_embedding_cas (the extension never computes embeddings or performs network I/O). The vector is stored as the builtin real[] - NOT a pgvector ''vector'' column - so CREATE EXTENSION pgokf succeeds without pgvector installed; it is cast to vector(dim) at query time and in the HNSW index only when pgvector is present. Rows cascade from pgokf.concepts, so removing a concept or unregistering a bundle drops its embedding automatically, and a sync that re-stages a concept deletes its row in the same transaction. Semantic ranking ranks only ELIGIBLE rows: source_file_hash equal to the concept''s current file_hash, model/dim/contract matching the current embedding policy, and the concept effectively fresh; a row with NULL provenance (legacy or written by the compatibility setter) never ranks.';
COMMENT ON COLUMN pgokf.concept_embedding.dim IS
    'Length of embedding, constrained equal to cardinality(embedding); the effective dimension of the stored vector. Semantic eligibility additionally requires dim to equal the current embedding_dim policy.';
COMMENT ON COLUMN pgokf.concept_embedding.model IS
    'Identifier of the embedding model that computed the vector (pgokf.set_concept_embedding_cas requires it). NULL marks a legacy row - pre-0.3.0 or written through the compatibility setter pgokf.set_concept_embedding - which is never eligible for semantic ranking.';
COMMENT ON COLUMN pgokf.concept_embedding.updated_at IS
    'When this embedding row was last written by pgokf.set_concept_embedding / set_concept_embedding_cas; the embedded_at provenance of search result metadata.';
COMMENT ON COLUMN pgokf.concept_embedding.source_file_hash IS
    'The concept''s file_hash at embed time (pgokf.set_concept_embedding_cas compare-and-sets against it). Semantic eligibility requires it to equal the concept''s current file_hash; NULL marks a legacy row that never ranks.';
COMMENT ON COLUMN pgokf.concept_embedding.input_hash IS
    'Hash of the exact bounded input text the embedder sent (title + description + body_text under the render contract), supplied by the embedder as provenance; the catalog stores it opaquely and never re-computes it. NULL marks a legacy row.';
COMMENT ON COLUMN pgokf.concept_embedding.contract IS
    'The embedder''s render-contract identity (input construction and truncation version), e.g. pgokf-embed/v1/max-chars:8000. When the embedding_contract policy key pins a value, semantic eligibility requires an exact match; NULL marks a legacy row that never ranks.';

--     11b. The embedding contract policy keys (upgrade-only ALTERs; the fresh
--          install declares the columns in the same position - last - in the
--          config_table block of src/catalog/config.rs). NOT NULL with a
--          constant default is metadata-only (no table rewrite).
ALTER TABLE pgokf_private.config ADD COLUMN embedding_model text NOT NULL DEFAULT '';
ALTER TABLE pgokf_private.config ADD COLUMN embedding_contract text NOT NULL DEFAULT '';

COMMENT ON COLUMN pgokf_private.config.embedding_dim IS
    'Expected dimension (1..=16000) of the caller-computed concept embeddings streamed in via pgokf.set_concept_embedding / set_concept_embedding_cas: the setters reject any real[] whose length differs, and pgokf.rebuild_embedding_index builds its pgvector HNSW index with this typmod (vector(embedding_dim)). Default 1536. The extension never computes embeddings. Semantic ranking additionally requires a stored row''s dim to equal this key, so a change revokes the eligibility of every stored vector and marks every bundle holding embedding rows stale (reason embedding_contract_changed) in the same transaction; follow a change with re-ingestion and pgokf.rebuild_embedding_index. HNSW indexing applies only up to pgvector''s 2000-dimension index limit; above it semantic search still works via an exact scan.';
COMMENT ON COLUMN pgokf_private.config.embedding_model IS
    'Optional pin on the embedding model a stored concept vector must carry to be eligible for semantic ranking: empty (the default) accepts any non-NULL model; a non-empty value requires an exact match. A change marks every bundle holding embedding rows stale (reason embedding_contract_changed) in the same transaction, before new vectors are queued; the embedding watcher re-embeds the now-stale rows against the new policy. A row with NULL model is legacy and never ranks regardless.';
COMMENT ON COLUMN pgokf_private.config.embedding_contract IS
    'Optional pin on the render-contract identity (input construction and truncation version, e.g. pgokf-embed/v1/max-chars:8000) a stored concept vector must carry to be eligible for semantic ranking: empty (the default) accepts any non-NULL contract; a non-empty value requires an exact match. A change marks every bundle holding embedding rows stale (reason embedding_contract_changed) in the same transaction. A row with NULL contract is legacy and never ranks regardless.';

--     11c. The compare-and-set setter, declared exactly as the 0.3.0-dev
--          install script declares it, then hardened as the
--          embedding_function_hardening block of src/catalog/embedding.rs
--          hardens it; the refreshed comments of the pre-existing embedding
--          functions follow (COMMENT ON replaces).
CREATE FUNCTION pgokf."set_concept_embedding_cas"(
    "bundle_id" bigint,
    "concept_id" TEXT,
    "embedding" real[],
    "expected_file_hash" TEXT,
    "input_hash" TEXT,
    "model" TEXT,
    "contract" TEXT
) RETURNS bool
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'set_concept_embedding_cas_wrapper';

ALTER FUNCTION pgokf.set_concept_embedding_cas(bigint, text, real[], text, text, text, text)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;
REVOKE ALL ON FUNCTION pgokf.set_concept_embedding_cas(bigint, text, real[], text, text, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.set_concept_embedding_cas(bigint, text, real[], text, text, text, text) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.set_concept_embedding_cas(bigint, text, real[], text, text, text, text) IS
    'Store or replace one concept''s embedding with full provenance, compare-and-set against the concept''s current file_hash: the write commits only when the concept''s file_hash still equals expected_file_hash under a row lock, returning true; a mismatch returns false having written nothing (retryable - re-read and re-embed, never an error-loop). input_hash is the caller-computed hash of the exact bounded input text, model the embedding model, contract the render-contract identity; all provenance arguments must be non-empty (22023 otherwise, as for a wrong dimension or an unknown concept; 42501 outside pgokf_writer). Only rows written through this setter carry the provenance semantic ranking requires.';
COMMENT ON FUNCTION pgokf.set_concept_embedding(bigint, text, real[]) IS
    'Store or replace one concept''s embedding (real[]) streamed in by a companion embedder; the extension never computes embeddings. Writer-tier (pgokf_writer; admin inherits it), SECURITY DEFINER. Validates the concept exists and len(embedding)=embedding_dim (else 22023) and upserts. The vector is stored as real[] so pgokf needs no static pgvector dependency. This 0.2.0 compatibility signature carries no provenance, so the row it writes is a legacy row (model/source_file_hash/input_hash/contract all NULL, cleared on overwrite) that never ranks semantically; use pgokf.set_concept_embedding_cas for an eligible, provenance-carrying write.';
COMMENT ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer) IS
    'Semantic nearest-neighbor search: rank concepts by pgvector cosine distance to query_embedding (rank = normalized cosine similarity). Reader-level, invoker rights; active bundles only. query_embedding must have embedding_dim dimensions; limit_count in 1..=500. Requires pgvector: raises 22023 naming the missing dependency when it is not installed (no lexical fallback). Only ELIGIBLE embeddings rank: source_file_hash equal to the concept''s current file_hash, model/dimension/contract matching the embedding_model/embedding_dim/embedding_contract policy, and the concept effectively fresh (bundle freshness fresh, no covering concept/path override); a stale or legacy (NULL-provenance) row never ranks even while the HNSW index physically retains it.';
COMMENT ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer) IS
    'Hybrid search: Reciprocal Rank Fusion (RRF, k=60) of the lexical result of query (via the configured search_backend) and the semantic result of query_embedding, fused entirely in SQL (rank = fused RRF score). Reader-level, invoker rights; enabled bundles only; limit_count in 1..=500. The semantic component ranks eligible (current, fresh) embeddings only, so an ineligible vector never leaks into the fused result; the lexical component may still return a stale concept, labeled by pgokf.concept_search_fresh. Degrades to lexical-only with a WARNING when pgvector is not installed.';
COMMENT ON FUNCTION pgokf.rebuild_embedding_index() IS
    'Admin-only. (Re)build the pgvector HNSW (cosine) index on pgokf.concept_embedding for the configured embedding_dim; returns true when built, or false (with a NOTICE) when pgvector is absent or embedding_dim exceeds pgvector''s 2000-dimension HNSW limit. The index physically retains ineligible rows; the ranking predicates, not the index, enforce eligibility.';

--     11d. The embedding provenance attributes of concept_search_fresh_result
--          (the fresh install declares them in the same position - last - in
--          the search_fresh_type block of src/catalog/search.rs; ALTER TYPE
--          ... ADD ATTRIBUTE appends, and carries no data). The function's
--          declaration is unchanged - the same C wrapper, rebuilt in the
--          0.3.0-dev shared library, fills the new attributes - so only the
--          refreshed comments follow.
ALTER TYPE pgokf.concept_search_fresh_result ADD ATTRIBUTE embedding_state text;
ALTER TYPE pgokf.concept_search_fresh_result ADD ATTRIBUTE embedding_model text;
ALTER TYPE pgokf.concept_search_fresh_result ADD ATTRIBUTE embedding_dim integer;
ALTER TYPE pgokf.concept_search_fresh_result ADD ATTRIBUTE embedding_input_hash text;
ALTER TYPE pgokf.concept_search_fresh_result ADD ATTRIBUTE embedded_at timestamptz;

COMMENT ON TYPE pgokf.concept_search_fresh_result IS
    'One ranked hit from pgokf.concept_search_fresh: the concept_search_result columns plus the concept''s effective freshness annotation - state, reason codes, the scope the state was recorded at, stale_since, the producer''s opaque observed/indexed revisions, the catalog generation the materialization covers (published_revision), the bundle''s live catalog_generation, and last_reconciled_at - plus the embedding provenance: embedding_state (missing / current / stale, where current means the stored vector satisfies the semantic eligibility predicate: source file hash equal to the concept''s current file_hash, model/dimension/contract matching the embedding policy, and the concept effectively fresh), embedding_model, embedding_dim, embedding_input_hash, and embedded_at (NULL when no embedding row exists).';
COMMENT ON FUNCTION pgokf.concept_search_fresh(text, bigint, integer, text, text, text[], text, text, jsonb) IS
    'Rank catalog concepts with effective freshness: the concept_search contract plus a freshness filter (any - the default - fresh, or stale; 22023 otherwise) and a per-hit freshness annotation (state, reasons, scope, stale_since, opaque observed/indexed revisions, published_revision, catalog_generation, last_reconciled_at) with concept > path > bundle override precedence; a concept with no recorded row is fresh. Every hit also carries its embedding provenance (embedding_state missing/current/stale under the semantic eligibility predicate, plus model, dimension, input hash, and embedded_at). Lexical results may include stale concepts, always labeled; the filter applies before pagination. Reader-level and tenant-scoped like concept_search. This variant always ranks with the native FTS pipeline; composition with the optional BM25 backend is deferred. Semantic ranking itself (concept_search_semantic / concept_search_hybrid) excludes ineligible embeddings rather than labeling them.';

--     11e. The refreshed capability declaration (the effective_freshness_view
--          block of src/catalog/freshness.rs), adding embedding_freshness.
CREATE OR REPLACE FUNCTION pgokf.capabilities() RETURNS jsonb
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
            'embedding_freshness', 1)
    $fn$;
COMMENT ON FUNCTION pgokf.capabilities() IS
    'The catalog capabilities this pgokf release implements, as a jsonb object of capability name to interface version: catalog_generation, publication_fence, freshness_dependency, effective_freshness, catalog_change_event, search_freshness, and embedding_freshness (all version 1). Immutable; a producer declares the capabilities it requires and checks them here. Later releases only add entries or raise versions.';

--     11f. The refreshed bundle_freshness.embedding_contract evidence comment
--          (COMMENT ON replaces).
COMMENT ON COLUMN pgokf.bundle_freshness.embedding_contract IS
    'The embedding contract (model/dimension/render version) the producer reconciled against, as opaque jsonb evidence recorded by pgokf.mark_fresh. Semantic ranking does not read this evidence: it enforces the live embedding_model / embedding_dim / embedding_contract policy against each embedding row''s own provenance.';

-- ===========================================================================
-- 12. Generation-bound typed relationships (capability C).
--
--     12a. The publication/relationship tables (the relationship_tables block
--          of src/catalog/relationships.rs, verbatim).
CREATE TABLE pgokf.relationship_publication (
    publication_id    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id         text NOT NULL DEFAULT 'default',
    producer          text NOT NULL,
    source_bundle_id  bigint,
    source_bundle_path text NOT NULL,
    publication_generation bigint NOT NULL,
    expected_catalog_generation bigint NOT NULL,
    activated_catalog_generation bigint,
    fencing_token     bigint NOT NULL,
    relationship_set_hash text NOT NULL,
    idempotency_key   text NOT NULL,
    manifest_hash     text,
    state             text NOT NULL DEFAULT 'staged',
    row_count         integer NOT NULL DEFAULT 0,
    created_by        text NOT NULL DEFAULT session_user,
    created_at        timestamptz NOT NULL DEFAULT now(),
    activated_at      timestamptz,
    updated_at        timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT relationship_publication_bundle_fk
        FOREIGN KEY (source_bundle_id) REFERENCES pgokf.bundles (id) ON DELETE SET NULL,
    CONSTRAINT relationship_publication_state_chk
        CHECK (state IN ('staged', 'active', 'superseded')),
    CONSTRAINT relationship_publication_generation_chk
        CHECK (publication_generation > 0 AND fencing_token > 0
               AND expected_catalog_generation >= 0),
    CONSTRAINT relationship_publication_activation_chk
        CHECK (state <> 'active' OR activated_catalog_generation IS NOT NULL)
);

-- Duplicate prevention for live bundles: one publication per
-- (tenant_id, producer, source_bundle_id, publication_generation). The
-- partial index excludes detached audit rows (source_bundle_id IS NULL after
-- the ON DELETE SET NULL detach), so hard-deleting two bundles that each held
-- the same producer/generation key never collides in the retained ledger.
CREATE UNIQUE INDEX relationship_publication_uq
    ON pgokf.relationship_publication
    (tenant_id, producer, source_bundle_id, publication_generation)
    WHERE source_bundle_id IS NOT NULL;

-- The sync-time activation scan: staged/active publications of one bundle.
CREATE INDEX relationship_publication_bundle_state_idx
    ON pgokf.relationship_publication (source_bundle_id, state);

CREATE TABLE pgokf.relationship (
    publication_id    bigint NOT NULL,
    ordinal           integer NOT NULL,
    tenant_id         text NOT NULL DEFAULT 'default',
    source_bundle_id  bigint NOT NULL,
    source_concept_id text NOT NULL,
    relation_type     text NOT NULL,
    direction         text NOT NULL DEFAULT 'directed',
    target_bundle_id  bigint,
    target_concept_id text,
    external_target   text,
    source_location   jsonb,
    confidence        double precision,
    unresolved        boolean NOT NULL DEFAULT false,
    cross_bundle      boolean NOT NULL DEFAULT false,
    provenance        jsonb,
    row_hash          text NOT NULL,
    CONSTRAINT relationship_pkey PRIMARY KEY (publication_id, ordinal),
    CONSTRAINT relationship_publication_fk
        FOREIGN KEY (publication_id)
        REFERENCES pgokf.relationship_publication (publication_id) ON DELETE CASCADE,
    CONSTRAINT relationship_direction_chk
        CHECK (direction IN ('directed', 'undirected')),
    CONSTRAINT relationship_relation_type_chk
        CHECK (length(relation_type) <= 128
               AND position(':' IN relation_type) > 1
               AND position(':' IN relation_type) < length(relation_type)),
    CONSTRAINT relationship_target_chk
        CHECK ((target_bundle_id IS NOT NULL) = (target_concept_id IS NOT NULL)
               AND NOT (target_concept_id IS NOT NULL AND external_target IS NOT NULL)
               AND (unresolved OR target_concept_id IS NOT NULL)),
    CONSTRAINT relationship_confidence_chk
        CHECK (confidence IS NULL OR (confidence >= 0 AND confidence <= 1))
);

CREATE INDEX relationship_source_type_idx
    ON pgokf.relationship (source_bundle_id, source_concept_id, relation_type);
CREATE INDEX relationship_target_type_idx
    ON pgokf.relationship (target_bundle_id, target_concept_id, relation_type)
    WHERE target_concept_id IS NOT NULL;

-- Multi-tenant isolation (see pgokf.bundles): opt-in-by-usage RLS on the
-- denormalized tenant_id. Not forced; no API role holds any grant on the raw
-- tables (readers get the pgokf.current_relationships projection only), so the
-- policies are defense in depth.
ALTER TABLE pgokf.relationship_publication ENABLE ROW LEVEL SECURITY;
CREATE POLICY relationship_publication_tenant_isolation ON pgokf.relationship_publication
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

ALTER TABLE pgokf.relationship ENABLE ROW LEVEL SECURITY;
CREATE POLICY relationship_tenant_isolation ON pgokf.relationship
    USING (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
             OR pg_catalog.current_setting('pgokf.tenant', true) = '')
            AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
    WITH CHECK (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
        OR tenant_id = pg_catalog.current_setting('pgokf.tenant', true));

REVOKE ALL ON pgokf.relationship_publication FROM PUBLIC;
REVOKE ALL ON pgokf.relationship FROM PUBLIC;

COMMENT ON TABLE pgokf.relationship_publication IS
    'Relationship publication ledger: one immutable attempt/result record per (tenant_id, producer, source_bundle_id, publication_generation), bound to a live pgokf.publication_fence slot (fencing_token) and to the catalog generation the set was computed against (expected_catalog_generation; activated_catalog_generation once active). State staged (invisible until the matching catalog generation is accepted by a refresh) / active / superseded. relationship_set_hash is the BLAKE3 digest of the canonicalized row set and doubles as the idempotency key: an identical retried replace_relationships is a no-op, a differing one under the same key is a 23505 conflict. The bundle reference detaches (ON DELETE SET NULL) so a hard deletion never erases the audit row; source_bundle_path is the durable identity snapshot. Live-key uniqueness is a partial index over attached rows only, so detached ledger rows never collide. When competing staged attempts of one producer scope expect the accepted generation, only the newest publication_generation activates. Superseded retention is bounded: an activation hard-deletes superseded publications whose updated_at is more than 30 days old - the activating bundle''s own and the detached ledger''s alike (their rows cascade) - and an unregister/purge sweeps the aged detached rows it leaves behind, so detached history never outlives the window, while the immediately previous superseded set is always retained with its rows and activation evidence. Granted to no API role.';
COMMENT ON COLUMN pgokf.relationship_publication.publication_id IS
    'Surrogate identity of the publication (GENERATED ALWAYS AS IDENTITY), the foreign-key target of pgokf.relationship.';
COMMENT ON COLUMN pgokf.relationship_publication.tenant_id IS
    'Multi-tenant owner, stamped from the source bundle at write time; part of the natural key.';
COMMENT ON COLUMN pgokf.relationship_publication.producer IS
    'Opaque caller-supplied producer label; part of the natural key. NOT authorization: replace_relationships requires session_user membership in pgokf_writer (admin inherits).';
COMMENT ON COLUMN pgokf.relationship_publication.source_bundle_id IS
    'Live reference to the bundle whose concepts are the relationship sources, or NULL after the bundle was unregistered/purged (ON DELETE SET NULL: a hard deletion never cascades away publication audit). Use source_bundle_path for the durable identity.';
COMMENT ON COLUMN pgokf.relationship_publication.source_bundle_path IS
    'Immutable snapshot of the source bundle''s canonical path (or content:<name> key) at write time; survives bundle deletion.';
COMMENT ON COLUMN pgokf.relationship_publication.publication_generation IS
    'The producer-side monotonic publication generation; equals the target_generation of the publication fence that authorized the write; part of the natural key.';
COMMENT ON COLUMN pgokf.relationship_publication.expected_catalog_generation IS
    'The pgokf.bundles.catalog_generation the row set was computed against: the current generation activates immediately; current + 1 stages for the imminent refresh; anything else is rejected (22023).';
COMMENT ON COLUMN pgokf.relationship_publication.activated_catalog_generation IS
    'The catalog generation this publication is the visible relationship set for, once active; retained as a historical record after supersession; NULL only while staged.';
COMMENT ON COLUMN pgokf.relationship_publication.fencing_token IS
    'The live fencing token of the (tenant, producer, bundle) publication fence slot at write time; a superseded or expired token is rejected, so an older producer attempt can never publish.';
COMMENT ON COLUMN pgokf.relationship_publication.relationship_set_hash IS
    'BLAKE3 hex digest of the canonicalized relationship set (rows sorted by canonical identity, per-row digests concatenated in order). Identical retried input hashes identically regardless of submission order, making replace_relationships idempotent.';
COMMENT ON COLUMN pgokf.relationship_publication.idempotency_key IS
    'The idempotency identity of the write; equal to relationship_set_hash. A retry under the same natural key with the same key is a no-op; a different key conflicts (23505).';
COMMENT ON COLUMN pgokf.relationship_publication.manifest_hash IS
    'Hash of the publication manifest the issuing fence carried (producer-supplied evidence, copied from the fence slot); NULL when the fence named none.';
COMMENT ON COLUMN pgokf.relationship_publication.state IS
    'staged (written against the next catalog generation; invisible until a refresh accepts exactly that generation), active (the current visible set for its activated generation), or superseded (replaced by a newer activation or left behind by a generation advance).';
COMMENT ON COLUMN pgokf.relationship_publication.row_count IS
    'Number of relationship rows in the set as declared at write time; 0 is a deliberate empty set (an empty replacement removes the prior set on activation). Activation-time source quarantine can remove rows, so the stored rows of an activated publication may be fewer than row_count.';
COMMENT ON COLUMN pgokf.relationship_publication.created_by IS
    'The session_user that wrote the publication, captured by column default.';
COMMENT ON COLUMN pgokf.relationship_publication.created_at IS
    'When the publication was written (transaction now()).';
COMMENT ON COLUMN pgokf.relationship_publication.activated_at IS
    'When the publication became active; retained as a historical record after supersession; NULL only while staged.';
COMMENT ON COLUMN pgokf.relationship_publication.updated_at IS
    'When this row last changed (activation or supersession); the supersession timestamp starts the 30-day window after which the next activation - or, for a detached row, the unregister/purge that detached it - prunes the superseded publication.';

COMMENT ON TABLE pgokf.relationship IS
    'The typed relationship rows of one publication (fk pgokf.relationship_publication): source concept, producer-defined namespaced relation_type (opaque text; the catalog never enumerates or interprets it), direction, the optional resolved target (target_bundle_id, target_concept_id), an optional opaque external target identifier, opaque source_location/provenance jsonb, confidence, the unresolved/cross_bundle flags, and the canonical ordinal/row hash. Rows are written once with their publication and never mutated except by activation-time target re-resolution; the one removal path is activation-time source validation, which quarantines (deletes) a staged row whose source concept does not exist in the accepted catalog generation, so a nonexistent source can never become a graph node or appear in pgokf.current_relationships. Publications are retained as audit, so rows are too, for as long as the publication itself is retained. Granted to no API role; readers use pgokf.current_relationships.';
COMMENT ON COLUMN pgokf.relationship.publication_id IS
    'The publication this row belongs to (part of the primary key).';
COMMENT ON COLUMN pgokf.relationship.ordinal IS
    'Zero-based position of the row in the canonical (sorted) order of its publication''s set, making the stored order deterministic and identical for a retried submission.';
COMMENT ON COLUMN pgokf.relationship.tenant_id IS
    'Multi-tenant owner, denormalized from the publication for a local row-level-security predicate; always equals the publication''s tenant_id.';
COMMENT ON COLUMN pgokf.relationship.source_bundle_id IS
    'Snapshot of the source bundle identity at write time (denormalized from the publication so it survives the publication''s ON DELETE SET NULL detach). Concept ids are unique only within a bundle: every graph key is (source_bundle_id, source_concept_id).';
COMMENT ON COLUMN pgokf.relationship.source_concept_id IS
    'Concept id of the relationship source within the source bundle.';
COMMENT ON COLUMN pgokf.relationship.relation_type IS
    'Producer-defined namespaced relation type (<namespace>:<name>, at most 128 characters). Opaque to the catalog: no domain relation enumeration exists and none is validated beyond the namespaced shape.';
COMMENT ON COLUMN pgokf.relationship.direction IS
    'directed (traversed source -> target; the default) or undirected (traversed both ways by concept_relationship_neighbors).';
COMMENT ON COLUMN pgokf.relationship.target_bundle_id IS
    'The resolved target''s bundle, when the endpoint validated at write (or activation) time against a bundle active and visible to the writer; NULL for external and unresolved-with-dropped-reference rows. Not a foreign key: the target identity is a snapshot and a target bundle''s later deletion must not rewrite relationship audit.';
COMMENT ON COLUMN pgokf.relationship.target_concept_id IS
    'The resolved target''s concept id within target_bundle_id, or the producer-declared target concept retained as opaque metadata on an unresolved row whose bundle IS visible but lacks the concept (when the target bundle itself was absent, inactive, or invisible to the writer, both endpoint references are dropped - the invisible and absent cases are indistinguishable); NULL for external and target-less rows.';
COMMENT ON COLUMN pgokf.relationship.external_target IS
    'Opaque producer-defined identifier of a target outside the catalog (mutually exclusive with a resolved target concept); never resolved or traversed.';
COMMENT ON COLUMN pgokf.relationship.source_location IS
    'Opaque producer-supplied jsonb locating the relationship in the producer''s own storage (never interpreted by the catalog).';
COMMENT ON COLUMN pgokf.relationship.confidence IS
    'Optional producer-supplied confidence in [0, 1]; opaque metadata.';
COMMENT ON COLUMN pgokf.relationship.unresolved IS
    'True when no live resolved target backs the row: an external target, no declared target, a target concept absent at write/activation time, or a target bundle that was absent, inactive, or invisible to the writer (the invisible and absent cases are indistinguishable - no existence leak). Unresolved rows are returned as metadata and never materialized as traversal edges.';
COMMENT ON COLUMN pgokf.relationship.cross_bundle IS
    'True when the writer declared a target in a bundle other than the source bundle (recorded from the declaration, even when the target endpoint did not resolve).';
COMMENT ON COLUMN pgokf.relationship.provenance IS
    'Opaque producer-supplied jsonb provenance (never interpreted by the catalog).';
COMMENT ON COLUMN pgokf.relationship.row_hash IS
    'BLAKE3 hex digest of the row''s canonical text; the publication''s relationship_set_hash is computed over these in ordinal order.';

--     12b. The reader projection (the current_relationships_view block of
--          src/catalog/relationships.rs, verbatim).
CREATE VIEW pgokf.current_relationships AS
SELECT p.publication_id,
       p.producer,
       p.publication_generation,
       p.activated_catalog_generation AS catalog_generation,
       r.source_bundle_id,
       r.source_concept_id,
       r.relation_type,
       r.direction,
       r.target_bundle_id,
       r.target_concept_id,
       r.external_target,
       r.source_location,
       r.confidence,
       r.unresolved,
       r.cross_bundle,
       r.provenance,
       r.ordinal,
       r.row_hash,
       r.tenant_id
FROM pgokf.relationship r
JOIN pgokf.relationship_publication p ON p.publication_id = r.publication_id
JOIN pgokf.bundles sb
  ON sb.id = p.source_bundle_id AND sb.enabled AND sb.retired_at IS NULL
LEFT JOIN pgokf.bundles tb ON tb.id = r.target_bundle_id
WHERE p.state = 'active'
  AND (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
         OR pg_catalog.current_setting('pgokf.tenant', true) = '')
        AND NOT (SELECT pgokf.tenant_required()))
       OR p.tenant_id = pg_catalog.current_setting('pgokf.tenant', true))
  AND (r.unresolved
       OR r.target_bundle_id IS NULL
       OR (tb.enabled AND tb.retired_at IS NULL
           AND (((pg_catalog.current_setting('pgokf.tenant', true) IS NULL
                  OR pg_catalog.current_setting('pgokf.tenant', true) = '')
                 AND NOT (SELECT pgokf.tenant_required()))
                OR tb.tenant_id = pg_catalog.current_setting('pgokf.tenant', true))));

COMMENT ON VIEW pgokf.current_relationships IS
    'Reader projection of the current typed relationships: the rows of every ACTIVE relationship publication whose source bundle is active (enabled and not retired), tenant-scoped like the projection tables, with resolved rows hidden when their target bundle is not currently active and tenant-visible (unresolved and external rows remain as metadata and are never materialized as nonexistent references). Only the active relationship generation is exposed: staged and superseded publications stay invisible here, so no query can combine new concept content with old-generation relationships. SELECT is granted to pgokf_reader; the raw pgokf.relationship / pgokf.relationship_publication tables are granted to no API role.';
GRANT SELECT ON pgokf.current_relationships TO pgokf_reader;

--     12c. The composite result types (the relationship_types block of
--          src/catalog/relationships.rs, verbatim).
CREATE TYPE pgokf.relationship_publication_info AS (
    publication_id     bigint,
    tenant_id          text,
    producer           text,
    source_bundle_id   bigint,
    source_bundle_path text,
    publication_generation bigint,
    expected_catalog_generation bigint,
    activated_catalog_generation bigint,
    fencing_token      bigint,
    relationship_set_hash text,
    manifest_hash      text,
    state              text,
    row_count          integer,
    idempotency_key    text,
    created_at         timestamptz,
    activated_at       timestamptz
);

COMMENT ON TYPE pgokf.relationship_publication_info IS
    'One relationship publication as pgokf.replace_relationships reports it: the natural key, the generation/fence binding, the relationship-set hash (doubling as the idempotency key), the state (staged/active/superseded), the row count, and the timestamps.';

CREATE TYPE pgokf.relationship_neighbor AS (
    start_bundle_id    bigint,
    start_concept_id   text,
    bundle_id          bigint,
    concept_id         text,
    hops               integer,
    path_bundle_ids    bigint[],
    path_concept_ids   text[],
    relation_type      text,
    title              text,
    freshness_state    text,
    freshness_reasons  text[],
    freshness_scope    text,
    stale_since        timestamptz,
    observed_revision  text,
    indexed_revision   text,
    published_revision text,
    catalog_generation bigint,
    last_reconciled_at timestamptz,
    embedding_state    text,
    embedding_model    text,
    embedding_dim      integer,
    embedding_input_hash text,
    embedded_at        timestamptz
);

COMMENT ON TYPE pgokf.relationship_neighbor IS
    'One concept reachable from a start concept through pgokf.current_relationships: the (bundle_id, concept_id) node, shortest hop count, the path taken as parallel bundle/concept arrays, the relation type of the reaching edge, the title, and the effective freshness annotation (state, reasons, scope, stale_since, opaque revisions, catalog generation, last_reconciled_at) plus the embedding provenance - the same metadata contract as pgokf.concept_search_fresh.';

--     12d. The SQL-callable functions, declared exactly as the 0.3.0-dev
--          install script declares them (C-language wrappers exported by the
--          0.3.0-dev shared library).
CREATE FUNCTION pgokf."concept_relationship_neighbors"(
    "start_bundle_id" bigint,
    "start_concept_id" TEXT,
    "max_hops" INT DEFAULT 2,
    "direction" TEXT DEFAULT 'outbound',
    "relation_types" TEXT[] DEFAULT NULL,
    "max_results" INT DEFAULT 500
) RETURNS SETOF pgokf.relationship_neighbor
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'concept_relationship_neighbors_wrapper';

CREATE FUNCTION pgokf."replace_relationships"(
    "producer" TEXT,
    "source_bundle_id" bigint,
    "publication_generation" bigint,
    "expected_catalog_generation" bigint,
    "fencing_token" bigint,
    "rows" jsonb
) RETURNS pgokf.relationship_publication_info
STRICT
LANGUAGE c
AS 'MODULE_PATHNAME', 'replace_relationships_wrapper';

--     12e. The hardening (the relationship_function_hardening block of
--          src/catalog/relationships.rs, verbatim).
ALTER FUNCTION pgokf.replace_relationships(text, bigint, bigint, bigint, bigint, jsonb)
    SECURITY DEFINER SET search_path = pg_catalog, pg_temp;

REVOKE ALL ON FUNCTION pgokf.replace_relationships(text, bigint, bigint, bigint, bigint, jsonb) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.replace_relationships(text, bigint, bigint, bigint, bigint, jsonb) TO pgokf_writer;
COMMENT ON FUNCTION pgokf.replace_relationships(text, bigint, bigint, bigint, bigint, jsonb) IS
    'Replace a source bundle''s typed relationship set for one publication generation, atomically, returning pgokf.relationship_publication_info. Writer-tier (pgokf_writer; admin inherits), SECURITY DEFINER, tenant-confined; producer is an opaque label, not authorization. Compare-and-set under the source bundle advisory lock: fencing_token must be the live unexpired token of the (tenant, producer, bundle) publication fence and publication_generation must equal its target (22023 otherwise, so a superseded or expired attempt never publishes). Generation rule, against the bundle''s current catalog generation G: expected = G activates immediately (superseding the producer''s prior active publication); expected = G + 1 stages the set, invisible until a refresh accepts exactly that generation (run_bundle_sync activates it in the sync transaction and supersedes the prior generation''s publications, so new concepts never combine with old-generation relationships); anything else is 22023. rows is a jsonb array of row objects (source_concept_id, namespaced relation_type ''<namespace>:<name>'', optional direction directed|undirected, optional resolved target target_bundle_id + target_concept_id (concept alone targets the source bundle), optional external_target (mutually exclusive with a resolved target), optional source_location/provenance jsonb, optional confidence in [0,1]); at most 10000 rows, duplicate canonical identities are 22023. Endpoint validation never leaks: an absent, inactive, or cross-tenant target bundle resolves to the same unresolved row with the bundle reference dropped. Rows are canonicalized (sorted) and hashed: an identical retried call is a no-op, the same publication key with a different set is 23505. An empty rows array removes the prior set on activation. A bundle whose relationship coverage a refresh supersedes without replacement stays stale (reason relationship_coverage_missing) and pgokf.mark_fresh refuses until a matching replacement activates.';

REVOKE ALL ON FUNCTION pgokf.concept_relationship_neighbors(bigint, text, integer, text, text[], integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_relationship_neighbors(bigint, text, integer, text, text[], integer) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_relationship_neighbors(bigint, text, integer, text, text[], integer) IS
    'Cycle-safe breadth-first traversal of the current typed relationships from (start_bundle_id, start_concept_id), over pgokf.current_relationships only (the active relationship generation; active bundles; tenant-scoped), keyed on (bundle_id, concept_id). direction is ''outbound'' (default), ''inbound'', or ''both'' (22023 otherwise); relation_types NULL or empty follows every type; max_hops must be at least 1 and is capped at pgokf.max_graph_hops; max_results defaults to 500 and is capped at 10000. Unresolved and external rows never become edges. Each node returns its shortest hop count, path (parallel bundle/concept arrays), the reaching edge''s relation type, title, and the effective freshness annotation plus embedding provenance of pgokf.concept_search_fresh. An unknown or inactive seed yields an empty result. Reader-level, invoker rights. pgokf.concept_neighbors (the Markdown link graph) is unchanged.';

--     12f. The refreshed capability declaration (the effective_freshness_view
--          block of src/catalog/freshness.rs), adding typed_relationships, and
--          the refreshed reason-code/mark_fresh comments (COMMENT ON replaces).
CREATE OR REPLACE FUNCTION pgokf.capabilities() RETURNS jsonb
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
COMMENT ON FUNCTION pgokf.capabilities() IS
    'The catalog capabilities this pgokf release implements, as a jsonb object of capability name to interface version: catalog_generation, publication_fence, freshness_dependency, effective_freshness, catalog_change_event, search_freshness, embedding_freshness, and typed_relationships (all version 1). Immutable; a producer declares the capabilities it requires and checks them here. Later releases only add entries or raise versions.';

COMMENT ON COLUMN pgokf.bundle_freshness.reason_codes IS
    'Machine-readable, producer-supplied reason codes explaining the current non-fresh state (merged, deduplicated). Catalog-defined codes: legacy_pre_0.3.0, dependency_source_changed, change_scope_unknown, bundle_retired, bundle_restored, bundle_disabled, relationship_coverage_missing; producers may add their own opaque codes.';
COMMENT ON FUNCTION pgokf.mark_fresh(bigint, bigint, bigint, text, text, jsonb, text) IS
    'Compare-and-set reconciliation completion: mark the bundle fresh only if its observed source revision still equals expected_observed_source_generation AND its live catalog generation equals expected_catalog_generation AND no newer materialized generation exists AND claimed_invalidation_epoch - the claim token this attempt''s own pgokf.mark_reconciling returned - covers the newest dependency invalidation epoch and the standing claim (a completion based on evidence older than the latest dependency invalidation is refused, and a newer attempt''s claim can never validate an older attempt''s token) AND no relationship_coverage_missing evidence stands (a refresh that superseded the bundle''s relationship coverage must be answered with a matching pgokf.replace_relationships publication first) AND it is not retired; returns false (changing nothing) otherwise, so a superseded attempt can never clear staleness. The check runs under the bundle advisory lock, so it never certifies a generation or epoch older than a committed mutation it waited behind. On success records the manifest hash and embedding contract evidence and sets last_reconciled_at. Writer-tier; tenant-confined.';

-- ===========================================================================
-- 13. Type-constrained semantic/hybrid search: concept_search_semantic and
--     concept_search_hybrid gain one optional trailing argument,
--     concept_types text[] DEFAULT NULL - a type-membership filter applied
--     inside the ranked query BEFORE the candidate list is truncated, so a
--     type-filtered call returns up to limit_count eligible hits of the
--     selected types even when excluded types outrank them (a caller-side
--     post-filter of a truncated window could return an empty or underfilled
--     page while eligible hits existed). The internal bm25_hits helper gains
--     the same trailing parameter so the hybrid lexical half is constrained
--     before its own truncation under the pg_search provider too.
--
--     A trailing-default argument list is a DIFFERENT function identity in
--     pg_proc: the widened functions are new rows, not redefinitions, and
--     leaving the superseded overloads in place would (a) make pg_proc carry
--     two overloads where a fresh install carries one, and (b) make a call
--     that omits the new argument ambiguous between the two, breaking the
--     very backward compatibility the default preserves. The superseded
--     overloads are therefore dropped here and replaced in the same
--     transaction by STRICT SUPERSETS that resolve every historical call
--     through the new argument's NULL default (= no filter, the pre-0.3.0
--     behavior). This mirrors the concept_search after_cursor replacement of
--     0.1.8 -> 0.1.9; a function carries no data, so no row is touched. The
--     declarations below are verbatim those of the fresh 0.3.0-dev install
--     (the concept_search_semantic / concept_search_hybrid pg_externs and the
--     embedding_function_hardening block of src/catalog/embedding.rs, and the
--     bm25_hits_function block of src/catalog/search_backend.rs).
DROP FUNCTION IF EXISTS pgokf.concept_search_semantic(real[], bigint, integer);
DROP FUNCTION IF EXISTS pgokf.concept_search_hybrid(text, real[], bigint, integer);
DROP FUNCTION IF EXISTS pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text);

CREATE FUNCTION pgokf."concept_search_semantic"(
    "query_embedding" real[],
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" INT DEFAULT 10,
    "concept_types" TEXT[] DEFAULT NULL
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'concept_search_semantic_wrapper';

CREATE FUNCTION pgokf."concept_search_hybrid"(
    "query" TEXT,
    "query_embedding" real[],
    "bundle_id" bigint DEFAULT NULL,
    "limit_count" INT DEFAULT 10,
    "concept_types" TEXT[] DEFAULT NULL
) RETURNS SETOF pgokf.concept_search_result
STABLE PARALLEL RESTRICTED
LANGUAGE c
AS 'MODULE_PATHNAME', 'concept_search_hybrid_wrapper';

REVOKE ALL ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer, text[]) FROM PUBLIC;
REVOKE ALL ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer, text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer, text[]) TO pgokf_reader;
GRANT EXECUTE ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer, text[]) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.concept_search_semantic(real[], bigint, integer, text[]) IS
    'Semantic nearest-neighbor search: rank concepts by pgvector cosine distance to query_embedding (rank = normalized cosine similarity). Reader-level, invoker rights; active bundles only. query_embedding must have embedding_dim dimensions; limit_count in 1..=500. Requires pgvector: raises 22023 naming the missing dependency when it is not installed (no lexical fallback). Only ELIGIBLE embeddings rank: source_file_hash equal to the concept''s current file_hash, model/dimension/contract matching the embedding_model/embedding_dim/embedding_contract policy, and the concept effectively fresh (bundle freshness fresh, no covering concept/path override); a stale or legacy (NULL-provenance) row never ranks even while the HNSW index physically retains it. concept_types is an optional type-membership filter (a hit''s type must be one of the listed types; NULL or empty is no filter) applied inside the ranked query BEFORE the candidate list is truncated to limit_count, so a filtered call returns up to limit_count eligible hits of the selected types even when excluded types outrank them.';
COMMENT ON FUNCTION pgokf.concept_search_hybrid(text, real[], bigint, integer, text[]) IS
    'Hybrid search: Reciprocal Rank Fusion (RRF, k=60) of the lexical result of query (via the configured search_backend) and the semantic result of query_embedding, fused entirely in SQL (rank = fused RRF score). Reader-level, invoker rights; enabled bundles only; limit_count in 1..=500. The semantic component ranks eligible (current, fresh) embeddings only, so an ineligible vector never leaks into the fused result; the lexical component may still return a stale concept, labeled by pgokf.concept_search_fresh. Degrades to lexical-only with a WARNING when pgvector is not installed. concept_types is an optional type-membership filter (a hit''s type must be one of the listed types; NULL or empty is no filter) applied to BOTH ranked inputs before either is truncated to limit_count, so the fused page is exactly the type-filtered top-limit_count, never underfilled by higher-ranked excluded types.';

CREATE FUNCTION pgokf.bm25_hits(
    p_query text,
    p_bundle_id bigint,
    p_limit bigint,
    p_text_search_config text,
    p_concept_type text,
    p_tags text[],
    p_status text,
    p_trust_tier text,
    p_after_rank real,
    p_after_bundle_id bigint,
    p_after_concept_id text,
    p_concept_types text[])
RETURNS TABLE (
    bundle_id bigint,
    concept_id text,
    path text,
    title text,
    type text,
    rank real,
    headline text)
LANGUAGE plpgsql
STABLE PARALLEL SAFE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
    -- Resolved once, up front, and bound into the query as a parameter: the
    -- policies inline current_setting() directly, but pg_search 0.25 cannot
    -- plan its scan under a predicate that calls a function (that inline
    -- form is exactly what row-level security injects for non-owners, and
    -- exactly what the Unsupported-query-shape error was about). Empty = unset.
    v_tenant text := NULLIF(pg_catalog.current_setting('pgokf.tenant', true), '');
BEGIN
    -- The policies' rule, applied here because this body bypasses them: an
    -- unscoped session sees nothing when the catalog requires a tenant.
    IF v_tenant IS NULL AND pgokf.tenant_required() THEN
        RETURN;
    END IF;
    RETURN QUERY
    SELECT hits.bundle_id,
           hits.concept_id,
           hits.path,
           hits.title,
           hits.type,
           hits.rank,
           hits.headline
    FROM (
        SELECT c.bundle_id AS bundle_id,
               c.id AS concept_id,
               c.path AS path,
               c.title AS title,
               c.type AS type,
               paradedb.score(c) AS rank,
               pg_catalog.ts_headline(
                   p_text_search_config::pg_catalog.regconfig,
                   pg_catalog.concat_ws(' ', c.title, c.description, c.body_text),
                   pg_catalog.websearch_to_tsquery(p_text_search_config::pg_catalog.regconfig, p_query)) AS headline
        FROM pgokf.concepts c
        JOIN pgokf.bundles b ON b.id = c.bundle_id AND b.enabled AND b.retired_at IS NULL
        LEFT JOIN pgokf.concept_provenance cp
               ON cp.bundle_id = c.bundle_id AND cp.concept_id = c.id
        WHERE c.id @@@ paradedb.boolean(should => ARRAY[
                  paradedb.match('title', p_query),
                  paradedb.match('description', p_query),
                  paradedb.match('body_text', p_query)])
          AND (v_tenant IS NULL OR c.tenant_id = v_tenant)
          AND (p_bundle_id IS NULL OR c.bundle_id = p_bundle_id)
          AND (p_concept_type IS NULL OR c.type = p_concept_type)
          AND (p_tags IS NULL OR c.tags @> p_tags)
          AND (p_status IS NULL OR cp.status = p_status)
          AND (p_trust_tier IS NULL OR cp.trust_tier = p_trust_tier)
          AND (p_concept_types IS NULL OR c.type = ANY(p_concept_types))
    ) AS hits
    WHERE p_after_rank IS NULL
       OR hits.rank < p_after_rank
       OR (hits.rank = p_after_rank AND hits.bundle_id > p_after_bundle_id)
       OR (hits.rank = p_after_rank AND hits.bundle_id = p_after_bundle_id AND hits.concept_id > p_after_concept_id)
    ORDER BY hits.rank DESC, hits.bundle_id ASC, hits.concept_id ASC
    LIMIT p_limit;
END
$fn$;

REVOKE ALL ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[]) TO pgokf_reader;
COMMENT ON FUNCTION pgokf.bm25_hits(text, bigint, bigint, text, text, text[], text, text, real, bigint, text, text[]) IS
    'Internal helper behind concept_search when search_backend = bm25 resolves to the ParadeDB pg_search provider (the pg_textsearch provider runs inline with invoker rights and does not use it); not part of the stable API. Runs the ParadeDB pg_search BM25 hit query with the owner''s privileges (row-level security wraps the catalog tables in a shape pg_search cannot plan for non-owners) while applying the same pgokf.tenant scoping the policies enforce, over active bundles only, with concept_search''s filters (p_concept_types is the type-membership form concept_search_hybrid uses to constrain the lexical candidate list before truncation), keyset cursor, and limit. Reader-level; returns exactly the rows concept_search would.';

-- ===========================================================================
-- 14. The scheduled-refresh read surface (the scheduled_refreshes_reader
-- block of src/catalog/schedule.rs, verbatim). pg_cron grants SELECT on
-- cron.job to PUBLIC but restricts rows to username = current_user, and
-- pgokf.schedule_refresh - SECURITY DEFINER since 0.1.9 - registers every
-- job under the extension owner's identity, so an ordinary login reading
-- cron.job directly sees none of them. pgokf.list_scheduled_refreshes runs
-- as that owner (SECURITY DEFINER), confines itself to the session tenant
-- like the RLS-backed readers, and raises the same 22023 schedule_refresh
-- raises when pg_cron is not installed. Reader-tier.
-- ===========================================================================
CREATE FUNCTION pgokf.list_scheduled_refreshes()
RETURNS TABLE (bundle_id bigint, schedule text)
LANGUAGE plpgsql STABLE
SECURITY DEFINER SET search_path = pg_catalog, pg_temp
AS $list_scheduled_refreshes$
DECLARE
    v_tenant text := NULLIF(pg_catalog.current_setting('pgokf.tenant', true), '');
BEGIN
    -- The read-side tenant rule, applied explicitly because this body runs
    -- as the extension owner and so bypasses row-level security: an
    -- unscoped session sees nothing when the catalog requires a tenant,
    -- exactly like the RLS-backed readers.
    IF v_tenant IS NULL AND pgokf.tenant_required() THEN
        RETURN;
    END IF;
    -- Reading the jobs needs pg_cron exactly like scheduling them: refuse
    -- with the same 22023 naming the missing dependency rather than
    -- failing on the absent cron.job relation.
    IF NOT (SELECT pg_catalog.count(*) > 0
            FROM pg_catalog.pg_extension
            WHERE extname = 'pg_cron') THEN
        RAISE EXCEPTION
            'listing scheduled refreshes requires the pg_cron extension, which is not installed; add pg_cron to shared_preload_libraries and run CREATE EXTENSION pg_cron'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    -- SECURITY DEFINER is the point of this surface: pg_cron grants SELECT
    -- on cron.job to PUBLIC but its row-security policy restricts rows to
    -- username = current_user, and pgokf.schedule_refresh - itself
    -- SECURITY DEFINER - registers every job under the extension owner's
    -- identity. An ordinary login therefore reads zero rows whatever its
    -- grants; running as that owner, this read sees exactly the jobs the
    -- extension manages. (A job another role created directly under the
    -- convention's name is pg_cron-invisible to it, by the same policy.)
    RETURN QUERY
    SELECT b.id, j.schedule
    FROM cron.job AS j
    JOIN pgokf.bundles AS b
      ON b.id = pg_catalog.substring(j.jobname, 'pgokf_refresh_([0-9]{1,18})')::bigint
    WHERE j.jobname ~ '^pgokf_refresh_[0-9]{1,18}$'
      AND (v_tenant IS NULL OR b.tenant_id = v_tenant)
    ORDER BY b.id;
END
$list_scheduled_refreshes$;
REVOKE ALL ON FUNCTION pgokf.list_scheduled_refreshes() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.list_scheduled_refreshes() TO pgokf_reader;
COMMENT ON FUNCTION pgokf.list_scheduled_refreshes() IS
    'Every scheduled bundle refresh the extension manages, as (bundle_id, schedule) rows ordered by bundle id: the read counterpart of pgokf.schedule_refresh / unschedule_refresh. SECURITY DEFINER because pg_cron restricts cron.job rows to username = current_user (grants do not change that) while schedule_refresh registers jobs under the extension owner''s identity - this read runs as that owner, so the app''s login sees the schedules it would otherwise read as zero rows. Tenant-confined like the RLS-backed readers (an unscoped session sees nothing when require_tenant is on; a scoped session sees only its tenant''s jobs), joining each pgokf_refresh_<id> job back to its bundle. Requires pg_cron: raises 22023 naming the missing dependency when it is not installed, exactly like schedule_refresh. Reader-tier (granted to pgokf_reader, inherited by writer and admin).';

-- ===========================================================================
-- 15. The external repository-registry surface (the registry_surface block
-- of src/catalog/registry.rs, verbatim). A registered repository's row is
-- owned by the repository-registry producer service, a separate codebase
-- whose migrations create the ast_graph schema; the coupling is
-- runtime-only, exactly like the pg_cron adapter. Reads follow the narrow
-- grant pattern: pgokf_reader gets USAGE on the ast_graph schema and SELECT
-- on exactly the columns the admin UI lists (never checkout_path or the
-- producer's internal graph_id, and no secret exists here at all - fetch
-- credentials live behind the producer's admin API, which never returns
-- them; tenant_id is granted so callers can confine their read to the
-- session tenant), applied only where the table is present. Writes go
-- through the SECURITY DEFINER functions below, granted to pgokf_admin; each
-- resolves the table at call time, raises a curated 22023 where it is
-- absent, and confines its update to the session tenant (pgokf.tenant), so a
-- cross-tenant id earns the same 22023 as an unknown one.
-- ===========================================================================
DO $registry_reader_grant$
BEGIN
    IF pg_catalog.to_regclass('ast_graph.repository_registry') IS NOT NULL THEN
        GRANT USAGE ON SCHEMA ast_graph TO pgokf_reader;
        GRANT SELECT (repository_id, repository_key, project_name, default_branch,
                      remote_url, status, poll_interval_seconds,
                      last_indexed_commit, last_published_commit,
                      last_published_generation, tenant_id)
            ON ast_graph.repository_registry TO pgokf_reader;
    ELSE
        RAISE NOTICE 'pgokf: ast_graph.repository_registry is not present in this database; skipping the registry reader grant (the producer service installs that schema)';
    END IF;
END
$registry_reader_grant$;

CREATE FUNCTION pgokf.registry_set_status(repository_id uuid, status text)
RETURNS void
LANGUAGE plpgsql
VOLATILE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $registry_set_status$
BEGIN
    -- Late binding is the point: the table reference below is planned on
    -- first execution, so this curated 22023 answers instead of a bare
    -- 42P01 in a database the producer does not share.
    IF pg_catalog.to_regclass('ast_graph.repository_registry') IS NULL THEN
        RAISE EXCEPTION
            'registry writes require the producer service schema (ast_graph.repository_registry), which is not present in this database'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    -- The producer's poll loop enumerates status 'active' rows only, so
    -- 'paused' stops a repository's reconciliation without deleting
    -- anything; any other value is refused rather than inventing a state
    -- the producer does not define.
    IF status NOT IN ('active', 'paused') THEN
        RAISE EXCEPTION
            'registry status must be active or paused, not %', status
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    UPDATE ast_graph.repository_registry AS r
       SET status = registry_set_status.status,
           updated_at = pg_catalog.now()
     WHERE r.repository_id = registry_set_status.repository_id
       -- The session tenant confines the write: a cross-tenant id finds no
       -- row and earns the same 22023 an unknown id does, so the answer
       -- never reveals that another tenant's repository exists. An unset,
       -- empty (the GUC's registered default), or all-whitespace
       -- pgokf.tenant all normalize to the producer's 'default' tenant.
       AND r.tenant_id = COALESCE(NULLIF(pg_catalog.btrim(pg_catalog.current_setting('pgokf.tenant', true)), ''), 'default');
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'no registered repository with id %', repository_id
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
END
$registry_set_status$;
REVOKE ALL ON FUNCTION pgokf.registry_set_status(uuid, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.registry_set_status(uuid, text) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.registry_set_status(uuid, text) IS
    'Pause or resume one registered repository of the external repository-registry producer service by setting its ast_graph.repository_registry status (active or paused; the producer polls active rows only, so pausing stops reconciliation without deleting the registration). Admin-only (pgokf_admin), SECURITY DEFINER over a table no API role may write directly; tenant-confined: the update matches only rows whose tenant_id equals the session''s pgokf.tenant setting, with an unset, empty (the GUC''s registered default), or all-whitespace value normalizing to the producer''s ''default'' tenant, so a cross-tenant id earns the same 22023 as an unknown one without revealing that the row exists. The producer schema coupling is runtime-only - the curated 22023 names the missing dependency when ast_graph.repository_registry is absent, and 22023 also covers an unknown repository id or a status outside (active, paused).';

CREATE FUNCTION pgokf.registry_set_poll_interval(repository_id uuid, poll_interval_seconds integer)
RETURNS void
LANGUAGE plpgsql
VOLATILE
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $registry_set_poll_interval$
BEGIN
    IF pg_catalog.to_regclass('ast_graph.repository_registry') IS NULL THEN
        RAISE EXCEPTION
            'registry writes require the producer service schema (ast_graph.repository_registry), which is not present in this database'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    -- The producer polls a repository at most this often (its own floor is
    -- 5 seconds); the day-long ceiling keeps a typo from silencing a
    -- repository for weeks.
    IF poll_interval_seconds < 5 OR poll_interval_seconds > 86400 THEN
        RAISE EXCEPTION
            'poll interval must be between 5 and 86400 seconds, not %', poll_interval_seconds
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    UPDATE ast_graph.repository_registry AS r
       SET poll_interval_seconds = registry_set_poll_interval.poll_interval_seconds,
           updated_at = pg_catalog.now()
     WHERE r.repository_id = registry_set_poll_interval.repository_id
       AND r.tenant_id = COALESCE(NULLIF(pg_catalog.btrim(pg_catalog.current_setting('pgokf.tenant', true)), ''), 'default');
    IF NOT FOUND THEN
        RAISE EXCEPTION
            'no registered repository with id %', repository_id
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
END
$registry_set_poll_interval$;
REVOKE ALL ON FUNCTION pgokf.registry_set_poll_interval(uuid, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION pgokf.registry_set_poll_interval(uuid, integer) TO pgokf_admin;
COMMENT ON FUNCTION pgokf.registry_set_poll_interval(uuid, integer) IS
    'Set one registered repository''s poll interval in seconds (5 to 86400) on the external repository-registry producer service''s ast_graph.repository_registry row: the producer reconciles an active repository at most this often. Admin-only (pgokf_admin), SECURITY DEFINER over a table no API role may write directly; tenant-confined: the update matches only rows whose tenant_id equals the session''s pgokf.tenant setting, with an unset, empty (the GUC''s registered default), or all-whitespace value normalizing to the producer''s ''default'' tenant, so a cross-tenant id earns the same 22023 as an unknown one without revealing that the row exists. The producer schema coupling is runtime-only - the curated 22023 names the missing dependency when ast_graph.repository_registry is absent, and 22023 also covers an unknown repository id or an out-of-range interval.';

-- Last, so the new relations are registered for pg_dump (the rule for every
-- upgrade script since 0.1.14). Later phases insert their sections BEFORE
-- this line.
SELECT pgokf_private.register_dump_relations();
