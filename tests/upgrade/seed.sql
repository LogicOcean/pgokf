-- SPDX-License-Identifier: AGPL-3.0-only
-- Populate the old schema without calling new-library functions against it.
INSERT INTO pgokf.bundles (path, name, source_type, file_count, sync_hash)
VALUES ('content:upgrade-fixture', 'upgrade-fixture', 'content', 1, 'preserved');
INSERT INTO pgokf.concepts (bundle_id, id, path, title, file_hash, body_text)
SELECT id, 'entry', 'entry.md', 'Upgrade fixture', repeat('a', 64), 'preserved body'
FROM pgokf.bundles;
INSERT INTO pgokf.concept_source (bundle_id, concept_id, raw_content, byte_size)
SELECT id, 'entry', convert_to('preserved source', 'UTF8'), 16 FROM pgokf.bundles;
INSERT INTO pgokf.concept_metadata (bundle_id, concept_id, key, value)
SELECT id, 'entry', 'fixture', '{"preserved":true}'::jsonb FROM pgokf.bundles;
