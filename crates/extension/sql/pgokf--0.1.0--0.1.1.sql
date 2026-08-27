-- pgokf extension upgrade: 0.1.0 -> 0.1.1
--
-- This is a documented, forward-compatible EXAMPLE upgrade script. It exists
-- to exercise and prove out the PostgreSQL extension upgrade mechanism
-- (`ALTER EXTENSION pgokf UPDATE TO '0.1.1'`) end to end, without altering any
-- catalog data or public API object. It is intentionally a no-op: no table,
-- type, function, index, grant, or comment defined by the 0.1.0 install is
-- dropped, renamed, or rewritten, so every registered bundle, concept,
-- metadata row, link, and provenance record survives the upgrade byte for byte.
--
-- pgrx names the full install script after the crate's Cargo version
-- (`pgokf--0.1.0.sql`) and copies every `sql/pgokf--<from>--<to>.sql` file it
-- finds into the extension directory, so this file becomes the update path
-- from 0.1.0 to 0.1.1 automatically. The crate version and the control file's
-- `default_version` remain 0.1.0; cutting a real 0.1.1 (or 1.0.0) release is a
-- deliberate human step that bumps those together with a full install script.
--
-- Template for a REAL future migration (keep it additive and idempotent):
--
--     -- add a new nullable column with a safe default (no rewrite on modern PG)
--     ALTER TABLE pgokf.concepts ADD COLUMN IF NOT EXISTS <name> <type>;
--     -- backfill in a way that tolerates re-runs
--     -- document the new object
--     COMMENT ON COLUMN pgokf.concepts.<name> IS '...';
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

DO $pgokf_upgrade$
BEGIN
    -- Deliberate no-op body: the upgrade transaction runs and commits, moving
    -- pg_extension.extversion to 0.1.1 while leaving all catalog data intact.
    NULL;
END
$pgokf_upgrade$;
