-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.4.0 -> 0.5.0
--
-- Release receipt. 0.5.0 changes the companions (the Confluence ingestion
-- companion and the enforced --tls policy) and no catalog object or data:
-- this edge preserves ordinary ALTER EXTENSION UPDATE from 0.4.0.

SELECT pgokf_private.register_dump_relations();
