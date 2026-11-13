-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.5.0 -> 0.6.0
--
-- Release receipt. 0.6.0 changes the companions (the receiver for Confluence
-- events the pgokf Forge app pushes) and no catalog object or data: this
-- edge preserves ordinary ALTER EXTENSION UPDATE from 0.5.0.

SELECT pgokf_private.register_dump_relations();
