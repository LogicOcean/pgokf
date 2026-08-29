-- pgokf extension upgrade: 0.1.12 -> 0.1.13
--
-- The 0.1.13 release is a SECURITY / BUGFIX remediation release. Every fix is
-- either internal Rust logic (query text, input validation, a Markdown-log
-- timestamp parser), companion-tool behavior (optional TLS to PostgreSQL), or
-- documentation. NONE of them changes the in-database SQL surface: no table,
-- type, function signature, index, grant, comment, role, or configuration key
-- defined by the 0.1.12 install is added, dropped, renamed, or rewritten.
--
-- Specifically, this release:
--   * rejects a non-finite (NaN/Infinity) embedding element at write time in
--     pgokf.set_concept_embedding (a new 22023 validation, same signature);
--   * rewrites pgokf.concept_neighbors' internal traversal from a simple-path-
--     enumerating recursive CTE to an O(V+E) breadth-first search (identical
--     results, same signature) to close an algorithmic-complexity DoS;
--   * re-checks purge eligibility under the per-bundle advisory lock in
--     pgokf.purge_retired to close a TOCTOU data-loss race with unretire_bundle;
--   * parses a space-separated `YYYY-MM-DD HH:MM[:SS]` leading timestamp in
--     reserved log.md entries to the real instant instead of midnight;
--   * adds a deterministic (bundle_id, concept_id) tiebreak to
--     concept_search_semantic / concept_search_hybrid ordering;
--   * filters concept_neighbors' NULL-bundle disambiguation to active bundles
--     and excludes a self-linked seed from its own neighbor set;
--   * adds optional TLS to the out-of-process companion binaries.
--
-- All of that lives in the shared library and the companion tools; none of it
-- needs SQL here. This upgrade is therefore a deliberate no-op, exactly like the
-- 0.1.0 -> 0.1.1 and 0.1.11 -> 0.1.12 example scripts. It exists so that
-- `ALTER EXTENSION pgokf UPDATE TO '0.1.13'` runs and commits end to end, moving
-- pg_extension.extversion to 0.1.13 while leaving every registered bundle,
-- concept, embedding, metadata row, link, history version, and provenance record
-- intact byte for byte. Loading the 0.1.13 shared library is what activates the
-- corrected code paths.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

DO $pgokf_upgrade$
BEGIN
    -- Deliberate no-op body: the upgrade transaction runs and commits, moving
    -- pg_extension.extversion to 0.1.13 while leaving all catalog data intact.
    -- The 0.1.13 release changes only shared-library code paths and companion
    -- tools; the in-database SQL surface is unchanged from 0.1.12.
    NULL;
END
$pgokf_upgrade$;
