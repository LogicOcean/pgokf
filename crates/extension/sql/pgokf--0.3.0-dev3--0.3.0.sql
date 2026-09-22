-- SPDX-License-Identifier: AGPL-3.0-only
-- pgokf extension upgrade: 0.3.0-dev3 -> 0.3.0
--
-- Finalization receipt of the point-versioned development cycle. Installed
-- object definitions are unchanged from dev3; this edge exists so every
-- point-versioned deployment (dev, dev1, dev2, dev3) reaches the clean 0.3.0
-- release through ordinary ALTER EXTENSION pgokf UPDATE. Never hand-apply
-- same-version SQL.
SELECT pgokf_private.register_dump_relations();
