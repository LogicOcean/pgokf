-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.6.0 -> 0.7.0
--
-- Release receipt. 0.7.0 changes the companions (one pooled connection
-- lifecycle, so pgokf-mcp reconnects instead of exiting) and no catalog
-- object or data: this edge preserves ordinary ALTER EXTENSION UPDATE from
-- 0.6.0.

SELECT pgokf_private.register_dump_relations();
