-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.3.0-dev1 -> 0.3.0-dev2
--
-- The second point-versioned step of the 0.3.0 development cycle (see the
-- "Development point versions" section of docs/api-stability.md). This step
-- changes no object definition: it re-applies the COMMENT ON FUNCTION text
-- of pgokf.replace_relationships so the documented row ceiling matches the
-- code. The hard limit on the rows argument of one replace_relationships
-- call moved from a compile-time 10000 to the pgokf.max_relationship_rows
-- GUC (default 50000, ceiling 1000000, SIGHUP context like the extension's
-- other resource ceilings; registered by the shared library in
-- src/guc.rs, so it needs no SQL here). COMMENT ON replaces the existing
-- comment, so the statement is idempotent and an installation that reached
-- 0.3.0-dev1 by any route is identical to a fresh 0.3.0-dev2 install after
-- this script runs.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

COMMENT ON FUNCTION pgokf.replace_relationships(text, bigint, bigint, bigint, bigint, jsonb) IS
    'Replace a source bundle''s typed relationship set for one publication generation, atomically, returning pgokf.relationship_publication_info. Writer-tier (pgokf_writer; admin inherits), SECURITY DEFINER, tenant-confined; producer is an opaque label, not authorization. Compare-and-set under the source bundle advisory lock: fencing_token must be the live unexpired token of the (tenant, producer, bundle) publication fence and publication_generation must equal its target (22023 otherwise, so a superseded or expired attempt never publishes). Generation rule, against the bundle''s current catalog generation G: expected = G activates immediately (superseding the producer''s prior active publication); expected = G + 1 stages the set, invisible until a refresh accepts exactly that generation (run_bundle_sync activates it in the sync transaction and supersedes the prior generation''s publications, so new concepts never combine with old-generation relationships); anything else is 22023. rows is a jsonb array of row objects (source_concept_id, namespaced relation_type ''<namespace>:<name>'', optional direction directed|undirected, optional resolved target target_bundle_id + target_concept_id (concept alone targets the source bundle), optional external_target (mutually exclusive with a resolved target), optional source_location/provenance jsonb, optional confidence in [0,1]); at most pgokf.max_relationship_rows rows (default 50000), duplicate canonical identities are 22023. Endpoint validation never leaks: an absent, inactive, or cross-tenant target bundle resolves to the same unresolved row with the bundle reference dropped. Rows are canonicalized (sorted) and hashed: an identical retried call is a no-op, the same publication key with a different set is 23505. An empty rows array removes the prior set on activation. A bundle whose relationship coverage a refresh supersedes without replacement stays stale (reason relationship_coverage_missing) and pgokf.mark_fresh refuses until a matching replacement activates.';
