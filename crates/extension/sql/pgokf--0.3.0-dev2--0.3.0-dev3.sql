-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.3.0-dev2 -> 0.3.0-dev3
--
-- Receipts the development readiness repair. Installed object definitions
-- are unchanged. The dev -> dev1 membership lookup is repaired in its
-- original edge because early dev installations must execute that edge
-- BEFORE they can reach this one; an absent function must not cause the
-- membership probe itself to throw. Existing dev1/dev2 installations need
-- no data or object repair. Never hand-apply same-version SQL.
SELECT pgokf_private.register_dump_relations();
