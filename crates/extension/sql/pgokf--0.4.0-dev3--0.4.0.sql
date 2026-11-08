-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.4.0-dev3 -> 0.4.0
--
-- Finalization receipt for the point-versioned 0.4 development cycle. Object
-- definitions and data are unchanged from dev3; this edge preserves ordinary
-- ALTER EXTENSION UPDATE for catalogs that installed the final development
-- point.

SELECT pgokf_private.register_dump_relations();