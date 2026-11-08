-- Open the 0.4.0 development line: upgrade the published 0.3.1 release to the
-- 0.4.0-dev1 development point version.
-- Record manifest byte identity separately from catalog and package hashes.
ALTER TABLE pgokf.skills ADD COLUMN IF NOT EXISTS skill_md_sha256 text;
ALTER TABLE pgokf.skills ADD COLUMN IF NOT EXISTS skill_md_byte_size bigint;
UPDATE pgokf.skills
SET skill_md_sha256 = pg_catalog.encode(pg_catalog.sha256(skill_md), 'hex'),
    skill_md_byte_size = pg_catalog.octet_length(skill_md)::bigint
WHERE skill_md_sha256 IS NULL OR skill_md_byte_size IS NULL;
ALTER TABLE pgokf.skills ALTER COLUMN skill_md_sha256 SET NOT NULL;
ALTER TABLE pgokf.skills ALTER COLUMN skill_md_byte_size SET NOT NULL;
COMMENT ON COLUMN pgokf.skills.skill_md_sha256 IS
    'Lowercase hexadecimal SHA-256 of the exact SKILL.md bytes; distinct from the BLAKE3 source file and package hashes.';
COMMENT ON COLUMN pgokf.skills.skill_md_byte_size IS
    'Length in bytes of skill_md, recorded so discovery can size the manifest without loading its content.';
SELECT pgokf_private.register_dump_relations();
