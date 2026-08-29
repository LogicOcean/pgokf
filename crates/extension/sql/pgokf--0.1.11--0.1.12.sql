-- pgokf extension upgrade: 0.1.11 -> 0.1.12
--
-- The 0.1.12 release is a COMPANION-TOOLING release: it ships three new
-- out-of-process binaries (pgokf-embed, the reference OpenAI-compatible
-- embedder; pgokf-mcp, a Model Context Protocol server; and a --watch daemon
-- mode for pgokf-ingest) that all speak to the extension exclusively through
-- its already-shipped public SQL surface. The in-database extension itself is
-- functionally UNCHANGED from 0.1.11: no table, type, function, index, grant,
-- comment, or configuration key defined by the 0.1.11 install is added,
-- dropped, renamed, or rewritten.
--
-- This upgrade is therefore a deliberate no-op, exactly like the 0.1.0 -> 0.1.1
-- example script. It exists so that `ALTER EXTENSION pgokf UPDATE TO '0.1.12'`
-- runs and commits end to end, moving pg_extension.extversion to 0.1.12 while
-- leaving every registered bundle, concept, embedding, metadata row, link,
-- history version, and provenance record intact byte for byte. Loading the
-- 0.1.12 shared library changes no code path the catalog exercises.
--
-- Never DROP, TRUNCATE, DELETE, or rewrite existing catalog data in an upgrade
-- script: doing so would break the no-data-loss guarantee asserted by the
-- api_stability upgrade tests.

DO $pgokf_upgrade$
BEGIN
    -- Deliberate no-op body: the upgrade transaction runs and commits, moving
    -- pg_extension.extversion to 0.1.12 while leaving all catalog data intact.
    -- The 0.1.12 release adds only out-of-process companion binaries; the
    -- in-database surface is unchanged from 0.1.11.
    NULL;
END
$pgokf_upgrade$;
