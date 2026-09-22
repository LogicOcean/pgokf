-- Upgrade the immutable unpublished 0.3.0 candidate to 0.3.1.
-- No catalog changes; refresh the dump registration receipt.
SELECT pgokf_private.register_dump_relations();
